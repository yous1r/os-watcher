mod types;
mod config;
mod collector;
mod diskstats;
mod state;
mod gossip;
mod api;
mod alerts;
mod tui;
mod storage;
mod smart;

use std::sync::Arc;
use chrono::Utc;
use clap::{Parser, Subcommand};
use tracing::{info, error};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;
use anyhow::Result;

use crate::config::{Config, load_config, generate_default_config};
use crate::collector::MetricsCollector;
use crate::gossip::GossipService;
use crate::state::new_shared_state;
use crate::types::{NodeInfo, NodeStatus};

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
        /// Serve the web dashboard (static frontend) alongside the API
        #[arg(long)]
        web: bool,
        /// Directory of built web assets (defaults to "web/dist")
        #[arg(long, default_value = "web/dist")]
        web_dir: String,
    },
    /// Print default configuration to stdout
    GenConfig,
    /// Show status of all known nodes (requires a running agent)
    Status {
        /// Agent API address
        #[arg(long, default_value = "http://127.0.0.1:7980")]
        api: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Init logging
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(&cli.log_level))
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
        web_dir: "web/dist".to_string(),
    }) {
        Commands::GenConfig => {
            print!("{}", generate_default_config());
        }

        Commands::Status { api } => {
            run_status_check(&api).await?;
        }

        Commands::Start { gossip_port, api_port, peers, tui: use_tui, web, web_dir } => {
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
                let extra: Vec<String> = peers_str.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                cfg.network.peers.extend(extra);
            }

            // Resolve web dashboard directory: enabled only with --web.
            let web_dir = if web { Some(web_dir) } else { None };

            run_agent(cfg, use_tui, web_dir).await?;
        }
    }

    Ok(())
}

async fn run_agent(cfg: Config, use_tui: bool, web_dir: Option<String>) -> Result<()> {
    let hostname = sysinfo::System::host_name().unwrap_or_else(|| "unknown".to_string());
    let node_name = cfg.node.name.clone().unwrap_or_else(|| hostname.clone());
    let node_id = Uuid::new_v4();

    let api_addr = format!("{}:{}", cfg.api.bind_addr, cfg.api.port);
    let gossip_addr = format!("{}:{}", cfg.network.bind_addr, cfg.network.gossip_port);

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
    info!("  Gossip: udp://{}", gossip_addr);
    info!("  Peers configured: {}", cfg.network.peers.len());

    // Initialize shared state
    let state = new_shared_state(local_node);

    // Initialize database
    let db = Arc::new(
        storage::Database::new(&cfg.storage.db_path).await
            .expect("Failed to initialize database")
    );

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
        let mut interval = tokio::time::interval(
            tokio::time::Duration::from_secs(collect_interval)
        );

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
        if let Some(ref dir) = api_web_dir {
            info!("  Web dashboard: http://{} (serving '{}')", api_addr, dir);
        }
        tokio::spawn(async move {
            if let Err(e) = api::run_api_server(api_state, &api_bind, api_port, api_web_dir).await {
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
        tui::run_tui(state, tui_refresh_ms).await?;
    } else {
        info!("os-watcher running. Press Ctrl+C to stop.");
        info!("Use '--tui' flag to start the terminal dashboard.");
        info!("API: http://{}/api/v1/metrics", api_addr);
        tokio::signal::ctrl_c().await?;
        info!("Shutting down...");
    }

    Ok(())
}

/// Query a running agent's API and print status
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
