//! Vitals agent (探针) entry point: every `SCRAPE_INTERVAL`, read host metrics from the
//! configurable proc/sys root and POST a JSON batch to the server's `/ingest`.
//!
//! Deployed alongside the server with the host's `/proc`, `/sys` and `/` mounted read-only
//! (e.g. `HOST_PROC=/host/proc`) so it reports REAL host metrics from inside the container.
//!
//! The HTTP client is a tiny hand-rolled HTTP/1.1 POST over `std::net::TcpStream` (same
//! spirit as the server's healthcheck): the target is the INTERNAL plaintext server, so no
//! TLS / heavyweight client is pulled in.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use vitals::config::AgentConfig;
use vitals::metrics::IngestBatch;
use vitals::now_secs;
use vitals::probe::{self, Counters};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber::fmt::init();
    let cfg = AgentConfig::from_env();

    // `oneshot`: one scrape pair (so CPU% has a delta) + one POST, then exit. Handy for
    // smoke-testing the agent->server path without waiting on the interval loop.
    if std::env::args().nth(1).as_deref() == Some("oneshot") {
        std::process::exit(run_oneshot(&cfg));
    }

    tracing::info!(
        host = %cfg.host_id,
        interval = cfg.scrape_interval,
        server = %cfg.ingest_url(),
        proc = %cfg.host_proc,
        "Vitals agent starting (host probe)"
    );

    let mut prev: Option<Counters> = None;
    let mut tick = tokio::time::interval(Duration::from_secs(cfg.scrape_interval));
    loop {
        tick.tick().await;
        let ts = now_secs();
        let (counters, samples) = probe::collect(&cfg, prev.as_ref(), ts);
        prev = Some(counters);

        if samples.is_empty() {
            tracing::warn!("scrape produced no samples (check HOST_PROC mount)");
            continue;
        }
        let batch = IngestBatch {
            host: cfg.host_id.clone(),
            samples,
        };
        match post_batch(&cfg, &batch) {
            Ok(code) if (200..300).contains(&code) => {
                tracing::debug!(samples = batch.samples.len(), code, "batch ingested")
            }
            Ok(code) => tracing::warn!(code, "ingest rejected"),
            Err(e) => tracing::warn!(error = %e, "ingest POST failed"),
        }
    }
}

/// One scrape pair + POST. Returns a process exit code (0 on a 2xx ingest).
fn run_oneshot(cfg: &AgentConfig) -> i32 {
    let t0 = now_secs();
    let (prev, _) = probe::collect(cfg, None, t0);
    std::thread::sleep(Duration::from_secs(1));
    let (_, samples) = probe::collect(cfg, Some(&prev), now_secs());
    let batch = IngestBatch {
        host: cfg.host_id.clone(),
        samples,
    };
    println!(
        "oneshot: host={} samples={} -> {}",
        batch.host,
        batch.samples.len(),
        cfg.ingest_url()
    );
    match post_batch(cfg, &batch) {
        Ok(code) if (200..300).contains(&code) => {
            println!("oneshot: ingest OK ({code})");
            0
        }
        Ok(code) => {
            eprintln!("oneshot: ingest rejected ({code})");
            1
        }
        Err(e) => {
            eprintln!("oneshot: POST failed: {e}");
            1
        }
    }
}

/// POST a batch as JSON to `{SERVER_URL}/ingest` with the bearer token. Returns the HTTP
/// status code.
fn post_batch(cfg: &AgentConfig, batch: &IngestBatch) -> Result<u16, String> {
    let body = serde_json::to_vec(batch).map_err(|e| format!("serialize batch: {e}"))?;
    http_post_json(&cfg.ingest_url(), &cfg.ingest_token, &body)
}

/// Minimal HTTP/1.1 `POST` of a JSON body. Parses `http://host[:port]/path`, writes the
/// request, and returns the response status code. Plaintext only (internal hop).
fn http_post_json(url: &str, bearer: &str, body: &[u8]) -> Result<u16, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("SERVER_URL must be http://… (got {url})"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let host_port = if authority.contains(':') {
        authority.to_string()
    } else {
        format!("{authority}:80")
    };
    // Host header is the authority WITHOUT a default :80 (cosmetic, but correct).
    let host_header = authority;

    let mut stream = TcpStream::connect(&host_port).map_err(|e| format!("connect {host_port}: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;

    let req = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host_header}\r\n\
         Authorization: Bearer {bearer}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\r\n",
        path = path,
        host_header = host_header,
        bearer = bearer,
        len = body.len(),
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write headers: {e}"))?;
    stream.write_all(body).map_err(|e| format!("write body: {e}"))?;
    stream.flush().map_err(|e| e.to_string())?;

    let mut buf = Vec::new();
    stream
        .read_to_end(&mut buf)
        .map_err(|e| format!("read response: {e}"))?;
    let text = String::from_utf8_lossy(&buf);
    let status_line = text.lines().next().unwrap_or("");
    // "HTTP/1.1 200 OK" -> 200
    status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| format!("malformed status line: {status_line:?}"))
}
