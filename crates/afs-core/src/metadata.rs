//! The metadata store: names, inodes, and (in later milestones) refs, commits,
//! and attribution. Content bytes never live here — only content addresses do
//! (`docs/DESIGN.md` §4b).

use crate::attribution::{Actor, ActorInit, EditOp, EditOpInit, ToolCallInit};
use crate::collab::{Event, EventInit, Presence};
use crate::error::Result;
use crate::suggest::{Suggestion, SuggestionInit, SuggestionStatus};
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

    /// Begin an atomic write transaction (`docs/DESIGN.md` §4b).
    ///
    /// A logical filesystem write is several statements — create an inode, link
    /// a dentry, set content, record blame, append an op-log entry — and if any
    /// step fails or the process crashes between them the store is left corrupt:
    /// a dangling dentry, an orphaned inode, or a content/blame mismatch. Route
    /// such writes through a transaction so they commit all-or-nothing. Dropping
    /// the returned [`MetaTxn`] without [`commit`](MetaTxn::commit) rolls back.
    ///
    /// SQLite uses `BEGIN IMMEDIATE` (one writer at a time); Postgres pins a
    /// pooled connection for the `BEGIN…COMMIT`.
    async fn begin(&self) -> Result<Box<dyn MetaTxn>>;

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

    // --- refs (branches / tags / HEAD) -----------------------------------

    /// Read a ref's value (a commit hex, or a symbolic `ref:<name>`).
    async fn get_ref(&self, name: &str) -> Result<Option<String>>;

    /// Set (upsert) a ref.
    async fn set_ref(&self, name: &str, value: &str) -> Result<()>;

    /// Compare-and-swap a ref: succeed only if its current value equals `expect`
    /// (`None` meaning "must not exist"). Returns whether the swap happened.
    async fn cas_ref(&self, name: &str, expect: Option<&str>, new: &str) -> Result<bool>;

    /// Delete a ref (no-op if absent).
    async fn delete_ref(&self, name: &str) -> Result<()>;

    /// List all refs as `(name, value)` pairs.
    async fn list_refs(&self) -> Result<Vec<(String, String)>>;

    // --- workspace config ------------------------------------------------

    async fn get_config(&self, key: &str) -> Result<Option<String>>;
    async fn set_config(&self, key: &str, value: &str) -> Result<()>;

    // --- working tree ----------------------------------------------------

    /// Clear the entire working tree (all dentries, symlinks, and inodes except
    /// the root) — used by `checkout` before materializing a commit.
    async fn truncate_tree(&self) -> Result<()>;

    // --- merge: conflicts + locks ----------------------------------------

    /// Record (upsert) an unresolved merge conflict at `path`.
    async fn set_conflict(&self, path: &str, kind: &str) -> Result<()>;

    /// List unresolved conflicts as `(path, kind)`.
    async fn list_conflicts(&self) -> Result<Vec<(String, String)>>;

    /// Clear all recorded conflicts (e.g. once a merge is committed).
    async fn clear_conflicts(&self) -> Result<()>;

    /// Acquire an exclusive lock on `path` for `owner`; `false` if already held.
    async fn acquire_lock(&self, path: &str, owner: &str, at: i64) -> Result<bool>;

    /// Release `owner`'s lock on `path`; `false` if not held by `owner`.
    async fn release_lock(&self, path: &str, owner: &str) -> Result<bool>;

    /// List held locks as `(path, owner, acquired_at)`.
    async fn list_locks(&self) -> Result<Vec<(String, String, i64)>>;

    // --- attribution -----------------------------------------------------

    async fn create_actor(&self, init: ActorInit) -> Result<i64>;
    async fn get_actor(&self, id: i64) -> Result<Option<Actor>>;
    async fn create_session(
        &self,
        actor_id: i64,
        client: Option<&str>,
        started_at: i64,
    ) -> Result<i64>;
    async fn record_tool_call(&self, tc: ToolCallInit) -> Result<i64>;
    async fn append_edit_op(&self, op: EditOpInit) -> Result<i64>;
    async fn list_edit_ops(&self, actor_id: i64, session_id: Option<i64>) -> Result<Vec<EditOp>>;
    async fn set_line_blame(&self, ino: Ino, runs: &str) -> Result<()>;
    async fn get_line_blame(&self, ino: Ino) -> Result<Option<String>>;

    // --- collaboration: change feed + presence ---------------------------

    /// Append an event to the change feed, returning its monotonic `seq`.
    async fn append_event(&self, ev: EventInit, ts: i64) -> Result<i64>;
    /// Events strictly after `after_seq`, oldest first, capped at `limit`.
    async fn events_since(&self, after_seq: i64, limit: i64) -> Result<Vec<Event>>;
    /// Upsert a session's presence heartbeat (and current path).
    async fn touch_presence(
        &self,
        session_id: i64,
        actor_id: i64,
        path: Option<&str>,
        at: i64,
    ) -> Result<()>;
    /// Sessions with `last_seen >= since_ts`, most recently seen first.
    async fn active_presence(&self, since_ts: i64) -> Result<Vec<Presence>>;
    /// Delete presence rows with `last_seen < older_than` (keeps the table from
    /// growing without bound — one row accretes per session otherwise). Returns
    /// the number reaped.
    async fn reap_presence(&self, older_than: i64) -> Result<u64>;

    // --- agent-suggestion review queue -----------------------------------

    /// Record a new (pending) suggestion, returning its id.
    async fn create_suggestion(&self, init: SuggestionInit, ts: i64) -> Result<i64>;
    /// Fetch a suggestion by id.
    async fn get_suggestion(&self, id: i64) -> Result<Option<Suggestion>>;
    /// Suggestions filtered by `status` and/or `path`, newest first.
    async fn list_suggestions(
        &self,
        status: Option<SuggestionStatus>,
        path: Option<&str>,
    ) -> Result<Vec<Suggestion>>;
    /// Transition a *pending* suggestion to `status`, stamping who/when.
    /// Returns `false` if it wasn't pending (already resolved / not found).
    async fn resolve_suggestion(
        &self,
        id: i64,
        status: SuggestionStatus,
        resolved_by: Option<i64>,
        ts: i64,
    ) -> Result<bool>;
}

