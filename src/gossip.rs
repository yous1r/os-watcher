use anyhow::Result;
use chrono::Utc;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::config::NetworkConfig;
use crate::state::SharedState;
use crate::storage::Database;
use crate::types::*;

/// Message envelope with hop count for TTL-based flood prevention
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct GossipEnvelope {
    message: GossipMessage,
    hops: u8,
    max_hops: u8,
    /// Unique message ID to prevent loops
    msg_id: uuid::Uuid,
}

/// Broadcast a NodeLeave message on a best-effort basis so peers mark this
/// node offline immediately rather than waiting for the stale timeout.
/// Called once from main() just before the process exits.
pub async fn broadcast_leave(state: &SharedState, config: &NetworkConfig) {
    let (node_id, peer_addrs) = {
        let s = state.read().await;
        (s.local_node.id, s.peer_gossip_addrs())
    };

    let bind_addr = format!("{}:0", config.bind_addr);
    let socket = match UdpSocket::bind(&bind_addr).await {
        Ok(s) => s,
        Err(e) => {
            warn!("broadcast_leave: failed to bind UDP socket: {}", e);
            return;
        }
    };
    if let Err(e) = socket.set_broadcast(true) {
        warn!("broadcast_leave: set_broadcast failed: {}", e);
    }

    let envelope = GossipEnvelope {
        message: GossipMessage::NodeLeave(node_id),
        hops: 0,
        max_hops: config.max_hops,
        msg_id: uuid::Uuid::new_v4(),
    };
    let data = match serde_json::to_vec(&envelope) {
        Ok(d) => d,
        Err(_) => return,
    };

    // Send to all known peers
    for addr in &peer_addrs {
        let _ = socket.send_to(&data, addr).await;
    }

    // Also broadcast on LAN so nodes we haven't synced yet get notified
    if config.enable_discovery {
        let broadcast_addr = format!("255.255.255.255:{}", config.gossip_port);
        let _ = socket.send_to(&data, &broadcast_addr).await;
    }

    info!(
        "NodeLeave broadcast sent ({} known peers)",
        peer_addrs.len()
    );
}

/// Namespace for gossip service associated functions.
pub struct GossipService;

