mod alerts;
mod api;
mod auth;
mod collector;
mod config;
mod deploy;
mod disk_health;
mod diskstats;
mod gossip;
mod http;
mod notify;
mod service;
mod smart;
mod state;
mod storage;
mod tui;
mod types;
mod uninstall;
mod upgrade;

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use std::{path::Path, path::PathBuf, sync::Arc};
use tracing::{error, info};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

use crate::collector::MetricsCollector;
use crate::config::{generate_default_config, load_config, Config, ConfigProfile, PackageKind};
use crate::gossip::{broadcast_leave, GossipService};
use crate::state::new_shared_state;
use crate::types::{NodeInfo, NodeStatus};
use crate::upgrade::{UpgradeHelperRequest, UpgradeManager};

#[derive(Parser, Clone)]
#[command(
    name = "os-watcher",
    about = "Decentralized host resource monitor",
    version = env!("CARGO_PKG_VERSION")
)]
struct Cli {
    /// Path to config file
    #[arg(short, long, default_value = "config.toml")]
    config: String,

    /// Log level (trace, debug, info, warn, error)
    #[arg(short, long, default_value = "info")]
    log_level: String,

    /// Append logs to this file instead of writing them to stdout.
    /// A Windows service defaults to `os-watcher.log` next to the executable,
    /// because a service has no console to log to.
    #[arg(long)]
    log_file: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Clone)]
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
    /// Rewrite the package-dependent keys of the config in place.
    ///
    /// Installing a package keeps the existing `config.toml`, so after a
    /// node/full switch the config still describes the old package. This
    /// updates `[web]` and `[upgrade] package` to match, leaving every other
    /// setting and comment untouched. The deploy scripts call it after
    /// unpacking a package.
    ReconcileConfig {
        /// Package flavour the installed files belong to
        #[arg(long, value_enum)]
        package: PackageKind,
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
    /// Internal helper that removes the installation after the agent exits.
    #[command(hide = true)]
    UninstallHelper {
        #[arg(long)]
        install_dir: PathBuf,
        #[arg(long)]
        service_name: String,
        /// Back up `config.toml` before removing the tree
        #[arg(long)]
        backup: bool,
        /// Keep `config.toml` instead of deleting it
        #[arg(long)]
        keep_config: bool,
        #[arg(long)]
        backup_dir: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // A process the SCM launched has to talk to the SCM before doing anything
    // else; `dispatch` blocks until the service stops. It returns `false` for
    // an ordinary console launch, which then runs as before. Keeping this out
    // of an async runtime matters: the call parks its thread for the whole
    // service lifetime.
    if service::dispatch(&cli)? {
        return Ok(());
    }

    // The guard owns the background writer thread, so it has to outlive the
    // run; dropping it early silently loses the tail of the log.
    let _log_guard = init_logging(&cli.log_level, cli.log_file.clone())?;

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build the async runtime")?
        .block_on(run(cli))
}

/// Install the tracing subscriber, to stdout or to a log file.
///
/// Returns a guard that must stay alive for as long as logging is wanted: it
/// owns the background writer behind `--log-file`. Without a log file the guard
/// is `None` and logs go to stdout, which is what the console path wants.
pub fn init_logging(level: &str, log_file: Option<PathBuf>) -> Result<Option<WorkerGuard>> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));

    let Some(path) = log_file else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .compact()
            .init();
        return Ok(None);
    };

    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create log directory {}", dir.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("log file path {} names no file", path.display()))?;

    let (writer, guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::never(dir, name));
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .compact()
                // Escape codes would just be noise in a file.
                .with_ansi(false)
                .with_writer(writer),
        )
        .init();

    Ok(Some(guard))
}

