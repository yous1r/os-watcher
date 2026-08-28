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
        if original.file_bytes()? > self.max_size_bytes {
            let target_bytes = self
                .max_size_bytes
                .checked_mul(9)
                .ok_or_else(|| anyhow!("database shrink target overflow"))?
                / 10;
            let mut deleted = 0_u64;
            let mut stats = original;

            loop {
                let used_bytes = stats.used_bytes()?;
                if used_bytes <= target_bytes {
                    break;
                }

                let metric_count = self.metrics_history_count().await?;
                ensure!(
                    metric_count > 0,
                    "database exceeds its startup shrink target but has no metric history to evict"
                );

                // Keep the newest record when protected data plus that record
                // already fits the hard cap, even if it cannot provide 10% headroom.
                if metric_count == 1 && used_bytes <= self.max_size_bytes {
                    break;
                }

                let batch_size = (metric_count / 10).max(1).min(EVICTION_BATCH_SIZE);
                let batch = self.delete_oldest_metrics_batch(batch_size).await?;
                ensure!(
                    batch > 0,
                    "database exceeds its startup shrink target but has no metric history to evict"
                );
                deleted = deleted
                    .checked_add(batch)
                    .ok_or_else(|| anyhow!("evicted metric count overflow"))?;
                stats = self.page_stats().await?;
            }

            sqlx::query("VACUUM").execute(&self.pool).await?;
            warn!(
                "Evicted {} old metric records while shrinking database",
                deleted
            );
        }

        let applied = {
            let mut connection = self.pool.acquire().await?;
            apply_max_page_count(&mut connection, self.max_size_bytes).await?
        };
        let final_stats = self.page_stats().await?;
        let allowed_pages = self.max_size_bytes / final_stats.page_size;
        ensure!(
            applied <= allowed_pages,
            "failed to apply database page limit: SQLite allowed {applied} pages, cap allows {allowed_pages}"
        );
        ensure!(
            final_stats.page_count <= applied,
            "database page count exceeds its applied page limit"
        );
        ensure!(
            final_stats.file_bytes()? <= self.max_size_bytes,
            "database remains above size limit after metric eviction; protected data cannot fit"
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

        let db = Database::new_with_max_size(path, LIMIT).await.unwrap();

        assert!(database_bytes(&db).await <= LIMIT);
        let newest: String = sqlx::query_scalar(
            "SELECT hostname FROM metrics_history ORDER BY timestamp DESC, id DESC LIMIT 1",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert!(newest.starts_with("079-"));
    }
}
