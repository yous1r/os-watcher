# 设计规格：硬盘健康采集 + 物理网卡过滤

日期：2026-07-28  
状态：已批准

---

## 背景与目标

当前存在两个问题：

1. **硬盘温度 / SMART 数据缺失**：现有 `smart.rs` 完全依赖 `smartctl`，在 smartctl 未安装时降级为全部 `None`；Windows 上设备名映射（`/dev/sdX` 风格）与 sysinfo 实际报告的路径不匹配，导致 SMART 数据永远无法关联到磁盘条目。
2. **虚拟网卡污染**：`collector.rs` 将 sysinfo 枚举到的所有网络接口全部上报，包括回环地址、Docker 桥、VPN tunnel、Windows 隧道适配器等，前端显示冗余。

目标：让 Linux / Windows 均能展示真实的硬盘温度和健康状态；只向前端暴露物理网卡。

---

## 不修改的部分

- `src/types.rs`：`SmartInfo`、`SmartHealth`、`NetworkInterface` 数据结构不变
- `src/tui.rs`、`src/api.rs`：零改动
- `web/` 前端：零改动
- `SmartCollector` 对外接口（`refresh_if_due()`、`lookup()`）签名不变

---

## 新增文件结构

```
src/disk_health/
  mod.rs           路由入口，暴露 DiskHealthCollector（替代原 SmartCollector 内部实现）
  autoinstall.rs   smartctl 自动安装逻辑
  linux.rs         Linux 平台：hwmon + smartctl
  windows.rs       Windows 平台：PowerShell/WMI
  filter.rs        is_physical_device()，从 smart.rs 迁移并扩展
```

`src/smart.rs` 保留对外类型和 `SmartCollector` 结构，内部将平台采集委托给 `disk_health` 模块。

---

## Linux 实现

### 温度（hwmon，无需特权）

内核通过 sysfs hwmon 子系统导出所有热传感器：

```
/sys/class/hwmon/hwmonN/name          → 驱动名（如 "nvme", "drivetemp"）
/sys/class/hwmon/hwmonN/temp1_input   → 温度，单位 millidegree Celsius
/sys/class/hwmon/hwmonN/device/block/ → 目录存在时，子目录名即块设备名（如 "sda"）
```

采集步骤：

1. 枚举 `/sys/class/hwmon/hwmon*`
2. 读 `name`，仅保留驱动名为 `nvme`、`drivetemp`、`megaraid`（SAS 控制器）的条目
3. 读 `temp1_input`，除以 1000 得到摄氏度
4. 通过 `device/block/` 子目录确定对应块设备名，建立 `块设备名 → 温度` 映射

### 型号与介质类型（无需特权）

```
/sys/block/<dev>/device/model      → 设备型号字符串（trim 空白）
/sys/block/<dev>/queue/rotational  → "1" = HDD，"0" = SSD/NVMe
```

### SMART 属性（可选增强，需特权）

SMART 原始属性（重分配扇区、寿命损耗等）通过 `smartctl` 获取：

1. 启动时 `which smartctl` 探测
2. 未找到 → 调用 `autoinstall::try_install_smartctl()`
3. 安装成功或已存在 → 走现有 `scan_devices()` + `query_device()` 逻辑（保持不变）
4. smartctl 不可用时，`SmartInfo` 用 hwmon 温度 + sysfs 型号填充，SMART 属性字段留 `None`

### 降级行为

| smartctl 状态 | 温度 | 型号 | SMART 属性 |
|-------------|------|------|-----------|
| 可用 + 有权限 | hwmon（或 smartctl） | sysfs / smartctl | 完整 |
| 不可用，安装成功 | hwmon | sysfs | 完整 |
| 不可用，安装失败 | hwmon | sysfs | None |

---

## Windows 实现

### PowerShell/WMI 采集

单次调用 PowerShell，输出 JSON：

```powershell
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
```

字段映射：

| PowerShell 字段 | SmartInfo 字段 |
|---------------|--------------|
| Temperature | temperature_celsius |
| Wear | percentage_used |
| HealthStatus = "Healthy" | SmartHealth::Passed |
| HealthStatus = "Unhealthy" | SmartHealth::Failed |
| FriendlyName | model |
| SerialNumber | serial |

### 磁盘名映射

