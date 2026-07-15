//! Metric TSDB storage.
//!
//! `Store` is a small trait with an in-memory and a PostgreSQL implementation, mirroring
//! keystone/keyward's seam: handlers depend only on the trait, so a FusionDB-backed store
//! can drop in later. The PostgreSQL layer uses ONLY portable standard SQL (TEXT/BIGINT/
//! DOUBLE PRECISION, composite PRIMARY KEY, a secondary INDEX, parameterized queries,
//! INSERT .. ON CONFLICT, GROUP BY/MAX latest-row join — NO `DISTINCT ON`, NO arrays, NO
//! JSONB) and runtime queries (no compile-time macros), so the build needs NO database and
//! the same statements later run unchanged on FusionDB over pgwire.

use std::collections::HashSet;
use std::sync::Mutex;

use async_trait::async_trait;
use serde::Serialize;

use crate::metrics::{Sample, SampleRow};

/// One recorded anomaly (maps 1:1 to an `anomalies` row). A `(host, metric)` series whose
/// latest sample drifts `|z| >= z_threshold` from the baseline of its preceding points is
/// recorded here once per `(host, metric, ts)` — the same self-baseline detection Augur ran,
/// now folded into Vitals so there is no second TSDB and no scrape hop.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Anomaly {
    pub host: String,
    pub metric: String,
    pub ts: i64,
    pub value: f64,
    /// Signed z-score of `value` against the preceding window's baseline.
    pub score: f64,
    pub note: String,
}

impl Anomaly {
    pub fn new(host: &str, metric: &str, ts: i64, value: f64, score: f64, note: String) -> Self {
        Anomaly {
            host: host.to_string(),
            metric: metric.to_string(),
            ts,
            value,
            score,
            note,
        }
    }
}

/// Pluggable metric TSDB. Methods are `async`: the axum handlers (and the retention pruner)
/// `.await` them directly on the serving runtime, so a query never blocks a worker thread.
/// The in-memory store's `std::sync::Mutex` is taken and dropped within a single synchronous
/// section (no `.await` inside), so the guard is never held across a yield point.
#[async_trait]
pub trait Store: Send + Sync {
    /// Append a host's scrape batch. Idempotent per `(host, metric, ts)` (first write wins).
    async fn insert_samples(&self, host: &str, samples: &[Sample]);

    /// Return rows matching the filters, ordered by `(host, metric, ts)`:
    /// - `host`/`metric` `None` means "any";
    /// - `since` is an inclusive lower bound on `ts`.
    async fn query(&self, host: Option<&str>, metric: Option<&str>, since: i64) -> Vec<SampleRow>;

    /// The most-recent sample for every `(host, metric)` pair (the dashboard's gauges).
    async fn latest(&self) -> Vec<SampleRow>;

    /// Delete samples with `ts < older_than`. Returns the number of rows removed.
    async fn prune(&self, older_than: i64) -> u64;

    /// The most-recent `limit` samples for one `(host, metric)` series, returned OLDEST-first
    /// (ascending `ts`) so the last element is the latest sample — the order the analytics expect.
    /// Drives the background anomaly detector and the dashboard forecast.
    async fn recent_samples(&self, host: &str, metric: &str, limit: i64) -> Vec<SampleRow>;

    /// Record one anomaly. Idempotent on `(host, metric, ts)` — returns `true` only when newly
    /// inserted, so repeated detection of the same latest sample emits exactly once.
    async fn record_anomaly(&self, anomaly: &Anomaly) -> bool;

    /// Recent anomalies, newest-first, capped at `limit`. `host`/`metric` `None` means "any".
    async fn recent_anomalies(
        &self,
        host: Option<&str>,
        metric: Option<&str>,
        limit: i64,
    ) -> Vec<Anomaly>;
}

