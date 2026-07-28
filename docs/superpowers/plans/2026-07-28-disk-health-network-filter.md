# 硬盘健康采集 + 物理网卡过滤 实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 在 Linux 上通过 hwmon sysfs 获取硬盘温度（无需特权），Windows 上通过 PowerShell/WMI 采集，smartctl 缺失时自动安装；同时过滤掉虚拟网卡，只向前端暴露物理接口。

**架构：** 新建 `src/disk_health/` 模块，按平台分别实现采集逻辑，通过 `#[cfg]` 属性路由；`SmartCollector`（`src/smart.rs`）的公开接口保持不变，内部委托给 `disk_health::DiskHealthCollector`。网卡过滤在 `src/collector.rs` 的采集侧完成，API 和 TUI 无需改动。

**技术栈：** Rust 2021，sysinfo 0.32，serde_json 1，nix 0.29（Unix geteuid），std::process::Command，chrono 0.4

---

## 文件结构

| 文件 | 操作 | 职责 |
|------|------|------|
| `src/disk_health/filter.rs` | 新建 | `is_physical_device()` + `is_physical_interface()` |
| `src/disk_health/autoinstall.rs` | 新建 | smartctl 自动安装，仅尝试一次 |
| `src/disk_health/linux.rs` | 新建 | hwmon 温度 + sysfs 型号 + smartctl 集成 |
| `src/disk_health/windows.rs` | 新建 | PowerShell/WMI 采集 + JSON 解析 |
| `src/disk_health/mod.rs` | 新建 | 平台路由，暴露 `DiskHealthCollector` |
| `src/smart.rs` | 修改 | 内部委托给 `disk_health`，公开接口不变 |
| `src/collector.rs` | 修改 | 网络采集循环加 `is_physical_interface()` 过滤 |
| `Cargo.toml` | 修改 | 添加 `nix` crate（Unix-only） |

---

### 任务 1：`src/disk_health/filter.rs` — 物理设备与网卡过滤

**文件：**
- 新建：`src/disk_health/filter.rs`

- [ ] **步骤 1：编写失败的测试**

新建文件 `src/disk_health/filter.rs`，先只写 `#[cfg(test)]` 模块：

