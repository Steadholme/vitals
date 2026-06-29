//! `POST /ingest` auth + round-trip through the in-memory app (no database, no port bind).

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;
use vitals::config::DEFAULT_INGEST_TOKEN;
use vitals::{app, build_dev_state, AppState};

async fn send(state: &AppState, req: Request<Body>) -> (StatusCode, Vec<u8>) {
    let resp = app(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, bytes)
}

fn ingest_req(token: Option<&str>, body: Value) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri("/ingest")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(t) = token {
        b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    b.body(Body::from(body.to_string())).unwrap()
}

fn batch() -> Value {
    json!({
        "host": "node-a",
        "samples": [
            { "metric": "cpu_pct", "value": 12.5, "ts": 1_700_000_000 },
            { "metric": "mem_pct", "value": 48.0, "ts": 1_700_000_000 }
        ]
    })
}

#[tokio::test]
async fn ingest_without_token_is_401() {
    let state = build_dev_state();
    let (status, body) = send(&state, ingest_req(None, batch())).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"], "unauthorized");
    // Nothing was stored.
    assert!(state.store.latest().is_empty());
}

#[tokio::test]
async fn ingest_with_wrong_token_is_401() {
    let state = build_dev_state();
    let (status, _) = send(&state, ingest_req(Some("nope"), batch())).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn ingest_with_token_accepts_and_stores() {
    let state = build_dev_state();
    let (status, body) = send(&state, ingest_req(Some(DEFAULT_INGEST_TOKEN), batch())).await;
    assert_eq!(status, StatusCode::OK, "body: {}", String::from_utf8_lossy(&body));
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["accepted"], 2);

    // The samples are queryable.
    let rows = state.store.query(Some("node-a"), None, 0);
    assert_eq!(rows.len(), 2);
    let latest = state.store.latest();
    assert_eq!(latest.len(), 2);
}

#[tokio::test]
async fn ingest_rejects_empty_host() {
    let state = build_dev_state();
    let bad = json!({ "host": "", "samples": [] });
    let (status, _) = send(&state, ingest_req(Some(DEFAULT_INGEST_TOKEN), bad)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
