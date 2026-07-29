use std::time::Instant;

use crate::diskstats::DiskStats;
use crate::smart::SmartCollector;
use crate::types::*;
use sysinfo::{
    Disks, Networks, System,
    ProcessStatus, ProcessesToUpdate,
};
use chrono::Utc;
use tracing::debug;

/// How often to re-read SMART data. Querying devices is slow and needs
/// privileges, so it runs far less frequently than the regular metrics.
const SMART_REFRESH_SECS: u64 = 300;

pub struct MetricsCollector {
    sys: System,
    disks: Disks,
    networks: Networks,
    smart: SmartCollector,
    /// Per-device I/O counters, which sysinfo does not expose.
    diskstats: DiskStats,
    /// When the previous sample was taken, used to turn the byte deltas that
    /// sysinfo reports into per-second rates.
    last_sample: Option<Instant>,
}

/// 组装物理盘视图的中间输入：一个挂载点/分区的原始信息。
/// 由 `collect()` 从 sysinfo 的 `Disks` 逐项填充，与采集逻辑解耦以便测试。
pub struct DiskInput {
    pub name: String,
    pub mount_point: String,
    pub fs_type: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub read_bytes_per_sec: f64,
    pub write_bytes_per_sec: f64,
    pub per_device_io: bool,
}

/// 根据 SMART 的 rotation_rate 与设备名推断盘类型。
fn infer_disk_type(device: &str, smart: Option<&SmartInfo>) -> DiskType {
    let base = device.rsplit('/').next().unwrap_or(device);
    if base.starts_with("nvme") {
        return DiskType::Nvme;
    }
    match smart.and_then(|s| s.rotation_rate) {
        Some(0) => DiskType::Ssd,
        Some(_) => DiskType::Hdd,
        None => DiskType::Unknown,
    }
}

/// 无盘符映射的便捷版本（Linux/测试）。
pub fn assemble_physical_disks(
    inputs: &[DiskInput],
    smart: &std::collections::HashMap<String, SmartInfo>,
) -> Vec<PhysicalDisk> {
    let empty = std::collections::HashMap::new();
    assemble_physical_disks_with_map(inputs, smart, &empty)
}

/// 带盘符→物理盘映射的完整版本。`partition_map` 在 Windows 上把盘符
/// （`C:\`）对应到物理盘标识（`disk0`），其它平台传空即可。
pub fn assemble_physical_disks_with_map(
    inputs: &[DiskInput],
    smart: &std::collections::HashMap<String, SmartInfo>,
    partition_map: &std::collections::HashMap<String, String>,
) -> Vec<PhysicalDisk> {
    use crate::disk_health::parent_device;

    // 先为每个已知物理盘建一个空壳，key 为设备标识。
    let mut disks: std::collections::HashMap<String, PhysicalDisk> =
        std::collections::HashMap::new();
    for (device, info) in smart {
        disks.insert(
            device.clone(),
            PhysicalDisk {
                device: device.clone(),
                model: info.model.clone(),
                disk_type: infer_disk_type(device, Some(info)),
                total_bytes: 0,
                smart: Some(info.clone()),
                read_bytes_per_sec: 0.0,
                write_bytes_per_sec: 0.0,
                per_device_io: false,
                partitions: Vec::new(),
            },
        );
    }

    // 把每个挂载点归到某块物理盘。
    for input in inputs {
        let key = match_physical_disk(&input.name, smart, partition_map, parent_device);
        let disk = disks.entry(key.clone()).or_insert_with(|| PhysicalDisk {
            device: key.clone(),
            model: None,
            disk_type: DiskType::Unknown,
            total_bytes: 0,
            smart: None,
            read_bytes_per_sec: 0.0,
            write_bytes_per_sec: 0.0,
            per_device_io: false,
            partitions: Vec::new(),
        });

        if input.read_bytes_per_sec > disk.read_bytes_per_sec {
            disk.read_bytes_per_sec = input.read_bytes_per_sec;
        }
        if input.write_bytes_per_sec > disk.write_bytes_per_sec {
            disk.write_bytes_per_sec = input.write_bytes_per_sec;
        }

        disk.partitions.push(Partition {
            name: input.name.clone(),
            mount_point: input.mount_point.clone(),
            fs_type: input.fs_type.clone(),
            total_bytes: input.total_bytes,
            used_bytes: input.used_bytes,
            usage_percent: if input.total_bytes > 0 {
                (input.used_bytes as f32 / input.total_bytes as f32) * 100.0
            } else {
                0.0
            },
        });
    }

    let mut out: Vec<PhysicalDisk> = disks.into_values().collect();
    out.sort_by(|a, b| {
        let a_unknown = a.device == "unknown";
        let b_unknown = b.device == "unknown";
        a_unknown
            .cmp(&b_unknown)
            .then_with(|| a.device.cmp(&b.device))
    });
    out
}

