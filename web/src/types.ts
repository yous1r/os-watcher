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

export type AlertSeverity = "Info" | "Warning" | "Critical";

export interface Alert {
  id: string;
  node_id: string;
  rule_name: string;
  severity: AlertSeverity;
  message: string;
  triggered_at: string;
  resolved_at: string | null;
  value: number;
  threshold: number;
}

export interface ApiResponse<T> {
  success: boolean;
  data: T;
}
