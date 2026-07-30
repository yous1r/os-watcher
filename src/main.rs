mod alerts;
mod api;
mod collector;
mod config;
mod disk_health;
mod diskstats;
mod gossip;
mod smart;
mod state;
mod storage;
mod tui;
mod types;
mod upgrade;

use anyhow::Result;
use chrono::Utc;
use clap::{Parser, Subcommand};
use std::{path::PathBuf, sync::Arc};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

use crate::collector::MetricsCollector;
use crate::config::{generate_default_config, load_config, Config, ConfigProfile, PackageKind};
use crate::gossip::{GossipService, broadcast_leave};
use crate::state::new_shared_state;
use crate::types::{NodeInfo, NodeStatus};
use crate::upgrade::{UpgradeHelperRequest, UpgradeManager};

#[derive(Parser)]
#[command(
    name = "os-watcher",
    about = "Decentralized host resource monitor",
    version = "0.1.0"
)]
struct Cli {
    /// Path to config file
    #[arg(short, long, default_value = "config.toml")]
    config: String,

    /// Log level (trace, debug, info, warn, error)
    #[arg(short, long, default_value = "info")]
    log_level: String,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the monitoring agent (default)
    Start {
        /// Override gossip port
        #[arg(long)]
        gossip_port: Option<u16>,
        /// Override API port
        #[arg(long)]
        api_port: Option<u16>,
        /// Add manual peer addresses (comma-separated host:port)
        #[arg(long)]
        peers: Option<String>,
        /// Run in TUI mode
        #[arg(long, short)]
        tui: bool,
        /// Serve the web dashboard (static frontend) alongside the API.
        /// Overrides `web.enabled = false` in the config file.
        #[arg(long)]
        web: bool,
        /// Directory of built web assets (defaults to `web.dir` from the config)
        #[arg(long)]
        web_dir: Option<String>,
    },
    /// Print default configuration to stdout
    GenConfig {
        /// Which package flavour to generate defaults for:
        /// "node" (collect only) or "full" (also serves the dashboard)
        #[arg(long, value_enum, default_value = "node")]
        profile: ConfigProfile,
    },
    /// Show status of all known nodes (requires a running agent)
    Status {
        /// Agent API address
        #[arg(long, default_value = "http://127.0.0.1:7980")]
        api: String,
    },
    /// Internal helper used by self-upgrade restart orchestration.
    #[command(hide = true)]
    UpgradeHelper {
        #[arg(long)]
        service_name: String,
        #[arg(long)]
        current_exe: PathBuf,
        #[arg(long)]
        install_dir: PathBuf,
        #[arg(long)]
        backup_dir: PathBuf,
        #[arg(long)]
        status_file: PathBuf,
        #[arg(long)]
        target_version: String,
        #[arg(long)]
        package: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Init logging
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&cli.log_level)),
        )
        .with_target(false)
        .compact()
        .init();

    match cli.command.unwrap_or(Commands::Start {
        gossip_port: None,
        api_port: None,
        peers: None,
        tui: false,
        web: false,
        web_dir: None,
    }) {
        Commands::GenConfig { profile } => {
            print!("{}", generate_default_config(profile));
        }

        Commands::Status { api } => {
            run_status_check(&api).await?;
        }

        Commands::UpgradeHelper {
            service_name,
            current_exe,
            install_dir,
            backup_dir,
            status_file,
            target_version,
            package,
        } => {
            let package = parse_package_kind(&package)?;
            upgrade::run_upgrade_helper(UpgradeHelperRequest {
                service_name,
                current_exe,
                install_dir,
                backup_dir,
                status_file,
                target_version,
                package,
            })
            .await?;
        }

        Commands::Start {
            gossip_port,
            api_port,
            peers,
            tui: use_tui,
            web,
            web_dir,
        } => {
            // Load or default config
            let mut cfg = match load_config(&cli.config) {
                Ok(c) => {
                    info!("Loaded config from {}", cli.config);
                    c
                }
                Err(_) => {
                    info!("No config file found, using defaults");
                    Config::default()
                }
            };

            // Apply CLI overrides
            if let Some(p) = gossip_port {
                cfg.network.gossip_port = p;
            }
            if let Some(p) = api_port {
                cfg.api.port = p;
            }
            if let Some(peers_str) = peers {
                let extra: Vec<String> = peers_str
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                cfg.network.peers.extend(extra);
            }

            // Resolve the web dashboard directory. The dashboard is on when
            // either the config enables it (the `full` package default) or
            // `--web` is passed; `--web-dir` overrides the configured path.
            if let Some(dir) = web_dir {
                cfg.web.dir = dir;
            }
            let web_dir = if web || cfg.web.enabled {
                Some(cfg.web.dir.clone())
            } else {
                None
            };

            run_agent(cfg, use_tui, web_dir).await?;
        }
    }

    Ok(())
}

