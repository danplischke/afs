//! The working-tree engine: POSIX-flavored operations over a [`MetadataStore`]
//! plus a [`ContentStore`].
//!
//! This is the mutable working tree of `docs/DESIGN.md` §3. In M0 it is the whole
//! story (no commits yet); later milestones layer commits/branches (M3), merge
//! (M4), and attribution (M6) on top without changing this surface.

use crate::chunk::{AVG_CHUNK, ChunkRef, MAX_CHUNK, MIN_CHUNK, Manifest, chunk_bounds};
use crate::content::ContentStore;
use crate::error::{AfsError, Result};
use crate::metadata::MetadataStore;
use crate::types::{DirEntry, FileKind, Hash, INO_ROOT, Ino, Inode, InodeInit};
use bytes::{Bytes, BytesMut};

const DIR_MODE: u32 = 0o040755;
const FILE_MODE: u32 = 0o100644;
const SYMLINK_MODE: u32 = 0o120777;

/// A filesystem over a metadata store and a content store.
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

    /// Split an absolute path into its non-empty segments.
    fn split(path: &str) -> Result<Vec<&str>> {
        if !path.starts_with('/') {
            return Err(AfsError::InvalidPath(format!(
                "path must be absolute: {path}"
            )));
        }
        Ok(path.split('/').filter(|s| !s.is_empty()).collect())
    }

    /// Resolve an absolute path to its inode.
    async fn resolve(&self, path: &str) -> Result<Ino> {
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
    async fn resolve_parent<'a>(&self, path: &'a str) -> Result<(Ino, &'a str)> {
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

    async fn ensure_dir(&self, ino: Ino) -> Result<()> {
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
        let ino = self
            .meta
            .create_inode(InodeInit {
                kind: FileKind::Dir,
                mode: DIR_MODE,
            })
            .await?;
        self.meta.add_dentry(parent, name, ino).await?;
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
                    let child = self
                        .meta
                        .create_inode(InodeInit {
                            kind: FileKind::Dir,
                            mode: DIR_MODE,
                        })
                        .await?;
                    self.meta.add_dentry(ino, seg, child).await?;
                    ino = child;
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
        self.meta.remove_dentry(parent, name).await?;
        self.meta.delete_inode(ino).await?;
        Ok(())
    }

    /// List a directory's entries, ordered by name.
    pub async fn ls(&self, path: &str) -> Result<Vec<DirEntry>> {
        let ino = self.resolve(path).await?;
        self.ensure_dir(ino).await?;
        self.meta.list_dir(ino).await
    }

    // --- file operations --------------------------------------------------

    /// Resolve or create the file inode for `(parent, name)`.
    async fn ensure_file(&self, parent: Ino, name: &str, path: &str) -> Result<Ino> {
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
                Ok(existing)
            }
            None => {
                let ino = self
                    .meta
                    .create_inode(InodeInit {
                        kind: FileKind::File,
                        mode: FILE_MODE,
                    })
                    .await?;
                self.meta.add_dentry(parent, name, ino).await?;
                Ok(ino)
            }
        }
    }

    /// Chunk `data` (content-defined), store each chunk, and write a manifest.
    /// Returns `(manifest_hash, size)`; an empty body yields `(None, 0)`.
    async fn store_body(&self, data: &[u8]) -> Result<(Option<Hash>, u64)> {
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
        Ok((Some(mhash), manifest.size))
    }

    async fn load_manifest(&self, mhash: &Hash) -> Result<Manifest> {
        let bytes = self.content.get(mhash).await?;
        Manifest::decode(&bytes)
    }

    /// Write `data` as the entire contents of `path`, creating the file if needed.
    /// The body is content-defined-chunked; unchanged chunks are deduplicated.
    pub async fn write(&self, path: &str, data: &[u8]) -> Result<()> {
        let (parent, name) = self.resolve_parent(path).await?;
        self.ensure_dir(parent).await?;
        let ino = self.ensure_file(parent, name, path).await?;
        let (mhash, size) = self.store_body(data).await?;
        self.meta.set_content(ino, mhash, size).await?;
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
        let ino = self.ensure_file(parent, name, path).await?;

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
        self.meta.set_content(ino, mhash, size).await?;
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
                Some(mhash) => {
                    let manifest = self.load_manifest(&mhash).await?;
                    let mut buf = BytesMut::with_capacity(manifest.size as usize);
                    for c in &manifest.chunks {
                        buf.extend_from_slice(&self.content.get(&c.hash).await?);
                    }
                    Ok(buf.freeze())
                }
            },
        }
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
        self.meta.remove_dentry(parent, name).await?;
        let nlink = inode.nlink - 1;
        if nlink <= 0 {
            self.meta.delete_inode(ino).await?;
        } else {
            self.meta.set_nlink(ino, nlink).await?;
        }
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

        if let Some(dino) = self.meta.lookup(dp, dn).await? {
            if dino == sino {
                return Ok(());
            }
            let dinode = self
                .meta
                .get_inode(dino)
                .await?
                .ok_or_else(|| AfsError::NotFound(to.to_string()))?;
            match dinode.kind {
                FileKind::Dir => {
                    if self.meta.child_count(dino).await? > 0 {
                        return Err(AfsError::DirectoryNotEmpty(to.to_string()));
                    }
                    self.meta.remove_dentry(dp, dn).await?;
                    self.meta.delete_inode(dino).await?;
                }
                _ => {
                    self.meta.remove_dentry(dp, dn).await?;
                    let nlink = dinode.nlink - 1;
                    if nlink <= 0 {
                        self.meta.delete_inode(dino).await?;
                    } else {
                        self.meta.set_nlink(dino, nlink).await?;
                    }
                }
            }
        }

        self.meta.remove_dentry(sp, sn).await?;
        self.meta.add_dentry(dp, dn, sino).await?;
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
        let ino = self
            .meta
            .create_inode(InodeInit {
                kind: FileKind::Symlink,
                mode: SYMLINK_MODE,
            })
            .await?;
        self.meta.set_symlink(ino, target).await?;
        self.meta.add_dentry(parent, name, ino).await?;
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
