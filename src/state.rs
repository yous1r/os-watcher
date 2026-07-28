use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use chrono::Utc;
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
    /// Violation counters for alert rules (rule_name -> consecutive count)
    pub violation_counts: HashMap<String, u32>,
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

    /// Update or insert a peer's info
    pub fn upsert_peer(&mut self, info: NodeInfo) {
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
            .filter(|p| p.id != self.local_node.id && p.status == NodeStatus::Online)
            .map(|p| p.gossip_addr.clone())
            .collect()
    }

    /// Add an alert
    pub fn add_alert(&mut self, alert: Alert) {
        self.alerts.push(alert);
    }

    /// Resolve an alert by rule name and node
    pub fn resolve_alert(&mut self, node_id: &NodeId, rule_name: &str) {
        let now = Utc::now();
        for alert in self.alerts.iter_mut() {
            if alert.node_id == *node_id
                && alert.rule_name == rule_name
                && alert.resolved_at.is_none()
            {
                alert.resolved_at = Some(now);
            }
        }
    }

    /// Get unresolved alerts
    pub fn active_alerts(&self) -> Vec<&Alert> {
        self.alerts.iter()
            .filter(|a| a.resolved_at.is_none())
            .collect()
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
