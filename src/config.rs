//! Configuration, env-driven with working dev defaults.
//!
//! Two roles share this module: [`ServerConfig`] (the TSDB + dashboard) and
//! [`AgentConfig`] (the host probe). Each value keeps its dev default when the env var is
//! unset/empty, so the in-memory dev server boots with NO configuration and NO database —
//! the same discipline as keystone/keyward.

/// Default server listen address (all interfaces, port 8300).
pub const DEFAULT_BIND_ADDR: &str = "0.0.0.0:8300";
/// Dev/test default ingest bearer token. Production MUST override `INGEST_TOKEN`.
pub const DEFAULT_INGEST_TOKEN: &str = "vitals-dev-ingest-token-change-me";
/// Default retention window in hours (7 days).
pub const DEFAULT_RETENTION_HOURS: u64 = 168;

/// Default anomaly z-score threshold: `|z| >= this` over the rolling window records an anomaly.
pub const DEFAULT_Z: f64 = 3.0;
/// Default rolling window length (samples per series for the self-baseline + forecast).
pub const DEFAULT_WINDOW: usize = 60;
/// Default short-term forecast horizon in steps (dashboard sparkline projection).
pub const DEFAULT_FORECAST_STEPS: usize = 6;
/// Default detector cadence in seconds (one self-baseline pass per minute).
pub const DEFAULT_DETECT_SECS: u64 = 60;
/// Hard cap on how many anomaly rows the dashboard / API renders.
pub const ANOMALY_LIMIT: i64 = 50;

/// Default scrape interval in seconds.
pub const DEFAULT_SCRAPE_INTERVAL: u64 = 10;
/// Default proc root.
pub const DEFAULT_HOST_PROC: &str = "/proc";
/// Default sys root.
pub const DEFAULT_HOST_SYS: &str = "/sys";
/// Default filesystem root probed for disk usage.
pub const DEFAULT_HOST_ROOT: &str = "/";
/// Default server URL the agent POSTs batches to.
pub const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:8300";

/// TSDB + dashboard server configuration.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Listen address (`BIND_ADDR`).
    pub bind_addr: String,
    /// Bearer token required on `POST /ingest` (`INGEST_TOKEN`).
    pub ingest_token: String,
    /// Prune samples older than this many hours (`RETENTION_HOURS`).
    pub retention_hours: u64,
    /// Run the background anomaly detector (`VITALS_DETECT`, default on).
    pub detect_enabled: bool,
    /// Detector cadence in seconds (`VITALS_DETECT_SECS`).
    pub detect_secs: u64,
    /// Anomaly z-score threshold (`VITALS_Z`).
    pub z_threshold: f64,
    /// Rolling window length in samples (`VITALS_WINDOW`).
    pub window: usize,
    /// Forecast horizon in steps (`VITALS_FORECAST_STEPS`).
    pub forecast_steps: usize,
    /// Optional Klaxon base URL (`KLAXON_URL`, e.g. `http://klaxon:9050`). `None` => no notify.
    pub klaxon_url: Option<String>,
    /// Optional Klaxon ingest bearer token (`KLAXON_INGEST_TOKEN`). `None` => no notify.
    pub klaxon_token: Option<String>,
    /// Optional recipient for anomaly notifications (`KLAXON_NOTIFY_EMAIL`). `None` => no notify.
    pub klaxon_email: Option<String>,
}

impl ServerConfig {
    /// Dev defaults (in-memory, no database, dev ingest token).
    pub fn dev() -> Self {
        ServerConfig {
            bind_addr: DEFAULT_BIND_ADDR.to_string(),
            ingest_token: DEFAULT_INGEST_TOKEN.to_string(),
            retention_hours: DEFAULT_RETENTION_HOURS,
            detect_enabled: true,
            detect_secs: DEFAULT_DETECT_SECS,
            z_threshold: DEFAULT_Z,
            window: DEFAULT_WINDOW,
            forecast_steps: DEFAULT_FORECAST_STEPS,
            klaxon_url: None,
            klaxon_token: None,
            klaxon_email: None,
        }
    }