/// 决定一个挂载点归属哪块物理盘，返回物理盘的 key。
///
/// 优先级：partition_map 命中 → 精确命中 smart → parent_device → 前缀匹配 → "unknown"
fn match_physical_disk(
    partition_name: &str,
    smart: &std::collections::HashMap<String, SmartInfo>,
    partition_map: &std::collections::HashMap<String, String>,
    parent_device: fn(&str) -> String,
) -> String {
    // Windows：盘符经映射直达物理盘标识。
    if let Some(disk) = partition_map.get(partition_name) {
        return disk.clone();
    }
    if smart.contains_key(partition_name) {
        return partition_name.to_string();
    }
    let parent = parent_device(partition_name);
    if smart.contains_key(&parent) {
        return parent;
    }
    // 前缀匹配：分区名以某个物理盘 key 开头（如 "/dev/nvme0n1p1" 命中 "/dev/nvme0n1"）。
    if let Some(k) = smart
        .keys()
        .find(|k| partition_name.starts_with(k.as_str()))
    {
        return k.clone();
    }
    "unknown".to_string()
}

/// /sys/block/<dev>/size 的扇区数换算为字节（512 字节/扇区）。
fn sectors_to_bytes(sectors: u64) -> u64 {
    sectors.saturating_mul(512)
}

/// 去掉设备路径的 `/dev/` 前缀，得到 sysfs/diskstats 用的基础名。
/// `/dev/nvme0n1` → `nvme0n1`；无前缀时原样返回。
fn device_basename(device: &str) -> &str {
    device.rsplit('/').next().unwrap_or(device)
}

/// Linux 下用 sysfs 补整盘容量、用 diskstats 补按盘 I/O。
///
/// `diskstats` 的父设备行本身就是整盘计数，按设备基础名查即可。
#[cfg(target_os = "linux")]
fn enrich_linux_disks(disks: &mut [PhysicalDisk], diskstats: &crate::diskstats::DiskStats) {
    for disk in disks.iter_mut() {
        if disk.device == "unknown" {
            continue;
        }
        let base = device_basename(&disk.device);

        // 整盘容量：/sys/block/<base>/size
        let size_path = format!("/sys/block/{}/size", base);
        if let Ok(s) = std::fs::read_to_string(&size_path) {
            if let Ok(sectors) = s.trim().parse::<u64>() {
                disk.total_bytes = sectors_to_bytes(sectors);
            }
        }

        // 整盘 I/O：diskstats 的父设备行。
        if let Some(delta) = diskstats.lookup(base) {
            disk.read_bytes_per_sec = delta.read_bytes_per_sec;
            disk.write_bytes_per_sec = delta.write_bytes_per_sec;
            disk.per_device_io = true;
        }
    }
}

/// 非 Linux：无 sysfs/diskstats，容量与 I/O 维持 assemble 阶段的值。
#[cfg(not(target_os = "linux"))]
fn enrich_linux_disks(_disks: &mut [PhysicalDisk], _diskstats: &crate::diskstats::DiskStats) {}

impl MetricsCollector {
    pub fn new() -> Self {
        let mut sys = System::new_all();
        sys.refresh_all();
        let disks = Disks::new_with_refreshed_list();
        let networks = Networks::new_with_refreshed_list();
        let smart = SmartCollector::new(SMART_REFRESH_SECS);
        let diskstats = DiskStats::new();
        Self { sys, disks, networks, smart, diskstats, last_sample: None }
    }

