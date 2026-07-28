use anyhow::Result;
use sqlx::{SqlitePool, sqlite::SqlitePoolOptions};
use chrono::Utc;
use tracing::info;

use crate::types::*;

pub struct Database {
    pool: SqlitePool,
}

impl Database {
    pub async fn new(db_path: &str) -> Result<Self> {
        // SQLite connection string
        let url = if db_path == ":memory:" {
            "sqlite::memory:".to_string()
        } else {
            format!("sqlite://{}?mode=rwc", db_path)
        };

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await?;

        let db = Self { pool };
        db.run_migrations().await?;
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