sysinfo 在 Windows 报告挂载点（如 `C:\`）而非设备路径。映射策略：

1. 通过 `DeviceId`（如 `disk0`）与 sysinfo 的 `disk.name()` 做精确匹配
2. 精确匹配失败时，按型号前缀做模糊匹配
3. 仍失败时，`smart` 字段留 `None`

### smartctl 安装（可选增强）

```
winget install --id Smartmontools.Smartmontools --silent
```

失败时尝试：

```
choco install smartmontools -y
```

两者都失败则只用 WMI 路径，不影响温度和健康状态展示。

---

## smartctl 自动安装（`autoinstall.rs`）

### 策略

- **仅在首次发现 smartctl 缺失时尝试一次**，结果缓存在 `unavailable` 标志，不重试
- 安装失败 → `warn!` 一条日志，继续走降级路径

### Linux 包管理器探测顺序

| 包管理器 | 探测命令 | 安装命令 |
|--------|---------|---------|
| apt | which apt | apt install -y smartmontools |
| dnf | which dnf | dnf install -y smartmontools |
| yum | which yum | yum install -y smartmontools |
| pacman | which pacman | pacman -S --noconfirm smartmontools |
| zypper | which zypper | zypper install -y smartmontools |

### sudo 判断

```rust
#[cfg(unix)]
fn needs_sudo() -> bool {
    // nix::unistd::geteuid() == 0 表示已经是 root，不需要 sudo
    nix::unistd::geteuid().as_raw() != 0
}
```

非 root 时，在包管理器命令前加 `sudo`（先确认 sudo 可用）。

### 跨平台安全

- `Command::new` 执行，参数通过数组传递，不拼接 shell 字符串，无注入风险
- 安装命令的 exit code 非零即视为失败

---

## 网卡过滤

### 过滤函数（`collector.rs`）

```rust
fn is_physical_interface(name: &str) -> bool {
    // 精确排除回环接口
    const LOOPBACK_EXACT: &[&str] = &["lo", "Loopback Pseudo-Interface 1"];
    if LOOPBACK_EXACT.iter().any(|&s| name == s) {
        return false;
    }
    // 虚拟接口前缀排除
    const VIRTUAL_PREFIXES: &[&str] = &[
        "veth",    // Linux 容器 veth pair
        "docker",  // Docker 桥
        "br-",     // Linux 软件网桥
        "virbr",   // libvirt 桥
        "vmnet",   // VMware 宿主虚拟网卡
        "vnet",    // libvirt/KVM tap
        "tun",     // TUN 虚拟设备
        "tap",     // TAP 虚拟设备
        "wg",      // WireGuard
        "utun",    // macOS VPN tunnel
        "llw",     // macOS Low Latency WLAN
        "isatap",  // Windows ISATAP 隧道
        "teredo",  // Windows Teredo 隧道
        "6to4",    // Windows 6to4 隧道
    ];
    !VIRTUAL_PREFIXES.iter().any(|&p| name.to_lowercase().starts_with(p))
}
```

### 应用位置

`collector.rs` 的 `collect()` 方法中，网络采集循环改为：

```rust
let networks: Vec<NetworkInterface> = self.networks.iter()
    .filter(|(name, _)| is_physical_interface(name))
    .map(|(name, data)| { ... })
    .collect();
```

过滤在采集侧完成，API 响应和 TUI 均自动生效。

---

## 测试覆盖

| 模块 | 测试 |
|-----|-----|
| `disk_health/filter.rs` | 各前缀过滤、回环精确匹配、正常接口不被过滤 |
| `disk_health/linux.rs` | hwmon 路径解析（mock sysfs）、设备名映射 |
| `disk_health/windows.rs` | PowerShell JSON 解析、字段映射、HealthStatus 转换 |
| `disk_health/autoinstall.rs` | 探测到 smartctl 不尝试安装；安装失败不 panic |
| `collector.rs` | is_physical_interface 单元测试 |

---

## 实现顺序

1. `src/disk_health/filter.rs` — 物理设备判断，迁移并扩展
2. `src/disk_health/autoinstall.rs` — smartctl 安装逻辑
3. `src/disk_health/linux.rs` — hwmon 温度 + sysfs 型号 + smartctl 集成
4. `src/disk_health/windows.rs` — PowerShell 采集 + JSON 解析
5. `src/disk_health/mod.rs` — 平台路由，暴露统一接口
6. `src/smart.rs` — 内部委托给 disk_health，接口不变
7. `src/collector.rs` — 添加 is_physical_interface 过滤
8. 测试验证
