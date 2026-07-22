//! A SQLite-backed [`MetadataStore`] — the M0 default and the SQLite half of the
//! pluggable-backend story (`docs/DESIGN.md` §4b).
//!
//! rusqlite is synchronous, so DB work runs under a mutex inside each `async`
//! method. Because no `.await` occurs while the guard is held, the futures stay
//! `Send`. A production build would move DB work onto `spawn_blocking` (or use an
//! async driver like sqlx, which M2 introduces alongside Postgres).

use crate::error::{AfsError, Result};
use crate::metadata::MetadataStore;
use crate::migrations::MIGRATIONS;
use crate::types::{DirEntry, FileKind, Hash, INO_ROOT, Ino, Inode, InodeInit};
use crate::util::now_secs;
use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::{Arc, Mutex};

const DIR_MODE: i64 = 0o040755;

/// A metadata store backed by a single SQLite database.
pub struct SqliteMetadataStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteMetadataStore {
    /// Open (creating if needed) a database file at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Open a private in-memory database (handy for tests).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .expect("afs sqlite connection mutex poisoned")
    }
}

/// Build an [`Inode`] from a raw row tuple.
#[allow(clippy::type_complexity)]
fn build_inode(row: (i64, String, i64, i64, i64, Option<String>, i64, i64)) -> Result<Inode> {
    let (ino, kind, mode, nlink, size, content_hash, mtime, ctime) = row;
    let kind = FileKind::parse(&kind)
        .ok_or_else(|| AfsError::Metadata(format!("unknown inode kind {kind:?}")))?;
    let content = match content_hash {
        Some(s) => Some(
            Hash::from_hex(&s)
                .ok_or_else(|| AfsError::Metadata(format!("bad content hash {s:?}")))?,
        ),
        None => None,
    };
    Ok(Inode {
        ino,
        kind,
        mode: mode as u32,
        nlink,
        size: size as u64,
        content,
        mtime,
        ctime,
    })
}

#[async_trait]
impl MetadataStore for SqliteMetadataStore {
    async fn init(&self) -> Result<()> {
        let conn = self.lock();
        let now = now_secs();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_meta(version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL);",
        )?;
        for m in MIGRATIONS {
            let applied = conn
                .query_row(
                    "SELECT 1 FROM schema_meta WHERE version = ?1",
                    params![m.version],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !applied {
                conn.execute_batch(m.sqlite)?;
                conn.execute(
                    "INSERT INTO schema_meta(version, applied_at) VALUES (?1, ?2)",
                    params![m.version, now],
                )?;
            }
        }
        conn.execute(
            "INSERT OR IGNORE INTO inode(ino, kind, mode, nlink, size, content_hash, mtime, ctime)
             VALUES (?1, 'dir', ?2, 1, 0, NULL, ?3, ?3)",
            params![INO_ROOT, DIR_MODE, now],
        )?;
        Ok(())
    }

