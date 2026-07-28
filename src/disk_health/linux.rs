// src/disk_health/linux.rs
#![cfg(target_os = "linux")]

use std::path::Path;
use chrono::Utc;
use crate::types::{SmartHealth, SmartInfo};

/// Read a single-line text file and trim whitespace. Returns None on any error.
fn read_file(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// Probe hwmon entries and build a map of block-device-name → temperature °C.
///
/// Reads `/sys/class/hwmon/hwmonN/name` to filter disk-temperature drivers,
/// then `/sys/class/hwmon/hwmonN/temp1_input` for the value, and resolves the
/// backing block device from `/sys/class/hwmon/hwmonN/device/block/`.
pub fn hwmon_temperatures() -> std::collections::HashMap<String, i32> {
    hwmon_temperatures_from("/sys/class/hwmon")
}

/// Testable variant that accepts an arbitrary root path.
pub(crate) fn hwmon_temperatures_from(root: &str) -> std::collections::HashMap<String, i32> {
    const DISK_DRIVERS: &[&str] = &["nvme", "drivetemp", "megaraid"];
    let mut map = std::collections::HashMap::new();

    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return map,
    };

    for entry in entries.flatten() {
        let hwmon_path = entry.path();

        // Only process disk-temperature drivers.
        let driver_name = read_file(&hwmon_path.join("name")).unwrap_or_default();
        if !DISK_DRIVERS.iter().any(|&d| driver_name == d) {
            continue;
        }

        // Temperature in millidegrees Celsius → divide by 1000.
        let temp_millic = match read_file(&hwmon_path.join("temp1_input"))
            .and_then(|s| s.parse::<i64>().ok())
        {
            Some(v) => v,
            None => continue,
        };
        let temp_c = (temp_millic / 1000) as i32;

        // Resolve the block device name via device/block/ subdirectory.
        let block_dir = hwmon_path.join("device").join("block");
        if let Ok(block_entries) = std::fs::read_dir(&block_dir) {
            for bentry in block_entries.flatten() {
                let dev_name = bentry.file_name().to_string_lossy().to_string();
                map.insert(dev_name, temp_c);
            }
        } else {
            // NVMe exposes block dir directly under the hwmon dir itself.
            let direct = hwmon_path.join("device");
            if let Ok(sub) = std::fs::read_dir(&direct) {
                for s in sub.flatten() {
                    let n = s.file_name().to_string_lossy().to_string();
                    // nvme block devices start with "nvme"
                    if n.starts_with("nvme") {
                        map.insert(n, temp_c);
                    }
                }
            }
        }
    }

    map
}

/// Read model string from sysfs for a block device name (e.g. "sda").
fn sysfs_model(dev: &str) -> Option<String> {
    let path = format!("/sys/block/{}/device/model", dev);
    read_file(Path::new(&path))
}

/// Whether the block device is rotational (HDD) vs SSD/NVMe.
fn is_rotational(dev: &str) -> Option<bool> {
    let path = format!("/sys/block/{}/queue/rotational", dev);
    read_file(Path::new(&path)).map(|s| s == "1")
}

/// Build a `SmartInfo` from hwmon temperature + sysfs metadata,
/// without requiring smartctl.
pub fn build_smart_from_sysfs(dev_name: &str, temp_c: Option<i32>) -> SmartInfo {
    let model = sysfs_model(dev_name);
    let rotational = is_rotational(dev_name);
    let rotation_rate = match rotational {
        Some(true) => Some(7200),   // placeholder — no RPM from sysfs
        _ => Some(0),               // SSD/NVMe convention: 0
    };
    SmartInfo {
        device: format!("/dev/{}", dev_name),
        model,
        serial: None,
        firmware: None,
        rotation_rate,
        health: SmartHealth::Unknown,
        temperature_celsius: temp_c,
        power_on_hours: None,
        power_cycle_count: None,
        reallocated_sectors: None,
        percentage_used: None,
        data_units_written_bytes: None,
        attributes: vec![],
        collected_at: Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_hwmon(root: &TempDir, idx: u32, driver: &str, temp_millic: i64, block_dev: &str) {
        let hwmon = root.path().join(format!("hwmon{}", idx));
        fs::create_dir_all(&hwmon).unwrap();
        fs::write(hwmon.join("name"), driver).unwrap();
        fs::write(hwmon.join("temp1_input"), temp_millic.to_string()).unwrap();
        let block_dir = hwmon.join("device").join("block").join(block_dev);
        fs::create_dir_all(&block_dir).unwrap();
    }

    #[test]
    fn reads_drivetemp_entry() {
        let root = TempDir::new().unwrap();
        make_hwmon(&root, 0, "drivetemp", 42_000, "sda");
        let map = hwmon_temperatures_from(root.path().to_str().unwrap());
        assert_eq!(map.get("sda"), Some(&42));
    }

    #[test]
    fn reads_nvme_entry() {
        let root = TempDir::new().unwrap();
        make_hwmon(&root, 1, "nvme", 38_500, "nvme0n1");
        let map = hwmon_temperatures_from(root.path().to_str().unwrap());
        assert_eq!(map.get("nvme0n1"), Some(&38));
    }

    #[test]
    fn skips_non_disk_driver() {
        let root = TempDir::new().unwrap();
        make_hwmon(&root, 2, "coretemp", 60_000, "sdb");
        let map = hwmon_temperatures_from(root.path().to_str().unwrap());
        assert!(map.is_empty(), "coretemp should not appear in disk map");
    }

    #[test]
    fn multiple_devices_in_same_scan() {
        let root = TempDir::new().unwrap();
        make_hwmon(&root, 0, "drivetemp", 35_000, "sda");
        make_hwmon(&root, 1, "nvme", 41_000, "nvme0n1");
        let map = hwmon_temperatures_from(root.path().to_str().unwrap());
        assert_eq!(map.get("sda"), Some(&35));
        assert_eq!(map.get("nvme0n1"), Some(&41));
    }

    #[test]
    fn build_smart_from_sysfs_fills_temperature() {
        let info = build_smart_from_sysfs("sda", Some(38));
        assert_eq!(info.temperature_celsius, Some(38));
        assert_eq!(info.device, "/dev/sda");
        // health is Unknown when only sysfs data is available
        assert_eq!(info.health, crate::types::SmartHealth::Unknown);
    }
}
