use anyhow::{anyhow, ensure, Result};
use sqlx::{sqlite::SqlitePoolOptions, SqliteConnection, SqlitePool};
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
        let max_connections = if db_path == ":memory:" { 1 } else { 5 };

        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
            // Release idle connections after 30 s so they don't accumulate.
            .idle_timeout(Duration::from_secs(30))
            // If all connections are busy, fail fast rather than blocking
            // indefinitely — the caller logs the error and moves on.
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
        };
        db.run_migrations().await?;
        db.enforce_startup_size_limit().await?;
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
            warn!(
                "Startup database compaction deleted {deleted_before_compaction} metric records before the first VACUUM and {deleted_after_compaction} after it"
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
        let node_id_str = node_id.to_string();
        let ts = metrics.timestamp.to_rfc3339();
        let raw = serde_json::to_string(metrics)?;

        sqlx::query(r#"
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
        .await?;

        Ok(())
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
    use tempfile::TempDir;

    async fn temp_database() -> (TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.db");
        let db = Database::new(path.to_str().unwrap()).await.unwrap();
        (dir, db)
    }

    async fn insert_raw_metric(db: &Database, timestamp: &str, hostname: &str) {
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
        let remaining_metrics: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM metrics_history")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(remaining_metrics, 0);
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
