//! Server-side rendering of the Vitals dashboard.
//!
//! Pure functions: a `&[HostView]` + the signed-in email in, an HTML `String` out. The CSS
//! is embedded (`include_str!`) so the slim image never misses an asset and the page is one
//! self-contained document. The brand lockup, tokens, app-bar, cards, status pills and
//! tables match the shared Steadholme enterprise design.

use std::collections::BTreeMap;

use crate::analytics;
use crate::config::ServerConfig;
use crate::metrics::{self, SampleRow};
use crate::store::Anomaly;

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
    /// Short-term linear projection of cpu_pct beyond the window (dashed sparkline tail).
    pub forecast_cpu: Vec<f64>,
    /// Short-term linear projection of mem_pct beyond the window.
    pub forecast_mem: Vec<f64>,
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
pub fn build_host_views(
    latest: &[SampleRow],
    window: &[SampleRow],
    forecast_steps: usize,
) -> Vec<HostView> {
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
    // Short-term linear forecast over each host's recent cpu/mem series (Augur's projection,
    // folded in). Skipped when the window is too thin to fit a line.
    if forecast_steps > 0 {
        for hv in by_host.values_mut() {
            if hv.spark_cpu.len() >= 2 {
                hv.forecast_cpu = analytics::forecast(&hv.spark_cpu, forecast_steps).points;
            }
            if hv.spark_mem.len() >= 2 {
                hv.forecast_mem = analytics::forecast(&hv.spark_mem, forecast_steps).points;
            }
        }
    }
    by_host.into_values().collect()
}

/// Render the whole dashboard document.
pub fn render(
    hosts: &[HostView],
    anomalies: &[Anomaly],
    config: &ServerConfig,
    email: &str,
    now: i64,
) -> String {
    let cards: String = if hosts.is_empty() {
        empty_state()
    } else {
        hosts.iter().map(|h| host_card(h, now)).collect()
    };
    let capability_panel = capability_panel(config);
    let anomaly_panel = anomalies_panel(anomalies, config, now);
    let online = hosts.iter().filter(|h| now - h.last_ts <= 60).count();

    format!(
        r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="light">
<title>Vitals · Steadholme</title>
<style>{css}</style>
</head>
<body>
<header class="topbar">
  <div class="topbar__inner">
    <a class="brand" href="/" aria-label="Steadholme Vitals">
      <span class="brand__glyph" aria-hidden="true">{shield}</span>
      <span class="brand__word">STEADHOLME</span>
    </a>
    <div class="topbar__right">{userbox}</div>
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
  {capability_panel}
  {anomaly_panel}
  <div class="grid">{cards}</div>
</main>
</body>
</html>"#,
        css = APP_CSS,
        shield = SHIELD_SVG,
        userbox = userbox(email),
        online = online,
        total = hosts.len(),
        capability_panel = capability_panel,
        anomaly_panel = anomaly_panel,
        cards = cards,
    )
}

