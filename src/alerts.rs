use std::collections::HashSet;

use crate::config::AlertRule;
use crate::notify::{AlertEvent, NotificationService};
use crate::state::SharedState;
use crate::storage::Database;
use crate::types::*;
use chrono::{Duration as ChronoDuration, Utc};
use tracing::{info, warn};
use uuid::Uuid;

/// A node that has not reported within this many seconds is treated as gone for
/// alerting purposes. Gossip marks peers offline after 120s; this is the backstop
/// for a node whose `last_seen` stopped advancing without an explicit status change.
const ALERT_STALE_SECS: i64 = 180;

/// Evaluate alert rules against the current metrics for ALL known nodes.
///
/// Only nodes that are currently reporting are evaluated: an offline or vanished
/// node cannot keep a condition violated with its last sample, so its alerts are
/// resolved with an explicit reason instead of hanging forever.
///
/// `resolved_retention_minutes` is `[storage] alert_history_minutes`: resolved
/// alerts older than that window are dropped from memory and from `alerts_log`,
/// which is what the panel's 最近恢复 list reads.
pub async fn evaluate_alerts(
    state: &SharedState,
    rules: &[AlertRule],
    db: &Database,
    notifier: &NotificationService,
    resolved_retention_minutes: u64,
) {
    let (evaluable, offline_ids, removed_ids, active_keys, mut new_counts) = {
        let s = state.read().await;
        let local_id = s.local_node.id;

        let mut evaluable: Vec<(NodeId, String, SystemMetrics)> = vec![];
        let mut evaluable_ids: HashSet<NodeId> = HashSet::new();
        for (node_id, metrics) in s.metrics.iter() {
            let alive = *node_id == local_id
                || s.peers.get(node_id).is_some_and(|peer| {
                    peer.status != NodeStatus::Offline && !s.is_stale(node_id, ALERT_STALE_SECS)
                });
            if !alive {
                continue;
            }

            let hostname = s
                .peers
                .get(node_id)
                .map(|peer| peer.hostname.clone())
                .filter(|hostname| !hostname.is_empty())
                .or_else(|| (!metrics.hostname.is_empty()).then(|| metrics.hostname.clone()))
                .unwrap_or_else(|| node_id.simple().to_string()[..8].to_string());

            evaluable_ids.insert(*node_id);
            evaluable.push((*node_id, hostname, metrics.clone()));
        }

        // Anything with metrics or active alerts that is no longer reporting must
        // not keep its alerts open. A node still in the mesh is "offline"; one that
        // disappeared entirely (e.g. re-registered with a new UUID) is "removed".
        let mut offline_ids: Vec<NodeId> = vec![];
        let mut removed_ids: Vec<NodeId> = vec![];
        let mut seen: HashSet<NodeId> = HashSet::new();
        let candidates = s.metrics.keys().copied().chain(
            s.alerts
                .iter()
                .filter(|a| a.resolved_at.is_none())
                .map(|a| a.node_id),
        );
        for node_id in candidates {
            if node_id == local_id || !seen.insert(node_id) {
                continue;
            }
            if s.peers.contains_key(&node_id) {
                if !evaluable_ids.contains(&node_id) {
                    offline_ids.push(node_id);
                }
            } else {
                removed_ids.push(node_id);
            }
        }

        let mut counts = s.violation_counts.clone();
        for node_id in offline_ids.iter().chain(removed_ids.iter()) {
            counts.remove(node_id);
        }

        let active_keys: HashSet<(NodeId, String)> = s
            .alerts
            .iter()
            .filter(|a| a.resolved_at.is_none())
            .map(|a| (a.node_id, a.rule_name.clone()))
            .collect();

        (evaluable, offline_ids, removed_ids, active_keys, counts)
    };

    let mut triggered: Vec<Alert> = vec![];
    let mut resolutions: Vec<(NodeId, String, AlertResolveReason)> = vec![];

    for (node_id, hostname, metrics) in &evaluable {
        let node_counts = new_counts.entry(*node_id).or_default();

        for rule in rules {
            // Per-node, per-rule violation counter, keyed by rule name.
            let count = node_counts.entry(rule.name.clone()).or_insert(0);

            let sample = match extract_metric_sample(metrics, &rule.metric, rule.target.as_deref()) {
                Some(sample) => sample,
                None => {
                    // The rule's device/metric is gone from the report: resolve
                    // instead of leaving the alert open forever.
                    if *count > 0 {
                        resolutions.push((
                            *node_id,
                            rule.name.clone(),
                            AlertResolveReason::MetricUnavailable,
                        ));
                    }
                    *count = 0;
                    continue;
                }
            };

            if !evaluate_condition(sample.value, &rule.operator, rule.threshold) {
                if *count > 0 {
                    resolutions.push((
                        *node_id,
                        rule.name.clone(),
                        AlertResolveReason::ConditionCleared,
                    ));
                }
                *count = 0;
                continue;
            }

            *count += 1;
            if *count < rule.consecutive_violations {
                continue;
            }

            // Already alerting for this node+rule: nothing to do until it resolves.
            if active_keys.contains(&(*node_id, rule.name.clone())) {
                continue;
            }

            warn!(
                "Alert triggered: {} on {} - {} = {:.2} {} {:.2}",
                rule.name, hostname, rule.metric, sample.value, rule.operator, rule.threshold
            );

            triggered.push(Alert {
                id: Uuid::new_v4(),
                node_id: *node_id,
                hostname: hostname.clone(),
                rule_name: rule.name.clone(),
                metric: rule.metric.clone(),
                target: sample.target.clone(),
                operator: rule.operator.clone(),
                severity: rule_severity(&rule.severity),
                message: render_message(rule, &sample, hostname),
                triggered_at: Utc::now(),
                resolved_at: None,
                resolved_reason: None,
                value: sample.value,
                threshold: rule.threshold,
            });
        }
    }

    // Same cutoff for both stores, so a restart cannot resurrect expired rows.
    // The window is clamped to a year so a nonsensical config value cannot
    // overflow the duration arithmetic.
    let window = ChronoDuration::minutes(resolved_retention_minutes.min(60 * 24 * 365) as i64);
    let resolved_cutoff = Utc::now() - window;

    // Apply state changes in a single write lock, then persist and push outside it.
    let mut written: Vec<Alert> = vec![];
    let mut resolved_alerts: Vec<Alert> = vec![];
    {
        let mut s = state.write().await;
        s.violation_counts = new_counts;

        for node_id in offline_ids {
            resolved_alerts.extend(s.resolve_alerts_of_node(&node_id, AlertResolveReason::NodeOffline));
        }
        for node_id in removed_ids {
            resolved_alerts.extend(s.resolve_alerts_of_node(&node_id, AlertResolveReason::NodeRemoved));
        }
        for (node_id, rule_name, reason) in resolutions {
            resolved_alerts.extend(s.resolve_alert(&node_id, &rule_name, reason));
        }

        for alert in triggered {
            if s.add_alert(alert.clone()) {
                written.push(alert);
            }
        }

        s.prune_resolved_alerts(resolved_cutoff);
    }

    // Drop expired rows from the history as well: the panel reads 最近恢复 from
    // the database, so pruning only the in-memory copy would let them reappear.
    match db.purge_resolved_alerts(resolved_cutoff).await {
        Ok(0) => {}
        Ok(count) => info!("Purged {count} expired resolved alert(s)"),
        Err(error) => warn!("Failed to purge expired resolved alerts: {error}"),
    }

    for alert in &resolved_alerts {
        info!(
            "Alert resolved: {} on {} ({})",
            alert.rule_name,
            alert.hostname,
            alert.resolved_reason.map(|reason| reason.label()).unwrap_or("")
        );
        if let Err(error) = db.mark_alert_resolved(alert).await {
            warn!("Failed to persist resolved alert {}: {error}", alert.id);
        }
    }
    for alert in resolved_alerts {
        notifier.dispatch_alert(alert, AlertEvent::Resolved);
    }

    for alert in &written {
        if let Err(error) = db.store_alert(alert).await {
            warn!("Failed to persist alert {}: {error}", alert.id);
        }
    }
    for alert in written {
        notifier.dispatch_alert(alert, AlertEvent::Triggered);
    }
}

