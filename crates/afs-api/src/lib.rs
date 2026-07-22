//! afs-api — an HTTP/JSON surface over a workspace (`docs/DESIGN.md` §6, M7).
//!
//! A thin [`axum`] layer that exposes the same operations as the `afs` CLI to any
//! HTTP client: read/write/list files, versioning (commit/log/branches/checkout),
//! attribution (blame), and the live-collaboration feed + presence. Everything
//! goes through [`afs_sdk::Workspace`], so writes are recorded on the change feed
//! and attributed exactly as they are everywhere else.
//!
//! Files are transferred as raw bytes (`application/octet-stream`); everything
//! else is JSON. Paths are the URL tail after the resource segment, e.g.
//! `GET /files/notes/todo.txt` reads `/notes/todo.txt`.

use afs_sdk::{Workspace, WriteCtx};
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;

type Shared = Arc<Workspace>;

/// Build the router for a workspace.
pub fn router(ws: Shared) -> Router {
    Router::new()
        .route("/health", get(health))
        .route(
            "/files/{*path}",
            get(read_file).put(write_file).delete(delete_file),
        )
        .route("/dirs", get(list_root).post(make_root))
        .route("/dirs/{*path}", get(list_dir).post(make_dir))
        .route("/stat/{*path}", get(stat))
        .route("/blame/{*path}", get(blame))
        .route("/rename", post(rename))
        .route("/commit", post(commit))
        .route("/log", get(log))
        .route("/diff", get(diff))
        .route("/diff/file", get(diff_file))
        .route("/branches", get(list_branches).post(create_branch))
        .route("/checkout", post(checkout))
        .route("/events", get(events))
        .route("/presence", get(presence))
        .route("/suggestions", get(list_suggestions).post(create_suggestion))
        .route("/suggestions/{id}", get(get_suggestion))
        .route("/suggestions/{id}/diff", get(suggestion_diff))
        .route("/suggestions/{id}/accept", post(accept_suggestion))
        .route("/suggestions/{id}/reject", post(reject_suggestion))
        .route("/actors", post(create_actor))
        .route("/sessions", post(create_session))
        .with_state(ws)
}

/// Serve the workspace over HTTP, blocking until the server stops.
pub async fn serve(ws: Shared, addr: SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router(ws)).await
}

/// Normalize a URL-tail path to an absolute afs path.
fn abspath(p: &str) -> String {
    if p.starts_with('/') {
        p.to_string()
    } else {
        format!("/{p}")
    }
}

// --- error mapping ----------------------------------------------------------

/// Wraps an [`afs_sdk::AfsError`] with an HTTP status.
struct ApiError(afs_sdk::AfsError);

