//! Background self-baselining anomaly detector (folded in from the retired Augur service).
//!
//! Augur ran as its own deployable that scraped Vitals `/api/metrics` over a raw TCP socket every
//! minute, re-ingested the rows into a SECOND TSDB, and scored them. That whole hop is gone: the
//! samples already live in this process's store, so the detector reads them DIRECTLY through the
//! `Store` trait — no scrape, no duplicate storage, no extra container.
//!
//! Every `VITALS_DETECT_SECS` the detector walks each `(host, metric)` series, scores its latest
//! sample against the SELF-baseline (mean + stddev) of the PRECEDING points in the window, and when
//! `|z| >= VITALS_Z` records an anomaly (deduped per `(host, metric, ts)`), emits the
//! `vitals.anomaly` Watchtower audit, and fires an optional Klaxon notify.
//!
//! RESILIENCE: the task is detached and isolated from the ingest write path — a panic or hot loop
//! here can never starve or crash metric ingestion/retention. The numerics are trivially cheap
//! (OLS over a short window), and the pass only logs on any store hiccup.

use std::time::Duration;

use crate::analytics;
use crate::audit::AuditEvent;
use crate::klaxon;
use crate::store::Anomaly;
use crate::AppState;

/// Run one full detection pass over every tracked `(host, metric)` series. Never returns an error —
/// the next tick simply tries again.
pub async fn run_once(state: &AppState) {
    // `latest()` yields exactly one row per (host, metric) pair — the set of series to score.
    let pairs = state.store.latest().await;
    let window = state.config.window as i64;
    let z_threshold = state.config.z_threshold;

    let mut flagged = 0usize;
    for row in &pairs {
        let samples = state
            .store
            .recent_samples(&row.host, &row.metric, window)
            .await;
        let values: Vec<f64> = samples.iter().map(|s| s.value).collect();

        let Some(score) = analytics::detect_latest(&values, z_threshold) else {
            continue;
        };
        let Some(last) = samples.last() else { continue };

        let note = format!("z={:.2} over window={}", score, values.len());
        let anomaly = Anomaly::new(&row.host, &row.metric, last.ts, last.value, score, note);

        // Dedup: only announce a newly-recorded anomaly (first detection of this latest sample).
        if !state.store.record_anomaly(&anomaly).await {
            continue;
        }
        flagged += 1;

        tracing::info!(host = %row.host, metric = %row.metric, z = score, value = last.value, "anomaly recorded");
        state.audit.emit(AuditEvent::warning(
            "vitals.anomaly",
            "vitals.detector",
            &format!("{}/{}", row.host, row.metric),
            &format!("z={:.2} value={:.3}", score, last.value),
        ));
        klaxon::notify(&state.config, &anomaly);
    }

    if flagged > 0 {
        tracing::debug!(flagged, pairs = pairs.len(), "anomaly detection pass");
    }
}

/// Spawn the background detector: score every series immediately, then every `config.detect_secs`.
/// The task is detached; it lives for the process lifetime and never panics the server.
pub fn spawn_detector(state: AppState) {
    tokio::spawn(async move {
        let interval = Duration::from_secs(state.config.detect_secs.max(1));
        tracing::info!(
            secs = interval.as_secs(),
            z = state.config.z_threshold,
            window = state.config.window,
            "anomaly detector started"
        );
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;
            run_once(&state).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::audit::AuditSink;
    use crate::config::ServerConfig;
    use crate::metrics::Sample;
    use crate::store::{InMemoryStore, Store};
    use crate::AppState;

    fn dev_state(store: Arc<dyn Store>) -> AppState {
        AppState {
            config: Arc::new(ServerConfig::dev()),
            store,
            audit: AuditSink::disabled(),
        }
    }

    #[tokio::test]
    async fn detects_and_records_a_spike_then_dedups() {
        let store = Arc::new(InMemoryStore::new());
        // A calm cpu_pct baseline on host "box", then a sharp spike as the latest sample.
        let calm = [10.0, 10.2, 9.8, 10.1, 9.9, 10.0, 10.3, 9.7, 10.0, 10.1];
        for (i, v) in calm.iter().enumerate() {
            store
                .insert_samples("box", &[Sample::new("cpu_pct", *v, (i as i64) * 60)])
                .await;
        }
        store
            .insert_samples("box", &[Sample::new("cpu_pct", 95.0, (calm.len() as i64) * 60)])
            .await;

        let state = dev_state(store.clone());
        super::run_once(&state).await;

        let found = store.recent_anomalies(Some("box"), Some("cpu_pct"), 10).await;
        assert_eq!(found.len(), 1, "the spike should be recorded once");
        assert!(found[0].score > 3.0);
        assert!((found[0].value - 95.0).abs() < 1e-9);

        // A second pass over the SAME latest sample must not double-record.
        super::run_once(&state).await;
        assert_eq!(
            store.recent_anomalies(Some("box"), Some("cpu_pct"), 10).await.len(),
            1
        );
    }

    #[tokio::test]
    async fn calm_series_is_not_flagged() {
        let store = Arc::new(InMemoryStore::new());
        let calm = [40.0, 41.0, 39.5, 40.2, 40.8, 39.9, 40.1, 40.3, 39.7, 40.0, 40.05];
        for (i, v) in calm.iter().enumerate() {
            store
                .insert_samples("box", &[Sample::new("mem_pct", *v, (i as i64) * 60)])
                .await;
        }
        let state = dev_state(store.clone());
        super::run_once(&state).await;
        assert!(store.recent_anomalies(None, None, 10).await.is_empty());
    }
}
