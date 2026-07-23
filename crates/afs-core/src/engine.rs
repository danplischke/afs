//! The working-tree engine: POSIX-flavored operations over a [`MetadataStore`]
//! plus a [`ContentStore`].
//!
//! This is the mutable working tree of `docs/DESIGN.md` §3. In M0 it is the whole
//! story (no commits yet); later milestones layer commits/branches (M3), merge
//! (M4), and attribution (M6) on top without changing this surface.

use crate::chunk::{AVG_CHUNK, ChunkRef, MAX_CHUNK, MIN_CHUNK, Manifest, chunk_bounds};
use crate::content::ContentStore;
use crate::error::{AfsError, Result};
use crate::metadata::{MetaTxn, MetadataStore};
use crate::types::{DirEntry, FileKind, Hash, INO_ROOT, Ino, Inode, InodeInit};
use bytes::{Bytes, BytesMut};

const DIR_MODE: u32 = 0o040755;
const FILE_MODE: u32 = 0o100644;
const SYMLINK_MODE: u32 = 0o120777;

/// Reject a single path component that could escape the workspace tree or
/// corrupt the dentry graph: the traversal names `.`/`..`, an empty name, or a
/// name embedding a path separator or NUL. Enforced at every metadata boundary
/// (path resolution and the inode-oriented FUSE/NFS ops) so a poisoned name can
/// never be *stored* — which is what stops it from later escaping during a host
/// materialization such as the sandbox's `export_tree` (`host_dir.join("..")`).
pub(crate) fn validate_component(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(AfsError::InvalidPath(format!(
            "invalid path component: {name:?}"
        )));
    }
    Ok(())
}

/// A filesystem over a metadata store and a content store.
#[derive(Clone)]
pub struct Fs<M: MetadataStore, C: ContentStore> {
    pub meta: M,
    pub content: C,
}

impl<M: MetadataStore, C: ContentStore> Fs<M, C> {
    pub fn new(meta: M, content: C) -> Self {
        Self { meta, content }
    }

    /// Initialize the metadata schema, the root directory, and versioning state
    /// (HEAD → `main`, default `versioning = native`).
    pub async fn init(&self) -> Result<()> {
        self.meta.init().await?;
        self.init_versioning().await?;
        Ok(())
    }

    // --- path helpers -----------------------------------------------------

    /// Split an absolute path into its non-empty segments, rejecting any
    /// traversal component (`.`/`..`) so no path can escape the workspace root.
    fn split(path: &str) -> Result<Vec<&str>> {
        if !path.starts_with('/') {
            return Err(AfsError::InvalidPath(format!(
                "path must be absolute: {path}"
            )));
        }
        let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        for seg in &segs {
            validate_component(seg)?;
        }
        Ok(segs)
    }

    /// Resolve an absolute path to its inode.
    pub(crate) async fn resolve(&self, path: &str) -> Result<Ino> {
        let mut ino = INO_ROOT;
        for seg in Self::split(path)? {
            ino = self
                .meta
                .lookup(ino, seg)
                .await?
                .ok_or_else(|| AfsError::NotFound(path.to_string()))?;
        }
        Ok(ino)
    }

