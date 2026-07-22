//! A Postgres-backed [`MetadataStore`] — the multi-writer backend (`docs/DESIGN.md`
//! §4b).
//!
//! Runs the same schema as SQLite (via the shared [`crate::migrations`] list, in
//! the Postgres dialect) so the engine and the whole FS test suite work unchanged.
//! Postgres unlocks the shared-workspace goals: MVCC multi-writer, advisory locks
//! for hot-inode critical sections, and `LISTEN/NOTIFY` change feeds (consumed by
//! the watch API in M8).

use crate::error::{AfsError, Result};
use crate::metadata::MetadataStore;
use crate::migrations::MIGRATIONS;
use crate::types::{DirEntry, FileKind, Hash, Ino, Inode, InodeInit};
use crate::util::now_secs;
use async_trait::async_trait;
use deadpool_postgres::{Manager, Pool};
use tokio_postgres::error::SqlState;
use tokio_postgres::{NoTls, Row};

const DIR_MODE: i64 = 0o040755;

/// A metadata store backed by a Postgres database (with a connection pool).
pub struct PostgresMetadataStore {
    pool: Pool,
}

impl PostgresMetadataStore {
    /// Connect to Postgres. `dsn` is a libpq DSN or URL, e.g.
    /// `postgres://user:pass@host/db` or `host=/var/run/postgresql dbname=afs`.
    pub async fn connect(dsn: &str) -> Result<Self> {
        let cfg: tokio_postgres::Config = dsn
            .parse()
            .map_err(|e: tokio_postgres::Error| AfsError::Metadata(e.to_string()))?;
        let mgr = Manager::new(cfg, NoTls);
        let pool = Pool::builder(mgr)
            .max_size(16)
            .build()
            .map_err(|e| AfsError::Metadata(e.to_string()))?;
        Ok(Self { pool })
    }

    async fn client(&self) -> Result<deadpool_postgres::Object> {
        self.pool
            .get()
            .await
            .map_err(|e| AfsError::Metadata(e.to_string()))
    }

    /// Acquire a session-level advisory lock (`pg_advisory_lock`). Pair with
    /// [`Self::advisory_unlock`]. Used to serialize hot-inode critical sections.
    pub async fn advisory_lock(&self, key: i64) -> Result<()> {
        let c = self.client().await?;
        c.execute("SELECT pg_advisory_lock($1)", &[&key]).await?;
        Ok(())
    }

    pub async fn advisory_unlock(&self, key: i64) -> Result<bool> {
        let c = self.client().await?;
        let row = c
            .query_one("SELECT pg_advisory_unlock($1)", &[&key])
            .await?;
        Ok(row.get::<_, bool>(0))
    }

    /// Send a `LISTEN/NOTIFY` message (change-feed plumbing; the watch consumer
    /// arrives in M8).
    pub async fn notify(&self, channel: &str, payload: &str) -> Result<()> {
        let c = self.client().await?;
        c.execute("SELECT pg_notify($1, $2)", &[&channel, &payload])
            .await?;
        Ok(())
    }
}

fn row_to_inode(r: &Row) -> Result<Inode> {
    let kind_s: String = r.get(1);
    let kind = FileKind::parse(&kind_s)
        .ok_or_else(|| AfsError::Metadata(format!("unknown inode kind {kind_s:?}")))?;
    let content = match r.get::<_, Option<String>>(5) {
        Some(s) => Some(
            Hash::from_hex(&s)
                .ok_or_else(|| AfsError::Metadata(format!("bad content hash {s:?}")))?,
        ),
        None => None,
    };
    Ok(Inode {
        ino: r.get(0),
        kind,
        mode: r.get::<_, i64>(2) as u32,
        nlink: r.get(3),
        size: r.get::<_, i64>(4) as u64,
        content,
        mtime: r.get(6),
        ctime: r.get(7),
    })
}

#[async_trait]
impl MetadataStore for PostgresMetadataStore {
    async fn init(&self) -> Result<()> {
        let c = self.client().await?;
        let now = now_secs();
        c.batch_execute(
            "CREATE TABLE IF NOT EXISTS schema_meta(version BIGINT PRIMARY KEY, applied_at BIGINT NOT NULL);",
        )
        .await?;
        for m in MIGRATIONS {
            let applied = c
                .query_opt(
                    "SELECT 1 FROM schema_meta WHERE version = $1",
                    &[&m.version],
                )
                .await?
                .is_some();
            if !applied {
                c.batch_execute(m.postgres).await?;
                c.execute(
                    "INSERT INTO schema_meta(version, applied_at) VALUES ($1, $2)",
                    &[&m.version, &now],
                )
                .await?;
            }
        }
        // Root directory (ino=1), then advance the identity sequence past it.
        c.execute(
            "INSERT INTO inode(ino, kind, mode, nlink, size, content_hash, mtime, ctime)
             VALUES (1, 'dir', $1, 1, 0, NULL, $2, $2) ON CONFLICT (ino) DO NOTHING",
            &[&DIR_MODE, &now],
        )
        .await?;
        c.execute(
            "SELECT setval(pg_get_serial_sequence('inode', 'ino'), (SELECT MAX(ino) FROM inode))",
            &[],
        )
        .await?;
        Ok(())
    }

