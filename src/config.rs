use serde::{Deserialize, Serialize};

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
    /// Database settings
    pub storage: StorageConfig,
    /// Alert rules
    #[serde(default)]
    pub alerts: Vec<AlertRule>,
    /// TUI settings
    #[serde(default)]
    pub tui: TuiConfig,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    /// SQLite database file path
    #[serde(default = "default_db_path")]
    pub db_path: String,
    /// How long to retain metrics history (hours)
    #[serde(default = "default_retention_hours")]
    pub retention_hours: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuiConfig {
    /// Refresh interval for TUI display (milliseconds)
    #[serde(default = "default_tui_refresh_ms")]
    pub refresh_ms: u64,
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
fn default_node_role() -> String { "both".to_string() }
fn default_collect_interval() -> u64 { 5 }
fn default_top_processes() -> usize { 10 }
fn default_true() -> bool { true }
fn default_gossip_port() -> u16 { 7979 }
fn default_announce_interval() -> u64 { 30 }
fn default_gossip_interval() -> u64 { 10 }
fn default_max_hops() -> u8 { 3 }
fn default_bind_addr() -> String { "0.0.0.0".to_string() }
fn default_api_port() -> u16 { 7980 }
fn default_db_path() -> String { "os-watcher.db".to_string() }
fn default_retention_hours() -> u64 { 168 } // 7 days
fn default_tui_refresh_ms() -> u64 { 1000 }
fn default_consecutive_violations() -> u32 { 1 }
fn default_severity() -> String { "warning".to_string() }

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
            storage: StorageConfig {
                db_path: default_db_path(),
                retention_hours: default_retention_hours(),
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
                    message: Some("Memory usage is {value:.1}% (threshold: {threshold}%)".to_string()),
                },
            ],
            tui: TuiConfig {
                refresh_ms: default_tui_refresh_ms(),
            },
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

/// Load config from a TOML file, falling back to defaults
pub fn load_config(path: &str) -> anyhow::Result<Config> {
    let content = std::fs::read_to_string(path)?;
    let config: Config = toml::from_str(&content)?;
    Ok(config)
}

/// Generate a default config file
pub fn generate_default_config() -> String {
    r#"# os-watcher configuration

[node]
# name = "my-server"  # defaults to hostname
role = "both"  # "agent", "server", or "both"

[metrics]
collect_interval_secs = 5
top_processes_count = 10
collect_processes = true

[network]
gossip_port = 7979
enable_discovery = true
peers = []  # add peers manually: ["192.168.1.10:7979", "192.168.1.11:7979"]
announce_interval_secs = 30
gossip_interval_secs = 10
max_hops = 3
bind_addr = "0.0.0.0"

[api]
port = 7980
bind_addr = "0.0.0.0"
enabled = true

[storage]
db_path = "os-watcher.db"
retention_hours = 168  # 7 days

[tui]
refresh_ms = 1000

# Alert rules
[[alerts]]
name = "high_cpu"
metric = "cpu"
operator = "gt"
threshold = 90.0
consecutive_violations = 3
severity = "warning"

[[alerts]]
name = "high_memory"
metric = "memory"
operator = "gt"
threshold = 90.0
consecutive_violations = 2
severity = "warning"

[[alerts]]
name = "disk_almost_full"
metric = "disk"
operator = "gt"
threshold = 85.0
consecutive_violations = 1
severity = "critical"
"#.to_string()
}