/// In-memory `Store`. `std::sync::Mutex<Vec>` — no async lock needed. The default when
/// `VITALS_STORE` is unset; keeps the whole service database-free (used by tests too).
#[derive(Default)]
pub struct InMemoryStore {
    rows: Mutex<Vec<SampleRow>>,
    anomalies: Mutex<Vec<Anomaly>>,
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Store for InMemoryStore {
    async fn insert_samples(&self, host: &str, samples: &[Sample]) {
        // The whole critical section is synchronous (no `.await` inside), so the std `Mutex`
        // guard is never held across a yield point.
        let mut rows = self.rows.lock().expect("rows lock poisoned");
        // Mirror Postgres' `ON CONFLICT (host, metric, ts) DO NOTHING`: first write wins.
        let mut seen: HashSet<(&str, &str, i64)> = rows
            .iter()
            .map(|r| (r.host.as_str(), r.metric.as_str(), r.ts))
            .collect();
        // `seen` borrows `rows`; collect the new rows first, then extend.
        let mut to_add = Vec::new();
        for s in samples {
            let key = (host, s.metric.as_str(), s.ts);
            if seen.insert(key) {
                to_add.push(SampleRow {
                    host: host.to_string(),
                    metric: s.metric.clone(),
                    value: s.value,
                    ts: s.ts,
                });
            }
        }
        drop(seen);
        rows.extend(to_add);
    }

    async fn query(&self, host: Option<&str>, metric: Option<&str>, since: i64) -> Vec<SampleRow> {
        let rows = self.rows.lock().expect("rows lock poisoned");
        let mut out: Vec<SampleRow> = rows
            .iter()
            .filter(|r| host.is_none_or(|h| r.host == h))
            .filter(|r| metric.is_none_or(|m| r.metric == m))
            .filter(|r| r.ts >= since)
            .cloned()
            .collect();
        out.sort_by(|a, b| {
            a.host
                .cmp(&b.host)
                .then(a.metric.cmp(&b.metric))
                .then(a.ts.cmp(&b.ts))
        });
        out
    }

    async fn latest(&self) -> Vec<SampleRow> {
        use std::collections::HashMap;
        let rows = self.rows.lock().expect("rows lock poisoned");
        let mut best: HashMap<(String, String), SampleRow> = HashMap::new();
        for r in rows.iter() {
            let key = (r.host.clone(), r.metric.clone());
            match best.get(&key) {
                Some(cur) if cur.ts >= r.ts => {}
                _ => {
                    best.insert(key, r.clone());
                }
            }
        }
        let mut out: Vec<SampleRow> = best.into_values().collect();
        out.sort_by(|a, b| a.host.cmp(&b.host).then(a.metric.cmp(&b.metric)));
        out
    }

    async fn prune(&self, older_than: i64) -> u64 {
        let mut rows = self.rows.lock().expect("rows lock poisoned");
        let before = rows.len();
        rows.retain(|r| r.ts >= older_than);
        (before - rows.len()) as u64
    }

    async fn recent_samples(&self, host: &str, metric: &str, limit: i64) -> Vec<SampleRow> {
        let rows = self.rows.lock().expect("rows lock poisoned");
        let mut v: Vec<SampleRow> = rows
            .iter()
            .filter(|r| r.host == host && r.metric == metric)
            .cloned()
            .collect();
        v.sort_by_key(|a| a.ts);
        let limit = limit.max(0) as usize;
        if v.len() > limit {
            v.drain(0..v.len() - limit);
        }
        v
    }

    async fn record_anomaly(&self, anomaly: &Anomaly) -> bool {
        let mut anomalies = self.anomalies.lock().expect("anomalies lock poisoned");
        // Mirror Postgres' `ON CONFLICT (host, metric, ts) DO NOTHING`: first write wins.
        if anomalies
            .iter()
            .any(|a| a.host == anomaly.host && a.metric == anomaly.metric && a.ts == anomaly.ts)
        {
            return false;
        }
        anomalies.push(anomaly.clone());
        true
    }

    async fn recent_anomalies(
        &self,
        host: Option<&str>,
        metric: Option<&str>,
        limit: i64,
    ) -> Vec<Anomaly> {
        let anomalies = self.anomalies.lock().expect("anomalies lock poisoned");
        let mut v: Vec<Anomaly> = anomalies
            .iter()
            .filter(|a| host.is_none_or(|h| a.host == h))
            .filter(|a| metric.is_none_or(|m| a.metric == m))
            .cloned()
            .collect();
        // Newest-first; ties broken by (host, metric) for a stable order.
        v.sort_by(|a, b| {
            b.ts.cmp(&a.ts)
                .then(a.host.cmp(&b.host))
                .then(a.metric.cmp(&b.metric))
        });
        v.truncate(limit.max(0) as usize);
        v
    }
}

// --------------------------------------------------------------------------------------
// PostgreSQL-backed `Store` (portable: standard SQL, runtime queries, no macros).
// --------------------------------------------------------------------------------------
//
// Selected at runtime by `VITALS_STORE=postgres`. The `Store` trait is async, so each method
// drives sqlx natively and the handlers (plus the retention pruner) `.await` it on the serving
// runtime — there is NO `block_in_place` and NO sync-over-async bridge, so a query under ingest
// load never blocks a worker thread or wedges `/healthz`.

use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::Row;

/// PostgreSQL-backed [`Store`]. Holds just a `PgPool`; the async trait methods drive sqlx
/// natively, so no worker thread is ever blocked on a DB round-trip.
pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    /// Open a pooled connection. Async; call from within a Tokio runtime.
    pub async fn connect(database_url: &str) -> Result<Self, sqlx::Error> {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect(database_url)
            .await?;
        Ok(Self::from_pool(pool))
    }

