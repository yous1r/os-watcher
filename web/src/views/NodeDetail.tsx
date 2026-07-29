import { createSignal, createMemo, For, Show, createEffect } from "solid-js";
import { Select, createListCollection } from "@ark-ui/solid/select";
import type { NodeSnapshot, ProcessInfo, PhysicalDisk, DiskType } from "../types";
import { formatBytes, formatRate, formatUptime, usageTone } from "../format";

/** 进程表格的排序维度。 */
type SortKey = "cpu" | "memory" | "disk_read" | "disk_write";

const SORT_OPTIONS: { key: SortKey; label: string }[] = [
  { key: "cpu", label: "CPU" },
  { key: "memory", label: "内存" },
  { key: "disk_read", label: "磁盘读取" },
  { key: "disk_write", label: "磁盘写入" },
];

/** 网卡表格的排序维度。 */
type NetSortKey = "rx" | "tx" | "name";

const NET_SORT_OPTIONS: { key: NetSortKey; label: string }[] = [
  { key: "rx", label: "下行" },
  { key: "tx", label: "上行" },
  { key: "name", label: "名称" },
];

/** 按选定维度取出进程的排序值。 */
function sortValue(p: ProcessInfo, key: SortKey): number {
  switch (key) {
    case "cpu":
      return p.cpu_usage;
    case "memory":
      return p.memory_bytes;
    case "disk_read":
      return p.disk_read_bps;
    case "disk_write":
      return p.disk_write_bps;
  }
}

