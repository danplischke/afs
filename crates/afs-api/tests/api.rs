//! HTTP surface: files, versioning, attribution, and the collaboration feed,
//! driven in-process through the router (no socket) via `tower::oneshot`.

use afs_api::router;
use afs_sdk::Workspace;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

async fn app() -> Router {
    let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
    let ws = Workspace::open_local(dir.path().join("meta.db"), dir.path().join("cas"))
        .await
        .unwrap();
    router(Arc::new(ws))
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Vec<u8>) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, body)
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn put_bytes(uri: &str, body: &[u8]) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(uri)
        .body(Body::from(body.to_vec()))
        .unwrap()
}

fn post_json(uri: &str, v: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&v).unwrap()))
        .unwrap()
}

fn as_json(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap()
}

#[tokio::test]
async fn files_roundtrip_and_listing() {
    let app = app().await;

    // write (auto-creates the parent dir)
    let (st, body) = send(&app, put_bytes("/files/notes/todo.txt", b"buy milk\n")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(as_json(&body)["written"], 9);

    // read it back verbatim
    let (st, body) = send(&app, get("/files/notes/todo.txt")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body, b"buy milk\n");

    // list the directory
    let (st, body) = send(&app, get("/dirs/notes")).await;
    assert_eq!(st, StatusCode::OK);
    let entries = as_json(&body);
    assert_eq!(entries[0]["name"], "todo.txt");
    assert_eq!(entries[0]["kind"], "file");

    // stat
    let (st, body) = send(&app, get("/stat/notes/todo.txt")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(as_json(&body)["size"], 9);
    assert_eq!(as_json(&body)["kind"], "file");

    // delete
    let (st, _) = send(&app, Request::builder().method("DELETE").uri("/files/notes/todo.txt").body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = send(&app, get("/files/notes/todo.txt")).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn missing_file_is_404_and_dir_read_is_400() {
    let app = app().await;
    let (st, body) = send(&app, get("/files/nope.txt")).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert!(as_json(&body)["error"].as_str().unwrap().contains("not found"));

    send(&app, post_json("/dirs/adir", json!({}))).await;
    let (st, _) = send(&app, get("/files/adir")).await; // reading a directory
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn versioning_over_http() {
    let app = app().await;
    send(&app, put_bytes("/files/a.txt", b"one")).await;
    let (st, body) = send(&app, post_json("/commit", json!({"author": "dan", "message": "first"}))).await;
    assert_eq!(st, StatusCode::OK);
    assert!(as_json(&body)["hash"].as_str().unwrap().len() >= 12);

    let (st, body) = send(&app, get("/log")).await;
    assert_eq!(st, StatusCode::OK);
    let log = as_json(&body);
    assert_eq!(log.as_array().unwrap().len(), 1);
    assert_eq!(log[0]["message"], "first");
    assert_eq!(log[0]["author"], "dan");

    // a branch shows up as current
    let (_st, body) = send(&app, get("/branches")).await;
    let branches = as_json(&body);
    assert!(branches.as_array().unwrap().iter().any(|b| b["name"] == "main" && b["current"] == true));
}

#[tokio::test]
async fn attributed_write_shows_up_in_blame_and_feed() {
    let app = app().await;

    // register an agent actor + session
    let (_st, body) = send(&app, post_json("/actors", json!({"name": "claude", "agent": true, "model": "opus"}))).await;
    let actor = as_json(&body)["id"].as_i64().unwrap();
    let (_st, body) = send(&app, post_json("/sessions", json!({"actor": actor}))).await;
    let session = as_json(&body)["id"].as_i64().unwrap();

    // attributed write via query params
    let uri = format!("/files/notes.txt?actor={actor}&session={session}");
    let (st, _) = send(&app, put_bytes(&uri, b"line one\nline two\n")).await;
    assert_eq!(st, StatusCode::OK);

    // blame attributes it to the agent
    let (st, body) = send(&app, get("/blame/notes.txt")).await;
    assert_eq!(st, StatusCode::OK);
    let blame = as_json(&body);
    assert_eq!(blame[0]["kind"], "agent");
    assert_eq!(blame[0]["actor"], "claude");

    // the write is on the change feed, attributed
    let (st, body) = send(&app, get("/events")).await;
    assert_eq!(st, StatusCode::OK);
    let events = as_json(&body);
    let write = events
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "write")
        .unwrap();
    assert_eq!(write["actor_id"].as_i64(), Some(actor));
    assert_eq!(write["session_id"].as_i64(), Some(session));
}