impl GossipService {
    /// Run with an explicit receiver channel
    pub async fn run_with_rx(
        state: SharedState,
        config: NetworkConfig,
        db: Arc<Database>,
    ) -> Result<()> {
        let bind_addr = format!("{}:{}", config.bind_addr, config.gossip_port);
        let socket = Arc::new(UdpSocket::bind(&bind_addr).await?);
        socket.set_broadcast(true)?;

        info!("Gossip service listening on {}", bind_addr);

        let (tx, mut rx) = mpsc::channel::<(GossipEnvelope, Option<String>)>(256);

        let max_hops = config.max_hops;
        let gossip_port = config.gossip_port;
        let announce_interval_secs = config.announce_interval_secs;
        let gossip_interval_secs = config.gossip_interval_secs;

        // Connect to manually configured peers
        {
            let state_read = state.read().await;
            let local_id = state_read.local_node.id;
            drop(state_read);

            for peer_addr in &config.peers {
                let envelope = GossipEnvelope {
                    message: GossipMessage::SyncRequest { from: local_id },
                    hops: 0,
                    max_hops,
                    msg_id: uuid::Uuid::new_v4(),
                };
                let data = serde_json::to_vec(&envelope).unwrap_or_default();
                let _ = socket.send_to(&data, peer_addr).await;
                info!("Sent sync request to configured peer: {}", peer_addr);
            }
        }

        // Task: Receive incoming messages
        let recv_socket = Arc::clone(&socket);
        let recv_state = Arc::clone(&state);
        let recv_tx = tx.clone();
        let recv_db = Arc::clone(&db);

        tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            loop {
                match recv_socket.recv_from(&mut buf).await {
                    Ok((len, src)) => {
                        if let Ok(envelope) = serde_json::from_slice::<GossipEnvelope>(&buf[..len])
                        {
                            Self::handle_message(
                                &recv_state,
                                envelope,
                                src,
                                &recv_tx,
                                max_hops,
                                Arc::clone(&recv_db),
                            )
                            .await;
                        }
                    }
                    Err(e) => {
                        error!("UDP receive error: {}", e);
                    }
                }
            }
        });

        // Task: Periodic self-announcement (broadcast)
        let ann_state = Arc::clone(&state);
        let ann_tx = tx.clone();

        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_secs(announce_interval_secs));
            // Announce immediately on start
            interval.tick().await;
            loop {
                let node_info = {
                    let s = ann_state.read().await;
                    let mut info = s.local_node.clone();
                    info.last_seen = Utc::now();
                    info
                };

                let envelope = GossipEnvelope {
                    message: GossipMessage::NodeAnnounce(node_info),
                    hops: 0,
                    max_hops,
                    msg_id: uuid::Uuid::new_v4(),
                };

                let broadcast_addr = format!("255.255.255.255:{}", gossip_port);
                let _ = ann_tx.send((envelope.clone(), Some(broadcast_addr))).await;

                // Also unicast to known peers
                let peer_addrs = {
                    let s = ann_state.read().await;
                    s.peer_gossip_addrs()
                };
                for addr in peer_addrs {
                    let _ = ann_tx.send((envelope.clone(), Some(addr))).await;
                }

                interval.tick().await;
            }
        });

        // Task: Periodic metrics gossip
        let gossip_state = Arc::clone(&state);
        let gossip_tx = tx.clone();

        tokio::spawn(async move {
            // Tick immediately so we push metrics as soon as there's something
            // to send, rather than waiting the full interval.
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_secs(gossip_interval_secs));
            loop {
                interval.tick().await;
                let (node_id, metrics, peer_addrs) = {
                    let s = gossip_state.read().await;
                    let metrics = s.metrics.get(&s.local_node.id).cloned();
                    let addrs = s.peer_gossip_addrs();
                    (s.local_node.id, metrics, addrs)
                };

                if let Some(metrics) = metrics {
                    for addr in peer_addrs {
                        let envelope = GossipEnvelope {
                            message: GossipMessage::MetricsUpdate {
                                node_id,
                                metrics: Box::new(metrics.clone()),
                            },
                            hops: 0,
                            max_hops,
                            msg_id: uuid::Uuid::new_v4(),
                        };
                        let _ = gossip_tx.send((envelope, Some(addr))).await;
                    }
                }
            }
        });

        // Task: Stale peer cleanup
        let cleanup_state = Arc::clone(&state);

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
            loop {
                interval.tick().await;
                let mut s = cleanup_state.write().await;
                let local_id = s.local_node.id;
                let stale: Vec<NodeId> = s
                    .peers
                    .keys()
                    .filter(|&&id| id != local_id)
                    .filter(|id| s.is_stale(id, 120))
                    .cloned()
                    .collect();

                for id in stale {
                    warn!("Node {} is stale, marking offline", id);
                    s.mark_offline(&id);
                }
            }
        });

        // Main send loop
        let send_socket = Arc::clone(&socket);
        while let Some((envelope, target)) = rx.recv().await {
            let data = match serde_json::to_vec(&envelope) {
                Ok(d) => d,
                Err(e) => {
                    error!("Failed to serialize gossip message: {}", e);
                    continue;
                }
            };

            if let Some(addr) = target {
                if let Err(e) = send_socket.send_to(&data, &addr).await {
                    debug!("Failed to send to {}: {}", addr, e);
                }
            }
        }

        Ok(())
    }

    async fn handle_message(
        state: &SharedState,
        envelope: GossipEnvelope,
        src: SocketAddr,
        tx: &mpsc::Sender<(GossipEnvelope, Option<String>)>,
        max_hops: u8,
        db: Arc<Database>,
    ) {
        match envelope.message {
            GossipMessage::NodeAnnounce(mut info) => {
                info.status = NodeStatus::Online;
                info.last_seen = Utc::now();

                // If the peer advertised a wildcard bind address (0.0.0.0 / ::) as its
                // gossip_addr, it hasn't applied the advertise_addr fix yet.  Replace the
                // wildcard host with the actual UDP source IP so we have a routable address.
                {
                    let parts: Vec<&str> = info.gossip_addr.rsplitn(2, ':').collect();
                    if parts.len() == 2 {
                        let host = parts[1];
                        let port = parts[0];
                        if host == "0.0.0.0" || host == "::" {
                            info.gossip_addr = format!("{}:{}", src.ip(), port);
                            debug!(
                                "Rewrote gossip_addr for {} to {}",
                                info.hostname, info.gossip_addr
                            );
                        }
                    }
                }

                debug!(
                    "Received announce from node {} ({})",
                    info.hostname, info.id
                );

                let should_sync = {
                    let s = state.read().await;
                    !s.peers.contains_key(&info.id)
                };

                {
                    let mut s = state.write().await;
                    s.upsert_peer(info.clone());
                }

                // If this is a new peer, request sync
                if should_sync {
                    let local_id = state.read().await.local_node.id;
                    let sync_req = GossipEnvelope {
                        message: GossipMessage::SyncRequest { from: local_id },
                        hops: 0,
                        max_hops,
                        msg_id: uuid::Uuid::new_v4(),
                    };
                    let _ = tx.send((sync_req, Some(info.gossip_addr.clone()))).await;
                }

                // Forward if hops remain
                if envelope.hops < envelope.max_hops {
                    let fwd = GossipEnvelope {
                        message: GossipMessage::NodeAnnounce(info),
                        hops: envelope.hops + 1,
                        ..envelope
                    };
                    let peer_addrs = state.read().await.peer_gossip_addrs();
                    for addr in peer_addrs {
                        let _ = tx.send((fwd.clone(), Some(addr))).await;
                    }
                }
            }

            GossipMessage::MetricsUpdate { node_id, metrics } => {
                // The owner of a sample re-broadcasts its current one on every
                // gossip tick, and a node that already has it must not persist or
                // forward it again: forwarding turned every sample into one row
                // per node in the mesh (31x on a live install), and re-forwarding
                // an unchanged sample made it echo until `max_hops` ran out.
                let is_new_sample = {
                    let s = state.read().await;
                    s.metrics
                        .get(&node_id)
                        .is_none_or(|known| known.timestamp != metrics.timestamp)
                };
                if !is_new_sample {
                    return;
                }

                {
                    let mut s = state.write().await;
                    // Update last_seen for the node
                    if let Some(peer) = s.peers.get_mut(&node_id) {
                        peer.last_seen = Utc::now();
                        peer.status = NodeStatus::Online;
                    }
                    s.update_metrics(node_id, *metrics.clone());
                }

                // 持久化远程节点指标（后台任务，不阻塞接收循环）
                {
                    let db_clone = Arc::clone(&db);
                    let metrics_clone = *metrics.clone();
                    tokio::spawn(async move {
                        if let Err(e) = db_clone.store_metrics(&node_id, &metrics_clone).await {
                            tracing::warn!(
                                "Failed to persist remote metrics for {}: {}",
                                node_id,
                                e
                            );
                        }
                    });
                }

                // Forward if hops remain
                if envelope.hops < envelope.max_hops {
                    let fwd = GossipEnvelope {
                        message: GossipMessage::MetricsUpdate { node_id, metrics },
                        hops: envelope.hops + 1,
                        ..envelope
                    };
                    let peer_addrs = state.read().await.peer_gossip_addrs();
                    for addr in peer_addrs {
                        let _ = tx.send((fwd.clone(), Some(addr))).await;
                    }
                }
            }

            GossipMessage::NodeLeave(node_id) => {
                info!("Node {} left the mesh", node_id);
                let mut s = state.write().await;
                s.mark_offline(&node_id);
            }

            GossipMessage::Ping { from } => {
                let local_id = state.read().await.local_node.id;
                let pong = GossipEnvelope {
                    message: GossipMessage::Pong {
                        from: local_id,
                        to: from,
                    },
                    hops: 0,
                    max_hops,
                    msg_id: uuid::Uuid::new_v4(),
                };
                let _ = tx.send((pong, Some(src.to_string()))).await;
            }

            GossipMessage::Pong { from, .. } => {
                debug!("Pong from {}", from);
                let mut s = state.write().await;
                if let Some(peer) = s.peers.get_mut(&from) {
                    peer.last_seen = Utc::now();
                    peer.status = NodeStatus::Online;
                }
            }

            GossipMessage::SyncRequest { from } => {
                debug!("Sync request from {}", from);
                let (nodes, metrics) = {
                    let s = state.read().await;
                    let nodes: Vec<NodeInfo> = s.peers.values().cloned().collect();
                    let metrics = s.metrics.clone();
                    (nodes, metrics)
                };

                let local_id = state.read().await.local_node.id;
                let response = GossipEnvelope {
                    message: GossipMessage::SyncResponse {
                        from: local_id,
                        nodes,
                        metrics,
                    },
                    hops: 0,
                    max_hops: 0, // Don't forward sync responses
                    msg_id: uuid::Uuid::new_v4(),
                };
                let _ = tx.send((response, Some(src.to_string()))).await;
            }

            GossipMessage::SyncResponse {
                from,
                nodes,
                metrics,
            } => {
                debug!("Sync response from {}, {} nodes", from, nodes.len());
                let mut s = state.write().await;
                let local_id = s.local_node.id;
                for node in nodes {
                    // Always update remote peers' info so we learn the real
                    // gossip_addr / api_addr they advertise. Skip overwriting
                    // our own entry so local status is authoritative.
                    if node.id != local_id {
                        s.upsert_peer(node);
                    }
                }
                for (node_id, m) in metrics {
                    // Accept metrics for any node, including ones we already
                    // know about — this is how we get initial data from peers
                    // that started before us or whose metrics we missed.
                    if node_id != local_id {
                        s.update_metrics(node_id, m);
                    }
                }
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::new_shared_state;
    use crate::storage::Database;
    use crate::types::{CpuMetrics, MemoryMetrics, NodeInfo, NodeStatus, SystemMetrics};
    use chrono::Utc;
    use tokio::sync::mpsc;
    use uuid::Uuid;

    fn node(name: &str) -> NodeInfo {
        NodeInfo {
            id: Uuid::new_v4(),
            hostname: name.to_string(),
            api_addr: "127.0.0.1:7980".to_string(),
            gossip_addr: "127.0.0.1:7979".to_string(),
            status: NodeStatus::Online,
            last_seen: Utc::now(),
            version: "0.1.0".to_string(),
        }
    }

    fn sample(at: chrono::DateTime<Utc>) -> SystemMetrics {
        SystemMetrics {
            timestamp: at,
            cpu: CpuMetrics {
                usage_percent: 10.0,
                core_usages: vec![10.0],
                core_count: 1,
            },
            memory: MemoryMetrics {
                total_bytes: 1024,
                used_bytes: 128,
                available_bytes: 896,
                usage_percent: 12.5,
                swap_total_bytes: 0,
                swap_used_bytes: 0,
            },
            disks: vec![],
            physical_disks: vec![],
            networks: vec![],
            load_average: None,
            top_processes: vec![],
            uptime_seconds: 1,
            os_name: "test".to_string(),
            hostname: "peer".to_string(),
        }
    }

    async fn update(
        state: &SharedState,
        db: &Arc<Database>,
        tx: &mpsc::Sender<(GossipEnvelope, Option<String>)>,
        node_id: NodeId,
        metrics: SystemMetrics,
        hops: u8,
    ) {
        GossipService::handle_message(
            state,
            GossipEnvelope {
                message: GossipMessage::MetricsUpdate {
                    node_id,
                    metrics: Box::new(metrics),
                },
                hops,
                max_hops: 3,
                msg_id: Uuid::new_v4(),
            },
            "127.0.0.1:1234".parse().unwrap(),
            tx,
            3,
            Arc::clone(db),
        )
        .await;
    }

    /// A node re-broadcasts its current sample every gossip tick. Re-storing and
    /// re-forwarding it is what turned one sample into a row per node in the mesh
    /// and made it echo until `max_hops` ran out.
    #[tokio::test]
    async fn a_rebroadcast_sample_is_not_stored_or_forwarded_again() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = Arc::new(
            Database::new(dir.path().join("history.db").to_str().unwrap())
                .await
                .expect("database should initialize"),
        );
        let state = new_shared_state(node("local"));
        let peer = node("peer");
        let peer_id = peer.id;
        {
            let mut s = state.write().await;
            s.upsert_peer(peer);
            // A second peer means a fresh sample has somewhere to be forwarded.
            s.upsert_peer(node("other"));
        }
        let (tx, mut rx) = mpsc::channel(16);
        let first = sample(Utc::now());

        update(&state, &db, &tx, peer_id, first.clone(), 0).await;
        let forwarded_first = rx.try_recv().is_ok();
        assert!(forwarded_first, "a new sample must reach the other peers");

        // The same sample again, as the owner re-broadcasts it.
        update(&state, &db, &tx, peer_id, first.clone(), 0).await;
        assert!(
            rx.try_recv().is_err(),
            "a sample already held must not be forwarded again"
        );

        // Persistence is spawned so the receive loop is not blocked, so wait for
        // the write instead of racing it.
        let mut stored = Vec::new();
        for _ in 0..100 {
            stored = db
                .get_recent_metrics(&peer_id, 10)
                .await
                .expect("history should be readable");
            if !stored.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(
            stored.len(),
            1,
            "a repeated sample must occupy a single row"
        );

        // A genuinely newer sample is still accepted and forwarded.
        let newer = sample(Utc::now() + chrono::Duration::seconds(5));
        update(&state, &db, &tx, peer_id, newer, 0).await;
        assert!(rx.try_recv().is_ok(), "a new sample must be forwarded");
    }
}
