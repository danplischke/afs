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

pub use afs_core::{
    Actor, ActorInit, ActorKind, AfsError, BlameRange, CommitInfo, Conflict, DiffEntry, DiffStatus,
    DirEntry, EditOp, EncryptedStore, Event, EventInit, FileKind, GcStats, Hash, Inode, MemStore,
    MergeOutcome, PackStore, Presence, TieredStore, ToolCallInit, VersioningMode, WriteCtx,
};
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

    /// SQLite metadata + a local content store **encrypted at rest** with a key
    /// derived from `passphrase`. The same passphrase must be used on reopen;
    /// the wrong one fails loudly rather than returning garbage.
    pub async fn open_local_encrypted(
        db_path: impl AsRef<Path>,
        cas_dir: impl AsRef<Path>,
        passphrase: &str,
    ) -> Result<Self> {
        let meta: Meta = Arc::new(SqliteMetadataStore::open(db_path)?);
        let backend: Content = Arc::new(LocalCasStore::open(cas_dir).await?);
        let content: Content = Arc::new(EncryptedStore::from_passphrase(backend, passphrase));
        Self::open(meta, content).await
    }

    /// SQLite metadata + an S3-compatible object store for content.
    pub async fn open_s3(db_path: impl AsRef<Path>, cfg: S3Config) -> Result<Self> {
        let meta: Meta = Arc::new(SqliteMetadataStore::open(db_path)?);
        let content: Content = Arc::new(ObjectContentStore::s3(cfg)?);
        Self::open(meta, content).await
    }

    /// SQLite metadata + a **packed** local content store: chunks batched into
    /// pack objects under `data_dir`, with the per-chunk index under `index_dir`.
    /// The local mirror of [`Workspace::open_s3_packed`].
    pub async fn open_local_packed(
        db_path: impl AsRef<Path>,
        data_dir: impl AsRef<Path>,
        index_dir: impl AsRef<Path>,
    ) -> Result<Self> {
        let meta: Meta = Arc::new(SqliteMetadataStore::open(db_path)?);
        let data: Content = Arc::new(LocalCasStore::open(data_dir).await?);
        let index: Content = Arc::new(LocalCasStore::open(index_dir).await?);
        let content: Content = Arc::new(PackStore::new(data, index));
        Self::open(meta, content).await
    }

    /// SQLite metadata + an S3-compatible object store whose chunks are batched
    /// into **pack objects** (few large PUTs instead of many tiny ones), with the
    /// per-chunk index kept in a local directory. This is the recommended layout
    /// for object storage; call [`Workspace::flush`] (or `commit`) to seal the
    /// open pack and [`Workspace::repack`] to reclaim deleted space.
    pub async fn open_s3_packed(
        db_path: impl AsRef<Path>,
        cfg: S3Config,
        index_dir: impl AsRef<Path>,
    ) -> Result<Self> {
        let meta: Meta = Arc::new(SqliteMetadataStore::open(db_path)?);
        let data: Content = Arc::new(ObjectContentStore::s3(cfg)?);
        let index: Content = Arc::new(LocalCasStore::open(index_dir).await?);
        let content: Content = Arc::new(PackStore::new(data, index));
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

    /// Record a collaboration event (best-effort: a feed hiccup never fails the
    /// underlying operation, which has already succeeded).
    async fn emit(
        &self,
        kind: &str,
        path: &str,
        detail: Option<String>,
        actor: Option<i64>,
        session: Option<i64>,
    ) {
        // Tag the event with the branch it happened on so a per-branch UI can
        // filter the feed. Best-effort, like the emit itself.
        let branch = self.fs.current_branch().await.ok().flatten();
        let _ = self
            .fs
            .record_event(EventInit {
                actor_id: actor,
                session_id: session,
                kind: kind.to_string(),
                path: path.to_string(),
                detail,
                branch,
            })
            .await;
    }

    pub async fn write(&self, path: &str, data: &[u8]) -> Result<()> {
        self.fs.write(path, data).await?;
        self.emit("write", path, None, None, None).await;
        Ok(())
    }

    /// Write a file by streaming from a blocking reader (for large files).
    pub async fn write_reader<R: std::io::Read + Send + 'static>(
        &self,
        path: &str,
        reader: R,
    ) -> Result<()> {
        self.fs.write_reader(path, reader).await?;
        self.emit("write", path, None, None, None).await;
        Ok(())
    }

    pub async fn read(&self, path: &str) -> Result<Bytes> {
        self.fs.read(path).await
    }

    pub async fn read_range(&self, path: &str, off: u64, len: u64) -> Result<Bytes> {
        self.fs.read_range(path, off, len).await
    }

    pub async fn mkdir_p(&self, path: &str) -> Result<()> {
        self.fs.mkdir_p(path).await?;
        self.emit("mkdir", path, None, None, None).await;
        Ok(())
    }

    pub async fn ls(&self, path: &str) -> Result<Vec<DirEntry>> {
        self.fs.ls(path).await
    }

    pub async fn stat(&self, path: &str) -> Result<Inode> {
        self.fs.stat(path).await
    }

    pub async fn remove(&self, path: &str) -> Result<()> {
        self.fs.remove(path).await?;
        self.emit("remove", path, None, None, None).await;
        Ok(())
    }

    pub async fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.fs.rename(from, to).await?;
        self.emit("rename", from, Some(to.to_string()), None, None).await;
        Ok(())
    }

    pub async fn readlink(&self, path: &str) -> Result<String> {
        self.fs.readlink(path).await
    }

    pub async fn symlink(&self, target: &str, linkpath: &str) -> Result<()> {
        self.fs.symlink(target, linkpath).await?;
        self.emit("symlink", linkpath, Some(target.to_string()), None, None)
            .await;
        Ok(())
    }

    // --- versioning ------------------------------------------------------

    /// Snapshot the working tree into a commit on the current branch.
    pub async fn commit(&self, author: &str, message: &str) -> Result<Hash> {
        let hash = self.fs.commit(author, message).await?;
        self.emit("commit", "/", Some(message.to_string()), None, None)
            .await;
        Ok(hash)
    }

    /// Commit history from HEAD (first-parent).
    pub async fn log(&self) -> Result<Vec<CommitInfo>> {
        self.fs.log().await
    }

    /// Working-tree changes relative to HEAD.
    pub async fn status(&self) -> Result<Vec<DiffEntry>> {
        self.fs.status().await
    }

    /// Paths that differ between two refs/commits (`from` → `to`), compared by
    /// content address — the cheap file-list half of a branch comparison.
    pub async fn diff(&self, from: &str, to: &str) -> Result<Vec<DiffEntry>> {
        self.fs.diff(from, to).await
    }

    /// A unified line diff of one path between two refs/commits (empty when
    /// unchanged on both sides).
    pub async fn diff_file(&self, from: &str, to: &str, path: &str) -> Result<String> {
        self.fs.diff_file(from, to, path).await
    }

    /// The current branch name (or `None` if detached).
    pub async fn current_branch(&self) -> Result<Option<String>> {
        self.fs.current_branch().await
    }

    /// Create a branch at the current HEAD commit.
    pub async fn create_branch(&self, name: &str) -> Result<()> {
        self.fs.create_branch(name).await
    }

    /// Switch the working tree to `branch`.
    pub async fn checkout(&self, branch: &str) -> Result<()> {
        self.fs.checkout(branch).await
    }

    /// All branches with their commit hashes.
    pub async fn list_branches(&self) -> Result<Vec<(String, Hash)>> {
        self.fs.list_branches().await
    }

    pub async fn versioning_mode(&self) -> Result<VersioningMode> {
        self.fs.versioning_mode().await
    }

    pub async fn set_versioning_mode(&self, mode: VersioningMode) -> Result<()> {
        self.fs.set_versioning_mode(mode).await
    }

    // --- maintenance -----------------------------------------------------

    /// Reclaim content-store objects unreachable from any ref or the live
    /// working tree. Run when the workspace is idle.
    pub async fn gc(&self) -> Result<GcStats> {
        self.fs.gc().await
    }

    /// Seal any buffered writes to durable storage (a no-op unless the content
    /// backend batches, e.g. a packed store). `commit` flushes automatically.
    pub async fn flush(&self) -> Result<()> {
        self.fs.content.flush().await
    }

    /// Compact the content store, reclaiming space held by deleted objects;
    /// returns the bytes reclaimed. Meaningful for a packed store; run after
    /// `gc`. A no-op for in-place backends.
    pub async fn repack(&self) -> Result<u64> {
        self.fs.content.repack().await
    }

    // --- merge + locks ---------------------------------------------------

    /// Merge commit `theirs` into the current branch.
    pub async fn merge(&self, theirs: Hash, author: &str, message: &str) -> Result<MergeOutcome> {
        self.fs.merge(theirs, author, message).await
    }

    /// Merge branch `name` into the current branch.
    pub async fn merge_branch(
        &self,
        name: &str,
        author: &str,
        message: &str,
    ) -> Result<MergeOutcome> {
        let target = self
            .fs
            .list_branches()
            .await?
            .into_iter()
            .find(|(n, _)| n == name)
            .map(|(_, h)| h)
            .ok_or_else(|| AfsError::NotFound(format!("branch {name}")))?;
        self.fs.merge(target, author, message).await
    }

    /// Unresolved merge conflicts as `(path, kind)`.
    pub async fn conflicts(&self) -> Result<Vec<(String, String)>> {
        self.fs.conflicts().await
    }

    pub async fn lock(&self, path: &str, owner: &str) -> Result<bool> {
        let acquired = self.fs.lock(path, owner).await?;
        if acquired {
            self.emit("lock", path, Some(owner.to_string()), None, None)
                .await;
        }
        Ok(acquired)
    }

    pub async fn unlock(&self, path: &str, owner: &str) -> Result<bool> {
        let released = self.fs.unlock(path, owner).await?;
        if released {
            self.emit("unlock", path, Some(owner.to_string()), None, None)
                .await;
        }
        Ok(released)
    }

    pub async fn locks(&self) -> Result<Vec<(String, String, i64)>> {
        self.fs.locks().await
    }

    // --- attribution -----------------------------------------------------

    /// Register a human actor.
    pub async fn create_human(&self, name: &str, auth_subject: Option<&str>) -> Result<i64> {
        self.fs.create_human(name, auth_subject).await
    }

    /// Register an agent actor, optionally with the human that launched it.
    pub async fn create_agent(
        &self,
        name: &str,
        model: &str,
        controller: Option<i64>,
    ) -> Result<i64> {
        self.fs.create_agent(name, model, controller).await
    }

    pub async fn get_actor(&self, id: i64) -> Result<Option<Actor>> {
        self.fs.get_actor(id).await
    }

    pub async fn create_session(&self, actor_id: i64, client: Option<&str>) -> Result<i64> {
        self.fs.create_session(actor_id, client).await
    }

    /// Attributed write: records the actor and updates per-line authorship.
    pub async fn write_as(&self, ctx: WriteCtx, path: &str, data: &[u8]) -> Result<()> {
        self.fs.write_as(ctx, path, data).await?;
        self.emit("write", path, None, Some(ctx.actor), ctx.session)
            .await;
        Ok(())
    }

    /// Per-line-range authorship for a path (human vs agent).
    pub async fn blame(&self, path: &str) -> Result<Vec<BlameRange>> {
        self.fs.blame(path).await
    }

    /// The edit-op log for an actor (optionally one session).
    pub async fn edit_ops(&self, actor_id: i64, session_id: Option<i64>) -> Result<Vec<EditOp>> {
        self.fs.edit_ops(actor_id, session_id).await
    }

    /// Revert every line an actor wrote in a session. Returns files changed.
    pub async fn revert_session(&self, actor_id: i64, session_id: i64) -> Result<usize> {
        self.fs.revert_session(actor_id, session_id).await
    }

    // --- live collaboration ----------------------------------------------

    /// Tail the change feed: events strictly after `after_seq`, oldest first.
    /// Poll with the last seen `seq` as the cursor (Postgres also fires
    /// `NOTIFY afs_events` so consumers can be pushed instead of polling).
    pub async fn watch(&self, after_seq: i64) -> Result<Vec<Event>> {
        self.fs.events_since(after_seq, 1000).await
    }

    /// Record an arbitrary event on the change feed.
    pub async fn record_event(&self, ev: EventInit) -> Result<i64> {
        self.fs.record_event(ev).await
    }

    /// Heartbeat a session's presence (and the path it is working on).
    pub async fn touch(&self, actor_id: i64, session_id: i64, path: Option<&str>) -> Result<()> {
        self.fs.touch_presence(session_id, actor_id, path).await
    }

    /// Sessions active within the last `window_secs` seconds.
    pub async fn presence(&self, window_secs: i64) -> Result<Vec<Presence>> {
        self.fs.presence(window_secs).await
    }
}
