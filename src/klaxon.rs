//! Optional Klaxon notification on a flagged anomaly.
//!
//! Klaxon is the estate's notification hub. When `KLAXON_URL`, `KLAXON_INGEST_TOKEN`, and
//! `KLAXON_NOTIFY_EMAIL` are all configured, Vitals fires a single best-effort `POST /api/notify`
//! (own-bearer ingest, NOT the SSO gateway) addressed to the configured operator. The send is
//! fully non-blocking and degrades silently: it is spawned onto a detached task with a short
//! timeout, and ANY failure (Klaxon down, bad config, timeout) only logs — it NEVER blocks or
//! fails the detection pass. When the settings are absent, [`notify`] is a no-op. Folded in from
//! the retired Augur service.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::config::ServerConfig;
use crate::store::Anomaly;

/// Per-POST budget (connect + write + read). Klaxon is in-network; keep it short.
const POST_TIMEOUT: Duration = Duration::from_secs(2);

/// Fire a best-effort anomaly notification. Spawns a detached task and returns immediately; a
/// no-op when Klaxon is not fully configured.
pub fn notify(config: &ServerConfig, anomaly: &Anomaly) {
    if !config.klaxon_ready() {
        return;
    }
    // All three are Some by `klaxon_ready`.
    let (url, token, email) = (
        config.klaxon_url.clone().unwrap_or_default(),
        config.klaxon_token.clone().unwrap_or_default(),
        config.klaxon_email.clone().unwrap_or_default(),
    );
    let Some(target) = Target::parse(&url) else {
        tracing::warn!(url = %url, "invalid KLAXON_URL — notify skipped");
        return;
    };

    let series = format!("{}/{}", anomaly.host, anomaly.metric);
    let title = format!("Anomaly: {series}");
    let body = format!(
        "{series} drifted from its baseline (z={:.2}, value={:.3}). {}",
        anomaly.score, anomaly.value, anomaly.note
    );
    let payload = serde_json::json!({
        "user_email": email,
        "source": "vitals",
        "title": title,
        "body": body,
        "url": "https://vitals.w33d.xyz/",
    })
    .to_string();

    tokio::spawn(async move {
        match tokio::time::timeout(POST_TIMEOUT, post(&target, &token, &payload)).await {
            Ok(Ok(status)) if (200..300).contains(&status) => {
                tracing::debug!(status, "klaxon notify delivered")
            }
            Ok(Ok(status)) => tracing::warn!(status, "klaxon rejected notify"),
            Ok(Err(e)) => tracing::warn!(error = %e, "klaxon notify POST failed"),
            Err(_) => tracing::warn!("klaxon notify timed out"),
        }
    });
}

/// Send one `POST <base>/api/notify`. Hand-rolled HTTP/1.1 over a raw TCP stream (Klaxon is the
/// internal plaintext `http://klaxon:9050`, so no TLS client is needed). Returns the status code.
async fn post(target: &Target, token: &str, body: &str) -> std::io::Result<u16> {
    let mut stream = TcpStream::connect((target.host.as_str(), target.port)).await?;
    let req = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {authority}\r\n\
         Authorization: Bearer {token}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\r\n{body}",
        path = target.path,
        authority = target.authority,
        len = body.len(),
    );
    stream.write_all(req.as_bytes()).await?;
    stream.flush().await?;
    let mut buf = Vec::with_capacity(256);
    stream.read_to_end(&mut buf).await?;
    parse_status(&buf)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "no HTTP status line"))
}

fn parse_status(buf: &[u8]) -> Option<u16> {
    let line_end = buf.iter().position(|&b| b == b'\n').unwrap_or(buf.len());
    let line = std::str::from_utf8(&buf[..line_end]).ok()?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// Parsed Klaxon target: connect host/port, the `Host` authority, and the `<base>/api/notify` path.
struct Target {
    host: String,
    port: u16,
    authority: String,
    path: String,
}

impl Target {
    /// Parse `http://host[:port][/base]` into a connect target with `/api/notify` appended. Only
    /// plain `http` is accepted (the internal hop is plaintext); anything else returns `None`.
    fn parse(url: &str) -> Option<Target> {
        let rest = url.strip_prefix("http://")?;
        let (authority, base_path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        if authority.is_empty() {
            return None;
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().ok()?),
            None => (authority.to_string(), 80u16),
        };
        if host.is_empty() {
            return None;
        }
        let base = base_path.trim_end_matches('/');
        Some(Target {
            host,
            port,
            authority: authority.to_string(),
            path: format!("{base}/api/notify"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_parse_appends_notify_path() {
        let t = Target::parse("http://klaxon:9050").unwrap();
        assert_eq!(t.host, "klaxon");
        assert_eq!(t.port, 9050);
        assert_eq!(t.path, "/api/notify");
        assert!(Target::parse("https://klaxon:9050").is_none());
    }

    #[test]
    fn notify_is_noop_without_config() {
        // dev() has no Klaxon settings -> must not panic, must not spawn.
        notify(
            &ServerConfig::dev(),
            &Anomaly::new("box", "cpu_pct", 1, 9.0, 5.0, "z=5".into()),
        );
    }
}
