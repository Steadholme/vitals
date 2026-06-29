//! In-memory `Store` semantics: insert/query/latest round-trip, idempotent inserts, and
//! retention prune.

use vitals::metrics::Sample;
use vitals::store::{InMemoryStore, Store};

fn s(metric: &str, value: f64, ts: i64) -> Sample {
    Sample::new(metric, value, ts)
}

#[tokio::test]
async fn round_trip_query_and_latest() {
    let store = InMemoryStore::new();
    store.insert_samples(
        "h1",
        &[s("cpu_pct", 10.0, 100), s("cpu_pct", 20.0, 110), s("mem_pct", 40.0, 110)],
    ).await;
    store.insert_samples("h2", &[s("cpu_pct", 5.0, 105)]).await;

    // Filter by host + metric, since lower bound is inclusive.
    let cpu_h1 = store.query(Some("h1"), Some("cpu_pct"), 0).await;
    assert_eq!(cpu_h1.len(), 2);
    assert_eq!(cpu_h1[0].ts, 100);
    assert_eq!(cpu_h1[1].ts, 110);

    let since = store.query(Some("h1"), Some("cpu_pct"), 110).await;
    assert_eq!(since.len(), 1);
    assert_eq!(since[0].value, 20.0);

    // latest(): newest per (host, metric).
    let latest = store.latest().await;
    assert_eq!(latest.len(), 3); // (h1,cpu_pct),(h1,mem_pct),(h2,cpu_pct)
    let h1_cpu = latest
        .iter()
        .find(|r| r.host == "h1" && r.metric == "cpu_pct")
        .unwrap();
    assert_eq!(h1_cpu.value, 20.0);
    assert_eq!(h1_cpu.ts, 110);
}

#[tokio::test]
async fn insert_is_idempotent_per_host_metric_ts() {
    let store = InMemoryStore::new();
    store.insert_samples("h1", &[s("cpu_pct", 10.0, 100)]).await;
    // Same (host, metric, ts) — first write wins (mirrors ON CONFLICT DO NOTHING).
    store.insert_samples("h1", &[s("cpu_pct", 99.0, 100)]).await;
    let rows = store.query(Some("h1"), Some("cpu_pct"), 0).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].value, 10.0);
}

#[tokio::test]
async fn retention_prune_drops_old_samples() {
    let store = InMemoryStore::new();
    store.insert_samples(
        "h1",
        &[s("cpu_pct", 1.0, 1000), s("cpu_pct", 2.0, 2000), s("cpu_pct", 3.0, 3000)],
    ).await;
    // Drop everything with ts < 2500.
    let removed = store.prune(2500).await;
    assert_eq!(removed, 2);
    let rows = store.query(None, None, 0).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].ts, 3000);
    // Pruning again removes nothing.
    assert_eq!(store.prune(2500).await, 0);
}