/// A metric observation together with the device identity it came from.
struct MetricSample {
    value: f64,
    /// Chinese metric name, used in messages, the panel and push notifications.
    label: &'static str,
    /// Unit suffix for the value, empty for unitless metrics.
    unit: &'static str,
    /// Device or mount point the value belongs to, when the metric has one.
    target: Option<String>,
}

/// Extract a metric sample given the rule's metric name and optional target.
fn extract_metric_sample(
    metrics: &SystemMetrics,
    metric: &str,
    target: Option<&str>,
) -> Option<MetricSample> {
    let sample = |value: f64, label: &'static str, unit: &'static str, target: Option<String>| {
        MetricSample {
            value,
            label,
            unit,
            target,
        }
    };

    match metric {
        "cpu" => Some(sample(
            metrics.cpu.usage_percent as f64,
            "CPU 使用率",
            "%",
            None,
        )),
        "memory" => Some(sample(
            metrics.memory.usage_percent as f64,
            "内存使用率",
            "%",
            None,
        )),
        "swap" => {
            if metrics.memory.swap_total_bytes == 0 {
                return None;
            }
            Some(sample(
                (metrics.memory.swap_used_bytes as f64 / metrics.memory.swap_total_bytes as f64)
                    * 100.0,
                "Swap 使用率",
                "%",
                None,
            ))
        }
        "disk" => {
            // Always name the device: "a disk is full" is useless without knowing which.
            let disk = match target {
                Some(target) => metrics
                    .disks
                    .iter()
                    .find(|disk| disk.mount_point == target || disk.name == target)?,
                None => metrics.disks.iter().max_by(|a, b| {
                    a.usage_percent
                        .partial_cmp(&b.usage_percent)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })?,
            };

            let name = if disk.mount_point.is_empty() {
                disk.name.clone()
            } else {
                disk.mount_point.clone()
            };

            Some(sample(
                disk.usage_percent as f64,
                "磁盘使用率",
                "%",
                Some(name),
            ))
        }
        "load1" => metrics.load_average.as_ref().map(|la| {
            sample(la.one, "1 分钟负载", "", None)
        }),
        "load5" => metrics.load_average.as_ref().map(|la| {
            sample(la.five, "5 分钟负载", "", None)
        }),
        "load15" => metrics.load_average.as_ref().map(|la| {
            sample(la.fifteen, "15 分钟负载", "", None)
        }),
        _ => None,
    }
}

