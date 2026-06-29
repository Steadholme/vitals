//! `POST /ingest` — agent scrape batches.
//!
//! Bearer-guarded with `INGEST_TOKEN`. The body is an [`IngestBatch`]; each sample is
//! written to the TSDB under the batch's host. Returns `{ "accepted": <n> }`.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use serde_json::json;

use crate::auth::require_ingest;
use crate::error::AppError;
use crate::metrics::IngestBatch;
use crate::AppState;

pub async fn ingest(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<IngestBatch>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_ingest(&headers, &state.config.ingest_token)?;

    // Decode AFTER auth so an unauthenticated caller learns nothing about the body shape.
    let Json(batch) = body.map_err(|e| AppError::InvalidRequest(e.body_text()))?;

    if batch.host.trim().is_empty() {
        return Err(AppError::InvalidRequest("host must not be empty".to_string()));
    }
    let accepted = batch.samples.len();
    state.store.insert_samples(&batch.host, &batch.samples).await;
    Ok(Json(json!({ "accepted": accepted })))
}
