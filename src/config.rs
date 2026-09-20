use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use toml_edit::{DocumentMut, Item, Table, TableLike, Value};

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
    /// Directory holding the built frontend assets. A relative path resolves
    /// against the working directory, or against the install directory when
    /// running as a service. Release packages unpack their assets into
    /// `web-dist`; a source checkout builds them into `web/dist`.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
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

/// Directory a release package unpacks its frontend bundle into.
///
/// `web.dir` has to name it for the dashboard to be found: a release install
/// has no `web/dist`, which is the source-checkout default.
pub const WEB_DIST_DIR: &str = "web-dist";

/// Rewrite the package-dependent keys of a config so they describe `package`.
///
/// Installing a package replaces the files it ships but deliberately keeps the
/// user's `config.toml`, which leaves the two disagreeing after a node/full
/// switch: the dashboard stays disabled after installing the full package, and
/// the full package's bundle keeps being referenced after switching to node.
/// Only the keys the package decides are touched; everything else, comments
/// included, survives untouched.
pub fn reconcile_package_config(text: &str, package: PackageKind) -> anyhow::Result<String> {
    // `toml_edit` drops carriage returns when it re-renders, so normalize
    // before parsing and restore the file's own line endings afterwards.
    let crlf = text.contains("\r\n");
    let normalized = text.replace("\r\n", "\n");
    let mut doc =
        DocumentMut::from_str(&normalized).context("parse config TOML for reconciliation")?;

    let web = ensure_section(&mut doc, "web")?;
    if package == PackageKind::Full {
        set_value(web, "enabled", Value::from(true));
        // A relative dir is anchored to the install directory, which is where
        // the bundle lands. An absolute path is an operator's own out-of-tree
        // bundle, so it is left alone.
        let managed = web
            .get("dir")
            .and_then(|item| item.as_str())
            .map(|dir| Path::new(dir).is_relative())
            .unwrap_or(true);
        if managed {
            set_value(web, "dir", Value::from(WEB_DIST_DIR));
        }
    } else {
        set_value(web, "enabled", Value::from(false));
    }

    // The next self-upgrade installs the package this config describes.
    let upgrade = ensure_section(&mut doc, "upgrade")?;
    set_value(upgrade, "package", Value::from(package.as_str()));
    set_package_comment(upgrade, package);

    let out = doc.to_string();
    let out = if crlf { out.replace('\n', "\r\n") } else { out };

    // A config that loaded before must still load, or an upgrade would write
    // one the agent silently falls back to defaults on. A file that was
    // already incomplete is the user's own state: reconciliation keeps its
    // edits rather than failing the whole install over it.
    if toml::from_str::<Config>(&normalized).is_ok() {
        toml::from_str::<Config>(&out)
            .map_err(|err| anyhow!("reconciled config no longer parses: {err}"))?;
    }
    Ok(out)
}

/// Borrow `[name]`, adding an empty table when the section is missing.
fn ensure_section<'a>(doc: &'a mut DocumentMut, name: &str) -> anyhow::Result<&'a mut dyn TableLike> {
    let root = doc.as_table_mut();
    if !root.contains_key(name) {
        root.insert(name, Item::Table(Table::new()));
    }
    root.get_mut(name)
        .and_then(|item| item.as_table_like_mut())
        .ok_or_else(|| anyhow!("[{name}] in the config is not a table"))
}

/// Set `key` in `section`, keeping the old value's decor.
///
/// Replacing an existing `Value` in place is what preserves the inline comment
/// and indentation around it; only a key that is not there yet is inserted.
fn set_value(section: &mut dyn TableLike, key: &str, new: Value) {
    match section.get_mut(key).and_then(|item| item.as_value_mut()) {
        Some(value) => {
            let decor = value.decor().clone();
            *value = new;
            *value.decor_mut() = decor;
        }
        None => {
            section.insert(key, Item::Value(new));
        }
    }
}

/// The comment each release template ships on `[upgrade] package`. The wording
/// names the package, so rewriting the value without it leaves the file
/// contradicting itself.
const PACKAGE_COMMENT_NODE: &str = "默认升级普通节点包";
const PACKAGE_COMMENT_FULL: &str = "默认升级带 Web 面板的完整包";

