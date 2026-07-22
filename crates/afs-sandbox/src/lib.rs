//! afs-sandbox — run an unmodified process against an **isolated copy-on-write
//! view** of an afs workspace, then import what it changed back as an attributed
//! commit (`docs/DESIGN.md` §4e; the agentfs `run` use case for overlay).
//!
//! Flow:
//! 1. **Materialize** the workspace's working tree to a real `lower/` directory.
//! 2. Mount an **unprivileged overlayfs** (`lower` + a scratch `upper`/`work`) in a
//!    user+mount namespace and `exec` the command with cwd in the merged view.
//! 3. On exit, the overlay `upper/` holds exactly the delta (created/modified
//!    files, plus whiteouts for deletions).
//! 4. **Import** that delta back into afs via attributed writes (blame + edit-op),
//!    or `--discard` it.
//!
//! The kernel overlay is the disposable *scratch*; afs's own object graph is the
//! durable, versioned, attributed layer the delta lands in.

use afs_sdk::{FileKind, Workspace, WriteCtx};
use anyhow::{bail, Context, Result};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

/// Options for a sandbox run.
pub struct RunOpts {
    /// Attribute imported changes to this actor (records blame + edit-ops).
    pub actor: Option<i64>,
    /// Throw the delta away instead of importing it.
    pub discard: bool,
    /// Working root for `lower/upper/work/merged` (a temp dir).
    pub work_root: PathBuf,
}

/// The result of a sandbox run.
#[derive(Debug)]
pub struct Outcome {
    pub exit_code: i32,
    pub imported: bool,
    pub files_changed: usize,
}

/// Whether unprivileged overlayfs-in-a-user-namespace works here (probes once).
pub fn overlay_supported() -> bool {
    let base = std::env::temp_dir().join(format!("afs-ovl-probe-{}", std::process::id()));
    let (low, up, work, merged) = (
        base.join("l"),
        base.join("u"),
        base.join("w"),
        base.join("m"),
    );
    for d in [&low, &up, &work, &merged] {
        let _ = std::fs::create_dir_all(d);
    }
    let script =
        "mount -t overlay overlay -o lowerdir=\"$1\",upperdir=\"$2\",workdir=\"$3\" \"$4\"";
    let ok = std::process::Command::new("unshare")
        .args(["-U", "-r", "-m", "/bin/sh", "-c", script, "probe"])
        .arg(&low)
        .arg(&up)
        .arg(&work)
        .arg(&merged)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let _ = std::fs::remove_dir_all(&base);
    ok
}

/// Run `cmd` in a sandbox over `ws`'s working tree.
pub async fn run(ws: &Workspace, opts: RunOpts, cmd: &[String]) -> Result<Outcome> {
    if cmd.is_empty() {
        bail!("no command given to sandbox");
    }
    let root = &opts.work_root;
    let lower = root.join("lower");
    let upper = root.join("upper");
    let work = root.join("work");
    let merged = root.join("merged");
    for d in [&lower, &upper, &work, &merged] {
        tokio::fs::create_dir_all(d).await?;
    }

    // 1. materialize the working tree into `lower/`
    export_tree(ws, "/", &lower)
        .await
        .context("materializing workspace into the sandbox lower layer")?;

    // 2. run the command in an unprivileged overlay namespace
    let exit_code = run_in_overlay(&lower, &upper, &work, &merged, cmd).await?;

    // 3. import the captured delta (unless discarding)
    let (imported, files_changed) = if opts.discard {
        (false, 0)
    } else {
        let session = match opts.actor {
            Some(a) => Some(ws.create_session(a, Some("sandbox")).await?),
            None => None,
        };
        let n = import_delta(ws, &upper, &upper, opts.actor, session).await?;
        (true, n)
    };

    Ok(Outcome {
        exit_code,
        imported,
        files_changed,
    })
}

