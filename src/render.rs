//! Server-side rendering of the Vitals dashboard.
//!
//! Pure functions: a `&[HostView]` + the signed-in email in, an HTML `String` out. The CSS
//! is embedded (`include_str!`) so the slim image never misses an asset and the page is one
//! self-contained document. The brand lockup, tokens, app-bar, cards, status pills and
//! tables match the shared HOLDFAST enterprise design.

use std::collections::BTreeMap;

use crate::metrics::{self, SampleRow};

const APP_CSS: &str = include_str!("../static/app.css");

/// Everything the dashboard shows for one host.
#[derive(Clone, Debug, Default)]
pub struct HostView {
    pub host: String,
    /// metric name -> latest value (headline gauges + raw figures).
    pub gauges: BTreeMap<String, f64>,
    /// Recent cpu_pct series (oldest -> newest) for the sparkline.
    pub spark_cpu: Vec<f64>,
    /// Recent mem_pct series (oldest -> newest) for the sparkline.
    pub spark_mem: Vec<f64>,
    /// Epoch seconds of this host's most recent sample (freshness).
    pub last_ts: i64,
}

impl HostView {
    fn g(&self, metric: &str) -> Option<f64> {
        self.gauges.get(metric).copied()
    }
}

/// Build per-host views from the store's `latest()` rows + a recent window of samples.
///
/// `latest` carries one row per `(host, metric)`; `window` carries the recent cpu_pct /
/// mem_pct series used for sparklines (ordered by `(host, metric, ts)`).
pub fn build_host_views(latest: &[SampleRow], window: &[SampleRow]) -> Vec<HostView> {
    let mut by_host: BTreeMap<String, HostView> = BTreeMap::new();
    for r in latest {
        let hv = by_host.entry(r.host.clone()).or_insert_with(|| HostView {
            host: r.host.clone(),
            ..Default::default()
        });
        hv.gauges.insert(r.metric.clone(), r.value);
        hv.last_ts = hv.last_ts.max(r.ts);
    }
    for r in window {
        // Skip hosts that have no latest row (shouldn't happen, but stay defensive).
        let Some(hv) = by_host.get_mut(&r.host) else {
            continue;
        };
        match r.metric.as_str() {
            metrics::M_CPU_PCT => hv.spark_cpu.push(r.value),
            metrics::M_MEM_PCT => hv.spark_mem.push(r.value),
            _ => {}
        }
    }
    by_host.into_values().collect()
}

/// Render the whole dashboard document.
pub fn render(hosts: &[HostView], email: &str, now: i64) -> String {
    let cards: String = if hosts.is_empty() {
        empty_state()
    } else {
        hosts.iter().map(|h| host_card(h, now)).collect()
    };
    let online = hosts.iter().filter(|h| now - h.last_ts <= 60).count();

    format!(
        r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="light">
<title>Vitals · HOLDFAST</title>
<style>{css}</style>
</head>
<body>
<header class="topbar">
  <div class="topbar__inner">
    <a class="brand" href="/" aria-label="HOLDFAST Vitals">
      <span class="brand__glyph" aria-hidden="true">{shield}</span>
      <span class="brand__word">HOLDFAST</span>
      <span class="brand__sep">/</span>
      <span class="brand__app">Vitals</span>
    </a>
    <div class="topbar__right">
      <span class="user-email" title="signed in">{email}</span>
      <a class="btn btn-ghost btn-sm" href="/_gw/auth/logout">Logout</a>
    </div>
  </div>
</header>
<main class="wrap">
  <div class="page-head">
    <div>
      <h1>主机探针 · Host Vitals</h1>
      <p class="muted">实时 CPU / 内存 / 磁盘 / 负载，每台主机采样上报。</p>
    </div>
    <div class="summary">
      <span class="pill pill--ok">{online} 在线</span>
      <span class="pill pill--muted">{total} 主机</span>
    </div>
  </div>
  <div class="grid">{cards}</div>
</main>
</body>
</html>"#,
        css = APP_CSS,
        shield = SHIELD_SVG,
        email = esc(email),
        online = online,
        total = hosts.len(),
        cards = cards,
    )
}