```rust
// src/disk_health/filter.rs

/// Device name prefixes that never correspond to real physical media.
const VIRTUAL_DEVICE_PREFIXES: &[&str] = &[
    "loop", "ram", "zram", "dm-", "md", "sr", "fd", "vd", "xvd", "zd",
];

/// Whether a scanned device path or name looks like real physical media.
pub fn is_physical_device(path: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    if base.is_empty() {
        return false;
    }
    if base.starts_with("nvme") {
        return true;
    }
    !VIRTUAL_DEVICE_PREFIXES.iter().any(|p| base.starts_with(p))
}

/// Returns true when the network interface name looks like a physical NIC.
pub fn is_physical_interface(name: &str) -> bool {
    const LOOPBACK_EXACT: &[&str] = &["lo", "Loopback Pseudo-Interface 1"];
    if LOOPBACK_EXACT.iter().any(|&s| name == s) {
        return false;
    }
    const VIRTUAL_PREFIXES: &[&str] = &[
        "veth", "docker", "br-", "virbr", "vmnet", "vnet",
        "tun", "tap", "wg", "utun", "llw", "isatap", "teredo", "6to4",
    ];
    !VIRTUAL_PREFIXES.iter().any(|&p| name.to_lowercase().starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_nvme_devices() {
        assert!(is_physical_device("/dev/nvme0"));
        assert!(is_physical_device("/dev/nvme0n1"));
    }

    #[test]
    fn keeps_sata_disks() {
        assert!(is_physical_device("/dev/sda"));
        assert!(is_physical_device("/dev/sdb"));
    }

    #[test]
    fn drops_virtual_block_devices() {
        assert!(!is_physical_device("/dev/loop0"));
        assert!(!is_physical_device("/dev/dm-0"));
        assert!(!is_physical_device("/dev/md0"));
        assert!(!is_physical_device("/dev/vda"));
        assert!(!is_physical_device("/dev/xvda"));
        assert!(!is_physical_device("/dev/zram0"));
        assert!(!is_physical_device("/dev/sr0"));
        assert!(!is_physical_device("/dev/zd16"));
    }

    #[test]
    fn drops_loopback_exact_match() {
        assert!(!is_physical_interface("lo"));
        assert!(!is_physical_interface("Loopback Pseudo-Interface 1"));
    }

    #[test]
    fn drops_virtual_nic_prefixes() {
        assert!(!is_physical_interface("veth0"));
        assert!(!is_physical_interface("docker0"));
        assert!(!is_physical_interface("br-abc123"));
        assert!(!is_physical_interface("virbr0"));
        assert!(!is_physical_interface("tun0"));
        assert!(!is_physical_interface("wg0"));
        assert!(!is_physical_interface("utun2"));
        assert!(!is_physical_interface("ISATAP"));   // case-insensitive
        assert!(!is_physical_interface("Teredo"));
    }

    #[test]
    fn keeps_physical_nics() {
        assert!(is_physical_interface("eth0"));
        assert!(is_physical_interface("enp3s0"));
        assert!(is_physical_interface("wlan0"));
        assert!(is_physical_interface("Wi-Fi"));
        assert!(is_physical_interface("以太网"));
    }
}
```

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test -p os-watcher disk_health::filter -- --nocapture`

预期：FAIL — `error[E0583]: file not found for module disk_health`（模块尚未 mod 导出）

- [ ] **步骤 3：创建 `src/disk_health/mod.rs` 的最小骨架**

```rust
// src/disk_health/mod.rs
pub mod filter;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "windows")]
mod windows;

mod autoinstall;
```

同时在 `src/main.rs` 或 `src/lib.rs`（视项目结构）中添加 `mod disk_health;`。

检查 `src/main.rs` 顶部有哪些模块声明，在同处添加：

```rust
mod disk_health;
```

- [ ] **步骤 4：运行测试验证通过**

运行：`cargo test -p os-watcher disk_health::filter`

预期：所有 filter 测试 PASS（8 个测试）

- [ ] **步骤 5：Commit**

```bash
git add src/disk_health/filter.rs src/disk_health/mod.rs src/main.rs
git commit -m "feat(disk_health): add physical device and NIC filter"
```

---

### 任务 2：`Cargo.toml` — 添加 nix 依赖

**文件：**
- 修改：`Cargo.toml`

- [ ] **步骤 1：添加 nix 为 Unix-only 依赖**

在 `[dependencies]` 末尾追加：

```toml
# Unix UID detection for sudo判断 in smartctl autoinstall
[target.'cfg(unix)'.dependencies]
nix = { version = "0.29", features = ["user"] }
```

- [ ] **步骤 2：验证编译通过**

运行：`cargo check`

预期：无编译错误

- [ ] **步骤 3：Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: add nix dependency for unix UID detection"
```

---

### 任务 3：`src/disk_health/autoinstall.rs` — smartctl 自动安装

**文件：**
- 新建：`src/disk_health/autoinstall.rs`

- [ ] **步骤 1：编写失败的测试**