fn rule_severity(severity: &str) -> AlertSeverity {
    match severity {
        "critical" => AlertSeverity::Critical,
        "info" => AlertSeverity::Info,
        _ => AlertSeverity::Warning,
    }
}

fn operator_symbol(operator: &str) -> &'static str {
    match operator {
        "gt" | ">" => ">",
        "gte" | ">=" => ">=",
        "lt" | "<" => "<",
        "lte" | "<=" => "<=",
        "eq" | "==" => "=",
        _ => "?",
    }
}

/// Render the alert message: either the rule's template with placeholders
/// substituted, or a default sentence naming host, device, value and threshold.
fn render_message(rule: &AlertRule, sample: &MetricSample, hostname: &str) -> String {
    let operator = operator_symbol(&rule.operator);

    if let Some(template) = rule.message.as_deref() {
        return template
            .replace("{hostname}", hostname)
            .replace("{target}", sample.target.as_deref().unwrap_or(""))
            .replace("{metric}", sample.label)
            .replace("{value:.1}", &format!("{:.1}", sample.value))
            .replace("{threshold}", &format!("{:.1}", rule.threshold))
            .replace("{unit}", sample.unit)
            .replace("{operator}", operator);
    }

    let target = match sample.target.as_deref() {
        Some(target) => format!("（{target}）"),
        None => String::new(),
    };

    format!(
        "{} · {}{} = {:.1}{}（{} 阈值 {:.1}{}）",
        hostname, sample.label, target, sample.value, sample.unit, operator, rule.threshold, sample.unit
    )
}

