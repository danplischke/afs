//! The working-tree engine: POSIX-flavored operations over a [`MetadataStore`]
//! plus a [`ContentStore`].
//!
//! This is the mutable working tree of `docs/DESIGN.md` §3. In M0 it is the whole
//! story (no commits yet); later milestones layer commits/branches (M3), merge
//! (M4), and attribution (M6) on top without changing this surface.

use crate::content::ContentStore;
use crate::error::{AfsError, Result};
use crate::metadata::MetadataStore;
use crate::types::{DirEntry, FileKind, INO_ROOT, Ino, Inode, InodeInit};
use bytes::Bytes;

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

    /// Initialize the metadata schema and the root directory.
    pub async fn init(&self) -> Result<()> {
        self.meta.init().await
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

    /// Write `data` as the entire contents of `path`, creating the file if
    /// needed. The body is stored as a single content-addressed blob.
    pub async fn write(&self, path: &str, data: &[u8]) -> Result<()> {
        let (parent, name) = self.resolve_parent(path).await?;
        self.ensure_dir(parent).await?;
        let ino = match self.meta.lookup(parent, name).await? {
            Some(existing) => {
                let inode = self
                    .meta
                    .get_inode(existing)
                    .await?
                    .ok_or_else(|| AfsError::NotFound(path.to_string()))?;
                if inode.kind == FileKind::Dir {
                    return Err(AfsError::IsADirectory(path.to_string()));
                }
                existing
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
                ino
            }
        };
        let hash = if data.is_empty() {
            None
        } else {
            Some(self.content.put(data).await?)
        };
        self.meta.set_content(ino, hash, data.len() as u64).await?;
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
                Some(h) => self.content.get(&h).await,
                None => Ok(Bytes::new()),
            },
        }
    }

    /// Read the byte range `[off, off + len)` of a file.
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
        match inode.content {
            Some(h) => self.content.get_range(&h, off, len).await,
            None => Ok(Bytes::new()),
        }
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
