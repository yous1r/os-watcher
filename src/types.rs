use std::collections::HashMap;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Unique identifier for a node in the mesh network
pub type NodeId = Uuid;

/// CPU metrics for a single sample
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuMetrics {
    /// Overall CPU usage percentage (0-100)
    pub usage_percent: f32,
    /// Per-core usage percentages
    pub core_usages: Vec<f32>,
    /// Number of logical cores
    pub core_count: usize,
}

/// Memory metrics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryMetrics {
    /// Total memory in bytes
    pub total_bytes: u64,
    /// Used memory in bytes
    pub used_bytes: u64,
    /// Available memory in bytes
    pub available_bytes: u64,
    /// Usage percentage (0-100)
    pub usage_percent: f32,
    /// Total swap in bytes
    pub swap_total_bytes: u64,
    /// Used swap in bytes
    pub swap_used_bytes: u64,
}

/// Disk I/O metrics for a single disk
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskInfo {
    pub name: String,
    /// Mount point
    pub mount_point: String,
    /// Total space in bytes
    pub total_bytes: u64,
    /// Used space in bytes
    pub used_bytes: u64,
    /// Usage percentage (0-100)
    pub usage_percent: f32,
    /// File system type
    pub fs_type: String,
    /// Bytes read since last sample
    pub read_bytes: u64,
    /// Bytes written since last sample
    pub written_bytes: u64,
    /// Read throughput in bytes per second, normalized over the sample interval
    #[serde(default)]
    pub read_bytes_per_sec: f64,
    /// Write throughput in bytes per second, normalized over the sample interval
    #[serde(default)]
    pub write_bytes_per_sec: f64,
    /// Whether the I/O figures above are this device's own counters (true) or
    /// the whole-machine total repeated on every disk (false). Only Linux
    /// exposes per-device counters; elsewhere this is false.
    #[serde(default)]
    pub per_device_io: bool,
    /// SMART health data, when available (requires smartctl and privileges)
    #[serde(default)]
    pub smart: Option<SmartInfo>,
}

/// 物理盘的介质类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiskType {
    /// 机械硬盘
    Hdd,
    /// SATA/SAS 固态盘
    Ssd,
    /// NVMe 固态盘
    Nvme,
    /// 无法判定
    Unknown,
}

/// 物理盘上的一个分区/挂载点
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Partition {
    /// 分区设备名或盘符，如 "/dev/nvme0n1p2"、"C:\\"
    pub name: String,
    /// 挂载点
    pub mount_point: String,
    /// 文件系统类型
    pub fs_type: String,
    /// 总容量（字节）
    pub total_bytes: u64,
    /// 已用容量（字节）
    pub used_bytes: u64,
    /// 使用率（0-100）
    pub usage_percent: f32,
}

/// 一块物理磁盘及其上的分区
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhysicalDisk {
    /// 物理设备路径/标识，如 "/dev/nvme0n1"、"disk0"——悬浮时展示
    pub device: String,
    /// 型号，主标题；拿不到时前端回退到设备名
    pub model: Option<String>,
    /// 介质类型
    pub disk_type: DiskType,
    /// 整盘物理容量（字节）；未知时为 0
    pub total_bytes: u64,
    /// SMART 健康数据（复用已有结构），无则 None
    pub smart: Option<SmartInfo>,
    /// 整盘读速率（字节/秒）
    pub read_bytes_per_sec: f64,
    /// 整盘写速率（字节/秒）
    pub write_bytes_per_sec: f64,
    /// I/O 是否为该盘真实计数（true）还是全机聚合（false）
    pub per_device_io: bool,
    /// 该盘上的分区/挂载点
    pub partitions: Vec<Partition>,
}

/// Overall SMART health assessment reported by the device
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SmartHealth {
    /// Device passed its self-assessment
    Passed,
    /// Device failed its self-assessment — failure may be imminent
    Failed,
    /// Could not determine health (no support, no permission, not queried)
    Unknown,
}

/// A single SMART attribute as reported by smartctl
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmartAttribute {
    /// Attribute ID (e.g. 5 = Reallocated_Sector_Ct)
    pub id: u8,
    /// Attribute name
    pub name: String,
    /// Normalized current value
    pub value: i64,
    /// Normalized worst-ever value
    pub worst: i64,
    /// Failure threshold
    pub threshold: i64,
    /// Vendor-specific raw value, as a display string
    pub raw: String,
    /// Whether this attribute has failed (value <= threshold)
    pub failing: bool,
}