/// An in-progress atomic write, returned by [`MetadataStore::begin`].
///
/// It exposes only the write subset a logical filesystem operation needs. Reads
/// (existence checks, `get_inode`) are done on the store *before* `begin`; the
/// store's own constraints — chiefly the unique `(parent, name)` dentry index —
/// together with all-or-nothing rollback ensure a losing race (two creators of
/// the same path) errors and unwinds cleanly instead of orphaning an inode.
///
/// Mutations are staged and become visible only on [`commit`](Self::commit).
/// Dropping without committing rolls the whole transaction back.
#[async_trait]
pub trait MetaTxn: Send {
    /// Allocate a new inode (`nlink` = 1, no content). Returns its number.
    async fn create_inode(&mut self, init: InodeInit) -> Result<Ino>;
    /// Set an inode's content address and size.
    async fn set_content(&mut self, ino: Ino, content: Option<Hash>, size: u64) -> Result<()>;
    /// Set an inode's link count.
    async fn set_nlink(&mut self, ino: Ino, nlink: i64) -> Result<()>;
    /// Delete an inode (and any symlink row).
    async fn delete_inode(&mut self, ino: Ino) -> Result<()>;
    /// Link `name` in `parent` to `ino`. Errors if the name already exists.
    async fn add_dentry(&mut self, parent: Ino, name: &str, ino: Ino) -> Result<()>;
    /// Unlink `name` from `parent` (no-op if absent).
    async fn remove_dentry(&mut self, parent: Ino, name: &str) -> Result<()>;
    /// Set (or replace) a symlink target.
    async fn set_symlink(&mut self, ino: Ino, target: &str) -> Result<()>;
    /// Set (or replace) an inode's line-authorship map.
    async fn set_line_blame(&mut self, ino: Ino, runs: &str) -> Result<()>;
    /// Append an op-log entry, returning its id.
    async fn append_edit_op(&mut self, op: EditOpInit) -> Result<i64>;
    /// Commit every staged mutation atomically. Consumes the transaction.
    async fn commit(self: Box<Self>) -> Result<()>;
}

