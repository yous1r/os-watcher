//! SMART disk health collection via `smartctl` (smartmontools).
//!
//! SMART data cannot be read through `sysinfo`; it requires querying the
//! device directly, which needs elevated privileges and costs tens to
//! hundreds of milliseconds per device. So this module is deliberately kept
//! off the hot metrics path: `SmartCollector` refreshes on its own slow
//! interval and the main collector only reads the cache.
//!
//! If `smartctl` is missing or lacks permission, everything degrades to
//! `None` — SMART is treated as optional enrichment, never a hard dependency.

use std::collections::HashMap;
use std::process::Command;
use std::time::{Duration, Instant};

use chrono::Utc;
use serde_json::Value;
use tracing::{debug, warn};

use crate::types::{SmartAttribute, SmartHealth, SmartInfo};

/// Attribute IDs worth surfacing rather than dumping all ~30 attributes.
const NOTABLE_ATTRIBUTE_IDS: &[u8] = &[
    5,   // Reallocated_Sector_Ct
    9,   // Power_On_Hours
    10,  // Spin_Retry_Count
    12,  // Power_Cycle_Count
    177, // Wear_Leveling_Count
    187, // Reported_Uncorrect
    188, // Command_Timeout
    190, // Airflow_Temperature
    194, // Temperature_Celsius
    196, // Reallocated_Event_Count
    197, // Current_Pending_Sector
    198, // Offline_Uncorrectable
    199, // UDMA_CRC_Error_Count
    231, // SSD_Life_Left
    233, // Media_Wearout_Indicator
];

/// Caches SMART data and refreshes it on a slow interval.
pub struct SmartCollector {
    /// Physical device path -> last successful reading.
    cache: HashMap<String, SmartInfo>,
    /// When the cache was last refreshed.
    last_refresh: Option<Instant>,
    /// How long a cached reading stays valid.
    interval: Duration,
    /// Set once we know `smartctl` is unusable, to stop retrying every cycle.
    unavailable: bool,
}

impl SmartCollector {
    /// Create a collector that refreshes at most every `interval_secs`.
    pub fn new(interval_secs: u64) -> Self {
        Self {
            cache: HashMap::new(),
            last_refresh: None,
            interval: Duration::from_secs(interval_secs.max(60)),
            unavailable: false,
        }
    }

    /// Whether the cache is due for a refresh.
    fn is_stale(&self) -> bool {
        match self.last_refresh {
            None => true,
            Some(t) => t.elapsed() >= self.interval,
        }
    }

    /// Refresh the cache if it is stale. Cheap no-op otherwise, so this is
    /// safe to call from the regular collection loop.
    pub fn refresh_if_due(&mut self) {
        if self.unavailable || !self.is_stale() {
            return;
        }

        self.last_refresh = Some(Instant::now());

        let devices = match scan_devices() {
            Ok(d) => d,
            Err(e) => {
                // smartctl absent or unusable: log once, then stay quiet.
                warn!("SMART collection disabled: {}", e);
                self.unavailable = true;
                return;
            }
        };

        if devices.is_empty() {
            debug!("SMART scan found no devices");
            return;
        }

        for device in devices {
            match query_device(&device) {
                Ok(info) => {
                    self.cache.insert(device, info);
                }
                Err(e) => {
                    debug!("SMART query failed for {}: {}", device, e);
                }
            }
        }

        debug!("SMART cache refreshed: {} device(s)", self.cache.len());
    }

    /// Look up cached SMART data for the physical device backing a disk.
    ///
    /// `disk_name` is the value reported by `sysinfo` (e.g. `/dev/sda1`,
    /// `/dev/nvme0n1p2`), which is usually a partition — this maps it back to
    /// the parent device that SMART is reported against.
    pub fn lookup(&self, disk_name: &str) -> Option<SmartInfo> {
        if self.cache.is_empty() {
            return None;
        }

        // Exact match first.
        if let Some(info) = self.cache.get(disk_name) {
            return Some(info.clone());
        }

        // Otherwise reduce the partition to its parent device and match that.
        let parent = parent_device(disk_name);
        if let Some(info) = self.cache.get(&parent) {
            return Some(info.clone());
        }

        // Last resort: a cached device that prefixes this disk's name.
        self.cache
            .iter()
            .find(|(dev, _)| disk_name.starts_with(dev.as_str()))
            .map(|(_, info)| info.clone())
    }

