//! `GET /` — the server-rendered enterprise dashboard.
//!
//! Identity comes from the gateway: Sluice runs the OIDC login and injects `X-Auth-Email`
//! on the `auth=sso` route, so this page does NO login of its own — it just reads the
//! header for the app-bar. The page shows every host's current CPU/mem/disk/load plus
//! recent sparklines, built from the TSDB.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Html;

use crate::render::{build_host_views, render};
use crate::{now_secs, AppState};

/// Sparkline lookback window in seconds (recent trend shown under each gauge).
const SPARK_WINDOW_SECS: i64 = 30 * 60;

pub async fn dashboard(State(state): State<AppState>, headers: HeaderMap) -> Html<String> {
    // Trusted because the gateway strips inbound X-Auth-* before injecting its own.
    let email = headers
        .get("x-auth-email")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .unwrap_or("—");

    let now = now_secs();
    let latest = state.store.latest();
    let window = state.store.query(None, None, now - SPARK_WINDOW_SECS);
    let hosts = build_host_views(&latest, &window);

    Html(render(&hosts, email, now))
}
