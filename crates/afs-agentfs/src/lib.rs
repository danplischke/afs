//! afs-agentfs — import a `tursodatabase/agentfs` database into an afs workspace
//! (`docs/DESIGN.md` §11 mapping; roadmap M9 migration tool).
//!
//! agentfs stores an agent's whole filesystem — inodes, directory entries,
//! fixed-size data chunks, symlinks — plus a tool-call audit log in one SQLite
//! database. This crate reads that database (its schema is fixed by the AgentFS
//! SPEC) and replays it into afs: directories, files (reassembled from their
//! chunks and re-chunked content-defined by afs), and symlinks, optionally
//! **attributed to a synthetic agent actor** so the imported tree carries afs
//! blame, with the agentfs `tool_calls` folded into afs's own audit log.
//!
//! Scope: a standalone agentfs snapshot is a materialized tree, so the overlay
//! copy-on-write tables (`fs_whiteout`, `fs_origin`) — meaningful only for a
//! delta layered over a base — are not consulted. POSIX permission bits beyond
//! the file kind are not preserved (afs applies its own default modes).

use afs_core::error::{AfsError, Result};
use afs_core::{ToolCallInit, WriteCtx};
use afs_sdk::Workspace;
use async_recursion::async_recursion;
use std::collections::HashMap;
use std::path::Path;

// POSIX mode type bits (agentfs stores kind in the mode's upper bits).
const S_IFMT: i64 = 0o170000;
const S_IFDIR: i64 = 0o040000;
const S_IFREG: i64 = 0o100000;
const S_IFLNK: i64 = 0o120000;

/// The agentfs root inode is fixed by the spec.
const ROOT_INO: i64 = 1;

/// Options for an agentfs import.
pub struct ImportOptions {
    /// Attribute every imported file to a synthetic agent actor, so afs blame
    /// shows the content as agent-authored.
    pub attribute: bool,
    /// Name for that agent actor (and the audit's owner).
    pub agent_name: String,
    /// Replay agentfs's `tool_calls` into afs's audit log.
    pub import_tool_calls: bool,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            attribute: true,
            agent_name: "agentfs".to_string(),
            import_tool_calls: true,
        }
    }
}

/// What an import produced.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImportStats {
    pub dirs: usize,
    pub files: usize,
    pub symlinks: usize,
    pub tool_calls: usize,
    pub bytes: u64,
}

/// Import the agentfs database at `db_path` into `ws`.
pub async fn import_agentfs(
    ws: &Workspace,
    db_path: &Path,
    opts: &ImportOptions,
) -> Result<ImportStats> {
    let snap = read_snapshot(db_path)?;

    // A synthetic actor/session carries attribution + the imported audit log.
    let ctx = if opts.attribute || opts.import_tool_calls {
        let agent = ws
            .fs()
            .create_agent(&opts.agent_name, "imported", None)
            .await?;
        let session = ws.fs().create_session(agent, Some("agentfs-import")).await?;
        Some((agent, session))
    } else {
        None
    };

    let mut stats = ImportStats::default();
    let write_ctx = ctx
        .filter(|_| opts.attribute)
        .map(|(a, s)| WriteCtx::session(a, s));
    import_dir(ws, &snap, ROOT_INO, "", write_ctx, &mut stats).await?;

    if opts.import_tool_calls {
        if let Some((agent, session)) = ctx {
            for tc in &snap.tool_calls {
                ws.fs()
                    .record_tool_call(ToolCallInit {
                        session_id: Some(session),
                        actor_id: Some(agent),
                        name: tc.name.clone(),
                        parameters: tc.parameters.clone(),
                        result: tc.result.clone(),
                        error: tc.error.clone(),
                        started_at: tc.started_at,
                        completed_at: tc.completed_at,
                        duration_ms: tc.duration_ms,
                    })
                    .await?;
                stats.tool_calls += 1;
            }
        }
    }

    Ok(stats)
}

