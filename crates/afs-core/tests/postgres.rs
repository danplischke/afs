//! Postgres backend: the same metadata + engine behavior as SQLite, plus a
//! concurrent-writers check and the advisory-lock / NOTIFY helpers.
//!
//! Self-skips unless `AFS_PG_TEST_URL` points at a reachable database, e.g.
//!   AFS_PG_TEST_URL="host=/tmp/afs-pg/sock port=5433 user=postgres dbname=afs"

use afs_core::{FileKind, Fs, InodeInit, MemStore, MetadataStore, PostgresMetadataStore};
use std::sync::Arc;

fn dsn() -> Option<String> {
    std::env::var("AFS_PG_TEST_URL").ok()
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

    // --- advisory lock + NOTIFY helpers -------------------------------------
    let pg = PostgresMetadataStore::connect(&dsn).await.unwrap();
    pg.advisory_lock(4242).await.unwrap();
    assert!(pg.advisory_unlock(4242).await.unwrap());
    pg.notify("afs_changes", "hello").await.unwrap();
}