impl From<afs_sdk::AfsError> for ApiError {
    fn from(e: afs_sdk::AfsError) -> Self {
        ApiError(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        use afs_sdk::AfsError::*;
        let status = match self.0 {
            NotFound(_) | ContentMissing(_) => StatusCode::NOT_FOUND,
            AlreadyExists(_) | Conflict(_) => StatusCode::CONFLICT,
            IsADirectory(_) | NotADirectory(_) | DirectoryNotEmpty(_) | InvalidPath(_)
            | InvalidArgument(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(json!({ "error": self.0.to_string() }))).into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

// --- files ------------------------------------------------------------------

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

async fn read_file(State(ws): State<Shared>, Path(path): Path<String>) -> ApiResult<Vec<u8>> {
    Ok(ws.read(&abspath(&path)).await?.to_vec())
}

#[derive(Deserialize)]
struct WriteQuery {
    actor: Option<i64>,
    session: Option<i64>,
}

async fn write_file(
    State(ws): State<Shared>,
    Path(path): Path<String>,
    Query(q): Query<WriteQuery>,
    body: Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    let p = abspath(&path);
    if let Some((parent, _)) = p.rsplit_once('/') {
        if !parent.is_empty() {
            ws.mkdir_p(parent).await?;
        }
    }
    match q.actor {
        Some(actor) => {
            let ctx = match q.session {
                Some(s) => WriteCtx::session(actor, s),
                None => WriteCtx::actor(actor),
            };
            ws.write_as(ctx, &p, &body).await?;
        }
        None => ws.write(&p, &body).await?,
    }
    Ok(Json(json!({ "path": p, "written": body.len() })))
}

async fn delete_file(
    State(ws): State<Shared>,
    Path(path): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let p = abspath(&path);
    ws.remove(&p).await?;
    Ok(Json(json!({ "removed": p })))
}

// --- directories ------------------------------------------------------------

#[derive(Serialize)]
struct EntryDto {
    name: String,
    kind: String,
}

async fn list_path(ws: &Workspace, path: &str) -> ApiResult<Json<Vec<EntryDto>>> {
    let entries = ws
        .ls(path)
        .await?
        .into_iter()
        .map(|e| EntryDto {
            name: e.name,
            kind: e.kind.as_str().to_string(),
        })
        .collect();
    Ok(Json(entries))
}

async fn list_root(State(ws): State<Shared>) -> ApiResult<Json<Vec<EntryDto>>> {
    list_path(&ws, "/").await
}

async fn list_dir(
    State(ws): State<Shared>,
    Path(path): Path<String>,
) -> ApiResult<Json<Vec<EntryDto>>> {
    list_path(&ws, &abspath(&path)).await
}

async fn make_root(State(_ws): State<Shared>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(json!({ "created": "/" })))
}

async fn make_dir(
    State(ws): State<Shared>,
    Path(path): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let p = abspath(&path);
    ws.mkdir_p(&p).await?;
    Ok(Json(json!({ "created": p })))
}

#[derive(Serialize)]
struct InodeDto {
    ino: i64,
    kind: String,
    mode: u32,
    nlink: i64,
    size: u64,
    mtime: i64,
    ctime: i64,
}

async fn stat(State(ws): State<Shared>, Path(path): Path<String>) -> ApiResult<Json<InodeDto>> {
    let i = ws.stat(&abspath(&path)).await?;
    Ok(Json(InodeDto {
        ino: i.ino,
        kind: i.kind.as_str().to_string(),
        mode: i.mode,
        nlink: i.nlink,
        size: i.size,
        mtime: i.mtime,
        ctime: i.ctime,
    }))
}

#[derive(Deserialize)]
struct RenameReq {
    from: String,
    to: String,
}

async fn rename(
    State(ws): State<Shared>,
    Json(req): Json<RenameReq>,
) -> ApiResult<Json<serde_json::Value>> {
    ws.rename(&req.from, &req.to).await?;
    Ok(Json(json!({ "from": req.from, "to": req.to })))
}

// --- versioning -------------------------------------------------------------

#[derive(Deserialize)]
struct CommitReq {
    #[serde(default = "default_author")]
    author: String,
    message: String,
}
fn default_author() -> String {
    "api".to_string()
}

async fn commit(
    State(ws): State<Shared>,
    Json(req): Json<CommitReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let hash = ws.commit(&req.author, &req.message).await?;
    Ok(Json(json!({ "hash": hash.to_hex() })))
}

#[derive(Serialize)]
struct CommitDto {
    hash: String,
    author: String,
    message: String,
    timestamp: i64,
    parents: Vec<String>,
}

async fn log(State(ws): State<Shared>) -> ApiResult<Json<Vec<CommitDto>>> {
    let out = ws
        .log()
        .await?
        .into_iter()
        .map(|ci| CommitDto {
            hash: ci.hash.to_hex(),
            author: ci.commit.author,
            message: ci.commit.message,
            timestamp: ci.commit.timestamp,
            parents: ci.commit.parents.iter().map(|h| h.to_hex()).collect(),
        })
        .collect();
    Ok(Json(out))
}

#[derive(Deserialize)]
struct DiffQuery {
    from: String,
    to: String,
}

#[derive(Serialize)]
struct DiffEntryDto {
    path: String,
    status: &'static str,
}

/// `GET /diff?from=main&to=feature` — the changed-path list between two
/// refs/commits (compared by content address).
async fn diff(State(ws): State<Shared>, Query(q): Query<DiffQuery>) -> ApiResult<Json<Vec<DiffEntryDto>>> {
    let out = ws
        .diff(&q.from, &q.to)
        .await?
        .into_iter()
        .map(|d| DiffEntryDto {
            path: d.path,
            status: match d.status {
                afs_sdk::DiffStatus::Added => "added",
                afs_sdk::DiffStatus::Modified => "modified",
                afs_sdk::DiffStatus::Deleted => "deleted",
            },
        })
        .collect();
    Ok(Json(out))
}

#[derive(Deserialize)]
struct DiffFileQuery {
    from: String,
    to: String,
    path: String,
}

#[derive(Serialize)]
struct DiffFileDto {
    path: String,
    diff: String,
}

/// `GET /diff/file?from=main&to=feature&path=/x` — a unified line diff of one
/// path (empty `diff` when unchanged on both sides).
async fn diff_file(
    State(ws): State<Shared>,
    Query(q): Query<DiffFileQuery>,
) -> ApiResult<Json<DiffFileDto>> {
    let diff = ws.diff_file(&q.from, &q.to, &q.path).await?;
    Ok(Json(DiffFileDto {
        path: q.path,
        diff,
    }))
}

// --- agent-suggestion review queue ------------------------------------------

fn write_ctx(actor: i64, session: Option<i64>) -> WriteCtx {
    match session {
        Some(s) => WriteCtx::session(actor, s),
        None => WriteCtx::actor(actor),
    }
}

#[derive(Serialize)]
struct SuggestionDto {
    id: i64,
    actor_id: i64,
    session_id: Option<i64>,
    branch: Option<String>,
    path: String,
    base_hash: Option<String>,
    proposed_hash: Option<String>,
    summary: Option<String>,
    status: String,
    created_ts: i64,
    resolved_ts: Option<i64>,
    resolved_by: Option<i64>,
}

impl From<afs_sdk::Suggestion> for SuggestionDto {
    fn from(s: afs_sdk::Suggestion) -> Self {
        Self {
            id: s.id,
            actor_id: s.actor_id,
            session_id: s.session_id,
            branch: s.branch,
            path: s.path,
            base_hash: s.base_hash,
            proposed_hash: s.proposed_hash,
            summary: s.summary,
            status: s.status.as_str().to_string(),
            created_ts: s.created_ts,
            resolved_ts: s.resolved_ts,
            resolved_by: s.resolved_by,
        }
    }
}

#[derive(Deserialize)]
struct CreateSuggestQuery {
    actor: i64,
    session: Option<i64>,
    path: String,
    summary: Option<String>,
    #[serde(default)]
    delete: bool,
}

/// `POST /suggestions?actor=&path=&summary=` with the proposed bytes as the
/// body (or `&delete=true` and an empty body to propose a deletion).
async fn create_suggestion(
    State(ws): State<Shared>,
    Query(q): Query<CreateSuggestQuery>,
    body: Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    let ctx = write_ctx(q.actor, q.session);
    let id = if q.delete {
        ws.suggest_delete(ctx, &q.path, q.summary.as_deref()).await?
    } else {
        ws.suggest(ctx, &q.path, &body, q.summary.as_deref()).await?
    };
    Ok(Json(json!({ "id": id })))
}

#[derive(Deserialize)]
struct ListSuggestQuery {
    status: Option<String>,
    path: Option<String>,
}

async fn list_suggestions(
    State(ws): State<Shared>,
    Query(q): Query<ListSuggestQuery>,
) -> ApiResult<Json<Vec<SuggestionDto>>> {
    let status = match q.status.as_deref() {
        Some(s) => Some(
            afs_sdk::SuggestionStatus::parse(s)
                .ok_or_else(|| afs_sdk::AfsError::InvalidArgument(format!("bad status {s}")))?,
        ),
        None => None,
    };
    let out = ws
        .list_suggestions(status, q.path.as_deref())
        .await?
        .into_iter()
        .map(SuggestionDto::from)
        .collect();
    Ok(Json(out))
}

async fn get_suggestion(
    State(ws): State<Shared>,
    Path(id): Path<i64>,
) -> ApiResult<Json<SuggestionDto>> {
    let s = ws
        .get_suggestion(id)
        .await?
        .ok_or_else(|| afs_sdk::AfsError::NotFound(format!("suggestion #{id}")))?;
    Ok(Json(s.into()))
}

async fn suggestion_diff(
    State(ws): State<Shared>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let diff = ws.suggestion_diff(id).await?;
    Ok(Json(json!({ "id": id, "diff": diff })))
}

#[derive(Deserialize)]
struct ResolveQuery {
    actor: i64,
    session: Option<i64>,
}

async fn accept_suggestion(
    State(ws): State<Shared>,
    Path(id): Path<i64>,
    Query(q): Query<ResolveQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    ws.accept_suggestion(id, write_ctx(q.actor, q.session)).await?;
    Ok(Json(json!({ "accepted": id })))
}

async fn reject_suggestion(
    State(ws): State<Shared>,
    Path(id): Path<i64>,
    Query(q): Query<ResolveQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    ws.reject_suggestion(id, write_ctx(q.actor, q.session)).await?;
    Ok(Json(json!({ "rejected": id })))
}

#[derive(Serialize)]
struct BranchDto {
    name: String,
    hash: String,
    current: bool,
}

async fn list_branches(State(ws): State<Shared>) -> ApiResult<Json<Vec<BranchDto>>> {
    let current = ws.current_branch().await?;
    let out = ws
        .list_branches()
        .await?
        .into_iter()
        .map(|(name, hash)| BranchDto {
            current: current.as_deref() == Some(&name),
            name,
            hash: hash.to_hex(),
        })
        .collect();
    Ok(Json(out))
}

#[derive(Deserialize)]
struct BranchReq {
    name: String,
}

async fn create_branch(
    State(ws): State<Shared>,
    Json(req): Json<BranchReq>,
) -> ApiResult<Json<serde_json::Value>> {
    ws.create_branch(&req.name).await?;
    Ok(Json(json!({ "created": req.name })))
}

async fn checkout(
    State(ws): State<Shared>,
    Json(req): Json<BranchReq>,
) -> ApiResult<Json<serde_json::Value>> {
    ws.checkout(&req.name).await?;
    Ok(Json(json!({ "branch": req.name })))
}

// --- attribution ------------------------------------------------------------

#[derive(Serialize)]
struct BlameDto {
    line_start: u32,
    line_end: u32,
    actor: String,
    kind: String,
}

async fn blame(State(ws): State<Shared>, Path(path): Path<String>) -> ApiResult<Json<Vec<BlameDto>>> {
    let out = ws
        .blame(&abspath(&path))
        .await?
        .into_iter()
        .map(|r| BlameDto {
            line_start: r.line_start,
            line_end: r.line_end,
            actor: r.actor.display_name,
            kind: r.actor.kind.as_str().to_string(),
        })
        .collect();
    Ok(Json(out))
}

// --- collaboration ----------------------------------------------------------

#[derive(Serialize)]
struct EventDto {
    seq: i64,
    actor_id: Option<i64>,
    session_id: Option<i64>,
    kind: String,
    path: String,
    detail: Option<String>,
    ts: i64,
    branch: Option<String>,
}

#[derive(Deserialize)]
struct EventsQuery {
    since: Option<i64>,
    /// Restrict the feed to changes on this branch (the per-branch UI view).
    branch: Option<String>,
}

async fn events(
    State(ws): State<Shared>,
    Query(q): Query<EventsQuery>,
) -> ApiResult<Json<Vec<EventDto>>> {
    let out = ws
        .watch(q.since.unwrap_or(0))
        .await?
        .into_iter()
        .filter(|e| match &q.branch {
            Some(b) => e.branch.as_deref() == Some(b.as_str()),
            None => true,
        })
        .map(|e| EventDto {
            seq: e.seq,
            actor_id: e.actor_id,
            session_id: e.session_id,
            kind: e.kind,
            path: e.path,
            detail: e.detail,
            ts: e.ts,
            branch: e.branch,
        })
        .collect();
    Ok(Json(out))
}

#[derive(Serialize)]
struct PresenceDto {
    session_id: i64,
    actor_id: i64,
    display_name: String,
    kind: String,
    path: Option<String>,
    last_seen: i64,
}

#[derive(Deserialize)]
struct PresenceQuery {
    window: Option<i64>,
}

async fn presence(
    State(ws): State<Shared>,
    Query(q): Query<PresenceQuery>,
) -> ApiResult<Json<Vec<PresenceDto>>> {
    let out = ws
        .presence(q.window.unwrap_or(60))
        .await?
        .into_iter()
        .map(|p| PresenceDto {
            session_id: p.session_id,
            actor_id: p.actor_id,
            display_name: p.display_name,
            kind: p.kind.as_str().to_string(),
            path: p.path,
            last_seen: p.last_seen,
        })
        .collect();
    Ok(Json(out))
}

// --- actors + sessions ------------------------------------------------------

#[derive(Deserialize)]
struct ActorReq {
    name: String,
    #[serde(default)]
    agent: bool,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    controller: Option<i64>,
}

async fn create_actor(
    State(ws): State<Shared>,
    Json(req): Json<ActorReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = if req.agent {
        ws.create_agent(&req.name, req.model.as_deref().unwrap_or("unknown"), req.controller)
            .await?
    } else {
        ws.create_human(&req.name, None).await?
    };
    Ok(Json(json!({ "id": id })))
}

#[derive(Deserialize)]
struct SessionReq {
    actor: i64,
    #[serde(default)]
    client: Option<String>,
}

async fn create_session(
    State(ws): State<Shared>,
    Json(req): Json<SessionReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = ws.create_session(req.actor, req.client.as_deref()).await?;
    Ok(Json(json!({ "id": id })))
}
