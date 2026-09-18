use anyhow::{anyhow, Result};
use sqlx::{sqlite::SqlitePoolOptions, SqlitePool};
use std::time::Duration;
use chrono::{DateTime, Utc};
use tracing::{info, warn};
use uuid::Uuid;

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
        // One-time rebuild of the pre-0.3 `alerts_log` shape: that table lacked
        // hostname/metric/target/operator and had no writer (its only insert path
        // was unreachable), so dropping it loses nothing. Once the new columns are
        // present the table is left alone, so alert history survives restarts.
        if self.table_exists("alerts_log").await? {
            let columns: Vec<(String,)> =
                sqlx::query_as("SELECT name FROM pragma_table_info('alerts_log')")
                    .fetch_all(&self.pool)
                    .await?;
            if !columns.iter().any(|(name,)| name == "hostname") {
                sqlx::query("DROP TABLE alerts_log").execute(&self.pool).await?;
                warn!("Rebuilt alerts_log with the extended alert schema");
            }
        }

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
            CREATE TABLE IF NOT EXISTS nodes_seen (
                id TEXT PRIMARY KEY, hostname TEXT NOT NULL, api_addr TEXT NOT NULL,
                gossip_addr TEXT NOT NULL, last_seen TEXT NOT NULL, version TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS alerts_log (
                id TEXT PRIMARY KEY, node_id TEXT NOT NULL, hostname TEXT NOT NULL,
                rule_name TEXT NOT NULL, metric TEXT NOT NULL, target TEXT,
                operator TEXT NOT NULL, severity TEXT NOT NULL, message TEXT NOT NULL,
                value REAL NOT NULL, threshold REAL NOT NULL,
                triggered_at TEXT NOT NULL, resolved_at TEXT, resolved_reason TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_alerts_triggered_at ON alerts_log(triggered_at DESC);
            CREATE TABLE IF NOT EXISTS notify_channels (
                id TEXT PRIMARY KEY, name TEXT NOT NULL, enabled INTEGER NOT NULL,
                min_severity TEXT NOT NULL, config_json TEXT NOT NULL,
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                last_sent_at TEXT, last_error TEXT
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

    /// Persist a newly triggered (or restored) alert.
    pub async fn store_alert(&self, alert: &Alert) -> Result<()> {
        sqlx::query(r#"
            INSERT OR REPLACE INTO alerts_log
                (id, node_id, hostname, rule_name, metric, target, operator, severity,
                 message, value, threshold, triggered_at, resolved_at, resolved_reason)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#)
        .bind(alert.id.to_string())
        .bind(alert.node_id.to_string())
        .bind(&alert.hostname)
        .bind(&alert.rule_name)
        .bind(&alert.metric)
        .bind(alert.target.as_deref())
        .bind(&alert.operator)
        .bind(alert_severity_str(alert.severity))
        .bind(&alert.message)
        .bind(alert.value)
        .bind(alert.threshold)
        .bind(alert.triggered_at.to_rfc3339())
        .bind(alert.resolved_at.map(|t| t.to_rfc3339()))
        .bind(alert.resolved_reason.map(resolve_reason_str))
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Record that an alert has been resolved.
    pub async fn mark_alert_resolved(&self, alert: &Alert) -> Result<()> {
        sqlx::query("UPDATE alerts_log SET resolved_at = ?, resolved_reason = ? WHERE id = ?")
            .bind(alert.resolved_at.map(|t| t.to_rfc3339()))
            .bind(alert.resolved_reason.map(resolve_reason_str))
            .bind(alert.id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Alerts that were still active when the process last stopped.
    pub async fn load_active_alerts(&self) -> Result<Vec<Alert>> {
        let rows: Vec<AlertRow> = sqlx::query_as(
            "SELECT id, node_id, hostname, rule_name, metric, target, operator, severity,
                    message, value, threshold, triggered_at, resolved_at, resolved_reason
             FROM alerts_log WHERE resolved_at IS NULL ORDER BY triggered_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;

        let alerts: Vec<Alert> = rows.into_iter().filter_map(alert_from_row).collect();
        Ok(alerts)
    }

    /// Recently resolved alerts, newest first.
    pub async fn recent_resolved_alerts(&self, limit: i64) -> Result<Vec<Alert>> {
        let rows: Vec<AlertRow> = sqlx::query_as(
            "SELECT id, node_id, hostname, rule_name, metric, target, operator, severity,
                    message, value, threshold, triggered_at, resolved_at, resolved_reason
             FROM alerts_log WHERE resolved_at IS NOT NULL
             ORDER BY resolved_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        let alerts: Vec<Alert> = rows.into_iter().filter_map(alert_from_row).collect();
        Ok(alerts)
    }

    /// Delete resolved alerts older than `cutoff`.
    ///
    /// The panel shows 最近恢复 straight from this table, so expired rows must
    /// leave the database too — otherwise they reappear in the history after a
    /// restart even though the in-memory copy was pruned.
    pub async fn purge_resolved_alerts(&self, cutoff: DateTime<Utc>) -> Result<u64> {
        let cutoff_str = cutoff.to_rfc3339();
        let result = sqlx::query(
            "DELETE FROM alerts_log WHERE resolved_at IS NOT NULL AND resolved_at < ?",
        )
        .bind(&cutoff_str)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected())
    }

    /// All configured push channels, oldest first.
    pub async fn list_notify_channels(&self) -> Result<Vec<NotifyChannel>> {
        let rows: Vec<ChannelRow> = sqlx::query_as(
                "SELECT id, name, enabled, min_severity, config_json, created_at, updated_at,
                        last_sent_at, last_error
                 FROM notify_channels ORDER BY created_at ASC",
            )
            .fetch_all(&self.pool)
            .await?;

        Ok(rows.into_iter().filter_map(channel_from_row).collect())
    }

    pub async fn get_notify_channel(&self, id: &Uuid) -> Result<Option<NotifyChannel>> {
        let row: Option<ChannelRow> = sqlx::query_as(
                "SELECT id, name, enabled, min_severity, config_json, created_at, updated_at,
                        last_sent_at, last_error
                 FROM notify_channels WHERE id = ?",
            )
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;

        Ok(row.and_then(channel_from_row))
    }

    pub async fn upsert_notify_channel(&self, channel: &NotifyChannel) -> Result<()> {
        let config_json = serde_json::to_string(&channel.config)?;
        sqlx::query(r#"
            INSERT OR REPLACE INTO notify_channels
                (id, name, enabled, min_severity, config_json, created_at, updated_at,
                 last_sent_at, last_error)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#)
        .bind(channel.id.to_string())
        .bind(&channel.name)
        .bind(channel.enabled as i64)
        .bind(channel_severity_str(channel.min_severity))
        .bind(&config_json)
        .bind(channel.created_at.to_rfc3339())
        .bind(channel.updated_at.to_rfc3339())
        .bind(channel.last_sent_at.map(|t| t.to_rfc3339()))
        .bind(channel.last_error.as_deref())
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Delete a channel, returning the number of affected rows.
    pub async fn delete_notify_channel(&self, id: &Uuid) -> Result<u64> {
        let result = sqlx::query("DELETE FROM notify_channels WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    /// Record the outcome of a push attempt: `None` marks a successful send
    /// (stamping `last_sent_at` and clearing the error), `Some` stores the error.
    pub async fn record_notify_result(&self, id: &Uuid, error: Option<&str>) -> Result<()> {
        match error {
            None => {
                sqlx::query("UPDATE notify_channels SET last_sent_at = ?, last_error = NULL WHERE id = ?")
                    .bind(Utc::now().to_rfc3339())
                    .bind(id.to_string())
                    .execute(&self.pool)
                    .await?;
            }
            Some(message) => {
                let truncated: String = message.chars().take(500).collect();
                sqlx::query("UPDATE notify_channels SET last_error = ? WHERE id = ?")
                    .bind(truncated)
                    .bind(id.to_string())
                    .execute(&self.pool)
                    .await?;
            }
        }
        Ok(())
    }

    /// Whether a table exists in the schema.
    async fn table_exists(&self, name: &str) -> Result<bool> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
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

/// Column tuple of an `alerts_log` row.
type AlertRow = (
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    String,
    String,
    f64,
    f64,
    String,
    Option<String>,
    Option<String>,
);

/// Column tuple of a `notify_channels` row.
type ChannelRow = (
    String,
    String,
    i64,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
);

fn alert_from_row(row: AlertRow) -> Option<Alert> {
    let (
        id,
        node_id,
        hostname,
        rule_name,
        metric,
        target,
        operator,
        severity,
        message,
        value,
        threshold,
        triggered_at,
        resolved_at,
        resolved_reason,
    ) = row;

    Some(Alert {
        id: Uuid::parse_str(&id).ok()?,
        node_id: Uuid::parse_str(&node_id).ok()?,
        hostname,
        rule_name,
        metric,
        target,
        operator,
        severity: alert_severity_from(&severity),
        message,
        triggered_at: parse_time(&triggered_at)?,
        resolved_at: resolved_at.as_deref().and_then(parse_time),
        resolved_reason: resolved_reason.as_deref().and_then(resolve_reason_from),
        value,
        threshold,
    })
}

fn channel_from_row(row: ChannelRow) -> Option<NotifyChannel> {
    let (id, name, enabled, min_severity, config_json, created_at, updated_at, last_sent_at, last_error) =
        row;

    let config: ChannelConfig = serde_json::from_str(&config_json).ok()?;

    Some(NotifyChannel {
        id: Uuid::parse_str(&id).ok()?,
        name,
        enabled: enabled != 0,
        min_severity: channel_severity_from(&min_severity),
        config,
        created_at: parse_time(&created_at)?,
        updated_at: parse_time(&updated_at)?,
        last_sent_at: last_sent_at.as_deref().and_then(parse_time),
        last_error,
    })
}

fn parse_time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

/// Severity as stored in SQLite. Deliberately explicit instead of `{:?}` or the
/// serde representation, so a later rename of either cannot silently break reads.
fn alert_severity_str(severity: AlertSeverity) -> &'static str {
    match severity {
        AlertSeverity::Info => "info",
        AlertSeverity::Warning => "warning",
        AlertSeverity::Critical => "critical",
    }
}

fn alert_severity_from(value: &str) -> AlertSeverity {
    match value {
        "info" => AlertSeverity::Info,
        "critical" => AlertSeverity::Critical,
        _ => AlertSeverity::Warning,
    }
}

fn resolve_reason_str(reason: AlertResolveReason) -> &'static str {
    match reason {
        AlertResolveReason::ConditionCleared => "condition_cleared",
        AlertResolveReason::NodeOffline => "node_offline",
        AlertResolveReason::NodeRemoved => "node_removed",
        AlertResolveReason::MetricUnavailable => "metric_unavailable",
    }
}

fn resolve_reason_from(value: &str) -> Option<AlertResolveReason> {
    match value {
        "condition_cleared" => Some(AlertResolveReason::ConditionCleared),
        "node_offline" => Some(AlertResolveReason::NodeOffline),
        "node_removed" => Some(AlertResolveReason::NodeRemoved),
        "metric_unavailable" => Some(AlertResolveReason::MetricUnavailable),
        _ => None,
    }
}

fn channel_severity_str(severity: NotifySeverity) -> &'static str {
    match severity {
        NotifySeverity::Info => "info",
        NotifySeverity::Warning => "warning",
        NotifySeverity::Critical => "critical",
    }
}

fn channel_severity_from(value: &str) -> NotifySeverity {
    match value {
        "info" => NotifySeverity::Info,
        "critical" => NotifySeverity::Critical,
        _ => NotifySeverity::Warning,
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

    fn alert(rule_name: &str, resolved_at: Option<chrono::DateTime<Utc>>) -> Alert {
        Alert {
            id: Uuid::new_v4(),
            node_id: Uuid::new_v4(),
            hostname: "test".to_string(),
            rule_name: rule_name.to_string(),
            metric: "cpu".to_string(),
            target: None,
            operator: "gt".to_string(),
            severity: AlertSeverity::Warning,
            message: "test".to_string(),
            triggered_at: Utc::now() - ChronoDuration::minutes(30),
            resolved_at,
            resolved_reason: resolved_at.map(|_| AlertResolveReason::ConditionCleared),
            value: 95.0,
            threshold: 90.0,
        }
    }

    #[tokio::test]
    async fn purge_removes_only_expired_resolved_alerts() {
        let dir = tempdir().expect("temp directory should be created");
        let db = Database::new_with_limit(dir.path().join("alerts.db").to_str().unwrap(), 128 * 1024)
            .await.expect("database should initialize");

        let mut expired = alert("expired", None);
        let mut recent = alert("recent", None);
        let active = alert("active", None);
        for entry in [&expired, &recent, &active] {
            db.store_alert(entry).await.expect("alert should be stored");
        }
        // Resolved rows are written by `mark_alert_resolved`, exactly as at runtime.
        expired.resolved_at = Some(Utc::now() - ChronoDuration::minutes(11));
        expired.resolved_reason = Some(AlertResolveReason::ConditionCleared);
        recent.resolved_at = Some(Utc::now() - ChronoDuration::minutes(2));
        recent.resolved_reason = Some(AlertResolveReason::ConditionCleared);
        db.mark_alert_resolved(&expired).await.unwrap();
        db.mark_alert_resolved(&recent).await.unwrap();

        let cutoff = Utc::now() - ChronoDuration::minutes(10);
        assert_eq!(db.purge_resolved_alerts(cutoff).await.unwrap(), 1);
        // Running again is a no-op: the boundary row stays until it expires too.
        assert_eq!(db.purge_resolved_alerts(cutoff).await.unwrap(), 0);

        let remaining: Vec<(String,)> =
            sqlx::query_as("SELECT rule_name FROM alerts_log ORDER BY rule_name")
                .fetch_all(&db.pool)
                .await
                .expect("alerts should be readable");
        assert_eq!(
            remaining.into_iter().map(|(name,)| name).collect::<Vec<_>>(),
            vec!["active".to_string(), "recent".to_string()],
            "an unresolved alert must survive the purge"
        );
    }
}
