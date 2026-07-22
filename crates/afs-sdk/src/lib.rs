//! afs-sdk — an ergonomic front door to an afs workspace.
//!
//! A workspace pairs a SQLite metadata store with a pluggable content backend
//! (`Arc<dyn ContentStore>`): a local directory, an S3-compatible object store, an
//! in-memory store, or a cached tier over any of them. Later milestones add
//! commits and attribution behind the same façade.

use afs_core::{
    ContentStore, Fs, LocalCasStore, ObjectContentStore, Result, S3Config, SqliteMetadataStore,
};
use std::path::Path;
use std::sync::Arc;

pub use afs_core::{AfsError, DirEntry, FileKind, Inode, MemStore, TieredStore};
pub use bytes::Bytes;

type Content = Arc<dyn ContentStore>;

/// A workspace: SQLite metadata over a pluggable content store.
pub struct Workspace {
    fs: Fs<SqliteMetadataStore, Content>,
}

impl Workspace {
    /// Open (creating if needed) a workspace with metadata at `db_path` and the
    /// given content backend.
    pub async fn open(db_path: impl AsRef<Path>, content: Content) -> Result<Self> {
        let meta = SqliteMetadataStore::open(db_path)?;
        let fs = Fs::new(meta, content);
        fs.init().await?;
        Ok(Self { fs })
    }

    /// Open a workspace with content-addressed blobs under a local directory.
    pub async fn open_local(db_path: impl AsRef<Path>, cas_dir: impl AsRef<Path>) -> Result<Self> {
        let content: Content = Arc::new(LocalCasStore::open(cas_dir).await?);
        Self::open(db_path, content).await
    }

    /// Open a workspace backed by an S3-compatible object store.
    pub async fn open_s3(db_path: impl AsRef<Path>, cfg: S3Config) -> Result<Self> {
        let content: Content = Arc::new(ObjectContentStore::s3(cfg)?);
        Self::open(db_path, content).await
    }

    /// Access the underlying engine for operations not surfaced here.
    pub fn fs(&self) -> &Fs<SqliteMetadataStore, Content> {
        &self.fs
    }

    pub async fn write(&self, path: &str, data: &[u8]) -> Result<()> {
        self.fs.write(path, data).await
    }

    /// Write a file by streaming from a blocking reader (for large files).
    pub async fn write_reader<R: std::io::Read + Send + 'static>(
        &self,
        path: &str,
        reader: R,
    ) -> Result<()> {
        self.fs.write_reader(path, reader).await
    }

    pub async fn read(&self, path: &str) -> Result<Bytes> {
        self.fs.read(path).await
    }

    pub async fn read_range(&self, path: &str, off: u64, len: u64) -> Result<Bytes> {
        self.fs.read_range(path, off, len).await
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