    pub fn collect(&mut self, top_n_processes: usize) -> SystemMetrics {
        self.sys.refresh_memory();
        self.sys.refresh_cpu_usage();

        // Refresh processes with `remove_dead_processes = true`. This matters:
        // `refresh_all()` passes `false`, which leaves exited processes in
        // sysinfo's map along with their last sampled CPU and memory. A process
        // that was busy right before exiting (a finished compiler run, say)
        // would then stay pinned to the top of the list forever.
        self.sys
            .refresh_processes(ProcessesToUpdate::All, true);

        self.disks.refresh();
        self.networks.refresh();
        self.diskstats.refresh();
        // Cheap no-op unless the SMART cache is due for a refresh.
        self.smart.refresh_if_due();

        let timestamp = Utc::now();

        // sysinfo reports byte counters as deltas since the previous refresh,
        // so dividing by the real elapsed time is what makes them rates. The
        // collection interval is configurable and refreshes take a variable
        // amount of time, so measure rather than assume.
        let now = Instant::now();
        let elapsed_secs = self
            .last_sample
            .map(|prev| now.duration_since(prev).as_secs_f64())
            .filter(|s| *s > 0.0);
        self.last_sample = Some(now);

        // On the very first sample there is no interval to divide by, so rates
        // stay at zero rather than reporting a misleading spike.
        let per_sec = |delta: u64| -> f64 {
            match elapsed_secs {
                Some(secs) => delta as f64 / secs,
                None => 0.0,
            }
        };

        // --- CPU ---
        let cpu_usage = self.sys.global_cpu_usage();
        let core_usages: Vec<f32> = self.sys.cpus().iter().map(|c| c.cpu_usage()).collect();
        let core_count = self.sys.cpus().len();

        let cpu = CpuMetrics {
            usage_percent: cpu_usage,
            core_usages,
            core_count,
        };

        // --- Memory ---
        let total_mem = self.sys.total_memory();
        let used_mem = self.sys.used_memory();
        let available_mem = self.sys.available_memory();
        let mem_usage_pct = if total_mem > 0 {
            (used_mem as f32 / total_mem as f32) * 100.0
        } else {
            0.0
        };

        let memory = MemoryMetrics {
            total_bytes: total_mem,
            used_bytes: used_mem,
            available_bytes: available_mem,
            usage_percent: mem_usage_pct,
            swap_total_bytes: self.sys.total_swap(),
            swap_used_bytes: self.sys.used_swap(),
        };

        // Whole-machine disk activity summed from per-process I/O. Used only as
        // a fallback where the kernel gives us no per-device counters, since it
        // cannot say which disk the traffic actually hit.
        let (host_read_bytes, host_written_bytes) = self.sys.processes().values().fold(
            (0u64, 0u64),
            |(r, w), p| {
                let io = p.disk_usage();
                (r.saturating_add(io.read_bytes), w.saturating_add(io.written_bytes))
            },
        );
        let host_read_per_sec = per_sec(host_read_bytes);
        let host_write_per_sec = per_sec(host_written_bytes);

        // --- Disks ---
        // Bind these separately so the closure below borrows only the fields it
        // needs rather than all of `self`.
        let smart = &self.smart;
        let diskstats = &self.diskstats;
        let disks: Vec<DiskInfo> = self.disks.iter().map(|d| {
            let total = d.total_space();
            let available = d.available_space();
            let used = total.saturating_sub(available);
            let usage_pct = if total > 0 {
                (used as f32 / total as f32) * 100.0
            } else {
                0.0
            };

            let name = d.name().to_string_lossy().to_string();
            let fs_type = d.file_system().to_string_lossy().to_string();

            // Real per-device I/O from the kernel, when this platform has it.
            let io = diskstats.lookup(&name);

            DiskInfo {
                // Enriched from the SMART cache, which refreshes on its own
                // slow interval. None when smartctl is unavailable, or when
                // this is a virtual/network volume with no physical device.
                smart: smart.lookup(&name),
                name,
                mount_point: d.mount_point().to_string_lossy().to_string(),
                total_bytes: total,
                used_bytes: used,
                usage_percent: usage_pct,
                fs_type,
                // Real per-device counters where the kernel exposes them
                // (Linux). Elsewhere these fall back to the whole-machine
                // total, which is the same on every entry — see field docs.
                read_bytes: io.map_or(host_read_bytes, |d| d.read_bytes),
                written_bytes: io.map_or(host_written_bytes, |d| d.written_bytes),
                read_bytes_per_sec: io.map_or(host_read_per_sec, |d| d.read_bytes_per_sec),
                write_bytes_per_sec: io.map_or(host_write_per_sec, |d| d.write_bytes_per_sec),
                per_device_io: io.is_some(),
            }
        }).collect();

        // 物理盘视图的原始输入：与上面的 DiskInfo 用同一批挂载点，但保留设备名
        // 以便按父设备归并。
        let disk_inputs: Vec<DiskInput> = self.disks.iter().map(|d| {
            let name = d.name().to_string_lossy().to_string();
            let total = d.total_space();
            let available = d.available_space();
            let used = total.saturating_sub(available);
            let io = diskstats.lookup(&name);
            DiskInput {
                read_bytes_per_sec: io.map_or(host_read_per_sec, |x| x.read_bytes_per_sec),
                write_bytes_per_sec: io.map_or(host_write_per_sec, |x| x.write_bytes_per_sec),
                per_device_io: io.is_some(),
                name,
                mount_point: d.mount_point().to_string_lossy().to_string(),
                fs_type: d.file_system().to_string_lossy().to_string(),
                total_bytes: total,
                used_bytes: used,
            }
        }).collect();

        let smart_snapshot = smart.snapshot();
        let partition_map = smart.partition_map();
        let mut physical_disks =
            assemble_physical_disks_with_map(&disk_inputs, &smart_snapshot, &partition_map);
        enrich_linux_disks(&mut physical_disks, diskstats);

        // --- Networks ---
        use crate::disk_health::filter::is_physical_interface;
        let networks: Vec<NetworkInterface> = self.networks.iter()
            .filter(|(name, _)| is_physical_interface(name))
            .map(|(name, data)| {
            let received = data.received();
            let transmitted = data.transmitted();
            NetworkInterface {
                name: name.clone(),
                received_bytes: received,
                transmitted_bytes: transmitted,
                // Downlink/uplink throughput, which is what actually reads as
                // "network usage" — the raw deltas above depend on how long
                // the interval happened to be.
                rx_bytes_per_sec: per_sec(received),
                tx_bytes_per_sec: per_sec(transmitted),
                total_received_bytes: data.total_received(),
                total_transmitted_bytes: data.total_transmitted(),
            }
        }).collect();

        // --- Load Average ---
        let load_avg = System::load_average();
        let load_average = Some(LoadAverage {
            one: load_avg.one,
            five: load_avg.five,
            fifteen: load_avg.fifteen,
        });

        // --- Top Processes ---
        let mut processes: Vec<ProcessInfo> = self.sys.processes().values().map(|p| {
            let status = match p.status() {
                ProcessStatus::Run => "running",
                ProcessStatus::Sleep => "sleeping",
                ProcessStatus::Stop => "stopped",
                ProcessStatus::Zombie => "zombie",
                _ => "unknown",
            };
            let io = p.disk_usage();
            ProcessInfo {
                pid: p.pid().as_u32(),
                name: p.name().to_string_lossy().to_string(),
                cpu_usage: p.cpu_usage(),
                memory_bytes: p.memory(),
                status: status.to_string(),
                // Per-process I/O rates, so the dashboard can rank processes by
                // disk activity rather than only by CPU.
                disk_read_bps: per_sec(io.read_bytes),
                disk_write_bps: per_sec(io.written_bytes),
            }
        }).collect();

        // Rank by CPU but keep enough headroom that the frontend can re-sort by
        // disk I/O without the heaviest I/O processes having been truncated away.
        processes.sort_by(|a, b| {
            let a_key = (a.cpu_usage as f64)
                .max(0.0)
                .max((a.disk_read_bps + a.disk_write_bps) / 1_048_576.0);
            let b_key = (b.cpu_usage as f64)
                .max(0.0)
                .max((b.disk_read_bps + b.disk_write_bps) / 1_048_576.0);
            b_key.partial_cmp(&a_key).unwrap_or(std::cmp::Ordering::Equal)
        });
        processes.truncate(top_n_processes);

        // --- System Info ---
        let uptime_seconds = System::uptime();
        let os_name = format!(
            "{} {}",
            System::name().unwrap_or_else(|| "Unknown".to_string()),
            System::os_version().unwrap_or_else(|| "".to_string())
        );
        let hostname = System::host_name().unwrap_or_else(|| "unknown".to_string());

        debug!("Collected metrics: cpu={:.1}%, mem={:.1}%, disks={}, nets={}",
            cpu_usage, mem_usage_pct, disks.len(), networks.len());

        SystemMetrics {
            timestamp,
            cpu,
            memory,
            disks,
            physical_disks,
            networks,
            load_average,
            top_processes: processes,
            uptime_seconds,
            os_name,
            hostname,
        }
    }
}