/// One host card: freshness pill, four headline gauges, two sparklines, a raw-figures table.
fn host_card(h: &HostView, now: i64) -> String {
    let age = now - h.last_ts;
    let (pill_class, pill_text) = if age <= 60 {
        ("pill--ok", "在线".to_string())
    } else if age <= 600 {
        ("pill--warn", format!("{} 前", human_age(age)))
    } else {
        ("pill--down", format!("{} 前", human_age(age)))
    };

    let cpu = h.g(metrics::M_CPU_PCT);
    let mem = h.g(metrics::M_MEM_PCT);
    let disk = h.g(metrics::M_DISK_PCT);
    let load = h.g(metrics::M_LOAD1);

    let gauges = format!(
        "{}{}{}{}",
        gauge("CPU", cpu, pct_fmt(cpu), pct_tone(cpu)),
        gauge("内存 MEM", mem, pct_fmt(mem), pct_tone(mem)),
        gauge("磁盘 DISK", disk, pct_fmt(disk), pct_tone(disk)),
        gauge("负载 LOAD", load.map(|v| (v * 10.0).min(100.0)), load_fmt(load), load_tone(load)),
    );

    let mem_detail = match (h.g(metrics::M_MEM_USED), h.g(metrics::M_MEM_TOTAL)) {
        (Some(u), Some(t)) => format!("{} / {}", human_bytes(u), human_bytes(t)),
        _ => "—".to_string(),
    };
    let disk_detail = match (h.g(metrics::M_DISK_USED), h.g(metrics::M_DISK_TOTAL)) {
        (Some(u), Some(t)) => format!("{} / {}", human_bytes(u), human_bytes(t)),
        _ => "—".to_string(),
    };
    let load_detail = match (h.g(metrics::M_LOAD1), h.g(metrics::M_LOAD5), h.g(metrics::M_LOAD15)) {
        (Some(a), Some(b), Some(c)) => format!("{a:.2} · {b:.2} · {c:.2}"),
        _ => "—".to_string(),
    };
    let net_detail = match (h.g(metrics::M_NET_RX), h.g(metrics::M_NET_TX)) {
        (Some(rx), Some(tx)) => format!("↓ {}/s · ↑ {}/s", human_bytes(rx), human_bytes(tx)),
        _ => "—".to_string(),
    };
    let uptime_detail = h
        .g(metrics::M_UPTIME)
        .map(|s| human_uptime(s as i64))
        .unwrap_or_else(|| "—".to_string());

    format!(
        r#"<section class="card">
  <div class="card__head">
    <h2 title="{host_attr}">{host}</h2>
    <span class="pill {pill_class}">{pill_text}</span>
  </div>
  <div class="gauges">{gauges}</div>
  <div class="sparks">
    <div class="spark">
      <div class="spark__label">CPU %</div>
      {spark_cpu}
    </div>
    <div class="spark">
      <div class="spark__label">MEM %</div>
      {spark_mem}
    </div>
  </div>
  <dl class="kv">
    <div class="kv__row"><dt>内存</dt><dd>{mem_detail}</dd></div>
    <div class="kv__row"><dt>磁盘</dt><dd>{disk_detail}</dd></div>
    <div class="kv__row"><dt>负载 1/5/15</dt><dd>{load_detail}</dd></div>
    <div class="kv__row"><dt>网络</dt><dd>{net_detail}</dd></div>
    <div class="kv__row"><dt>运行时间</dt><dd>{uptime_detail}</dd></div>
  </dl>
</section>"#,
        host_attr = esc(&h.host),
        host = esc(&h.host),
        pill_class = pill_class,
        pill_text = esc(&pill_text),
        gauges = gauges,
        spark_cpu = sparkline(&h.spark_cpu, 100.0, "var(--accent)"),
        spark_mem = sparkline(&h.spark_mem, 100.0, "var(--success)"),
        mem_detail = esc(&mem_detail),
        disk_detail = esc(&disk_detail),
        load_detail = esc(&load_detail),
        net_detail = esc(&net_detail),
        uptime_detail = esc(&uptime_detail),
    )
}

/// A single headline gauge: a labelled value with a tone-coloured progress bar.
fn gauge(label: &str, fill_pct: Option<f64>, value_text: String, tone: &str) -> String {
    let pct = fill_pct.unwrap_or(0.0).clamp(0.0, 100.0);
    format!(
        r#"<div class="gauge">
  <div class="gauge__top"><span class="gauge__label">{label}</span><span class="gauge__val">{val}</span></div>
  <div class="gauge__bar"><span class="gauge__fill {tone}" style="width:{pct:.1}%"></span></div>
</div>"#,
        label = esc(label),
        val = esc(&value_text),
        tone = tone,
        pct = pct,
    )
}

