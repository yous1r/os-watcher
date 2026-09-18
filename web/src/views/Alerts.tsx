import { For, Show } from "solid-js";
import type { Alert, AlertResolveReason, AlertSeverity } from "../types";
import { formatTime, formatUptime } from "../format";

const SEVERITY_LABEL: Record<AlertSeverity, string> = {
  Critical: "严重",
  Warning: "警告",
  Info: "提示",
};

const RESOLVE_REASON_LABEL: Record<AlertResolveReason, string> = {
  condition_cleared: "条件已恢复",
  node_offline: "节点离线",
  node_removed: "节点已移除",
  metric_unavailable: "指标不可用",
};

/** 告警触发至今的持续时间，用于展示「已持续」。 */
function alertAgeSecs(triggeredAt: string): number {
  const startedAt = Date.parse(triggeredAt);
  if (Number.isNaN(startedAt)) return 0;
  return Math.max(0, (Date.now() - startedAt) / 1000);
}

/** 告警视图：当前活动告警 + 最近自动恢复的告警。 */
export function Alerts(props: {
  alerts: Alert[];
  history: Alert[];
  /** [storage] alert_history_minutes：服务端实际执行的保留时长。 */
  historyMinutes: number;
}) {
  const retentionHint = () =>
    props.historyMinutes > 0 ? `仅保留 ${props.historyMinutes} 分钟` : "解除后立即清理";
  return (
    <>
      <Show
        when={props.alerts.length > 0}
        fallback={<div class="empty ok-empty">当前无活动告警 ✓</div>}
      >
        <div class="alerts-list">
          <For each={props.alerts}>
            {(a) => (
              <div
                class="alert-item"
                classList={{
                  "sev-crit": a.severity === "Critical",
                  "sev-warn": a.severity === "Warning",
                  "sev-info": a.severity === "Info",
                }}
              >
                <div class="alert-sev">{SEVERITY_LABEL[a.severity]}</div>
                <div class="alert-main">
                  <div class="alert-rule">
                    <span class="alert-host">{a.hostname}</span>
                    <span class="alert-rule-name">{a.rule_name}</span>
                  </div>
                  <div class="alert-msg">{a.message}</div>
                </div>
                <div class="alert-meta">
                  <div class="alert-value">
                    {a.value.toFixed(1)} / 阈值 {a.threshold.toFixed(1)}
                  </div>
                  <div class="alert-time">
                    {formatTime(a.triggered_at)} 起
                  </div>
                  <div class="alert-duration">
                    已持续 {formatUptime(alertAgeSecs(a.triggered_at))}
                  </div>
                </div>
              </div>
            )}
          </For>
        </div>
      </Show>

      <Show when={props.history.length > 0}>
        <div class="panel history-panel">
          <div class="panel-head">
            <h3>最近恢复 <small>{retentionHint()}</small></h3>
          </div>
          <div class="history-list">
            <For each={props.history}>
              {(a) => (
                <div class="history-item">
                  <span class="history-host">{a.hostname}</span>
                  <span class="history-msg">{a.message}</span>
                  <span class="history-meta">
                    <Show when={a.resolved_reason}>
                      {(reason) => (
                        <span class="history-reason">{RESOLVE_REASON_LABEL[reason()]}</span>
                      )}
                    </Show>
                    <span class="history-time">
                      {a.resolved_at ? formatTime(a.resolved_at) : "--"}
                    </span>
                  </span>
                </div>
              )}
            </For>
          </div>
        </div>
      </Show>
    </>
  );
}
