//! The metadata store: names, inodes, and (in later milestones) refs, commits,
//! and attribution. Content bytes never live here — only content addresses do
//! (`docs/DESIGN.md` §4b).

use crate::error::Result;
use crate::types::{DirEntry, Hash, Ino, Inode, InodeInit};
use async_trait::async_trait;
use std::sync::Arc;

/// Abstracts the metadata backend so the same engine runs on SQLite (M0) or
/// Postgres (M2). The trait is intentionally intent-level; SQL dialects stay
/// behind the implementation.
#[async_trait]
pub trait MetadataStore: Send + Sync {
    /// Create the schema (idempotent) and ensure the root directory (`INO_ROOT`).
    async fn init(&self) -> Result<()>;

    /// Fetch an inode by number.
    async fn get_inode(&self, ino: Ino) -> Result<Option<Inode>>;

    /// Allocate a new inode. `nlink` starts at 1; size at 0; no content.
    async fn create_inode(&self, init: InodeInit) -> Result<Ino>;

    /// Set an inode's content address and size (touches mtime/ctime).
    async fn set_content(&self, ino: Ino, content: Option<Hash>, size: u64) -> Result<()>;

    /// Set an inode's link count.
    async fn set_nlink(&self, ino: Ino, nlink: i64) -> Result<()>;

    /// Delete an inode and any symlink row. The caller ensures `nlink` hit 0.
    /// Reclaiming now-unreferenced content is deferred to GC (M9).
    async fn delete_inode(&self, ino: Ino) -> Result<()>;

    /// Resolve `name` within directory `parent`.
    async fn lookup(&self, parent: Ino, name: &str) -> Result<Option<Ino>>;

    /// Link `name` in `parent` to `ino`. Errors if the name already exists.
    async fn add_dentry(&self, parent: Ino, name: &str, ino: Ino) -> Result<()>;

    /// Unlink `name` from `parent` (no-op if absent).
    async fn remove_dentry(&self, parent: Ino, name: &str) -> Result<()>;

    /// List the entries of directory `parent`, ordered by name.
    async fn list_dir(&self, parent: Ino) -> Result<Vec<DirEntry>>;

    /// Number of entries directly under `parent`.
    async fn child_count(&self, parent: Ino) -> Result<usize>;

    /// Set (or replace) the target of a symlink inode.
    async fn set_symlink(&self, ino: Ino, target: &str) -> Result<()>;

    /// Fetch a symlink target, or `None` if `ino` is not a symlink.
    async fn get_symlink(&self, ino: Ino) -> Result<Option<String>>;
}

/// Delegating impl so `Arc<dyn MetadataStore>` (and `Arc<ConcreteStore>`) is
/// itself a [`MetadataStore`]. This lets a workspace pick its backend at runtime.
#[async_trait]
impl<T: MetadataStore + ?Sized> MetadataStore for Arc<T> {
    async fn init(&self) -> Result<()> {
        (**self).init().await
    }
    async fn get_inode(&self, ino: Ino) -> Result<Option<Inode>> {
        (**self).get_inode(ino).await
    }
    async fn create_inode(&self, init: InodeInit) -> Result<Ino> {
        (**self).create_inode(init).await
    }
    async fn set_content(&self, ino: Ino, content: Option<Hash>, size: u64) -> Result<()> {
        (**self).set_content(ino, content, size).await
    }
    async fn set_nlink(&self, ino: Ino, nlink: i64) -> Result<()> {
        (**self).set_nlink(ino, nlink).await
    }
    async fn delete_inode(&self, ino: Ino) -> Result<()> {
        (**self).delete_inode(ino).await
    }
    async fn lookup(&self, parent: Ino, name: &str) -> Result<Option<Ino>> {
        (**self).lookup(parent, name).await
    }
    async fn add_dentry(&self, parent: Ino, name: &str, ino: Ino) -> Result<()> {
        (**self).add_dentry(parent, name, ino).await
    }
    async fn remove_dentry(&self, parent: Ino, name: &str) -> Result<()> {
        (**self).remove_dentry(parent, name).await
    }
    async fn list_dir(&self, parent: Ino) -> Result<Vec<DirEntry>> {
        (**self).list_dir(parent).await
    }
    async fn child_count(&self, parent: Ino) -> Result<usize> {
        (**self).child_count(parent).await
    }
    async fn set_symlink(&self, ino: Ino, target: &str) -> Result<()> {
        (**self).set_symlink(ino, target).await
    }
    async fn get_symlink(&self, ino: Ino) -> Result<Option<String>> {
        (**self).get_symlink(ino).await
    }
}