/** 盘类型徽章文案。 */
function diskTypeLabel(t: DiskType): string {
  switch (t) {
    case "Hdd": return "HDD";
    case "Ssd": return "SSD";
    case "Nvme": return "NVMe";
    default: return "未知";
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

/** 节点详情视图：选择单个节点，展示 CPU/内存/磁盘/网络/进程详情。 */
export function NodeDetail(props: { snapshots: NodeSnapshot[] }) {
  const [selectedId, setSelectedId] = createSignal<string | null>(null);
  const [sortKey, setSortKey] = createSignal<SortKey>("cpu");
  const [netSortKey, setNetSortKey] = createSignal<NetSortKey>("rx");

  // 默认选中第一个节点。
  createEffect(() => {
    if (selectedId() === null && props.snapshots.length > 0) {
      setSelectedId(props.snapshots[0].info.id);
    }
  });

  const collection = createMemo(() =>
    createListCollection({
      items: props.snapshots.map((s) => ({
        label: `${s.info.hostname} (${s.info.api_addr})`,
        value: s.info.id,
      })),
    })
  );

  const current = createMemo(() =>
    props.snapshots.find((s) => s.info.id === selectedId())
  );

  // 后端已按 CPU 取过 top N，这里只对这批结果重新排序，不改变集合本身。
  const sortedProcesses = createMemo(() => {
    const procs = current()?.metrics?.top_processes ?? [];
    const key = sortKey();
    return [...procs].sort((a, b) => sortValue(b, key) - sortValue(a, key));
  });

  const sortedNetworks = createMemo(() => {
    const nets = current()?.metrics?.networks ?? [];
    const key = netSortKey();
    // 名称是字符串升序，速率是数值降序，两者比较方式不同。
    if (key === "name") {
      return [...nets].sort((a, b) => a.name.localeCompare(b.name));
    }
    const field = key === "rx" ? "rx_bytes_per_sec" : "tx_bytes_per_sec";
    return [...nets].sort((a, b) => b[field] - a[field]);
  });

  return (
    <Show
      when={props.snapshots.length > 0}
      fallback={<div class="empty">暂无节点数据</div>}
    >
      <div class="detail-header">
        <Select.Root
          collection={collection()}
          value={selectedId() ? [selectedId()!] : []}
          onValueChange={(e) => setSelectedId(e.value[0] ?? null)}
          class="node-select"
        >
          <Select.Label>选择节点</Select.Label>
          <Select.Control>
            <Select.Trigger class="select-trigger">
              <Select.ValueText placeholder="选择一个节点" />
              <Select.Indicator>▾</Select.Indicator>
            </Select.Trigger>
          </Select.Control>
          <Select.Positioner>
            <Select.Content class="select-content">
              <For each={collection().items}>
                {(item) => (
                  <Select.Item item={item} class="select-item">
                    <Select.ItemText>{item.label}</Select.ItemText>
                    <Select.ItemIndicator>✓</Select.ItemIndicator>
                  </Select.Item>
                )}
              </For>
            </Select.Content>
          </Select.Positioner>
        </Select.Root>
      </div>

      <Show
        when={current()?.metrics}
        fallback={<div class="empty">该节点暂无指标数据</div>}
      >
        {(metrics) => (
          <div class="detail-body">
            <div class="detail-gauges">
              <Gauge
                label={`CPU (${metrics().cpu.core_count} 核)`}
                pct={metrics().cpu.usage_percent}
              />
              <Gauge
                label="内存"
                pct={metrics().memory.usage_percent}
                sub={`${formatBytes(metrics().memory.used_bytes)} / ${formatBytes(metrics().memory.total_bytes)}`}
              />
              <Show when={metrics().memory.swap_total_bytes > 0}>
                <Gauge
                  label="Swap"
                  pct={
                    (metrics().memory.swap_used_bytes /
                      metrics().memory.swap_total_bytes) *
                    100
                  }
                  sub={`${formatBytes(metrics().memory.swap_used_bytes)} / ${formatBytes(metrics().memory.swap_total_bytes)}`}
                />
              </Show>
            </div>

            <div class="detail-cols">
              <div class="panel">
                <h3>磁盘</h3>
                <For each={metrics().physical_disks}>
                  {(disk) => (
                    <div
                      class="disk-card"
                      classList={{ crit: disk.smart?.health === "Failed" }}
                    >
                      <div class="disk-card-head" title={diskTooltip(disk)}>
                        <span class="disk-model">{diskTitle(disk)}</span>
                        <span class="disk-badges">
                          <span class="disk-type-badge">{diskTypeLabel(disk.disk_type)}</span>
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
                                {s().health === "Passed" ? "健康" : s().health === "Failed" ? "异常" : "未知"}
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
                          <span class="crit">重分配扇区 {disk.smart!.reallocated_sectors}</span>
                        </Show>
                      </div>

                      <div class="disk-card-io">
                        <span>读 {formatRate(disk.read_bytes_per_sec)}</span>
                        <span>写 {formatRate(disk.write_bytes_per_sec)}</span>
                        <Show when={!disk.per_device_io}>
                          <span class="io-note" title="内核未提供按设备计数，此处为全机聚合值">全机</span>
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
                                    style={{ width: `${Math.min(p.usage_percent, 100)}%` }}
                                  />
                                </div>
                                <div class="part-detail">
                                  {formatBytes(p.used_bytes)} / {formatBytes(p.total_bytes)} ({p.usage_percent.toFixed(0)}%)
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

              <div class="panel">
                <div class="panel-head">
                  <h3>网卡</h3>
                  <div class="sort-tabs">
                    <For each={NET_SORT_OPTIONS}>
                      {(opt) => (
                        <button
                          type="button"
                          class="sort-tab"
                          classList={{ active: netSortKey() === opt.key }}
                          onClick={() => setNetSortKey(opt.key)}
                        >
                          {opt.label}
                        </button>
                      )}
                    </For>
                  </div>
                </div>
                <table class="proc-table">
                  <thead>
                    <tr>
                      <th>接口</th>
                      <th class="num">下行</th>
                      <th class="num">上行</th>
                      <th class="num">累计收/发</th>
                    </tr>
                  </thead>
                  <tbody>
                    <For each={sortedNetworks()}>
                      {(n) => (
                        <tr>
                          <td class="proc-name">{n.name}</td>
                          <td class="num rx">↓ {formatRate(n.rx_bytes_per_sec)}</td>
                          <td class="num tx">↑ {formatRate(n.tx_bytes_per_sec)}</td>
                          <td class="num dim">
                            {formatBytes(n.total_received_bytes)} /{" "}
                            {formatBytes(n.total_transmitted_bytes)}
                          </td>
                        </tr>
                      )}
                    </For>
                  </tbody>
                </table>
              </div>

              <div class="panel">
                <div class="panel-head">
                  <h3>Top 进程</h3>
                  {/* 排序维度切换：点击表头也可切换 */}
                  <div class="sort-tabs">
                    <For each={SORT_OPTIONS}>
                      {(opt) => (
                        <button
                          type="button"
                          class="sort-tab"
                          classList={{ active: sortKey() === opt.key }}
                          onClick={() => setSortKey(opt.key)}
                        >
                          {opt.label}
                        </button>
                      )}
                    </For>
                  </div>
                </div>
                <table class="proc-table">
                  <thead>
                    <tr>
                      <th>PID</th>
                      <th>名称</th>
                      <SortableHeader
                        label="CPU"
                        col="cpu"
                        active={sortKey()}
                        onSort={setSortKey}
                      />
                      <SortableHeader
                        label="内存"
                        col="memory"
                        active={sortKey()}
                        onSort={setSortKey}
                      />
                      <SortableHeader
                        label="读取"
                        col="disk_read"
                        active={sortKey()}
                        onSort={setSortKey}
                      />
                      <SortableHeader
                        label="写入"
                        col="disk_write"
                        active={sortKey()}
                        onSort={setSortKey}
                      />
                    </tr>
                  </thead>
                  <tbody>
                    <For each={sortedProcesses()}>
                      {(p) => (
                        <tr>
                          <td>{p.pid}</td>
                          <td class="proc-name">{p.name}</td>
                          <td classList={{ [usageTone(p.cpu_usage)]: true }}>
                            {p.cpu_usage.toFixed(1)}%
                          </td>
                          <td>{formatBytes(p.memory_bytes)}</td>
                          <td class="num">{formatRate(p.disk_read_bps)}</td>
                          <td class="num">{formatRate(p.disk_write_bps)}</td>
                        </tr>
                      )}
                    </For>
                  </tbody>
                </table>
              </div>
            </div>

            <div class="detail-foot">
              运行时间 {formatUptime(metrics().uptime_seconds)} · {metrics().os_name} · {metrics().hostname}
            </div>
          </div>
        )}
      </Show>
    </Show>
  );
}

/** 大号仪表：标签 + 进度条 + 百分比。 */
function Gauge(props: { label: string; pct: number; sub?: string }) {
  const tone = () => usageTone(props.pct);
  return (
    <div class="gauge">
      <div class="gauge-head">
        <span class="gauge-label">{props.label}</span>
        <span class="gauge-pct" classList={{ [tone()]: true }}>
          {props.pct.toFixed(1)}%
        </span>
      </div>
      <div class="bar bar-lg">
        <div
          class="bar-fill"
          classList={{ [tone()]: true }}
          style={{ width: `${Math.min(props.pct, 100)}%` }}
        />
      </div>
      <Show when={props.sub}>
        <div class="gauge-sub">{props.sub}</div>
      </Show>
    </div>
  );
}

/** 可点击排序的表头单元格，当前排序列显示降序箭头。 */
function SortableHeader(props: {
  label: string;
  col: SortKey;
  active: SortKey;
  onSort: (key: SortKey) => void;
}) {
  const isActive = () => props.active === props.col;
  return (
    <th
      class="sortable"
      classList={{ active: isActive() }}
      onClick={() => props.onSort(props.col)}
      title={`按${props.label}排序`}
    >
      {props.label}
      <Show when={isActive()}>
        <span class="sort-arrow">↓</span>
      </Show>
    </th>
  );
}
