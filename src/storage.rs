use anyhow::{anyhow, ensure, Result};
use sqlx::{sqlite::SqlitePoolOptions, SqliteConnection, SqlitePool};
use tokio::sync::Mutex;
use std::time::Duration;
use chrono::Utc;
use tracing::{info, warn};

use crate::types::*;

const MIB: u64 = 1024 * 1024;
const MAX_DATABASE_BYTES: u64 = 500 * MIB;
const EVICTION_BATCH_SIZE: i64 = 10_000;

pub struct Database {
    pool: SqlitePool,
    max_size_bytes: u64,
    metrics_lock: Mutex<()>,
}

#[derive(Debug, Clone, Copy)]
struct PageStats {
    page_size: u64,
    page_count: u64,
    freelist_count: u64,
}

impl PageStats {
    fn file_bytes(self) -> Result<u64> {
        self.page_size
            .checked_mul(self.page_count)
            .ok_or_else(|| anyhow!("database file size overflow"))
    }

    fn used_bytes(self) -> Result<u64> {
        let used_pages = self
            .page_count
            .checked_sub(self.freelist_count)
            .ok_or_else(|| anyhow!("database freelist exceeds its page count"))?;
        self.page_size
            .checked_mul(used_pages)
            .ok_or_else(|| anyhow!("database used size overflow"))
    }
}

async fn apply_max_page_count(
    connection: &mut SqliteConnection,
    max_size_bytes: u64,
) -> sqlx::Result<u64> {
    let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
        .fetch_one(&mut *connection)
        .await?;
    let page_size = u64::try_from(page_size)
        .map_err(|_| sqlx::Error::Protocol("SQLite returned a negative page size".into()))?;
    if page_size == 0 {
        return Err(sqlx::Error::Protocol(
            "SQLite returned a zero page size".into(),
        ));
    }

    let max_pages = max_size_bytes / page_size;
    if max_pages == 0 {
        return Err(sqlx::Error::Protocol(format!(
            "database size limit {max_size_bytes} is smaller than SQLite page size {page_size}"
        )));
    }

    let statement = format!("PRAGMA max_page_count = {max_pages}");
    let applied: i64 = sqlx::query_scalar(&statement)
        .fetch_one(&mut *connection)
        .await?;
    u64::try_from(applied)
        .map_err(|_| sqlx::Error::Protocol("SQLite returned a negative max page count".into()))
}

/// Classify a sqlx error as SQLITE_FULL (extended code 13).
/// Matches on the database error code, never on message text, so lock
/// conflicts, corruption, or other I/O failures are never treated as full.
fn is_database_full(error: &sqlx::Error) -> bool {
    matches!(
        error,
        sqlx::Error::Database(database_error)
            if database_error.code().as_deref() == Some("13")
    )
}

impl Database {
    pub async fn new(db_path: &str) -> Result<Self> {
        Self::new_with_max_size(db_path, MAX_DATABASE_BYTES).await
    }

    async fn new_with_max_size(db_path: &str, max_size_bytes: u64) -> Result<Self> {
        ensure!(max_size_bytes > 0, "database size limit must be positive");

        // SQLite connection string
        let url = if db_path == ":memory:" {
            "sqlite::memory:".to_string()
        } else {
            format!("sqlite://{}?mode=rwc", db_path)
        };

        if db_path == ":memory:" {
            // An in-memory database must stay on one connection.  Run migrations
            // before applying the cap because the migration may add an index.
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                // Release idle connections after 30 s so they don't accumulate.
                .idle_timeout(Duration::from_secs(30))
                // If all connections are busy, fail fast rather than blocking
                // indefinitely — the caller logs the error and moves on.
                .acquire_timeout(Duration::from_secs(5))
                .connect(&url)
                .await?;
            let db = Self {
                pool,
                max_size_bytes,
                metrics_lock: Mutex::new(()),
            };
            db.run_migrations().await?;
            db.enforce_startup_size_limit().await?;
            info!("Database initialized at {}", db_path);
            return Ok(db);
        }

        // Existing file databases may be exactly at their old page cap while
        // missing a migration-created index.  Bootstrap on one uncapped
        // connection so migration and startup compaction can finish first.
        let bootstrap_pool = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(Duration::from_secs(30))
            .acquire_timeout(Duration::from_secs(5))
            .connect(&url)
            .await?;
        let bootstrap = Self {
            pool: bootstrap_pool,
            max_size_bytes,
            metrics_lock: Mutex::new(()),
        };
        bootstrap.run_migrations().await?;
        bootstrap.enforce_startup_size_limit().await?;
        bootstrap.pool.close().await;

        // Every connection in the steady-state file-backed pool receives the
        // per-connection cap, including connections opened lazily after init.
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .idle_timeout(Duration::from_secs(30))
            .acquire_timeout(Duration::from_secs(5))
            .after_connect(move |connection, _metadata| {
                Box::pin(async move {
                    apply_max_page_count(connection, max_size_bytes)
                        .await
                        .map(|_| ())
                })
            })
            .connect(&url)
            .await?;
        let db = Self {
            pool,
            max_size_bytes,
            metrics_lock: Mutex::new(()),
        };
        info!("Database initialized at {}", db_path);
        Ok(db)
    }

