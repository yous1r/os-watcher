import { createSignal, For, onCleanup, Show } from "solid-js";
import { fetchNodeUpgradeStatus, triggerNodeUpgrade } from "../api";
import type { NodeSnapshot, PackageKind, UpgradeStatus, VersionInfo } from "../types";
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

function normalizeVersion(version: string): number[] | null {
  const core = version.trim().replace(/^[vV]/, "").split(/[-+]/)[0];
  const parts = core.split(".").slice(0, 3);
  if (parts.length === 0 || parts.some((part) => !/^\d+$/.test(part))) {
    return null;
  }
  return [0, 1, 2].map((idx) => Number(parts[idx] ?? 0));
}

function isNewerVersion(latest: string | null, current: string): boolean {
  if (!latest) return false;
  const latestParts = normalizeVersion(latest);
  const currentParts = normalizeVersion(current);
  if (!latestParts || !currentParts) {
    return latest.replace(/^[vV]/, "") !== current.replace(/^[vV]/, "");
  }
  for (let idx = 0; idx < latestParts.length; idx += 1) {
    if (latestParts[idx] > currentParts[idx]) return true;
    if (latestParts[idx] < currentParts[idx]) return false;
  }
  return false;
}

/** 概览视图：所有节点的卡片网格。 */
export function Overview(props: {
  snapshots: NodeSnapshot[];
  versionInfo: VersionInfo | null;
  onUpgradeRequested?: () => void;
}) {
  const [target, setTarget] = createSignal<NodeSnapshot | null>(null);
  const [selectedPackage, setSelectedPackage] = createSignal<PackageKind>("node");
  const [upgradingId, setUpgradingId] = createSignal<string | null>(null);
  const [lockedUpgradeIds, setLockedUpgradeIds] = createSignal<Set<string>>(new Set());
  const [resultMessage, setResultMessage] = createSignal<string | null>(null);
  const pollTimers: number[] = [];

  const latestVersion = () => props.versionInfo?.latest ?? null;
  const updateAvailable = (snap: NodeSnapshot) =>
    isNewerVersion(latestVersion(), snap.info.version);
  const isUpgradeLocked = (id: string) =>
    upgradingId() === id || lockedUpgradeIds().has(id);

  onCleanup(() => pollTimers.forEach((timer) => window.clearTimeout(timer)));

  const openUpgradeDialog = (snap: NodeSnapshot) => {
    setTarget(snap);
    setSelectedPackage(props.versionInfo?.package ?? "node");
    setResultMessage(null);
  };

  const closeUpgradeDialog = () => {
    if (upgradingId()) return;
    setTarget(null);
    setResultMessage(null);
  };

  const updateLock = (id: string, locked: boolean) => {
    setLockedUpgradeIds((current) => {
      const next = new Set(current);
      if (locked) {
        next.add(id);
      } else {
        next.delete(id);
      }
      return next;
    });
  };

  const showResultForTarget = (id: string, message: string) => {
    if (target()?.info.id === id) {
      setResultMessage(message);
    }
  };

  const phaseText = (status: UpgradeStatus) => {
    if (status.phase === "succeeded") return "升级完成";
    if (status.phase === "rolled_back") return "升级失败，已回滚";
    if (status.phase === "failed") return "升级失败";
    return status.message;
  };

  const pollUpgradeStatus = (snap: NodeSnapshot, attempt = 0) => {
    const timer = window.setTimeout(async () => {
      try {
        const status = await fetchNodeUpgradeStatus(snap.info.api_addr);
        showResultForTarget(snap.info.id, phaseText(status));
        if (status.running && attempt < 60) {
          pollUpgradeStatus(snap, attempt + 1);
          return;
        }
        updateLock(snap.info.id, false);
        props.onUpgradeRequested?.();
      } catch (err) {
        if (attempt < 60) {
          pollUpgradeStatus(snap, attempt + 1);
          return;
        }
        updateLock(snap.info.id, false);
        showResultForTarget(
          snap.info.id,
          err instanceof Error ? err.message : "升级状态确认超时"
        );
      }
    }, 2500);
    pollTimers.push(timer);
  };

  const confirmUpgrade = async () => {
    const snap = target();
    if (!snap || isUpgradeLocked(snap.info.id)) return;

    setUpgradingId(snap.info.id);
    setResultMessage(null);
    try {
      const status = await triggerNodeUpgrade(snap.info.api_addr, {
        package: selectedPackage(),
      });
      setResultMessage(`升级请求已提交：${status.message}`);
      if (status.running) {
        updateLock(snap.info.id, true);
        pollUpgradeStatus(snap);
      }
      props.onUpgradeRequested?.();
    } catch (err) {
      setResultMessage(err instanceof Error ? err.message : "升级请求失败");
    } finally {
      setUpgradingId(null);
    }
  };

  return (
    <>
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
                  <div class="node-version">
                    <span>版本 {snap.info.version}</span>
                    <Show when={updateAvailable(snap)}>
                      <button
                        type="button"
                        class="update-dot"
                        title={`发现新版本 ${latestVersion()}`}
                        aria-label={`升级 ${snap.info.hostname}`}
                        disabled={!online || isUpgradeLocked(snap.info.id)}
                        onClick={() => openUpgradeDialog(snap)}
                      />
                    </Show>
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

      <Show when={target()}>
        {(snap) => (
          <div class="modal-backdrop" onClick={closeUpgradeDialog}>
            <div class="upgrade-dialog" onClick={(event) => event.stopPropagation()}>
              <div class="upgrade-dialog-head">
                <h2>确认升级</h2>
                <button
                  type="button"
                  class="dialog-close"
                  aria-label="关闭"
                  disabled={upgradingId() !== null}
                  onClick={closeUpgradeDialog}
                >
                  ×
                </button>
              </div>
              <div class="upgrade-meta">
                <div>
                  <span>节点</span>
                  <strong>{snap().info.hostname}</strong>
                </div>
                <div>
                  <span>当前版本</span>
                  <strong>{snap().info.version}</strong>
                </div>
                <div>
                  <span>最新版本</span>
                  <strong>{latestVersion() ?? "--"}</strong>
                </div>
              </div>
              <div class="package-toggle" role="group" aria-label="升级包类型">
                <button
                  type="button"
                  classList={{ active: selectedPackage() === "node" }}
                  onClick={() => setSelectedPackage("node")}
                  disabled={upgradingId() !== null}
                >
                  Node
                </button>
                <button
                  type="button"
                  classList={{ active: selectedPackage() === "full" }}
                  onClick={() => setSelectedPackage("full")}
                  disabled={upgradingId() !== null}
                >
                  Full
                </button>
              </div>
              <Show when={resultMessage()}>
                {(message) => <div class="upgrade-result">{message()}</div>}
              </Show>
              <div class="dialog-actions">
                <button
                  type="button"
                  class="btn-secondary"
                  disabled={upgradingId() !== null}
                  onClick={closeUpgradeDialog}
                >
                  取消
                </button>
                <button
                  type="button"
                  class="btn-primary"
                  disabled={
                    upgradingId() !== null || isUpgradeLocked(snap().info.id)
                  }
                  onClick={confirmUpgrade}
                >
                  {isUpgradeLocked(snap().info.id) ? "升级中…" : "确认升级"}
                </button>
              </div>
            </div>
          </div>
        )}
      </Show>
    </>
  );
}
