use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

/// Root configuration structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Node-specific settings
    pub node: NodeConfig,
    /// Metrics collection settings
    pub metrics: MetricsConfig,
    /// Network/gossip settings
    pub network: NetworkConfig,
    /// REST API settings
    pub api: ApiConfig,
    /// Web dashboard settings
    #[serde(default)]
    pub web: WebConfig,
    /// Database settings
    pub storage: StorageConfig,
    /// Alert rules
    #[serde(default)]
    pub alerts: Vec<AlertRule>,
    /// TUI settings
    #[serde(default)]
    pub tui: TuiConfig,
    /// Self-upgrade settings
    #[serde(default)]
    pub upgrade: UpgradeConfig,
    /// Remote node deployment settings
    #[serde(default)]
    pub deploy: DeployConfig,
    /// Admin authentication for management endpoints
    #[serde(default)]
    pub auth: AuthConfig,
    /// Outbound push notifications
    #[serde(default)]
    pub notify: NotifyConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    /// Human-readable node name (defaults to hostname)
    pub name: Option<String>,
    /// Node role: "agent" (collect only), "server" (aggregate), "both" (default)
    #[serde(default = "default_node_role")]
    pub role: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsConfig {
    /// How often to collect metrics (seconds)
    #[serde(default = "default_collect_interval")]
    pub collect_interval_secs: u64,
    /// How many top processes to track
    #[serde(default = "default_top_processes")]
    pub top_processes_count: usize,
    /// Whether to collect per-process metrics
    #[serde(default = "default_true")]
    pub collect_processes: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// UDP port for gossip/discovery
    #[serde(default = "default_gossip_port")]
    pub gossip_port: u16,
    /// Whether to enable LAN broadcast discovery
    #[serde(default = "default_true")]
    pub enable_discovery: bool,
    /// Manually configured peer addresses (host:port)
    #[serde(default)]
    pub peers: Vec<String>,
    /// How often to broadcast presence (seconds)
    #[serde(default = "default_announce_interval")]
    pub announce_interval_secs: u64,
    /// How often to gossip metrics (seconds)
    #[serde(default = "default_gossip_interval")]
    pub gossip_interval_secs: u64,
    /// Max hops for gossip propagation
    #[serde(default = "default_max_hops")]
    pub max_hops: u8,
    /// Bind address for network listener
    #[serde(default = "default_bind_addr")]
    pub bind_addr: String,
    /// Address advertised to peers (host only, no port).
    /// Defaults to auto-detect when bind_addr is "0.0.0.0".
    /// Set this explicitly when auto-detection picks the wrong interface
    /// (e.g. multiple NICs, Docker bridge, VPN).
    #[serde(default)]
    pub advertise_addr: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiConfig {
    /// REST API port
    #[serde(default = "default_api_port")]
    pub port: u16,
    /// Bind address for API server
    #[serde(default = "default_bind_addr")]
    pub bind_addr: String,
    /// Enable API server
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// Web dashboard (static frontend) settings.
///
/// Disabled by default so plain agent nodes never try to serve a bundle they
/// do not ship. The `full` release package enables it in its example config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebConfig {
    /// Serve the built web dashboard alongside the API.
    /// The `--web` CLI flag turns this on regardless of the config value.
    #[serde(default)]
    pub enabled: bool,
    /// Directory holding the built frontend assets. When it holds no bundle,
    /// the shipped layouts (`web-dist` in a release bundle, `web/dist` in a
    /// source checkout) are probed next to the working directory and the
    /// executable, so one config works in both layouts.
    #[serde(default = "default_web_dir")]
    pub dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    /// SQLite database file path
    #[serde(default = "default_db_path")]
    pub db_path: String,
    /// How long to retain metrics history (hours)
    #[serde(default = "default_retention_hours")]
    pub retention_hours: u64,
    /// How long resolved alerts stay in the 最近恢复 list and in the
    /// `alerts_log` table before being deleted (minutes). `0` deletes them as
    /// soon as they resolve.
    #[serde(default = "default_alert_history_minutes")]
    pub alert_history_minutes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuiConfig {
    /// Refresh interval for TUI display (milliseconds)
    #[serde(default = "default_tui_refresh_ms")]
    pub refresh_ms: u64,
}

/// Release package flavour used for deploy and self-upgrade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PackageKind {
    /// Node-only package without bundled web assets
    Node,
    /// Full package with bundled web dashboard
    Full,
}

impl PackageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Full => "full",
        }
    }
}

impl Default for PackageKind {
    fn default() -> Self {
        Self::Node
    }
}

