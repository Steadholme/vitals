//! Vitals — host probe (探针) + metrics TSDB + enterprise dashboard for the HOLDFAST stack.
//!
//! Library root: defines [`AppState`], wires the routes via [`app`], and provides
//! [`build_dev_state`] (in-memory store, no database) and [`build_state_from_env`]
//! (env-selected store). Integration tests consume [`app`] directly via `tower::oneshot`,
//! exactly like keystone/keyward. The probe agent ([`probe`], [`config::AgentConfig`]) is a
//! separate binary that POSTs to the server defined here.
//!
//! Server endpoints:
//! - `GET  /healthz`        liveness (public; container HEALTHCHECK)
//! - `POST /ingest`         agent scrape batches (bearer `INGEST_TOKEN`)
//! - `GET  /api/metrics`    JSON time-series (behind the gateway `auth=sso` route)
//! - `GET  /`               the dashboard       (behind the gateway `auth=sso` route)

pub mod auth;
pub mod config;
pub mod error;
pub mod handlers;
pub mod metrics;
pub mod probe;
pub mod render;
pub mod store;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::routing::{get, post};
use axum::Router;

use crate::config::ServerConfig;
use crate::store::{InMemoryStore, PgStore, Store};

/// Shared application state. Cheap to clone (everything behind `Arc`).
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<ServerConfig>,
    pub store: Arc<dyn Store>,
}

/// Build the router wiring all server endpoints onto `state`.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(handlers::health::healthz))
        .route("/ingest", post(handlers::ingest::ingest))
        .route("/api/metrics", get(handlers::api::metrics))
        .route("/", get(handlers::dashboard::dashboard))
        // Sluice forwards the gateway prefix UNMODIFIED (no strip), so a request to the
        // `/vitals` route arrives here as `GET /vitals`. Register the dashboard as the
        // fallback (mirrors watchtower) so the page renders behind the gateway prefix.
        .fallback(get(handlers::dashboard::dashboard))
        .with_state(state)
}

/// Construct dev state: dev [`ServerConfig`] + an empty [`InMemoryStore`]. Used by the
/// integration tests and by `main`'s memory mode, so they need no database.
pub fn build_dev_state() -> AppState {
    AppState {
        config: Arc::new(ServerConfig::dev()),
        store: Arc::new(InMemoryStore::new()),
    }
}

/// Build runtime state from the environment.
///
/// [`ServerConfig`] comes from [`ServerConfig::from_env`]. The store is selected by
/// `VITALS_STORE`:
/// - `memory` (default): empty [`InMemoryStore`] — no database required.
/// - `postgres`: connect `DATABASE_URL`, run the idempotent migration, wire [`PgStore`].
///
/// Returns an error string on misconfiguration so `main` can fail loudly.
pub async fn build_state_from_env() -> Result<AppState, String> {
    let config = ServerConfig::from_env();

    let store_kind = std::env::var("VITALS_STORE").unwrap_or_else(|_| "memory".to_string());
    let store: Arc<dyn Store> = match store_kind.as_str() {
        "postgres" => {
            let database_url = std::env::var("DATABASE_URL")
                .map_err(|_| "VITALS_STORE=postgres requires DATABASE_URL".to_string())?;
            tracing::info!("VITALS_STORE=postgres — connecting to database");
            let pg = PgStore::connect(&database_url)
                .await
                .map_err(|e| format!("connect postgres: {e}"))?;
            pg.migrate()
                .await
                .map_err(|e| format!("run migration: {e}"))?;
            tracing::info!("postgres store ready (migrated)");
            Arc::new(pg)
        }
        "memory" => Arc::new(InMemoryStore::new()),
        other => return Err(format!("unknown VITALS_STORE={other} (use memory|postgres)")),
    };

    Ok(AppState {
        config: Arc::new(config),
        store,
    })
}

/// Current wall-clock time in epoch seconds.
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_secs() as i64
}