/// Always-visible inventory of the capabilities that otherwise sit behind APIs, background tasks,
/// env-driven integrations, or small visual affordances like dashed forecast tails.
fn capability_panel(config: &ServerConfig) -> String {
    let detect_class = if config.detect_enabled {
        "pill--ok"
    } else {
        "pill--muted"
    };
    let detect_text = if config.detect_enabled {
        "检测开启"
    } else {
        "检测关闭"
    };
    let klaxon_text = if config.klaxon_ready() {
        "已连接"
    } else {
        "未配置"
    };
    let klaxon_class = if config.klaxon_ready() {
        "pill--ok"
    } else {
        "pill--muted"
    };
    let metrics = [
        metrics::M_CPU_PCT,
        metrics::M_MEM_PCT,
        metrics::M_MEM_USED,
        metrics::M_MEM_TOTAL,
        metrics::M_DISK_PCT,
        metrics::M_DISK_USED,
        metrics::M_DISK_TOTAL,
        metrics::M_LOAD1,
        metrics::M_LOAD5,
        metrics::M_LOAD15,
        metrics::M_NET_RX,
        metrics::M_NET_TX,
        metrics::M_UPTIME,
    ]
    .iter()
    .map(|m| format!(r#"<span class="tag mono">{}</span>"#, esc(m)))
    .collect::<String>();

    format!(
        r#"<section class="card capability-card">
  <div class="card__head">
    <h2>运行能力 · Runtime Surface</h2>
    <span class="pill {detect_class}">{detect_text}</span>
  </div>
  <div class="card__body">
    <div class="capability-grid">
      <div class="capability-item"><span class="capability-item__label">采集入口</span><span class="capability-item__value mono">POST /ingest</span><span class="muted">Bearer INGEST_TOKEN</span></div>
      <div class="capability-item"><span class="capability-item__label">JSON API</span><span class="capability-item__value"><a href="/api/metrics">/api/metrics</a> · <a href="/api/anomalies">/api/anomalies</a></span><span class="muted">支持 host / metric 过滤</span></div>
      <div class="capability-item"><span class="capability-item__label">异常检测</span><span class="capability-item__value">z ≥ {z}</span><span class="muted">{detect_secs}s cadence · {window} samples</span></div>
      <div class="capability-item"><span class="capability-item__label">预测</span><span class="capability-item__value">{forecast_steps} steps</span><span class="muted">CPU / MEM 虚线延伸</span></div>
      <div class="capability-item"><span class="capability-item__label">保留期</span><span class="capability-item__value">{retention_hours}h</span><span class="muted">后台定时裁剪</span></div>
      <div class="capability-item"><span class="capability-item__label">Klaxon 通知</span><span class="pill {klaxon_class}">{klaxon_text}</span><span class="muted">异常时 best-effort 通知</span></div>
    </div>
    <div class="metric-strip" aria-label="采集指标词表">{metrics}</div>
  </div>
</section>"#,
        detect_class = detect_class,
        detect_text = detect_text,
        z = esc(&format!("{:.2}", config.z_threshold)),
        detect_secs = config.detect_secs,
        window = config.window,
        forecast_steps = config.forecast_steps,
        retention_hours = config.retention_hours,
        klaxon_class = klaxon_class,
        klaxon_text = klaxon_text,
        metrics = metrics,
    )
}

/// The estate anomaly watch: recent self-baseline anomalies the background detector recorded
/// (host/metric, when, z-score, value, note). Folded in from the retired Augur service.
fn anomalies_panel(anomalies: &[Anomaly], config: &ServerConfig, now: i64) -> String {
    if anomalies.is_empty() {
        let (pill_class, pill_text, body) = if config.detect_enabled {
            (
                "pill--ok",
                "0 异常",
                format!(
                    "当前没有超过 z ≥ {} 的自基线异常。检测每 {}s 运行一次，窗口 {} 个样本。",
                    esc(&format!("{:.2}", config.z_threshold)),
                    config.detect_secs,
                    config.window,
                ),
            )
        } else {
            (
                "pill--muted",
                "检测关闭",
                "VITALS_DETECT 当前关闭；历史异常仍可通过 /api/anomalies 查询。".to_string(),
            )
        };
        return format!(
            r#"<section class="card anomaly-card">
  <div class="card__head">
    <h2>异常监测 · Anomaly Watch</h2>
    <span class="pill {pill_class}">{pill_text}</span>
  </div>
  <div class="card__body">
    <p class="muted">{body}</p>
  </div>
</section>"#,
            pill_class = pill_class,
            pill_text = pill_text,
            body = body,
        );
    }
    let rows: String = anomalies
        .iter()
        .map(|a| {
            format!(
                r#"<tr>
  <td><span class="mono">{host}</span> · {metric}</td>
  <td><span class="tag tag-down">z = {z}</span></td>
  <td>{value}</td>
  <td class="muted">{when}</td>
  <td class="muted">{note}</td>
</tr>"#,
                host = esc(&a.host),
                metric = esc(&a.metric),
                z = esc(&format!("{:.2}", a.score)),
                value = esc(&format!("{:.3}", a.value)),
                when = esc(&format!("{} 前", human_age(now - a.ts))),
                note = esc(&a.note),
            )
        })
        .collect();
    format!(
        r#"<section class="card anomaly-card">
  <div class="card__head">
    <h2>异常监测 · Anomaly Watch</h2>
    <span class="pill pill--warn">{n} 异常</span>
  </div>
  <div class="card__body">
    <table>
      <thead><tr><th>指标 Series</th><th>z-score</th><th>当前值</th><th>时间</th><th>说明</th></tr></thead>
      <tbody>{rows}</tbody>
    </table>
  </div>
</section>"#,
        n = anomalies.len(),
        rows = rows,
    )
}

/// Cross-subdomain SSO logout (terminated at the gateway / Keystone IdP).
const LOGOUT_URL: &str = "/_gw/auth/logout";

/// The right side of the app-bar, shared with every Steadholme service: a page title, an
/// "All apps" pill back to the apex portal, the signed-in user chip (avatar initial + email),
/// and the cross-subdomain logout. `email` is the gateway-injected identity; the unknown
/// placeholder (`—`) or an empty string renders no user chip (public-page friendly).
fn userbox(email: &str) -> String {
    let has_identity = !email.is_empty() && email != "—";
    let chip = if has_identity {
        let initial = email
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "H".to_string());
        format!(
            "<span class=\"userchip\"><span class=\"userchip__avatar\" aria-hidden=\"true\">{}</span><span class=\"user-email\" title=\"signed in\">{}</span></span>",
            esc(&initial),
            esc(email),
        )
    } else {
        String::new()
    };
    format!(
        concat!(
            "<span class=\"topbar__title\">Vitals</span>",
            "<a class=\"allapps\" href=\"https://w33d.xyz\" title=\"All apps\">",
            "<svg viewBox=\"0 0 24 24\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"2\" stroke-linecap=\"round\" stroke-linejoin=\"round\" aria-hidden=\"true\">",
            "<rect x=\"3\" y=\"3\" width=\"7\" height=\"7\" rx=\"1.5\"/><rect x=\"14\" y=\"3\" width=\"7\" height=\"7\" rx=\"1.5\"/>",
            "<rect x=\"3\" y=\"14\" width=\"7\" height=\"7\" rx=\"1.5\"/><rect x=\"14\" y=\"14\" width=\"7\" height=\"7\" rx=\"1.5\"/></svg>All apps</a>",
            "{chip}",
            "<a class=\"btn btn-ghost btn-sm\" href=\"{logout}\">Logout</a>",
        ),
        chip = chip,
        logout = LOGOUT_URL,
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
        gauge(
            "负载 LOAD",
            load.map(|v| (v * 10.0).min(100.0)),
            load_fmt(load),
            load_tone(load)
        ),
    );

    let mem_detail = match (h.g(metrics::M_MEM_USED), h.g(metrics::M_MEM_TOTAL)) {
        (Some(u), Some(t)) => format!("{} / {}", human_bytes(u), human_bytes(t)),
        _ => "—".to_string(),
    };
    let disk_detail = match (h.g(metrics::M_DISK_USED), h.g(metrics::M_DISK_TOTAL)) {
        (Some(u), Some(t)) => format!("{} / {}", human_bytes(u), human_bytes(t)),
        _ => "—".to_string(),
    };
    let load_detail = match (
        h.g(metrics::M_LOAD1),
        h.g(metrics::M_LOAD5),
        h.g(metrics::M_LOAD15),
    ) {
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
        spark_cpu = sparkline(&h.spark_cpu, &h.forecast_cpu, 100.0, "var(--accent)"),
        spark_mem = sparkline(&h.spark_mem, &h.forecast_mem, 100.0, "var(--success)"),
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

/// Inline SVG sparkline. `max` scales the y-axis; `stroke` is a CSS colour. `forecast` is the
/// short-term linear projection drawn as a dashed continuation beyond the history (empty = none).
/// Empty `values` render a flat baseline so the layout never jumps.
fn sparkline(values: &[f64], forecast: &[f64], max: f64, stroke: &str) -> String {
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
    // x-axis spans history + forecast so the projection has room on the right.
    let total = n + forecast.len();
    let span = (total - 1).max(1) as f64;
    let x_at = |i: usize| i as f64 / span * W;
    let y_at = |v: f64| H - (v.clamp(0.0, max) / max) * H;

    let pts: String = values
        .iter()
        .enumerate()
        .map(|(i, v)| format!("{:.1},{:.1}", x_at(i), y_at(*v)))
        .collect::<Vec<_>>()
        .join(" ");
    // Area fill polygon: the history line points plus the bottom corners (history span only).
    let hist_right = x_at(n - 1);
    let area = format!("0,{H} {pts} {hx:.1},{H}", H = H, pts = pts, hx = hist_right);

    // Dashed forecast continuation: anchor at the last real point, then the projected points.
    let forecast_line = if forecast.is_empty() {
        String::new()
    } else {
        let mut fp: Vec<String> = Vec::with_capacity(forecast.len() + 1);
        fp.push(format!("{:.1},{:.1}", x_at(n - 1), y_at(values[n - 1])));
        for (k, v) in forecast.iter().enumerate() {
            fp.push(format!("{:.1},{:.1}", x_at(n + k), y_at(*v)));
        }
        format!(
            r#"
  <polyline points="{fpts}" fill="none" stroke="{stroke}" stroke-width="1.5" stroke-dasharray="3 3" stroke-opacity="0.7" stroke-linejoin="round" stroke-linecap="round"/>"#,
            fpts = fp.join(" "),
            stroke = stroke,
        )
    };

    format!(
        r#"<svg class="spark__svg" viewBox="0 0 {W} {H}" preserveAspectRatio="none" role="img" aria-label="trend">
  <polygon points="{area}" fill="{stroke}" fill-opacity="0.10"/>
  <polyline points="{pts}" fill="none" stroke="{stroke}" stroke-width="2" stroke-linejoin="round" stroke-linecap="round"/>{forecast_line}
</svg>"#,
        W = W,
        H = H,
        area = area,
        pts = pts,
        stroke = stroke,
        forecast_line = forecast_line,
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
    v.map(|x| format!("{x:.1}%"))
        .unwrap_or_else(|| "—".to_string())
}

fn load_fmt(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.2}"))
        .unwrap_or_else(|| "—".to_string())
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

/// Steadholme shield glyph (indigo gradient), shared with the Keystone/console app-bar.
const SHIELD_SVG: &str = r##"<svg viewBox="0 0 48 48" fill="none" xmlns="http://www.w3.org/2000/svg">
<defs><linearGradient id="hf-shield-v" x1="8" y1="4" x2="40" y2="44" gradientUnits="userSpaceOnUse">
<stop stop-color="#818CF8"/><stop offset="1" stop-color="#4F46E5"/></linearGradient></defs>
<path d="M24 4 8 9.5V22c0 11 7 17.4 16 21.5C33 39.4 40 33 40 22V9.5L24 4Z" fill="url(#hf-shield-v)"/>
<rect x="20" y="19" width="8" height="13" rx="1" fill="#fff" fill-opacity="0.92"/>
<path d="M20 19v-2.5a4 4 0 0 1 8 0V19" stroke="#fff" stroke-width="2" stroke-opacity="0.92" fill="none"/>
</svg>"##;