    async fn page_stats(&self) -> Result<PageStats> {
        let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
            .fetch_one(&self.pool)
            .await?;
        let page_count: i64 = sqlx::query_scalar("PRAGMA page_count")
            .fetch_one(&self.pool)
            .await?;
        let freelist_count: i64 = sqlx::query_scalar("PRAGMA freelist_count")
            .fetch_one(&self.pool)
            .await?;

        let stats = PageStats {
            page_size: u64::try_from(page_size)?,
            page_count: u64::try_from(page_count)?,
            freelist_count: u64::try_from(freelist_count)?,
        };
        ensure!(stats.page_size > 0, "SQLite returned a zero page size");
        ensure!(
            stats.freelist_count <= stats.page_count,
            "database freelist exceeds its page count"
        );
        Ok(stats)
    }

    async fn metrics_history_count(&self) -> Result<i64> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM metrics_history")
            .fetch_one(&self.pool)
            .await?;
        ensure!(count >= 0, "SQLite returned a negative metric history count");
        Ok(count)
    }

    async fn enforce_startup_size_limit(&self) -> Result<()> {
        let original = self.page_stats().await?;
        let final_stats = if original.file_bytes()? > self.max_size_bytes {
            let target_bytes = self
                .max_size_bytes
                .checked_mul(9)
                .ok_or_else(|| anyhow!("database shrink target overflow"))?
                / 10;
            let mut deleted_before_compaction = 0_u64;
            let mut stats = original;

            loop {
                let used_bytes = stats.used_bytes()?;
                if used_bytes <= target_bytes {
                    break;
                }

                let metric_count = self.metrics_history_count().await?;
                // Fragmented pre-compaction page usage cannot determine whether
                // protected data, or protected data plus the newest metric, fits.
                if metric_count <= 1 {
                    break;
                }

                let batch_size = (metric_count / 10).max(1).min(EVICTION_BATCH_SIZE);
                let batch = self.delete_oldest_metrics_batch(batch_size).await?;
                ensure!(
                    batch > 0,
                    "database exceeds its startup shrink target but has no metric history to evict"
                );
                deleted_before_compaction = deleted_before_compaction
                    .checked_add(batch)
                    .ok_or_else(|| anyhow!("evicted metric count overflow"))?;
                stats = self.page_stats().await?;
            }

            sqlx::query("VACUUM").execute(&self.pool).await?;
            let stats_after_first_compaction = self.page_stats().await?;
            let mut deleted_after_compaction = 0_u64;
            let final_compaction_stats =
                if stats_after_first_compaction.file_bytes()? > self.max_size_bytes {
                    let metric_count = self.metrics_history_count().await?;
                    if metric_count == 1 {
                        let batch = self.delete_oldest_metrics_batch(1).await?;
                        ensure!(
                            batch == 1,
                            "database remains above its size limit after compaction but the sole metric could not be evicted"
                        );
                        deleted_after_compaction = deleted_after_compaction
                            .checked_add(batch)
                            .ok_or_else(|| anyhow!("evicted metric count overflow"))?;

                        // Rare correctness path: only compacted size proves that
                        // the newest metric itself cannot coexist with protected data.
                        sqlx::query("VACUUM").execute(&self.pool).await?;
                        self.page_stats().await?
                    } else {
                        stats_after_first_compaction
                    }
                } else {
                    stats_after_first_compaction
                };
            let final_file_bytes = final_compaction_stats.file_bytes()?;
            warn!(
                "Startup database compaction deleted {deleted_before_compaction} metric records before the first VACUUM and {deleted_after_compaction} after it; final compacted main-file size is {final_file_bytes} bytes"
            );

            final_compaction_stats
        } else {
            original
        };

        let applied = {
            let mut connection = self.pool.acquire().await?;
            apply_max_page_count(&mut connection, self.max_size_bytes).await?
        };
        ensure!(
            final_stats.file_bytes()? <= self.max_size_bytes,
            "database remains above size limit after metric eviction; protected data cannot fit"
        );
        let allowed_pages = self.max_size_bytes / final_stats.page_size;
        ensure!(
            applied <= allowed_pages,
            "failed to apply database page limit: SQLite allowed {applied} pages, cap allows {allowed_pages}"
        );
        ensure!(
            final_stats.page_count <= applied,
            "database page count exceeds its applied page limit"
        );
        Ok(())
    }

    async fn run_migrations(&self) -> Result<()> {
        sqlx::query(r#"
            CREATE TABLE IF NOT EXISTS metrics_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                node_id TEXT NOT NULL,
                hostname TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                cpu_usage REAL NOT NULL,
                memory_usage REAL NOT NULL,
                memory_used_bytes INTEGER NOT NULL,
                memory_total_bytes INTEGER NOT NULL,
                uptime_seconds INTEGER NOT NULL,
                raw_json TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_metrics_node_time
                ON metrics_history(node_id, timestamp);

            CREATE INDEX IF NOT EXISTS idx_metrics_time
                ON metrics_history(timestamp, id);

            CREATE TABLE IF NOT EXISTS alerts_log (
                id TEXT PRIMARY KEY,
                node_id TEXT NOT NULL,
                rule_name TEXT NOT NULL,
                severity TEXT NOT NULL,
                message TEXT NOT NULL,
                triggered_at TEXT NOT NULL,
                resolved_at TEXT,
                value REAL NOT NULL,
                threshold REAL NOT NULL
            );

            CREATE TABLE IF NOT EXISTS nodes_seen (
                id TEXT PRIMARY KEY,
                hostname TEXT NOT NULL,
                api_addr TEXT NOT NULL,
                gossip_addr TEXT NOT NULL,
                last_seen TEXT NOT NULL,
                version TEXT NOT NULL
            );
        "#)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Store a metrics snapshot
    pub async fn store_metrics(&self, node_id: &NodeId, metrics: &SystemMetrics) -> Result<()> {
        let _metrics_guard = self.metrics_lock.lock().await;
        let node_id_str = node_id.to_string();
        let ts = metrics.timestamp.to_rfc3339();
        let raw = serde_json::to_string(metrics)?;

        loop {
            match sqlx::query(r#"
                INSERT INTO metrics_history
                    (node_id, hostname, timestamp, cpu_usage, memory_usage,
                     memory_used_bytes, memory_total_bytes, uptime_seconds, raw_json)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#)
            .bind(&node_id_str)
            .bind(&metrics.hostname)
            .bind(&ts)
            .bind(metrics.cpu.usage_percent as f64)
            .bind(metrics.memory.usage_percent as f64)
            .bind(metrics.memory.used_bytes as i64)
            .bind(metrics.memory.total_bytes as i64)
            .bind(metrics.uptime_seconds as i64)
            .bind(&raw)
            .execute(&self.pool)
            .await
            {
                Ok(_) => return Ok(()),
                Err(error) if is_database_full(&error) => {
                    let deleted = self.delete_oldest_metrics_batch(EVICTION_BATCH_SIZE).await?;
                    if deleted == 0 {
                        return Err(error.into());
                    }
                    warn!(
                        "Database full; evicted {} oldest metric records",
                        deleted
                    );
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Get recent metrics for a node
    pub async fn get_recent_metrics(
        &self,
        node_id: &NodeId,
        limit: i64,
    ) -> Result<Vec<SystemMetrics>> {
        let node_id_str = node_id.to_string();

        let rows: Vec<(String,)> = sqlx::query_as(r#"
            SELECT raw_json FROM metrics_history
            WHERE node_id = ?
            ORDER BY timestamp DESC
            LIMIT ?
        "#)
        .bind(&node_id_str)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        let metrics: Vec<SystemMetrics> = rows.iter()
            .filter_map(|(raw,)| serde_json::from_str(raw).ok())
            .collect();

        Ok(metrics)
    }

    /// Store an alert
    pub async fn store_alert(&self, alert: &Alert) -> Result<()> {
        sqlx::query(r#"
            INSERT OR REPLACE INTO alerts_log
                (id, node_id, rule_name, severity, message, triggered_at, resolved_at, value, threshold)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#)
        .bind(alert.id.to_string())
        .bind(alert.node_id.to_string())
        .bind(&alert.rule_name)
        .bind(format!("{:?}", alert.severity))
        .bind(&alert.message)
        .bind(alert.triggered_at.to_rfc3339())
        .bind(alert.resolved_at.map(|t| t.to_rfc3339()))
        .bind(alert.value)
        .bind(alert.threshold)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Upsert node info
    pub async fn upsert_node(&self, node: &NodeInfo) -> Result<()> {
        sqlx::query(r#"
            INSERT OR REPLACE INTO nodes_seen
                (id, hostname, api_addr, gossip_addr, last_seen, version)
            VALUES (?, ?, ?, ?, ?, ?)
        "#)
        .bind(node.id.to_string())
        .bind(&node.hostname)
        .bind(&node.api_addr)
        .bind(&node.gossip_addr)
        .bind(node.last_seen.to_rfc3339())
        .bind(&node.version)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn delete_oldest_metrics_batch(&self, limit: i64) -> Result<u64> {
        let result = sqlx::query(r#"
            DELETE FROM metrics_history
            WHERE id IN (
                SELECT id FROM metrics_history
                ORDER BY timestamp ASC, id ASC
                LIMIT ?
            )
        "#)
        .bind(limit)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected())
    }

    /// Delete metrics older than retention_hours
    pub async fn cleanup_old_metrics(&self, retention_hours: u64) -> Result<u64> {
        let cutoff = Utc::now() - chrono::Duration::hours(retention_hours as i64);
        let cutoff_str = cutoff.to_rfc3339();

        let result = sqlx::query(r#"
            DELETE FROM metrics_history WHERE timestamp < ?
        "#)
        .bind(&cutoff_str)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use std::sync::Arc;
    use tempfile::TempDir;

    async fn temp_database() -> (TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.db");
        let db = Database::new(path.to_str().unwrap()).await.unwrap();
        (dir, db)
    }

    async fn try_insert_raw_metric(db: &Database, timestamp: &str, hostname: &str) -> sqlx::Result<()> {
        sqlx::query(
            r#"INSERT INTO metrics_history
               (node_id, hostname, timestamp, cpu_usage, memory_usage,
                memory_used_bytes, memory_total_bytes, uptime_seconds, raw_json)
               VALUES (?, ?, ?, 0, 0, 0, 0, 0, '{}')"#,
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(hostname)
        .bind(timestamp)
        .execute(&db.pool)
        .await
        .map(|_| ())
    }

    async fn insert_raw_metric(db: &Database, timestamp: &str, hostname: &str) {
        try_insert_raw_metric(db, timestamp, hostname)
            .await
            .unwrap();
    }


    async fn database_bytes(db: &Database) -> u64 {
        let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        let page_count: i64 = sqlx::query_scalar("PRAGMA page_count")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        page_size as u64 * page_count as u64
    }

    fn metrics_at(timestamp: chrono::DateTime<Utc>, payload_bytes: usize) -> SystemMetrics {
        SystemMetrics {
            timestamp,
            cpu: CpuMetrics {
                usage_percent: 0.0,
                core_usages: vec![],
                core_count: 1,
            },
            memory: MemoryMetrics {
                total_bytes: 1,
                used_bytes: 0,
                available_bytes: 1,
                usage_percent: 0.0,
                swap_total_bytes: 0,
                swap_used_bytes: 0,
            },
            disks: vec![],
            physical_disks: vec![],
            networks: vec![],
            load_average: None,
            top_processes: vec![],
            uptime_seconds: 0,
            os_name: "test".to_string(),
            hostname: "x".repeat(payload_bytes),
        }
    }

    #[tokio::test]
    async fn cleanup_old_metrics_only_removes_expired_rows() {
        let (_dir, db) = temp_database().await;
        insert_raw_metric(
            &db,
            &(Utc::now() - chrono::Duration::hours(48)).to_rfc3339(),
            "expired",
        )
        .await;
        insert_raw_metric(&db, &Utc::now().to_rfc3339(), "current").await;

        assert_eq!(db.cleanup_old_metrics(24).await.unwrap(), 1);
        let remaining: Vec<String> = sqlx::query_scalar("SELECT hostname FROM metrics_history")
            .fetch_all(&db.pool)
            .await
            .unwrap();
        assert_eq!(remaining, vec!["current"]);
    }

    #[tokio::test]
    async fn delete_oldest_metrics_orders_by_timestamp_then_id() {
        let (_dir, db) = temp_database().await;
        let older_time = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let same_time = Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap();
        let newer_time = Utc.with_ymd_and_hms(2026, 1, 3, 0, 0, 0).unwrap();

        // Make ID order disagree with timestamp order, then make timestamp-only
        // scans prefer descending IDs so both ORDER BY terms are independently required.
        insert_raw_metric(&db, &same_time.to_rfc3339(), "older-id").await;
        insert_raw_metric(&db, &same_time.to_rfc3339(), "newer-id").await;
        insert_raw_metric(&db, &older_time.to_rfc3339(), "oldest-time").await;
        insert_raw_metric(&db, &newer_time.to_rfc3339(), "newest-time").await;

        sqlx::query("DROP INDEX idx_metrics_time")
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE INDEX idx_metrics_time_desc_id ON metrics_history(timestamp ASC, id DESC)",
        )
        .execute(&db.pool)
        .await
        .unwrap();

        assert_eq!(db.delete_oldest_metrics_batch(2).await.unwrap(), 2);

        let remaining: Vec<String> =
            sqlx::query_scalar("SELECT hostname FROM metrics_history ORDER BY timestamp, id")
                .fetch_all(&db.pool)
                .await
                .unwrap();
        assert_eq!(remaining, vec!["newer-id", "newest-time"]);
    }

    #[tokio::test]
    async fn opening_oversized_database_evicts_oldest_and_shrinks_file() {
        const LIMIT: u64 = 256 * 1024;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oversized.db");
        let path = path.to_str().unwrap();

        let db = Database::new_with_max_size(path, 4 * 1024 * 1024)
            .await
            .unwrap();
        for index in 0..80 {
            let timestamp = Utc.timestamp_opt(index, 0).unwrap().to_rfc3339();
            insert_raw_metric(
                &db,
                &timestamp,
                &format!("{index:03}-{}", "x".repeat(8 * 1024)),
            )
            .await;
        }
        assert!(database_bytes(&db).await > LIMIT);
        db.pool.close().await;
        let oversized_file_bytes = std::fs::metadata(path).unwrap().len();
        assert!(
            oversized_file_bytes > LIMIT,
            "setup file length {oversized_file_bytes} did not exceed limit {LIMIT}"
        );

        let db = Database::new_with_max_size(path, LIMIT).await.unwrap();

        assert!(database_bytes(&db).await <= LIMIT);
        let compacted_file_bytes = std::fs::metadata(path).unwrap().len();
        assert!(
            compacted_file_bytes <= LIMIT,
            "compacted file length {compacted_file_bytes} exceeds limit {LIMIT}"
        );
        let newest: String = sqlx::query_scalar(
            "SELECT hostname FROM metrics_history ORDER BY timestamp DESC, id DESC LIMIT 1",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert!(newest.starts_with("079-"));
    }

    #[tokio::test]
    async fn opening_legacy_database_near_limit_can_add_time_index() {
        const LIMIT: u64 = 256 * 1024;
        const MAX_LARGE_ROWS: usize = 128;
        const MAX_TINY_ROWS: usize = 1024;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy-near-limit.db");
        let path_str = path.to_str().unwrap();
        let db = Database::new_with_max_size(path_str, LIMIT)
            .await
            .unwrap();

        sqlx::query("DROP INDEX idx_metrics_time")
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("VACUUM").execute(&db.pool).await.unwrap();

        let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        let page_size = u64::try_from(page_size).unwrap();
        assert!(page_size > 0, "SQLite returned a zero page size");
        let max_pages = LIMIT / page_size;
        assert!(max_pages > 0, "database limit must include a SQLite page");
        let applied_max_pages: i64 = sqlx::query_scalar(&format!(
            "PRAGMA max_page_count = {max_pages}"
        ))
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(u64::try_from(applied_max_pages).unwrap(), max_pages);

        let mut page_count: u64 = sqlx::query_scalar("PRAGMA page_count")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        for index in 0..MAX_LARGE_ROWS {
            if page_count >= max_pages {
                break;
            }

            match try_insert_raw_metric(
                &db,
                &Utc.timestamp_opt(index as i64, 0).unwrap().to_rfc3339(),
                &format!("legacy-{index:03}-{}", "x".repeat(8 * 1024)),
            )
            .await
            {
                Ok(()) => {}
                Err(error) => {
                    assert!(
                        is_database_full(&error),
                        "large legacy setup insert failed unexpectedly: {error}"
                    );
                    break;
                }
            }
            page_count = sqlx::query_scalar("PRAGMA page_count")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        }

        for index in 0..MAX_TINY_ROWS {
            if page_count >= max_pages {
                break;
            }

            match try_insert_raw_metric(
                &db,
                &Utc.timestamp_opt(1_000_000 + index as i64, 0)
                    .unwrap()
                    .to_rfc3339(),
                &format!("tiny-{index:04}"),
            )
            .await
            {
                Ok(()) => {}
                Err(error) => {
                    assert!(
                        is_database_full(&error),
                        "tiny legacy setup insert failed unexpectedly: {error}"
                    );
                    break;
                }
            }
            page_count = sqlx::query_scalar("PRAGMA page_count")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        }

        let freelist_count: u64 = sqlx::query_scalar("PRAGMA freelist_count")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        let file_bytes = std::fs::metadata(&path).unwrap().len();
        assert_eq!(page_count, max_pages, "legacy setup did not reach exact cap");
        assert_eq!(freelist_count, 0, "legacy setup left free pages");
        assert_eq!(file_bytes, LIMIT, "legacy setup file length must equal cap");

        db.pool.close().await;
        let reopened = Database::new_with_max_size(path_str, LIMIT)
            .await
            .expect("legacy database at the cap should migrate successfully");
        let time_index: Option<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'index' AND name = 'idx_metrics_time'",
        )
        .fetch_optional(&reopened.pool)
        .await
        .unwrap();
        assert_eq!(time_index.as_deref(), Some("idx_metrics_time"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_metric_writes_preserve_the_latest_timestamp() {
        const LIMIT: u64 = 256 * 1024;
        const TARGET_MARGIN: u64 = 32 * 1024;
        const MAX_PREFILL_ROWS: usize = 128;
        const WRITER_COUNT: usize = 64;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("concurrent-writes.db");
        let db = Database::new_with_max_size(path.to_str().unwrap(), LIMIT)
            .await
            .unwrap();

        let mut prefill_rows = 0_usize;
        let mut prefill_bytes = database_bytes(&db).await;
        while prefill_rows < MAX_PREFILL_ROWS && prefill_bytes <= LIMIT - TARGET_MARGIN {
            let timestamp = Utc
                .timestamp_opt(prefill_rows as i64, 0)
                .unwrap()
                .to_rfc3339();
            insert_raw_metric(
                &db,
                &timestamp,
                &format!("prefill-{prefill_rows:03}-{}", "p".repeat(8 * 1024)),
            )
            .await;
            prefill_rows += 1;
            prefill_bytes = database_bytes(&db).await;
        }
        assert!(prefill_rows > 0, "bounded prefill inserted no metric rows");
        assert!(
            prefill_bytes > LIMIT - TARGET_MARGIN,
            "bounded prefill reached only {prefill_bytes} page bytes; target is {}",
            LIMIT - TARGET_MARGIN
        );
        assert!(
            prefill_bytes <= LIMIT,
            "prefill page bytes {prefill_bytes} exceed limit {LIMIT}"
        );

        let db = Arc::new(db);
        let node_id = uuid::Uuid::new_v4();
        // Scheduling is intentionally uncontrolled; equal timestamps make the "latest"
        // assertion independent of Tokio lock acquisition order. The single-thread
        // eviction test covers timestamp ordering; these writers provide a bounded
        // stress regression for interleaved eviction and retry operations.
        let mut writers = Vec::with_capacity(WRITER_COUNT);
        for index in 0..WRITER_COUNT {
            let db = Arc::clone(&db);
            let metrics = metrics_at(
                Utc.timestamp_opt(1_000_000, 0).unwrap(),
                8 * 1024,
            );
            writers.push(tokio::spawn(async move {
                db.store_metrics(&node_id, &metrics).await
            }));
        }

        let mut join_results = Vec::with_capacity(WRITER_COUNT);
        for writer in writers {
            join_results.push(writer.await);
        }
        for (index, result) in join_results.into_iter().enumerate() {
            let result = result.expect("metric writer task panicked");
            result.unwrap_or_else(|error| panic!("metric writer {index} failed: {error:#}"));
        }

        // Every concurrent writer uses the same timestamp, so this "latest" assertion
        // remains deterministic regardless of Tokio lock acquisition order.
        let expected_latest = Utc
            .timestamp_opt(1_000_000, 0)
            .unwrap()
            .to_rfc3339();
        let latest_timestamp: String = sqlx::query_scalar(
            "SELECT timestamp FROM metrics_history ORDER BY timestamp DESC, id DESC LIMIT 1",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            latest_timestamp,
            expected_latest,
            "equal writer timestamps make latest independent of Tokio lock acquisition order"
        );

        let final_main_file_bytes = std::fs::metadata(&path).unwrap().len();
        assert!(
            final_main_file_bytes <= LIMIT,
            "main database file uses {final_main_file_bytes} bytes; limit is {LIMIT}"
        );
    }

    #[tokio::test]
    async fn opening_oversized_protected_database_without_metrics_compacts() {
        const LIMIT: u64 = 256 * 1024;
        const TARGET: u64 = LIMIT * 9 / 10;
        const MAX_ALERT_ROWS: i64 = 256;
        const MAX_METRIC_ROWS: i64 = 64;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("protected-oversized.db");
        let path = path.to_str().unwrap();
        let db = Database::new_with_max_size(path, 4 * 1024 * 1024)
            .await
            .unwrap();

        let alert_message = "a".repeat(2 * 1024);

        sqlx::query(
            r#"INSERT INTO nodes_seen
               (id, hostname, api_addr, gossip_addr, last_seen, version)
               VALUES ('protected-node', 'protected-host', '127.0.0.1:1',
                       '127.0.0.1:2', '1970-01-01T00:00:00+00:00', 'test')"#,
        )
        .execute(&db.pool)
        .await
        .unwrap();
        let mut alert_count = 0_i64;
        let mut protected_stats = db.page_stats().await.unwrap();
        for index in 0..MAX_ALERT_ROWS {
            if protected_stats.used_bytes().unwrap() > TARGET {
                break;
            }

            sqlx::query(
                r#"INSERT INTO alerts_log
                   (id, node_id, rule_name, severity, message, triggered_at,
                    resolved_at, value, threshold)
                   VALUES (?, 'protected-node', 'protected-rule', 'warning', ?, ?,
                           NULL, 1.0, 2.0)"#,
            )
            .bind(format!("alert-{index:03}"))
            .bind(&alert_message)
            .bind(Utc.timestamp_opt(index, 0).unwrap().to_rfc3339())
            .execute(&db.pool)
            .await
            .unwrap();
            alert_count += 1;
            protected_stats = db.page_stats().await.unwrap();
        }

        let protected_used_bytes = protected_stats.used_bytes().unwrap();
        assert!(alert_count > 0, "setup did not insert protected alert rows");
        assert!(
            protected_used_bytes > TARGET,
            "bounded alert setup reached only {protected_used_bytes} live bytes; target is {TARGET}"
        );
        assert!(
            protected_used_bytes < LIMIT,
            "protected rows use {protected_used_bytes} bytes and do not fit below limit {LIMIT}"
        );

        let mut metric_count = 0_i64;
        let mut expanded_file_bytes = protected_stats.file_bytes().unwrap();
        for index in 0..MAX_METRIC_ROWS {
            if expanded_file_bytes > LIMIT {
                break;
            }

            let timestamp = Utc
                .timestamp_opt(10_000 + index, 0)
                .unwrap()
                .to_rfc3339();
            insert_raw_metric(
                &db,
                &timestamp,
                &format!("metric-{index:03}-{}", "m".repeat(8 * 1024)),
            )
            .await;
            metric_count += 1;
            expanded_file_bytes = database_bytes(&db).await;
        }

        assert!(metric_count > 0, "setup did not insert metric rows");
        assert!(
            expanded_file_bytes > LIMIT,
            "bounded metric setup reached only {expanded_file_bytes} file bytes; limit is {LIMIT}"
        );

        let deleted = sqlx::query("DELETE FROM metrics_history")
            .execute(&db.pool)
            .await
            .unwrap()
            .rows_affected();
        assert_eq!(deleted, metric_count as u64);

        let post_delete_stats = db.page_stats().await.unwrap();
        let post_delete_used_bytes = post_delete_stats.used_bytes().unwrap();
        assert!(
            post_delete_stats.file_bytes().unwrap() > LIMIT,
            "deleting metrics unexpectedly removed the physical oversize precondition"
        );
        assert!(
            post_delete_stats.freelist_count > 0,
            "deleting metrics did not leave free pages for compaction"
        );
        assert!(
            post_delete_used_bytes > TARGET,
            "protected live data uses only {post_delete_used_bytes} bytes; target is {TARGET}"
        );
        assert!(
            post_delete_used_bytes < LIMIT,
            "protected live data uses {post_delete_used_bytes} bytes and cannot fit below {LIMIT}"
        );
        let remaining_metrics: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM metrics_history")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(remaining_metrics, 0);

        db.pool.close().await;
        let oversized_file_bytes = std::fs::metadata(path).unwrap().len();
        assert!(
            oversized_file_bytes > LIMIT,
            "setup file length {oversized_file_bytes} did not exceed limit {LIMIT}"
        );

        let db = Database::new_with_max_size(path, LIMIT)
            .await
            .expect("oversized protected data with no metrics should be compacted, not rejected");

        let compacted_file_bytes = std::fs::metadata(path).unwrap().len();
        assert!(
            compacted_file_bytes <= LIMIT,
            "compacted file length {compacted_file_bytes} exceeds limit {LIMIT}"
        );
        let remaining_alerts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM alerts_log")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(remaining_alerts, alert_count);
        let remaining_nodes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM nodes_seen")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(remaining_nodes, 1);
        let remaining_node_id: String = sqlx::query_scalar("SELECT id FROM nodes_seen")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(remaining_node_id, "protected-node");
        let remaining_metrics: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM metrics_history")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(remaining_metrics, 0);
    }

    #[tokio::test]
    async fn full_database_evicts_oldest_and_keeps_latest_metric() {
        const LIMIT: u64 = 256 * 1024;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("full.db");
        let db = Database::new_with_max_size(path.to_str().unwrap(), LIMIT)
            .await
            .unwrap();
        let node_id = uuid::Uuid::new_v4();

        for second in 0..80 {
            let metrics = metrics_at(Utc.timestamp_opt(second, 0).unwrap(), 8 * 1024);
            db.store_metrics(&node_id, &metrics).await.unwrap();
        }

        let page_bytes = database_bytes(&db).await;
        assert!(
            page_bytes <= LIMIT,
            "database page bytes {page_bytes} exceed limit {LIMIT}"
        );
        let main_file_bytes = std::fs::metadata(&path).unwrap().len();
        assert!(
            main_file_bytes <= LIMIT,
            "main database file uses {main_file_bytes} bytes; limit is {LIMIT}"
        );

        let latest_timestamp: String = sqlx::query_scalar(
            "SELECT timestamp FROM metrics_history ORDER BY timestamp DESC, id DESC LIMIT 1",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        let expected_latest = Utc.timestamp_opt(79, 0).unwrap().to_rfc3339();
        assert_eq!(latest_timestamp, expected_latest);

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM metrics_history")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert!(count < 80, "the quota must evict at least one old metric");

        let oldest_timestamp = Utc.timestamp_opt(0, 0).unwrap().to_rfc3339();
        let oldest_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM metrics_history WHERE timestamp = ?")
                .bind(oldest_timestamp)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(oldest_count, 0, "the oldest metric must be evicted");
    }

    #[tokio::test]
    async fn file_pool_applies_max_page_count_to_every_connection() {
        const LIMIT: u64 = 256 * 1024;
        const POOL_SIZE: usize = 5;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pooled.db");
        let db = Database::new_with_max_size(path.to_str().unwrap(), LIMIT)
            .await
            .unwrap();

        let mut connections = Vec::with_capacity(POOL_SIZE);
        for _ in 0..POOL_SIZE {
            connections.push(db.pool.acquire().await.unwrap());
        }
        assert_eq!(connections.len(), POOL_SIZE);

        for (index, connection) in connections.iter_mut().enumerate() {
            let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
                .fetch_one(&mut **connection)
                .await
                .unwrap();
            let max_page_count: i64 = sqlx::query_scalar("PRAGMA max_page_count")
                .fetch_one(&mut **connection)
                .await
                .unwrap();
            let page_size = u64::try_from(page_size).unwrap();
            let max_page_count = u64::try_from(max_page_count).unwrap();
            assert!(page_size > 0, "connection {index} returned a zero page size");
            assert_eq!(
                max_page_count,
                LIMIT / page_size,
                "connection {index} has the wrong maximum page count"
            );
        }
    }
}
