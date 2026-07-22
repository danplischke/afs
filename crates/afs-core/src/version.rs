//! The versioning engine (`docs/DESIGN.md` §4c): commits, branches, checkout,
//! log, and status/diff, layered on the working-tree engine.
//!
//! The git-style object graph ([`crate::objectgraph`]) is the source of truth for
//! committed state; the inode/dentry working tree is a mutable view. `commit`
//! snapshots the working tree into trees + a commit; `checkout` materializes a
//! commit back into the working tree. Versioning is opt-in via the workspace's
//! `versioning` config (`off` disables commits entirely).

use crate::chunk::Manifest;
use crate::content::ContentStore;
use crate::engine::Fs;
use crate::error::{AfsError, Result};
use crate::metadata::MetadataStore;
use crate::objectgraph::{
    Commit, CommitInfo, DiffEntry, DiffStatus, Tree, TreeEntry, TreeKind, VersioningMode,
};
use crate::types::{FileKind, Hash, INO_ROOT, Ino, InodeInit};
use crate::util::now_secs;
use async_recursion::async_recursion;
use std::collections::BTreeMap;

const HEAD: &str = "HEAD";
const DEFAULT_BRANCH: &str = "main";

impl<M: MetadataStore, C: ContentStore> Fs<M, C> {
    /// Ensure HEAD and the default versioning mode exist (called by `init`).
    pub async fn init_versioning(&self) -> Result<()> {
        if self.meta.get_ref(HEAD).await?.is_none() {
            self.meta
                .set_ref(HEAD, &format!("ref:{DEFAULT_BRANCH}"))
                .await?;
        }
        if self.meta.get_config("versioning").await?.is_none() {
            self.meta
                .set_config("versioning", VersioningMode::Native.as_str())
                .await?;
        }
        Ok(())
    }

    /// The workspace's versioning mode (defaults to `native`).
    pub async fn versioning_mode(&self) -> Result<VersioningMode> {
        Ok(self
            .meta
            .get_config("versioning")
            .await?
            .and_then(|s| VersioningMode::parse(&s))
            .unwrap_or(VersioningMode::Native))
    }

    pub async fn set_versioning_mode(&self, mode: VersioningMode) -> Result<()> {
        self.meta.set_config("versioning", mode.as_str()).await
    }

    pub(crate) async fn ensure_commits_enabled(&self) -> Result<()> {
        if !self.versioning_mode().await?.commits_enabled() {
            return Err(AfsError::InvalidArgument(
                "versioning is disabled (off mode)".to_string(),
            ));
        }
        Ok(())
    }

    /// The current branch name (from HEAD), or `None` if HEAD is detached.
    pub async fn current_branch(&self) -> Result<Option<String>> {
        match self.meta.get_ref(HEAD).await? {
            Some(v) => Ok(v.strip_prefix("ref:").map(|s| s.to_string())),
            None => Ok(None),
        }
    }

    /// The commit HEAD points at, or `None` on an unborn branch.
    pub async fn head_commit(&self) -> Result<Option<Hash>> {
        let head = match self.meta.get_ref(HEAD).await? {
            Some(v) => v,
            None => return Ok(None),
        };
        let value = match head.strip_prefix("ref:") {
            Some(branch) => match self.meta.get_ref(branch).await? {
                Some(v) => v,
                None => return Ok(None), // unborn branch
            },
            None => head, // detached HEAD holds a commit hex directly
        };
        Ok(Hash::from_hex(&value))
    }

    /// Snapshot the working tree into a new commit, advancing the current branch.
    pub async fn commit(&self, author: &str, message: &str) -> Result<Hash> {
        self.ensure_commits_enabled().await?;
        let branch = self.current_branch().await?.ok_or_else(|| {
            AfsError::InvalidArgument("cannot commit with a detached HEAD".into())
        })?;
        let parent = self.head_commit().await?;

        // A merge in progress contributes the incoming commit as a second parent.
        let merge_head = self
            .meta
            .get_ref("MERGE_HEAD")
            .await?
            .and_then(|s| Hash::from_hex(&s));
        let mut parents: Vec<Hash> = parent.iter().copied().collect();
        if let Some(mh) = merge_head
            && !parents.contains(&mh)
        {
            parents.push(mh);
        }

        let tree = self.build_tree(INO_ROOT).await?;
        let commit = Commit {
            tree,
            parents,
            author: author.to_string(),
            message: message.to_string(),
            timestamp: now_secs(),
        };
        let commit_hash = self.content.put(&commit.encode()).await?;

        let expect = parent.map(|h| h.to_hex());
        let swapped = self
            .meta
            .cas_ref(&branch, expect.as_deref(), &commit_hash.to_hex())
            .await?;
        if !swapped {
            return Err(AfsError::Metadata(
                "branch moved concurrently; retry the commit".to_string(),
            ));
        }
        // Merge resolved: clear the in-progress state.
        if merge_head.is_some() {
            self.meta.delete_ref("MERGE_HEAD").await?;
            self.meta.clear_conflicts().await?;
        }
        Ok(commit_hash)
    }

