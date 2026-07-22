//! An [`ObjectStore`]-backed [`ContentStore`] — the object-storage / remote
//! backend (`docs/DESIGN.md` §4a).
//!
//! One adapter serves S3, R2, GCS (S3 API), MinIO, and an in-memory store via the
//! `object_store` crate. Because the in-memory store runs the *same* adapter code
//! as S3, the FS test suite that passes on `in_memory()` exercises the S3 path
//! (modulo network + credentials).

use crate::content::ContentStore;
use crate::error::{AfsError, Result};
use crate::types::Hash;
use async_trait::async_trait;
use bytes::Bytes;
use object_store::path::Path as OsPath;
use object_store::{ObjectStore, PutPayload};
use std::sync::Arc;

/// Connection settings for an S3-compatible backend.
#[derive(Clone, Debug, Default)]
pub struct S3Config {
    pub bucket: String,
    pub region: String,
    /// Custom endpoint (MinIO/R2/localstack). Omit for AWS S3.
    pub endpoint: Option<String>,
    /// Allow plain HTTP (for local MinIO). Ignored without a custom endpoint.
    pub allow_http: bool,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    /// Key prefix for stored objects (default `objects`).
    pub prefix: Option<String>,
}

/// A content-addressed store over any `object_store` backend.
pub struct ObjectContentStore {
    store: Arc<dyn ObjectStore>,
    prefix: String,
}

impl ObjectContentStore {
    pub fn new(store: Arc<dyn ObjectStore>, prefix: impl Into<String>) -> Self {
        Self {
            store,
            prefix: prefix.into(),
        }
    }

    /// An in-memory object store — same adapter as S3, no network. For tests.
    pub fn in_memory() -> Self {
        Self::new(Arc::new(object_store::memory::InMemory::new()), "objects")
    }

    /// Build an S3-compatible content store (S3 / R2 / GCS-S3 / MinIO).
    pub fn s3(cfg: S3Config) -> Result<Self> {
        let mut builder = object_store::aws::AmazonS3Builder::new()
            .with_bucket_name(&cfg.bucket)
            .with_region(&cfg.region);
        if let Some(endpoint) = &cfg.endpoint {
            builder = builder
                .with_endpoint(endpoint)
                .with_allow_http(cfg.allow_http);
        }
        if let (Some(k), Some(s)) = (&cfg.access_key_id, &cfg.secret_access_key) {
            builder = builder.with_access_key_id(k).with_secret_access_key(s);
        }
        let store = builder.build().map_err(AfsError::from)?;
        let prefix = cfg.prefix.clone().unwrap_or_else(|| "objects".to_string());
        Ok(Self::new(Arc::new(store), prefix))
    }

    fn path_for(&self, hash: &Hash) -> OsPath {
        let hex = hash.to_hex();
        OsPath::from(format!("{}/{}/{}", self.prefix, &hex[0..2], &hex[2..]))
    }
}

#[async_trait]
impl ContentStore for ObjectContentStore {
    async fn put(&self, bytes: &[u8]) -> Result<Hash> {
        let hash = Hash::of(bytes);
        let path = self.path_for(&hash);
        // Idempotent: content-addressed, so an existing object is identical.
        if self.store.head(&path).await.is_ok() {
            return Ok(hash);
        }
        self.store
            .put(&path, PutPayload::from(bytes.to_vec()))
            .await
            .map_err(AfsError::from)?;
        Ok(hash)
    }

    async fn put_keyed(&self, key: &Hash, bytes: &[u8]) -> Result<()> {
        let path = self.path_for(key);
        if self.store.head(&path).await.is_ok() {
            return Ok(());
        }
        self.store
            .put(&path, PutPayload::from(bytes.to_vec()))
            .await
            .map_err(AfsError::from)?;
        Ok(())
    }

    async fn get(&self, hash: &Hash) -> Result<Bytes> {
        let path = self.path_for(hash);
        match self.store.get(&path).await {
            Ok(result) => result.bytes().await.map_err(AfsError::from),
            Err(object_store::Error::NotFound { .. }) => {
                Err(AfsError::ContentMissing(hash.to_hex()))
            }
            Err(e) => Err(AfsError::from(e)),
        }
    }

    async fn get_range(&self, hash: &Hash, off: u64, len: u64) -> Result<Bytes> {
        let path = self.path_for(hash);
        let meta = match self.store.head(&path).await {
            Ok(m) => m,
            Err(object_store::Error::NotFound { .. }) => {
                return Err(AfsError::ContentMissing(hash.to_hex()));
            }
            Err(e) => return Err(AfsError::from(e)),
        };
        let size = meta.size;
        let start = off.min(size);
        let end = start.saturating_add(len).min(size);
        if start >= end {
            return Ok(Bytes::new());
        }
        self.store
            .get_range(&path, start..end)
            .await
            .map_err(AfsError::from)
    }

    async fn has(&self, hash: &Hash) -> Result<bool> {
        match self.store.head(&self.path_for(hash)).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(AfsError::from(e)),
        }
    }

    async fn list(&self) -> Result<Vec<Hash>> {
        use futures::StreamExt;
        let prefix = OsPath::from(self.prefix.clone());
        let mut stream = self.store.list(Some(&prefix));
        let mut out = Vec::new();
        while let Some(meta) = stream.next().await {
            let location = meta.map_err(AfsError::from)?.location;
            // `<prefix>/<aa>/<rest>` -> the 64-char hex address.
            let parts: Vec<&str> = location.as_ref().rsplit('/').collect();
            if parts.len() >= 2
                && let Some(h) = Hash::from_hex(&format!("{}{}", parts[1], parts[0]))
            {
                out.push(h);
            }
        }
        Ok(out)
    }

    async fn delete(&self, hash: &Hash) -> Result<u64> {
        let path = self.path_for(hash);
        let size = match self.store.head(&path).await {
            Ok(m) => m.size,
            Err(object_store::Error::NotFound { .. }) => return Ok(0),
            Err(e) => return Err(AfsError::from(e)),
        };
        match self.store.delete(&path).await {
            Ok(()) => Ok(size),
            Err(object_store::Error::NotFound { .. }) => Ok(0),
            Err(e) => Err(AfsError::from(e)),
        }
    }
}