    /// Devices currently held in the cache.
    pub fn devices(&self) -> Vec<&SmartInfo> {
        self.cache.values().collect()
    }
}

/// Reduce a partition path to the physical device SMART reports against.
///
/// `/dev/sda1` -> `/dev/sda`, `/dev/nvme0n1p2` -> `/dev/nvme0n1`.
fn parent_device(name: &str) -> String {
    let trimmed = name.trim_end_matches(|c: char| c.is_ascii_digit());

    // NVMe partitions are `<device>p<N>`; drop the trailing `p`.
    if trimmed.ends_with('p') && trimmed.contains("nvme") {
        return trimmed[..trimmed.len() - 1].to_string();
    }

    if trimmed.is_empty() {
        name.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Run smartctl and parse its JSON output.
fn run_smartctl(args: &[&str]) -> Result<Value, String> {
    let output = Command::new("smartctl")
        .args(args)
        .output()
        .map_err(|e| format!("could not run smartctl: {e}"))?;

    // smartctl uses a bitmask exit status; bits 0-1 mean the command itself
    // failed, higher bits are device conditions that still yield valid JSON.
    let code = output.status.code().unwrap_or(-1);
    if code & 0b11 != 0 {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "smartctl exited {code}: {}",
            stderr.trim().lines().next().unwrap_or("unknown error")
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&stdout).map_err(|e| format!("invalid smartctl JSON: {e}"))
}

/// Enumerate real physical devices via `smartctl --scan`.
///
/// The scan also turns up things that have no physical media behind them —
/// loopback mounts, device-mapper/LVM volumes, software RAID, and the
/// paravirtualised disks handed out by hypervisors. Querying those either
/// errors out or returns meaningless data, so they are filtered here.
fn scan_devices() -> Result<Vec<String>, String> {
    let json = run_smartctl(&["--scan", "--json"])?;

    let devices: Vec<String> = json
        .get("devices")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|d| d.get("name").and_then(Value::as_str))
                .filter(|name| is_physical_device(name))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    Ok(devices)
}

/// Device name prefixes that never correspond to real physical media.
///
/// `vd*`/`xvd*` are virtio and Xen paravirtual disks, `dm-*` is
/// device-mapper (LVM, LUKS), `md*` is software RAID, and the rest are
/// pseudo-devices.
const VIRTUAL_DEVICE_PREFIXES: &[&str] = &[
    "loop",  // loopback file mounts
    "ram",   // ramdisks
    "zram",  // compressed ramdisks
    "dm-",   // device-mapper: LVM, LUKS
    "md",    // software RAID
    "sr",    // optical drives
    "fd",    // floppy
    "vd",    // virtio paravirtual disk (KVM/QEMU)
    "xvd",   // Xen paravirtual disk
    "zd",    // ZFS volumes
];

/// Whether a scanned device path looks like real physical media.
///
/// NVMe controllers (`/dev/nvme0`) and SATA/SAS disks (`/dev/sda`) are kept;
/// anything matching a known virtual prefix is dropped. Windows uses opaque
/// `/dev/sdN`-style aliases via smartctl, which fall through as physical.
fn is_physical_device(path: &str) -> bool {
    // Reduce `/dev/nvme0n1` to `nvme0n1` for prefix matching.
    let base = path.rsplit('/').next().unwrap_or(path);

    if base.is_empty() {
        return false;
    }

    // NVMe is explicitly physical and must be checked before the `md`/`vd`
    // prefix rules, since names like `nvme0n1` contain no virtual marker but
    // we want the intent to be unambiguous.
    if base.starts_with("nvme") {
        return true;
    }

    !VIRTUAL_DEVICE_PREFIXES
        .iter()
        .any(|prefix| base.starts_with(prefix))
}

/// Query full SMART data for one device.
fn query_device(device: &str) -> Result<SmartInfo, String> {
    let json = run_smartctl(&["--all", "--json", device])?;
    Ok(parse_smart_json(device, &json))
}

/// Translate smartctl JSON into our `SmartInfo`.
///
/// Split out from the command execution so it can be unit tested against
/// captured fixtures without needing smartctl or a real disk.
fn parse_smart_json(device: &str, json: &Value) -> SmartInfo {
    let health = match json
        .pointer("/smart_status/passed")
        .and_then(Value::as_bool)
    {
        Some(true) => SmartHealth::Passed,
        Some(false) => SmartHealth::Failed,
        None => SmartHealth::Unknown,
    };

    let temperature_celsius = json
        .pointer("/temperature/current")
        .and_then(Value::as_i64)
        .map(|v| v as i32);

    let power_on_hours = json
        .pointer("/power_on_time/hours")
        .and_then(Value::as_u64);

    let power_cycle_count = json.get("power_cycle_count").and_then(Value::as_u64);

    let rotation_rate = json
        .get("rotation_rate")
        .and_then(Value::as_u64)
        .map(|v| v as u32);

    // NVMe exposes wear directly; SATA SSDs express it via attributes.
    let percentage_used = json
        .pointer("/nvme_smart_health_information_log/percentage_used")
        .and_then(Value::as_u64)
        .map(|v| v.min(100) as u8);

    // NVMe reports written volume in 1000 * 512-byte units.
    let data_units_written_bytes = json
        .pointer("/nvme_smart_health_information_log/data_units_written")
        .and_then(Value::as_u64)
        .map(|units| units.saturating_mul(512_000));

    let attributes = parse_attributes(json);

    // Reallocated sector count comes from attribute 5 on SATA devices.
    let reallocated_sectors = attributes
        .iter()
        .find(|a| a.id == 5)
        .and_then(|a| a.raw.parse::<u64>().ok());

    SmartInfo {
        device: device.to_string(),
        model: json
            .get("model_name")
            .and_then(Value::as_str)
            .map(str::to_string),
        serial: json
            .get("serial_number")
            .and_then(Value::as_str)
            .map(str::to_string),
        firmware: json
            .get("firmware_version")
            .and_then(Value::as_str)
            .map(str::to_string),
        rotation_rate,
        health,
        temperature_celsius,
        power_on_hours,
        power_cycle_count,
        reallocated_sectors,
        percentage_used,
        data_units_written_bytes,
        attributes,
        collected_at: Utc::now(),
    }
}

/// Extract the notable SATA SMART attributes from smartctl JSON.
fn parse_attributes(json: &Value) -> Vec<SmartAttribute> {
    let Some(table) = json
        .pointer("/ata_smart_attributes/table")
        .and_then(Value::as_array)
    else {
        return vec![];
    };

    table
        .iter()
        .filter_map(|entry| {
            let id = entry.get("id").and_then(Value::as_u64)? as u8;
            if !NOTABLE_ATTRIBUTE_IDS.contains(&id) {
                return None;
            }

            let value = entry.get("value").and_then(Value::as_i64).unwrap_or(-1);
            let threshold = entry.get("thresh").and_then(Value::as_i64).unwrap_or(-1);

            // Prefer smartctl's own verdict; fall back to comparing against
            // the threshold, which is only meaningful when both are known.
            let failing = entry
                .pointer("/flags/prefailure")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                && value >= 0
                && threshold > 0
                && value <= threshold;

            Some(SmartAttribute {
                id,
                name: entry
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                value,
                worst: entry.get("worst").and_then(Value::as_i64).unwrap_or(-1),
                threshold,
                raw: entry
                    .pointer("/raw/string")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| {
                        entry
                            .pointer("/raw/value")
                            .and_then(Value::as_u64)
                            .map(|v| v.to_string())
                    })
                    .unwrap_or_default(),
                failing,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_device_strips_sata_partition() {
        assert_eq!(parent_device("/dev/sda1"), "/dev/sda");
        assert_eq!(parent_device("/dev/sdb12"), "/dev/sdb");
    }

    #[test]
    fn parent_device_strips_nvme_partition() {
        assert_eq!(parent_device("/dev/nvme0n1p2"), "/dev/nvme0n1");
    }

    #[test]
    fn parent_device_leaves_whole_device_alone() {
        assert_eq!(parent_device("/dev/sda"), "/dev/sda");
    }

    #[test]
    fn parses_nvme_health_payload() {
        let json: Value = serde_json::from_str(
            r#"{
                "model_name": "Samsung SSD 980 PRO 1TB",
                "serial_number": "S5GXNF0R123456",
                "firmware_version": "5B2QGXA7",
                "rotation_rate": 0,
                "smart_status": { "passed": true },
                "temperature": { "current": 41 },
                "power_on_time": { "hours": 4210 },
                "power_cycle_count": 812,
                "nvme_smart_health_information_log": {
                    "percentage_used": 7,
                    "data_units_written": 20000
                }
            }"#,
        )
        .unwrap();

        let info = parse_smart_json("/dev/nvme0", &json);

        assert_eq!(info.health, SmartHealth::Passed);
        assert_eq!(info.temperature_celsius, Some(41));
        assert_eq!(info.power_on_hours, Some(4210));
        assert_eq!(info.percentage_used, Some(7));
        assert_eq!(info.data_units_written_bytes, Some(20_000 * 512_000));
        assert_eq!(info.model.as_deref(), Some("Samsung SSD 980 PRO 1TB"));
        assert!(!info.is_unhealthy());
    }

    #[test]
    fn flags_failed_health_as_unhealthy() {
        let json: Value =
            serde_json::from_str(r#"{ "smart_status": { "passed": false } }"#).unwrap();

        let info = parse_smart_json("/dev/sda", &json);

        assert_eq!(info.health, SmartHealth::Failed);
        assert!(info.is_unhealthy());
    }

    #[test]
    fn parses_sata_attributes_and_reallocated_sectors() {
        let json: Value = serde_json::from_str(
            r#"{
                "smart_status": { "passed": true },
                "ata_smart_attributes": { "table": [
                    {
                        "id": 5, "name": "Reallocated_Sector_Ct",
                        "value": 100, "worst": 100, "thresh": 10,
                        "flags": { "prefailure": true },
                        "raw": { "value": 24, "string": "24" }
                    },
                    {
                        "id": 194, "name": "Temperature_Celsius",
                        "value": 65, "worst": 50, "thresh": 0,
                        "flags": { "prefailure": false },
                        "raw": { "value": 35, "string": "35" }
                    },
                    {
                        "id": 241, "name": "Total_LBAs_Written",
                        "value": 99, "worst": 99, "thresh": 0,
                        "flags": { "prefailure": false },
                        "raw": { "value": 1, "string": "1" }
                    }
                ]}
            }"#,
        )
        .unwrap();

        let info = parse_smart_json("/dev/sda", &json);

        // Only notable attributes are kept, so 241 is filtered out.
        assert_eq!(info.attributes.len(), 2);
        // Nonzero reallocated sectors are a wear signal even when SMART passes.
        assert_eq!(info.reallocated_sectors, Some(24));
        assert!(info.is_unhealthy());
    }

    #[test]
    fn marks_attribute_failing_when_below_threshold() {
        let json: Value = serde_json::from_str(
            r#"{
                "smart_status": { "passed": true },
                "ata_smart_attributes": { "table": [
                    {
                        "id": 5, "name": "Reallocated_Sector_Ct",
                        "value": 8, "worst": 8, "thresh": 10,
                        "flags": { "prefailure": true },
                        "raw": { "value": 0, "string": "0" }
                    }
                ]}
            }"#,
        )
        .unwrap();

        let info = parse_smart_json("/dev/sda", &json);

        assert!(info.attributes[0].failing);
        assert!(info.is_unhealthy());
    }

    #[test]
    fn keeps_nvme_devices() {
        assert!(is_physical_device("/dev/nvme0"));
        assert!(is_physical_device("/dev/nvme0n1"));
        assert!(is_physical_device("/dev/nvme1n1"));
    }

    #[test]
    fn keeps_sata_and_sas_disks() {
        assert!(is_physical_device("/dev/sda"));
        assert!(is_physical_device("/dev/sdb"));
    }

    #[test]
    fn drops_paravirtual_disks() {
        // Disks handed out by KVM/QEMU and Xen have no physical media.
        assert!(!is_physical_device("/dev/vda"));
        assert!(!is_physical_device("/dev/vdb1"));
        assert!(!is_physical_device("/dev/xvda"));
    }

    #[test]
    fn drops_pseudo_and_mapped_devices() {
        assert!(!is_physical_device("/dev/loop0"));
        assert!(!is_physical_device("/dev/dm-0"));
        assert!(!is_physical_device("/dev/md0"));
        assert!(!is_physical_device("/dev/zram0"));
        assert!(!is_physical_device("/dev/sr0"));
        assert!(!is_physical_device("/dev/zd16"));
    }

    #[test]
    fn missing_fields_degrade_to_unknown() {
        let json: Value = serde_json::from_str("{}").unwrap();

        let info = parse_smart_json("/dev/sdz", &json);

        assert_eq!(info.health, SmartHealth::Unknown);
        assert_eq!(info.temperature_celsius, None);
        assert!(info.attributes.is_empty());
        assert!(!info.is_unhealthy());
    }
}