    /// Recursively snapshot directory `dir_ino` into a tree object; returns its hash.
    #[async_recursion]
    async fn build_tree(&self, dir_ino: Ino) -> Result<Hash> {
        let mut entries = Vec::new();
        for de in self.meta.list_dir(dir_ino).await? {
            let inode = self
                .meta
                .get_inode(de.ino)
                .await?
                .ok_or_else(|| AfsError::NotFound(format!("ino {}", de.ino)))?;
            let (kind, hash) = match de.kind {
                FileKind::Dir => (TreeKind::Dir, self.build_tree(de.ino).await?),
                FileKind::File => {
                    let h = match inode.content {
                        Some(h) => h,
                        None => self.content.put(&Manifest::default().encode()).await?,
                    };
                    (TreeKind::File, h)
                }
                FileKind::Symlink => {
                    let target = self.meta.get_symlink(de.ino).await?.unwrap_or_default();
                    (
                        TreeKind::Symlink,
                        self.content.put(target.as_bytes()).await?,
                    )
                }
            };
            entries.push(TreeEntry {
                name: de.name,
                mode: inode.mode,
                kind,
                hash,
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        self.content.put(&Tree { entries }.encode()).await
    }

    /// Create a branch at the current HEAD commit.
    pub async fn create_branch(&self, name: &str) -> Result<()> {
        self.ensure_commits_enabled().await?;
        let head = self.head_commit().await?.ok_or_else(|| {
            AfsError::InvalidArgument("cannot branch before the first commit".into())
        })?;
        if !self.meta.cas_ref(name, None, &head.to_hex()).await? {
            return Err(AfsError::AlreadyExists(format!("branch {name}")));
        }
        Ok(())
    }

    /// Branch names (all refs except HEAD) with their commit hashes.
    pub async fn list_branches(&self) -> Result<Vec<(String, Hash)>> {
        let mut out = Vec::new();
        for (name, value) in self.meta.list_refs().await? {
            if name == HEAD {
                continue;
            }
            if let Some(h) = Hash::from_hex(&value) {
                out.push((name, h));
            }
        }
        Ok(out)
    }

    /// Switch the working tree to `branch`, materializing its commit.
    pub async fn checkout(&self, branch: &str) -> Result<()> {
        self.ensure_commits_enabled().await?;
        let value = self
            .meta
            .get_ref(branch)
            .await?
            .ok_or_else(|| AfsError::NotFound(format!("branch {branch}")))?;
        let commit_hash =
            Hash::from_hex(&value).ok_or_else(|| AfsError::Metadata("bad ref value".into()))?;
        let commit = Commit::decode(&self.content.get(&commit_hash).await?)?;

        self.meta.truncate_tree().await?;
        self.materialize_into(commit.tree, INO_ROOT).await?;
        self.meta.set_ref(HEAD, &format!("ref:{branch}")).await?;
        Ok(())
    }

    /// Materialize a tree's entries as children of `parent_ino`.
    #[async_recursion]
    pub(crate) async fn materialize_into(&self, tree_hash: Hash, parent_ino: Ino) -> Result<()> {
        let tree = Tree::decode(&self.content.get(&tree_hash).await?)?;
        for e in &tree.entries {
            match e.kind {
                TreeKind::Dir => {
                    let ino = self
                        .meta
                        .create_inode(InodeInit {
                            kind: FileKind::Dir,
                            mode: e.mode,
                        })
                        .await?;
                    self.meta.add_dentry(parent_ino, &e.name, ino).await?;
                    self.materialize_into(e.hash, ino).await?;
                }
                TreeKind::File => {
                    let ino = self
                        .meta
                        .create_inode(InodeInit {
                            kind: FileKind::File,
                            mode: e.mode,
                        })
                        .await?;
                    let size = Manifest::decode(&self.content.get(&e.hash).await?)?.size;
                    self.meta.set_content(ino, Some(e.hash), size).await?;
                    self.meta.add_dentry(parent_ino, &e.name, ino).await?;
                }
                TreeKind::Symlink => {
                    let ino = self
                        .meta
                        .create_inode(InodeInit {
                            kind: FileKind::Symlink,
                            mode: e.mode,
                        })
                        .await?;
                    let target =
                        String::from_utf8_lossy(&self.content.get(&e.hash).await?).into_owned();
                    self.meta.set_symlink(ino, &target).await?;
                    self.meta.add_dentry(parent_ino, &e.name, ino).await?;
                }
            }
        }
        Ok(())
    }

    /// Commit history from HEAD, following first parents.
    pub async fn log(&self) -> Result<Vec<CommitInfo>> {
        let mut out = Vec::new();
        let mut cursor = self.head_commit().await?;
        while let Some(hash) = cursor {
            let commit = Commit::decode(&self.content.get(&hash).await?)?;
            cursor = commit.parents.first().copied();
            out.push(CommitInfo { hash, commit });
        }
        Ok(out)
    }

    /// Changes between the working tree and HEAD (like `git status`).
    pub async fn status(&self) -> Result<Vec<DiffEntry>> {
        let base = match self.head_commit().await? {
            Some(h) => {
                let commit = Commit::decode(&self.content.get(&h).await?)?;
                let mut map = BTreeMap::new();
                self.flatten_tree(commit.tree, String::new(), &mut map)
                    .await?;
                map
            }
            None => BTreeMap::new(),
        };
        let mut work = BTreeMap::new();
        self.flatten_working(INO_ROOT, String::new(), &mut work)
            .await?;
        Ok(diff_maps(&base, &work))
    }

    #[async_recursion]
    async fn flatten_working(
        &self,
        dir_ino: Ino,
        prefix: String,
        map: &mut BTreeMap<String, Hash>,
    ) -> Result<()> {
        for de in self.meta.list_dir(dir_ino).await? {
            let path = format!("{prefix}/{}", de.name);
            match de.kind {
                FileKind::Dir => self.flatten_working(de.ino, path, map).await?,
                FileKind::File => {
                    let inode = self
                        .meta
                        .get_inode(de.ino)
                        .await?
                        .ok_or_else(|| AfsError::NotFound(path.clone()))?;
                    let h = match inode.content {
                        Some(h) => h,
                        None => Hash::of(&Manifest::default().encode()),
                    };
                    map.insert(path, h);
                }
                FileKind::Symlink => {
                    let target = self.meta.get_symlink(de.ino).await?.unwrap_or_default();
                    map.insert(path, Hash::of(target.as_bytes()));
                }
            }
        }
        Ok(())
    }

    #[async_recursion]
    async fn flatten_tree(
        &self,
        tree_hash: Hash,
        prefix: String,
        map: &mut BTreeMap<String, Hash>,
    ) -> Result<()> {
        let tree = Tree::decode(&self.content.get(&tree_hash).await?)?;
        for e in &tree.entries {
            let path = format!("{prefix}/{}", e.name);
            match e.kind {
                TreeKind::Dir => self.flatten_tree(e.hash, path, map).await?,
                TreeKind::File | TreeKind::Symlink => {
                    map.insert(path, e.hash);
                }
            }
        }
        Ok(())
    }
}

fn diff_maps(base: &BTreeMap<String, Hash>, work: &BTreeMap<String, Hash>) -> Vec<DiffEntry> {
    let mut out = Vec::new();
    for (path, wh) in work {
        match base.get(path) {
            None => out.push(DiffEntry {
                path: path.clone(),
                status: DiffStatus::Added,
            }),
            Some(bh) if bh != wh => out.push(DiffEntry {
                path: path.clone(),
                status: DiffStatus::Modified,
            }),
            _ => {}
        }
    }
    for path in base.keys() {
        if !work.contains_key(path) {
            out.push(DiffEntry {
                path: path.clone(),
                status: DiffStatus::Deleted,
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}