impl Default for MetricsCollector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use crate::disk_health::filter::is_physical_interface;

    #[test]
    fn loopback_is_excluded() {
        assert!(!is_physical_interface("lo"));
        assert!(!is_physical_interface("Loopback Pseudo-Interface 1"));
    }

    #[test]
    fn docker_bridge_is_excluded() {
        assert!(!is_physical_interface("docker0"));
        assert!(!is_physical_interface("br-abc123"));
    }

    #[test]
    fn vpn_tunnels_are_excluded() {
        assert!(!is_physical_interface("tun0"));
        assert!(!is_physical_interface("wg0"));
        assert!(!is_physical_interface("utun2"));
    }

    #[test]
    fn physical_nics_are_kept() {
        assert!(is_physical_interface("eth0"));
        assert!(is_physical_interface("enp3s0"));
        assert!(is_physical_interface("wlan0"));
        assert!(is_physical_interface("Wi-Fi"));
        assert!(is_physical_interface("以太网"));
    }

    use crate::collector::{assemble_physical_disks, assemble_physical_disks_with_map, DiskInput};
    use crate::types::{DiskType, SmartHealth, SmartInfo};
    use std::collections::HashMap;

    fn smart_stub(device: &str, rotation: Option<u32>) -> SmartInfo {
        SmartInfo {
            device: device.to_string(),
            model: Some("Test Model".to_string()),
            serial: None,
            firmware: None,
            rotation_rate: rotation,
            health: SmartHealth::Passed,
            temperature_celsius: Some(40),
            power_on_hours: None,
            power_cycle_count: None,
            reallocated_sectors: None,
            percentage_used: None,
            data_units_written_bytes: None,
            attributes: vec![],
            collected_at: chrono::Utc::now(),
        }
    }

    fn disk_input(name: &str, mount: &str, total: u64, used: u64) -> DiskInput {
        DiskInput {
            name: name.to_string(),
            mount_point: mount.to_string(),
            fs_type: "ext4".to_string(),
            total_bytes: total,
            used_bytes: used,
            read_bytes_per_sec: 0.0,
            write_bytes_per_sec: 0.0,
            per_device_io: false,
        }
    }

    #[test]
    fn partitions_group_under_parent_device() {
        let mut smart = HashMap::new();
        smart.insert("/dev/nvme0n1".to_string(), smart_stub("/dev/nvme0n1", Some(0)));

        let inputs = vec![
            disk_input("/dev/nvme0n1p1", "/boot", 500_000, 100_000),
            disk_input("/dev/nvme0n1p2", "/", 1_000_000, 400_000),
        ];

        let disks = assemble_physical_disks(&inputs, &smart);

        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].device, "/dev/nvme0n1");
        assert_eq!(disks[0].partitions.len(), 2);
        assert_eq!(disks[0].disk_type, DiskType::Nvme);
    }

    #[test]
    fn unmatched_partition_falls_into_unknown_disk() {
        // SMART 缓存为空（如 smartctl 未安装）——所有分区归入合成的"未知盘"。
        let smart: HashMap<String, SmartInfo> = HashMap::new();
        let inputs = vec![disk_input("//server/share", "/mnt/net", 0, 0)];

        let disks = assemble_physical_disks(&inputs, &smart);

        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].device, "unknown");
        assert!(disks[0].model.is_none());
        assert_eq!(disks[0].partitions.len(), 1);
    }

    #[test]
    fn disk_type_inferred_from_rotation_rate() {
        let mut smart = HashMap::new();
        smart.insert("/dev/sda".to_string(), smart_stub("/dev/sda", Some(7200)));
        smart.insert("/dev/sdb".to_string(), smart_stub("/dev/sdb", Some(0)));

        let inputs = vec![
            disk_input("/dev/sda1", "/data", 100, 10),
            disk_input("/dev/sdb1", "/backup", 100, 10),
        ];

        let disks = assemble_physical_disks(&inputs, &smart);

        let sda = disks.iter().find(|d| d.device == "/dev/sda").unwrap();
        let sdb = disks.iter().find(|d| d.device == "/dev/sdb").unwrap();
        assert_eq!(sda.disk_type, DiskType::Hdd);
        assert_eq!(sdb.disk_type, DiskType::Ssd);
    }

    #[test]
    fn sysfs_size_converts_sectors_to_bytes() {
        // /sys/block/<dev>/size 以 512 字节扇区计数。
        assert_eq!(super::sectors_to_bytes(2_048), 2_048 * 512);
        assert_eq!(super::sectors_to_bytes(0), 0);
    }

    #[test]
    fn device_basename_strips_dev_prefix() {
        assert_eq!(super::device_basename("/dev/nvme0n1"), "nvme0n1");
        assert_eq!(super::device_basename("/dev/sda"), "sda");
        assert_eq!(super::device_basename("disk0"), "disk0");
    }

    #[test]
    fn windows_drive_letters_map_to_physical_disk() {
        let mut smart = HashMap::new();
        smart.insert("disk0".to_string(), smart_stub("disk0", Some(0)));

        let mut pmap = HashMap::new();
        pmap.insert("C:\\".to_string(), "disk0".to_string());
        pmap.insert("D:\\".to_string(), "disk0".to_string());

        let inputs = vec![
            disk_input("C:\\", "C:\\", 500, 200),
            disk_input("D:\\", "D:\\", 500, 100),
        ];

        let disks = assemble_physical_disks_with_map(&inputs, &smart, &pmap);

        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].device, "disk0");
        assert_eq!(disks[0].partitions.len(), 2);
    }
}
