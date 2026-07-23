//! Python bindings for afs — an async-native module for driving a workspace
//! from Python (FastAPI, scripts, orchestration).
//!
//! Every I/O method returns a Python awaitable (via `pyo3-async-runtimes`), so
//! it drops straight into `async def` endpoints:
//!
//! ```python
//! import afs
//! ws = await afs.Workspace.open_local("meta.db", "cas")
//! # attribute a write to the authenticated user / agent you resolved yourself:
//! ctx = afs.WriteCtx.session(actor_id, session_id)
//! await ws.write_as(ctx, "/notes.txt", b"hello")
//! ```
//!
//! Structured results come back as plain dicts/lists so they are directly
//! JSON-serializable in an API response. Mounting (FUSE) and NFS serving are
//! exposed so orchestration can live in Python too.

use afs_core::{LocalCasStore, PostgresMetadataStore};
use afs_sdk::{
    Actor, BlameRange, CommitInfo, DiffEntry, DiffStatus, DirEntry, Event, Inode, Presence,
    Suggestion, SuggestionStatus, Workspace as CoreWorkspace, WriteCtx as CoreWriteCtx,
};
use pyo3::create_exception;
use pyo3::exceptions::{
    PyFileExistsError, PyFileNotFoundError, PyIsADirectoryError, PyNotADirectoryError, PyOSError,
    PyValueError,
};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};
use pyo3_async_runtimes::tokio::future_into_py;
use std::path::Path;
use std::sync::Arc;

create_exception!(afs, AfsError, pyo3::exceptions::PyException);
create_exception!(afs, ConflictError, AfsError);

/// Map an afs error onto the closest Python exception.
fn to_pyerr(e: afs_sdk::AfsError) -> PyErr {
    use afs_sdk::AfsError::*;
    let msg = e.to_string();
    match e {
        NotFound(_) | ContentMissing(_) => PyFileNotFoundError::new_err(msg),
        AlreadyExists(_) => PyFileExistsError::new_err(msg),
        NotADirectory(_) => PyNotADirectoryError::new_err(msg),
        IsADirectory(_) => PyIsADirectoryError::new_err(msg),
        DirectoryNotEmpty(_) => PyOSError::new_err(msg),
        InvalidArgument(_) | InvalidPath(_) => PyValueError::new_err(msg),
        Conflict(_) => ConflictError::new_err(msg),
        _ => AfsError::new_err(msg),
    }
}

fn io_err(e: std::io::Error) -> PyErr {
    PyOSError::new_err(e.to_string())
}

// --- dict builders (kept JSON-serializable) ---------------------------------

fn diff_status_str(s: DiffStatus) -> &'static str {
    match s {
        DiffStatus::Added => "added",
        DiffStatus::Modified => "modified",
        DiffStatus::Deleted => "deleted",
    }
}

fn hash_opt(h: Option<&afs_sdk::Hash>) -> Option<String> {
    h.map(|h| h.to_hex())
}

fn inode_dict(py: Python<'_>, i: &Inode) -> PyResult<Py<PyAny>> {
    let d = PyDict::new(py);
    d.set_item("ino", i.ino)?;
    d.set_item("kind", i.kind.as_str())?;
    d.set_item("mode", i.mode)?;
    d.set_item("nlink", i.nlink)?;
    d.set_item("size", i.size)?;
    d.set_item("content", hash_opt(i.content.as_ref()))?;
    d.set_item("mtime", i.mtime)?;
    d.set_item("ctime", i.ctime)?;
    Ok(d.into_any().unbind())
}

fn dir_entry_dict(py: Python<'_>, e: &DirEntry) -> PyResult<Py<PyAny>> {
    let d = PyDict::new(py);
    d.set_item("name", &e.name)?;
    d.set_item("ino", e.ino)?;
    d.set_item("kind", e.kind.as_str())?;
    Ok(d.into_any().unbind())
}