fn parse_package_kind(value: &str) -> Result<PackageKind> {
    match value {
        "node" => Ok(PackageKind::Node),
        "full" => Ok(PackageKind::Full),
        _ => Err(anyhow::anyhow!("invalid package kind: {value}")),
    }
}

async fn run_agent(cfg: Config, use_tui: bool, web_dir: Option<String>) -> Result<()> {
    let hostname = sysinfo::System::host_name().unwrap_or_else(|| "unknown".to_string());
    let node_name = cfg.node.name.clone().unwrap_or_else(|| hostname.clone());
    let node_id = Uuid::new_v4();

    // Resolve the address we advertise to other nodes.
    // When bind_addr is 0.0.0.0 we must tell peers something routable;
    // prefer the explicit advertise_addr, then auto-detect the outbound IP.
    let advertise_host = resolve_advertise_addr(&cfg.network);

    let api_addr = resolve_api_addr(&cfg.api, &advertise_host);
    // The gossip_addr we announce must be reachable by remote nodes, so use
    // the resolved advertise address, not the wildcard bind address.
    let gossip_addr = format_host_port(&advertise_host, cfg.network.gossip_port);

    let local_node = NodeInfo {
        id: node_id,
        hostname: node_name.clone(),
        api_addr: api_addr.clone(),
        gossip_addr: gossip_addr.clone(),
        status: NodeStatus::Online,
        last_seen: Utc::now(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };

    info!("Starting os-watcher node: {} ({})", node_name, node_id);
    info!("  API: http://{}", api_addr);
    info!("  Gossip (advertised): udp://{}", gossip_addr);
    info!(
        "  Gossip bind: udp://{}:{}",
        cfg.network.bind_addr, cfg.network.gossip_port
    );
    info!("  Peers configured: {}", cfg.network.peers.len());

    // Initialize shared state
    let state = new_shared_state(local_node);

    // Initialize database
    let db = Arc::new(
        storage::Database::new(&cfg.storage.db_path)
            .await
            .expect("Failed to initialize database"),
    );

    // Start release-version polling before serving API requests.
    let upgrade_manager = UpgradeManager::new(cfg.upgrade.clone(), env!("CARGO_PKG_VERSION"))?;
    upgrade_manager.spawn_version_check_loop();

    // Clone for tasks
    let cfg = Arc::new(cfg);
    let alerts_config = cfg.alerts.clone();
    let collect_interval = cfg.metrics.collect_interval_secs;
    let top_n = cfg.metrics.top_processes_count;
    let retention_hours = cfg.storage.retention_hours;
    let tui_refresh_ms = cfg.tui.refresh_ms;

    // Task 1: Metrics collection loop
    let collect_state = Arc::clone(&state);
    let collect_db = Arc::clone(&db);
    tokio::spawn(async move {
        let mut collector = MetricsCollector::new();
        let mut interval =
            tokio::time::interval(tokio::time::Duration::from_secs(collect_interval));

        loop {
            interval.tick().await;
            let metrics = collector.collect(top_n);
            let node_id = collect_state.read().await.local_node.id;

            // Store in state
            {
                let mut s = collect_state.write().await;
                s.update_metrics(node_id, metrics.clone());
            }

            // Persist to database
            if let Err(e) = collect_db.store_metrics(&node_id, &metrics).await {
                error!("Failed to store metrics: {}", e);
            }

            // Evaluate alert rules
            alerts::evaluate_alerts(&collect_state, &alerts_config).await;
        }
    });

    // Task 2: Gossip service
    let gossip_state = Arc::clone(&state);
    let gossip_cfg = (*cfg).network.clone();
    tokio::spawn(async move {
        if let Err(e) = GossipService::run_with_rx(gossip_state, gossip_cfg).await {
            error!("Gossip service error: {}", e);
        }
    });

    // Task 3: API server
    if cfg.api.enabled {
        let api_state = Arc::clone(&state);
        let api_bind = cfg.api.bind_addr.clone();
        let api_port = cfg.api.port;
        let api_web_dir = web_dir.clone();
        let api_upgrade = upgrade_manager.clone();
        if let Some(ref dir) = api_web_dir {
            info!("  Web dashboard: http://{} (serving '{}')", api_addr, dir);
        }
        tokio::spawn(async move {
            if let Err(e) =
                api::run_api_server(api_state, api_upgrade, &api_bind, api_port, api_web_dir).await
            {
                error!("API server error: {}", e);
            }
        });
    }

    // Task 4: Database cleanup loop
    let cleanup_db = Arc::clone(&db);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(3600));
        loop {
            interval.tick().await;
            match cleanup_db.cleanup_old_metrics(retention_hours).await {
                Ok(n) => info!("Cleaned up {} old metric records", n),
                Err(e) => error!("Cleanup error: {}", e),
            }
        }
    });

    // Run TUI or just wait
    if use_tui {
        tui::run_tui(Arc::clone(&state), tui_refresh_ms).await?;
    } else {
        info!("os-watcher running. Press Ctrl+C to stop.");
        info!("Use '--tui' flag to start the terminal dashboard.");
        info!("API: http://{}/api/v1/metrics", api_addr);
        tokio::signal::ctrl_c().await?;
        info!("Shutting down...");
    }

    // Notify peers that this node is leaving so they mark it offline
    // immediately instead of waiting for the stale-peer timeout.
    broadcast_leave(&state, &cfg.network).await;

    Ok(())
}