    async fn get_inode(&self, ino: Ino) -> Result<Option<Inode>> {
        let c = self.client().await?;
        let row = c
            .query_opt(
                "SELECT ino, kind, mode, nlink, size, content_hash, mtime, ctime
                 FROM inode WHERE ino = $1",
                &[&ino],
            )
            .await?;
        match row {
            Some(r) => Ok(Some(row_to_inode(&r)?)),
            None => Ok(None),
        }
    }

    async fn create_inode(&self, init: InodeInit) -> Result<Ino> {
        let c = self.client().await?;
        let now = now_secs();
        let mode = init.mode as i64;
        let row = c
            .query_one(
                "INSERT INTO inode(kind, mode, nlink, size, content_hash, mtime, ctime)
                 VALUES ($1, $2, 1, 0, NULL, $3, $3) RETURNING ino",
                &[&init.kind.as_str(), &mode, &now],
            )
            .await?;
        Ok(row.get(0))
    }

    async fn set_content(&self, ino: Ino, content: Option<Hash>, size: u64) -> Result<()> {
        let c = self.client().await?;
        let hex = content.map(|h| h.to_hex());
        let size = size as i64;
        let now = now_secs();
        c.execute(
            "UPDATE inode SET content_hash = $1, size = $2, mtime = $3, ctime = $3 WHERE ino = $4",
            &[&hex, &size, &now, &ino],
        )
        .await?;
        Ok(())
    }

    async fn set_nlink(&self, ino: Ino, nlink: i64) -> Result<()> {
        let c = self.client().await?;
        c.execute(
            "UPDATE inode SET nlink = $1 WHERE ino = $2",
            &[&nlink, &ino],
        )
        .await?;
        Ok(())
    }

    async fn delete_inode(&self, ino: Ino) -> Result<()> {
        let c = self.client().await?;
        c.execute("DELETE FROM symlink WHERE ino = $1", &[&ino])
            .await?;
        c.execute("DELETE FROM inode WHERE ino = $1", &[&ino])
            .await?;
        Ok(())
    }

    async fn lookup(&self, parent: Ino, name: &str) -> Result<Option<Ino>> {
        let c = self.client().await?;
        let row = c
            .query_opt(
                "SELECT ino FROM dentry WHERE parent_ino = $1 AND name = $2",
                &[&parent, &name],
            )
            .await?;
        Ok(row.map(|r| r.get(0)))
    }

