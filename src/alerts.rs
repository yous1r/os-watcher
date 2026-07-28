use crate::config::AlertRule;
use crate::state::SharedState;
use crate::types::*;
use chrono::Utc;
use tracing::{info, warn};
use uuid::Uuid;

/// Evaluate alert rules against the current metrics for a node
pub async fn evaluate_alerts(state: &SharedState, rules: &[AlertRule]) {
    let (node_id, metrics, violation_counts) = {
        let s = state.read().await;
        let node_id = s.local_node.id;
        let metrics = s.metrics.get(&node_id).cloned();
        let counts = s.violation_counts.clone();
        (node_id, metrics, counts)
    };

    let Some(metrics) = metrics else { return };

    let mut new_counts = violation_counts;
    let mut triggered: Vec<(String, Alert)> = vec![];
    let mut resolved: Vec<String> = vec![];

    for rule in rules {
        let value = match extract_metric_value(&metrics, &rule.metric, rule.target.as_deref()) {
            Some(v) => v,
            None => continue,
        };

        let violated = evaluate_condition(value, &rule.operator, rule.threshold);
        let count = new_counts.entry(rule.name.clone()).or_insert(0);

        if violated {
            *count += 1;
            if *count >= rule.consecutive_violations {
                let severity = match rule.severity.as_str() {
                    "critical" => AlertSeverity::Critical,
                    "info" => AlertSeverity::Info,
                    _ => AlertSeverity::Warning,
                };

                let message = rule.message.as_deref()
                    .unwrap_or("{metric} is {value:.1} (threshold: {threshold})")
                    .replace("{metric}", &rule.metric)
                    .replace("{value:.1}", &format!("{:.1}", value))
                    .replace("{threshold}", &rule.threshold.to_string());

                warn!(
                    "Alert triggered: {} - {} = {:.2} {} {:.2}",
                    rule.name, rule.metric, value, rule.operator, rule.threshold
                );

                let alert = Alert {
                    id: Uuid::new_v4(),
                    node_id,
                    rule_name: rule.name.clone(),
                    severity,
                    message,
                    triggered_at: Utc::now(),
                    resolved_at: None,
                    value,
                    threshold: rule.threshold,
                };
                triggered.push((rule.name.clone(), alert));
            }
        } else {
            if *count > 0 {
                resolved.push(rule.name.clone());
                info!("Alert resolved: {}", rule.name);
            }
            *count = 0;
        }
    }

    // Apply changes to state
    let mut s = state.write().await;
    s.violation_counts = new_counts;

    for (rule_name, alert) in triggered {
        // Only add if there's no active alert for this rule
        let already_active = s.alerts.iter()
            .any(|a| a.rule_name == rule_name && a.resolved_at.is_none());
        if !already_active {
            s.add_alert(alert);
        }
    }

    for rule_name in resolved {
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
                metrics.disks.iter()
                    .find(|d| d.mount_point == target || d.name == target)
                    .map(|d| d.usage_percent as f64)
            } else {
                // Return highest disk usage across all disks
                metrics.disks.iter()
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
