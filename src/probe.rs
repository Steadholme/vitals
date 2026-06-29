//! Host probe (探针): read raw counters from a configurable proc/sys root and turn two
//! consecutive reads into a batch of [`Sample`]s.
//!
//! The parsing functions are pure (`&str` in, numbers out) so they're unit-tested against
//! checked-in `/proc` fixtures with no real `/proc` access. [`collect`] does the file I/O
//! plus the rate/percentage math; it holds the previous [`Counters`] so CPU% and network
//! rates are deltas over the scrape interval.

use crate::config::AgentConfig;
use crate::metrics::{self, Sample};

/// Cumulative counters captured each scrape; deltas between two of these yield rates.
#[derive(Clone, Debug, Default)]
pub struct Counters {
    /// `/proc/stat` aggregate idle jiffies (idle + iowait).
    pub cpu_idle: u64,
    /// `/proc/stat` aggregate total jiffies (sum of all fields).
    pub cpu_total: u64,
    /// `/proc/net/dev` total received bytes over non-loopback interfaces.
    pub net_rx: u64,
    /// `/proc/net/dev` total transmitted bytes over non-loopback interfaces.
    pub net_tx: u64,
    /// Epoch seconds this snapshot was taken.
    pub ts: i64,
}

/// Parsed CPU aggregate line of `/proc/stat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuTimes {
    pub idle: u64,
    pub total: u64,
}

/// Parse the aggregate `cpu` line of `/proc/stat`.
///
/// `cpu  user nice system idle iowait irq softirq steal guest guest_nice`
/// total = sum of all present fields; idle = idle + iowait (fields 4 and 5).
pub fn parse_stat(content: &str) -> Option<CpuTimes> {
    let line = content.lines().find(|l| {
        let mut it = l.split_whitespace();
        it.next() == Some("cpu")
    })?;
    let nums: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .map(|t| t.parse::<u64>().unwrap_or(0))
        .collect();
    if nums.len() < 4 {
        return None;
    }
    let total: u64 = nums.iter().sum();
    let idle = nums[3] + nums.get(4).copied().unwrap_or(0);
    Some(CpuTimes { idle, total })
}

/// CPU busy percent (0..=100) from two `/proc/stat` snapshots. Returns 0 when the total
/// did not advance (no elapsed jiffies / counter reset).
pub fn cpu_percent(prev: CpuTimes, cur: CpuTimes) -> f64 {
    let dtotal = cur.total.saturating_sub(prev.total);
    if dtotal == 0 {
        return 0.0;
    }
    let didle = cur.idle.saturating_sub(prev.idle);
    let busy = dtotal.saturating_sub(didle) as f64;
    (busy / dtotal as f64 * 100.0).clamp(0.0, 100.0)
}

/// Parse `/proc/meminfo` into `(total_bytes, available_bytes)`. Values are listed in kB.
pub fn parse_meminfo(content: &str) -> Option<(u64, u64)> {
    let mut total_kb = None;
    let mut avail_kb = None;
    for line in content.lines() {
        let mut it = line.split_whitespace();
        match it.next() {
            Some("MemTotal:") => total_kb = it.next().and_then(|v| v.parse::<u64>().ok()),
            Some("MemAvailable:") => avail_kb = it.next().and_then(|v| v.parse::<u64>().ok()),
            _ => {}
        }
    }
    Some((total_kb? * 1024, avail_kb? * 1024))
}

/// Parse `/proc/loadavg` into `(load1, load5, load15)`.
pub fn parse_loadavg(content: &str) -> Option<(f64, f64, f64)> {
    let mut it = content.split_whitespace();
    let l1 = it.next()?.parse().ok()?;
    let l5 = it.next()?.parse().ok()?;
    let l15 = it.next()?.parse().ok()?;
    Some((l1, l5, l15))
}

/// Parse `/proc/uptime` into uptime seconds (first float).
pub fn parse_uptime(content: &str) -> Option<f64> {
    content.split_whitespace().next()?.parse().ok()
}

/// Parse `/proc/net/dev` into `(rx_bytes_total, tx_bytes_total)` summed over all
/// non-loopback interfaces. Columns after the `iface:` label are
/// `rx_bytes rx_packets ... (8 rx cols) tx_bytes ...`; rx_bytes is col 0, tx_bytes is col 8.
pub fn parse_net_dev(content: &str) -> (u64, u64) {
    let mut rx = 0u64;
    let mut tx = 0u64;
    for line in content.lines() {
        let Some((iface, rest)) = line.split_once(':') else {
            continue; // header rows have no colon
        };
        let iface = iface.trim();
        if iface == "lo" || iface.is_empty() {
            continue;
        }
        let cols: Vec<u64> = rest
            .split_whitespace()
            .map(|t| t.parse::<u64>().unwrap_or(0))
            .collect();
        if cols.len() >= 9 {
            rx += cols[0];
            tx += cols[8];
        }
    }
    (rx, tx)
}