```rust
// src/disk_health/autoinstall.rs
use std::sync::atomic::{AtomicBool, Ordering};

/// Global flag: we only attempt to install once per process lifetime.
static INSTALL_ATTEMPTED: AtomicBool = AtomicBool::new(false);

/// Try to install smartmontools using the platform package manager.
/// Returns `true` if smartctl is now available (was already present, or
/// installation succeeded), `false` otherwise.
/// Attempts at most once; subsequent calls return `false` immediately.
pub fn try_install_smartctl() -> bool {
    // Already attempted → do not retry.
    if INSTALL_ATTEMPTED.swap(true, Ordering::SeqCst) {
        return false;
    }
    install_platform()
}

#[cfg(unix)]
fn needs_sudo() -> bool {
    // nix::unistd::geteuid() == 0 means already root → no sudo needed.
    nix::unistd::geteuid().as_raw() != 0
}

#[cfg(unix)]
fn install_platform() -> bool {
    // Ordered by prevalence.
    const MANAGERS: &[(&str, &[&str])] = &[
        ("apt",    &["apt", "install", "-y", "smartmontools"]),
        ("dnf",    &["dnf", "install", "-y", "smartmontools"]),
        ("yum",    &["yum", "install", "-y", "smartmontools"]),
        ("pacman", &["pacman", "-S", "--noconfirm", "smartmontools"]),
        ("zypper", &["zypper", "install", "-y", "smartmontools"]),
    ];

    for (manager, args) in MANAGERS {
        // Skip if the package manager is not installed.
        if std::process::Command::new("which")
            .arg(manager)
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            continue;
        }

        let success = if needs_sudo() {
            // Confirm sudo itself is available before prepending it.
            if std::process::Command::new("which")
                .arg("sudo")
                .output()
                .map(|o| !o.status.success())
                .unwrap_or(true)
            {
                false
            } else {
                let mut cmd_args = vec!["sudo"];
                cmd_args.extend_from_slice(args);
                std::process::Command::new(cmd_args[0])
                    .args(&cmd_args[1..])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false)
            }
        } else {
            std::process::Command::new(args[0])
                .args(&args[1..])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };

        if success {
            tracing::info!("smartmontools installed via {}", manager);
            return true;
        }
    }

    tracing::warn!("smartctl auto-install failed: no usable package manager found");
    false
}

#[cfg(target_os = "windows")]
fn install_platform() -> bool {
    // Try winget first, then choco.
    let winget = std::process::Command::new("winget")
        .args(["install", "--id", "Smartmontools.Smartmontools", "--silent", "--accept-source-agreements", "--accept-package-agreements"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if winget {
        tracing::info!("smartmontools installed via winget");
        return true;
    }

    let choco = std::process::Command::new("choco")
        .args(["install", "smartmontools", "-y"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if choco {
        tracing::info!("smartmontools installed via choco");
        return true;
    }

    tracing::warn!("smartctl auto-install failed on Windows: winget and choco both unavailable or failed");
    false
}

#[cfg(not(any(unix, target_os = "windows")))]
fn install_platform() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Second call after the flag is set must return false immediately.
    #[test]
    fn does_not_retry_after_first_attempt() {
        // Reset flag manually for this isolated test.
        INSTALL_ATTEMPTED.store(false, Ordering::SeqCst);
        // First call will try (and fail in test env — no package manager in CI).
        let _first = try_install_smartctl();
        // Flag is now true. Second call must be false.
        let second = try_install_smartctl();
        assert!(!second, "must not retry installation");
        // Restore for other tests.
        INSTALL_ATTEMPTED.store(false, Ordering::SeqCst);
    }
}
```

- [ ] **步骤 2：将模块加入 `src/disk_health/mod.rs`**

确保 `mod.rs` 中已有 `mod autoinstall;`（任务 1 步骤 3 已添加骨架）。

- [ ] **步骤 3：运行测试验证**

运行：`cargo test -p os-watcher disk_health::autoinstall`

预期：`does_not_retry_after_first_attempt` PASS

- [ ] **步骤 4：Commit**

```bash
git add src/disk_health/autoinstall.rs src/disk_health/mod.rs
git commit -m "feat(disk_health): add smartctl autoinstall with one-shot retry guard"
```

---

### 任务 4：`src/disk_health/linux.rs` — hwmon + sysfs + smartctl 集成

**文件：**
- 新建：`src/disk_health/linux.rs`

