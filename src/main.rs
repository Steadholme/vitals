//! Vitals server entry point: init state from env, start the retention pruner, serve.
//!
//! Also exposes a dependency-free `vitals-server healthcheck` subcommand used as the
//! container HEALTHCHECK: it GETs `http://127.0.0.1:$PORT/healthz` over a raw TCP socket
//! and exits 0 on `200`, 1 otherwise — so the image needs no curl.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

#[tokio::main]
async fn main() {
    // Container HEALTHCHECK path — handled before any server setup, exits the process.
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        std::process::exit(run_healthcheck());
    }

    tracing_subscriber::fmt::init();

    let state = match vitals::build_state_from_env().await {
        Ok(state) => state,
        Err(e) => {
            tracing::error!(error = %e, "failed to build application state");
            std::process::exit(1);
        }
    };

    let addr: SocketAddr = state
        .config
        .bind_addr
        .parse()
        .expect("invalid bind_addr in config");

    spawn_retention_pruner(state.clone());

    // Background self-baselining anomaly detector (folded in from the retired Augur service).
    // Detached + isolated from the ingest write path; gated by VITALS_DETECT (default on).
    if state.config.detect_enabled {
        vitals::detector::spawn_detector(state.clone());
    }

    let retention = state.config.retention_hours;
    let app = vitals::app(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {addr}: {e}"));

    tracing::info!(%addr, retention_hours = retention, "Vitals server listening (TSDB + dashboard)");
    axum::serve(listener, app).await.expect("server error");
}

/// Background timer: every hour, delete samples older than `RETENTION_HOURS`.
fn spawn_retention_pruner(state: vitals::AppState) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        // The first tick fires immediately; that's a fine startup prune.
        loop {
            tick.tick().await;
            let cutoff = vitals::now_secs() - state.config.retention_secs();
            // The Store is async: await the prune directly on the serving runtime — no
            // block_in_place, so the pruner never blocks a worker thread.
            let removed = state.store.prune(cutoff).await;
            if removed > 0 {
                tracing::info!(removed, cutoff, "retention prune");
            }
        }
    });
}

/// GET `/healthz` over a raw TCP socket. Returns process exit code (0 = healthy).
fn run_healthcheck() -> i32 {
    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8300".to_string());
    // Healthcheck always talks to the loopback regardless of the bind interface.
    let port = bind_addr.rsplit(':').next().unwrap_or("8300");
    let target = format!("127.0.0.1:{port}");

    match healthcheck_once(&target) {
        Ok(true) => 0,
        Ok(false) => {
            eprintln!("healthcheck: {target} did not return 200");
            1
        }
        Err(e) => {
            eprintln!("healthcheck: {target} error: {e}");
            1
        }
    }
}

fn healthcheck_once(target: &str) -> std::io::Result<bool> {
    let addr: SocketAddr = target
        .parse()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{e}")))?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(b"GET /healthz HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
    let mut buf = String::new();
    stream.read_to_string(&mut buf)?;
    let status_line = buf.lines().next().unwrap_or("");
    Ok(status_line.contains("200"))
}