/// Determine the IP address to advertise in gossip messages.
///
/// Priority:
/// 1. `network.advertise_addr` (explicitly configured)
/// 2. Auto-detect by opening a UDP socket toward 8.8.8.8 (no packets sent)
/// 3. Fall back to the bind_addr as-is
fn resolve_advertise_addr(cfg: &crate::config::NetworkConfig) -> String {
    if let Some(ref addr) = cfg.advertise_addr {
        return addr.clone();
    }

    // If bind_addr is a specific IP (not 0.0.0.0 / ::), use it directly.
    if cfg.bind_addr != "0.0.0.0" && cfg.bind_addr != "::" {
        return cfg.bind_addr.clone();
    }

    // Auto-detect by observing which source address the OS selects when
    // routing toward a public address. We use connect() on a UDP socket
    // (no actual packet is sent) and inspect the local address.
    if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if sock.connect("8.8.8.8:80").is_ok() {
            if let Ok(local) = sock.local_addr() {
                return local.ip().to_string();
            }
        }
    }

    cfg.bind_addr.clone()
}

fn resolve_api_addr(cfg: &crate::config::ApiConfig, advertise_host: &str) -> String {
    let host = if cfg.bind_addr == "0.0.0.0" || cfg.bind_addr == "::" {
        advertise_host
    } else {
        &cfg.bind_addr
    };
    format_host_port(host, cfg.port)
}

fn format_host_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

async fn run_status_check(api_base: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let url = format!("{}/api/v1/metrics", api_base);

    match client.get(&url).send().await {
        Ok(resp) => {
            let body = resp.text().await?;
            println!("{}", body);
        }
        Err(e) => {
            eprintln!("Failed to connect to {}: {}", api_base, e);
            eprintln!("Is os-watcher running?");
            std::process::exit(1);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ApiConfig;

    #[test]
    fn api_addr_uses_advertise_host_when_api_binds_wildcard() {
        let cfg = ApiConfig {
            bind_addr: "0.0.0.0".to_string(),
            port: 7980,
            enabled: true,
        };

        assert_eq!(resolve_api_addr(&cfg, "192.168.1.20"), "192.168.1.20:7980");
    }

    #[test]
    fn api_addr_keeps_specific_api_bind_address() {
        let cfg = ApiConfig {
            bind_addr: "10.0.0.5".to_string(),
            port: 7980,
            enabled: true,
        };

        assert_eq!(resolve_api_addr(&cfg, "192.168.1.20"), "10.0.0.5:7980");
    }
}