    /// Construct from an existing pool (used by tests that share a pool).
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Idempotent, portable migration. Standard SQL only — safe to run on every startup.
    pub async fn migrate(&self) -> Result<(), sqlx::Error> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS metric_samples (\
                 host TEXT NOT NULL, \
                 metric TEXT NOT NULL, \
                 value DOUBLE PRECISION NOT NULL, \
                 ts BIGINT NOT NULL, \
                 PRIMARY KEY (host, metric, ts)\
             )",
        )
        .execute(&self.pool)
        .await?;
        // The dashboard/api read pattern is (host, metric, ts-range); the PK already covers
        // it left-to-right, but an explicit index documents intent and helps prune scans.
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_metric_samples_host_metric_ts \
                 ON metric_samples (host, metric, ts)",
        )
        .execute(&self.pool)
        .await?;
        // Anomalies recorded by the background detector (one per (host, metric, ts), deduped).
        // Folded in from the retired Augur service — standard SQL only, runs unchanged on FusionDB.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS anomalies (\
                 host TEXT NOT NULL, \
                 metric TEXT NOT NULL, \
                 ts BIGINT NOT NULL, \
                 value DOUBLE PRECISION NOT NULL, \
                 score DOUBLE PRECISION NOT NULL, \
                 note TEXT NOT NULL DEFAULT '', \
                 PRIMARY KEY (host, metric, ts)\
             )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_anomalies_ts ON anomalies (ts)",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn insert_samples_async(
        &self,
        host: &str,
        samples: &[Sample],
    ) -> Result<(), sqlx::Error> {
        // One parameterized statement per sample inside a single transaction: portable
        // (no multi-row VALUES generation, no COPY) and keeps a batch atomic.
        let mut tx = self.pool.begin().await?;
        for s in samples {
            sqlx::query(
                "INSERT INTO metric_samples (host, metric, value, ts) \
                 VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (host, metric, ts) DO NOTHING",
            )
            .bind(host)
            .bind(&s.metric)
            .bind(s.value)
            .bind(s.ts)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn query_async(
        &self,
        host: Option<&str>,
        metric: Option<&str>,
        since: i64,
    ) -> Result<Vec<SampleRow>, sqlx::Error> {
        // Build the predicate with stable parameter positions: $1 = host-or-null,
        // $2 = metric-or-null, $3 = since. `($1 IS NULL OR host = $1)` keeps one prepared
        // statement for all filter combinations (portable, no dynamic SQL).
        let rows = sqlx::query(
            "SELECT host, metric, value, ts FROM metric_samples \
             WHERE ($1 IS NULL OR host = $1) \
               AND ($2 IS NULL OR metric = $2) \
               AND ts >= $3 \
             ORDER BY host, metric, ts",
        )
        .bind(host)
        .bind(metric)
        .bind(since)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(Self::row_from).collect())
    }

    async fn latest_async(&self) -> Result<Vec<SampleRow>, sqlx::Error> {
        // Portable "latest row per group": join the base table to (host, metric, MAX(ts)).
        // Avoids Postgres-only `DISTINCT ON` so the statement also runs on FusionDB.
        let rows = sqlx::query(
            "SELECT m.host, m.metric, m.value, m.ts \
             FROM metric_samples m \
             JOIN (SELECT host, metric, MAX(ts) AS mts \
                     FROM metric_samples GROUP BY host, metric) latest \
               ON m.host = latest.host AND m.metric = latest.metric AND m.ts = latest.mts \
             ORDER BY m.host, m.metric",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(Self::row_from).collect())
    }

    async fn prune_async(&self, older_than: i64) -> Result<u64, sqlx::Error> {
        let res = sqlx::query("DELETE FROM metric_samples WHERE ts < $1")
            .bind(older_than)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }

    fn row_from(row: &sqlx::postgres::PgRow) -> SampleRow {
        SampleRow {
            host: row.get("host"),
            metric: row.get("metric"),
            value: row.get("value"),
            ts: row.get("ts"),
        }
    }

    fn anomaly_from(row: &sqlx::postgres::PgRow) -> Anomaly {
        Anomaly {
            host: row.get("host"),
            metric: row.get("metric"),
            ts: row.get("ts"),
            value: row.get("value"),
            score: row.get("score"),
            note: row.get("note"),
        }
    }

    async fn recent_samples_async(
        &self,
        host: &str,
        metric: &str,
        limit: i64,
    ) -> Result<Vec<SampleRow>, sqlx::Error> {
        // Pull the newest `limit` by ts DESC, then reverse to ascending for the analytics.
        let rows = sqlx::query(
            "SELECT host, metric, value, ts FROM metric_samples \
             WHERE host = $1 AND metric = $2 ORDER BY ts DESC LIMIT $3",
        )
        .bind(host)
        .bind(metric)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        let mut v: Vec<SampleRow> = rows.iter().map(Self::row_from).collect();
        v.reverse();
        Ok(v)
    }

    async fn record_anomaly_async(&self, a: &Anomaly) -> Result<bool, sqlx::Error> {
        let res = sqlx::query(
            "INSERT INTO anomalies (host, metric, ts, value, score, note) \
             VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (host, metric, ts) DO NOTHING",
        )
        .bind(&a.host)
        .bind(&a.metric)
        .bind(a.ts)
        .bind(a.value)
        .bind(a.score)
        .bind(&a.note)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    async fn recent_anomalies_async(
        &self,
        host: Option<&str>,
        metric: Option<&str>,
        limit: i64,
    ) -> Result<Vec<Anomaly>, sqlx::Error> {
        // Stable parameter positions: $1 host-or-null, $2 metric-or-null, $3 limit.
        let rows = sqlx::query(
            "SELECT host, metric, ts, value, score, note FROM anomalies \
             WHERE ($1 IS NULL OR host = $1) \
               AND ($2 IS NULL OR metric = $2) \
             ORDER BY ts DESC, host, metric LIMIT $3",
        )
        .bind(host)
        .bind(metric)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(Self::anomaly_from).collect())
    }
}

