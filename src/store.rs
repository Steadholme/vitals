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

use crate::metrics::{Sample, SampleRow};

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
}

/// In-memory `Store`. `std::sync::Mutex<Vec>` — no async lock needed. The default when
/// `VITALS_STORE` is unset; keeps the whole service database-free (used by tests too).
#[derive(Default)]
pub struct InMemoryStore {
    rows: Mutex<Vec<SampleRow>>,
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
}