fn commit_dict(py: Python<'_>, c: &CommitInfo) -> PyResult<Py<PyAny>> {
    let d = PyDict::new(py);
    d.set_item("hash", c.hash.to_hex())?;
    d.set_item("author", &c.commit.author)?;
    d.set_item("message", &c.commit.message)?;
    d.set_item("timestamp", c.commit.timestamp)?;
    d.set_item(
        "parents",
        c.commit.parents.iter().map(|h| h.to_hex()).collect::<Vec<_>>(),
    )?;
    Ok(d.into_any().unbind())
}

fn diff_dict(py: Python<'_>, e: &DiffEntry) -> PyResult<Py<PyAny>> {
    let d = PyDict::new(py);
    d.set_item("path", &e.path)?;
    d.set_item("status", diff_status_str(e.status))?;
    Ok(d.into_any().unbind())
}

fn actor_dict(py: Python<'_>, a: &Actor) -> PyResult<Py<PyAny>> {
    let d = PyDict::new(py);
    d.set_item("id", a.id)?;
    d.set_item("kind", a.kind.as_str())?;
    d.set_item("display_name", &a.display_name)?;
    d.set_item("auth_subject", a.auth_subject.clone())?;
    d.set_item("agent_model", a.agent_model.clone())?;
    d.set_item("agent_vendor", a.agent_vendor.clone())?;
    d.set_item("controller_actor_id", a.controller_actor_id)?;
    d.set_item("created_at", a.created_at)?;
    Ok(d.into_any().unbind())
}

fn blame_dict(py: Python<'_>, b: &BlameRange) -> PyResult<Py<PyAny>> {
    let d = PyDict::new(py);
    d.set_item("line_start", b.line_start)?;
    d.set_item("line_end", b.line_end)?;
    d.set_item("session", b.session)?;
    d.set_item("actor", actor_dict(py, &b.actor)?)?;
    Ok(d.into_any().unbind())
}

fn event_dict(py: Python<'_>, e: &Event) -> PyResult<Py<PyAny>> {
    let d = PyDict::new(py);
    d.set_item("seq", e.seq)?;
    d.set_item("actor_id", e.actor_id)?;
    d.set_item("session_id", e.session_id)?;
    d.set_item("kind", &e.kind)?;
    d.set_item("path", &e.path)?;
    d.set_item("detail", e.detail.clone())?;
    d.set_item("ts", e.ts)?;
    d.set_item("branch", e.branch.clone())?;
    Ok(d.into_any().unbind())
}

fn presence_dict(py: Python<'_>, p: &Presence) -> PyResult<Py<PyAny>> {
    let d = PyDict::new(py);
    d.set_item("session_id", p.session_id)?;
    d.set_item("actor_id", p.actor_id)?;
    d.set_item("display_name", &p.display_name)?;
    d.set_item("kind", p.kind.as_str())?;
    d.set_item("path", p.path.clone())?;
    d.set_item("last_seen", p.last_seen)?;
    Ok(d.into_any().unbind())
}

fn suggestion_dict(py: Python<'_>, s: &Suggestion) -> PyResult<Py<PyAny>> {
    let d = PyDict::new(py);
    d.set_item("id", s.id)?;
    d.set_item("actor_id", s.actor_id)?;
    d.set_item("session_id", s.session_id)?;
    d.set_item("branch", s.branch.clone())?;
    d.set_item("path", &s.path)?;
    d.set_item("base_hash", s.base_hash.clone())?;
    d.set_item("proposed_hash", s.proposed_hash.clone())?;
    d.set_item("summary", s.summary.clone())?;
    d.set_item("status", s.status.as_str())?;
    d.set_item("created_ts", s.created_ts)?;
    d.set_item("resolved_ts", s.resolved_ts)?;
    d.set_item("resolved_by", s.resolved_by)?;
    Ok(d.into_any().unbind())
}

// --- WriteCtx ---------------------------------------------------------------

