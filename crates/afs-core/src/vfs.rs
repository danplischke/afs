//! Inode-oriented operations for the FUSE/NFS access layer (`docs/DESIGN.md`
//! §4e). FUSE addresses everything by inode number, so these mirror the
//! path-based [`Fs`] methods but take `(parent_ino, name)` / `ino` directly.
//!
//! Reads assemble only the covering chunks; writes are read-modify-write of the
//! whole file for now (correct, but a production build would update chunks
//! incrementally).

use crate::content::ContentStore;
use crate::engine::Fs;
use crate::error::{AfsError, Result};
use crate::metadata::MetadataStore;
use crate::types::{DirEntry, FileKind, Ino, Inode, InodeInit};
use bytes::{Bytes, BytesMut};

const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const SYMLINK_MODE: u32 = 0o120777;

/// Ceiling on a single file's size. Whole-file operations (write/truncate)
/// materialize the body in memory, so an unbounded client-supplied size/offset
/// would otherwise abort the process on the allocation. Values above this are
/// rejected with [`AfsError::TooLarge`] (mapped to `EFBIG`/`NFS3ERR_FBIG`).
pub const MAX_FILE_SIZE: u64 = 8 * 1024 * 1024 * 1024; // 8 GiB

impl<M: MetadataStore, C: ContentStore> Fs<M, C> {
    /// Look up `name` in directory `parent`, returning its inode.
    pub async fn vfs_lookup(&self, parent: Ino, name: &str) -> Result<Option<Inode>> {
        match self.meta.lookup(parent, name).await? {
            Some(ino) => self.meta.get_inode(ino).await,
            None => Ok(None),
        }
    }

    /// Inode attributes.
    pub async fn vfs_getattr(&self, ino: Ino) -> Result<Inode> {
        self.meta
            .get_inode(ino)
            .await?
            .ok_or_else(|| AfsError::NotFound(format!("ino {ino}")))
    }

    /// Directory entries.
    pub async fn vfs_readdir(&self, ino: Ino) -> Result<Vec<DirEntry>> {
        self.meta.list_dir(ino).await
    }

    /// Read up to `size` bytes at `offset`, fetching only the covering chunks.
    pub async fn vfs_read(&self, ino: Ino, offset: u64, size: u32) -> Result<Bytes> {
        let inode = self.vfs_getattr(ino).await?;
        let Some(mhash) = inode.content else {
            return Ok(Bytes::new());
        };
        let manifest = self.load_manifest(&mhash).await?;
        let end = offset.saturating_add(size as u64).min(manifest.size);
        if offset >= end {
            return Ok(Bytes::new());
        }
        let mut buf = BytesMut::with_capacity((end - offset) as usize);
        let mut pos = 0u64;
        for c in &manifest.chunks {
            let cstart = pos;
            let cend = pos + c.len as u64;
            pos = cend;
            if cend <= offset {
                continue;
            }
            if cstart >= end {
                break;
            }
            let from = offset.max(cstart) - cstart;
            let to = end.min(cend) - cstart;
            buf.extend_from_slice(&self.content.get_range(&c.hash, from, to - from).await?);
        }
        Ok(buf.freeze())
    }

    /// Write `data` at `offset` (extending the file as needed). Returns bytes written.
    pub async fn vfs_write(&self, ino: Ino, offset: u64, data: &[u8]) -> Result<u32> {
        let inode = self.vfs_getattr(ino).await?;
        let mut bytes = match inode.content {
            Some(h) => self.read_body(&h).await?,
            None => Vec::new(),
        };
        // Checked: a hostile offset near u64::MAX would otherwise overflow and
        // trigger a giant resize / slice-index panic.
        let end = offset
            .checked_add(data.len() as u64)
            .filter(|&e| e <= MAX_FILE_SIZE)
            .ok_or_else(|| {
                AfsError::TooLarge(format!("write past {MAX_FILE_SIZE} bytes (ino {ino})"))
            })?;
        let end = end as usize;
        if bytes.len() < end {
            bytes.resize(end, 0);
        }
        bytes[offset as usize..end].copy_from_slice(data);
        let (mhash, size) = self.store_body(&bytes).await?;
        self.meta.set_content(ino, mhash, size).await?;
        Ok(data.len() as u32)
    }

    /// Truncate/extend a file to `size` bytes.
    pub async fn vfs_truncate(&self, ino: Ino, size: u64) -> Result<()> {
        let inode = self.vfs_getattr(ino).await?;
        if size > MAX_FILE_SIZE {
            return Err(AfsError::TooLarge(format!(
                "truncate to {size} bytes exceeds {MAX_FILE_SIZE} (ino {ino})"
            )));
        }
        let mut bytes = match inode.content {
            Some(h) => self.read_body(&h).await?,
            None => Vec::new(),
        };
        bytes.resize(size as usize, 0);
        let (mhash, sz) = self.store_body(&bytes).await?;
        self.meta.set_content(ino, mhash, sz).await?;
        Ok(())
    }

