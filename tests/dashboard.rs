//! The dashboard renders: app-bar brand + signed-in email (from X-Auth-Email), per-host
//! gauges, and the empty state. Driven through the real Router in-process.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use tower::ServiceExt;
use vitals::config::DEFAULT_INGEST_TOKEN;
use vitals::metrics::Sample;
use vitals::{app, build_dev_state, now_secs, AppState};

async fn get_html(state: &AppState, with_email: Option<&str>) -> (StatusCode, String) {
    let mut b = Request::builder().method("GET").uri("/");
    if let Some(e) = with_email {
        b = b.header("x-auth-email", e);
    }
    let resp = app(state.clone()).oneshot(b.body(Body::empty()).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn empty_dashboard_renders_brand_and_empty_state() {
    let state = build_dev_state();
    let (status, html) = get_html(&state, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("HOLDFAST"), "brand wordmark present");
    assert!(html.contains("Vitals"), "app name present");
    assert!(html.contains("/_gw/auth/logout"), "logout link to gateway");
    assert!(html.contains("暂无数据"), "empty state shown");
}

#[tokio::test]
async fn dashboard_shows_host_gauges_and_injected_email() {
    let state = build_dev_state();
    let now = now_secs();
    // Seed one fresh host across the headline metrics + a short cpu series.
    state.store.insert_samples(
        "edge-1",
        &[
            Sample::new("cpu_pct", 73.2, now - 20),
            Sample::new("cpu_pct", 81.0, now - 10),
            Sample::new("cpu_pct", 64.5, now),
            Sample::new("mem_pct", 55.0, now),
            Sample::new("disk_pct", 42.0, now),
            Sample::new("load1", 1.25, now),
            Sample::new("mem_used_bytes", 8_000_000_000.0, now),
            Sample::new("mem_total_bytes", 16_000_000_000.0, now),
            Sample::new("uptime_secs", 90_061.0, now),
        ],
    );

    let (status, html) = get_html(&state, Some("ops@holdfast.local")).await;
    assert_eq!(status, StatusCode::OK);
    // Injected identity shows in the app-bar.
    assert!(html.contains("ops@holdfast.local"), "signed-in email rendered");
    // Host + its latest gauge values are present.
    assert!(html.contains("edge-1"), "host name rendered");
    assert!(html.contains("64.5%"), "latest cpu value rendered");
    assert!(html.contains("55.0%"), "mem value rendered");
    // Sparkline SVG emitted (>=2 cpu points).
    assert!(html.contains("<polyline"), "cpu sparkline drawn");
    // Human-readable memory figure.
    assert!(html.contains("GiB"), "memory figure humanized");
    // The empty state is gone.
    assert!(!html.contains("暂无数据"));
}

#[tokio::test]
async fn email_html_is_escaped() {
    let state = build_dev_state();
    let (_, html) = get_html(&state, Some("<script>x</script>@e")).await;
    assert!(!html.contains("<script>x</script>@e"));
    assert!(html.contains("&lt;script&gt;"));
}

#[tokio::test]
async fn ingest_then_dashboard_reflects_it() {
    let state = build_dev_state();
    let now = now_secs();
    let body = serde_json::json!({
        "host": "via-ingest",
        "samples": [ { "metric": "cpu_pct", "value": 33.0, "ts": now } ]
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

    let (_, html) = get_html(&state, None).await;
    assert!(html.contains("via-ingest"));
    assert!(html.contains("33.0%"));
}
