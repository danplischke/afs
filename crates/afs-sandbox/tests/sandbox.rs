//! Sandbox end-to-end: run a real command over an isolated CoW view, then import
//! its delta (create / modify / delete) back into afs with attribution.
//!
//! Self-skips where unprivileged overlayfs isn't available.

use afs_sandbox::{overlay_supported, run, RunOpts};
use afs_sdk::{ActorKind, Workspace};

async fn workspace(dir: &std::path::Path) -> Workspace {
    Workspace::open_local(dir.join("meta.db"), dir.join("cas"))
        .await
        .unwrap()
}

#[tokio::test]
async fn imports_delta_with_attribution() {
    if !overlay_supported() {
        eprintln!("skipping: unprivileged overlayfs unavailable");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let ws = workspace(dir.path()).await;
    ws.write("/keep.txt", b"original\n").await.unwrap();
    ws.write("/gone.txt", b"delete me\n").await.unwrap();
    let agent = ws.create_agent("builder", "m", None).await.unwrap();

    // The sandboxed command modifies, creates, and deletes files.
    let cmd = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "echo modified >> keep.txt; echo created > new.txt; rm gone.txt".to_string(),
    ];
    let out = run(
        &ws,
        RunOpts {
            actor: Some(agent),
            discard: false,
            work_root: dir.path().join("sbx"),
        },
        &cmd,
    )
    .await
    .unwrap();
    assert_eq!(out.exit_code, 0);
    assert!(out.imported);

    // The delta landed in afs.
    assert_eq!(
        &ws.read("/keep.txt").await.unwrap()[..],
        b"original\nmodified\n"
    );
    assert_eq!(&ws.read("/new.txt").await.unwrap()[..], b"created\n");
    assert!(ws.stat("/gone.txt").await.is_err(), "gone.txt was deleted");

    // The new file is attributed to the sandbox's agent.
    let blame = ws.blame("/new.txt").await.unwrap();
    assert_eq!(blame[0].actor.id, agent);
    assert_eq!(blame[0].actor.kind, ActorKind::Agent);
}

#[tokio::test]
async fn discard_leaves_workspace_untouched() {
    if !overlay_supported() {
        eprintln!("skipping: unprivileged overlayfs unavailable");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let ws = workspace(dir.path()).await;
    ws.write("/f.txt", b"before\n").await.unwrap();

    let cmd = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "echo clobbered > f.txt; echo junk > extra.txt".to_string(),
    ];
    let out = run(
        &ws,
        RunOpts {
            actor: None,
            discard: true,
            work_root: dir.path().join("sbx"),
        },
        &cmd,
    )
    .await
    .unwrap();
    assert_eq!(out.exit_code, 0);
    assert!(!out.imported);

    // Nothing changed in afs.
    assert_eq!(&ws.read("/f.txt").await.unwrap()[..], b"before\n");
    assert!(ws.stat("/extra.txt").await.is_err());
}