/// Mount the overlay in a user+mount namespace and exec `cmd` with cwd=merged.
async fn run_in_overlay(
    lower: &Path,
    upper: &Path,
    work: &Path,
    merged: &Path,
    cmd: &[String],
) -> Result<i32> {
    // $1=lower $2=upper $3=work $4=merged, then the user command.
    const SCRIPT: &str = "mount -t overlay overlay -o lowerdir=\"$1\",upperdir=\"$2\",workdir=\"$3\" \"$4\" || exit 91\n\
                          cd \"$4\" || exit 92\n\
                          shift 4\n\
                          exec \"$@\"";
    let mut command = tokio::process::Command::new("unshare");
    command
        .args(["-U", "-r", "-m", "/bin/sh", "-c", SCRIPT, "afs-sandbox"])
        .arg(lower)
        .arg(upper)
        .arg(work)
        .arg(merged);
    for arg in cmd {
        command.arg(arg);
    }
    let status = command.status().await.context("spawning the sandbox")?;
    Ok(status.code().unwrap_or(-1))
}

fn join_afs(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// Recursively write the afs tree rooted at `afs_dir` into the host `host_dir`.
async fn export_tree(ws: &Workspace, afs_dir: &str, host_dir: &Path) -> Result<()> {
    tokio::fs::create_dir_all(host_dir).await?;
    for e in ws.ls(afs_dir).await? {
        let child_afs = join_afs(afs_dir, &e.name);
        let child_host = host_dir.join(&e.name);
        match e.kind {
            FileKind::Dir => {
                Box::pin(export_tree(ws, &child_afs, &child_host)).await?;
            }
            FileKind::File => {
                let bytes = ws.read(&child_afs).await?;
                tokio::fs::write(&child_host, &bytes).await?;
            }
            FileKind::Symlink => {
                let target = ws.readlink(&child_afs).await?;
                std::os::unix::fs::symlink(&target, &child_host)?;
            }
        }
    }
    Ok(())
}

/// Import the overlay `upper` delta under `dir` back into `ws`.
async fn import_delta(
    ws: &Workspace,
    root: &Path,
    dir: &Path,
    actor: Option<i64>,
    session: Option<i64>,
) -> Result<usize> {
    let mut count = 0;
    let mut rd = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        let host = entry.path();
        let rel = host.strip_prefix(root).unwrap_or(&host);
        let afs_path = format!("/{}", rel.to_string_lossy());
        let md = tokio::fs::symlink_metadata(&host).await?;
        let ft = md.file_type();

        if ft.is_char_device() && md.rdev() == 0 {
            // overlayfs whiteout => the path was deleted in the sandbox
            let _ = afs_rm_rf(ws, &afs_path).await;
            count += 1;
        } else if ft.is_dir() {
            ws.mkdir_p(&afs_path).await?;
            count += Box::pin(import_delta(ws, root, &host, actor, session)).await?;
        } else if ft.is_symlink() {
            let target = tokio::fs::read_link(&host).await?;
            let _ = ws.remove(&afs_path).await;
            ws.symlink(&target.to_string_lossy(), &afs_path).await?;
            count += 1;
        } else if ft.is_file() {
            let bytes = tokio::fs::read(&host).await?;
            match (actor, session) {
                (Some(a), Some(s)) => {
                    ws.write_as(WriteCtx::session(a, s), &afs_path, &bytes)
                        .await?
                }
                _ => ws.write(&afs_path, &bytes).await?,
            }
            count += 1;
        }
    }
    Ok(count)
}

/// Recursively remove an afs path (file or directory).
async fn afs_rm_rf(ws: &Workspace, path: &str) -> Result<()> {
    match ws.stat(path).await {
        Ok(inode) if inode.kind == FileKind::Dir => {
            for e in ws.ls(path).await? {
                Box::pin(afs_rm_rf(ws, &join_afs(path, &e.name))).await?;
            }
            ws.remove(path).await?;
        }
        Ok(_) => {
            ws.remove(path).await?;
        }
        Err(_) => {} // already gone
    }
    Ok(())
}