#[async_recursion]
async fn import_dir(
    ws: &Workspace,
    snap: &Snapshot,
    dir_ino: i64,
    prefix: &str,
    ctx: Option<WriteCtx>,
    stats: &mut ImportStats,
) -> Result<()> {
    let Some(children) = snap.children.get(&dir_ino) else {
        return Ok(());
    };
    for (name, ino) in children {
        let path = format!("{prefix}/{name}");
        let Some(inode) = snap.inodes.get(ino) else {
            continue; // dangling dentry
        };
        match inode.mode & S_IFMT {
            S_IFDIR => {
                ws.mkdir_p(&path).await?;
                stats.dirs += 1;
                import_dir(ws, snap, *ino, &path, ctx, stats).await?;
            }
            S_IFREG => {
                let empty = Vec::new();
                let bytes = snap.file_data.get(ino).unwrap_or(&empty);
                match ctx {
                    Some(c) => ws.write_as(c, &path, bytes).await?,
                    None => ws.write(&path, bytes).await?,
                }
                stats.files += 1;
                stats.bytes += bytes.len() as u64;
            }
            S_IFLNK => {
                if let Some(target) = snap.symlinks.get(ino) {
                    ws.symlink(target, &path).await?;
                    stats.symlinks += 1;
                }
            }
            _ => {} // devices/fifos/sockets: not represented in afs
        }
    }
    Ok(())
}

// --- reading the agentfs database ------------------------------------------

struct InodeRow {
    mode: i64,
    size: i64,
}

struct ToolCallRow {
    name: String,
    parameters: Option<String>,
    result: Option<String>,
    error: Option<String>,
    started_at: i64,
    completed_at: i64,
    duration_ms: i64,
}

/// Everything we need from an agentfs database, read into memory so the async
/// import never holds the (non-`Send`) SQLite connection across an await.
struct Snapshot {
    inodes: HashMap<i64, InodeRow>,
    /// parent inode -> its `(name, ino)` children.
    children: HashMap<i64, Vec<(String, i64)>>,
    symlinks: HashMap<i64, String>,
    /// regular-file inode -> reassembled bytes.
    file_data: HashMap<i64, Vec<u8>>,
    tool_calls: Vec<ToolCallRow>,
}

fn read_snapshot(db_path: &Path) -> Result<Snapshot> {
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|e| AfsError::Metadata(format!("open agentfs db: {e}")))?;

    let mut inodes = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT ino, mode, size FROM fs_inode")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
        })?;
        for row in rows {
            let (ino, mode, size) = row?;
            inodes.insert(ino, InodeRow { mode, size });
        }
    }

    let mut children: HashMap<i64, Vec<(String, i64)>> = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT parent_ino, name, ino FROM fs_dentry")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
        })?;
        for row in rows {
            let (parent, name, ino) = row?;
            children.entry(parent).or_default().push((name, ino));
        }
    }
    // Deterministic order regardless of SQLite row order.
    for v in children.values_mut() {
        v.sort();
    }

    let mut symlinks = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT ino, target FROM fs_symlink")?;
        let rows =
            stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (ino, target) = row?;
            symlinks.insert(ino, target);
        }
    }

    // Reassemble file bodies: chunks are fixed-size and ordered by chunk_index;
    // concatenating in order yields the body, which we clamp to the inode size.
    let mut file_data: HashMap<i64, Vec<u8>> = HashMap::new();
    {
        let mut stmt =
            conn.prepare("SELECT ino, data FROM fs_data ORDER BY ino, chunk_index")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
        })?;
        for row in rows {
            let (ino, data) = row?;
            file_data.entry(ino).or_default().extend_from_slice(&data);
        }
    }
    for (ino, bytes) in file_data.iter_mut() {
        if let Some(inode) = inodes.get(ino) {
            bytes.truncate(inode.size.max(0) as usize);
        }
    }

    let tool_calls = read_tool_calls(&conn)?;

    Ok(Snapshot {
        inodes,
        children,
        symlinks,
        file_data,
        tool_calls,
    })
}

/// The `tool_calls` table is optional in principle; tolerate its absence.
fn read_tool_calls(conn: &rusqlite::Connection) -> Result<Vec<ToolCallRow>> {
    let mut stmt = match conn.prepare(
        "SELECT name, parameters, result, error, started_at, completed_at, duration_ms \
         FROM tool_calls ORDER BY id",
    ) {
        Ok(s) => s,
        Err(_) => return Ok(Vec::new()),
    };
    let rows = stmt.query_map([], |r| {
        Ok(ToolCallRow {
            name: r.get(0)?,
            parameters: r.get(1)?,
            result: r.get(2)?,
            error: r.get(3)?,
            started_at: r.get(4)?,
            completed_at: r.get(5)?,
            duration_ms: r.get(6)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}