    async fn get_inode(&self, ino: Ino) -> Result<Option<Inode>> {
        let conn = self.lock();
        let row = conn
            .query_row(
                "SELECT ino, kind, mode, nlink, size, content_hash, mtime, ctime
                 FROM inode WHERE ino = ?1",
                params![ino],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, Option<String>>(5)?,
                        r.get::<_, i64>(6)?,
                        r.get::<_, i64>(7)?,
                    ))
                },
            )
            .optional()?;
        match row {
            Some(t) => Ok(Some(build_inode(t)?)),
            None => Ok(None),
        }
    }

    async fn create_inode(&self, init: InodeInit) -> Result<Ino> {
        let conn = self.lock();
        let now = now_secs();
        conn.execute(
            "INSERT INTO inode(kind, mode, nlink, size, content_hash, mtime, ctime)
             VALUES (?1, ?2, 1, 0, NULL, ?3, ?3)",
            params![init.kind.as_str(), init.mode as i64, now],
        )?;
        Ok(conn.last_insert_rowid())
    }

    async fn set_content(&self, ino: Ino, content: Option<Hash>, size: u64) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE inode SET content_hash = ?1, size = ?2, mtime = ?3, ctime = ?3 WHERE ino = ?4",
            params![content.map(|h| h.to_hex()), size as i64, now_secs(), ino],
        )?;
        Ok(())
    }

    async fn set_nlink(&self, ino: Ino, nlink: i64) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE inode SET nlink = ?1 WHERE ino = ?2",
            params![nlink, ino],
        )?;
        Ok(())
    }

    async fn delete_inode(&self, ino: Ino) -> Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM symlink WHERE ino = ?1", params![ino])?;
        conn.execute("DELETE FROM inode WHERE ino = ?1", params![ino])?;
        Ok(())
    }

    async fn lookup(&self, parent: Ino, name: &str) -> Result<Option<Ino>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT ino FROM dentry WHERE parent_ino = ?1 AND name = ?2",
            params![parent, name],
            |r| r.get::<_, i64>(0),
        )
        .optional()
        .map_err(Into::into)
    }

    async fn add_dentry(&self, parent: Ino, name: &str, ino: Ino) -> Result<()> {
        let conn = self.lock();
        match conn.execute(
            "INSERT INTO dentry(parent_ino, name, ino) VALUES (?1, ?2, ?3)",
            params![parent, name, ino],
        ) {
            Ok(_) => Ok(()),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Err(AfsError::AlreadyExists(name.to_string()))
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn remove_dentry(&self, parent: Ino, name: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM dentry WHERE parent_ino = ?1 AND name = ?2",
            params![parent, name],
        )?;
        Ok(())
    }

    async fn list_dir(&self, parent: Ino) -> Result<Vec<DirEntry>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT d.name, d.ino, i.kind
             FROM dentry d JOIN inode i ON i.ino = d.ino
             WHERE d.parent_ino = ?1
             ORDER BY d.name",
        )?;
        let rows = stmt.query_map(params![parent], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (name, ino, kind) = row?;
            let kind = FileKind::parse(&kind)
                .ok_or_else(|| AfsError::Metadata(format!("unknown inode kind {kind:?}")))?;
            out.push(DirEntry { name, ino, kind });
        }
        Ok(out)
    }

    async fn child_count(&self, parent: Ino) -> Result<usize> {
        let conn = self.lock();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM dentry WHERE parent_ino = ?1",
            params![parent],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    async fn set_symlink(&self, ino: Ino, target: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO symlink(ino, target) VALUES (?1, ?2)
             ON CONFLICT(ino) DO UPDATE SET target = excluded.target",
            params![ino, target],
        )?;
        Ok(())
    }

    async fn get_symlink(&self, ino: Ino) -> Result<Option<String>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT target FROM symlink WHERE ino = ?1",
            params![ino],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .map_err(Into::into)
    }

    async fn get_ref(&self, name: &str) -> Result<Option<String>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT value FROM ref WHERE name = ?1",
            params![name],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .map_err(Into::into)
    }

    async fn set_ref(&self, name: &str, value: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO ref(name, value) VALUES (?1, ?2)
             ON CONFLICT(name) DO UPDATE SET value = excluded.value",
            params![name, value],
        )?;
        Ok(())
    }

    async fn cas_ref(&self, name: &str, expect: Option<&str>, new: &str) -> Result<bool> {
        let conn = self.lock();
        let changed = match expect {
            None => conn.execute(
                "INSERT INTO ref(name, value) VALUES (?1, ?2) ON CONFLICT(name) DO NOTHING",
                params![name, new],
            )?,
            Some(v) => conn.execute(
                "UPDATE ref SET value = ?1 WHERE name = ?2 AND value = ?3",
                params![new, name, v],
            )?,
        };
        Ok(changed == 1)
    }

    async fn delete_ref(&self, name: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM ref WHERE name = ?1", params![name])?;
        Ok(())
    }

    async fn list_refs(&self) -> Result<Vec<(String, String)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT name, value FROM ref ORDER BY name")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    async fn get_config(&self, key: &str) -> Result<Option<String>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT value FROM config WHERE key = ?1",
            params![key],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .map_err(Into::into)
    }

    async fn set_config(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO config(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    async fn truncate_tree(&self) -> Result<()> {
        let conn = self.lock();
        conn.execute_batch(
            "DELETE FROM dentry; DELETE FROM symlink; DELETE FROM inode WHERE ino <> 1;",
        )?;
        Ok(())
    }

    async fn set_conflict(&self, path: &str, kind: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO conflict(path, kind) VALUES (?1, ?2)
             ON CONFLICT(path) DO UPDATE SET kind = excluded.kind",
            params![path, kind],
        )?;
        Ok(())
    }

    async fn list_conflicts(&self) -> Result<Vec<(String, String)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT path, kind FROM conflict ORDER BY path")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    async fn clear_conflicts(&self) -> Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM conflict", [])?;
        Ok(())
    }

    async fn acquire_lock(&self, path: &str, owner: &str, at: i64) -> Result<bool> {
        let conn = self.lock();
        let changed = conn.execute(
            "INSERT INTO file_lock(path, owner, acquired_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(path) DO NOTHING",
            params![path, owner, at],
        )?;
        Ok(changed == 1)
    }

    async fn release_lock(&self, path: &str, owner: &str) -> Result<bool> {
        let conn = self.lock();
        let changed = conn.execute(
            "DELETE FROM file_lock WHERE path = ?1 AND owner = ?2",
            params![path, owner],
        )?;
        Ok(changed == 1)
    }

    async fn list_locks(&self) -> Result<Vec<(String, String, i64)>> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT path, owner, acquired_at FROM file_lock ORDER BY path")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}
