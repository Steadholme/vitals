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
