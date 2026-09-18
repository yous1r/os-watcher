import { createMemo, createSignal, onCleanup, onMount, Show } from "solid-js";
import {
  ApiError,
  createNotifyChannel,
  fetchNotifyDefaults,
  updateNotifyChannel,
} from "../api";
import { authStore } from "../authStore";
import type {
  BarkAlgorithm,
  BarkEncryption,
  BarkMode,
  NotifyChannel,
  NotifyChannelPayload,
  NotifySeverity,
} from "../types";

const DIALOG_TITLE_ID = "channel-dialog-title";
const ERROR_ID = "channel-dialog-error";

const KEY_LEN: Record<BarkAlgorithm, number> = {
  aes128: 16,
  aes192: 24,
  aes256: 32,
};

const IV_LEN: Record<BarkMode, number | null> = { cbc: 16, ecb: null, gcm: 12 };

const ALGORITHM_LABEL: Record<BarkAlgorithm, string> = {
  aes128: "AES128",
  aes192: "AES192",
  aes256: "AES256",
};

const MODE_LABEL: Record<BarkMode, string> = {
  cbc: "CBC",
  ecb: "ECB",
  gcm: "GCM",
};

const SEVERITY_LABEL: Record<NotifySeverity, string> = {
  info: "提示及以上",
  warning: "警告及以上",
  critical: "仅严重",
};

const DEFAULT_SERVER_URL = "https://api.day.app";

/**
 * 客户端校验与服务端 `notify::validate_bark_config` 保持同一套错误文案，
 * 这样用户在表单里看到的原因与接口返回的一致。
 * 服务地址留空表示使用服务端配置的默认地址，此处不做格式校验。
 */
function validate(
  name: string,
  serverUrl: string,
  deviceKey: string,
  encryption: BarkEncryption | null
): string | null {
  if (!name) return "渠道名称不能为空";
  if (serverUrl && !/^https?:\/\//i.test(serverUrl)) {
    return "服务地址必须以 http:// 或 https:// 开头";
  }
  if (!deviceKey) return "设备 Key 不能为空";
  if (!encryption) return null;

  const expectedKeyLen = KEY_LEN[encryption.algorithm];
  if (encryption.key.length !== expectedKeyLen) {
    return `${ALGORITHM_LABEL[encryption.algorithm]} 密钥长度必须是 ${expectedKeyLen} 个字符`;
  }
  // iOS 端（CommonCrypto）只实现了 AES128/AES256 的 GCM，AES192+GCM 无法解密。
  if (encryption.mode === "gcm" && encryption.algorithm === "aes192") {
    return "GCM 模式仅支持 AES128 与 AES256 密钥";
  }

  const iv = encryption.iv ?? "";
  const expectedIvLen = IV_LEN[encryption.mode];
  if (expectedIvLen === null) {
    if (iv) return "ECB 模式不需要 IV";
    return null;
  }
  if (iv.length !== expectedIvLen) {
    return `${MODE_LABEL[encryption.mode]} 模式必须提供 ${expectedIvLen} 个字符的 IV`;
  }
  return null;
}