/// Everything that needs a runtime, shared by the console and service paths.
async fn run(cli: Cli) -> Result<()> {
    match cli.command.clone().unwrap_or(Commands::Start {
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

        Commands::ReconcileConfig { package } => {
            let path = service::anchor(&cli.config);
            if reconcile_config_file(&path, package)? {
                info!("Updated {} for the {package} package", path.display());
            } else {
                info!(
                    "{} already matches the {package} package",
                    path.display()
                );
            }
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

        Commands::UninstallHelper {
            install_dir,
            service_name,
            backup,
            keep_config,
            backup_dir,
        } => {
            // Only reached on Linux: there the helper is this binary, since
            // unlinking a running executable is allowed. Windows uses a
            // detached PowerShell script instead.
            let options = uninstall::UninstallOptions {
                install_dir,
                service_name,
                backup,
                keep_config,
                backup_dir,
            };
            uninstall::run_uninstall(&options).await?;
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
            let config_path = service::anchor(&cli.config);
            let mut cfg = match load_config(&config_path.to_string_lossy()) {
                Ok(c) => {
                    info!("Loaded config from {}", config_path.display());
                    c
                }
                Err(_) => {
                    info!("No config file found, using defaults");
                    Config::default()
                }
            };

            // A service inherits `System32` as its working directory, so the
            // relative paths a released config ships with would put the
            // database there and look for the dashboard there. A console run
            // keeps its own directory, so this is a service-only rewrite.
            anchor_relative_paths(&mut cfg, service::install_dir().as_deref());

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

/// Rewrite the package-dependent keys of the config at `path`.
///
/// Returns whether the file changed. A missing file is left missing rather
/// than created: the deploy scripts call this after unpacking a package, and
/// the first install has already written its own config by then.
fn reconcile_config_file(path: &Path, package: PackageKind) -> Result<bool> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };

    let reconciled = crate::config::reconcile_package_config(&text, package)?;
    if reconciled == text {
        return Ok(false);
    }
    std::fs::write(path, reconciled).with_context(|| format!("write {}", path.display()))?;
    Ok(true)
}

fn parse_package_kind(value: &str) -> Result<PackageKind> {
    match value {
        "node" => Ok(PackageKind::Node),
        "full" => Ok(PackageKind::Full),
        _ => Err(anyhow::anyhow!("invalid package kind: {value}")),
    }
}

/// Rewrite the config's relative paths to sit under `base`.
///
/// `None` means "not a service", which leaves the config untouched so a console
/// run keeps resolving against its own working directory.
fn anchor_relative_paths(cfg: &mut Config, base: Option<&Path>) {
    let Some(base) = base else {
        return;
    };

    cfg.storage.db_path = service::anchor_to(base, Path::new(&cfg.storage.db_path))
        .to_string_lossy()
        .into_owned();
    cfg.web.dir = service::anchor_to(base, Path::new(&cfg.web.dir))
        .to_string_lossy()
        .into_owned();
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

    // Push notifications and admin sessions. Channels live in the database, so
    // the service only needs the global switch and egress settings.
    let notifier = notify::NotificationService::new(Arc::clone(&db), &cfg.notify)?;
    let auth_manager = auth::AuthManager::new(&cfg.auth);

    // Restore alerts that were still active when the process last stopped, so a
    // restart (including a self-upgrade) does not silently drop them.
    let restored_alerts = db.load_active_alerts().await.unwrap_or_default();
    if !restored_alerts.is_empty() {
        info!(
            "Restored {} active alert(s) from database",
            restored_alerts.len()
        );
        state.write().await.restore_alerts(restored_alerts);
    }

    // Bind every listening socket before anything is spawned. A port that is
    // already taken has to fail startup, not surface later as a log line while
    // the process claims to be running: a service that dies right after
    // `sc.exe start` returned success leaves the operator with nothing to go on.
    let gossip_socket = GossipService::bind_socket(&cfg.network).await?;
    let api_listener = if cfg.api.enabled {
        Some(api::bind_listener(&cfg.api.bind_addr, cfg.api.port).await?)
    } else {
        None
    };

    // The ports are held, so the node is genuinely up. A service reports
    // RUNNING only now, which makes `sc.exe start` fail on a bad config.
    service::report_ready();

    // Clone for tasks
    let cfg = Arc::new(cfg);
    let alerts_config = cfg.alerts.clone();
    let collect_interval = cfg.metrics.collect_interval_secs;
    let top_n = cfg.metrics.top_processes_count;
    let retention_hours = cfg.storage.retention_hours;
    let alert_history_minutes = cfg.storage.alert_history_minutes;
    let tui_refresh_ms = cfg.tui.refresh_ms;

    // Task 1: Metrics collection loop
    let collect_state = Arc::clone(&state);
    let collect_db = Arc::clone(&db);
    let collect_notifier = notifier.clone();
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
            alerts::evaluate_alerts(
                &collect_state,
                &alerts_config,
                &collect_db,
                &collect_notifier,
                alert_history_minutes,
            )
            .await;
        }
    });

    // Task 2: Gossip service
    let gossip_state = Arc::clone(&state);
    let gossip_cfg = cfg.network.clone();
    let gossip_db = Arc::clone(&db);
    tokio::spawn(async move {
        if let Err(e) =
            GossipService::run_with_socket(gossip_state, gossip_cfg, gossip_db, gossip_socket).await
        {
            error!("Gossip service error: {}", e);
        }
    });

    // Task 3: API server
    if let Some(api_listener) = api_listener {
        let api_state = Arc::clone(&state);
        let api_web_dir = web_dir.clone();
        let api_upgrade = upgrade_manager.clone();
        let api_upgrade_config = cfg.upgrade.clone();
        let api_deploy = cfg.deploy.clone();
        let api_gossip_addr = gossip_addr.clone();
        let api_db = Arc::clone(&db);
        let api_notify = notifier.clone();
        let api_auth = auth_manager.clone();
        if let Some(dir) = &api_web_dir {
            info!("  Web dashboard: http://{} (serving '{}')", api_addr, dir);
        }
        tokio::spawn(async move {
            if let Err(e) = api::run_api_server(
                api_state,
                api_upgrade,
                api_upgrade_config,
                api_deploy,
                api_gossip_addr,
                api_db,
                api_notify,
                api_auth,
                alert_history_minutes,
                api_listener,
                api_web_dir,
            )
            .await
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
            // Deletes only free pages, so the file keeps whatever peak it reached
            // until it is compacted. Reclaim the space on the same cadence.
            if let Err(e) = cleanup_db.enforce_capacity().await {
                error!("Capacity enforcement error: {}", e);
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
        // A service never sees Ctrl+C: the SCM signals a stop instead, and the
        // handler that received it is waiting on this future.
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            () = service::stopped() => {}
        }
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

    /// The released configs use `db_path = "os-watcher.db"` and `web.dir =
    /// "web-dist"`. Under a service those must land in the install directory
    /// rather than `System32`.
    #[test]
    fn service_mode_anchors_relative_paths_to_the_install_directory() {
        let mut cfg = Config::default();
        cfg.storage.db_path = "os-watcher.db".to_string();
        cfg.web.dir = "web-dist".to_string();

        anchor_relative_paths(&mut cfg, Some(Path::new(r"C:\Program Files\os-watcher")));

        assert_eq!(
            cfg.storage.db_path,
            r"C:\Program Files\os-watcher\os-watcher.db"
        );
        assert_eq!(cfg.web.dir, r"C:\Program Files\os-watcher\web-dist");
    }

    /// A console run is untouched, and an operator's absolute paths survive
    /// either way.
    #[test]
    fn console_mode_and_absolute_paths_are_left_alone() {
        let mut cfg = Config::default();
        cfg.storage.db_path = "os-watcher.db".to_string();

        anchor_relative_paths(&mut cfg, None);
        assert_eq!(cfg.storage.db_path, "os-watcher.db");

        cfg.storage.db_path = r"D:\data\os-watcher.db".to_string();
        anchor_relative_paths(&mut cfg, Some(Path::new(r"C:\Program Files\os-watcher")));
        assert_eq!(cfg.storage.db_path, r"D:\data\os-watcher.db");
    }
}