/// Inline SVG sparkline. `max` scales the y-axis; `stroke` is a CSS colour. Empty series
/// render a flat baseline so the layout never jumps.
fn sparkline(values: &[f64], max: f64, stroke: &str) -> String {
    const W: f64 = 240.0;
    const H: f64 = 44.0;
    if values.len() < 2 {
        return format!(
            r#"<svg class="spark__svg" viewBox="0 0 {W} {H}" preserveAspectRatio="none" role="img" aria-label="no data">
  <line x1="0" y1="{mid}" x2="{W}" y2="{mid}" stroke="var(--border)" stroke-width="1"/>
</svg>"#,
            W = W,
            H = H,
            mid = H / 2.0,
        );
    }
    let n = values.len();
    let max = if max <= 0.0 { 1.0 } else { max };
    let pts: String = values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let x = i as f64 / (n - 1) as f64 * W;
            let y = H - (v.clamp(0.0, max) / max) * H;
            format!("{x:.1},{y:.1}")
        })
        .collect::<Vec<_>>()
        .join(" ");
    // Area fill polygon: the line points plus the bottom corners.
    let area = format!("0,{H} {pts} {W},{H}", H = H, pts = pts, W = W);
    format!(
        r#"<svg class="spark__svg" viewBox="0 0 {W} {H}" preserveAspectRatio="none" role="img" aria-label="trend">
  <polygon points="{area}" fill="{stroke}" fill-opacity="0.10"/>
  <polyline points="{pts}" fill="none" stroke="{stroke}" stroke-width="2" stroke-linejoin="round" stroke-linecap="round"/>
</svg>"#,
        W = W,
        H = H,
        area = area,
        pts = pts,
        stroke = stroke,
    )
}

fn empty_state() -> String {
    r#"<section class="card card--empty">
  <h2>暂无数据</h2>
  <p class="muted">尚未收到任何探针上报。确认 vitals-agent 正在运行并指向本服务的 /ingest。</p>
</section>"#
        .to_string()
}

// --- formatting helpers ----------------------------------------------------------------

fn pct_fmt(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.1}%")).unwrap_or_else(|| "—".to_string())
}

fn load_fmt(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.2}")).unwrap_or_else(|| "—".to_string())
}

/// Tone class for a percentage gauge: green < 70 < amber < 90 < red.
fn pct_tone(v: Option<f64>) -> &'static str {
    match v {
        Some(x) if x >= 90.0 => "is-danger",
        Some(x) if x >= 70.0 => "is-warn",
        Some(_) => "is-ok",
        None => "is-muted",
    }
}

/// Tone for a 1-min load average (unitless; heuristic thresholds).
fn load_tone(v: Option<f64>) -> &'static str {
    match v {
        Some(x) if x >= 8.0 => "is-danger",
        Some(x) if x >= 4.0 => "is-warn",
        Some(_) => "is-ok",
        None => "is-muted",
    }
}

/// Human-readable byte size (binary units).
pub fn human_bytes(bytes: f64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = bytes.max(0.0);
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{v:.0} {}", UNITS[i])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// Human-readable uptime / freshness from seconds.
pub fn human_uptime(secs: i64) -> String {
    let s = secs.max(0);
    let d = s / 86400;
    let h = (s % 86400) / 3600;
    let m = (s % 3600) / 60;
    if d > 0 {
        format!("{d}d {h}h {m}m")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

fn human_age(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h", s / 3600)
    }
}

/// HTML-escape untrusted text (host ids, the signed-in email).
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// HOLDFAST shield glyph (indigo gradient), shared with the Keystone/console app-bar.
const SHIELD_SVG: &str = r##"<svg viewBox="0 0 48 48" fill="none" xmlns="http://www.w3.org/2000/svg">
<defs><linearGradient id="hf-shield-v" x1="8" y1="4" x2="40" y2="44" gradientUnits="userSpaceOnUse">
<stop stop-color="#818CF8"/><stop offset="1" stop-color="#4F46E5"/></linearGradient></defs>
<path d="M24 4 8 9.5V22c0 11 7 17.4 16 21.5C33 39.4 40 33 40 22V9.5L24 4Z" fill="url(#hf-shield-v)"/>
<rect x="20" y="19" width="8" height="13" rx="1" fill="#fff" fill-opacity="0.92"/>
<path d="M20 19v-2.5a4 4 0 0 1 8 0V19" stroke="#fff" stroke-width="2" stroke-opacity="0.92" fill="none"/>
</svg>"##;