/// The actor context to attribute a write to — construct it from whatever
/// user/agent you resolved in your endpoint. Passed by value to `write_as`,
/// `suggest`, `accept_suggestion`, … so it opts into `FromPyObject`.
#[pyclass(frozen, from_py_object)]
#[derive(Clone, Copy)]
struct WriteCtx {
    inner: CoreWriteCtx,
}

#[pymethods]
impl WriteCtx {
    /// Attribute to an actor (no session).
    #[staticmethod]
    fn actor(actor: i64) -> Self {
        Self {
            inner: CoreWriteCtx::actor(actor),
        }
    }

    /// Attribute to an actor acting within a session.
    #[staticmethod]
    fn session(actor: i64, session: i64) -> Self {
        Self {
            inner: CoreWriteCtx::session(actor, session),
        }
    }

    #[getter]
    fn actor_id(&self) -> i64 {
        self.inner.actor
    }

    #[getter]
    fn session_id(&self) -> Option<i64> {
        self.inner.session
    }

    fn __repr__(&self) -> String {
        format!(
            "WriteCtx(actor={}, session={:?})",
            self.inner.actor, self.inner.session
        )
    }
}

// --- FUSE mount handle ------------------------------------------------------

/// A live FUSE mount. Unmounts when `unmount()` is called or the object is
/// dropped. Usable as a context manager.
#[pyclass]
struct Mount {
    session: Option<fuser::BackgroundSession>,
    mountpoint: String,
}

#[pymethods]
impl Mount {
    /// Unmount now (idempotent).
    fn unmount(&mut self) {
        self.session.take(); // dropping the BackgroundSession unmounts
    }

    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    #[pyo3(signature = (*_args))]
    fn __exit__(&mut self, _args: &Bound<'_, PyAny>) {
        self.unmount();
    }

    fn __repr__(&self) -> String {
        let state = if self.session.is_some() { "mounted" } else { "unmounted" };
        format!("Mount(mountpoint={:?}, {state})", self.mountpoint)
    }
}

// --- Workspace --------------------------------------------------------------

/// An afs workspace. Open one with a classmethod, then drive it with async
/// methods. Cheap to hold; clones share the same backend.
#[pyclass]
struct Workspace {
    inner: CoreWorkspace,
}