/// Point the inline comment on `[upgrade] package` at `package`.
///
/// Only the two wordings the release templates ship are swapped, so an
/// operator's own comment is left exactly as written.
fn set_package_comment(section: &mut dyn TableLike, package: PackageKind) {
    let (from, to) = match package {
        PackageKind::Full => (PACKAGE_COMMENT_NODE, PACKAGE_COMMENT_FULL),
        PackageKind::Node => (PACKAGE_COMMENT_FULL, PACKAGE_COMMENT_NODE),
    };

    let Some(value) = section.get_mut("package").and_then(|item| item.as_value_mut()) else {
        return;
    };
    let replaced = {
        let Some(comment) = value.decor().suffix().and_then(|suffix| suffix.as_str()) else {
            return;
        };
        if !comment.contains(from) {
            return;
        }
        comment.replace(from, to)
    };
    value.decor_mut().set_suffix(replaced);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(template: &str) -> Config {
        toml::from_str(template).expect("template must be valid config TOML")
    }

    /// The sections a config needs before it loads at all, so a fragment can
    /// be reconciled and then checked the way the agent loads it.
    const REQUIRED_SECTIONS: &str = "[node]\n[metrics]\n[network]\n[api]\n[storage]\n";

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
    fn reconcile_to_full_enables_the_dashboard_in_a_node_config() {
        let reconciled = reconcile_package_config(NODE_CONFIG_TEMPLATE, PackageKind::Full)
            .expect("reconciling a released config must succeed");

        let cfg = parse(&reconciled);
        assert!(cfg.web.enabled, "the full package must serve its dashboard");
        assert_eq!(cfg.web.dir, WEB_DIST_DIR);
        assert_eq!(cfg.upgrade.package, PackageKind::Full);
        assert!(
            reconciled.contains("dir = \"web-dist\""),
            "an added key must render as a normal assignment: {reconciled}"
        );
    }

    #[test]
    fn reconcile_to_node_disables_the_dashboard_in_a_full_config() {
        let reconciled = reconcile_package_config(FULL_CONFIG_TEMPLATE, PackageKind::Node)
            .expect("reconciling a released config must succeed");

        let cfg = parse(&reconciled);
        assert!(!cfg.web.enabled, "the node package ships no bundle to serve");
        assert_eq!(cfg.upgrade.package, PackageKind::Node);
    }

    #[test]
    fn reconcile_is_idempotent_for_the_same_package() {
        let once = reconcile_package_config(NODE_CONFIG_TEMPLATE, PackageKind::Full)
            .expect("reconcile must succeed");
        let twice =
            reconcile_package_config(&once, PackageKind::Full).expect("reconcile must succeed");

        assert_eq!(once, twice, "a second upgrade must not keep rewriting");
    }

    /// The config holds an operator's own settings and comments; only the keys
    /// the package decides may change.
    #[test]
    fn reconcile_keeps_unrelated_settings_and_comments() {
        let original = "\
# 本机配置
[node]
name = \"my-server\"

[metrics]
[network]
[api]

[web]
enabled = true   # 面板开关
dir = \"web-dist\"

[upgrade]
package = \"full\"
github_repo = \"someone/fork\"
proxy = \"http://10.0.0.142:10808\"

[storage]
db_path = \"D:/data/os-watcher.db\"
";
        let reconciled =
            reconcile_package_config(original, PackageKind::Node).expect("reconcile must succeed");

        assert!(reconciled.contains("# 本机配置"));
        assert!(
            reconciled.contains("enabled = false   # 面板开关"),
            "the inline comment must stay on the rewritten line: {reconciled}"
        );
        let cfg = parse(&reconciled);
        assert_eq!(cfg.node.name.as_deref(), Some("my-server"));
        assert_eq!(cfg.upgrade.github_repo, "someone/fork");
        assert_eq!(
            cfg.upgrade.proxy.as_deref(),
            Some("http://10.0.0.142:10808")
        );
        assert_eq!(cfg.storage.db_path, "D:/data/os-watcher.db");
    }

    /// An out-of-tree dashboard is a deliberate choice, not a package default.
    #[test]
    fn reconcile_leaves_an_absolute_web_dir_alone() {
        let absolute = if cfg!(windows) {
            "D:/dashboard"
        } else {
            "/opt/dashboard"
        };
        let original = format!("{REQUIRED_SECTIONS}[web]\nenabled = false\ndir = \"{absolute}\"\n");

        let reconciled =
            reconcile_package_config(&original, PackageKind::Full).expect("reconcile must succeed");

        let cfg = parse(&reconciled);
        assert!(cfg.web.enabled);
        assert_eq!(cfg.web.dir, absolute);
    }

    /// The shipped comment names the package, so a rewritten value must not
    /// leave the line describing the package it just stopped being.
    #[test]
    fn reconcile_retargets_the_package_comment() {
        let original = format!(
            "{REQUIRED_SECTIONS}[web]\nenabled = false\n\n[upgrade]\npackage = \"node\"                # {PACKAGE_COMMENT_NODE}\n"
        );

        let reconciled =
            reconcile_package_config(&original, PackageKind::Full).expect("reconcile must succeed");

        assert_eq!(parse(&reconciled).upgrade.package, PackageKind::Full);
        assert!(
            reconciled.contains(PACKAGE_COMMENT_FULL),
            "got: {reconciled}"
        );
        assert!(
            !reconciled.contains(PACKAGE_COMMENT_NODE),
            "the stale comment must be gone: {reconciled}"
        );

        // And back again, so a downgrade is not left naming the full package.
        let back =
            reconcile_package_config(&reconciled, PackageKind::Node).expect("reconcile must succeed");
        assert_eq!(parse(&back).upgrade.package, PackageKind::Node);
        assert!(back.contains(PACKAGE_COMMENT_NODE), "got: {back}");
        assert!(!back.contains(PACKAGE_COMMENT_FULL), "got: {back}");
    }

    /// An operator's own comment must survive; only the shipped wording moves.
    #[test]
    fn reconcile_keeps_an_operator_comment_on_the_package_key() {
        let original =
            format!("{REQUIRED_SECTIONS}[upgrade]\npackage = \"node\"  # 我这台是采集机\n");

        let reconciled =
            reconcile_package_config(&original, PackageKind::Full).expect("reconcile must succeed");

        assert!(
            reconciled.contains("# 我这台是采集机"),
            "got: {reconciled}"
        );
    }

    #[test]
    fn reconcile_creates_missing_sections_as_tables() {
        let original = "[node]\n[metrics]\n[network]\n[api]\n[storage]\n";

        let reconciled =
            reconcile_package_config(original, PackageKind::Full).expect("reconcile must succeed");

        assert!(reconciled.contains("[web]"), "got: {reconciled}");
        assert!(
            !reconciled.contains("web = {"),
            "an added section must not become an inline table: {reconciled}"
        );
        let cfg = parse(&reconciled);
        assert!(cfg.web.enabled);
        assert_eq!(cfg.upgrade.package, PackageKind::Full);
    }

    /// A release config is written on Windows and edited on Linux (and the
    /// other way round); reconciliation must not silently convert it.
    #[test]
    fn reconcile_preserves_the_files_line_endings() {
        let crlf = format!("{REQUIRED_SECTIONS}[web]\nenabled = false\n").replace('\n', "\r\n");
        let reconciled =
            reconcile_package_config(&crlf, PackageKind::Full).expect("reconcile must succeed");
        assert!(parse(&reconciled).web.enabled);
        assert_eq!(
            reconciled.matches('\n').count(),
            reconciled.matches("\r\n").count(),
            "no bare LF may appear in a CRLF config"
        );

        let lf = format!("{REQUIRED_SECTIONS}[web]\nenabled = true\n");
        let reconciled =
            reconcile_package_config(&lf, PackageKind::Node).expect("reconcile must succeed");
        assert!(!parse(&reconciled).web.enabled);
        assert!(!reconciled.contains('\r'), "an LF config must stay LF");
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