/// SMART health data for a physical device
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmartInfo {
    /// Physical device path queried (e.g. /dev/sda, /dev/nvme0)
    pub device: String,
    /// Device model, when reported
    pub model: Option<String>,
    /// Serial number, when reported
    pub serial: Option<String>,
    /// Firmware version, when reported
    pub firmware: Option<String>,
    /// Rotation rate in RPM; 0 or None indicates an SSD
    pub rotation_rate: Option<u32>,
    /// Overall self-assessment result
    pub health: SmartHealth,
    /// Composite temperature in Celsius, when reported
    pub temperature_celsius: Option<i32>,
    /// Power-on time in hours, when reported
    pub power_on_hours: Option<u64>,
    /// Power cycle count, when reported
    pub power_cycle_count: Option<u64>,
    /// Reallocated sector count (spinning disks) — nonzero indicates wear
    pub reallocated_sectors: Option<u64>,
    /// SSD/NVMe estimated lifetime used, as a percentage (0-100)
    pub percentage_used: Option<u8>,
    /// Total bytes written over device lifetime, when reported
    pub data_units_written_bytes: Option<u64>,
    /// Notable SMART attributes (SATA devices)
    #[serde(default)]
    pub attributes: Vec<SmartAttribute>,
    /// When this SMART sample was taken
    pub collected_at: DateTime<Utc>,
}

impl SmartInfo {
    /// Whether this device reports any condition worth alerting on.
    pub fn is_unhealthy(&self) -> bool {
        self.health == SmartHealth::Failed
            || self.attributes.iter().any(|a| a.failing)
            || self.percentage_used.is_some_and(|p| p >= 90)
            || self.reallocated_sectors.is_some_and(|c| c > 0)
    }
}

/// Network interface metrics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkInterface {
    pub name: String,
    /// Bytes received since last sample
    pub received_bytes: u64,
    /// Bytes transmitted since last sample
    pub transmitted_bytes: u64,
    /// Total bytes received
    pub total_received_bytes: u64,
    /// Total bytes transmitted
    pub total_transmitted_bytes: u64,
    /// Download rate in bytes/sec, derived from the sample delta and the
    /// actual elapsed time between collections
    #[serde(default)]
    pub rx_bytes_per_sec: f64,
    /// Upload rate in bytes/sec
    #[serde(default)]
    pub tx_bytes_per_sec: f64,
}

/// System load averages (Unix-like)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadAverage {
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
}

/// Process information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    pub cpu_usage: f32,
    pub memory_bytes: u64,
    pub status: String,
    /// Disk read rate in bytes per second
    #[serde(default)]
    pub disk_read_bps: f64,
    /// Disk write rate in bytes per second
    #[serde(default)]
    pub disk_write_bps: f64,
}

/// Complete snapshot of all system metrics at a point in time
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemMetrics {
    pub timestamp: DateTime<Utc>,
    pub cpu: CpuMetrics,
    pub memory: MemoryMetrics,
    pub disks: Vec<DiskInfo>,
    /// 物理磁盘视图（每块盘含其分区）。新增字段，旧节点数据缺失时为空。
    #[serde(default)]
    pub physical_disks: Vec<PhysicalDisk>,
    pub networks: Vec<NetworkInterface>,
    pub load_average: Option<LoadAverage>,
    /// Top processes by CPU usage
    pub top_processes: Vec<ProcessInfo>,
    /// System uptime in seconds
    pub uptime_seconds: u64,
    /// Operating system name
    pub os_name: String,
    /// Hostname
    pub hostname: String,
}

/// State of a node in the mesh
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum NodeStatus {
    Online,
    Offline,
    Unknown,
}

/// Information about a peer node
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    pub id: NodeId,
    pub hostname: String,
    /// REST API address (host:port)
    pub api_addr: String,
    /// Gossip/discovery address (host:port)
    pub gossip_addr: String,
    pub status: NodeStatus,
    pub last_seen: DateTime<Utc>,
    /// Protocol version for compatibility
    pub version: String,
}

/// A complete node entry with its latest metrics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSnapshot {
    pub info: NodeInfo,
    pub metrics: Option<SystemMetrics>,
}

/// Alert severity levels
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum AlertSeverity {
    Info,
    Warning,
    Critical,
}

/// An alert condition that has been triggered
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alert {
    pub id: Uuid,
    pub node_id: NodeId,
    pub rule_name: String,
    pub severity: AlertSeverity,
    pub message: String,
    pub triggered_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
    pub value: f64,
    pub threshold: f64,
}

/// Gossip message types for inter-node communication
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GossipMessage {
    /// Node announcing its presence and info
    NodeAnnounce(NodeInfo),
    /// Node sharing its metrics with peers
    MetricsUpdate {
        node_id: NodeId,
        metrics: Box<SystemMetrics>,
    },
    /// Node announcing it's going offline
    NodeLeave(NodeId),
    /// Ping to check node liveness
    Ping { from: NodeId },
    /// Response to a Ping
    Pong { from: NodeId, to: NodeId },
    /// Request all known nodes (for initial join)
    SyncRequest { from: NodeId },
    /// Response with known nodes
    SyncResponse {
        from: NodeId,
        nodes: Vec<NodeInfo>,
        metrics: HashMap<NodeId, SystemMetrics>,
    },
}

impl Default for LoadAverage {
    fn default() -> Self {
        Self {
            one: 0.0,
            five: 0.0,
            fifteen: 0.0,
        }
    }
}