#[async_trait]
impl Store for PgStore {
    async fn insert_samples(&self, host: &str, samples: &[Sample]) {
        if let Err(e) = self.insert_samples_async(host, samples).await {
            tracing::error!(error = %e, "pg insert_samples failed");
        }
    }

    async fn query(&self, host: Option<&str>, metric: Option<&str>, since: i64) -> Vec<SampleRow> {
        self.query_async(host, metric, since)
            .await
            .unwrap_or_else(|e| {
                tracing::error!(error = %e, "pg query failed");
                Vec::new()
            })
    }

    async fn latest(&self) -> Vec<SampleRow> {
        self.latest_async().await.unwrap_or_else(|e| {
            tracing::error!(error = %e, "pg latest failed");
            Vec::new()
        })
    }

    async fn prune(&self, older_than: i64) -> u64 {
        self.prune_async(older_than).await.unwrap_or_else(|e| {
            tracing::error!(error = %e, "pg prune failed");
            0
        })
    }

    async fn recent_samples(&self, host: &str, metric: &str, limit: i64) -> Vec<SampleRow> {
        self.recent_samples_async(host, metric, limit)
            .await
            .unwrap_or_else(|e| {
                tracing::error!(error = %e, "pg recent_samples failed");
                Vec::new()
            })
    }

