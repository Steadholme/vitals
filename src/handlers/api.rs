//! `GET /api/metrics` — JSON time-series for the dashboard / external readers.
//!
//! Query params (all optional):
//! - `host`   — restrict to one host;
//! - `metric` — restrict to one metric name;
//! - `since`  — inclusive epoch-seconds lower bound (default: now - 1h).
//!
//! Response: `{ "samples": [ { "host", "metric", "value", "ts" }, ... ] }`, ordered by
//! `(host, metric, ts)`.

use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::error::AppError;
use crate::{now_secs, AppState};

#[derive(Debug, Deserialize)]
pub struct MetricsQuery {
    pub host: Option<String>,
    pub metric: Option<String>,
    pub since: Option<i64>,
}

pub async fn metrics(
    State(state): State<AppState>,
    Query(q): Query<MetricsQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    // Default to the last hour so an unbounded scan never falls out of a bare request.
    let since = q.since.unwrap_or_else(|| now_secs() - 3600);
    let host = q.host.as_deref().filter(|s| !s.is_empty());
    let metric = q.metric.as_deref().filter(|s| !s.is_empty());
    let rows = state.store.query(host, metric, since).await;
    Ok(Json(json!({ "samples": rows })))
}

/// `GET /api/anomalies` — recent self-baseline anomalies recorded by the background detector.
///
/// Query params (all optional): `host`, `metric` (restrict to one series), `limit` (cap, default
/// [`ANOMALY_LIMIT`]). Response: `{ "z_threshold", "window", "detect_secs", "anomalies": [ ... ] }`
/// newest-first. Behind the gateway `auth=sso` route, like `/api/metrics`.
#[derive(Debug, Deserialize)]
pub struct AnomaliesQuery {
    pub host: Option<String>,
    pub metric: Option<String>,
    pub limit: Option<i64>,
}

pub async fn anomalies(
    State(state): State<AppState>,
    Query(q): Query<AnomaliesQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let host = q.host.as_deref().filter(|s| !s.is_empty());
    let metric = q.metric.as_deref().filter(|s| !s.is_empty());
    let limit = q
        .limit
        .filter(|n| *n > 0)
        .unwrap_or(crate::config::ANOMALY_LIMIT)
        .min(crate::config::ANOMALY_LIMIT);
    let rows = state.store.recent_anomalies(host, metric, limit).await;
    Ok(Json(json!({
        "z_threshold": state.config.z_threshold,
        "window": state.config.window,
        "detect_secs": state.config.detect_secs,
        "anomalies": rows,
    })))
}
