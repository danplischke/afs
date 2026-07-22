//! Attribution & blame: per-line human-vs-agent authorship, provenance, the
//! edit-op log, and reverting an agent's session.

use afs_core::{ActorKind, Fs, MemStore, SqliteMetadataStore, WriteCtx};
use std::sync::Arc;

async fn fixture() -> Fs<SqliteMetadataStore, Arc<MemStore>> {
    let store = Arc::new(MemStore::new());
    let meta = SqliteMetadataStore::open_in_memory().unwrap();
    let fs = Fs::new(meta, store);
    fs.init().await.unwrap();
    fs
}

#[tokio::test]
async fn human_and_agent_blame_is_per_line() {
    let fs = fixture().await;
    let alice = fs
        .create_human("alice", Some("alice@example.com"))
        .await
        .unwrap();
    let claude = fs
        .create_agent("claude", "claude-opus-4-8", Some(alice))
        .await
        .unwrap();
    let s_alice = fs.create_session(alice, Some("editor")).await.unwrap();
    let s_claude = fs.create_session(claude, Some("mcp")).await.unwrap();

    // Alice writes the file.
    fs.write_as(WriteCtx::session(alice, s_alice), "/f", b"l1\nl2\nl3\n")
        .await
        .unwrap();
    let b = fs.blame("/f").await.unwrap();
    assert_eq!(b.len(), 1);
    assert_eq!(b[0].actor.id, alice);
    assert_eq!(b[0].actor.kind, ActorKind::Human);
    assert_eq!((b[0].line_start, b[0].line_end), (1, 3));

    // Claude edits only line 2.
    fs.write_as(
        WriteCtx::session(claude, s_claude),
        "/f",
        b"l1\nCLAUDE\nl3\n",
    )
    .await
    .unwrap();
    let b = fs.blame("/f").await.unwrap();
    // three ranges: alice / claude / alice
    assert_eq!(b.len(), 3);
    assert_eq!(
        (b[0].actor.id, b[0].line_start, b[0].line_end),
        (alice, 1, 1)
    );
    assert_eq!(
        (b[1].actor.id, b[1].line_start, b[1].line_end),
        (claude, 2, 2)
    );
    assert_eq!(
        (b[2].actor.id, b[2].line_start, b[2].line_end),
        (alice, 3, 3)
    );
    assert_eq!(b[1].actor.kind, ActorKind::Agent);
    assert_eq!(b[1].actor.agent_model.as_deref(), Some("claude-opus-4-8"));

    // Provenance chain: the agent points at the human that launched it.
    let agent = fs.get_actor(claude).await.unwrap().unwrap();
    assert_eq!(agent.controller_actor_id, Some(alice));
}

#[tokio::test]
async fn edit_op_log_records_writes() {
    let fs = fixture().await;
    let claude = fs.create_agent("claude", "m", None).await.unwrap();
    let s = fs.create_session(claude, None).await.unwrap();
    fs.write_as(WriteCtx::session(claude, s), "/a", b"x")
        .await
        .unwrap();
    fs.write_as(WriteCtx::session(claude, s), "/b", b"y")
        .await
        .unwrap();

    let ops = fs.edit_ops(claude, Some(s)).await.unwrap();
    assert_eq!(ops.len(), 2);
    assert!(ops.iter().all(|o| o.actor_id == claude && o.op == "write"));
    let paths: Vec<&str> = ops.iter().map(|o| o.path.as_str()).collect();
    assert_eq!(paths, vec!["/a", "/b"]);
    // narrowing to a different (nonexistent) session yields nothing
    assert!(fs.edit_ops(claude, Some(s + 999)).await.unwrap().is_empty());
}

#[tokio::test]
async fn revert_session_removes_only_that_actors_lines() {
    let fs = fixture().await;
    let alice = fs.create_human("alice", None).await.unwrap();
    let claude = fs.create_agent("claude", "m", Some(alice)).await.unwrap();
    let s_alice = fs.create_session(alice, None).await.unwrap();
    let s_claude = fs.create_session(claude, None).await.unwrap();

    fs.write_as(
        WriteCtx::session(alice, s_alice),
        "/doc",
        b"human-1\nhuman-2\n",
    )
    .await
    .unwrap();
    // Claude appends a line (keeps the human lines).
    fs.write_as(
        WriteCtx::session(claude, s_claude),
        "/doc",
        b"human-1\nhuman-2\nagent-line\n",
    )
    .await
    .unwrap();
    assert_eq!(
        fs.blame("/doc").await.unwrap().last().unwrap().actor.id,
        claude
    );

    // Revert everything the agent wrote in its session.
    let changed = fs.revert_session(claude, s_claude).await.unwrap();
    assert_eq!(changed, 1);
    assert_eq!(&fs.read("/doc").await.unwrap()[..], b"human-1\nhuman-2\n");
    let b = fs.blame("/doc").await.unwrap();
    assert_eq!(b.len(), 1);
    assert_eq!(b[0].actor.id, alice);
}

#[tokio::test]
async fn binary_files_get_file_level_attribution() {
    let fs = fixture().await;
    let claude = fs.create_agent("claude", "m", None).await.unwrap();
    let s = fs.create_session(claude, None).await.unwrap();
    fs.write_as(
        WriteCtx::session(claude, s),
        "/b",
        &[0xff, 0xfe, 0x00, 0x01],
    )
    .await
    .unwrap();
    let b = fs.blame("/b").await.unwrap();
    assert_eq!(b.len(), 1);
    assert_eq!(b[0].actor.id, claude);
}
