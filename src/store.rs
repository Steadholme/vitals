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

use crate::metrics::{Sample, SampleRow};

/// Pluggable metric TSDB. No `.await` is ever held across the internal lock.
pub trait Store: Send + Sync {
    /// Append a host's scrape batch. Idempotent per `(host, metric, ts)` (first write wins).
    fn insert_samples(&self, host: &str, samples: &[Sample]);

    /// Return rows matching the filters, ordered by `(host, metric, ts)`:
    /// - `host`/`metric` `None` means "any";
    /// - `since` is an inclusive lower bound on `ts`.
    fn query(&self, host: Option<&str>, metric: Option<&str>, since: i64) -> Vec<SampleRow>;

    /// The most-recent sample for every `(host, metric)` pair (the dashboard's gauges).
    fn latest(&self) -> Vec<SampleRow>;

    /// Delete samples with `ts < older_than`. Returns the number of rows removed.
    fn prune(&self, older_than: i64) -> u64;
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

impl Store for InMemoryStore {
    fn insert_samples(&self, host: &str, samples: &[Sample]) {
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

    fn query(&self, host: Option<&str>, metric: Option<&str>, since: i64) -> Vec<SampleRow> {
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

    fn latest(&self) -> Vec<SampleRow> {
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

    fn prune(&self, older_than: i64) -> u64 {
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
// Selected at runtime by `VITALS_STORE=postgres`. The `Store` trait is synchronous
// (handlers never `.await` the store), so each method bridges to async sqlx via
// `block_in_place` + the runtime `Handle` — the same pattern keystone/keyward use. This
// needs a multi-threaded Tokio runtime, which production (`#[tokio::main]`) and the
// `multi_thread` integration test both provide.

use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::Row;

/// PostgreSQL-backed [`Store`]. Holds a `PgPool` plus the runtime [`Handle`] used to drive
/// async queries to completion from the synchronous trait methods.
///
/// [`Handle`]: tokio::runtime::Handle
pub struct PgStore {
    pool: PgPool,
    handle: tokio::runtime::Handle,
}

impl PgStore {
    /// Open a pooled connection. Captures the current runtime handle for the sync→async
    /// bridge; must be called from within a Tokio runtime.
    pub async fn connect(database_url: &str) -> Result<Self, sqlx::Error> {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect(database_url)
            .await?;
        Ok(Self {
            pool,
            handle: tokio::runtime::Handle::current(),
        })
    }

    /// Construct from an existing pool (used by tests that share a pool).
    pub fn from_pool(pool: PgPool) -> Self {
        Self {
            pool,
            handle: tokio::runtime::Handle::current(),
        }
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

    /// Drive an async DB op to completion from a synchronous trait method.
    fn block_on<F: std::future::Future>(&self, fut: F) -> F::Output {
        tokio::task::block_in_place(|| self.handle.block_on(fut))
    }
}

impl Store for PgStore {
    fn insert_samples(&self, host: &str, samples: &[Sample]) {
        if let Err(e) = self.block_on(self.insert_samples_async(host, samples)) {
            tracing::error!(error = %e, "pg insert_samples failed");
        }
    }

    fn query(&self, host: Option<&str>, metric: Option<&str>, since: i64) -> Vec<SampleRow> {
        self.block_on(self.query_async(host, metric, since))
            .unwrap_or_else(|e| {
                tracing::error!(error = %e, "pg query failed");
                Vec::new()
            })
    }

    fn latest(&self) -> Vec<SampleRow> {
        self.block_on(self.latest_async()).unwrap_or_else(|e| {
            tracing::error!(error = %e, "pg latest failed");
            Vec::new()
        })
    }

    fn prune(&self, older_than: i64) -> u64 {
        self.block_on(self.prune_async(older_than)).unwrap_or_else(|e| {
            tracing::error!(error = %e, "pg prune failed");
            0
        })
    }
}