    async fn add_dentry(&self, parent: Ino, name: &str, ino: Ino) -> Result<()> {
        let c = self.client().await?;
        match c
            .execute(
                "INSERT INTO dentry(parent_ino, name, ino) VALUES ($1, $2, $3)",
                &[&parent, &name, &ino],
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if e.code() == Some(&SqlState::UNIQUE_VIOLATION) => {
                Err(AfsError::AlreadyExists(name.to_string()))
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn remove_dentry(&self, parent: Ino, name: &str) -> Result<()> {
        let c = self.client().await?;
        c.execute(
            "DELETE FROM dentry WHERE parent_ino = $1 AND name = $2",
            &[&parent, &name],
        )
        .await?;
        Ok(())
    }

    async fn list_dir(&self, parent: Ino) -> Result<Vec<DirEntry>> {
        let c = self.client().await?;
        let rows = c
            .query(
                "SELECT d.name, d.ino, i.kind
                 FROM dentry d JOIN inode i ON i.ino = d.ino
                 WHERE d.parent_ino = $1
                 ORDER BY d.name",
                &[&parent],
            )
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let kind_s: String = r.get(2);
            let kind = FileKind::parse(&kind_s)
                .ok_or_else(|| AfsError::Metadata(format!("unknown inode kind {kind_s:?}")))?;
            out.push(DirEntry {
                name: r.get(0),
                ino: r.get(1),
                kind,
            });
        }
        Ok(out)
    }

    async fn child_count(&self, parent: Ino) -> Result<usize> {
        let c = self.client().await?;
        let row = c
            .query_one(
                "SELECT COUNT(*) FROM dentry WHERE parent_ino = $1",
                &[&parent],
            )
            .await?;
        Ok(row.get::<_, i64>(0) as usize)
    }

    async fn set_symlink(&self, ino: Ino, target: &str) -> Result<()> {
        let c = self.client().await?;
        c.execute(
            "INSERT INTO symlink(ino, target) VALUES ($1, $2)
             ON CONFLICT (ino) DO UPDATE SET target = EXCLUDED.target",
            &[&ino, &target],
        )
        .await?;
        Ok(())
    }

    async fn get_symlink(&self, ino: Ino) -> Result<Option<String>> {
        let c = self.client().await?;
        let row = c
            .query_opt("SELECT target FROM symlink WHERE ino = $1", &[&ino])
            .await?;
        Ok(row.map(|r| r.get(0)))
    }

    async fn get_ref(&self, name: &str) -> Result<Option<String>> {
        let c = self.client().await?;
        let row = c
            .query_opt("SELECT value FROM ref WHERE name = $1", &[&name])
            .await?;
        Ok(row.map(|r| r.get(0)))
    }

    async fn set_ref(&self, name: &str, value: &str) -> Result<()> {
        let c = self.client().await?;
        c.execute(
            "INSERT INTO ref(name, value) VALUES ($1, $2)
             ON CONFLICT (name) DO UPDATE SET value = EXCLUDED.value",
            &[&name, &value],
        )
        .await?;
        Ok(())
    }

    async fn cas_ref(&self, name: &str, expect: Option<&str>, new: &str) -> Result<bool> {
        let c = self.client().await?;
        let changed = match expect {
            None => {
                c.execute(
                    "INSERT INTO ref(name, value) VALUES ($1, $2) ON CONFLICT (name) DO NOTHING",
                    &[&name, &new],
                )
                .await?
            }
            Some(v) => {
                c.execute(
                    "UPDATE ref SET value = $1 WHERE name = $2 AND value = $3",
                    &[&new, &name, &v],
                )
                .await?
            }
        };
        Ok(changed == 1)
    }

    async fn delete_ref(&self, name: &str) -> Result<()> {
        let c = self.client().await?;
        c.execute("DELETE FROM ref WHERE name = $1", &[&name])
            .await?;
        Ok(())
    }

    async fn list_refs(&self) -> Result<Vec<(String, String)>> {
        let c = self.client().await?;
        let rows = c
            .query("SELECT name, value FROM ref ORDER BY name", &[])
            .await?;
        Ok(rows.into_iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    async fn get_config(&self, key: &str) -> Result<Option<String>> {
        let c = self.client().await?;
        let row = c
            .query_opt("SELECT value FROM config WHERE key = $1", &[&key])
            .await?;
        Ok(row.map(|r| r.get(0)))
    }

    async fn set_config(&self, key: &str, value: &str) -> Result<()> {
        let c = self.client().await?;
        c.execute(
            "INSERT INTO config(key, value) VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
            &[&key, &value],
        )
        .await?;
        Ok(())
    }

    async fn truncate_tree(&self) -> Result<()> {
        let c = self.client().await?;
        c.batch_execute(
            "DELETE FROM dentry; DELETE FROM symlink; DELETE FROM inode WHERE ino <> 1;",
        )
        .await?;
        Ok(())
    }

    async fn set_conflict(&self, path: &str, kind: &str) -> Result<()> {
        let c = self.client().await?;
        c.execute(
            "INSERT INTO conflict(path, kind) VALUES ($1, $2)
             ON CONFLICT (path) DO UPDATE SET kind = EXCLUDED.kind",
            &[&path, &kind],
        )
        .await?;
        Ok(())
    }

    async fn list_conflicts(&self) -> Result<Vec<(String, String)>> {
        let c = self.client().await?;
        let rows = c
            .query("SELECT path, kind FROM conflict ORDER BY path", &[])
            .await?;
        Ok(rows.into_iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    async fn clear_conflicts(&self) -> Result<()> {
        let c = self.client().await?;
        c.execute("DELETE FROM conflict", &[]).await?;
        Ok(())
    }

    async fn acquire_lock(&self, path: &str, owner: &str, at: i64) -> Result<bool> {
        let c = self.client().await?;
        let changed = c
            .execute(
                "INSERT INTO file_lock(path, owner, acquired_at) VALUES ($1, $2, $3)
                 ON CONFLICT (path) DO NOTHING",
                &[&path, &owner, &at],
            )
            .await?;
        Ok(changed == 1)
    }

    async fn release_lock(&self, path: &str, owner: &str) -> Result<bool> {
        let c = self.client().await?;
        let changed = c
            .execute(
                "DELETE FROM file_lock WHERE path = $1 AND owner = $2",
                &[&path, &owner],
            )
            .await?;
        Ok(changed == 1)
    }

    async fn list_locks(&self) -> Result<Vec<(String, String, i64)>> {
        let c = self.client().await?;
        let rows = c
            .query(
                "SELECT path, owner, acquired_at FROM file_lock ORDER BY path",
                &[],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.get(0), r.get(1), r.get(2)))
            .collect())
    }
}
