// 与后端 Rust 类型对应的 TypeScript 接口定义

export interface CpuMetrics {
  usage_percent: number;
  core_usages: number[];
  core_count: number;
}

export interface MemoryMetrics {
  total_bytes: number;
  used_bytes: number;
  available_bytes: number;
  usage_percent: number;
  swap_total_bytes: number;
  swap_used_bytes: number;
}

export type SmartHealth = "Passed" | "Failed" | "Unknown";

export interface SmartAttribute {
  id: number;
  name: string;
  value: number;
  worst: number;
  threshold: number;
  raw: string;
  failing: boolean;
}

export interface SmartInfo {
  device: string;
  model: string | null;
  serial: string | null;
  firmware: string | null;
  /** 0 或 null 表示固态硬盘 */
  rotation_rate: number | null;
  health: SmartHealth;
  temperature_celsius: number | null;
  power_on_hours: number | null;
  power_cycle_count: number | null;
  reallocated_sectors: number | null;
  /** SSD/NVMe 已用寿命百分比 */
  percentage_used: number | null;
  data_units_written_bytes: number | null;
  attributes: SmartAttribute[];
  collected_at: string;
}

export interface DiskInfo {
  name: string;
  mount_point: string;
  total_bytes: number;
  used_bytes: number;
  usage_percent: number;
  fs_type: string;
  read_bytes: number;
  written_bytes: number;
  read_bytes_per_sec: number;
  write_bytes_per_sec: number;
  /** true 表示这是该设备真实的 I/O；false 表示回退到了全机聚合值 */
  per_device_io: boolean;
  smart: SmartInfo | null;
}

/** 物理盘介质类型，与后端 DiskType 枚举对应 */
export type DiskType = "Hdd" | "Ssd" | "Nvme" | "Unknown";

export interface Partition {
  name: string;
  mount_point: string;
  fs_type: string;
  total_bytes: number;
  used_bytes: number;
  usage_percent: number;
}

export interface PhysicalDisk {
  /** 设备路径/标识，如 "/dev/nvme0n1"、"disk0"——悬浮时展示 */
  device: string;
  /** 型号，主标题；为 null 时前端回退到设备名 */
  model: string | null;
  disk_type: DiskType;
  /** 整盘容量，字节；未知为 0 */
  total_bytes: number;
  smart: SmartInfo | null;
  read_bytes_per_sec: number;
  write_bytes_per_sec: number;
  /** true=按盘真实计数；false=全机聚合 */
  per_device_io: boolean;
  partitions: Partition[];
}

export interface NetworkInterface {
  name: string;
  received_bytes: number;
  transmitted_bytes: number;
  total_received_bytes: number;
  total_transmitted_bytes: number;
  /** 下行速率，字节/秒 */
  rx_bytes_per_sec: number;
  /** 上行速率，字节/秒 */
  tx_bytes_per_sec: number;
}

export interface LoadAverage {
  one: number;
  five: number;
  fifteen: number;
}

export interface ProcessInfo {
  pid: number;
  name: string;
  cpu_usage: number;
  memory_bytes: number;
  status: string;
  /** 磁盘读取速率，字节/秒 */
  disk_read_bps: number;
  /** 磁盘写入速率，字节/秒 */
  disk_write_bps: number;
}

export interface SystemMetrics {
  timestamp: string;
  cpu: CpuMetrics;
  memory: MemoryMetrics;
  disks: DiskInfo[];
  physical_disks: PhysicalDisk[];
  networks: NetworkInterface[];
  load_average: LoadAverage | null;
  top_processes: ProcessInfo[];
  uptime_seconds: number;
  os_name: string;
  hostname: string;
}

export type NodeStatus = "Online" | "Offline" | "Unknown";

export interface NodeInfo {
  id: string;
  hostname: string;
  api_addr: string;
  gossip_addr: string;
  status: NodeStatus;
  last_seen: string;
  version: string;
}

export interface NodeSnapshot {
  info: NodeInfo;
  metrics: SystemMetrics | null;
}

export type PackageKind = "node" | "full";

export type UpgradePhase =
  | "idle"
  | "checking"
  | "downloading"
  | "backing_up"
  | "installing"
  | "restarting"
  | "succeeded"
  | "failed"
  | "rolled_back";