/// Delegating impl so `Arc<dyn MetadataStore>` (and `Arc<ConcreteStore>`) is
/// itself a [`MetadataStore`]. This lets a workspace pick its backend at runtime.
#[async_trait]
impl<T: MetadataStore + ?Sized> MetadataStore for Arc<T> {
    async fn init(&self) -> Result<()> {
        (**self).init().await
    }
    async fn begin(&self) -> Result<Box<dyn MetaTxn>> {
        (**self).begin().await
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
    async fn get_ref(&self, name: &str) -> Result<Option<String>> {
        (**self).get_ref(name).await
    }
    async fn set_ref(&self, name: &str, value: &str) -> Result<()> {
        (**self).set_ref(name, value).await
    }
    async fn cas_ref(&self, name: &str, expect: Option<&str>, new: &str) -> Result<bool> {
        (**self).cas_ref(name, expect, new).await
    }
    async fn delete_ref(&self, name: &str) -> Result<()> {
        (**self).delete_ref(name).await
    }
    async fn list_refs(&self) -> Result<Vec<(String, String)>> {
        (**self).list_refs().await
    }
    async fn get_config(&self, key: &str) -> Result<Option<String>> {
        (**self).get_config(key).await
    }
    async fn set_config(&self, key: &str, value: &str) -> Result<()> {
        (**self).set_config(key, value).await
    }
    async fn truncate_tree(&self) -> Result<()> {
        (**self).truncate_tree().await
    }
    async fn set_conflict(&self, path: &str, kind: &str) -> Result<()> {
        (**self).set_conflict(path, kind).await
    }
    async fn list_conflicts(&self) -> Result<Vec<(String, String)>> {
        (**self).list_conflicts().await
    }
    async fn clear_conflicts(&self) -> Result<()> {
        (**self).clear_conflicts().await
    }
    async fn acquire_lock(&self, path: &str, owner: &str, at: i64) -> Result<bool> {
        (**self).acquire_lock(path, owner, at).await
    }
    async fn release_lock(&self, path: &str, owner: &str) -> Result<bool> {
        (**self).release_lock(path, owner).await
    }
    async fn list_locks(&self) -> Result<Vec<(String, String, i64)>> {
        (**self).list_locks().await
    }
    async fn create_actor(&self, init: ActorInit) -> Result<i64> {
        (**self).create_actor(init).await
    }
    async fn get_actor(&self, id: i64) -> Result<Option<Actor>> {
        (**self).get_actor(id).await
    }
    async fn create_session(
        &self,
        actor_id: i64,
        client: Option<&str>,
        started_at: i64,
    ) -> Result<i64> {
        (**self).create_session(actor_id, client, started_at).await
    }
    async fn record_tool_call(&self, tc: ToolCallInit) -> Result<i64> {
        (**self).record_tool_call(tc).await
    }
    async fn append_edit_op(&self, op: EditOpInit) -> Result<i64> {
        (**self).append_edit_op(op).await
    }
    async fn list_edit_ops(&self, actor_id: i64, session_id: Option<i64>) -> Result<Vec<EditOp>> {
        (**self).list_edit_ops(actor_id, session_id).await
    }
    async fn set_line_blame(&self, ino: Ino, runs: &str) -> Result<()> {
        (**self).set_line_blame(ino, runs).await
    }
    async fn get_line_blame(&self, ino: Ino) -> Result<Option<String>> {
        (**self).get_line_blame(ino).await
    }
    async fn append_event(&self, ev: EventInit, ts: i64) -> Result<i64> {
        (**self).append_event(ev, ts).await
    }
    async fn events_since(&self, after_seq: i64, limit: i64) -> Result<Vec<Event>> {
        (**self).events_since(after_seq, limit).await
    }
    async fn touch_presence(
        &self,
        session_id: i64,
        actor_id: i64,
        path: Option<&str>,
        at: i64,
    ) -> Result<()> {
        (**self)
            .touch_presence(session_id, actor_id, path, at)
            .await
    }
    async fn active_presence(&self, since_ts: i64) -> Result<Vec<Presence>> {
        (**self).active_presence(since_ts).await
    }
    async fn reap_presence(&self, older_than: i64) -> Result<u64> {
        (**self).reap_presence(older_than).await
    }
    async fn create_suggestion(&self, init: SuggestionInit, ts: i64) -> Result<i64> {
        (**self).create_suggestion(init, ts).await
    }
    async fn get_suggestion(&self, id: i64) -> Result<Option<Suggestion>> {
        (**self).get_suggestion(id).await
    }
    async fn list_suggestions(
        &self,
        status: Option<SuggestionStatus>,
        path: Option<&str>,
    ) -> Result<Vec<Suggestion>> {
        (**self).list_suggestions(status, path).await
    }
    async fn resolve_suggestion(
        &self,
        id: i64,
        status: SuggestionStatus,
        resolved_by: Option<i64>,
        ts: i64,
    ) -> Result<bool> {
        (**self)
            .resolve_suggestion(id, status, resolved_by, ts)
            .await
    }
}
