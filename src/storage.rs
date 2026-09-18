use anyhow::{anyhow, Result};
use sqlx::{sqlite::SqlitePoolOptions, SqlitePool};
use std::time::Duration;
use chrono::Utc;
use tracing::{info, warn};

use crate::types::*;

const MAX_DATABASE_BYTES: u64 = 500 * 1024 * 1024;
const SHRINK_TARGET_RATIO: u64 = 90;
const EVICTION_BATCH_SIZE: i64 = 100;

pub struct Database {
    pool: SqlitePool,
    max_bytes: u64,
}

impl Database {
    pub async fn new(db_path: &str) -> Result<Self> {
        Self::new_with_limit(db_path, MAX_DATABASE_BYTES).await
    }

    async fn new_with_limit(db_path: &str, max_bytes: u64) -> Result<Self> {
        Self::open_with_limit(db_path, max_bytes).await
    }

    async fn open_with_limit(db_path: &str, max_bytes: u64) -> Result<Self> {
        let url = if db_path == ":memory:" {
            "sqlite::memory:".to_string()
        } else {
            format!("sqlite://{}?mode=rwc", db_path)
        };
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .idle_timeout(Duration::from_secs(30))
            .acquire_timeout(Duration::from_secs(5))
            .connect(&url)
            .await?;
        let db = Self { pool, max_bytes };
        db.run_migrations().await?;
        db.enforce_capacity().await?;
        info!("Database initialized at {}", db_path);
        Ok(db)
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
                id TEXT PRIMARY KEY, node_id TEXT NOT NULL, rule_name TEXT NOT NULL,
                severity TEXT NOT NULL, message TEXT NOT NULL, triggered_at TEXT NOT NULL,
                resolved_at TEXT, value REAL NOT NULL, threshold REAL NOT NULL
            );
            CREATE TABLE IF NOT EXISTS nodes_seen (
                id TEXT PRIMARY KEY, hostname TEXT NOT NULL, api_addr TEXT NOT NULL,
                gossip_addr TEXT NOT NULL, last_seen TEXT NOT NULL, version TEXT NOT NULL
            );
        "#)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn page_stats(&self) -> Result<(u64, u64, u64)> {
        let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
            .fetch_one(&self.pool).await?;
        let page_count: i64 = sqlx::query_scalar("PRAGMA page_count")
            .fetch_one(&self.pool).await?;
        let freelist_count: i64 = sqlx::query_scalar("PRAGMA freelist_count")
            .fetch_one(&self.pool).await?;
        Ok((page_size as u64, page_count as u64, freelist_count as u64))
    }

    async fn set_max_page_count(&self, page_size: u64) -> Result<()> {
        let max_pages = (self.max_bytes / page_size).max(1);
        let actual: i64 = sqlx::query_scalar(&format!("PRAGMA max_page_count = {max_pages}"))
            .fetch_one(&self.pool).await?;
        if actual as u64 > max_pages {
            return Err(anyhow!("SQLite max_page_count exceeds configured limit"));
        }
        Ok(())
    }

    async fn enforce_capacity(&self) -> Result<()> {
        let (page_size, page_count, freelist) = self.page_stats().await?;
        let file_bytes = page_size.saturating_mul(page_count);
        if file_bytes > self.max_bytes {
            let target = self.max_bytes.saturating_mul(SHRINK_TARGET_RATIO) / 100;
            let mut removed = 0;
            loop {
                let (_, pages, free) = self.page_stats().await?;
                if pages.saturating_sub(free).saturating_mul(page_size) <= target {
                    break;
                }
                let n = self.evict_oldest(EVICTION_BATCH_SIZE).await?;
                if n == 0 {
                    break;
                }
                removed += n;
            }
            sqlx::query("VACUUM").execute(&self.pool).await?;
            let (new_page_size, pages, _) = self.page_stats().await?;
            if new_page_size.saturating_mul(pages) > self.max_bytes {
                return Err(anyhow!("database remains above the {} byte limit after startup shrink", self.max_bytes));
            }
            warn!("Shrank over-limit database by removing {} metric records", removed);
            self.set_max_page_count(new_page_size).await?;
        } else {
            self.set_max_page_count(page_size).await?;
        }
        let _ = freelist;
        Ok(())
    }

    async fn evict_oldest(&self, limit: i64) -> Result<u64> {
        let result = sqlx::query("DELETE FROM metrics_history WHERE id IN (SELECT id FROM metrics_history ORDER BY timestamp ASC, id ASC LIMIT ?)")
            .bind(limit).execute(&self.pool).await?;
        Ok(result.rows_affected())
    }

    async fn insert_metrics(&self, node_id: &NodeId, metrics: &SystemMetrics, raw: &str) -> sqlx::Result<()> {
        sqlx::query(r#"INSERT INTO metrics_history
            (node_id, hostname, timestamp, cpu_usage, memory_usage,
             memory_used_bytes, memory_total_bytes, uptime_seconds, raw_json)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"#)
            .bind(node_id.to_string()).bind(&metrics.hostname)
            .bind(metrics.timestamp.to_rfc3339())
            .bind(metrics.cpu.usage_percent as f64).bind(metrics.memory.usage_percent as f64)
            .bind(metrics.memory.used_bytes as i64).bind(metrics.memory.total_bytes as i64)
            .bind(metrics.uptime_seconds as i64).bind(raw).execute(&self.pool).await
            .map(|_| ())
    }

    pub async fn store_metrics(&self, node_id: &NodeId, metrics: &SystemMetrics) -> Result<()> {
        let raw = serde_json::to_string(metrics)?;
        loop {
            match self.insert_metrics(node_id, metrics, &raw).await {
                Ok(_) => return Ok(()),
                Err(error) if is_sqlite_full(&error) => {
                    let removed = self.evict_oldest(EVICTION_BATCH_SIZE).await?;
                    if removed == 0 {
                        return Err(anyhow!(error));
                    }
                    warn!("Database reached capacity; evicted {} oldest metric records", removed);
                }
                Err(error) => return Err(anyhow!(error)),
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

fn is_sqlite_full(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(|database_error| database_error.code())
        .is_some_and(|code| code == "13")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration as ChronoDuration, Utc};
    use tempfile::tempdir;
    use uuid::Uuid;

    fn metrics(timestamp: chrono::DateTime<Utc>, payload_size: usize) -> SystemMetrics {
        SystemMetrics {
            timestamp,
            cpu: CpuMetrics { usage_percent: 10.0, core_usages: vec![10.0], core_count: 1 },
            memory: MemoryMetrics {
                total_bytes: 1024, used_bytes: 128, available_bytes: 896,
                usage_percent: 12.5, swap_total_bytes: 0, swap_used_bytes: 0,
            },
            disks: vec![], physical_disks: vec![], networks: vec![], load_average: None,
            top_processes: vec![ProcessInfo {
                pid: 1, name: "x".repeat(payload_size), cpu_usage: 1.0, memory_bytes: 1,
                status: "Run".to_string(), disk_read_bps: 0.0, disk_write_bps: 0.0,
            }],
            uptime_seconds: 1, os_name: "test".to_string(), hostname: "test".to_string(),
        }
    }

    async fn history_count(db: &Database) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM metrics_history")
            .fetch_one(&db.pool).await.expect("count should succeed")
    }

    #[tokio::test]
    async fn capacity_eviction_uses_global_oldest_order() {
        let dir = tempdir().expect("temp directory should be created");
        let db = Database::new_with_limit(
            dir.path().join("history.db").to_str().unwrap(),
            128 * 1024,
        )
        .await
        .expect("database should initialize");
        let node_id = Uuid::new_v4();
        for offset in 0..8 {
            db.store_metrics(&node_id, &metrics(Utc::now() - ChronoDuration::seconds(8 - offset), 10_000))
                .await.expect("metrics should be stored");
        }
        let newest = metrics(Utc::now(), 10_000);
        db.store_metrics(&node_id, &newest).await.expect("latest metrics should be stored after eviction");
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT timestamp FROM metrics_history ORDER BY timestamp ASC, id ASC",
        )
            .fetch_all(&db.pool)
            .await
            .expect("history should be readable");
        assert!(rows.len() < 9);
        assert!(rows.iter().any(|(timestamp,)| timestamp == &newest.timestamp.to_rfc3339()));
    }

    #[tokio::test]
    async fn startup_shrink_keeps_newest_history_and_protected_tables() {
        let dir = tempdir().expect("temp directory should be created");
        let path = dir.path().join("history.db");
        let node_id = Uuid::new_v4();
        {
            let db = Database::new_with_limit(path.to_str().unwrap(), 128 * 1024)
                .await.expect("database should initialize");
            for offset in 0..12 {
                db.store_metrics(&node_id, &metrics(Utc::now() - ChronoDuration::seconds(12 - offset), 4_000))
                    .await.expect("metrics should be stored");
            }
            sqlx::query("INSERT INTO nodes_seen (id, hostname, api_addr, gossip_addr, last_seen, version) VALUES (?, ?, ?, ?, ?, ?)")
                .bind(node_id.to_string()).bind("protected").bind("127.0.0.1:1").bind("127.0.0.1:2")
                .bind(Utc::now().to_rfc3339()).bind("test").execute(&db.pool).await
                .expect("protected row should be stored");
        }
        let db = Database::new_with_limit(path.to_str().unwrap(), 128 * 1024)
            .await.expect("over-limit database should shrink on startup");
        assert!(history_count(&db).await > 0);
        assert_eq!(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM nodes_seen WHERE hostname = 'protected'")
            .fetch_one(&db.pool).await.expect("protected row should remain"), 1);
        let page_size: i64 = sqlx::query_scalar("PRAGMA page_size").fetch_one(&db.pool).await.unwrap();
        let page_count: i64 = sqlx::query_scalar("PRAGMA page_count").fetch_one(&db.pool).await.unwrap();
        assert!((page_size * page_count) as u64 <= 128 * 1024);
    }

    #[tokio::test]
    async fn cleanup_removes_only_expired_metrics() {
        let dir = tempdir().expect("temp directory should be created");
        let db = Database::new_with_limit(dir.path().join("history.db").to_str().unwrap(), 128 * 1024)
            .await.expect("database should initialize");
        let node_id = Uuid::new_v4();
        db.store_metrics(&node_id, &metrics(Utc::now() - ChronoDuration::hours(2), 100)).await.unwrap();
        db.store_metrics(&node_id, &metrics(Utc::now(), 100)).await.unwrap();
        assert_eq!(db.cleanup_old_metrics(1).await.unwrap(), 1);
        assert_eq!(history_count(&db).await, 1);
    }
}
