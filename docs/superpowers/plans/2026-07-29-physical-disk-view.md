# 物理磁盘视图 实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 把详情页磁盘面板从"挂载点列表"改造成"物理盘卡片"视图——每块物理盘一张卡片（型号、容量、健康、温度、寿命、通电、整盘读写、盘类型），卡片内折叠展示该盘上的分区/挂载点及各自使用率。

**架构：** 后端 `types.rs` 新增 `PhysicalDisk` / `Partition` / `DiskType`，`SystemMetrics` 增加 `physical_disks` 字段（`#[serde(default)]`，不动现有 `disks`）。`collector.rs` 新增纯函数 `assemble_physical_disks`，把 `sysinfo` 挂载点按父设备归并到 `DiskHealthCollector` 缓存的物理盘下，Linux 从 sysfs/diskstats 补容量与整盘 I/O。`windows.rs` PowerShell 脚本补一段盘符→disk 号关联查询。前端 `NodeDetail.tsx` 渲染卡片。

**技术栈：** Rust（serde、sysinfo）、SolidJS + TypeScript、Vite。

**参考：**
- 设计文档：`docs/superpowers/specs/2026-07-29-physical-disk-view-design.md`
- 现有类型：`src/types.rs`（`DiskInfo`、`SmartInfo`、`SmartHealth`）
- 现有采集：`src/collector.rs:128-164`（磁盘组装）、`src/disk_health/mod.rs:99-102`（`devices()`）
- I/O 计数：`src/diskstats.rs`（`DiskStats::lookup` 返回 `DeviceDelta`）
- Windows WMI：`src/disk_health/windows.rs`（`parse_wmi_json`）
- 前端展示：`web/src/views/NodeDetail.tsx:146-213`、`web/src/types.ts:49-63`

---

## 文件结构

- **修改** `src/types.rs`：新增 `DiskType`、`Partition`、`PhysicalDisk`；`SystemMetrics` 加 `physical_disks` 字段。
- **修改** `src/collector.rs`：新增 `assemble_physical_disks` 及其辅助函数，在 `collect()` 里填充新字段；新增单元测试。
- **修改** `src/disk_health/mod.rs`：暴露 `parent_device` 供 collector 复用（当前是私有的），并给 `DiskHealthCollector` 加一个按设备名取 `SmartInfo` 的方法。
- **修改** `src/disk_health/windows.rs`：PowerShell 脚本补盘符→disk 号映射，新增解析函数与测试。
- **修改** `web/src/types.ts`：新增 `PhysicalDisk` / `Partition` / `DiskType` 接口。
- **修改** `web/src/views/NodeDetail.tsx`：磁盘面板改为物理盘卡片渲染。
- **修改** `web/src/styles.css`：物理盘卡片样式。

---

## 任务 1：后端数据模型

**文件：**
- 修改：`src/types.rs`（在 `DiskInfo` 之后、`SmartHealth` 之前插入新类型；`SystemMetrics` 加字段）
- 测试：`src/types.rs`（本任务无独立测试，序列化行为由任务 2 覆盖）

- [ ] **步骤 1：新增 `DiskType`、`Partition`、`PhysicalDisk`**

在 `src/types.rs` 中 `DiskInfo` 结构（以 `pub smart: Option<SmartInfo>,` 结尾、行 69 的 `}`）之后插入：

```rust
/// 物理盘的介质类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiskType {
    /// 机械硬盘
    Hdd,
    /// SATA/SAS 固态盘
    Ssd,
    /// NVMe 固态盘
    Nvme,
    /// 无法判定
    Unknown,
}

/// 物理盘上的一个分区/挂载点
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Partition {
    /// 分区设备名或盘符，如 "/dev/nvme0n1p2"、"C:\\"
    pub name: String,
    /// 挂载点
    pub mount_point: String,
    /// 文件系统类型
    pub fs_type: String,
    /// 总容量（字节）
    pub total_bytes: u64,
    /// 已用容量（字节）
    pub used_bytes: u64,
    /// 使用率（0-100）
    pub usage_percent: f32,
}

/// 一块物理磁盘及其上的分区
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhysicalDisk {
    /// 物理设备路径/标识，如 "/dev/nvme0n1"、"disk0"——悬浮时展示
    pub device: String,
    /// 型号，主标题；拿不到时前端回退到设备名
    pub model: Option<String>,
    /// 介质类型
    pub disk_type: DiskType,
    /// 整盘物理容量（字节）；未知时为 0
    pub total_bytes: u64,
    /// SMART 健康数据（复用已有结构），无则 None
    pub smart: Option<SmartInfo>,
    /// 整盘读速率（字节/秒）
    pub read_bytes_per_sec: f64,
    /// 整盘写速率（字节/秒）
    pub write_bytes_per_sec: f64,
    /// I/O 是否为该盘真实计数（true）还是全机聚合（false）
    pub per_device_io: bool,
    /// 该盘上的分区/挂载点
    pub partitions: Vec<Partition>,
}
```