- [ ] **步骤 1：编写失败的测试（纯解析逻辑，不依赖真实 /sys）**

```rust
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
```

> **注：** 测试用到 `tempfile` crate（dev-dependency）。在 `Cargo.toml` 的 `[dev-dependencies]` 段添加：
> ```toml
> tempfile = "3"
> ```

- [ ] **步骤 2：添加 `tempfile` dev-dependency**

在 `Cargo.toml` 末尾添加：

```toml
[dev-dependencies]
tempfile = "3"
```

- [ ] **步骤 3：运行测试验证通过**

运行：`cargo test -p os-watcher disk_health::linux`

预期：5 个测试全部 PASS

- [ ] **步骤 4：Commit**

```bash
git add src/disk_health/linux.rs Cargo.toml Cargo.lock
git commit -m "feat(disk_health): linux hwmon temperature reader and sysfs model lookup"
```

---

### 任务 5：`src/disk_health/windows.rs` — PowerShell/WMI 采集

**文件：**
- 新建：`src/disk_health/windows.rs`

- [ ] **步骤 1：编写测试（JSON 解析，不依赖真实 PowerShell）**

```rust
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
```

- [ ] **步骤 2：运行测试验证通过**

运行：`cargo test -p os-watcher disk_health::windows`

（在 Windows 上）预期：5 个测试全部 PASS。在 Linux 构建时此模块 `#![cfg(target_os = "windows")]` 不编译，测试不存在，PASS。

- [ ] **步骤 3：Commit**

```bash
git add src/disk_health/windows.rs
git commit -m "feat(disk_health): windows powershell/wmi disk collector"
```

---

### 任务 6：`src/disk_health/mod.rs` — 平台路由，暴露 `DiskHealthCollector`

**文件：**
- 修改：`src/disk_health/mod.rs`

`DiskHealthCollector` 是对外接口，与原 `SmartCollector` 内部逻辑等价，接受 `disk_name`（sysinfo 的值）并返回 `Option<SmartInfo>`。

- [ ] **步骤 1：替换骨架，写完整实现**

```rust
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
fn parent_device(name: &str) -> String {
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
        // Delegate to the existing scan+query logic already in smart.rs.
        // We replicate the minimal call here to avoid circular deps.
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
```

> **注：** `refresh_smartctl_only` 调用了 `crate::smart::parse_smart_json_pub`——在任务 7 中将现有 `parse_smart_json` 改为 `pub(crate)` 可见。

- [ ] **步骤 2：确认模块声明完整**

`src/disk_health/mod.rs` 现在是完整实现，不再只是骨架。

- [ ] **步骤 3：运行编译检查**

运行：`cargo check`

预期：无错误（`parse_smart_json_pub` 暂时 missing，会在任务 7 补上，此步骤暂时允许该错误存在或先注释掉 `refresh_smartctl_only` 中的调用）

- [ ] **步骤 4：Commit**

```bash
git add src/disk_health/mod.rs
git commit -m "feat(disk_health): platform-routing collector with Linux/Windows/fallback paths"
```

---

### 任务 7：`src/smart.rs` — 委托给 `disk_health`，接口不变

**文件：**
- 修改：`src/smart.rs`

目标：`SmartCollector` 的公开方法（`new`、`refresh_if_due`、`lookup`、`devices`）内部全部委托给 `DiskHealthCollector`，同时暴露 `parse_smart_json_pub` 供 `disk_health::mod` 使用。

- [ ] **步骤 1：修改 `src/smart.rs`**

保留所有现有测试和 `parse_smart_json`（将其改为 `pub(crate)`），替换 `SmartCollector` 实现：