    /// Dev defaults overridden by the environment.
    pub fn from_env() -> Self {
        let mut c = ServerConfig::dev();
        if let Some(v) = env_nonempty("BIND_ADDR") {
            c.bind_addr = v;
        }
        if let Some(v) = env_nonempty("INGEST_TOKEN") {
            c.ingest_token = v;
        }
        if let Some(v) = env_nonempty("RETENTION_HOURS").and_then(|v| v.parse().ok()) {
            c.retention_hours = v;
        }
        // The detector defaults ON; set VITALS_DETECT=off|false|0 to disable it.
        if let Ok(v) = std::env::var("VITALS_DETECT") {
            c.detect_enabled = !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "off" | "false" | "0" | "no"
            );
        }
        if let Some(v) = env_nonempty("VITALS_DETECT_SECS").and_then(|v| v.parse::<u64>().ok()) {
            c.detect_secs = if v == 0 { DEFAULT_DETECT_SECS } else { v };
        }
        if let Some(v) = env_nonempty("VITALS_Z").and_then(|v| v.parse::<f64>().ok()) {
            // A non-positive / non-finite threshold would flag everything; clamp to the default.
            c.z_threshold = if v.is_finite() && v > 0.0 { v } else { DEFAULT_Z };
        }
        if let Some(v) = env_nonempty("VITALS_WINDOW").and_then(|v| v.parse::<usize>().ok()) {
            // Need at least a handful of points for meaningful statistics.
            c.window = v.max(8);
        }
        if let Some(v) = env_nonempty("VITALS_FORECAST_STEPS").and_then(|v| v.parse::<usize>().ok()) {
            c.forecast_steps = v.clamp(1, 64);
        }
        c.klaxon_url = env_nonempty("KLAXON_URL");
        c.klaxon_token = env_nonempty("KLAXON_INGEST_TOKEN");
        c.klaxon_email = env_nonempty("KLAXON_NOTIFY_EMAIL");
        c
    }

    /// Retention window in seconds (at least one hour).
    pub fn retention_secs(&self) -> i64 {
        (self.retention_hours.max(1) as i64) * 3600
    }

    /// True when all three Klaxon settings are present, so a notify can be attempted.
    pub fn klaxon_ready(&self) -> bool {
        self.klaxon_url.is_some() && self.klaxon_token.is_some() && self.klaxon_email.is_some()
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self::dev()
    }
}

/// Host-probe agent configuration.
#[derive(Clone, Debug)]
pub struct AgentConfig {
    /// Host identity stamped on every sample (`HOST_ID`, else the hostname).
    pub host_id: String,
    /// Scrape cadence in seconds (`SCRAPE_INTERVAL`).
    pub scrape_interval: u64,
    /// proc root (`HOST_PROC`).
    pub host_proc: String,
    /// sys root (`HOST_SYS`).
    pub host_sys: String,
    /// Filesystem root probed for disk usage (`HOST_ROOT`).
    pub host_root: String,
    /// Server base URL batches are POSTed to (`SERVER_URL`).
    pub server_url: String,
    /// Bearer token presented on `/ingest` (`INGEST_TOKEN`).
    pub ingest_token: String,
}

impl AgentConfig {
    /// Build from the environment, resolving the host id (see [`resolve_host_id`]).
    pub fn from_env() -> Self {
        let host_proc = env_nonempty("HOST_PROC").unwrap_or_else(|| DEFAULT_HOST_PROC.to_string());
        AgentConfig {
            host_id: resolve_host_id(&host_proc),
            scrape_interval: env_nonempty("SCRAPE_INTERVAL")
                .and_then(|v| v.parse().ok())
                .filter(|n| *n > 0)
                .unwrap_or(DEFAULT_SCRAPE_INTERVAL),
            host_proc,
            host_sys: env_nonempty("HOST_SYS").unwrap_or_else(|| DEFAULT_HOST_SYS.to_string()),
            host_root: env_nonempty("HOST_ROOT").unwrap_or_else(|| DEFAULT_HOST_ROOT.to_string()),
            server_url: env_nonempty("SERVER_URL").unwrap_or_else(|| DEFAULT_SERVER_URL.to_string()),
            ingest_token: env_nonempty("INGEST_TOKEN")
                .unwrap_or_else(|| DEFAULT_INGEST_TOKEN.to_string()),
        }
    }

    /// Full ingest endpoint URL (`{server_url}/ingest`, with any trailing slash trimmed).
    pub fn ingest_url(&self) -> String {
        format!("{}/ingest", self.server_url.trim_end_matches('/'))
    }
}

/// Resolve the host id: `HOST_ID` env, else the `HOSTNAME` env (set by Docker), else
/// `{HOST_PROC}/sys/kernel/hostname` (the real host when the agent mounts host /proc),
/// else `"unknown"`.
pub fn resolve_host_id(host_proc: &str) -> String {
    if let Some(v) = env_nonempty("HOST_ID") {
        return v;
    }
    if let Some(v) = env_nonempty("HOSTNAME") {
        return v;
    }
    let path = format!("{}/sys/kernel/hostname", host_proc.trim_end_matches('/'));
    if let Ok(s) = std::fs::read_to_string(&path) {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    "unknown".to_string()
}

/// Read an env var, returning `None` when unset OR empty (empty never clobbers a default).
fn env_nonempty(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}
