// src/disk_health/mod.rs
pub mod filter;

pub(crate) mod autoinstall;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "windows")]
mod windows;

use std::collections::HashMap;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::types::SmartInfo;

/// How we detect whether smartctl is available.
fn smartctl_available() -> bool {
    Command::new("smartctl")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Reduce a partition path to the parent physical device.
/// `/dev/sda1` → `/dev/sda`, `/dev/nvme0n1p2` → `/dev/nvme0n1`
pub(crate) fn parent_device(name: &str) -> String {
    let trimmed = name.trim_end_matches(|c: char| c.is_ascii_digit());
    if trimmed.ends_with('p') && trimmed.contains("nvme") {
        return trimmed[..trimmed.len() - 1].to_string();
    }
    if trimmed.is_empty() { name.to_string() } else { trimmed.to_string() }
}

/// Unified disk health collector. Refreshes on a slow interval and caches
/// results so the hot metrics path just does a lookup.
pub struct DiskHealthCollector {
    /// device path / DeviceId → SmartInfo
    cache: HashMap<String, SmartInfo>,
    last_refresh: Option<Instant>,
    interval: Duration,
    /// True once we've confirmed smartctl is not available AND install failed.
    smartctl_unavailable: bool,
}

impl DiskHealthCollector {
    pub fn new(interval_secs: u64) -> Self {
        Self {
            cache: HashMap::new(),
            last_refresh: None,
            interval: Duration::from_secs(interval_secs.max(60)),
            smartctl_unavailable: false,
        }
    }

    fn is_stale(&self) -> bool {
        self.last_refresh.map(|t| t.elapsed() >= self.interval).unwrap_or(true)
    }

    /// Refresh if stale; cheap no-op otherwise.
    pub fn refresh_if_due(&mut self) {
        if !self.is_stale() {
            return;
        }
        self.last_refresh = Some(Instant::now());
        self.refresh_inner();
    }

    fn refresh_inner(&mut self) {
        #[cfg(target_os = "linux")]
        self.refresh_linux();

        #[cfg(target_os = "windows")]
        self.refresh_windows();

        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        self.refresh_smartctl_only();
    }

    /// Look up SMART data for the sysinfo-reported disk name.
    pub fn lookup(&self, disk_name: &str) -> Option<SmartInfo> {
        if self.cache.is_empty() {
            return None;
        }
        if let Some(i) = self.cache.get(disk_name) {
            return Some(i.clone());
        }
        let parent = parent_device(disk_name);
        if let Some(i) = self.cache.get(&parent) {
            return Some(i.clone());
        }
        self.cache.iter()
            .find(|(k, _)| disk_name.starts_with(k.as_str()))
            .map(|(_, i)| i.clone())
    }

    /// Devices currently held in the cache.
    pub fn devices(&self) -> Vec<&SmartInfo> {
        self.cache.values().collect()
    }

    /// 缓存的完整快照：设备标识 → SmartInfo 克隆。
    /// collector 用它作为物理盘列表的来源。
    pub fn snapshot(&self) -> std::collections::HashMap<String, SmartInfo> {
        self.cache.clone()
    }

    /// 盘符→物理盘标识映射。仅 Windows 有意义，其它平台返回空。
    pub fn partition_map(&self) -> std::collections::HashMap<String, String> {
        #[cfg(target_os = "windows")]
        {
            windows::query_partition_map()
        }
        #[cfg(not(target_os = "windows"))]
        {
            std::collections::HashMap::new()
        }
    }

    #[cfg(target_os = "linux")]
    fn refresh_linux(&mut self) {
        use linux::hwmon_temperatures;
        use linux::build_smart_from_sysfs;

        // 1. Collect hwmon temperatures.
        let temps = hwmon_temperatures();

        // 2. Try smartctl for full SMART data; install if missing.
        let use_smartctl = if self.smartctl_unavailable {
            false
        } else if smartctl_available() {
            true
        } else {
            let installed = autoinstall::try_install_smartctl();
            if !installed {
                self.smartctl_unavailable = true;
            }
            installed
        };

        if use_smartctl {
            self.refresh_smartctl_only();
            // Patch in hwmon temperatures when smartctl didn't report one.
            for (dev, info) in self.cache.iter_mut() {
                if info.temperature_celsius.is_none() {
                    let base = dev.rsplit('/').next().unwrap_or(dev.as_str());
                    if let Some(&t) = temps.get(base) {
                        info.temperature_celsius = Some(t);
                    }
                }
            }
        } else {
            // smartctl unavailable: build from hwmon + sysfs only.
            for (dev_name, temp) in &temps {
                let info = build_smart_from_sysfs(dev_name, Some(*temp));
                self.cache.insert(format!("/dev/{}", dev_name), info);
            }
        }
    }

    #[cfg(target_os = "windows")]
    fn refresh_windows(&mut self) {
        use windows::query_physical_disks;
        let new_data = query_physical_disks();
        // Try smartctl as an optional enhancement.
        let use_smartctl = if self.smartctl_unavailable {
            false
        } else if smartctl_available() {
            true
        } else {
            let ok = autoinstall::try_install_smartctl();
            if !ok { self.smartctl_unavailable = true; }
            ok
        };

        if use_smartctl {
            // Merge: WMI provides baseline, smartctl can enrich.
            self.refresh_smartctl_only();
        }

        // Always insert/overwrite with WMI data where it has richer info
        // (temperature, wear) that smartctl's Windows driver may not expose.
        for (id, info) in new_data {
            self.cache.entry(id).or_insert(info);
        }
    }

    fn refresh_smartctl_only(&mut self) {
        let output = Command::new("smartctl")
            .args(["--scan", "--json"])
            .output();
        let json = match output {
            Ok(o) if o.status.success() => {
                match serde_json::from_slice::<serde_json::Value>(&o.stdout) {
                    Ok(v) => v,
                    Err(_) => return,
                }
            }
            _ => {
                self.smartctl_unavailable = true;
                return;
            }
        };

        let devices: Vec<String> = json
            .get("devices")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|d| d.get("name").and_then(|n| n.as_str()))
                    .filter(|n| filter::is_physical_device(n))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        for device in devices {
            let result = Command::new("smartctl")
                .args(["--all", "--json", &device])
                .output();
            if let Ok(o) = result {
                if let Ok(j) = serde_json::from_slice::<serde_json::Value>(&o.stdout) {
                    if let Some(info) = crate::smart::parse_smart_json_pub(&device, &j) {
                        self.cache.insert(device, info);
                    }
                }
            }
        }
    }
}
