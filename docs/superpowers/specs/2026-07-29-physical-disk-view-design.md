# 物理磁盘视图设计

## 背景

详情页当前的"磁盘"面板遍历 `sysinfo` 的 `Disks`，展示的是**挂载点/分区**（`mount_point`、`fs_type`、容量使用率）。物理盘的 SMART/健康数据（`DiskHealthCollector` 采集的 `/dev/sda`、`nvme0n1`、Windows `disk0` 等）目前只是作为附加标注挂在挂载点行上。

目标：把展示改造成以**物理磁盘为主体**的视图——每块物理盘一张卡片（型号、容量、健康、温度、寿命、通电、整盘读写、盘类型），卡片内折叠展示该盘上的分区/挂载点及各自使用率（lsblk 式的父子结构）。

## 需求确认（来自头脑风暴）

- **布局**：物理盘为主体列表，分区/挂载点作为子项折叠展示（B 方案）。
- **Windows 映射**：走完整映射，盘符 → 物理盘关联查询，两端体验一致（A 方案）。
- **主标题**：默认只显示型号；鼠标悬浮 tooltip 显示完整型号 + 设备名。
- **物理盘信息**：卡片布局（非单行），展示容量、健康+温度、寿命/通电、整盘读写、盘类型全部信息。

## 1. 数据模型（后端 `src/types.rs`）

新增结构：

```rust
/// 物理盘类型
pub enum DiskType {
    Hdd,
    Ssd,
    Nvme,
    Unknown,
}

/// 一块物理磁盘及其上的分区
pub struct PhysicalDisk {
    pub device: String,              // "/dev/nvme0n1" / "disk0" — 设备路径，悬浮时显示
    pub model: Option<String>,       // 型号，主标题
    pub disk_type: DiskType,         // HDD / SSD / NVMe / Unknown
    pub total_bytes: u64,            // 整盘物理容量
    pub smart: Option<SmartInfo>,    // 健康/温度/寿命/通电（复用已有结构）
    pub read_bytes_per_sec: f64,     // 整盘读速率
    pub write_bytes_per_sec: f64,    // 整盘写速率
    pub per_device_io: bool,         // I/O 是真实按盘计数还是全机聚合
    pub partitions: Vec<Partition>,  // 该盘上的分区/挂载点
}

/// 物理盘上的一个分区/挂载点
pub struct Partition {
    pub name: String,                // "/dev/nvme0n1p2" / "C:\"
    pub mount_point: String,
    pub fs_type: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub usage_percent: f32,
}
```

`SystemMetrics` 增加 `physical_disks: Vec<PhysicalDisk>` 字段，用 `#[serde(default)]` 保证旧节点数据仍能解析。

**现有的 `disks: Vec<DiskInfo>` 保留不动**——gossip 协议、存储、告警都在用它，动它风险大。这是特意选的低风险路径：新增字段而非替换。

## 2. 分区 → 物理盘的映射与组装（`src/collector.rs`）

新增 `assemble_physical_disks` 函数，输入现有的 `Disks` 列表 + `DiskHealthCollector` 的物理盘缓存 + `diskstats`，输出 `Vec<PhysicalDisk>`。

### Linux

- 物理盘来源 = `DiskHealthCollector` 缓存里的设备（`/dev/sda`、`/dev/nvme0n1`）。
- 每个 `sysinfo` 挂载点的设备名（`/dev/nvme0n1p2`）用已有的 `parent_device()` 归到父盘。
- 整盘容量：读 `/sys/block/<dev>/size`（扇区数 × 512）。
- 整盘 I/O：`diskstats` 里父设备行本身就是整盘计数，直接查 `nvme0n1`，`per_device_io = true`。

### Windows

- 在 `src/disk_health/windows.rs` 的 PowerShell 脚本里补一段关联查询：`Get-Partition` + `Get-Volume` 把盘符（`C:\`）对应到 `DiskNumber`，再对上 `Get-PhysicalDisk.DeviceId`（`disk0`）。返回 `盘符 → disk 号` 的映射。
- 物理盘容量用 `Get-PhysicalDisk.Size`。
- I/O：Windows 没有按盘计数，`per_device_io = false`，整盘读写用全机聚合值（与现有挂载点行行为一致）。

### 兜底

如果某个挂载点归不到任何已知物理盘（网络盘、smartctl 未安装导致缓存为空等），归到一个合成的"其它/未知"盘，保证不丢数据。

### 盘类型推断

`DiskType` 由 `rotation_rate`（Linux/SMART）或 `media_type`（Windows WMI）推断：

- `rotation_rate == 0` → SSD/NVMe
- `rotation_rate > 0` → HDD
- 缺失 → Unknown
- NVMe 设备名（`nvme*`）单独识别为 NVMe

## 3. 前端展示（`web/src/views/NodeDetail.tsx` + `types.ts` + `styles.css`）

`types.ts` 补上 `PhysicalDisk` / `Partition` / `DiskType` 接口，与后端 serde 序列化字段名（snake_case）对齐。

磁盘面板从"挂载点行列表"改成"物理盘卡片列表"：

```
┌─ Samsung SSD 980 PRO ──────── [NVMe] [健康] ─┐   ← 型号主标题；悬浮显示完整型号+设备名
│  1TB · 41°C · 寿命已用 7% · 通电 4210h              │   ← 汇总信息区
│  读 12 MB/s   写 3 MB/s                            │   ← 整盘 I/O（全机聚合时标注"全机"）
│  ┌──────────────────────────────────┐  │
│  │ C:\   [████░] 82%   410G / 500G  NTFS  │  │   ← 分区/挂载点子项，各自使用率条
│  │ D:\   [██░░░░░] 18%    90G / 500G  NTFS │  │
│  └──────────────────────────────────┘  │
└──────────────────────────────────────┘
```

- **卡片标题**：型号 + 盘类型徽章 + 健康徽章；`title` 属性放 `完整型号 (设备路径)` 供悬浮查看。型号缺失时主标题回退到设备名。
- **汇总行**：容量、温度、寿命、通电（缺失的项不显示，不占位）。
- **I/O 行**：沿用现有 `formatRate`，`per_device_io == false` 时保留"全机"标注。
- **分区子区**：每个分区一行，复用现有的使用率进度条 + `usageTone` 配色。
- **健康高亮**：健康异常（Failed / 重分配扇区 / 寿命 ≥ 90%）时卡片高亮，沿用现有告警配色。

## 4. 测试与验证

- **collector.rs**：`assemble_physical_disks` 的单元测试——Linux 分区归父盘、NVMe 分区归并（`nvme0n1p2` → `nvme0n1`）、归不到盘时进"未知盘"兜底、盘类型推断。用构造的输入测，不依赖真实磁盘。
- **windows.rs**：盘符 → disk 号映射解析的测试，喂 PowerShell JSON 样例（沿用现有 `parse_wmi_json` 的测试风格）。
- **验证**：`cargo test` 全绿 + `cargo build --release` 通过；前端 `npm run build` 通过。TS 类型与 Rust 序列化字段名对齐。

## 范围外（YAGNI）

- 不改 gossip 协议、存储、告警对 `disks` 字段的使用。
- 不做 Windows 的按盘 I/O 计数（Windows 无此内核计数，保持全机聚合）。
- 不做历史趋势图等新增可视化，只改详情页当前磁盘面板。