export interface UpgradeStatus {
  running: boolean;
  phase: UpgradePhase;
  message: string;
  package: PackageKind | null;
  target_version: string | null;
  started_at: string | null;
  finished_at: string | null;
}

export interface VersionInfo {
  current: string;
  latest: string | null;
  update_available: boolean;
  checked_at: string | null;
  platform: string;
  package: PackageKind;
  upgrade: UpgradeStatus;
}

export interface UpgradeRequest {
  package?: PackageKind;
  proxy?: string;
}

export type AlertSeverity = "Info" | "Warning" | "Critical";

/** 告警被消除的原因，与后端 `AlertResolveReason` 的 snake_case 表示一致。 */
export type AlertResolveReason =
  | "condition_cleared"
  | "node_offline"
  | "node_removed"
  | "metric_unavailable";

export interface Alert {
  id: string;
  node_id: string;
  hostname: string;
  rule_name: string;
  metric: string;
  target: string | null;
  operator: string;
  severity: AlertSeverity;
  message: string;
  triggered_at: string;
  resolved_at: string | null;
  resolved_reason: AlertResolveReason | null;
  value: number;
  threshold: number;
}

/** 「最近恢复」的保留时长，对应 [storage] alert_history_minutes。 */
export interface AlertRetention {
  history_minutes: number;
}

/** 管理鉴权状态：访客只读，管理动作需要登录。 */
export interface SessionInfo {
  auth_required: boolean;
  authenticated: boolean;
}

// ---------- 推送渠道 ----------

export type BarkAlgorithm = "aes128" | "aes192" | "aes256";
export type BarkMode = "cbc" | "ecb" | "gcm";
export type NotifySeverity = "info" | "warning" | "critical";

/** 推送内容加密配置，与 Bark App 中的算法/模式/密钥/IV 一一对应。 */
export interface BarkEncryption {
  algorithm: BarkAlgorithm;
  mode: BarkMode;
  key: string;
  iv: string | null;
}

export interface BarkChannelConfig {
  kind: "bark";
  server_url: string;
  device_key: string;
  encryption: BarkEncryption | null;
}

export interface NotifyChannel {
  id: string;
  name: string;
  enabled: boolean;
  min_severity: NotifySeverity;
  config: BarkChannelConfig;
  created_at: string;
  updated_at: string;
  last_sent_at: string | null;
  last_error: string | null;
}

/** 新建/更新渠道时提交的字段。 */
export interface NotifyChannelPayload {
  name: string;
  enabled: boolean;
  min_severity: NotifySeverity;
  config: BarkChannelConfig;
}

/** 服务端配置的推送默认值（来自 config.toml 的 [notify] server_url）。 */
export interface NotifyDefaults {
  server_url: string;
}

export interface ApiResponse<T> {
  success: boolean;
  data: T;
  error?: string;
}

// ---------- 远程节点部署 ----------

/** SSH 认证方式：密码或私钥。 */
export type DeployAuth =
  | { type: "password"; password: string }
  | { type: "key"; private_key: string; passphrase: string | null };

/** 卸载请求：是否备份 config.toml、是否保留它。 */
export interface UninstallRequest {
  backup: boolean;
  keep_config: boolean;
}

/** 卸载已被接受；服务随后停止，故没有完成状态可轮询。 */
export interface UninstallResult {
  scheduled: boolean;
  backup: boolean;
  keep_config: boolean;
  message: string;
}

/** 部署请求首帧，通过 WebSocket 发送给后端。 */
export interface DeployRequest {
  host: string;
  port: number;
  username: string;
  auth: DeployAuth;
  package: PackageKind;
  api_port: number;
  gossip_port: number;
  peers: string[];
  service_name: string;
  install_dir: string;
  version: string;
  repo: string | null;
  proxy: string | null;
}

/** 部署阶段，对应后端 progress 事件的 step。 */
export type DeployStep = "connecting" | "uploading" | "installing" | "verifying";

/** 后端流式返回的部署事件，`type` 为标签。 */
export type DeployEvent =
  | { type: "progress"; step: DeployStep; message: string }
  | { type: "log"; stream: "stdout" | "stderr"; line: string }
  | { type: "retry"; attempt: number; max: number; message: string }
  | { type: "success"; message: string }
  | { type: "error"; message: string };
