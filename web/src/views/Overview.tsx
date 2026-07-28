import { For, Show } from "solid-js";
import type { NodeSnapshot } from "../types";
import { formatUptime, usageTone, maxDiskUsage } from "../format";

/** 单个指标的进度条。 */
function MetricBar(props: { label: string; pct: number; detail?: string }) {
  const tone = () => usageTone(props.pct);
  return (
    <div class="metric">
      <div class="metric-head">
        <span class="metric-label">{props.label}</span>
        <span class="metric-value" classList={{ [tone()]: true }}>
          {props.pct.toFixed(1)}%
        </span>
      </div>
      <div class="bar">
        <div
          class="bar-fill"
          classList={{ [tone()]: true }}
          style={{ width: `${Math.min(props.pct, 100)}%` }}
        />
      </div>
      <Show when={props.detail}>
        <div class="metric-detail">{props.detail}</div>
      </Show>
    </div>
  );
}

/** 概览视图：所有节点的卡片网格。 */
export function Overview(props: { snapshots: NodeSnapshot[] }) {
  return (
    <Show
      when={props.snapshots.length > 0}
      fallback={<div class="empty">暂无节点数据，等待采集…</div>}
    >
      <div class="node-grid">
        <For each={props.snapshots}>
          {(snap) => {
            const m = snap.metrics;
            const online = snap.info.status === "Online";
            return (
              <div class="node-card" classList={{ offline: !online }}>
                <div class="node-card-head">
                  <span
                    class="dot"
                    classList={{
                      "dot-online": snap.info.status === "Online",
                      "dot-offline": snap.info.status === "Offline",
                      "dot-unknown": snap.info.status === "Unknown",
                    }}
                  />
                  <span class="node-name">{snap.info.hostname}</span>
                  <span class="node-addr">{snap.info.api_addr}</span>
                </div>
                <Show
                  when={m}
                  fallback={<div class="node-nodata">无指标数据</div>}
                >
                  {(metrics) => (
                    <div class="node-metrics">
                      <MetricBar
                        label="CPU"
                        pct={metrics().cpu.usage_percent}
                        detail={`${metrics().cpu.core_count} 核`}
                      />
                      <MetricBar
                        label="内存"
                        pct={metrics().memory.usage_percent}
                      />
                      <MetricBar
                        label="磁盘"
                        pct={maxDiskUsage(metrics().disks)}
                      />
                      <div class="node-foot">
                        运行 {formatUptime(metrics().uptime_seconds)} · {metrics().os_name}
                      </div>
                    </div>
                  )}
                </Show>
              </div>
            );
          }}
        </For>
      </div>
    </Show>
  );
}
