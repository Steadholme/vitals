//! Probe parsing: turn checked-in `/proc` fixtures into numbers + a full `collect` batch.
//! No real `/proc` access for the pure parsers; `collect` runs against a temp proc root
//! built from the fixtures (disk uses the real filesystem root, which always exists).

use vitals::config::AgentConfig;
use vitals::metrics::{self, M_CPU_PCT, M_DISK_PCT, M_LOAD1, M_MEM_PCT, M_MEM_TOTAL, M_UPTIME};
use vitals::probe::{
    self, collect, cpu_percent, parse_loadavg, parse_meminfo, parse_net_dev, parse_stat,
    parse_uptime, Counters, CpuTimes,
};

const STAT1: &str = include_str!("fixtures/stat1");
const STAT2: &str = include_str!("fixtures/stat2");
const MEMINFO: &str = include_str!("fixtures/meminfo");
const LOADAVG: &str = include_str!("fixtures/loadavg");
const UPTIME: &str = include_str!("fixtures/uptime");
const NET_DEV: &str = include_str!("fixtures/net_dev");

#[test]
fn parse_stat_sums_total_and_idle() {
    let t1 = parse_stat(STAT1).expect("stat1 parses");
    // total = 1000+50+300+8000+100+0+20 = 9470 ; idle = 8000 + 100(iowait) = 8100
    assert_eq!(t1, CpuTimes { idle: 8100, total: 9470 });
    let t2 = parse_stat(STAT2).expect("stat2 parses");
    assert_eq!(t2, CpuTimes { idle: 8810, total: 10324 });
}

#[test]
fn cpu_percent_from_two_snapshots() {
    let p = parse_stat(STAT1).unwrap();
    let c = parse_stat(STAT2).unwrap();
    // dtotal=854, didle=710, busy=144 -> 16.86%
    let pct = cpu_percent(p, c);
    assert!((pct - 16.86).abs() < 0.05, "cpu pct was {pct}");
    // No elapsed jiffies -> 0, never NaN.
    assert_eq!(cpu_percent(c, c), 0.0);
}

#[test]
fn meminfo_used_percentage() {
    let (total, avail) = parse_meminfo(MEMINFO).expect("meminfo parses");
    assert_eq!(total, 16_384_000 * 1024);
    assert_eq!(avail, 8_192_000 * 1024);
    let used_pct = (total - avail) as f64 / total as f64 * 100.0;
    assert!((used_pct - 50.0).abs() < 1e-9, "used pct {used_pct}");
}

#[test]
fn loadavg_three_values() {
    assert_eq!(parse_loadavg(LOADAVG), Some((0.52, 0.48, 0.40)));
}

#[test]
fn uptime_first_float() {
    assert_eq!(parse_uptime(UPTIME), Some(123456.78));
}

#[test]
fn net_dev_sums_non_loopback() {
    // lo excluded; eth0 rx 5_000_000 + eth1 rx 500_000 ; tx 3_000_000 + 200_000.
    assert_eq!(parse_net_dev(NET_DEV), (5_500_000, 3_200_000));
}

#[test]
fn collect_against_temp_proc_root_emits_full_batch() {
    // Build a temp proc root from the fixtures: {proc}/stat, /meminfo, /loadavg, /uptime,
    // /net/dev. Disk probes the real "/" so statvfs succeeds.
    let dir = std::env::temp_dir().join(format!(
        "vitals-proc-{}-{}",
        std::process::id(),
        now_nanos()
    ));
    std::fs::create_dir_all(dir.join("net")).unwrap();
    std::fs::write(dir.join("stat"), STAT2).unwrap();
    std::fs::write(dir.join("meminfo"), MEMINFO).unwrap();
    std::fs::write(dir.join("loadavg"), LOADAVG).unwrap();
    std::fs::write(dir.join("uptime"), UPTIME).unwrap();
    std::fs::write(dir.join("net/dev"), NET_DEV).unwrap();

    let cfg = AgentConfig {
        host_id: "fixturehost".to_string(),
        scrape_interval: 10,
        host_proc: dir.to_string_lossy().to_string(),
        host_sys: "/sys".to_string(),
        host_root: "/".to_string(),
        server_url: "http://127.0.0.1:8300".to_string(),
        ingest_token: "t".to_string(),
    };

    // Previous snapshot 10s earlier with stat1's counters so CPU% has a real delta.
    let t1 = parse_stat(STAT1).unwrap();
    let prev = Counters {
        cpu_idle: t1.idle,
        cpu_total: t1.total,
        net_rx: 5_000_000,
        net_tx: 3_000_000,
        ts: 1000,
    };
    let (_counters, samples) = collect(&cfg, Some(&prev), 1010);
    let _ = std::fs::remove_dir_all(&dir);

    let by = |m: &str| samples.iter().find(|s| s.metric == m).map(|s| s.value);

    // CPU% present and matches the two-snapshot math.
    let cpu = by(M_CPU_PCT).expect("cpu_pct emitted");
    assert!((cpu - 16.86).abs() < 0.05, "cpu {cpu}");
    // Mem 50% used; total carried in bytes.
    assert!((by(M_MEM_PCT).unwrap() - 50.0).abs() < 1e-6);
    assert_eq!(by(M_MEM_TOTAL).unwrap(), (16_384_000u64 * 1024) as f64);
    // Load + uptime + disk gauges present.
    assert_eq!(by(M_LOAD1), Some(0.52));
    assert_eq!(by(M_UPTIME), Some(123456.78));
    let disk = by(M_DISK_PCT).expect("disk_pct emitted (statvfs on /)");
    assert!((0.0..=100.0).contains(&disk), "disk pct {disk}");

    // Net rate over a 10s delta: (5_500_000-5_000_000)/10 = 50_000 B/s.
    assert_eq!(by(metrics::M_NET_RX), Some(50_000.0));
}

#[test]
fn first_scrape_skips_rate_metrics() {
    let dir = std::env::temp_dir().join(format!("vitals-proc1-{}-{}", std::process::id(), now_nanos()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("stat"), STAT1).unwrap();
    std::fs::write(dir.join("meminfo"), MEMINFO).unwrap();
    let cfg = AgentConfig {
        host_id: "h".to_string(),
        scrape_interval: 10,
        host_proc: dir.to_string_lossy().to_string(),
        host_sys: "/sys".to_string(),
        host_root: "/".to_string(),
        server_url: "http://x".to_string(),
        ingest_token: "t".to_string(),
    };
    let (counters, samples) = collect(&cfg, None, 1000);
    let _ = std::fs::remove_dir_all(&dir);
    // No previous snapshot -> no cpu_pct / net rates, but mem gauge still present.
    assert!(samples.iter().all(|s| s.metric != M_CPU_PCT));
    assert!(samples.iter().any(|s| s.metric == M_MEM_PCT));
    // Counters captured for the next cycle.
    assert_eq!(counters.cpu_total, parse_stat(STAT1).unwrap().total);
    let _ = probe::read_counters(&cfg, 1000); // smoke: re-read path
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}