fn evaluate_condition(value: f64, operator: &str, threshold: f64) -> bool {
    match operator {
        "gt" | ">" => value > threshold,
        "lt" | "<" => value < threshold,
        "gte" | ">=" => value >= threshold,
        "lte" | "<=" => value <= threshold,
        "eq" | "==" => (value - threshold).abs() < f64::EPSILON,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AlertRule, NotifyConfig};
    use crate::state::new_shared_state;
    use chrono::Utc;
    use std::sync::Arc;
    use uuid::Uuid;

    /// Retention window used by every test that does not exercise expiry itself.
    const HISTORY_MINUTES: u64 = 10;

    fn make_node(id: NodeId, hostname: &str) -> NodeInfo {
        NodeInfo {
            id,
            hostname: hostname.to_string(),
            api_addr: format!("127.0.0.1:7980"),
            gossip_addr: format!("127.0.0.1:7979"),
            status: NodeStatus::Online,
            last_seen: Utc::now(),
            version: "0.1.0".to_string(),
        }
    }

    fn make_metrics(cpu: f32) -> SystemMetrics {
        SystemMetrics {
            hostname: "test".to_string(),
            timestamp: Utc::now(),
            cpu: CpuMetrics {
                usage_percent: cpu,
                core_usages: vec![cpu],
                core_count: 1,
            },
            memory: MemoryMetrics {
                total_bytes: 1024 * 1024 * 1024,
                used_bytes: 512 * 1024 * 1024,
                available_bytes: 512 * 1024 * 1024,
                usage_percent: 50.0,
                swap_total_bytes: 0,
                swap_used_bytes: 0,
            },
            disks: vec![],
            networks: vec![],
            load_average: None,
            uptime_seconds: 1000,
            top_processes: vec![],
            physical_disks: vec![],
            os_name: "test".to_string(),
        }
    }

    fn cpu_rule(name: &str, threshold: f64) -> AlertRule {
        AlertRule {
            name: name.to_string(),
            metric: "cpu".to_string(),
            operator: "gt".to_string(),
            threshold,
            consecutive_violations: 1,
            severity: "warning".to_string(),
            message: None,
            target: None,
        }
    }

    fn disk_rule(target: Option<&str>, operator: &str, message: Option<&str>) -> AlertRule {
        AlertRule {
            name: "disk_almost_full".to_string(),
            metric: "disk".to_string(),
            target: target.map(str::to_string),
            operator: operator.to_string(),
            threshold: 85.0,
            consecutive_violations: 1,
            severity: "critical".to_string(),
            message: message.map(str::to_string),
        }
    }

    /// A database plus a notifier whose pushes are disabled, so tests never
    /// touch the network. The returned `TempDir` must stay alive.
    async fn test_context() -> (tempfile::TempDir, Arc<Database>, NotificationService) {
        let dir = tempfile::tempdir().expect("temp directory should be created");
        let db = Arc::new(
            Database::new(dir.path().join("alerts.db").to_str().unwrap())
                .await
                .expect("database should initialize"),
        );
        let notifier = NotificationService::new(
            Arc::clone(&db),
            &NotifyConfig {
                enabled: false,
                ..NotifyConfig::default()
            },
        )
        .expect("notifier should build");

        (dir, db, notifier)
    }

    #[tokio::test]
    async fn alerts_generated_for_peer_nodes() {
        let (_dir, db, notifier) = test_context().await;
        let local_id = Uuid::new_v4();
        let peer_id = Uuid::new_v4();

        let local = make_node(local_id, "local");
        let state = new_shared_state(local);

        // Register peer and inject high-CPU metrics for the peer only.
        {
            let mut s = state.write().await;
            s.upsert_peer(make_node(peer_id, "peer1"));
            // Local node has normal CPU; peer has high CPU.
            s.update_metrics(local_id, make_metrics(10.0));
            s.update_metrics(peer_id, make_metrics(95.0));
        }

        let rules = vec![cpu_rule("high_cpu", 90.0)];
        evaluate_alerts(&state, &rules, &db, &notifier, HISTORY_MINUTES).await;

        let s = state.read().await;
        let active = s.active_alerts();
        assert_eq!(active.len(), 1, "exactly one alert should be active");
        assert_eq!(active[0].node_id, peer_id, "alert should be for the peer node");
        assert_eq!(active[0].rule_name, "high_cpu");
        assert_eq!(active[0].hostname, "peer1", "message must name the host");
        assert_eq!(active[0].metric, "cpu");
        assert_eq!(active[0].operator, "gt");
    }

    #[tokio::test]
    async fn alerts_generated_for_both_local_and_peer() {
        let (_dir, db, notifier) = test_context().await;
        let local_id = Uuid::new_v4();
        let peer_id = Uuid::new_v4();

        let local = make_node(local_id, "local");
        let state = new_shared_state(local);

        {
            let mut s = state.write().await;
            s.upsert_peer(make_node(peer_id, "peer1"));
            // Both nodes exceed the threshold.
            s.update_metrics(local_id, make_metrics(95.0));
            s.update_metrics(peer_id, make_metrics(95.0));
        }

        let rules = vec![cpu_rule("high_cpu", 90.0)];
        evaluate_alerts(&state, &rules, &db, &notifier, HISTORY_MINUTES).await;

        let s = state.read().await;
        let active = s.active_alerts();
        assert_eq!(active.len(), 2, "one alert per node");
        let node_ids: std::collections::HashSet<_> = active.iter().map(|a| a.node_id).collect();
        assert!(node_ids.contains(&local_id));
        assert!(node_ids.contains(&peer_id));
    }

    #[tokio::test]
    async fn local_only_alert_still_works() {
        let (_dir, db, notifier) = test_context().await;
        let local_id = Uuid::new_v4();
        let peer_id = Uuid::new_v4();

        let local = make_node(local_id, "local");
        let state = new_shared_state(local);

        {
            let mut s = state.write().await;
            s.upsert_peer(make_node(peer_id, "peer1"));
            // Only local exceeds threshold.
            s.update_metrics(local_id, make_metrics(95.0));
            s.update_metrics(peer_id, make_metrics(10.0));
        }

        let rules = vec![cpu_rule("high_cpu", 90.0)];
        evaluate_alerts(&state, &rules, &db, &notifier, HISTORY_MINUTES).await;

        let s = state.read().await;
        let active = s.active_alerts();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].node_id, local_id);
    }

    #[tokio::test]
    async fn alert_resolves_when_the_condition_clears() {
        let (_dir, db, notifier) = test_context().await;
        let local_id = Uuid::new_v4();
        let state = new_shared_state(make_node(local_id, "local"));

        {
            let mut s = state.write().await;
            s.update_metrics(local_id, make_metrics(95.0));
        }

        let rules = vec![cpu_rule("high_cpu", 90.0)];
        evaluate_alerts(&state, &rules, &db, &notifier, HISTORY_MINUTES).await;
        assert_eq!(state.read().await.active_alerts().len(), 1);

        {
            let mut s = state.write().await;
            s.update_metrics(local_id, make_metrics(10.0));
        }
        evaluate_alerts(&state, &rules, &db, &notifier, HISTORY_MINUTES).await;

        let s = state.read().await;
        assert!(s.active_alerts().is_empty(), "recovered condition must clear the alert");
        let resolved = s
            .alerts
            .iter()
            .find(|alert| alert.rule_name == "high_cpu")
            .expect("the triggered alert must still be recorded");
        assert_eq!(resolved.resolved_reason, Some(AlertResolveReason::ConditionCleared));

        let stored = db.recent_resolved_alerts(10).await.expect("history should read");
        assert_eq!(stored.len(), 1, "resolution must be persisted");
        assert_eq!(stored[0].resolved_reason, Some(AlertResolveReason::ConditionCleared));
    }

    fn resolved_alert(rule_name: &str) -> Alert {
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
            resolved_at: None,
            resolved_reason: None,
            value: 95.0,
            threshold: 90.0,
        }
    }

    /// 最近恢复 covers `[storage] alert_history_minutes`; an entry older than
    /// the configured window must be gone from the database after an evaluation
    /// pass, not merely hidden.
    #[tokio::test]
    async fn resolved_alerts_expire_at_the_configured_window() {
        let (_dir, db, notifier) = test_context().await;
        let local_id = Uuid::new_v4();
        let state = new_shared_state(make_node(local_id, "local"));

        let mut expired = resolved_alert("expired");
        let mut recent = resolved_alert("recent");
        for entry in [&expired, &recent] {
            db.store_alert(entry).await.expect("alert should be stored");
        }
        expired.resolved_at = Some(Utc::now() - ChronoDuration::minutes(11));
        expired.resolved_reason = Some(AlertResolveReason::ConditionCleared);
        recent.resolved_at = Some(Utc::now() - ChronoDuration::minutes(2));
        recent.resolved_reason = Some(AlertResolveReason::ConditionCleared);
        db.mark_alert_resolved(&expired).await.unwrap();
        db.mark_alert_resolved(&recent).await.unwrap();

        // A wider window keeps the older entry: the configured value decides.
        evaluate_alerts(&state, &[], &db, &notifier, 30).await;
        assert_eq!(
            db.recent_resolved_alerts(10).await.expect("history should read").len(),
            2,
            "entries inside a 30 minute window must survive"
        );

        // Narrowing to the default window expires everything past 10 minutes.
        evaluate_alerts(&state, &[], &db, &notifier, HISTORY_MINUTES).await;

        let history = db.recent_resolved_alerts(10).await.expect("history should read");
        let names: Vec<&str> = history.iter().map(|alert| alert.rule_name.as_str()).collect();
        assert_eq!(names, vec!["recent"], "the expired entry must be deleted");
    }

    #[tokio::test]
    async fn alert_resolves_when_the_node_goes_offline() {
        let (_dir, db, notifier) = test_context().await;
        let local_id = Uuid::new_v4();
        let peer_id = Uuid::new_v4();
        let state = new_shared_state(make_node(local_id, "local"));

        {
            let mut s = state.write().await;
            s.upsert_peer(make_node(peer_id, "peer1"));
            s.update_metrics(local_id, make_metrics(10.0));
            s.update_metrics(peer_id, make_metrics(95.0));
        }

        let rules = vec![cpu_rule("high_cpu", 90.0)];
        evaluate_alerts(&state, &rules, &db, &notifier, HISTORY_MINUTES).await;
        assert_eq!(state.read().await.active_alerts().len(), 1);

        // The peer stops reporting: its last sample must not keep the alert open.
        {
            let mut s = state.write().await;
            s.mark_offline(&peer_id);
        }
        evaluate_alerts(&state, &rules, &db, &notifier, HISTORY_MINUTES).await;

        let s = state.read().await;
        assert!(s.active_alerts().is_empty(), "offline nodes must not hold alerts open");
        let resolved = s
            .alerts
            .iter()
            .find(|alert| alert.node_id == peer_id)
            .expect("the peer alert must still be recorded");
        assert_eq!(resolved.resolved_reason, Some(AlertResolveReason::NodeOffline));
    }

    #[tokio::test]
    async fn alert_resolves_when_the_node_is_replaced() {
        let (_dir, db, notifier) = test_context().await;
        let local_id = Uuid::new_v4();
        let peer_id = Uuid::new_v4();
        let state = new_shared_state(make_node(local_id, "local"));

        {
            let mut s = state.write().await;
            s.upsert_peer(make_node(peer_id, "peer1"));
            s.update_metrics(local_id, make_metrics(10.0));
            s.update_metrics(peer_id, make_metrics(95.0));
        }

        let rules = vec![cpu_rule("high_cpu", 90.0)];
        evaluate_alerts(&state, &rules, &db, &notifier, HISTORY_MINUTES).await;
        assert_eq!(state.read().await.active_alerts().len(), 1);

        // The peer restarted and announced itself under a new id; `upsert_peer`
        // drops the stale entry, orphaning the alert that was attached to it.
        let replacement_id = Uuid::new_v4();
        {
            let mut s = state.write().await;
            s.upsert_peer(make_node(replacement_id, "peer1"));
            s.update_metrics(replacement_id, make_metrics(10.0));
        }
        evaluate_alerts(&state, &rules, &db, &notifier, HISTORY_MINUTES).await;

        let s = state.read().await;
        assert!(s.active_alerts().is_empty(), "orphaned alerts must be resolved");
        let resolved = s
            .alerts
            .iter()
            .find(|alert| alert.node_id == peer_id)
            .expect("the old alert must still be recorded");
        assert_eq!(resolved.resolved_reason, Some(AlertResolveReason::NodeRemoved));
    }

    #[test]
    fn message_names_host_device_value_and_threshold() {
        let mut metrics = make_metrics(10.0);
        metrics.disks = vec![
            DiskInfo {
                name: "sda1".to_string(),
                mount_point: "/".to_string(),
                total_bytes: 100,
                used_bytes: 50,
                usage_percent: 50.0,
                fs_type: "ext4".to_string(),
                read_bytes: 0,
                written_bytes: 0,
                read_bytes_per_sec: 0.0,
                write_bytes_per_sec: 0.0,
                per_device_io: false,
                smart: None,
            },
            DiskInfo {
                name: "sdb1".to_string(),
                mount_point: "/data".to_string(),
                total_bytes: 100,
                used_bytes: 92,
                usage_percent: 92.0,
                fs_type: "ext4".to_string(),
                read_bytes: 0,
                written_bytes: 0,
                read_bytes_per_sec: 0.0,
                write_bytes_per_sec: 0.0,
                per_device_io: false,
                smart: None,
            },
        ];

        let untargeted = disk_rule(None, "gt", None);
        let sample = extract_metric_sample(&metrics, "disk", None).expect("disk sample");
        assert_eq!(sample.target.as_deref(), Some("/data"), "the fullest disk is reported");
        assert_eq!(
            render_message(&untargeted, &sample, "srv-01"),
            "srv-01 · 磁盘使用率（/data） = 92.0%（> 阈值 85.0%）"
        );

        let templated = disk_rule(
            Some("/data"),
            "gte",
            Some("{hostname}/{target}/{metric}={value:.1}{unit} {operator}{threshold}"),
        );
        let targeted =
            extract_metric_sample(&metrics, "disk", Some("/data")).expect("targeted sample");
        assert_eq!(
            render_message(&templated, &targeted, "srv-01"),
            "srv-01//data/磁盘使用率=92.0% >=85.0"
        );
    }
}
