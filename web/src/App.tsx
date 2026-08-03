import { createSignal, createResource, onCleanup, Show } from "solid-js";
import { Tabs } from "@ark-ui/solid/tabs";
import { fetchSnapshots, fetchAlerts, fetchVersion, API_BASE } from "./api";
import type { NodeSnapshot, Alert, VersionInfo } from "./types";
import { formatTime } from "./format";
import { Overview } from "./views/Overview";
import { NodeDetail } from "./views/NodeDetail";
import { Alerts } from "./views/Alerts";
import { AddNodeDialog } from "./views/AddNodeDialog";
import { deployStore } from "./deployStore";

const REFRESH_MS = 3000;

export default function App() {
  const [connected, setConnected] = createSignal<boolean | null>(null);
  const [lastUpdate, setLastUpdate] = createSignal("--:--:--");
  const [addNodeOpen, setAddNodeOpen] = createSignal(false);

  // 轮询触发器：每个刷新周期递增，驱动 createResource 重新拉取。
  const [tick, setTick] = createSignal(0);

  const [snapshots] = createResource<NodeSnapshot[], number>(
    tick,
    async () => {
      try {
        const data = await fetchSnapshots();
        setConnected(true);
        setLastUpdate(formatTime(new Date().toISOString()));
        return data;
      } catch {
        setConnected(false);
        return [];
      }
    },
    { initialValue: [] }
  );

  const [alerts] = createResource<Alert[], number>(
    tick,
    async () => {
      try {
        return await fetchAlerts();
      } catch {
        return [];
      }
    },
    { initialValue: [] }
  );

  const [versionInfo, { refetch: refetchVersion }] = createResource<
    VersionInfo | null,
    number
  >(
    tick,
    async () => {
      try {
        return await fetchVersion();
      } catch {
        return null;
      }
    },
    { initialValue: null }
  );

  const timer = setInterval(() => setTick((t) => t + 1), REFRESH_MS);
  onCleanup(() => clearInterval(timer));

  const nodeCount = () => snapshots().length;
  const alertCount = () => alerts().length;

  return (
    <div class="app">
      <header class="topbar">
        <div class="brand">
          <span class="logo">◉</span>
          <span class="title">os-watcher</span>
          <span class="subtitle">去中心化主机监控</span>
        </div>
        <div class="status-line">
          <span
            class="dot"
            classList={{
              "dot-online": connected() === true,
              "dot-offline": connected() === false,
              "dot-unknown": connected() === null,
            }}
          />
          <span>
            {connected() === true
              ? "已连接"
              : connected() === false
                ? "连接失败"
                : "连接中…"}
          </span>
          <span class="sep">|</span>
          <span>{nodeCount()} 个节点</span>
          <span class="sep">|</span>
          <span class="alert-badge" classList={{ active: alertCount() > 0 }}>
            {alertCount()} 告警
          </span>
          <span class="sep">|</span>
          <span>{lastUpdate()}</span>
          <button
            type="button"
            class="add-node-btn"
            classList={{ deploying: deployStore.isRunning() }}
            title={
              deployStore.isRunning()
                ? "部署进行中，点击查看进度"
                : "添加节点"
            }
            onClick={() => setAddNodeOpen(true)}
          >
            {deployStore.isRunning()
              ? "部署中…"
              : deployStore.isActive()
                ? "部署结果"
                : "+ 添加节点"}
          </button>
        </div>
      </header>

      <Tabs.Root defaultValue="overview" class="tabs">
        <Tabs.List class="tab-list">
          <Tabs.Trigger value="overview" class="tab">
            概览
          </Tabs.Trigger>
          <Tabs.Trigger value="detail" class="tab">
            节点详情
          </Tabs.Trigger>
          <Tabs.Trigger value="alerts" class="tab">
            告警
            <Show when={alertCount() > 0}>
              <span class="tab-badge">{alertCount()}</span>
            </Show>
          </Tabs.Trigger>
          <Tabs.Indicator class="tab-indicator" />
        </Tabs.List>

        <Tabs.Content value="overview" class="tab-content">
          <Overview
            snapshots={snapshots()}
            versionInfo={versionInfo()}
            onUpgradeRequested={refetchVersion}
          />
        </Tabs.Content>
        <Tabs.Content value="detail" class="tab-content">
          <NodeDetail snapshots={snapshots()} />
        </Tabs.Content>
        <Tabs.Content value="alerts" class="tab-content">
          <Alerts alerts={alerts()} />
        </Tabs.Content>
      </Tabs.Root>

      <footer class="footer">
        <span>刷新间隔：{REFRESH_MS / 1000}s</span>
        <span class="sep">|</span>
        <span>数据源：{API_BASE}</span>
      </footer>

      <Show when={addNodeOpen()}>
        <AddNodeDialog
          onClose={() => setAddNodeOpen(false)}
          onDeployed={() => setTick((t) => t + 1)}
        />
      </Show>
    </div>
  );
}