#[pymethods]
impl Workspace {
    /// Open (creating if needed) a local workspace: SQLite metadata at
    /// `db_path`, content-addressed chunks under `cas_dir`.
    #[staticmethod]
    fn open_local<'py>(
        py: Python<'py>,
        db_path: String,
        cas_dir: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        future_into_py(py, async move {
            let ws = CoreWorkspace::open_local(&db_path, &cas_dir)
                .await
                .map_err(to_pyerr)?;
            Python::attach(|py| Py::new(py, Workspace { inner: ws }))
        })
    }

    /// Open a local workspace whose chunks are batched into pack objects
    /// (`data_dir`), with the pack index under `index_dir`.
    #[staticmethod]
    fn open_local_packed<'py>(
        py: Python<'py>,
        db_path: String,
        data_dir: String,
        index_dir: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        future_into_py(py, async move {
            let ws = CoreWorkspace::open_local_packed(&db_path, &data_dir, &index_dir)
                .await
                .map_err(to_pyerr)?;
            Python::attach(|py| Py::new(py, Workspace { inner: ws }))
        })
    }

    /// Open a workspace with Postgres metadata (multi-writer) over a local CAS.
    /// `dsn` is a libpq URL/DSN, e.g. `postgres://user:pass@host/db`.
    #[staticmethod]
    fn open_pg<'py>(
        py: Python<'py>,
        dsn: String,
        cas_dir: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        future_into_py(py, async move {
            let meta = Arc::new(PostgresMetadataStore::connect(&dsn).await.map_err(to_pyerr)?);
            let content = Arc::new(LocalCasStore::open(&cas_dir).await.map_err(to_pyerr)?);
            let ws = CoreWorkspace::open(meta, content).await.map_err(to_pyerr)?;
            Python::attach(|py| Py::new(py, Workspace { inner: ws }))
        })
    }

    // --- files --------------------------------------------------------------

    /// Read a file's bytes.
    fn read<'py>(&self, py: Python<'py>, path: String) -> PyResult<Bound<'py, PyAny>> {
        let ws = self.inner.clone();
        future_into_py(py, async move {
            let bytes = ws.read(&path).await.map_err(to_pyerr)?;
            Python::attach(|py| Ok(PyBytes::new(py, &bytes).into_any().unbind()))
        })
    }

    /// Write a file (unattributed). Creates parent directories.
    fn write<'py>(
        &self,
        py: Python<'py>,
        path: String,
        data: Vec<u8>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let ws = self.inner.clone();
        future_into_py(py, async move {
            ws.write(&path, &data).await.map_err(to_pyerr)?;
            Ok(())
        })
    }

    /// Write a file attributed to `ctx` (records blame + an edit-op). This is
    /// how you inject the authenticated user/agent behind a request.
    fn write_as<'py>(
        &self,
        py: Python<'py>,
        ctx: WriteCtx,
        path: String,
        data: Vec<u8>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let ws = self.inner.clone();
        let c = ctx.inner;
        future_into_py(py, async move {
            ws.write_as(c, &path, &data).await.map_err(to_pyerr)?;
            Ok(())
        })
    }

    /// Create a directory and any missing parents.
    fn mkdir_p<'py>(&self, py: Python<'py>, path: String) -> PyResult<Bound<'py, PyAny>> {
        let ws = self.inner.clone();
        future_into_py(py, async move {
            ws.mkdir_p(&path).await.map_err(to_pyerr)?;
            Ok(())
        })
    }

    /// List a directory (returns a list of `{name, ino, kind}`).
    fn ls<'py>(&self, py: Python<'py>, path: String) -> PyResult<Bound<'py, PyAny>> {
        let ws = self.inner.clone();
        future_into_py(py, async move {
            let entries = ws.ls(&path).await.map_err(to_pyerr)?;
            Python::attach(|py| {
                entries
                    .iter()
                    .map(|e| dir_entry_dict(py, e))
                    .collect::<PyResult<Vec<_>>>()
            })
        })
    }

    /// Inode metadata for a path.
    fn stat<'py>(&self, py: Python<'py>, path: String) -> PyResult<Bound<'py, PyAny>> {
        let ws = self.inner.clone();
        future_into_py(py, async move {
            let inode = ws.stat(&path).await.map_err(to_pyerr)?;
            Python::attach(|py| inode_dict(py, &inode))
        })
    }

    /// Remove a file or empty directory.
    fn remove<'py>(&self, py: Python<'py>, path: String) -> PyResult<Bound<'py, PyAny>> {
        let ws = self.inner.clone();
        future_into_py(py, async move {
            ws.remove(&path).await.map_err(to_pyerr)?;
            Ok(())
        })
    }

    /// Move/rename a path.
    fn rename<'py>(
        &self,
        py: Python<'py>,
        from: String,
        to: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let ws = self.inner.clone();
        future_into_py(py, async move {
            ws.rename(&from, &to).await.map_err(to_pyerr)?;
            Ok(())
        })
    }

    // --- versioning ---------------------------------------------------------

    /// Snapshot the working tree into a commit; returns the commit hash (hex).
    fn commit<'py>(
        &self,
        py: Python<'py>,
        author: String,
        message: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let ws = self.inner.clone();
        future_into_py(py, async move {
            let h = ws.commit(&author, &message).await.map_err(to_pyerr)?;
            Ok(h.to_hex())
        })
    }

    /// Commit history (HEAD, first-parent), newest first.
    fn log<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let ws = self.inner.clone();
        future_into_py(py, async move {
            let log = ws.log().await.map_err(to_pyerr)?;
            Python::attach(|py| log.iter().map(|c| commit_dict(py, c)).collect::<PyResult<Vec<_>>>())
        })
    }

    /// Working-tree changes relative to HEAD.
    fn status<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let ws = self.inner.clone();
        future_into_py(py, async move {
            let changes = ws.status().await.map_err(to_pyerr)?;
            Python::attach(|py| changes.iter().map(|d| diff_dict(py, d)).collect::<PyResult<Vec<_>>>())
        })
    }

    /// Changed paths between two refs/commits (`from` -> `to`).
    fn diff<'py>(
        &self,
        py: Python<'py>,
        from: String,
        to: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let ws = self.inner.clone();
        future_into_py(py, async move {
            let changes = ws.diff(&from, &to).await.map_err(to_pyerr)?;
            Python::attach(|py| changes.iter().map(|d| diff_dict(py, d)).collect::<PyResult<Vec<_>>>())
        })
    }

    /// A unified line diff of one path between two refs/commits.
    fn diff_file<'py>(
        &self,
        py: Python<'py>,
        from: String,
        to: String,
        path: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let ws = self.inner.clone();
        future_into_py(py, async move {
            let patch = ws.diff_file(&from, &to, &path).await.map_err(to_pyerr)?;
            Ok(patch)
        })
    }

    /// Create a branch at the current HEAD commit.
    fn create_branch<'py>(&self, py: Python<'py>, name: String) -> PyResult<Bound<'py, PyAny>> {
        let ws = self.inner.clone();
        future_into_py(py, async move {
            ws.create_branch(&name).await.map_err(to_pyerr)?;
            Ok(())
        })
    }

    /// Switch the working tree to a branch.
    fn checkout<'py>(&self, py: Python<'py>, name: String) -> PyResult<Bound<'py, PyAny>> {
        let ws = self.inner.clone();
        future_into_py(py, async move {
            ws.checkout(&name).await.map_err(to_pyerr)?;
            Ok(())
        })
    }

    /// All branches as `{name, hash}`.
    fn branches<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let ws = self.inner.clone();
        future_into_py(py, async move {
            let branches = ws.list_branches().await.map_err(to_pyerr)?;
            Python::attach(|py| {
                branches
                    .iter()
                    .map(|(name, hash)| {
                        let d = PyDict::new(py);
                        d.set_item("name", name)?;
                        d.set_item("hash", hash.to_hex())?;
                        Ok(d.into_any().unbind())
                    })
                    .collect::<PyResult<Vec<Py<PyAny>>>>()
            })
        })
    }

    /// The current branch name (or `None` if detached).
    fn current_branch<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let ws = self.inner.clone();
        future_into_py(py, async move {
            let b = ws.current_branch().await.map_err(to_pyerr)?;
            Ok(b)
        })
    }

    // --- attribution --------------------------------------------------------

    /// Register a human actor; returns its id.
    #[pyo3(signature = (name, auth_subject=None))]
    fn create_human<'py>(
        &self,
        py: Python<'py>,
        name: String,
        auth_subject: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let ws = self.inner.clone();
        future_into_py(py, async move {
            let id = ws
                .create_human(&name, auth_subject.as_deref())
                .await
                .map_err(to_pyerr)?;
            Ok(id)
        })
    }

    /// Register an agent actor (optionally controlled by a human); returns id.
    #[pyo3(signature = (name, model, controller=None))]
    fn create_agent<'py>(
        &self,
        py: Python<'py>,
        name: String,
        model: String,
        controller: Option<i64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let ws = self.inner.clone();
        future_into_py(py, async move {
            let id = ws
                .create_agent(&name, &model, controller)
                .await
                .map_err(to_pyerr)?;
            Ok(id)
        })
    }

    /// Open a session for an actor; returns its id.
    #[pyo3(signature = (actor_id, client=None))]
    fn create_session<'py>(
        &self,
        py: Python<'py>,
        actor_id: i64,
        client: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let ws = self.inner.clone();
        future_into_py(py, async move {
            let id = ws
                .create_session(actor_id, client.as_deref())
                .await
                .map_err(to_pyerr)?;
            Ok(id)
        })
    }

    /// Per-line-range authorship for a path.
    fn blame<'py>(&self, py: Python<'py>, path: String) -> PyResult<Bound<'py, PyAny>> {
        let ws = self.inner.clone();
        future_into_py(py, async move {
            let ranges = ws.blame(&path).await.map_err(to_pyerr)?;
            Python::attach(|py| ranges.iter().map(|b| blame_dict(py, b)).collect::<PyResult<Vec<_>>>())
        })
    }

    // --- live collaboration -------------------------------------------------

    /// Change-feed events strictly after `after_seq` (oldest first).
    #[pyo3(signature = (after_seq=0))]
    fn watch<'py>(&self, py: Python<'py>, after_seq: i64) -> PyResult<Bound<'py, PyAny>> {
        let ws = self.inner.clone();
        future_into_py(py, async move {
            let events = ws.watch(after_seq).await.map_err(to_pyerr)?;
            Python::attach(|py| events.iter().map(|e| event_dict(py, e)).collect::<PyResult<Vec<_>>>())
        })
    }

    /// Sessions active within the last `window_secs` seconds.
    #[pyo3(signature = (window_secs=60))]
    fn presence<'py>(&self, py: Python<'py>, window_secs: i64) -> PyResult<Bound<'py, PyAny>> {
        let ws = self.inner.clone();
        future_into_py(py, async move {
            let list = ws.presence(window_secs).await.map_err(to_pyerr)?;
            Python::attach(|py| list.iter().map(|p| presence_dict(py, p)).collect::<PyResult<Vec<_>>>())
        })
    }

    /// Heartbeat a session's presence (and current path).
    #[pyo3(signature = (actor_id, session_id, path=None))]
    fn touch<'py>(
        &self,
        py: Python<'py>,
        actor_id: i64,
        session_id: i64,
        path: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let ws = self.inner.clone();
        future_into_py(py, async move {
            ws.touch(actor_id, session_id, path.as_deref())
                .await
                .map_err(to_pyerr)?;
            Ok(())
        })
    }

    // --- agent-suggestion review queue --------------------------------------

    /// Propose an edit to `path` for review (does not touch the working tree).
    #[pyo3(signature = (ctx, path, data, summary=None))]
    fn suggest<'py>(
        &self,
        py: Python<'py>,
        ctx: WriteCtx,
        path: String,
        data: Vec<u8>,
        summary: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let ws = self.inner.clone();
        let c = ctx.inner;
        future_into_py(py, async move {
            let id = ws
                .suggest(c, &path, &data, summary.as_deref())
                .await
                .map_err(to_pyerr)?;
            Ok(id)
        })
    }

    /// Propose deleting `path`.
    #[pyo3(signature = (ctx, path, summary=None))]
    fn suggest_delete<'py>(
        &self,
        py: Python<'py>,
        ctx: WriteCtx,
        path: String,
        summary: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let ws = self.inner.clone();
        let c = ctx.inner;
        future_into_py(py, async move {
            let id = ws
                .suggest_delete(c, &path, summary.as_deref())
                .await
                .map_err(to_pyerr)?;
            Ok(id)
        })
    }

    /// Suggestions, optionally filtered by `status` and/or `path`, newest first.
    #[pyo3(signature = (status=None, path=None))]
    fn list_suggestions<'py>(
        &self,
        py: Python<'py>,
        status: Option<String>,
        path: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let ws = self.inner.clone();
        future_into_py(py, async move {
            let st = match status.as_deref() {
                Some(s) => Some(
                    SuggestionStatus::parse(s)
                        .ok_or_else(|| PyValueError::new_err(format!("unknown status {s:?}")))?,
                ),
                None => None,
            };
            let list = ws
                .list_suggestions(st, path.as_deref())
                .await
                .map_err(to_pyerr)?;
            Python::attach(|py| list.iter().map(|s| suggestion_dict(py, s)).collect::<PyResult<Vec<_>>>())
        })
    }

    /// A single suggestion by id, or `None`.
    fn get_suggestion<'py>(&self, py: Python<'py>, id: i64) -> PyResult<Bound<'py, PyAny>> {
        let ws = self.inner.clone();
        future_into_py(py, async move {
            let s = ws.get_suggestion(id).await.map_err(to_pyerr)?;
            Python::attach(|py| match s {
                Some(s) => suggestion_dict(py, &s).map(Some),
                None => Ok(None),
            })
        })
    }

    /// Render a suggestion as a unified line diff.
    fn suggestion_diff<'py>(&self, py: Python<'py>, id: i64) -> PyResult<Bound<'py, PyAny>> {
        let ws = self.inner.clone();
        future_into_py(py, async move {
            let patch = ws.suggestion_diff(id).await.map_err(to_pyerr)?;
            Ok(patch)
        })
    }

    /// Accept a pending suggestion, attributed to `approver`.
    fn accept_suggestion<'py>(
        &self,
        py: Python<'py>,
        id: i64,
        approver: WriteCtx,
    ) -> PyResult<Bound<'py, PyAny>> {
        let ws = self.inner.clone();
        let c = approver.inner;
        future_into_py(py, async move {
            ws.accept_suggestion(id, c).await.map_err(to_pyerr)?;
            Ok(())
        })
    }

    /// Reject a pending suggestion.
    fn reject_suggestion<'py>(
        &self,
        py: Python<'py>,
        id: i64,
        approver: WriteCtx,
    ) -> PyResult<Bound<'py, PyAny>> {
        let ws = self.inner.clone();
        let c = approver.inner;
        future_into_py(py, async move {
            ws.reject_suggestion(id, c).await.map_err(to_pyerr)?;
            Ok(())
        })
    }

    // --- mounting / serving -------------------------------------------------

    /// Mount this workspace as a FUSE filesystem at `mountpoint`, in the
    /// background. Returns a `Mount` handle; unmount by calling `.unmount()`,
    /// exiting its `with` block, or dropping it. Requires FUSE (`/dev/fuse`).
    fn mount(&self, py: Python<'_>, mountpoint: String) -> PyResult<Mount> {
        let ws = self.inner.clone();
        let mp = mountpoint.clone();
        let session = py
            .detach(move || afs_fuse::spawn(ws, Path::new(&mp)))
            .map_err(io_err)?;
        Ok(Mount {
            session: Some(session),
            mountpoint,
        })
    }

    /// Serve this workspace over NFSv3 at `addr` (e.g. `127.0.0.1:11111`). The
    /// returned awaitable runs until cancelled — drive it as a background task
    /// (`task = asyncio.create_task(ws.serve_nfs(addr))`) and `task.cancel()`
    /// to stop.
    fn serve_nfs<'py>(&self, py: Python<'py>, addr: String) -> PyResult<Bound<'py, PyAny>> {
        let ws = self.inner.clone();
        future_into_py(py, async move {
            afs_nfs::serve(ws, &addr).await.map_err(io_err)?;
            Ok(())
        })
    }
}

/// Whether a FUSE mount is possible here (`/dev/fuse` present and usable).
#[pyfunction]
fn fuse_mountable() -> bool {
    afs_fuse::mountable()
}

/// The compiled extension is imported as `afs._afs`; the pure-Python package
/// `afs` (see `python/afs/__init__.py`) re-exports everything from it and adds
/// optional integrations like `afs.fastapi`.
#[pymodule]
fn _afs(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Workspace>()?;
    m.add_class::<WriteCtx>()?;
    m.add_class::<Mount>()?;
    m.add_function(wrap_pyfunction!(fuse_mountable, m)?)?;
    m.add("AfsError", m.py().get_type::<AfsError>())?;
    m.add("ConflictError", m.py().get_type::<ConflictError>())?;
    Ok(())
}
