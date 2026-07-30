use crate::config::AlertRule;
use crate::state::SharedState;
use crate::types::*;
use chrono::Utc;
use tracing::{info, warn};
use uuid::Uuid;

/// Evaluate alert rules against the current metrics for ALL known nodes.
///
/// Previously this only ran against the local node, so alerts from peer nodes
/// were never generated and therefore never shown in the web dashboard.
pub async fn evaluate_alerts(state: &SharedState, rules: &[AlertRule]) {
    // Collect the set of node-ids and their latest metrics in one read lock.
    let (local_id, node_metrics, violation_counts) = {
        let s = state.read().await;
        let local_id = s.local_node.id;
        let node_metrics: Vec<(NodeId, SystemMetrics)> = s
            .metrics
            .iter()
            .map(|(&id, m)| (id, m.clone()))
            .collect();
        let counts = s.violation_counts.clone();
        (local_id, node_metrics, counts)
    };

    // Per-node, per-rule violation counters (keyed node_id -> rule_name -> count).
    let mut new_counts = violation_counts;

    let mut triggered: Vec<(NodeId, String, Alert)> = vec![];
    let mut resolved: Vec<(NodeId, String)> = vec![];

    for (node_id, metrics) in &node_metrics {
        let node_counts = new_counts.entry(*node_id).or_default();

        for rule in rules {
            let value = match extract_metric_value(metrics, &rule.metric, rule.target.as_deref()) {
                Some(v) => v,
                None => continue,
            };

            let violated = evaluate_condition(value, &rule.operator, rule.threshold);
            let count = node_counts.entry(rule.name.clone()).or_insert(0);

            if violated {
                *count += 1;
                if *count >= rule.consecutive_violations {
                    let severity = match rule.severity.as_str() {
                        "critical" => AlertSeverity::Critical,
                        "info" => AlertSeverity::Info,
                        _ => AlertSeverity::Warning,
                    };

                    let message = rule
                        .message
                        .as_deref()
                        .unwrap_or("{metric} is {value:.1} (threshold: {threshold})")
                        .replace("{metric}", &rule.metric)
                        .replace("{value:.1}", &format!("{:.1}", value))
                        .replace("{threshold}", &rule.threshold.to_string());

                    if *node_id == local_id {
                        warn!(
                            "Alert triggered: {} - {} = {:.2} {} {:.2}",
                            rule.name, rule.metric, value, rule.operator, rule.threshold
                        );
                    } else {
                        warn!(
                            "Alert triggered on peer {}: {} - {} = {:.2} {} {:.2}",
                            node_id, rule.name, rule.metric, value, rule.operator, rule.threshold
                        );
                    }

                    let alert = Alert {
                        id: Uuid::new_v4(),
                        node_id: *node_id,
                        rule_name: rule.name.clone(),
                        severity,
                        message,
                        triggered_at: Utc::now(),
                        resolved_at: None,
                        value,
                        threshold: rule.threshold,
                    };
                    triggered.push((*node_id, rule.name.clone(), alert));
                }
            } else {
                if *count > 0 {
                    resolved.push((*node_id, rule.name.clone()));
                    info!("Alert resolved on node {}: {}", node_id, rule.name);
                }
                *count = 0;
            }
        }
    }

    // Apply changes to state in a single write lock.
    let mut s = state.write().await;
    s.violation_counts = new_counts;

    for (node_id, rule_name, alert) in triggered {
        // Only add if there's no active alert for this rule+node combination.
        let already_active = s
            .alerts
            .iter()
            .any(|a| a.node_id == node_id && a.rule_name == rule_name && a.resolved_at.is_none());
        if !already_active {
            s.add_alert(alert);
        }
    }

    for (node_id, rule_name) in resolved {
        s.resolve_alert(&node_id, &rule_name);
    }
}

/// Extract a metric value given the rule's metric name and optional target
fn extract_metric_value(metrics: &SystemMetrics, metric: &str, target: Option<&str>) -> Option<f64> {
    match metric {
        "cpu" => Some(metrics.cpu.usage_percent as f64),
        "memory" => Some(metrics.memory.usage_percent as f64),
        "swap" => {
            if metrics.memory.swap_total_bytes > 0 {
                Some(
                    (metrics.memory.swap_used_bytes as f64
                        / metrics.memory.swap_total_bytes as f64)
                        * 100.0,
                )
            } else {
                None
            }
        }
        "disk" => {
            if let Some(target) = target {
                metrics
                    .disks
                    .iter()
                    .find(|d| d.mount_point == target || d.name == target)
                    .map(|d| d.usage_percent as f64)
            } else {
                // Return highest disk usage across all disks
                metrics
                    .disks
                    .iter()
                    .map(|d| d.usage_percent as f64)
                    .reduce(f64::max)
            }
        }
        "load1" => metrics.load_average.as_ref().map(|la| la.one),
        "load5" => metrics.load_average.as_ref().map(|la| la.five),
        "load15" => metrics.load_average.as_ref().map(|la| la.fifteen),
        _ => None,
    }
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
    use crate::config::AlertRule;
    use crate::state::new_shared_state;
    use crate::types::*;
    use chrono::Utc;
    use uuid::Uuid;

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

    #[tokio::test]
    async fn alerts_generated_for_peer_nodes() {
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
        evaluate_alerts(&state, &rules).await;

        let s = state.read().await;
        let active = s.active_alerts();
        assert_eq!(active.len(), 1, "exactly one alert should be active");
        assert_eq!(active[0].node_id, peer_id, "alert should be for the peer node");
        assert_eq!(active[0].rule_name, "high_cpu");
    }

    #[tokio::test]
    async fn alerts_generated_for_both_local_and_peer() {
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
        evaluate_alerts(&state, &rules).await;

        let s = state.read().await;
        let active = s.active_alerts();
        assert_eq!(active.len(), 2, "one alert per node");
        let node_ids: std::collections::HashSet<_> = active.iter().map(|a| a.node_id).collect();
        assert!(node_ids.contains(&local_id));
        assert!(node_ids.contains(&peer_id));
    }

    #[tokio::test]
    async fn local_only_alert_still_works() {
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
        evaluate_alerts(&state, &rules).await;

        let s = state.read().await;
        let active = s.active_alerts();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].node_id, local_id);
    }
}