/** 推送渠道编辑对话框：新建与修改共用一份表单。 */
export function ChannelDialog(props: {
  channel: NotifyChannel | null;
  onClose: () => void;
  onSaved: () => void;
}) {
  const existing = props.channel;
  const [name, setName] = createSignal(existing?.name ?? "");
  // 新建渠道留空：服务端会填入 [notify] server_url 配置的地址（自建 Bark 服务器改一处即可）。
  const [serverUrl, setServerUrl] = createSignal(existing?.config.server_url ?? "");
  const [defaultServerUrl, setDefaultServerUrl] = createSignal(DEFAULT_SERVER_URL);
  const [deviceKey, setDeviceKey] = createSignal(existing?.config.device_key ?? "");
  const [enabled, setEnabled] = createSignal(existing?.enabled ?? true);
  const [minSeverity, setMinSeverity] = createSignal<NotifySeverity>(
    existing?.min_severity ?? "warning"
  );
  const [encryptEnabled, setEncryptEnabled] = createSignal(
    existing ? existing.config.encryption != null : true
  );
  const [algorithm, setAlgorithm] = createSignal<BarkAlgorithm>(
    existing?.config.encryption?.algorithm ?? "aes256"
  );
  const [mode, setMode] = createSignal<BarkMode>(
    existing?.config.encryption?.mode ?? "gcm"
  );
  const [key, setKey] = createSignal(existing?.config.encryption?.key ?? "");
  const [iv, setIv] = createSignal(existing?.config.encryption?.iv ?? "");
  const [error, setError] = createSignal<string | null>(null);
  const [saving, setSaving] = createSignal(false);
  let dialogEl: HTMLDivElement | undefined;
  let previousFocus: HTMLElement | null = null;

  const ivDisabled = createMemo(() => mode() === "ecb");
  const ivHint = createMemo(() => {
    const expected = IV_LEN[mode()];
    if (expected === null) return "ECB 模式无需 IV";
    return `${expected} 个字符（Bark App 中的 IV）`;
  });

  const handleSubmit = async (event: SubmitEvent) => {
    event.preventDefault();
    if (saving()) return;

    const trimmedName = name().trim();
    const trimmedServer = serverUrl().trim().replace(/\/+$/, "");
    const trimmedKey = deviceKey().trim();
    const encryption: BarkEncryption | null = encryptEnabled()
      ? {
          algorithm: algorithm(),
          mode: mode(),
          key: key(),
          iv: mode() === "ecb" ? null : iv(),
        }
      : null;

    const problem = validate(trimmedName, trimmedServer, trimmedKey, encryption);
    if (problem) {
      setError(problem);
      return;
    }

    const payload: NotifyChannelPayload = {
      name: trimmedName,
      enabled: enabled(),
      min_severity: minSeverity(),
      config: {
        kind: "bark",
        server_url: trimmedServer,
        device_key: trimmedKey,
        encryption,
      },
    };

    setSaving(true);
    setError(null);
    try {
      if (existing) {
        await updateNotifyChannel(existing.id, payload);
      } else {
        await createNotifyChannel(payload);
      }
      props.onSaved();
      props.onClose();
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) {
        authStore.handleUnauthorized();
        props.onClose();
        return;
      }
      setError(err instanceof Error ? err.message : "保存失败，请重试。");
    } finally {
      setSaving(false);
    }
  };

  const handleDialogKeyDown = (event: KeyboardEvent) => {
    if (event.key === "Escape") {
      event.preventDefault();
      if (!saving()) props.onClose();
      return;
    }
    if (event.key !== "Tab" || !dialogEl) return;

    const focusable = Array.from(
      dialogEl.querySelectorAll<HTMLElement>(
        'button:not([disabled]), input:not([disabled]), select:not([disabled]), [tabindex]:not([tabindex="-1"])'
      )
    ).filter((element) => !element.hidden && element.getAttribute("aria-hidden") !== "true");
    if (focusable.length === 0) {
      event.preventDefault();
      dialogEl.focus();
      return;
    }

    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    const active = document.activeElement;
    if (event.shiftKey && (active === first || !dialogEl.contains(active))) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && active === last) {
      event.preventDefault();
      first.focus();
    }
  };

  onMount(() => {
    previousFocus = document.activeElement instanceof HTMLElement
      ? document.activeElement
      : null;
    document.addEventListener("keydown", handleDialogKeyDown);
    queueMicrotask(() => dialogEl?.focus());
  });

  // 拉取服务端配置的默认地址用于占位提示；失败时保持内置默认值。
  onMount(async () => {
    try {
      const defaults = await fetchNotifyDefaults();
      if (defaults.server_url) setDefaultServerUrl(defaults.server_url);
    } catch {
      // 保持内置默认值。
    }
  });

  onCleanup(() => {
    document.removeEventListener("keydown", handleDialogKeyDown);
    if (previousFocus?.isConnected) previousFocus.focus();
  });

  return (
    <div class="modal-backdrop" onClick={() => !saving() && props.onClose()}>
      <div
        class="upgrade-dialog channel-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby={DIALOG_TITLE_ID}
        ref={(element) => (dialogEl = element)}
        tabIndex={-1}
        onClick={(event) => event.stopPropagation()}
      >
        <div class="upgrade-dialog-head">
          <h2 id={DIALOG_TITLE_ID}>
            {existing ? "编辑推送渠道" : "新增推送渠道"}
          </h2>
          <button
            type="button"
            class="dialog-close"
            aria-label="关闭"
            disabled={saving()}
            onClick={props.onClose}
          >
            ×
          </button>
        </div>

        <form onSubmit={(event) => void handleSubmit(event)}>
          <label class="form-field">
            <span>名称</span>
            <input
              type="text"
              value={name()}
              placeholder="例如：我的 iPhone"
              disabled={saving()}
              onInput={(event) => setName(event.currentTarget.value)}
            />
          </label>

          <label class="form-field">
            <span>服务地址</span>
            <input
              type="text"
              value={serverUrl()}
              placeholder={defaultServerUrl()}
              disabled={saving()}
              onInput={(event) => setServerUrl(event.currentTarget.value)}
            />
            <small>留空使用服务端配置的默认地址（config.toml 的 [notify] server_url，当前：{defaultServerUrl()}）；自建 Bark 服务器可在此覆盖。</small>
          </label>

          <label class="form-field">
            <span>设备 Key</span>
            <input
              type="text"
              value={deviceKey()}
              placeholder="Bark App 首页 URL 中的最后一段"
              disabled={saving()}
              onInput={(event) => setDeviceKey(event.currentTarget.value)}
            />
          </label>

          <label class="form-field">
            <span>最低级别</span>
            <select
              value={minSeverity()}
              disabled={saving()}
              onChange={(event) =>
                setMinSeverity(event.currentTarget.value as NotifySeverity)
              }
            >
              <option value="info">{SEVERITY_LABEL.info}</option>
              <option value="warning">{SEVERITY_LABEL.warning}</option>
              <option value="critical">{SEVERITY_LABEL.critical}</option>
            </select>
          </label>

          <label class="form-field form-field-inline">
            <input
              type="checkbox"
              checked={enabled()}
              disabled={saving()}
              onChange={(event) => setEnabled(event.currentTarget.checked)}
            />
            <span>启用该渠道</span>
          </label>

          <label class="form-field form-field-inline">
            <input
              type="checkbox"
              checked={encryptEnabled()}
              disabled={saving()}
              onChange={(event) => setEncryptEnabled(event.currentTarget.checked)}
            />
            <span>加密推送内容</span>
          </label>

          <Show when={encryptEnabled()}>
            <div class="encryption-fields">
              <label class="form-field">
                <span>算法</span>
                <select
                  value={algorithm()}
                  disabled={saving()}
                  onChange={(event) =>
                    setAlgorithm(event.currentTarget.value as BarkAlgorithm)
                  }
                >
                  <option value="aes128">AES128</option>
                  <option value="aes192">AES192</option>
                  <option value="aes256">AES256</option>
                </select>
              </label>

              <label class="form-field">
                <span>模式</span>
                <select
                  value={mode()}
                  disabled={saving()}
                  onChange={(event) => setMode(event.currentTarget.value as BarkMode)}
                >
                  <option value="gcm">GCM</option>
                  <option value="cbc">CBC</option>
                  <option value="ecb">ECB</option>
                </select>
              </label>

              <label class="form-field">
                <span>密钥</span>
                <input
                  type="text"
                  value={key()}
                  placeholder={`${KEY_LEN[algorithm()]} 个字符`}
                  disabled={saving()}
                  onInput={(event) => setKey(event.currentTarget.value)}
                />
                <small>与 Bark App 中填写的密钥完全一致（{KEY_LEN[algorithm()]} 个字符）。</small>
              </label>

              <label class="form-field">
                <span>IV</span>
                <input
                  type="text"
                  value={ivDisabled() ? "" : iv()}
                  placeholder={ivDisabled() ? "—" : ivHint()}
                  disabled={saving() || ivDisabled()}
                  onInput={(event) => setIv(event.currentTarget.value)}
                />
                <small>{ivHint()}，需与 Bark App 一致。</small>
              </label>
            </div>
          </Show>

          <Show when={error()}>
            {(message) => (
              <p id={ERROR_ID} class="wizard-error" role="alert">
                {message()}
              </p>
            )}
          </Show>

          <div class="dialog-actions">
            <button
              type="button"
              class="btn-secondary"
              disabled={saving()}
              onClick={props.onClose}
            >
              取消
            </button>
            <button type="submit" class="btn-primary" disabled={saving()}>
              {saving() ? "保存中…" : "保存"}
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}
