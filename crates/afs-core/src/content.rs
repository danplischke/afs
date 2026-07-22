//! The content store: a pluggable, content-addressed blob store (`docs/DESIGN.md` §4a).
//!
//! M0 ships one backend, [`LocalCasStore`], which keeps blobs in a sharded
//! directory. M1 adds FastCDC chunking + manifests and an S3 backend behind the
//! same [`ContentStore`] trait.

use crate::error::{AfsError, Result};
use crate::types::Hash;
use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A content-addressed blob store. Writes are idempotent: storing identical
/// bytes yields the same [`Hash`] and does not duplicate storage.
#[async_trait]
pub trait ContentStore: Send + Sync {
    /// Store `bytes` and return their content address.
    async fn put(&self, bytes: &[u8]) -> Result<Hash>;

    /// Fetch the full blob for `hash`.
    async fn get(&self, hash: &Hash) -> Result<Bytes>;

    /// Fetch the byte range `[off, off + len)`, clamped to the blob's end.
    async fn get_range(&self, hash: &Hash, off: u64, len: u64) -> Result<Bytes>;

    /// Whether `hash` is present.
    async fn has(&self, hash: &Hash) -> Result<bool>;
}

/// A content-addressed store backed by a local directory.
///
/// Blobs live at `<root>/objects/<aa>/<rest-of-hex>`, sharded by the first byte
/// of the hash to keep directories small.
pub struct LocalCasStore {
    root: PathBuf,
}

impl LocalCasStore {
    /// Open (creating if needed) a store rooted at `root`.
    pub async fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        tokio::fs::create_dir_all(root.join("objects")).await?;
        Ok(Self { root })
    }

    fn path_for(&self, hash: &Hash) -> PathBuf {
        let hex = hash.to_hex();
        self.root.join("objects").join(&hex[0..2]).join(&hex[2..])
    }

    async fn exists(path: &Path) -> bool {
        tokio::fs::metadata(path).await.is_ok()
    }
}

#[async_trait]
impl ContentStore for LocalCasStore {
    async fn put(&self, bytes: &[u8]) -> Result<Hash> {
        let hash = Hash::of(bytes);
        let path = self.path_for(&hash);
        if Self::exists(&path).await {
            return Ok(hash); // already stored — content-addressed, so identical
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // Write to a temp sibling then rename, so readers never see a partial blob.
        let tmp = path.with_extension("tmp");
        tokio::fs::write(&tmp, bytes).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(hash)
    }

    async fn get(&self, hash: &Hash) -> Result<Bytes> {
        let path = self.path_for(hash);
        match tokio::fs::read(&path).await {
            Ok(v) => Ok(Bytes::from(v)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(AfsError::ContentMissing(hash.to_hex()))
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn get_range(&self, hash: &Hash, off: u64, len: u64) -> Result<Bytes> {
        // M0 reads the whole blob then slices. M1's chunk manifests make this a
        // true ranged read that only fetches the covering chunks.
        let full = self.get(hash).await?;
        let start = (off as usize).min(full.len());
        let end = start.saturating_add(len as usize).min(full.len());
        Ok(full.slice(start..end))
    }

    async fn has(&self, hash: &Hash) -> Result<bool> {
        Ok(Self::exists(&self.path_for(hash)).await)
    }
}

/// Delegating impl so `Arc<dyn ContentStore>` (and `Arc<ConcreteStore>`) is itself
/// a [`ContentStore`]. This lets the engine and [`TieredStore`] hold trait objects.
#[async_trait]
impl<T: ContentStore + ?Sized> ContentStore for Arc<T> {
    async fn put(&self, bytes: &[u8]) -> Result<Hash> {
        (**self).put(bytes).await
    }
    async fn get(&self, hash: &Hash) -> Result<Bytes> {
        (**self).get(hash).await
    }
    async fn get_range(&self, hash: &Hash, off: u64, len: u64) -> Result<Bytes> {
        (**self).get_range(hash, off, len).await
    }
    async fn has(&self, hash: &Hash) -> Result<bool> {
        (**self).has(hash).await
    }
}

/// An in-memory content store — for tests and ephemeral workspaces.
#[derive(Default)]
pub struct MemStore {
    map: Mutex<HashMap<Hash, Bytes>>,
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of distinct blobs stored (useful for dedup assertions in tests).
    pub fn len(&self) -> usize {
        self.map.lock().expect("mem store poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl ContentStore for MemStore {
    async fn put(&self, bytes: &[u8]) -> Result<Hash> {
        let hash = Hash::of(bytes);
        self.map
            .lock()
            .expect("mem store poisoned")
            .entry(hash)
            .or_insert_with(|| Bytes::copy_from_slice(bytes));
        Ok(hash)
    }

    async fn get(&self, hash: &Hash) -> Result<Bytes> {
        self.map
            .lock()
            .expect("mem store poisoned")
            .get(hash)
            .cloned()
            .ok_or_else(|| AfsError::ContentMissing(hash.to_hex()))
    }

    async fn get_range(&self, hash: &Hash, off: u64, len: u64) -> Result<Bytes> {
        let full = self.get(hash).await?;
        let start = (off as usize).min(full.len());
        let end = start.saturating_add(len as usize).min(full.len());
        Ok(full.slice(start..end))
    }

    async fn has(&self, hash: &Hash) -> Result<bool> {
        Ok(self
            .map
            .lock()
            .expect("mem store poisoned")
            .contains_key(hash))
    }
}

/// A two-tier store: a fast local `cache` in front of a (possibly remote)
/// `backend` (`docs/DESIGN.md` §4a). Reads are served from cache and populate it
/// on miss; writes are write-through to the backend and cached best-effort.
///
/// M1 is write-through for durability simplicity; write-back batching is a later
/// optimization. [`TieredStore::prefetch`] warms the cache for a file's chunks.
pub struct TieredStore {
    cache: Arc<dyn ContentStore>,
    backend: Arc<dyn ContentStore>,
}

impl TieredStore {
    pub fn new(cache: Arc<dyn ContentStore>, backend: Arc<dyn ContentStore>) -> Self {
        Self { cache, backend }
    }

    /// Warm the cache with `hashes` (e.g. a manifest's chunks, on open).
    pub async fn prefetch(&self, hashes: &[Hash]) -> Result<()> {
        for h in hashes {
            if !self.cache.has(h).await?
                && let Ok(bytes) = self.backend.get(h).await
            {
                let _ = self.cache.put(&bytes).await;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl ContentStore for TieredStore {
    async fn put(&self, bytes: &[u8]) -> Result<Hash> {
        let hash = self.backend.put(bytes).await?;
        let _ = self.cache.put(bytes).await; // best-effort
        Ok(hash)
    }

    async fn get(&self, hash: &Hash) -> Result<Bytes> {
        if let Ok(bytes) = self.cache.get(hash).await {
            return Ok(bytes);
        }
        let bytes = self.backend.get(hash).await?;
        let _ = self.cache.put(&bytes).await;
        Ok(bytes)
    }

    async fn get_range(&self, hash: &Hash, off: u64, len: u64) -> Result<Bytes> {
        if self.cache.has(hash).await? {
            return self.cache.get_range(hash, off, len).await;
        }
        self.backend.get_range(hash, off, len).await
    }

    async fn has(&self, hash: &Hash) -> Result<bool> {
        Ok(self.cache.has(hash).await? || self.backend.has(hash).await?)
    }
}
