//! afs-sdk — an ergonomic front door to an afs workspace.
//!
//! M0 exposes a single local workspace type backed by SQLite metadata and a
//! local content-addressed store. Later milestones add remote backends, commits,
//! and attribution behind the same façade.

use afs_core::{Fs, LocalCasStore, Result, SqliteMetadataStore};
use std::path::Path;

pub use afs_core::{AfsError, DirEntry, FileKind, Inode};
pub use bytes::Bytes;

/// A concrete local workspace: SQLite metadata + a local blob store.
pub struct Workspace {
    fs: Fs<SqliteMetadataStore, LocalCasStore>,
}

impl Workspace {
    /// Open (creating if needed) a local workspace with the metadata database at
    /// `db_path` and content-addressed blobs under `cas_dir`.
    pub async fn open_local(db_path: impl AsRef<Path>, cas_dir: impl AsRef<Path>) -> Result<Self> {
        let meta = SqliteMetadataStore::open(db_path)?;
        let content = LocalCasStore::open(cas_dir).await?;
        let fs = Fs::new(meta, content);
        fs.init().await?;
        Ok(Self { fs })
    }

    /// Access the underlying engine for operations not surfaced here.
    pub fn fs(&self) -> &Fs<SqliteMetadataStore, LocalCasStore> {
        &self.fs
    }

    pub async fn write(&self, path: &str, data: &[u8]) -> Result<()> {
        self.fs.write(path, data).await
    }

    pub async fn read(&self, path: &str) -> Result<Bytes> {
        self.fs.read(path).await
    }

    pub async fn mkdir_p(&self, path: &str) -> Result<()> {
        self.fs.mkdir_p(path).await.map(|_| ())
    }

    pub async fn ls(&self, path: &str) -> Result<Vec<DirEntry>> {
        self.fs.ls(path).await
    }

    pub async fn stat(&self, path: &str) -> Result<Inode> {
        self.fs.stat(path).await
    }

    pub async fn remove(&self, path: &str) -> Result<()> {
        self.fs.remove(path).await
    }

    pub async fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.fs.rename(from, to).await
    }
}
