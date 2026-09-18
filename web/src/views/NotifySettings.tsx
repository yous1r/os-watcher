import { createResource, createSignal, For, Show } from "solid-js";
import {
  ApiError,
  deleteNotifyChannel,
  fetchNotifyChannels,
  testNotifyChannel,
} from "../api";
import { authStore } from "../authStore";
import { formatTime } from "../format";
import type { NotifyChannel, NotifySeverity } from "../types";
import { ChannelDialog } from "./ChannelDialog";

const SEVERITY_LABEL: Record<NotifySeverity, string> = {
  info: "提示及以上",
  warning: "警告及以上",
  critical: "仅严重",
};

/** 判断某个接口错误是否属于「会话失效」，需要重新登录。 */
function isUnauthorized(err: unknown): boolean {
  return err instanceof ApiError && err.status === 401;
}

/** 推送设置视图：管理 Bark 推送渠道，仅管理员可操作。 */
export function NotifySettings() {
  const [reload, setReload] = createSignal(0);
  const [editing, setEditing] = createSignal<NotifyChannel | null>(null);
  const [dialogOpen, setDialogOpen] = createSignal(false);
  const [testResults, setTestResults] = createSignal<
    Record<string, { ok: boolean; message: string }>
  >({});
  const [busyId, setBusyId] = createSignal<string | null>(null);

  const canManage = authStore.canManage;

  const [channels] = createResource<NotifyChannel[], number>(
    () => (canManage() ? reload() : null),
    async () => {
      try {
        return await fetchNotifyChannels();
      } catch (err) {
        if (isUnauthorized(err)) authStore.handleUnauthorized();
        return [];
      }
    },
    { initialValue: [] }
  );

  const openCreate = () => {
    if (!authStore.allowed()) return;
    setEditing(null);
    setDialogOpen(true);
  };

  const openEdit = (channel: NotifyChannel) => {
    if (!authStore.allowed()) return;
    setEditing(channel);
    setDialogOpen(true);
  };

  const closeDialog = () => {
    setDialogOpen(false);
    setEditing(null);
  };

  const reloadChannels = () => setReload((value) => value + 1);

  const setTestResult = (id: string, ok: boolean, message: string) => {
    setTestResults((prev) => ({ ...prev, [id]: { ok, message } }));
  };

  const runTest = async (channel: NotifyChannel) => {
    if (busyId()) return;
    setBusyId(channel.id);
    setTestResults((prev) => {
      const next = { ...prev };
      delete next[channel.id];
      return next;
    });
    try {
      await testNotifyChannel(channel.id);
      setTestResult(channel.id, true, "测试推送已发送");
      reloadChannels();
    } catch (err) {
      if (isUnauthorized(err)) {
        authStore.handleUnauthorized();
      } else {
        setTestResult(channel.id, false, err instanceof Error ? err.message : "推送失败");
      }
    } finally {
      setBusyId(null);
    }
  };

  const remove = async (channel: NotifyChannel) => {
    if (!authStore.allowed()) return;
    if (!window.confirm(`确认删除推送渠道「${channel.name}」？`)) return;

    setBusyId(channel.id);
    try {
      await deleteNotifyChannel(channel.id);
      reloadChannels();
    } catch (err) {
      if (isUnauthorized(err)) {
        authStore.handleUnauthorized();
      } else {
        setTestResult(channel.id, false, err instanceof Error ? err.message : "删除失败");
      }
    } finally {
      setBusyId(null);
    }
  };

  const encryptionSummary = (channel: NotifyChannel) => {
    const encryption = channel.config.encryption;
    if (!encryption) return "明文推送";
    return `${encryption.algorithm.toUpperCase()} / ${encryption.mode.toUpperCase()} 加密`;
  };

  return (
    <div class="notify-settings">
      <Show when={!canManage()}>
        <div class="empty">
          <p>管理功能需要登录</p>
          <button type="button" class="btn-primary" onClick={authStore.openLogin}>
            管理员登录
          </button>
        </div>
      </Show>

      <Show when={canManage()}>
        <Show when={!authStore.authRequired()}>
          <p class="notice-warn">
            管理鉴权未启用（config.toml 中 [auth] enabled =
            false），任何人都能修改推送渠道。
          </p>
        </Show>

        <div class="panel">
          <div class="panel-head">
            <h3>推送渠道</h3>
            <button type="button" class="btn-primary" onClick={openCreate}>
              + 新增渠道
            </button>
          </div>

          <p class="notify-hint">
            告警触发与恢复时推送到这些渠道。渠道配置在聚合节点上即可，无需每个节点都配。
          </p>

          <Show
            when={channels().length > 0}
            fallback={<div class="empty">尚未配置推送渠道</div>}
          >
            <div class="notify-list">
              <For each={channels()}>
                {(channel) => (
                  <div class="notify-item" classList={{ "channel-disabled": !channel.enabled }}>
                    <div class="channel-head">
                      <span class="channel-kind">Bark</span>
                      <span class="channel-name">{channel.name}</span>
                      <Show when={!channel.enabled}>
                        <span class="channel-tag">已停用</span>
                      </Show>
                      <span class="channel-sev">{SEVERITY_LABEL[channel.min_severity]}</span>
                    </div>

                    <div class="channel-detail">
                      <span>{channel.config.server_url}</span>
                      <span class="sep">|</span>
                      <span>{encryptionSummary(channel)}</span>
                      <Show when={channel.last_sent_at}>
                        {(sentAt) => (
                          <>
                            <span class="sep">|</span>
                            <span>上次成功 {formatTime(sentAt())}</span>
                          </>
                        )}
                      </Show>
                    </div>

                    <Show when={channel.last_error}>
                      {(failure) => (
                        <div class="channel-status err">最近失败：{failure()}</div>
                      )}
                    </Show>

                    <div class="channel-actions">
                      <button
                        type="button"
                        class="btn-secondary"
                        disabled={busyId() === channel.id}
                        onClick={() => void runTest(channel)}
                      >
                        {busyId() === channel.id ? "处理中…" : "测试推送"}
                      </button>
                      <button
                        type="button"
                        class="btn-secondary"
                        disabled={busyId() === channel.id}
                        onClick={() => openEdit(channel)}
                      >
                        编辑
                      </button>
                      <button
                        type="button"
                        class="btn-secondary"
                        disabled={busyId() === channel.id}
                        onClick={() => void remove(channel)}
                      >
                        删除
                      </button>
                    </div>

                    <Show when={testResults()[channel.id]}>
                      {(result) => (
                        <div
                          class="channel-test-result"
                          classList={{ ok: result().ok, err: !result().ok }}
                        >
                          {result().message}
                        </div>
                      )}
                    </Show>
                  </div>
                )}
              </For>
            </div>
          </Show>
        </div>
      </Show>

      <Show when={dialogOpen()}>
        <ChannelDialog
          channel={editing()}
          onClose={closeDialog}
          onSaved={reloadChannels}
        />
      </Show>
    </div>
  );
}