impl fmt::Display for PackageKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Self-upgrade configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpgradeConfig {
    /// Enable version polling and upgrade API.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// GitHub repository in owner/name form.
    #[serde(default = "default_upgrade_github_repo")]
    pub github_repo: String,
    /// How often to poll GitHub Releases for the latest tag.
    #[serde(default = "default_upgrade_check_interval")]
    pub check_interval_secs: u64,
    /// Default package flavour when an upgrade request omits package.
    #[serde(default)]
    pub package: PackageKind,
    /// Service name used by systemd/sc.exe restart commands.
    #[serde(default = "default_upgrade_service_name")]
    pub service_name: String,
    /// Optional proxy URL; HTTP(S)_PROXY environment variables still work.
    #[serde(default)]
    pub proxy: Option<String>,
}

/// Remote node deployment settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_deploy_install_dir")]
    pub default_install_dir: String,
    #[serde(default = "default_deploy_connect_timeout")]
    pub connect_timeout_secs: u64,
    #[serde(default = "default_deploy_max_attempts")]
    pub max_attempts: u32,
}

/// Admin authentication for management endpoints (upgrade, remote deploy, push channels).
///
/// Monitoring endpoints stay readable by guests; only management actions need a
/// session. An empty password makes every management request fail with an
/// explicit configuration hint instead of silently allowing access.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    /// Whether management endpoints require a login session.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Admin password. Empty means management endpoints reject every request.
    #[serde(default)]
    pub password: String,
    /// Session lifetime in hours.
    #[serde(default = "default_session_ttl_hours")]
    pub session_ttl_hours: u64,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            password: String::new(),
            session_ttl_hours: default_session_ttl_hours(),
        }
    }
}

/// Outbound push notification settings.
///
/// Channels themselves live in the database and are managed from the panel;
/// only the global switch and egress parameters are configured here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotifyConfig {
    /// Master switch; channels survive being switched off.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Bark server root for channels that do not carry their own address. Point
    /// this at a self-hosted bark-server to avoid retyping it per channel.
    #[serde(default = "default_notify_server_url")]
    pub server_url: String,
    /// Optional proxy URL; HTTP(S)_PROXY environment variables still work.
    #[serde(default)]
    pub proxy: Option<String>,
    /// Request timeout in seconds.
    #[serde(default = "default_notify_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            server_url: default_notify_server_url(),
            proxy: None,
            timeout_secs: default_notify_timeout_secs(),
        }
    }
}

/// An alert rule definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertRule {
    pub name: String,
    /// Metric to monitor: "cpu", "memory", "disk", "load1", "load5", "load15"
    pub metric: String,
    /// Optional: specific disk/interface name
    pub target: Option<String>,
    /// Operator: "gt", "lt", "gte", "lte"
    pub operator: String,
    /// Threshold value
    pub threshold: f64,
    /// How many consecutive violations before alerting
    #[serde(default = "default_consecutive_violations")]
    pub consecutive_violations: u32,
    /// Severity: "info", "warning", "critical"
    #[serde(default = "default_severity")]
    pub severity: String,
    /// Custom message template
    pub message: Option<String>,
}

// Default value functions
fn default_node_role() -> String {
    "both".to_string()
}
fn default_collect_interval() -> u64 {
    5
}
fn default_top_processes() -> usize {
    10
}
fn default_true() -> bool {
    true
}
fn default_gossip_port() -> u16 {
    7979
}
fn default_announce_interval() -> u64 {
    30
}
fn default_gossip_interval() -> u64 {
    10
}
fn default_max_hops() -> u8 {
    3
}
fn default_bind_addr() -> String {
    "0.0.0.0".to_string()
}
fn default_api_port() -> u16 {
    7980
}
fn default_web_dir() -> String {
    "web/dist".to_string()
}
fn default_db_path() -> String {
    "os-watcher.db".to_string()
}
fn default_retention_hours() -> u64 {
    12
} // 12 hours