- [ ] **步骤 2：`SystemMetrics` 增加 `physical_disks` 字段**

在 `src/types.rs` 的 `SystemMetrics` 结构里，紧跟 `pub disks: Vec<DiskInfo>,` 之后插入：

```rust
    /// 物理磁盘视图（每块盘含其分区）。新增字段，旧节点数据缺失时为空。
    #[serde(default)]
    pub physical_disks: Vec<PhysicalDisk>,
```

- [ ] **步骤 3：编译检查**

运行：`cargo build`
预期：编译失败，报 `src/collector.rs:243` 构造 `SystemMetrics` 缺少字段 `physical_disks`。这是预期的——任务 2 补上。

- [ ] **步骤 4：Commit**

```bash
git add src/types.rs
git commit -m "feat(types): 新增 PhysicalDisk/Partition/DiskType 数据模型"
```

---

## 任务 2：暴露 `parent_device` 与设备查询辅助

**文件：**
- 修改：`src/disk_health/mod.rs:29-35`（`parent_device` 改为 `pub(crate)`）、`src/disk_health/mod.rs:99-102`（新增 `smart_for` 方法）

- [ ] **步骤 1：把 `parent_device` 改为 crate 可见**

在 `src/disk_health/mod.rs`，将行 29 的函数签名：

```rust
fn parent_device(name: &str) -> String {
```

改为：

```rust
pub(crate) fn parent_device(name: &str) -> String {
```

- [ ] **步骤 2：给 `DiskHealthCollector` 加 `snapshot` 方法**

collector 需要"设备名 → SmartInfo"的完整映射来构造物理盘列表。现有 `devices()` 只返回 `Vec<&SmartInfo>`，缺 key。在 `src/disk_health/mod.rs` 的 `devices()` 方法（行 100-102）之后新增：

```rust
    /// 缓存的完整快照：设备标识 → SmartInfo 克隆。
    /// collector 用它作为物理盘列表的来源。
    pub fn snapshot(&self) -> std::collections::HashMap<String, SmartInfo> {
        self.cache.clone()
    }
```

- [ ] **步骤 3：在 `SmartCollector` 上转发 `snapshot`**

`collector.rs` 通过 `SmartCollector` 间接使用 `DiskHealthCollector`。在 `src/smart.rs` 的 `devices()` 方法（行 64-66）之后新增：

```rust
    /// 缓存的完整快照：设备标识 → SmartInfo。
    pub fn snapshot(&self) -> std::collections::HashMap<String, SmartInfo> {
        self.inner.snapshot()
    }
```

- [ ] **步骤 4：编译检查**

运行：`cargo build`
预期：仍报 `physical_disks` 缺失（任务 1 遗留），但不应有新的关于 `parent_device`/`snapshot` 的错误。

- [ ] **步骤 5：Commit**

```bash
git add src/disk_health/mod.rs src/smart.rs
git commit -m "feat(disk_health): 暴露 parent_device 与缓存快照供 collector 复用"
```

---

## 任务 3：`assemble_physical_disks` 核心逻辑与测试（跨平台部分）

**文件：**
- 修改：`src/collector.rs`（新增函数与辅助函数、测试）

本任务实现平台无关的组装骨架：以 SMART 缓存快照为物理盘来源，把挂载点按父设备归并，归不到的进"未知盘"兜底，并推断盘类型。Linux 专属的容量/IO 补全放到任务 4。

- [ ] **步骤 1：编写失败的测试**

