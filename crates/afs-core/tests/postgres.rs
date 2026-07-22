//! Postgres backend: the same metadata + engine behavior as SQLite, plus a
//! concurrent-writers check and the advisory-lock / NOTIFY helpers.
//!
//! Self-skips unless `AFS_PG_TEST_URL` points at a reachable database, e.g.
//!   AFS_PG_TEST_URL="host=/tmp/afs-pg/sock port=5433 user=postgres dbname=afs"

use afs_core::{
    EventInit, FileKind, Fs, InodeInit, MemStore, MetadataStore, PostgresMetadataStore,
    SuggestionStatus, WriteCtx,
};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::time::timeout;

fn dsn() -> Option<String> {
    std::env::var("AFS_PG_TEST_URL").ok()
}

/// Serializes the PG tests: they share one database and each resets the schema,
/// so they must not overlap (cargo runs tests in a binary concurrently).
fn pg_lock() -> &'static tokio::sync::Mutex<()> {
    static L: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    L.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Drop and recreate the `public` schema so each run starts clean.
async fn reset(dsn: &str) {
    let (client, connection) = tokio_postgres::connect(dsn, tokio_postgres::NoTls)
        .await
        .expect("connect for reset");
    let handle = tokio::spawn(async move {
        let _ = connection.await;
    });
    client
        .batch_execute("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        .await
        .expect("reset public schema");
    drop(client);
    let _ = handle.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_backend() {
    let Some(dsn) = dsn() else {
        eprintln!("skipping postgres_backend: AFS_PG_TEST_URL unset");
        return;
    };
    let _guard = pg_lock().lock().await;
    reset(&dsn).await;

    // --- metadata-store level ------------------------------------------------
    let meta = PostgresMetadataStore::connect(&dsn).await.unwrap();
    meta.init().await.unwrap();
    meta.init().await.unwrap(); // idempotent

    let root = meta.get_inode(1).await.unwrap().expect("root inode");
    assert_eq!(root.kind, FileKind::Dir);

    let ino = meta
        .create_inode(InodeInit {
            kind: FileKind::File,
            mode: 0o100644,
        })
        .await
        .unwrap();
    assert!(ino > 1, "identity sequence must not collide with root");
    meta.add_dentry(1, "hello", ino).await.unwrap();
    assert_eq!(meta.lookup(1, "hello").await.unwrap(), Some(ino));
    // duplicate name is rejected
    assert!(meta.add_dentry(1, "hello", ino).await.is_err());
    assert_eq!(meta.child_count(1).await.unwrap(), 1);
    let entries = meta.list_dir(1).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "hello");

    meta.set_symlink(ino, "/target").await.unwrap();
    assert_eq!(
        meta.get_symlink(ino).await.unwrap().as_deref(),
        Some("/target")
    );

    meta.remove_dentry(1, "hello").await.unwrap();
    meta.delete_inode(ino).await.unwrap();
    assert!(meta.get_inode(ino).await.unwrap().is_none());

    // --- engine over Postgres (same code path as SQLite) --------------------
    let content = Arc::new(MemStore::new());
    let fs = Fs::new(PostgresMetadataStore::connect(&dsn).await.unwrap(), content);
    fs.init().await.unwrap();
    fs.mkdir_p("/a/b").await.unwrap();
    fs.write("/a/f.txt", b"hello pg").await.unwrap();
    assert_eq!(&fs.read("/a/f.txt").await.unwrap()[..], b"hello pg");
    fs.rename("/a/f.txt", "/a/g.txt").await.unwrap();
    assert!(fs.read("/a/f.txt").await.is_err());
    assert_eq!(&fs.read("/a/g.txt").await.unwrap()[..], b"hello pg");

    // --- concurrent writers to different inodes don't block/deadlock --------
    let fs = Arc::new(fs);
    let mut handles = Vec::new();
    for i in 0..20 {
        let fs = fs.clone();
        handles.push(tokio::spawn(async move {
            fs.write(&format!("/a/c{i:02}.txt"), format!("data-{i}").as_bytes())
                .await
        }));
    }
    for h in handles {
        h.await.unwrap().unwrap();
    }
    // b, g.txt, and 20 concurrent files
    assert_eq!(fs.ls("/a").await.unwrap().len(), 22);
    assert_eq!(&fs.read("/a/c07.txt").await.unwrap()[..], b"data-7");

    // --- versioning over Postgres (same engine, PG-backed refs/config) ------
    let commit = fs.commit("tester", "snapshot on pg").await.unwrap();
    let log = fs.log().await.unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].hash, commit);
    fs.create_branch("dev").await.unwrap();
    assert!(
        fs.list_branches()
            .await
            .unwrap()
            .iter()
            .any(|(n, _)| n == "dev")
    );

    // --- attribution over Postgres ------------------------------------------
    let human = fs.create_human("dev-human", Some("dev@x")).await.unwrap();
    let agent = fs
        .create_agent("dev-agent", "m", Some(human))
        .await
        .unwrap();
    let sess = fs.create_session(agent, None).await.unwrap();
    fs.write_as(WriteCtx::session(agent, sess), "/a/attr.txt", b"one\ntwo\n")
        .await
        .unwrap();
    assert_eq!(fs.blame("/a/attr.txt").await.unwrap()[0].actor.id, agent);
    assert_eq!(
        fs.get_actor(agent)
            .await
            .unwrap()
            .unwrap()
            .controller_actor_id,
        Some(human)
    );
    assert_eq!(fs.edit_ops(agent, Some(sess)).await.unwrap().len(), 1);

    // --- agent-suggestion review queue over Postgres ------------------------
    let sug = fs
        .suggest(
            afs_core::WriteCtx::session(agent, sess),
            "/a/attr.txt",
            b"one\ntwo\nthree\n",
            Some("append a line"),
        )
        .await
        .unwrap();
    let pending = fs.list_suggestions(Some(SuggestionStatus::Pending), None).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, sug);
    assert!(fs.suggestion_diff(sug).await.unwrap().contains("+three"));
    // working tree untouched until accept
    assert_eq!(&fs.read("/a/attr.txt").await.unwrap()[..], b"one\ntwo\n");
    fs.accept_suggestion(sug, afs_core::WriteCtx::actor(human)).await.unwrap();
    assert_eq!(&fs.read("/a/attr.txt").await.unwrap()[..], b"one\ntwo\nthree\n");
    assert_eq!(
        fs.get_suggestion(sug).await.unwrap().unwrap().status,
        SuggestionStatus::Accepted
    );

    // --- advisory lock + NOTIFY helpers -------------------------------------
    let pg = PostgresMetadataStore::connect(&dsn).await.unwrap();
    pg.advisory_lock(4242).await.unwrap();
    assert!(pg.advisory_unlock(4242).await.unwrap());
    pg.notify("afs_changes", "hello").await.unwrap();
}