    /// Resolve a path's parent directory inode and return `(parent, basename)`.
    pub(crate) async fn resolve_parent<'a>(&self, path: &'a str) -> Result<(Ino, &'a str)> {
        let segs = Self::split(path)?;
        let (name, dirs) = segs
            .split_last()
            .ok_or_else(|| AfsError::InvalidPath(format!("no basename in {path}")))?;
        let mut ino = INO_ROOT;
        for &seg in dirs {
            ino = self
                .meta
                .lookup(ino, seg)
                .await?
                .ok_or_else(|| AfsError::NotFound(path.to_string()))?;
        }
        Ok((ino, *name))
    }

    pub(crate) async fn ensure_dir(&self, ino: Ino) -> Result<()> {
        let inode = self
            .meta
            .get_inode(ino)
            .await?
            .ok_or_else(|| AfsError::NotFound(format!("ino {ino}")))?;
        if inode.kind != FileKind::Dir {
            return Err(AfsError::NotADirectory(format!("ino {ino}")));
        }
        Ok(())
    }

    // --- directory operations --------------------------------------------

    /// Create a single directory; its parent must already exist.
    pub async fn mkdir(&self, path: &str) -> Result<Ino> {
        let (parent, name) = self.resolve_parent(path).await?;
        self.ensure_dir(parent).await?;
        if self.meta.lookup(parent, name).await?.is_some() {
            return Err(AfsError::AlreadyExists(path.to_string()));
        }
        // Inode + dentry commit together, so a failed link can't orphan the
        // inode (C1/M6).
        let mut tx = self.meta.begin().await?;
        let ino = tx
            .create_inode(InodeInit {
                kind: FileKind::Dir,
                mode: DIR_MODE,
            })
            .await?;
        tx.add_dentry(parent, name, ino).await?;
        tx.commit().await?;
        Ok(ino)
    }

    /// Create a directory and any missing parents (like `mkdir -p`).
    /// Returns the inode of the final component (root for `/`).
    pub async fn mkdir_p(&self, path: &str) -> Result<Ino> {
        let mut ino = INO_ROOT;
        for seg in Self::split(path)? {
            match self.meta.lookup(ino, seg).await? {
                Some(child) => {
                    let inode = self
                        .meta
                        .get_inode(child)
                        .await?
                        .ok_or_else(|| AfsError::NotFound(path.to_string()))?;
                    if inode.kind != FileKind::Dir {
                        return Err(AfsError::NotADirectory(path.to_string()));
                    }
                    ino = child;
                }
                None => {
                    // Create this segment atomically (inode + dentry). If a
                    // concurrent writer wins the race, `add_dentry` errors on the
                    // unique index; the transaction rolls back (no orphaned
                    // inode) and we adopt the directory they created, keeping
                    // `mkdir -p` idempotent under concurrency (C1/M6).
                    let mut tx = self.meta.begin().await?;
                    let child = tx
                        .create_inode(InodeInit {
                            kind: FileKind::Dir,
                            mode: DIR_MODE,
                        })
                        .await?;
                    match tx.add_dentry(ino, seg, child).await {
                        Ok(()) => {
                            tx.commit().await?;
                            ino = child;
                        }
                        Err(AfsError::AlreadyExists(_)) => {
                            drop(tx); // roll back the just-created inode
                            let existing = self
                                .meta
                                .lookup(ino, seg)
                                .await?
                                .ok_or_else(|| AfsError::NotFound(path.to_string()))?;
                            let inode = self
                                .meta
                                .get_inode(existing)
                                .await?
                                .ok_or_else(|| AfsError::NotFound(path.to_string()))?;
                            if inode.kind != FileKind::Dir {
                                return Err(AfsError::NotADirectory(path.to_string()));
                            }
                            ino = existing;
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
        }
        Ok(ino)
    }

    /// Remove an empty directory.
    pub async fn rmdir(&self, path: &str) -> Result<()> {
        let (parent, name) = self.resolve_parent(path).await?;
        let ino = self
            .meta
            .lookup(parent, name)
            .await?
            .ok_or_else(|| AfsError::NotFound(path.to_string()))?;
        let inode = self
            .meta
            .get_inode(ino)
            .await?
            .ok_or_else(|| AfsError::NotFound(path.to_string()))?;
        if inode.kind != FileKind::Dir {
            return Err(AfsError::NotADirectory(path.to_string()));
        }
        if self.meta.child_count(ino).await? > 0 {
            return Err(AfsError::DirectoryNotEmpty(path.to_string()));
        }
        // Unlink + free the inode atomically (C1/L3).
        let mut tx = self.meta.begin().await?;
        tx.remove_dentry(parent, name).await?;
        tx.delete_inode(ino).await?;
        tx.commit().await?;
        Ok(())
    }

    /// List a directory's entries, ordered by name.
    pub async fn ls(&self, path: &str) -> Result<Vec<DirEntry>> {
        let ino = self.resolve(path).await?;
        self.ensure_dir(ino).await?;
        self.meta.list_dir(ino).await
    }

    // --- file operations --------------------------------------------------

    /// Resolve the *existing* file inode for `(parent, name)`, or `None` if the
    /// name is free. Errors if the name exists but is a directory. Creating a
    /// missing file is deferred to the caller's transaction (via
    /// [`create_file_in`](Self::create_file_in)) so the new inode, its dentry,
    /// and its content all commit atomically (C1/M6).
    pub(crate) async fn lookup_file(
        &self,
        parent: Ino,
        name: &str,
        path: &str,
    ) -> Result<Option<Ino>> {
        match self.meta.lookup(parent, name).await? {
            Some(existing) => {
                let inode = self
                    .meta
                    .get_inode(existing)
                    .await?
                    .ok_or_else(|| AfsError::NotFound(path.to_string()))?;
                if inode.kind == FileKind::Dir {
                    return Err(AfsError::IsADirectory(path.to_string()));
                }
                Ok(Some(existing))
            }
            None => Ok(None),
        }
    }

    /// Create a fresh regular-file inode and link it under `(parent, name)`,
    /// inside `tx`. Pairs with [`lookup_file`](Self::lookup_file): if the name
    /// was taken by a concurrent writer, `add_dentry` errors on the unique index
    /// and the whole transaction rolls back rather than orphaning the inode.
    pub(crate) async fn create_file_in(
        tx: &mut dyn MetaTxn,
        parent: Ino,
        name: &str,
    ) -> Result<Ino> {
        let ino = tx
            .create_inode(InodeInit {
                kind: FileKind::File,
                mode: FILE_MODE,
            })
            .await?;
        tx.add_dentry(parent, name, ino).await?;
        Ok(ino)
    }

    /// Chunk `data` (content-defined), store each chunk, and write a manifest.
    /// Returns `(manifest_hash, size)`; an empty body yields `(None, 0)`.
    pub(crate) async fn store_body(&self, data: &[u8]) -> Result<(Option<Hash>, u64)> {
        if data.is_empty() {
            return Ok((None, 0));
        }
        let mut chunks = Vec::new();
        for (off, len) in chunk_bounds(data) {
            let hash = self.content.put(&data[off..off + len]).await?;
            chunks.push(ChunkRef {
                hash,
                len: len as u32,
            });
        }
        let manifest = Manifest {
            size: data.len() as u64,
            chunks,
        };
        let mhash = self.content.put(&manifest.encode()).await?;
        // Durability barrier (C4): make the content durable before the metadata
        // commit that will reference it. For LocalCasStore each `put` already
        // fsynced; for PackStore this seals the open pack so a crash can't lose
        // chunks that only lived in the in-memory buffer while metadata points
        // at them. Most backends flush immediately, so this is a cheap no-op.
        self.content.flush().await?;
        Ok((Some(mhash), manifest.size))
    }

    pub(crate) async fn load_manifest(&self, mhash: &Hash) -> Result<Manifest> {
        let bytes = self.content.get(mhash).await?;
        Manifest::decode(&bytes)
    }

    /// Write `data` as the entire contents of `path`, creating the file if needed.
    /// The body is content-defined-chunked; unchanged chunks are deduplicated.
    pub async fn write(&self, path: &str, data: &[u8]) -> Result<()> {
        let (parent, name) = self.resolve_parent(path).await?;
        self.ensure_dir(parent).await?;
        let existing = self.lookup_file(parent, name, path).await?;
        // Content is made durable first (store_body flushes), then the metadata
        // that references it commits atomically: for a new file the inode, its
        // dentry, and its content all land together or not at all (C1).
        let (mhash, size) = self.store_body(data).await?;
        let mut tx = self.meta.begin().await?;
        let ino = match existing {
            Some(ino) => ino,
            None => Self::create_file_in(tx.as_mut(), parent, name).await?,
        };
        tx.set_content(ino, mhash, size).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Write a file by streaming from a blocking reader, chunking incrementally so
    /// large files never need to be fully resident. Creates the file if needed.
    pub async fn write_reader<R>(&self, path: &str, reader: R) -> Result<()>
    where
        R: std::io::Read + Send + 'static,
    {
        let (parent, name) = self.resolve_parent(path).await?;
        self.ensure_dir(parent).await?;
        let existing = self.lookup_file(parent, name, path).await?;

        // Chunk on the blocking pool; deliver one chunk at a time to the async side.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<std::result::Result<Vec<u8>, String>>(8);
        let handle = tokio::task::spawn_blocking(move || {
            for item in fastcdc::v2020::StreamCDC::new(reader, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK) {
                match item {
                    Ok(chunk) => {
                        if tx.blocking_send(Ok(chunk.data)).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.blocking_send(Err(e.to_string()));
                        break;
                    }
                }
            }
        });

        let mut chunks = Vec::new();
        let mut size: u64 = 0;
        while let Some(item) = rx.recv().await {
            let data = item.map_err(AfsError::Content)?;
            size += data.len() as u64;
            let hash = self.content.put(&data).await?;
            chunks.push(ChunkRef {
                hash,
                len: data.len() as u32,
            });
        }
        let _ = handle.await;

        let mhash = if size == 0 {
            None
        } else {
            let manifest = Manifest { size, chunks };
            Some(self.content.put(&manifest.encode()).await?)
        };
        // Durability barrier (C4): seal/flush content before metadata references it.
        self.content.flush().await?;
        // Commit the metadata atomically — the txn spans only this fast final
        // step, not the whole stream, so a large upload doesn't hold the write
        // lock while chunking.
        let mut txn = self.meta.begin().await?;
        let ino = match existing {
            Some(ino) => ino,
            None => Self::create_file_in(txn.as_mut(), parent, name).await?,
        };
        txn.set_content(ino, mhash, size).await?;
        txn.commit().await?;
        Ok(())
    }

    /// Read the entire contents of a file.
    pub async fn read(&self, path: &str) -> Result<Bytes> {
        let ino = self.resolve(path).await?;
        let inode = self
            .meta
            .get_inode(ino)
            .await?
            .ok_or_else(|| AfsError::NotFound(path.to_string()))?;
        match inode.kind {
            FileKind::Dir => Err(AfsError::IsADirectory(path.to_string())),
            FileKind::Symlink => Err(AfsError::InvalidArgument(format!("{path} is a symlink"))),
            FileKind::File => match inode.content {
                None => Ok(Bytes::new()),
                Some(mhash) => self.content_bytes(&mhash).await,
            },
        }
    }

    /// Reassemble a file body from its manifest hash (the content address stored
    /// on a file inode / tree entry). Used by `read` and by the diff API to
    /// reconstruct a specific version's bytes.
    pub(crate) async fn content_bytes(&self, mhash: &Hash) -> Result<Bytes> {
        let manifest = self.load_manifest(mhash).await?;
        // Don't trust the manifest's declared sizes for the up-front allocation:
        // even though `Manifest::decode` checks `size == Σ chunk.len`, a crafted
        // manifest can still declare many oversized chunk lengths and drive a
        // huge `with_capacity` that aborts the process. Reserve a bounded amount
        // and let the buffer grow as real chunk bytes arrive, enforcing the true
        // file-size ceiling on the accumulated bytes.
        const INITIAL_HINT: usize = 8 * 1024 * 1024;
        let hint = manifest
            .chunks
            .iter()
            .fold(0usize, |a, c| a.saturating_add(c.len as usize))
            .min(INITIAL_HINT);
        let mut buf = BytesMut::with_capacity(hint);
        for c in &manifest.chunks {
            let chunk = self.content.get(&c.hash).await?;
            if buf.len().saturating_add(chunk.len()) > crate::vfs::MAX_FILE_SIZE as usize {
                return Err(AfsError::TooLarge(format!(
                    "file body exceeds {} bytes",
                    crate::vfs::MAX_FILE_SIZE
                )));
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(buf.freeze())
    }

    /// Read the byte range `[off, off + len)` of a file, fetching only the chunks
    /// that overlap the range.
    pub async fn read_range(&self, path: &str, off: u64, len: u64) -> Result<Bytes> {
        let ino = self.resolve(path).await?;
        let inode = self
            .meta
            .get_inode(ino)
            .await?
            .ok_or_else(|| AfsError::NotFound(path.to_string()))?;
        if inode.kind != FileKind::File {
            return Err(AfsError::InvalidArgument(format!(
                "{path} is not a regular file"
            )));
        }
        let Some(mhash) = inode.content else {
            return Ok(Bytes::new());
        };
        let manifest = self.load_manifest(&mhash).await?;
        let end = off.saturating_add(len).min(manifest.size);
        if off >= end {
            return Ok(Bytes::new());
        }
        let mut buf = BytesMut::with_capacity((end - off) as usize);
        let mut pos: u64 = 0;
        for c in &manifest.chunks {
            let cstart = pos;
            let cend = pos + c.len as u64;
            pos = cend;
            if cend <= off {
                continue;
            }
            if cstart >= end {
                break;
            }
            let from = off.max(cstart) - cstart;
            let to = end.min(cend) - cstart;
            let part = self.content.get_range(&c.hash, from, to - from).await?;
            buf.extend_from_slice(&part);
        }
        Ok(buf.freeze())
    }

    /// Fetch inode metadata for a path.
    pub async fn stat(&self, path: &str) -> Result<Inode> {
        let ino = self.resolve(path).await?;
        self.meta
            .get_inode(ino)
            .await?
            .ok_or_else(|| AfsError::NotFound(path.to_string()))
    }

    /// Remove a file (decrementing link count; the inode is freed at nlink 0).
    pub async fn unlink(&self, path: &str) -> Result<()> {
        let (parent, name) = self.resolve_parent(path).await?;
        let ino = self
            .meta
            .lookup(parent, name)
            .await?
            .ok_or_else(|| AfsError::NotFound(path.to_string()))?;
        let inode = self
            .meta
            .get_inode(ino)
            .await?
            .ok_or_else(|| AfsError::NotFound(path.to_string()))?;
        if inode.kind == FileKind::Dir {
            return Err(AfsError::IsADirectory(path.to_string()));
        }
        // Unlink and the inode's fate (free vs. decrement) commit together, so a
        // crash can't drop the name yet leave the inode dangling (C1/L3).
        let mut tx = self.meta.begin().await?;
        tx.remove_dentry(parent, name).await?;
        let nlink = inode.nlink - 1;
        if nlink <= 0 {
            tx.delete_inode(ino).await?;
        } else {
            tx.set_nlink(ino, nlink).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Remove a file or an empty directory.
    pub async fn remove(&self, path: &str) -> Result<()> {
        let inode = self.stat(path).await?;
        if inode.kind == FileKind::Dir {
            self.rmdir(path).await
        } else {
            self.unlink(path).await
        }
    }

    /// Rename/move `from` to `to`. Overwrites an existing regular file or an
    /// existing empty directory at `to`.
    pub async fn rename(&self, from: &str, to: &str) -> Result<()> {
        let (sp, sn) = self.resolve_parent(from).await?;
        let sino = self
            .meta
            .lookup(sp, sn)
            .await?
            .ok_or_else(|| AfsError::NotFound(from.to_string()))?;
        let (dp, dn) = self.resolve_parent(to).await?;
        self.ensure_dir(dp).await?;

        // Read the destination's state before the txn; the mutations below all
        // commit together so a crash mid-rename can't leave the source unlinked
        // with the destination half-replaced, or orphan the overwritten inode.
        let overwrite = match self.meta.lookup(dp, dn).await? {
            Some(dino) if dino == sino => return Ok(()),
            Some(dino) => {
                let dinode = self
                    .meta
                    .get_inode(dino)
                    .await?
                    .ok_or_else(|| AfsError::NotFound(to.to_string()))?;
                if dinode.kind == FileKind::Dir && self.meta.child_count(dino).await? > 0 {
                    return Err(AfsError::DirectoryNotEmpty(to.to_string()));
                }
                Some((dino, dinode))
            }
            None => None,
        };

        let mut tx = self.meta.begin().await?;
        if let Some((dino, dinode)) = overwrite {
            tx.remove_dentry(dp, dn).await?;
            match dinode.kind {
                FileKind::Dir => tx.delete_inode(dino).await?,
                _ => {
                    let nlink = dinode.nlink - 1;
                    if nlink <= 0 {
                        tx.delete_inode(dino).await?;
                    } else {
                        tx.set_nlink(dino, nlink).await?;
                    }
                }
            }
        }
        tx.remove_dentry(sp, sn).await?;
        tx.add_dentry(dp, dn, sino).await?;
        tx.commit().await?;
        Ok(())
    }

    // --- symlinks ---------------------------------------------------------

    /// Create a symbolic link at `linkpath` pointing at `target`.
    pub async fn symlink(&self, target: &str, linkpath: &str) -> Result<Ino> {
        let (parent, name) = self.resolve_parent(linkpath).await?;
        self.ensure_dir(parent).await?;
        if self.meta.lookup(parent, name).await?.is_some() {
            return Err(AfsError::AlreadyExists(linkpath.to_string()));
        }
        // Inode, its target, and its dentry commit together (C1/M6).
        let mut tx = self.meta.begin().await?;
        let ino = tx
            .create_inode(InodeInit {
                kind: FileKind::Symlink,
                mode: SYMLINK_MODE,
            })
            .await?;
        tx.set_symlink(ino, target).await?;
        tx.add_dentry(parent, name, ino).await?;
        tx.commit().await?;
        Ok(ino)
    }

    /// Read a symlink's target.
    pub async fn readlink(&self, path: &str) -> Result<String> {
        let ino = self.resolve(path).await?;
        self.meta
            .get_symlink(ino)
            .await?
            .ok_or_else(|| AfsError::InvalidArgument(format!("{path} is not a symlink")))
    }
}
