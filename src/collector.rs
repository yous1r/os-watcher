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
}