/// Await the next batch with a timeout so a stuck subscription fails loudly.
async fn recv_batch(sub: &mut afs_core::EventSubscription) -> Vec<afs_core::Event> {
    timeout(Duration::from_secs(5), sub.recv())
        .await
        .expect("recv timed out")
        .expect("recv errored")
}

/// The `LISTEN`-backed push subscription: a committed change wakes `recv()` and
/// yields the new events in order; coalesced changes arrive in one batch; and a
/// branch filter delivers only that branch's events.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_change_feed_push() {
    let Some(dsn) = dsn() else {
        eprintln!("skipping postgres_change_feed_push: AFS_PG_TEST_URL unset");
        return;
    };
    let _guard = pg_lock().lock().await;
    reset(&dsn).await;
    let meta = PostgresMetadataStore::connect(&dsn).await.unwrap();
    meta.init().await.unwrap();

    let ev = |kind: &str, path: &str, branch: &str| EventInit {
        actor_id: None,
        session_id: None,
        kind: kind.to_string(),
        path: path.to_string(),
        detail: None,
        branch: Some(branch.to_string()),
    };

    // Subscribe from the start; a separate handle appends -> we get pushed.
    let mut sub = meta.subscribe(0, None).await.unwrap();
    let writer = PostgresMetadataStore::connect(&dsn).await.unwrap();
    writer.append_event(ev("write", "/a", "main"), 100).await.unwrap();

    let batch = recv_batch(&mut sub).await;
    assert_eq!(batch.len(), 1);
    assert_eq!(batch[0].path, "/a");
    assert_eq!(batch[0].branch.as_deref(), Some("main"));

    // Two changes before the next recv coalesce into one ordered batch.
    writer.append_event(ev("write", "/b", "main"), 101).await.unwrap();
    writer.append_event(ev("mkdir", "/c", "feature"), 102).await.unwrap();
    let batch = recv_batch(&mut sub).await;
    let paths: Vec<&str> = batch.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, vec!["/b", "/c"], "ordered by seq, both delivered");
    drop(sub);

    // A branch-filtered subscription only ever sees its own branch.
    let mut feat = meta.subscribe(0, Some("feature".to_string())).await.unwrap();
    let seen = recv_batch(&mut feat).await; // /c already exists on feature
    assert!(seen.iter().all(|e| e.branch.as_deref() == Some("feature")));
    assert!(seen.iter().any(|e| e.path == "/c"));

    writer.append_event(ev("write", "/d", "main"), 103).await.unwrap();
    writer.append_event(ev("write", "/e", "feature"), 104).await.unwrap();
    let batch = recv_batch(&mut feat).await;
    assert!(batch.iter().all(|e| e.branch.as_deref() == Some("feature")));
    assert!(batch.iter().any(|e| e.path == "/e"));
    assert!(batch.iter().all(|e| e.path != "/d"), "main change filtered out");
}

/// A from-zero / lagging subscriber pages the backlog in bounded batches
/// instead of loading every event into memory at once (drain has a LIMIT).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_drain_is_bounded() {
    let Some(dsn) = dsn() else {
        eprintln!("skipping postgres_drain_is_bounded: AFS_PG_TEST_URL unset");
        return;
    };
    let _guard = pg_lock().lock().await;
    reset(&dsn).await;
    let meta = PostgresMetadataStore::connect(&dsn).await.unwrap();
    meta.init().await.unwrap();

    // Append more than one drain batch (the internal LIMIT is 1024).
    for i in 0..1100i64 {
        meta.append_event(
            EventInit {
                actor_id: None,
                session_id: None,
                kind: "write".into(),
                path: format!("/f{i}"),
                detail: None,
                branch: None,
            },
            100 + i,
        )
        .await
        .unwrap();
    }

    let mut sub = meta.subscribe(0, None).await.unwrap();
    let first = recv_batch(&mut sub).await;
    assert_eq!(first.len(), 1024, "first drain is capped at the batch limit");
    let second = recv_batch(&mut sub).await;
    assert_eq!(second.len(), 76, "the remainder pages on the next drain");
    // ordered + contiguous across pages
    assert_eq!(first[0].seq + 1, first[1].seq);
    assert_eq!(first.last().unwrap().seq + 1, second[0].seq);
}