/// Default window for the 最近恢复 list; the panel reads the same value from
/// `/api/v1/alerts/retention`.
pub fn default_alert_history_minutes() -> u64 {
    10
}
fn default_tui_refresh_ms() -> u64 {
    1000
}
fn default_consecutive_violations() -> u32 {
    1
}
fn default_severity() -> String {
    "warning".to_string()
}
fn default_upgrade_github_repo() -> String {
    "yous1r/os-watcher".to_string()
}
fn default_upgrade_check_interval() -> u64 {
    1800
}
fn default_upgrade_service_name() -> String {
    "os-watcher".to_string()
}
fn default_deploy_install_dir() -> String {
    "/opt/os-watcher".to_string()
}
fn default_deploy_connect_timeout() -> u64 {
    20
}
fn default_deploy_max_attempts() -> u32 {
    3
}
fn default_session_ttl_hours() -> u64 {
    12
}
fn default_notify_timeout_secs() -> u64 {
    10
}
fn default_notify_server_url() -> String {
    crate::notify::DEFAULT_SERVER_URL.to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            node: NodeConfig {
                name: None,
                role: default_node_role(),
            },
            metrics: MetricsConfig {
                collect_interval_secs: default_collect_interval(),
                top_processes_count: default_top_processes(),
                collect_processes: true,
            },
            network: NetworkConfig {
                gossip_port: default_gossip_port(),
                enable_discovery: true,
                peers: vec![],
                announce_interval_secs: default_announce_interval(),
                gossip_interval_secs: default_gossip_interval(),
                max_hops: default_max_hops(),
                bind_addr: default_bind_addr(),
                advertise_addr: None,
            },
            api: ApiConfig {
                port: default_api_port(),
                bind_addr: default_bind_addr(),
                enabled: true,
            },
            web: WebConfig::default(),
            storage: StorageConfig {
                db_path: default_db_path(),
                retention_hours: default_retention_hours(),
                alert_history_minutes: default_alert_history_minutes(),
            },
            alerts: vec![
                // Default alert rules
                AlertRule {
                    name: "high_cpu".to_string(),
                    metric: "cpu".to_string(),
                    target: None,
                    operator: "gt".to_string(),
                    threshold: 90.0,
                    consecutive_violations: 3,
                    severity: "warning".to_string(),
                    message: Some("CPU usage is {value:.1}% (threshold: {threshold}%)".to_string()),
                },
                AlertRule {
                    name: "high_memory".to_string(),
                    metric: "memory".to_string(),
                    target: None,
                    operator: "gt".to_string(),
                    threshold: 90.0,
                    consecutive_violations: 2,
                    severity: "warning".to_string(),
                    message: Some(
                        "Memory usage is {value:.1}% (threshold: {threshold}%)".to_string(),
                    ),
                },
            ],
            tui: TuiConfig {
                refresh_ms: default_tui_refresh_ms(),
            },
            upgrade: UpgradeConfig::default(),
            deploy: DeployConfig::default(),
            auth: AuthConfig::default(),
            notify: NotifyConfig::default(),
        }
    }
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            refresh_ms: default_tui_refresh_ms(),
        }
    }
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dir: default_web_dir(),
        }
    }
}

impl Default for UpgradeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            github_repo: default_upgrade_github_repo(),
            check_interval_secs: default_upgrade_check_interval(),
            package: PackageKind::Node,
            service_name: default_upgrade_service_name(),
            proxy: None,
        }
    }
}

impl Default for DeployConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            default_install_dir: default_deploy_install_dir(),
            connect_timeout_secs: default_deploy_connect_timeout(),
            max_attempts: default_deploy_max_attempts(),
        }
    }
}

/// Load config from a TOML file, falling back to defaults
pub fn load_config(path: &str) -> anyhow::Result<Config> {
    let content = std::fs::read_to_string(path)?;
    let config: Config = toml::from_str(&content)?;
    Ok(config)
}

/// Which release flavour a config template targets.
///
/// The release pipeline ships a different example config per package, so the
/// defaults a user starts from match what their package can actually do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ConfigProfile {
    /// Collect-only node without a bundled dashboard (`os-watcher-*-node`)
    Node,
    /// Node that also serves the web dashboard (`os-watcher-*-full`)
    Full,
}

/// Example config shipped in the `-node` release package.
pub const NODE_CONFIG_TEMPLATE: &str = include_str!("../config.node.example.toml");
/// Example config shipped in the `-full` release package.
pub const FULL_CONFIG_TEMPLATE: &str = include_str!("../config.full.example.toml");

