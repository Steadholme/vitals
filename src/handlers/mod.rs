//! HTTP handlers. `health` is the unauthenticated liveness probe; `ingest` accepts agent
//! batches (bearer-guarded); `api` serves JSON time-series to the dashboard; `dashboard`
//! renders the server-side enterprise UI (gated by the gateway's `auth=sso` route).

pub mod api;
pub mod dashboard;
pub mod health;
pub mod ingest;