    /// Create a regular file under `parent`.
    pub async fn vfs_create(&self, parent: Ino, name: &str, mode: u32) -> Result<Inode> {
        if self.meta.lookup(parent, name).await?.is_some() {
            return Err(AfsError::AlreadyExists(name.to_string()));
        }
        // Inode + dentry commit together, so a failed link can't orphan the
        // inode (C1/M6).
        let mut tx = self.meta.begin().await?;
        let ino = tx
            .create_inode(InodeInit {
                kind: FileKind::File,
                mode: S_IFREG | (mode & 0o7777),
            })
            .await?;
        tx.add_dentry(parent, name, ino).await?;
        tx.commit().await?;
        self.vfs_getattr(ino).await
    }

    /// Create a directory under `parent`.
    pub async fn vfs_mkdir(&self, parent: Ino, name: &str, mode: u32) -> Result<Inode> {
        if self.meta.lookup(parent, name).await?.is_some() {
            return Err(AfsError::AlreadyExists(name.to_string()));
        }
        let mut tx = self.meta.begin().await?;
        let ino = tx
            .create_inode(InodeInit {
                kind: FileKind::Dir,
                mode: S_IFDIR | (mode & 0o7777),
            })
            .await?;
        tx.add_dentry(parent, name, ino).await?;
        tx.commit().await?;
        self.vfs_getattr(ino).await
    }

    /// Remove a file under `parent`.
    pub async fn vfs_unlink(&self, parent: Ino, name: &str) -> Result<()> {
        let ino = self
            .meta
            .lookup(parent, name)
            .await?
            .ok_or_else(|| AfsError::NotFound(name.to_string()))?;
        let inode = self.vfs_getattr(ino).await?;
        if inode.kind == FileKind::Dir {
            return Err(AfsError::IsADirectory(name.to_string()));
        }
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

    /// Remove an empty directory under `parent`.
    pub async fn vfs_rmdir(&self, parent: Ino, name: &str) -> Result<()> {
        let ino = self
            .meta
            .lookup(parent, name)
            .await?
            .ok_or_else(|| AfsError::NotFound(name.to_string()))?;
        let inode = self.vfs_getattr(ino).await?;
        if inode.kind != FileKind::Dir {
            return Err(AfsError::NotADirectory(name.to_string()));
        }
        if self.meta.child_count(ino).await? > 0 {
            return Err(AfsError::DirectoryNotEmpty(name.to_string()));
        }
        let mut tx = self.meta.begin().await?;
        tx.remove_dentry(parent, name).await?;
        tx.delete_inode(ino).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Rename/move `(parent, name)` to `(newparent, newname)`.
    pub async fn vfs_rename(
        &self,
        parent: Ino,
        name: &str,
        newparent: Ino,
        newname: &str,
    ) -> Result<()> {
        let sino = self
            .meta
            .lookup(parent, name)
            .await?
            .ok_or_else(|| AfsError::NotFound(name.to_string()))?;
        // Resolve the destination's state before the txn; the mutations below
        // commit together so a crash can't leave the source unlinked with the
        // destination half-replaced (C1).
        let overwrite = match self.meta.lookup(newparent, newname).await? {
            Some(dino) if dino == sino => return Ok(()),
            Some(dino) => {
                let dinode = self.vfs_getattr(dino).await?;
                if dinode.kind == FileKind::Dir && self.meta.child_count(dino).await? > 0 {
                    return Err(AfsError::DirectoryNotEmpty(newname.to_string()));
                }
                Some((dino, dinode))
            }
            None => None,
        };

        let mut tx = self.meta.begin().await?;
        if let Some((dino, dinode)) = overwrite {
            tx.remove_dentry(newparent, newname).await?;
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
        tx.remove_dentry(parent, name).await?;
        tx.add_dentry(newparent, newname, sino).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Create a symlink under `parent`.
    pub async fn vfs_symlink(&self, parent: Ino, name: &str, target: &str) -> Result<Inode> {
        if self.meta.lookup(parent, name).await?.is_some() {
            return Err(AfsError::AlreadyExists(name.to_string()));
        }
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
        self.vfs_getattr(ino).await
    }

    /// Read a symlink target by inode.
    pub async fn vfs_readlink(&self, ino: Ino) -> Result<String> {
        self.meta
            .get_symlink(ino)
            .await?
            .ok_or_else(|| AfsError::InvalidArgument(format!("ino {ino} is not a symlink")))
    }
}
