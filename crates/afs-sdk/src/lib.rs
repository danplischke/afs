//! afs-sdk — an ergonomic front door to an afs workspace.
//!
//! A workspace pairs a metadata store (SQLite or Postgres) with a pluggable
//! content backend (local dir, S3-compatible object store, in-memory, or a cached
//! tier). Both sides are `Arc<dyn …>`, so the backend is chosen at runtime. Later
//! milestones add commits and attribution behind the same façade.

use afs_core::{
    ContentStore, Fs, LocalCasStore, MetadataStore, ObjectContentStore, PostgresMetadataStore,
    Result, S3Config, SqliteMetadataStore,
};
use std::path::Path;
use std::sync::Arc;

pub use afs_core::{AfsError, DirEntry, FileKind, Inode, MemStore, TieredStore};
pub use bytes::Bytes;

type Meta = Arc<dyn MetadataStore>;
type Content = Arc<dyn ContentStore>;

/// A workspace: a metadata store over a content store.
pub struct Workspace {
    fs: Fs<Meta, Content>,
}

impl Workspace {
    /// Open (creating if needed) a workspace from explicit metadata + content
    /// backends.
    pub async fn open(meta: Meta, content: Content) -> Result<Self> {
        let fs = Fs::new(meta, content);
        fs.init().await?;
        Ok(Self { fs })
    }

    /// SQLite metadata + content-addressed blobs under a local directory.
    pub async fn open_local(db_path: impl AsRef<Path>, cas_dir: impl AsRef<Path>) -> Result<Self> {
        let meta: Meta = Arc::new(SqliteMetadataStore::open(db_path)?);
        let content: Content = Arc::new(LocalCasStore::open(cas_dir).await?);
        Self::open(meta, content).await
    }

    /// SQLite metadata + an S3-compatible object store for content.
    pub async fn open_s3(db_path: impl AsRef<Path>, cfg: S3Config) -> Result<Self> {
        let meta: Meta = Arc::new(SqliteMetadataStore::open(db_path)?);
        let content: Content = Arc::new(ObjectContentStore::s3(cfg)?);
        Self::open(meta, content).await
    }

    /// Postgres metadata (multi-writer) over the given content backend.
    pub async fn open_pg(dsn: &str, content: Content) -> Result<Self> {
        let meta: Meta = Arc::new(PostgresMetadataStore::connect(dsn).await?);
        Self::open(meta, content).await
    }

    /// Access the underlying engine for operations not surfaced here.
    pub fn fs(&self) -> &Fs<Meta, Content> {
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