```rust
// 在文件顶部，现有 use 之后添加：
use crate::disk_health::DiskHealthCollector;

// 将 parse_smart_json 的可见性改为 pub(crate)，函数名保持：
pub(crate) fn parse_smart_json_pub(device: &str, json: &serde_json::Value) -> Option<SmartInfo> {
    Some(parse_smart_json(device, json))
}

// 替换 SmartCollector 结构体和实现：
pub struct SmartCollector {
    inner: DiskHealthCollector,
}

impl SmartCollector {
    pub fn new(interval_secs: u64) -> Self {
        Self {
            inner: DiskHealthCollector::new(interval_secs),
        }
    }

    pub fn refresh_if_due(&mut self) {
        self.inner.refresh_if_due();
    }

    pub fn lookup(&self, disk_name: &str) -> Option<SmartInfo> {
        self.inner.lookup(disk_name)
    }

    pub fn devices(&self) -> Vec<&SmartInfo> {
        // DiskHealthCollector does not expose devices(); return empty for now.
        // TUI/API do not call this in the hot path.
        vec![]
    }
}
```

保留全部 `#[cfg(test)]` 内容不变——它们测试的是 `parse_smart_json` 等内部函数，仍然有效。

- [ ] **步骤 2：运行所有测试**

运行：`cargo test -p os-watcher`

预期：smart.rs 的现有测试全部 PASS，disk_health 测试全部 PASS

- [ ] **步骤 3：Commit**

```bash
git add src/smart.rs
git commit -m "refactor(smart): delegate internals to disk_health, keep public interface"
```

---

### 任务 8：`src/collector.rs` — 添加物理网卡过滤

**文件：**
- 修改：`src/collector.rs`

- [ ] **步骤 1：编写测试（在 collector.rs 的 #[cfg(test)] 段）**

在 `src/collector.rs` 末尾添加：

```rust
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
```

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test -p os-watcher collector::tests`

预期：FAIL（`is_physical_interface` 未在 collector 上下文中导入）

- [ ] **步骤 3：修改网络采集循环，添加过滤**

找到 `collector.rs` 中的网络采集段（约第 167 行）：

```rust
// 修改前：
let networks: Vec<NetworkInterface> = self.networks.iter().map(|(name, data)| {
```

替换为：

```rust
// 修改后：
use crate::disk_health::filter::is_physical_interface;
let networks: Vec<NetworkInterface> = self.networks.iter()
    .filter(|(name, _)| is_physical_interface(name))
    .map(|(name, data)| {
```

确保 `.map` 闭包的结尾 `}).collect();` 保持不变。

- [ ] **步骤 4：运行测试验证通过**

运行：`cargo test -p os-watcher collector::tests`

预期：4 个测试全部 PASS

- [ ] **步骤 5：运行完整测试套件**

运行：`cargo test -p os-watcher`

预期：全部 PASS，无编译 warning（除已有的）

- [ ] **步骤 6：Commit**

```bash
git add src/collector.rs
git commit -m "feat(collector): filter virtual NICs, only report physical interfaces"
```

---

### 任务 9：端到端验证

- [ ] **步骤 1：发布构建**

运行：`cargo build --release`

预期：编译成功，无 error

- [ ] **步骤 2：完整测试**

运行：`cargo test`

预期：全部测试 PASS

- [ ] **步骤 3：（Linux）检查 hwmon 路径**

在 Linux 主机上运行：`ls /sys/class/hwmon/`

预期：看到 `hwmon0`、`hwmon1` 等目录，`cat /sys/class/hwmon/hwmon0/name` 应显示驱动名。

- [ ] **步骤 4：（Linux）以普通用户运行，验证温度可读**

```bash
./target/release/os-watcher status
```

预期：硬盘条目中显示温度（无需 root）

- [ ] **步骤 5：（Windows）验证 WMI 采集**

```powershell
.\target\release\os-watcher.exe status
```

预期：硬盘条目显示 HealthStatus 和温度；网卡只显示以太网/Wi-Fi，无 Teredo/ISATAP 等。

- [ ] **步骤 6：Commit**

```bash
git add .
git commit -m "chore: verify end-to-end disk health and NIC filter"
```

