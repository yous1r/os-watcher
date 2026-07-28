import { For, Show } from "solid-js";
import type { Alert, AlertSeverity } from "../types";
import { formatTime } from "../format";

const SEVERITY_LABEL: Record<AlertSeverity, string> = {
  Critical: "严重",
  Warning: "警告",
  Info: "提示",
};

/** 告警视图：列出当前所有活动告警。 */
export function Alerts(props: { alerts: Alert[] }) {
  return (
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
                <div class="alert-rule">{a.rule_name}</div>
                <div class="alert-msg">{a.message}</div>
              </div>
              <div class="alert-meta">
                <div class="alert-value">
                  {a.value.toFixed(1)} / 阈值 {a.threshold.toFixed(1)}
                </div>
                <div class="alert-time">{formatTime(a.triggered_at)}</div>
              </div>
            </div>
          )}
        </For>
      </div>
    </Show>
  );
}
