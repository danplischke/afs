//! Import a faithful agentfs SQLite database (built to the AgentFS SPEC schema,
//! including a multi-chunk file) and verify the tree, content, symlinks, agent
//! attribution, and audit replay land in afs.

use afs_agentfs::{import_agentfs, ImportOptions};
use afs_sdk::Workspace;
use rusqlite::{params, Connection};
use std::path::Path;

/// Create an agentfs database at `path` with a small tree and two tool calls.
/// Returns the exact bytes written for the multi-chunk file.
fn make_agentfs_db(path: &Path) -> Vec<u8> {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE fs_inode (
          ino INTEGER PRIMARY KEY AUTOINCREMENT,
          mode INTEGER NOT NULL, nlink INTEGER NOT NULL DEFAULT 0,
          uid INTEGER NOT NULL DEFAULT 0, gid INTEGER NOT NULL DEFAULT 0,
          size INTEGER NOT NULL DEFAULT 0,
          atime INTEGER NOT NULL, mtime INTEGER NOT NULL, ctime INTEGER NOT NULL,
          rdev INTEGER NOT NULL DEFAULT 0,
          atime_nsec INTEGER NOT NULL DEFAULT 0,
          mtime_nsec INTEGER NOT NULL DEFAULT 0,
          ctime_nsec INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE fs_dentry (
          id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL,
          parent_ino INTEGER NOT NULL, ino INTEGER NOT NULL,
          UNIQUE(parent_ino, name)
        );
        CREATE TABLE fs_data (
          ino INTEGER NOT NULL, chunk_index INTEGER NOT NULL, data BLOB NOT NULL,
          PRIMARY KEY (ino, chunk_index)
        );
        CREATE TABLE fs_symlink (ino INTEGER PRIMARY KEY, target TEXT NOT NULL);
        CREATE TABLE fs_config (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE tool_calls (
          id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL,
          parameters TEXT, result TEXT, error TEXT,
          started_at INTEGER NOT NULL, completed_at INTEGER NOT NULL,
          duration_ms INTEGER NOT NULL
        );
        "#,
    )
    .unwrap();

    let inode = |conn: &Connection, ino: i64, mode: i64, size: i64| {
        conn.execute(
            "INSERT INTO fs_inode (ino, mode, size, atime, mtime, ctime) VALUES (?,?,?,0,0,0)",
            params![ino, mode, size],
        )
        .unwrap();
    };
    let dentry = |conn: &Connection, name: &str, parent: i64, ino: i64| {
        conn.execute(
            "INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?,?,?)",
            params![name, parent, ino],
        )
        .unwrap();
    };

    // A multi-chunk file: 2 full 4096-byte chunks + a 100-byte tail.
    let readme: Vec<u8> = (0..8292u32).map(|i| (i % 251) as u8).collect();
    let main_rs = b"fn main() {}\n".to_vec();

    inode(&conn, 1, 0o040755, 0); // root
    inode(&conn, 2, 0o040755, 0); // /src
    inode(&conn, 3, 0o100644, readme.len() as i64); // /readme.md
    inode(&conn, 4, 0o100644, main_rs.len() as i64); // /src/main.rs
    inode(&conn, 5, 0o120777, 0); // /link -> /readme.md

    dentry(&conn, "src", 1, 2);
    dentry(&conn, "readme.md", 1, 3);
    dentry(&conn, "main.rs", 2, 4);
    dentry(&conn, "link", 1, 5);

    for (idx, chunk) in readme.chunks(4096).enumerate() {
        conn.execute(
            "INSERT INTO fs_data (ino, chunk_index, data) VALUES (3, ?, ?)",
            params![idx as i64, chunk],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO fs_data (ino, chunk_index, data) VALUES (4, 0, ?)",
        params![main_rs],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO fs_symlink (ino, target) VALUES (5, '/readme.md')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO fs_config (key, value) VALUES ('chunk_size', '4096')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO tool_calls (name, parameters, result, error, started_at, completed_at, duration_ms) \
         VALUES ('write_file', '{\"path\":\"/readme.md\"}', 'ok', NULL, 100, 150, 50)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO tool_calls (name, parameters, result, error, started_at, completed_at, duration_ms) \
         VALUES ('run', 'cargo build', NULL, 'exit 1', 200, 900, 700)",
        [],
    )
    .unwrap();

    readme
}

#[tokio::test]
async fn imports_tree_content_attribution_and_audit() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("agent.db");
    let readme = make_agentfs_db(&db);

    let ws = Workspace::open_local(tmp.path().join("meta.db"), tmp.path().join("cas"))
        .await
        .unwrap();
    let stats = import_agentfs(&ws, &db, &ImportOptions::default())
        .await
        .unwrap();

    assert_eq!(stats.dirs, 1);
    assert_eq!(stats.files, 2);
    assert_eq!(stats.symlinks, 1);
    assert_eq!(stats.tool_calls, 2);
    assert_eq!(stats.bytes, readme.len() as u64 + b"fn main() {}\n".len() as u64);

    // Directory tree.
    let mut top: Vec<String> = ws.ls("/").await.unwrap().into_iter().map(|e| e.name).collect();
    top.sort();
    assert_eq!(top, vec!["link", "readme.md", "src"]);

    // Multi-chunk file reassembled byte-for-byte.
    assert_eq!(&ws.read("/readme.md").await.unwrap()[..], &readme[..]);
    assert_eq!(&ws.read("/src/main.rs").await.unwrap()[..], b"fn main() {}\n");

    // Symlink target.
    assert_eq!(ws.readlink("/link").await.unwrap(), "/readme.md");

    // Imported content is attributed to the synthetic agent.
    let blame = ws.blame("/src/main.rs").await.unwrap();
    assert!(!blame.is_empty());
    assert_eq!(blame[0].actor.kind.as_str(), "agent");
    assert_eq!(blame[0].actor.display_name, "agentfs");
}

#[tokio::test]
async fn imports_without_attribution() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("agent.db");
    let readme = make_agentfs_db(&db);

    let ws = Workspace::open_local(tmp.path().join("meta.db"), tmp.path().join("cas"))
        .await
        .unwrap();
    let opts = ImportOptions {
        attribute: false,
        agent_name: "agentfs".to_string(),
        import_tool_calls: false,
    };
    let stats = import_agentfs(&ws, &db, &opts).await.unwrap();

    // Content still imported; no audit replayed.
    assert_eq!(stats.tool_calls, 0);
    assert_eq!(&ws.read("/readme.md").await.unwrap()[..], &readme[..]);
}
