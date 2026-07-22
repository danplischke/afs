//! One suite run across every content-store backend. The in-memory object store
//! exercises the *same* adapter as S3, so passing here validates the S3 path
//! (modulo network/credentials). A real S3 run is gated behind env vars below.

use afs_core::{ContentStore, Hash, LocalCasStore, MemStore, ObjectContentStore, TieredStore};
use std::sync::Arc;

async fn suite<C: ContentStore>(store: C) {
    // put is content-addressed and idempotent
    let h = store.put(b"hello world").await.unwrap();
    assert_eq!(h, Hash::of(b"hello world"));
    assert_eq!(store.put(b"hello world").await.unwrap(), h);

    assert!(store.has(&h).await.unwrap());
    assert_eq!(&store.get(&h).await.unwrap()[..], b"hello world");

    // ranged reads, clamped to the blob end
    assert_eq!(&store.get_range(&h, 0, 5).await.unwrap()[..], b"hello");
    assert_eq!(&store.get_range(&h, 6, 100).await.unwrap()[..], b"world");
    assert_eq!(&store.get_range(&h, 100, 10).await.unwrap()[..], b"");

    // absent content
    let missing = Hash::of(b"nope");
    assert!(!store.has(&missing).await.unwrap());
    assert!(store.get(&missing).await.is_err());
}

#[tokio::test]
async fn mem_store() {
    suite(MemStore::new()).await;
}

#[tokio::test]
async fn local_cas_store() {
    let dir = tempfile::tempdir().unwrap();
    suite(LocalCasStore::open(dir.path()).await.unwrap()).await;
}

#[tokio::test]
async fn object_store_in_memory() {
    // Same adapter code path as S3.
    suite(ObjectContentStore::in_memory()).await;
}

#[tokio::test]
async fn tiered_store() {
    let dir = tempfile::tempdir().unwrap();
    let cache: Arc<dyn ContentStore> = Arc::new(LocalCasStore::open(dir.path()).await.unwrap());
    let backend: Arc<dyn ContentStore> = Arc::new(MemStore::new());
    suite(TieredStore::new(cache, backend)).await;
}

#[tokio::test]
async fn tiered_read_through_populates_cache() {
    let dir = tempfile::tempdir().unwrap();
    let cache: Arc<dyn ContentStore> = Arc::new(LocalCasStore::open(dir.path()).await.unwrap());
    let backend: Arc<dyn ContentStore> = Arc::new(MemStore::new());
    // Seed only the backend, then read through the tier.
    let h = backend.put(b"cached-through").await.unwrap();
    assert!(!cache.has(&h).await.unwrap());

    let tier = TieredStore::new(cache.clone(), backend);
    assert_eq!(&tier.get(&h).await.unwrap()[..], b"cached-through");
    assert!(
        cache.has(&h).await.unwrap(),
        "read should populate the cache"
    );
}

/// Real S3-compatible run. Set the env vars to enable (e.g. against MinIO):
///   AFS_S3_TEST_BUCKET, AFS_S3_TEST_REGION, AFS_S3_TEST_ENDPOINT,
///   AFS_S3_TEST_ACCESS_KEY_ID, AFS_S3_TEST_SECRET_ACCESS_KEY
#[tokio::test]
#[ignore = "requires an S3-compatible endpoint; set AFS_S3_TEST_* to run"]
async fn s3_backend() {
    use afs_core::S3Config;
    let bucket = std::env::var("AFS_S3_TEST_BUCKET").expect("AFS_S3_TEST_BUCKET");
    let cfg = S3Config {
        bucket,
        region: std::env::var("AFS_S3_TEST_REGION").unwrap_or_else(|_| "us-east-1".into()),
        endpoint: std::env::var("AFS_S3_TEST_ENDPOINT").ok(),
        allow_http: true,
        access_key_id: std::env::var("AFS_S3_TEST_ACCESS_KEY_ID").ok(),
        secret_access_key: std::env::var("AFS_S3_TEST_SECRET_ACCESS_KEY").ok(),
        prefix: Some("afs-test".into()),
    };
    suite(ObjectContentStore::s3(cfg).unwrap()).await;
}
