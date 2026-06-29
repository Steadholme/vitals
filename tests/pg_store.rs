//! PostgreSQL `Store` integration test.
//!
//! Runs ONLY when `TEST_DATABASE_URL` is set (it needs an external Postgres). When unset
//! the test prints a note and returns early — it never fails the default `cargo test` run,
//! which stays database-free. Spin up a throwaway Postgres and run:
//!
//! ```text
//! TEST_DATABASE_URL=postgres://postgres:pw@127.0.0.1:55440/vitals \
//!   cargo test --test pg_store -- --nocapture
//! ```
//!
//! The `Store` trait is async (PgStore drives sqlx natively, no block_in_place); the
//! multi_thread flavor below just keeps reads parallel under load.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;
use vitals::config::DEFAULT_INGEST_TOKEN;
use vitals::metrics::Sample;
use vitals::store::PgStore;
use vitals::{app, build_dev_state};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pg_store_full_integration() {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!(
            "NOTE: TEST_DATABASE_URL not set — skipping Postgres integration test \
             (needs external Postgres). This is expected for the default test run."
        );
        return;
    };

    // --- connect / migrate (idempotent: run twice) -------------------------
    let pg = PgStore::connect(&url).await.expect("connect to TEST_DATABASE_URL");
    pg.migrate().await.expect("migrate");
    pg.migrate().await.expect("migrate is idempotent");

    // Wire the PG store behind Arc<dyn Store> in an otherwise-dev AppState.
    let mut state = build_dev_state();
    state.store = Arc::new(pg);

    // Clean any rows from a previous run for our test hosts so assertions are exact.
    let removed = state.store.prune(i64::MAX).await;
    eprintln!("pre-clean removed {removed} stale rows");

    // --- direct Store-trait round-trip (async sqlx, awaited natively) ------
    state.store.insert_samples(
        "pg-node",
        &[
            Sample::new("cpu_pct", 10.0, 1000),
            Sample::new("cpu_pct", 20.0, 2000),
            Sample::new("mem_pct", 55.0, 2000),
        ],
    ).await;
    // ON CONFLICT DO NOTHING — re-insert with same (host,metric,ts) is a no-op.
    state.store.insert_samples("pg-node", &[Sample::new("cpu_pct", 999.0, 1000)]).await;

    let cpu = state.store.query(Some("pg-node"), Some("cpu_pct"), 0).await;
    assert_eq!(cpu.len(), 2, "two cpu rows");
    assert_eq!(cpu[0].value, 10.0, "first write wins on conflict");

    let since = state.store.query(Some("pg-node"), Some("cpu_pct"), 2000).await;
    assert_eq!(since.len(), 1);
    assert_eq!(since[0].value, 20.0);

    let latest = state.store.latest().await;
    let pg_cpu = latest
        .iter()
        .find(|r| r.host == "pg-node" && r.metric == "cpu_pct")
        .expect("latest cpu present");
    assert_eq!(pg_cpu.value, 20.0);
    assert_eq!(pg_cpu.ts, 2000);

    // --- retention prune ---------------------------------------------------
    let pruned = state.store.prune(1500).await;
    assert_eq!(pruned, 1, "the ts=1000 cpu row is pruned");
    assert!(state.store.query(Some("pg-node"), Some("cpu_pct"), 0).await.len() == 1);

    // --- full HTTP flow through the PG-backed app --------------------------
    let body = json!({
        "host": "pg-http",
        "samples": [
            { "metric": "cpu_pct", "value": 42.0, "ts": 3000 },
            { "metric": "disk_pct", "value": 71.0, "ts": 3000 }
        ]
    });
    let req = Request::builder()
        .method("POST")
        .uri("/ingest")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {DEFAULT_INGEST_TOKEN}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app(state.clone()).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["accepted"], 2);

    // The dashboard reads it back from Postgres.
    let dash = app(state.clone())
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let html = String::from_utf8(
        axum::body::to_bytes(dash.into_body(), usize::MAX).await.unwrap().to_vec(),
    )
    .unwrap();
    assert!(html.contains("pg-http"), "PG-backed host on dashboard");

    // Final cleanup so reruns start clean.
    let _ = state.store.prune(i64::MAX).await;

    println!(
        "PG STORE INTEGRATION OK: migrate (idempotent) + insert/query/latest/prune \
         + full ingest/dashboard flow against real Postgres"
    );
}