/// Per-second rate between two cumulative counter reads `dt` seconds apart. Guards against
/// a zero/negative interval and counter resets (returns 0).
fn rate(prev: u64, cur: u64, dt: i64) -> f64 {
    if dt <= 0 || cur < prev {
        return 0.0;
    }
    (cur - prev) as f64 / dt as f64
}

/// `statvfs(path)` -> `(total_bytes, used_bytes)`. Uses `f_frsize` as the block size,
/// `f_blocks` for capacity, and `f_bfree` for free space (used = total - free).
pub fn disk_usage(path: &str) -> Option<(u64, u64)> {
    let c_path = std::ffi::CString::new(path).ok()?;
    // SAFETY: `statvfs` only writes into `buf`; `c_path` is a valid NUL-terminated string
    // that outlives the call. A non-zero return means the path is unreadable -> None.
    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut buf) };
    if rc != 0 {
        return None;
    }
    let frsize = buf.f_frsize as u64;
    let total = buf.f_blocks as u64 * frsize;
    let free = buf.f_bfree as u64 * frsize;
    let used = total.saturating_sub(free);
    Some((total, used))
}

/// Read all host counters once. Errors on individual files are tolerated (that metric is
/// simply omitted); a fully unreadable proc root yields an empty counter snapshot.
pub fn read_counters(cfg: &AgentConfig, ts: i64) -> Counters {
    let proc = cfg.host_proc.trim_end_matches('/');
    let (cpu_idle, cpu_total) = std::fs::read_to_string(format!("{proc}/stat"))
        .ok()
        .and_then(|c| parse_stat(&c))
        .map(|t| (t.idle, t.total))
        .unwrap_or((0, 0));
    let (net_rx, net_tx) = std::fs::read_to_string(format!("{proc}/net/dev"))
        .ok()
        .map(|c| parse_net_dev(&c))
        .unwrap_or((0, 0));
    Counters {
        cpu_idle,
        cpu_total,
        net_rx,
        net_tx,
        ts,
    }
}

/// Read the current host state and emit a [`Sample`] batch stamped `ts`.
///
/// `prev` is the previous [`Counters`] (None on the first scrape): when present, CPU% and
/// network rates are computed as deltas; on the first scrape those rate metrics are skipped
/// (only absolute gauges — mem/disk/load/uptime — are emitted). Returns the fresh
/// [`Counters`] to feed the next call.
pub fn collect(cfg: &AgentConfig, prev: Option<&Counters>, ts: i64) -> (Counters, Vec<Sample>) {
    let proc = cfg.host_proc.trim_end_matches('/');
    let cur = read_counters(cfg, ts);
    let mut out = Vec::new();

    // CPU% + net rates need a previous snapshot.
    if let Some(p) = prev {
        let dt = (cur.ts - p.ts).max(0);
        out.push(Sample::new(
            metrics::M_CPU_PCT,
            cpu_percent(
                CpuTimes {
                    idle: p.cpu_idle,
                    total: p.cpu_total,
                },
                CpuTimes {
                    idle: cur.cpu_idle,
                    total: cur.cpu_total,
                },
            ),
            ts,
        ));
        out.push(Sample::new(metrics::M_NET_RX, rate(p.net_rx, cur.net_rx, dt), ts));
        out.push(Sample::new(metrics::M_NET_TX, rate(p.net_tx, cur.net_tx, dt), ts));
    }

    // Memory.
    if let Some((total, avail)) = std::fs::read_to_string(format!("{proc}/meminfo"))
        .ok()
        .and_then(|c| parse_meminfo(&c))
    {
        let used = total.saturating_sub(avail);
        let pct = if total > 0 {
            used as f64 / total as f64 * 100.0
        } else {
            0.0
        };
        out.push(Sample::new(metrics::M_MEM_PCT, pct, ts));
        out.push(Sample::new(metrics::M_MEM_USED, used as f64, ts));
        out.push(Sample::new(metrics::M_MEM_TOTAL, total as f64, ts));
    }

    // Load.
    if let Some((l1, l5, l15)) = std::fs::read_to_string(format!("{proc}/loadavg"))
        .ok()
        .and_then(|c| parse_loadavg(&c))
    {
        out.push(Sample::new(metrics::M_LOAD1, l1, ts));
        out.push(Sample::new(metrics::M_LOAD5, l5, ts));
        out.push(Sample::new(metrics::M_LOAD15, l15, ts));
    }

    // Disk (statvfs on HOST_ROOT).
    if let Some((total, used)) = disk_usage(&cfg.host_root) {
        let pct = if total > 0 {
            used as f64 / total as f64 * 100.0
        } else {
            0.0
        };
        out.push(Sample::new(metrics::M_DISK_PCT, pct, ts));
        out.push(Sample::new(metrics::M_DISK_USED, used as f64, ts));
        out.push(Sample::new(metrics::M_DISK_TOTAL, total as f64, ts));
    }

    // Uptime.
    if let Some(up) = std::fs::read_to_string(format!("{proc}/uptime"))
        .ok()
        .and_then(|c| parse_uptime(&c))
    {
        out.push(Sample::new(metrics::M_UPTIME, up, ts));
    }

    (cur, out)
}
