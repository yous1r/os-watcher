// src/disk_health/windows.rs
#![cfg(target_os = "windows")]

use chrono::Utc;
use serde::Deserialize;
use crate::types::{SmartHealth, SmartInfo};

/// Shape of one element returned by the PowerShell snippet.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WmiDisk {
    friendly_name: Option<String>,
    serial_number: Option<String>,
    media_type: Option<String>,
    health_status: Option<String>,
    temperature: Option<i32>,
    wear: Option<u8>,
    device_id: Option<String>,
}

/// Query all physical disks via PowerShell and return a map of
/// `device_id` (e.g. `"disk0"`) → `SmartInfo`.
pub fn query_physical_disks() -> std::collections::HashMap<String, SmartInfo> {
    let script = r#"
Get-PhysicalDisk | ForEach-Object {
  $rel = $_ | Get-StorageReliabilityCounter
  [PSCustomObject]@{
    FriendlyName  = $_.FriendlyName
    SerialNumber  = $_.SerialNumber
    MediaType     = $_.MediaType
    HealthStatus  = $_.HealthStatus
    Temperature   = $rel.Temperature
    Wear          = $rel.Wear
    DeviceId      = $_.DeviceId
  }
} | ConvertTo-Json -Compress
"#;

    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output();

    match output {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            parse_wmi_json(&text)
        }
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            tracing::warn!("PowerShell disk query failed: {}", err.trim());
            std::collections::HashMap::new()
        }
        Err(e) => {
            tracing::warn!("Could not run PowerShell: {}", e);
            std::collections::HashMap::new()
        }
    }
}

/// Parse PowerShell JSON — handles both a single object and an array.
pub(crate) fn parse_wmi_json(text: &str) -> std::collections::HashMap<String, SmartInfo> {
    let mut map = std::collections::HashMap::new();

    // PowerShell ConvertTo-Json returns a bare object when there is only one
    // disk; wrap it in an array so we can handle both cases uniformly.
    let json_text = text.trim();
    let normalised = if json_text.starts_with('{') {
        format!("[{}]", json_text)
    } else {
        json_text.to_string()
    };

    let disks: Vec<WmiDisk> = match serde_json::from_str(&normalised) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("Failed to parse WMI JSON: {}", e);
            return map;
        }
    };

    for disk in disks {
        let device_id = match &disk.device_id {
            Some(id) => id.clone(),
            None => continue,
        };

        let health = match disk.health_status.as_deref() {
            Some("Healthy") => SmartHealth::Passed,
            Some("Unhealthy") | Some("Warning") => SmartHealth::Failed,
            _ => SmartHealth::Unknown,
        };

        let rotation_rate = match disk.media_type.as_deref() {
            Some("HDD") => Some(7200_u32),
            Some("SSD") | Some("NVMe SSD") => Some(0),
            _ => None,
        };

        let info = SmartInfo {
            device: device_id.clone(),
            model: disk.friendly_name,
            serial: disk.serial_number,
            firmware: None,
            rotation_rate,
            health,
            temperature_celsius: disk.temperature,
            power_on_hours: None,
            power_cycle_count: None,
            reallocated_sectors: None,
            percentage_used: disk.wear,
            data_units_written_bytes: None,
            attributes: vec![],
            collected_at: Utc::now(),
        };

        map.insert(device_id, info);
    }

    map
}

#[cfg(test)]
mod tests {
    use super::*;

    const SINGLE_DISK_JSON: &str = r#"{
        "FriendlyName": "Samsung SSD 870 EVO 1TB",
        "SerialNumber": "S59CNX0R123456",
        "MediaType": "SSD",
        "HealthStatus": "Healthy",
        "Temperature": 32,
        "Wear": 5,
        "DeviceId": "disk0"
    }"#;

    const MULTI_DISK_JSON: &str = r#"[
        {
            "FriendlyName": "WD Blue 2TB",
            "SerialNumber": "WD-ABC123",
            "MediaType": "HDD",
            "HealthStatus": "Healthy",
            "Temperature": 38,
            "Wear": null,
            "DeviceId": "disk0"
        },
        {
            "FriendlyName": "Samsung NVMe",
            "SerialNumber": "S1234",
            "MediaType": "NVMe SSD",
            "HealthStatus": "Unhealthy",
            "Temperature": null,
            "Wear": 95,
            "DeviceId": "disk1"
        }
    ]"#;

    #[test]
    fn parses_single_disk_object() {
        let map = parse_wmi_json(SINGLE_DISK_JSON);
        let disk = map.get("disk0").expect("disk0 present");
        assert_eq!(disk.health, SmartHealth::Passed);
        assert_eq!(disk.temperature_celsius, Some(32));
        assert_eq!(disk.percentage_used, Some(5));
        assert_eq!(disk.model.as_deref(), Some("Samsung SSD 870 EVO 1TB"));
        assert_eq!(disk.rotation_rate, Some(0)); // SSD
    }

    #[test]
    fn parses_multi_disk_array() {
        let map = parse_wmi_json(MULTI_DISK_JSON);
        assert_eq!(map.len(), 2);

        let hdd = map.get("disk0").expect("disk0 present");
        assert_eq!(hdd.health, SmartHealth::Passed);
        assert_eq!(hdd.rotation_rate, Some(7200)); // HDD

        let nvme = map.get("disk1").expect("disk1 present");
        assert_eq!(nvme.health, SmartHealth::Failed);
        assert_eq!(nvme.percentage_used, Some(95));
        assert!(nvme.temperature_celsius.is_none());
    }

    #[test]
    fn health_status_unhealthy_maps_to_failed() {
        let json = r#"{"HealthStatus":"Unhealthy","DeviceId":"disk0"}"#;
        let map = parse_wmi_json(json);
        assert_eq!(map.get("disk0").unwrap().health, SmartHealth::Failed);
    }

    #[test]
    fn missing_device_id_is_skipped() {
        let json = r#"{"FriendlyName":"Test","HealthStatus":"Healthy"}"#;
        let map = parse_wmi_json(json);
        assert!(map.is_empty());
    }

    #[test]
    fn invalid_json_returns_empty_map() {
        let map = parse_wmi_json("not json at all");
        assert!(map.is_empty());
    }
}
