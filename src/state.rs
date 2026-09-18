use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use chrono::{DateTime, Utc};
use crate::types::*;

/// The central shared state for all metrics and peer information
#[derive(Debug)]
pub struct NodeState {
    /// This node's own identity
    pub local_node: NodeInfo,
    /// All known peers (including self)
    pub peers: HashMap<NodeId, NodeInfo>,
    /// Latest metrics per node
    pub metrics: HashMap<NodeId, SystemMetrics>,
    /// Active alerts
    pub alerts: Vec<Alert>,
    /// Violation counters for alert rules, keyed by (node_id, rule_name)
    pub violation_counts: HashMap<NodeId, HashMap<String, u32>>,
}

impl NodeState {
    pub fn new(local_node: NodeInfo) -> Self {
        let node_id = local_node.id;
        let mut peers = HashMap::new();
        peers.insert(node_id, local_node.clone());

        Self {
            local_node,
            peers,
            metrics: HashMap::new(),
            alerts: vec![],
            violation_counts: HashMap::new(),
        }
    }

    /// Update or insert a peer's info.
    ///
    /// If a peer with the same `gossip_addr` but a *different* ID already
    /// exists (e.g. the remote node restarted and got a new UUID), the stale
    /// entry is removed first so the web UI never shows duplicate nodes.
    pub fn upsert_peer(&mut self, info: NodeInfo) {
        // Collect stale IDs: same gossip address, different UUID.
        let stale_ids: Vec<NodeId> = self
            .peers
            .iter()
            .filter(|(&id, p)| id != info.id && p.gossip_addr == info.gossip_addr)
            .map(|(&id, _)| id)
            .collect();

        for id in stale_ids {
            self.peers.remove(&id);
            self.metrics.remove(&id);
        }

        self.peers.insert(info.id, info);
    }

    /// Update metrics for a node
    pub fn update_metrics(&mut self, node_id: NodeId, metrics: SystemMetrics) {
        self.metrics.insert(node_id, metrics);
    }

    /// Mark a node as offline
    pub fn mark_offline(&mut self, node_id: &NodeId) {
        if let Some(peer) = self.peers.get_mut(node_id) {
            peer.status = NodeStatus::Offline;
        }
    }

    /// Check if a node was last seen within the timeout
    pub fn is_stale(&self, node_id: &NodeId, timeout_secs: i64) -> bool {
        if let Some(peer) = self.peers.get(node_id) {
            let elapsed = Utc::now()
                .signed_duration_since(peer.last_seen)
                .num_seconds();
            elapsed > timeout_secs
        } else {
            true
        }
    }

    /// Get all online peers
    pub fn online_peers(&self) -> Vec<&NodeInfo> {
        self.peers.values()
            .filter(|p| p.status == NodeStatus::Online)
            .collect()
    }

    /// Get list of peer gossip addresses (excluding self)
    pub fn peer_gossip_addrs(&self) -> Vec<String> {
        self.peers.values()
            .filter(|p| p.id != self.local_node.id)
            // Include Online and Unknown peers — Unknown means we just
            // learned about them and haven't confirmed liveness yet, but
            // we still want to attempt contact so they can sync back.
            .filter(|p| p.status != NodeStatus::Offline)
            .map(|p| p.gossip_addr.clone())
            .collect()
    }

    /// Add an alert.
    ///
    /// Returns `false` when an active alert for the same `(node_id, rule_name)`
    /// already exists, in which case nothing is added.
    pub fn add_alert(&mut self, alert: Alert) -> bool {
        let already_active = self.alerts.iter().any(|a| {
            a.node_id == alert.node_id && a.rule_name == alert.rule_name && a.resolved_at.is_none()
        });
        if already_active {
            return false;
        }
        self.alerts.push(alert);
        true
    }

    /// Resolve the active alerts for a `(node_id, rule_name)` pair.
    ///
    /// Returns the alerts that were actually resolved by this call (empty when
    /// there was nothing active), so callers can persist and notify exactly once.
    pub fn resolve_alert(
        &mut self,
        node_id: &NodeId,
        rule_name: &str,
        reason: AlertResolveReason,
    ) -> Vec<Alert> {
        let now = Utc::now();
        let mut resolved = vec![];
        for alert in self.alerts.iter_mut() {
            if alert.node_id == *node_id
                && alert.rule_name == rule_name
                && alert.resolved_at.is_none()
            {
                alert.resolved_at = Some(now);
                alert.resolved_reason = Some(reason);
                resolved.push(alert.clone());
            }
        }
        resolved
    }

    /// Resolve every active alert owned by `node_id`, returning those resolved.
    pub fn resolve_alerts_of_node(
        &mut self,
        node_id: &NodeId,
        reason: AlertResolveReason,
    ) -> Vec<Alert> {
        let now = Utc::now();
        let mut resolved = vec![];
        for alert in self.alerts.iter_mut() {
            if alert.node_id == *node_id && alert.resolved_at.is_none() {
                alert.resolved_at = Some(now);
                alert.resolved_reason = Some(reason);
                resolved.push(alert.clone());
            }
        }
        resolved
    }

    /// Get unresolved alerts
    pub fn active_alerts(&self) -> Vec<&Alert> {
        self.alerts.iter()
            .filter(|a| a.resolved_at.is_none())
            .collect()
    }

    /// Drop resolved alerts older than `cutoff`, returning how many were removed.
    /// Resolved alerts are kept for a short window so the panel can show them;
    /// the database copy expires on the same cutoff.
    pub fn prune_resolved_alerts(&mut self, cutoff: DateTime<Utc>) -> usize {
        let before = self.alerts.len();
        self.alerts.retain(|a| match a.resolved_at {
            Some(at) => at >= cutoff,
            None => true,
        });
        before - self.alerts.len()
    }

    /// Re-insert alerts loaded from the database at startup.
    pub fn restore_alerts(&mut self, alerts: Vec<Alert>) {
        let mut restored: Vec<Alert> = alerts
            .into_iter()
            .filter(|a| a.resolved_at.is_none())
            .collect();
        restored.sort_by_key(|a| a.triggered_at);
        for alert in restored {
            self.add_alert(alert);
        }
    }

    /// Get a snapshot summary for all nodes
    pub fn node_snapshots(&self) -> Vec<NodeSnapshot> {
        self.peers.values().map(|info| {
            NodeSnapshot {
                info: info.clone(),
                metrics: self.metrics.get(&info.id).cloned(),
            }
        }).collect()
    }
}

/// Thread-safe wrapper around NodeState
pub type SharedState = Arc<RwLock<NodeState>>;

/// Create a new shared state
pub fn new_shared_state(local_node: NodeInfo) -> SharedState {
    Arc::new(RwLock::new(NodeState::new(local_node)))
}
