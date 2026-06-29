//! Metric vocabulary + wire/store data shapes.
//!
//! Metric names are a small, STABLE string vocabulary (one TEXT column in the TSDB), so
//! the agent, the ingest endpoint, the `/api/metrics` reader, and the dashboard all agree
//! without a schema migration. Values are `DOUBLE PRECISION` (percentages, bytes, seconds,
//! load averages all fit one column); `ts` is epoch seconds (`BIGINT`).

use serde::{Deserialize, Serialize};

/// CPU busy percentage over the scrape interval (0..=100), from `/proc/stat` deltas.
pub const M_CPU_PCT: &str = "cpu_pct";
/// Memory used percentage (0..=100): `(MemTotal - MemAvailable) / MemTotal * 100`.
pub const M_MEM_PCT: &str = "mem_pct";
/// Memory used in bytes.
pub const M_MEM_USED: &str = "mem_used_bytes";
/// Memory total in bytes.
pub const M_MEM_TOTAL: &str = "mem_total_bytes";
/// Root-filesystem used percentage (0..=100) via `statvfs(HOST_ROOT)`.
pub const M_DISK_PCT: &str = "disk_pct";
/// Root-filesystem used bytes.
pub const M_DISK_USED: &str = "disk_used_bytes";
/// Root-filesystem total bytes.
pub const M_DISK_TOTAL: &str = "disk_total_bytes";
/// 1-minute load average.
pub const M_LOAD1: &str = "load1";
/// 5-minute load average.
pub const M_LOAD5: &str = "load5";
/// 15-minute load average.
pub const M_LOAD15: &str = "load15";
/// Network receive rate, bytes/sec, summed over non-loopback interfaces (`/proc/net/dev`).
pub const M_NET_RX: &str = "net_rx_bps";
/// Network transmit rate, bytes/sec, summed over non-loopback interfaces.
pub const M_NET_TX: &str = "net_tx_bps";
/// System uptime in seconds (`/proc/uptime`).
pub const M_UPTIME: &str = "uptime_secs";

/// The metrics the dashboard renders as headline gauges (current value, per host).
pub const HEADLINE_METRICS: &[&str] = &[M_CPU_PCT, M_MEM_PCT, M_DISK_PCT, M_LOAD1];

/// One measurement on the wire (agent -> server) and in `/api/metrics` responses.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Sample {
    pub metric: String,
    pub value: f64,
    pub ts: i64,
}

impl Sample {
    pub fn new(metric: &str, value: f64, ts: i64) -> Self {
        Sample {
            metric: metric.to_string(),
            value,
            ts,
        }
    }
}

/// A scrape batch the agent POSTs to `/ingest`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IngestBatch {
    pub host: String,
    pub samples: Vec<Sample>,
}

/// A stored measurement (a TSDB row): a [`Sample`] tagged with its host.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct SampleRow {
    pub host: String,
    pub metric: String,
    pub value: f64,
    pub ts: i64,
}