/// Generate the default config file contents for a release profile.
pub fn generate_default_config(profile: ConfigProfile) -> String {
    match profile {
        ConfigProfile::Node => NODE_CONFIG_TEMPLATE.to_string(),
        ConfigProfile::Full => FULL_CONFIG_TEMPLATE.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(template: &str) -> Config {
        toml::from_str(template).expect("template must be valid config TOML")
    }

    #[test]
    fn node_template_disables_web_dashboard() {
        let cfg = parse(NODE_CONFIG_TEMPLATE);
        assert!(!cfg.web.enabled, "node package ships no frontend bundle");
        assert!(cfg.api.enabled, "API stays on for local status checks");
    }

    #[test]
    fn full_template_enables_web_dashboard() {
        let cfg = parse(FULL_CONFIG_TEMPLATE);
        assert!(cfg.web.enabled);
        // Must match the directory name the pipeline unpacks the bundle into.
        assert_eq!(cfg.web.dir, "web-dist");
    }

    #[test]
    fn profiles_generate_distinct_configs() {
        let node = generate_default_config(ConfigProfile::Node);
        let full = generate_default_config(ConfigProfile::Full);
        assert_ne!(node, full);
    }

    #[test]
    fn web_disabled_when_section_absent() {
        // Configs written before the [web] section existed must still load.
        let cfg: Config = toml::from_str(
            r#"
            [node]
            [metrics]
            [network]
            [api]
            [storage]
            "#,
        )
        .expect("legacy config without [web] should parse");
        assert!(!cfg.web.enabled);
    }

    #[test]
    fn upgrade_defaults_are_loaded_when_section_absent() {
        let cfg: Config = toml::from_str(
            r#"
            [node]
            [metrics]
            [network]
            [api]
            [storage]
            "#,
        )
        .expect("legacy config without [upgrade] should parse");

        assert!(cfg.upgrade.enabled);
        assert_eq!(cfg.upgrade.github_repo, "yous1r/os-watcher");
        assert_eq!(cfg.upgrade.package, PackageKind::Node);
    }

    #[test]
    fn deploy_defaults_are_loaded_when_section_absent() {
        let cfg: Config = toml::from_str(
            r#"
            [node]
            [metrics]
            [network]
            [api]
            [storage]
            "#,
        )
        .expect("legacy config without [deploy] should parse");

        assert!(cfg.deploy.enabled);
        assert_eq!(cfg.deploy.default_install_dir, "/opt/os-watcher");
        assert_eq!(cfg.deploy.connect_timeout_secs, 20);
        assert_eq!(cfg.deploy.max_attempts, 3);
    }

    #[test]
    fn partial_deploy_section_backfills_defaults() {
        let cfg: Config = toml::from_str(
            r#"
            [node]
            [metrics]
            [network]
            [api]
            [storage]
            [deploy]
            enabled = false
            "#,
        )
        .expect("partial [deploy] should parse");

        assert!(!cfg.deploy.enabled);
        assert_eq!(cfg.deploy.default_install_dir, "/opt/os-watcher");
        assert_eq!(cfg.deploy.connect_timeout_secs, 20);
        assert_eq!(cfg.deploy.max_attempts, 3);
    }

    #[test]
    fn full_template_defaults_upgrade_package_to_full() {
        let cfg = parse(FULL_CONFIG_TEMPLATE);
        assert_eq!(cfg.upgrade.package, PackageKind::Full);
    }

    #[test]
    fn auth_and_notify_defaults_are_loaded_when_section_absent() {
        let cfg: Config = toml::from_str(
            r#"
            [node]
            [metrics]
            [network]
            [api]
            [storage]
            "#,
        )
        .expect("legacy config without [auth]/[notify] should parse");

        assert!(cfg.auth.enabled, "management endpoints must be protected by default");
        assert!(cfg.auth.password.is_empty());
        assert_eq!(cfg.auth.session_ttl_hours, 12);
        assert!(cfg.notify.enabled);
        assert!(cfg.notify.proxy.is_none());
        assert_eq!(cfg.notify.timeout_secs, 10);
        assert_eq!(cfg.notify.server_url, crate::notify::DEFAULT_SERVER_URL);
        assert_eq!(
            cfg.storage.alert_history_minutes,
            default_alert_history_minutes(),
            "最近恢复 must default to a 10 minute window"
        );
    }

    #[test]
    fn alert_history_retention_is_configurable() {
        let cfg: Config = toml::from_str(
            r#"
            [node]
            [metrics]
            [network]
            [api]
            [storage]
            alert_history_minutes = 30
            "#,
        )
        .expect("config with a retention window should parse");
        assert_eq!(cfg.storage.alert_history_minutes, 30);
    }

    #[test]
    fn a_self_hosted_bark_server_is_loadable_from_the_config() {
        let cfg: Config = toml::from_str(
            r#"
            [node]
            [metrics]
            [network]
            [api]
            [storage]
            [notify]
            enabled = true
            server_url = "https://bark.example.com/bark/"
            "#,
        )
        .expect("config with a self-hosted server should parse");

        assert_eq!(cfg.notify.server_url, "https://bark.example.com/bark/");
    }

    #[test]
    fn example_configs_include_deploy_defaults_and_trusted_network_warning() {
        for template in [NODE_CONFIG_TEMPLATE, FULL_CONFIG_TEMPLATE] {
            assert!(template.contains("[deploy]"));
            assert!(template.contains("仅在可信网络内暴露"));
            let cfg = parse(template);
            assert!(cfg.deploy.enabled);
            assert_eq!(cfg.deploy.default_install_dir, "/opt/os-watcher");
            assert_eq!(cfg.deploy.connect_timeout_secs, 20);
            assert_eq!(cfg.deploy.max_attempts, 3);
        }
    }

    #[test]
    fn readme_warns_about_unauthenticated_remote_deployment_and_links_configs() {
        let readme = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md"),
        )
        .expect("root README.md should exist");
        assert!(readme.contains("仅在可信网络内暴露"));
        assert!(readme.contains("config.node.example.toml"));
        assert!(readme.contains("config.full.example.toml"));
    }
}