在 `src/collector.rs` 末尾的 `#[cfg(test)] mod tests` 块内（现有 `physical_nics_are_kept` 测试之后、`}` 之前）加入。先在测试模块顶部补 import：

```rust
    use crate::collector::{assemble_physical_disks, DiskInput};
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
```

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test --lib partitions_group_under_parent_device`
预期：编译失败——`assemble_physical_disks`、`DiskInput` 未定义。

- [ ] **步骤 3：实现 `DiskInput`、`assemble_physical_disks` 与盘类型推断**

在 `src/collector.rs` 顶部 import 区补上（`use crate::types::*;` 已存在，`PhysicalDisk` 等随之可见）。在文件中 `impl MetricsCollector` 之前（约行 28）插入以下平台无关代码：

```rust
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

/// 把挂载点归并到物理盘下。
///
/// `smart` 是设备标识（`/dev/sda`、`disk0`）→ `SmartInfo` 的缓存快照，作为
/// 物理盘来源。每个挂载点用 `parent_device` 归到父盘；归不到已知盘的挂载点
/// 落入合成的 `"unknown"` 盘，保证不丢数据。
///
/// 整盘容量/IO 的平台相关补全由调用方在此结果上完成（见 `enrich_linux_disk`）。
pub fn assemble_physical_disks(
    inputs: &[DiskInput],
    smart: &std::collections::HashMap<String, SmartInfo>,
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
        let key = match_physical_disk(&input.name, smart, parent_device);
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

        // 整盘 IO 取该盘上各分区里的最大值：Linux 下父设备行会经 enrich 覆盖；
        // 非 Linux 下每个分区携带的都是同一个全机聚合值，取其一即可。
        if input.read_bytes_per_sec > disk.read_bytes_per_sec {
            disk.read_bytes_per_sec = input.read_bytes_per_sec;
        }
        if input.write_bytes_per_sec > disk.write_bytes_per_sec {
            disk.write_bytes_per_sec = input.write_bytes_per_sec;
        }
        if !input.per_device_io {
            // 只要有分区是聚合值，就标记为非按盘计数。
            disk.per_device_io = disk.per_device_io && false;
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
    // 稳定排序：有型号的在前，其余按设备名，"unknown" 垫底，便于展示与测试。
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
/// 依次尝试：精确命中缓存 → 父设备命中缓存 → 前缀匹配缓存中的某个 key；
/// 都不中则归入 `"unknown"`。
fn match_physical_disk(
    partition_name: &str,
    smart: &std::collections::HashMap<String, SmartInfo>,
    parent_device: fn(&str) -> String,
) -> String {
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
```

**注意 `per_device_io` 逻辑：** 空壳初始为 `false`，Linux enrich 阶段（任务 4）会把真正拿到按盘计数的盘置 `true`。上面循环里对聚合值的分区不会误置 `true`。

- [ ] **步骤 4：运行测试验证通过**

运行：`cargo test --lib assemble_physical_disks partitions_group unmatched_partition disk_type_inferred`
预期：三个测试 PASS。

- [ ] **步骤 5：Commit**

```bash
git add src/collector.rs
git commit -m "feat(collector): 新增 assemble_physical_disks 归并挂载点到物理盘"
```

---

## 任务 4：Linux 整盘容量与 I/O 补全

**文件：**
- 修改：`src/collector.rs`（新增 `enrich_linux_disk` 及在 `assemble_physical_disks` 后调用）

- [ ] **步骤 1：编写失败的测试**

在 `src/collector.rs` 测试模块内新增。该测试只验证纯函数 `disk_sysfs_size_bytes` 的容量换算与设备名归一化（读真实 sysfs 依赖平台，不在单测覆盖）：

```rust
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
```

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test --lib sysfs_size_converts device_basename_strips`
预期：编译失败——`sectors_to_bytes`、`device_basename` 未定义。

- [ ] **步骤 3：实现容量换算与 Linux enrich**

在 `src/collector.rs` 中 `match_physical_disk` 之后插入。`sectors_to_bytes` 与 `device_basename` 为跨平台纯函数；`enrich_linux_disks` 仅在 Linux 编译：

```rust
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
```

- [ ] **步骤 4：运行测试验证通过**

运行：`cargo test --lib sysfs_size_converts device_basename_strips`
预期：两个测试 PASS。

- [ ] **步骤 5：Commit**

```bash
git add src/collector.rs
git commit -m "feat(collector): Linux 下补全物理盘容量与按盘 I/O"
```

---

## 任务 5：在 `collect()` 中填充 `physical_disks`

**文件：**
- 修改：`src/collector.rs:128-164`（磁盘循环，收集 `DiskInput`）、`src/collector.rs:243-254`（构造 `SystemMetrics`）

- [ ] **步骤 1：在磁盘循环中收集 `DiskInput`**

`src/collector.rs` 现有的 `let disks: Vec<DiskInfo> = self.disks.iter().map(...)` 块（行 128-164）保留不动。在该块**之后**、`// --- Networks ---`（行 166）之前，插入收集 `DiskInput` 的循环。注意此处仍需 `diskstats`、`smart` 的借用，与上方 `DiskInfo` 循环一致：

```rust
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
        let mut physical_disks = assemble_physical_disks(&disk_inputs, &smart_snapshot);
        enrich_linux_disks(&mut physical_disks, diskstats);
```

**注意借用顺序：** `smart` 和 `diskstats` 在行 126-127 已绑定为 `&self.smart` / `&self.diskstats`。`smart.snapshot()` 需要 `SmartCollector::snapshot`（任务 2 已加）。此块必须在这两个绑定的作用域内、且在 `disks` 向量构造之后。

- [ ] **步骤 2：把字段写入 `SystemMetrics`**

`src/collector.rs` 结尾构造 `SystemMetrics { ... }`（行 243）中，`disks,` 之后插入：

```rust
            physical_disks,
```

- [ ] **步骤 3：编译并跑全部测试**

运行：`cargo build && cargo test --lib`
预期：编译通过；此前所有测试 + 新增测试全部 PASS。

- [ ] **步骤 4：Commit**

```bash
git add src/collector.rs
git commit -m "feat(collector): collect() 填充 physical_disks 字段"
```

---

## 任务 6：Windows 盘符→物理盘映射

**文件：**
- 修改：`src/disk_health/windows.rs`（PowerShell 脚本、新增 `query_partition_map` 与解析函数、测试）

Windows 上 `sysinfo` 报盘符（`C:\`），物理盘是 `disk0`。本任务提供盘符→disk 号的映射数据，供 collector 在 Windows 上把分区正确归并（Linux 的 `parent_device` 对盘符无效，会全部落入 unknown，故需此映射）。

> **说明：** 任务 3 的 `match_physical_disk` 目前对 Windows 盘符只能落入 unknown。本任务先产出并测试映射解析逻辑；将映射接入 `match_physical_disk` 的工作在步骤 4 完成。

- [ ] **步骤 1：编写失败的测试**

在 `src/disk_health/windows.rs` 的 `#[cfg(test)] mod tests` 内新增：

```rust
    const PARTITION_MAP_JSON: &str = r#"[
        {"DriveLetter":"C","DiskNumber":0},
        {"DriveLetter":"D","DiskNumber":0},
        {"DriveLetter":"E","DiskNumber":1}
    ]"#;

    #[test]
    fn parses_drive_letter_to_disk_number() {
        let map = parse_partition_map(PARTITION_MAP_JSON);
        assert_eq!(map.get("C:\\"), Some(&"disk0".to_string()));
        assert_eq!(map.get("D:\\"), Some(&"disk0".to_string()));
        assert_eq!(map.get("E:\\"), Some(&"disk1".to_string()));
    }

    #[test]
    fn partition_map_handles_single_object() {
        let json = r#"{"DriveLetter":"C","DiskNumber":0}"#;
        let map = parse_partition_map(json);
        assert_eq!(map.get("C:\\"), Some(&"disk0".to_string()));
    }

    #[test]
    fn partition_map_skips_null_drive_letter() {
        // 无盘符的分区（恢复分区等）应被跳过。
        let json = r#"[{"DriveLetter":null,"DiskNumber":0}]"#;
        let map = parse_partition_map(json);
        assert!(map.is_empty());
    }
```

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test --lib parses_drive_letter_to_disk_number`
预期（在 Windows 上）：编译失败——`parse_partition_map` 未定义。

> **非 Windows 环境说明：** `windows.rs` 以 `#![cfg(target_os = "windows")]` 开头，Linux/macOS 下整个模块不编译，这些测试不会运行。CI 的 Windows job 会覆盖。本地若在非 Windows 上，跳过步骤 2/4 的运行验证，仅确保代码结构正确。

- [ ] **步骤 3：实现 `parse_partition_map` 与 `query_partition_map`**

在 `src/disk_health/windows.rs` 的 `query_physical_disks` 函数之后新增：

```rust
/// 查询盘符→物理盘号的映射。
///
/// `Get-Partition` 给出 DriveLetter 与 DiskNumber，据此把 sysinfo 报的盘符
/// （`C:\`）对应到 `Get-PhysicalDisk` 的 DeviceId（`disk0`）。
pub fn query_partition_map() -> std::collections::HashMap<String, String> {
    let script = r#"
Get-Partition | Where-Object { $_.DriveLetter } | ForEach-Object {
  [PSCustomObject]@{
    DriveLetter = [string]$_.DriveLetter
    DiskNumber  = $_.DiskNumber
  }
} | ConvertTo-Json -Compress
"#;

    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output();

    match output {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            parse_partition_map(&text)
        }
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            tracing::warn!("PowerShell partition query failed: {}", err.trim());
            std::collections::HashMap::new()
        }
        Err(e) => {
            tracing::warn!("Could not run PowerShell: {}", e);
            std::collections::HashMap::new()
        }
    }
}

/// 一条 Get-Partition 结果。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PartitionRow {
    drive_letter: Option<String>,
    disk_number: Option<u32>,
}

/// 解析 Get-Partition 的 JSON，返回 `"C:\\"` → `"disk0"` 映射。
/// 单对象与数组两种形态都处理，无盘符或无盘号的行跳过。
pub(crate) fn parse_partition_map(text: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();

    let json_text = text.trim();
    if json_text.is_empty() {
        return map;
    }
    let normalised = if json_text.starts_with('{') {
        format!("[{}]", json_text)
    } else {
        json_text.to_string()
    };

    let rows: Vec<PartitionRow> = match serde_json::from_str(&normalised) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("Failed to parse partition JSON: {}", e);
            return map;
        }
    };

    for row in rows {
        let (Some(letter), Some(num)) = (row.drive_letter, row.disk_number) else {
            continue;
        };
        let letter = letter.trim();
        if letter.is_empty() {
            continue;
        }
        // sysinfo 报的挂载点形如 "C:\"，统一成该形态作为 key。
        let mount = format!("{}:\\", letter);
        map.insert(mount, format!("disk{}", num));
    }

    map
}
```

- [ ] **步骤 4：运行测试验证通过（Windows）**

运行：`cargo test --lib parse_partition_map parses_drive_letter partition_map_handles partition_map_skips`
预期（Windows）：全部 PASS。非 Windows：跳过，见步骤 2 说明。

- [ ] **步骤 5：Commit**

```bash
git add src/disk_health/windows.rs
git commit -m "feat(disk_health): Windows 盘符→物理盘号映射查询"
```

---

## 任务 7：把 Windows 映射接入归并逻辑

**文件：**
- 修改：`src/collector.rs`（`assemble_physical_disks` 增加可选映射参数；调用方传入）
- 修改：`src/disk_health/mod.rs`（新增跨平台的 `partition_map` 转发）

- [ ] **步骤 1：给 `DiskHealthCollector`/`SmartCollector` 加跨平台 `partition_map`**

Windows 返回真实映射，其它平台返回空。先在 `src/disk_health/mod.rs` 的 `snapshot` 方法之后新增：

```rust
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
```

再在 `src/smart.rs` 的 `snapshot` 转发之后新增：

```rust
    /// 盘符→物理盘标识映射（仅 Windows 非空）。
    pub fn partition_map(&self) -> std::collections::HashMap<String, String> {
        self.inner.partition_map()
    }
```

- [ ] **步骤 2：编写失败的测试**

在 `src/collector.rs` 测试模块新增，验证带映射时盘符正确归并：

```rust
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
```

同时在测试模块 import 处补 `assemble_physical_disks_with_map`：

```rust
    use crate::collector::assemble_physical_disks_with_map;
```

- [ ] **步骤 3：运行测试验证失败**

运行：`cargo test --lib windows_drive_letters_map_to_physical_disk`
预期：编译失败——`assemble_physical_disks_with_map` 未定义。

- [ ] **步骤 4：重构 `assemble_physical_disks` 支持映射**

把任务 3 的 `assemble_physical_disks` 改为薄封装，核心逻辑移入带映射版本。将 `match_physical_disk` 的签名扩展为接受映射，映射命中优先于 `parent_device`。

替换 `assemble_physical_disks` 函数定义为：

```rust
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
```

并把 `match_physical_disk` 替换为（新增 `partition_map` 参数，最优先匹配）：

```rust
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
    if let Some(k) = smart
        .keys()
        .find(|k| partition_name.starts_with(k.as_str()))
    {
        return k.clone();
    }
    "unknown".to_string()
}
```

- [ ] **步骤 5：更新 `collect()` 传入映射**

在 `src/collector.rs` 的 `collect()` 中（任务 5 步骤 1 加的那段），把：

```rust
        let smart_snapshot = smart.snapshot();
        let mut physical_disks = assemble_physical_disks(&disk_inputs, &smart_snapshot);
        enrich_linux_disks(&mut physical_disks, diskstats);
```

改为：

```rust
        let smart_snapshot = smart.snapshot();
        let partition_map = smart.partition_map();
        let mut physical_disks =
            assemble_physical_disks_with_map(&disk_inputs, &smart_snapshot, &partition_map);
        enrich_linux_disks(&mut physical_disks, diskstats);
```

- [ ] **步骤 6：运行全部测试**

运行：`cargo build && cargo test --lib`
预期：编译通过，全部测试 PASS（含 `windows_drive_letters_map_to_physical_disk`）。

- [ ] **步骤 7：Commit**

```bash
git add src/collector.rs src/disk_health/mod.rs src/smart.rs
git commit -m "feat(collector): 接入 Windows 盘符映射完成物理盘归并"
```

---

## 任务 8：前端类型定义

**文件：**
- 修改：`web/src/types.ts`（在 `DiskInfo` 之后新增，`SystemMetrics` 加字段）

- [ ] **步骤 1：新增 `DiskType` / `Partition` / `PhysicalDisk` 接口**

在 `web/src/types.ts` 的 `DiskInfo` 接口（以 `smart: SmartInfo | null;` 结尾的 `}`，行 63）之后插入。字段名与 Rust serde 输出（snake_case；enum 为 PascalCase 变体字符串）对齐：

```typescript
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
```

- [ ] **步骤 2：`SystemMetrics` 加 `physical_disks` 字段**

在 `web/src/types.ts` 的 `SystemMetrics` 接口里，紧跟 `disks: DiskInfo[];` 之后插入：

```typescript
  physical_disks: PhysicalDisk[];
```

- [ ] **步骤 3：类型检查**

运行：`cd web && npx tsc --noEmit`
预期：无类型错误（此时字段新增但尚未被组件使用，合法）。

- [ ] **步骤 4：Commit**

```bash
git add web/src/types.ts
git commit -m "feat(web): 新增 PhysicalDisk/Partition/DiskType 前端类型"
```

---

## 任务 9：前端物理盘卡片渲染

**文件：**
- 修改：`web/src/views/NodeDetail.tsx:146-213`（磁盘面板）、顶部 import
- 修改：`web/src/styles.css`（卡片样式，追加到文件末尾）

- [ ] **步骤 1：新增盘类型与显示辅助**

在 `web/src/views/NodeDetail.tsx` 顶部，`import` 之后、`type SortKey` 之前插入辅助函数与徽章文案：

```tsx
import type { PhysicalDisk, DiskType } from "../types";

/** 盘类型徽章文案。 */
function diskTypeLabel(t: DiskType): string {
  switch (t) {
    case "Hdd":
      return "HDD";
    case "Ssd":
      return "SSD";
    case "Nvme":
      return "NVMe";
    default:
      return "未知";
  }
}

/** 卡片主标题：型号优先，缺失时回退设备名。 */
function diskTitle(d: PhysicalDisk): string {
  return d.model ?? d.device;
}

/** 悬浮 tooltip：完整型号 + 设备名。 */
function diskTooltip(d: PhysicalDisk): string {
  return d.model ? `${d.model} (${d.device})` : d.device;
}
```

> 注意：`import type { NodeSnapshot, ProcessInfo } from "../types";` 已存在于行 3，可将 `PhysicalDisk, DiskType` 合并进该行而非新增 import 行，避免重复。合并后为：
> `import type { NodeSnapshot, ProcessInfo, PhysicalDisk, DiskType } from "../types";`
> 采用合并方式，删除上面单独的 import 行。

- [ ] **步骤 2：替换磁盘面板 JSX**

将 `web/src/views/NodeDetail.tsx` 中磁盘面板整块（行 147-213，即 `<div class="panel"><h3>磁盘</h3> ... </div>` 包含 `<For each={metrics().disks}>` 的那个 panel）替换为遍历 `physical_disks` 的卡片列表：

```tsx
              <div class="panel">
                <h3>磁盘</h3>
                <For each={metrics().physical_disks}>
                  {(disk) => (
                    <div
                      class="disk-card"
                      classList={{
                        crit: disk.smart?.health === "Failed",
                      }}
                    >
                      <div class="disk-card-head" title={diskTooltip(disk)}>
                        <span class="disk-model">{diskTitle(disk)}</span>
                        <span class="disk-badges">
                          <span class="disk-type-badge">
                            {diskTypeLabel(disk.disk_type)}
                          </span>
                          <Show when={disk.smart}>
                            {(s) => (
                              <span
                                class="smart-health"
                                classList={{
                                  ok: s().health === "Passed",
                                  crit: s().health === "Failed",
                                  unknown: s().health === "Unknown",
                                }}
                              >
                                {s().health === "Passed"
                                  ? "健康"
                                  : s().health === "Failed"
                                    ? "异常"
                                    : "未知"}
                              </span>
                            )}
                          </Show>
                        </span>
                      </div>

                      <div class="disk-card-summary">
                        <Show when={disk.total_bytes > 0}>
                          <span>{formatBytes(disk.total_bytes)}</span>
                        </Show>
                        <Show when={disk.smart?.temperature_celsius != null}>
                          <span>{disk.smart!.temperature_celsius}°C</span>
                        </Show>
                        <Show when={disk.smart?.percentage_used != null}>
                          <span>寿命已用 {disk.smart!.percentage_used}%</span>
                        </Show>
                        <Show when={disk.smart?.power_on_hours != null}>
                          <span>通电 {disk.smart!.power_on_hours}h</span>
                        </Show>
                        <Show when={(disk.smart?.reallocated_sectors ?? 0) > 0}>
                          <span class="crit">
                            重分配扇区 {disk.smart!.reallocated_sectors}
                          </span>
                        </Show>
                      </div>

                      <div class="disk-card-io">
                        <span>读 {formatRate(disk.read_bytes_per_sec)}</span>
                        <span>写 {formatRate(disk.write_bytes_per_sec)}</span>
                        <Show when={!disk.per_device_io}>
                          <span
                            class="io-note"
                            title="内核未提供按设备计数，此处为全机聚合值"
                          >
                            全机
                          </span>
                        </Show>
                      </div>

                      <div class="disk-partitions">
                        <For each={disk.partitions}>
                          {(p) => {
                            const tone = usageTone(p.usage_percent);
                            return (
                              <div class="part-row">
                                <div class="part-mount">{p.mount_point}</div>
                                <div class="bar">
                                  <div
                                    class="bar-fill"
                                    classList={{ [tone]: true }}
                                    style={{
                                      width: `${Math.min(p.usage_percent, 100)}%`,
                                    }}
                                  />
                                </div>
                                <div class="part-detail">
                                  {formatBytes(p.used_bytes)} /{" "}
                                  {formatBytes(p.total_bytes)} (
                                  {p.usage_percent.toFixed(0)}%)
                                  <Show when={p.fs_type}>
                                    <span class="part-fs"> {p.fs_type}</span>
                                  </Show>
                                </div>
                              </div>
                            );
                          }}
                        </For>
                      </div>
                    </div>
                  )}
                </For>
              </div>
```

- [ ] **步骤 3：追加卡片样式**

在 `web/src/styles.css` 末尾追加：

```css
/* ---- 物理盘卡片 ---- */
.disk-card {
  border: 1px solid var(--border, #2a2f3a);
  border-radius: 8px;
  padding: 10px 12px;
  margin-bottom: 10px;
}
.disk-card.crit {
  border-color: var(--crit, #e5484d);
}
.disk-card-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  margin-bottom: 6px;
}
.disk-model {
  font-weight: 600;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.disk-badges {
  display: flex;
  align-items: center;
  gap: 6px;
  flex-shrink: 0;
}
.disk-type-badge {
  font-size: 0.75em;
  padding: 1px 6px;
  border-radius: 4px;
  background: var(--badge-bg, #2a2f3a);
}
.disk-card-summary,
.disk-card-io {
  display: flex;
  flex-wrap: wrap;
  gap: 12px;
  font-size: 0.85em;
  color: var(--muted, #9aa4b2);
  margin-bottom: 4px;
}
.disk-partitions {
  margin-top: 8px;
  padding-top: 8px;
  border-top: 1px dashed var(--border, #2a2f3a);
}
.part-row {
  display: grid;
  grid-template-columns: 80px 1fr auto;
  align-items: center;
  gap: 8px;
  margin-bottom: 4px;
}
.part-mount {
  font-size: 0.85em;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.part-detail {
  font-size: 0.8em;
  color: var(--muted, #9aa4b2);
  white-space: nowrap;
}
.part-fs {
  opacity: 0.7;
}
```

> **样式变量说明：** 上面用了 `var(--xxx, fallback)` 形式，即便现有 `styles.css` 未定义这些变量也会用 fallback 色，不会破坏现有主题。若现有文件已有同名变量则自动沿用。

- [ ] **步骤 4：构建前端**

运行：`cd web && npm run build`
预期：构建成功，无 TS 错误。

- [ ] **步骤 5：Commit**

```bash
git add web/src/views/NodeDetail.tsx web/src/styles.css
git commit -m "feat(web): 磁盘面板改为物理盘卡片视图"
```

---

## 任务 10：端到端验证

**文件：** 无（仅验证）

- [ ] **步骤 1：后端全量测试**

运行：`cargo test`
预期：全部 PASS（含新增的 collector、windows 测试）。

- [ ] **步骤 2：release 构建**

运行：`cargo build --release`
预期：编译成功。

- [ ] **步骤 3：前端构建**

运行：`cd web && npm run build`
预期：`web/dist` 产物更新，无错误。

- [ ] **步骤 4：本地冒烟（可选，Linux/Windows 任一）**

运行：`cargo run --release -- gen-config --profile full > /tmp/c.toml && cargo run --release -- --config /tmp/c.toml start --web`
访问 `http://localhost:7980`，确认磁盘面板显示物理盘卡片、卡片内有分区行、悬浮标题显示型号+设备名。
结束后清理：`rm -f /tmp/c.toml os-watcher.db`

- [ ] **步骤 5：无临时文件残留检查**

运行：`git status`
预期：无意外的未跟踪文件（`os-watcher.db`、临时 config 已清理）。

---

## 自检记录

**规格覆盖度：**
- 数据模型（设计 §1）→ 任务 1、8 ✅
- 分区归并 + Windows 映射 + 兜底 + 盘类型（设计 §2）→ 任务 3、4、6、7 ✅
- 前端卡片展示（设计 §3）→ 任务 9 ✅
- 测试与验证（设计 §4）→ 任务 3/4/6 单测 + 任务 10 端到端 ✅
- 保留旧 `disks` 字段不动 → 任务 1 仅新增字段，storage 存整体 JSON 兼容 ✅

**类型一致性：** `DiskInput`（任务 3 定义 → 任务 5、7 使用）、`assemble_physical_disks`（任务 3 → 任务 7 重构为 `_with_map` 封装）、`match_physical_disk` 签名在任务 3 定义、任务 7 扩展参数并同步所有调用点、`snapshot`/`partition_map`（任务 2、7 在 mod.rs 与 smart.rs 成对新增）、`DiskType` 变体字符串前后端一致（Rust PascalCase 枚举 ↔ TS `"Hdd"|"Ssd"|"Nvme"|"Unknown"`）。均一致。

**占位符扫描：** 无 TODO/待定；每个代码步骤含完整代码。