    async fn record_anomaly(&self, anomaly: &Anomaly) -> bool {
        self.record_anomaly_async(anomaly).await.unwrap_or_else(|e| {
            tracing::error!(error = %e, "pg record_anomaly failed");
            false
        })
    }

    async fn recent_anomalies(
        &self,
        host: Option<&str>,
        metric: Option<&str>,
        limit: i64,
    ) -> Vec<Anomaly> {
        self.recent_anomalies_async(host, metric, limit)
            .await
            .unwrap_or_else(|e| {
                tracing::error!(error = %e, "pg recent_anomalies failed");
                Vec::new()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(metric: &str, value: f64, ts: i64) -> Sample {
        Sample::new(metric, value, ts)
    }

    #[tokio::test]
    async fn recent_samples_is_ascending_tail_per_pair() {
        let store = InMemoryStore::new();
        for ts in 0..10i64 {
            store.insert_samples("box", &[s("cpu_pct", ts as f64, ts)]).await;
        }
        // A different host/metric must not leak into the window.
        store.insert_samples("other", &[s("cpu_pct", 99.0, 5)]).await;
        store.insert_samples("box", &[s("mem_pct", 99.0, 5)]).await;

        let w = store.recent_samples("box", "cpu_pct", 3).await;
        assert_eq!(w.iter().map(|r| r.ts).collect::<Vec<_>>(), [7, 8, 9]);
        assert!(w.iter().all(|r| r.host == "box" && r.metric == "cpu_pct"));
    }

    #[tokio::test]
    async fn anomaly_dedup_and_filter() {
        let store = InMemoryStore::new();
        assert!(
            store
                .record_anomaly(&Anomaly::new("box", "cpu_pct", 100, 99.0, 5.0, "z=5".into()))
                .await
        );
        // Same (host, metric, ts) -> no second row (first write wins).
        assert!(
            !store
                .record_anomaly(&Anomaly::new("box", "cpu_pct", 100, 99.0, 6.0, "z=6".into()))
                .await
        );
        store
            .record_anomaly(&Anomaly::new("box", "mem_pct", 200, 80.0, 4.0, "z=4".into()))
            .await;

        assert_eq!(store.recent_anomalies(Some("box"), Some("cpu_pct"), 10).await.len(), 1);
        assert_eq!(store.recent_anomalies(Some("box"), None, 10).await.len(), 2);
        assert_eq!(store.recent_anomalies(None, None, 10).await.len(), 2);
        // Newest-first across the series.
        assert_eq!(store.recent_anomalies(None, None, 10).await[0].ts, 200);
    }
}
