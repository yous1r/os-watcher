import { createSignal, Show, onCleanup, onMount } from "solid-js";
import { fetchLocal } from "../api";
import type { DeployAuth, DeployRequest, PackageKind } from "../types";
import { DeployProgress } from "./DeployProgress";
import { deployStore } from "../deployStore";

type WizardStep = 1 | 2 | 3 | 4;
type AuthMethod = "password" | "key";

const DEFAULT_API_PORT = 7980;
const DEFAULT_GOSSIP_PORT = 7979;
const DEFAULT_INSTALL_DIR = "/opt/os-watcher";
const DEFAULT_SERVICE_NAME = "os-watcher";
const DIALOG_TITLE_ID = "add-node-dialog-title";
const VALIDATION_ERROR_ID = "add-node-validation-error";
const SERVICE_NAME_PATTERN = /^[A-Za-z0-9._@-]+$/;
const HOST_FORBIDDEN_PATTERN = /[\s`$;&|<>(){}\\%'"\r\n]/;
const CONTROL_CHARACTER_PATTERN = /[\u0000-\u001F\u007F]/;

type ValidationIssue = {
  message: string;
  fieldId: string;
  step: 1 | 2;
};

function parsePort(value: string): number | null {
  if (!/^\d+$/.test(value.trim())) return null;
  const parsed = Number(value);
  return Number.isInteger(parsed) && parsed >= 1 && parsed <= 65535
    ? parsed
    : null;
}

function splitPeers(value: string): string[] {
  return value
    .split(/[\s,]+/)
    .map((peer) => peer.trim())
    .filter(Boolean);
}

function isValidPeer(peer: string): boolean {
  if (!peer || /[\s,`$;&|<>(){}\\%'"\r\n]/.test(peer)) return false;

  let host: string;
  let port: string;
  if (peer.startsWith("[")) {
    const separator = peer.indexOf("]:");
    if (separator <= 1 || peer.indexOf("]:", separator + 2) !== -1) return false;
    host = peer.slice(1, separator);
    port = peer.slice(separator + 2);
    if (!host.includes(":") || host.includes("[") || host.includes("]")) return false;
  } else {
    const parts = peer.split(":");
    if (parts.length !== 2) return false;
    [host, port] = parts;
  }

  return host.length > 0 && parsePort(port) !== null;
}

/**
 * 添加节点向导：四步（SSH 连接 / 高级配置 / 确认 / 部署）。
 * 密码与私钥仅停留在内存，不写 localStorage、不打日志。
 *
 * 部署连接与状态由模块级 deployStore 持有，脱离本组件生命周期：
 * 关闭对话框不会断开连接（后台运行），重新打开时直接回到第四步展示进行中的部署。
 */
export function AddNodeDialog(props: {
  onClose: () => void;
  onDeployed?: () => void;
}) {
  // 若已有进行中/未确认的部署，重新打开时直接落到第四步展示它。
  const [step, setStep] = createSignal<WizardStep>(
    deployStore.isActive() ? 4 : 1
  );

  // 第一步：SSH 连接
  const [host, setHost] = createSignal("");
  const [port, setPort] = createSignal("22");
  const [username, setUsername] = createSignal("root");
  const [authMethod, setAuthMethod] = createSignal<AuthMethod>("password");
  const [password, setPassword] = createSignal("");
  const [privateKey, setPrivateKey] = createSignal("");
  const [passphrase, setPassphrase] = createSignal("");

  // 第二步：高级配置（默认折叠）
  const [advancedOpen, setAdvancedOpen] = createSignal(false);
  const [pkg, setPkg] = createSignal<PackageKind>("node");
  const [apiPort, setApiPort] = createSignal(String(DEFAULT_API_PORT));
  const [gossipPort, setGossipPort] = createSignal(String(DEFAULT_GOSSIP_PORT));
  const [peers, setPeers] = createSignal("");
  const [serviceName, setServiceName] = createSignal(DEFAULT_SERVICE_NAME);
  const [installDir, setInstallDir] = createSignal(DEFAULT_INSTALL_DIR);
  const [version, setVersion] = createSignal("latest");
  const [proxy, setProxy] = createSignal("");
  const [validationError, setValidationError] = createSignal("");
  const [invalidField, setInvalidField] = createSignal<string | null>(null);

  let dialogEl: HTMLDivElement | undefined;
  let previousFocus: HTMLElement | null = null;

  const handleClose = () => {
    props.onClose();
  };

  const clearValidation = () => {
    setValidationError("");
    setInvalidField(null);
  };

  const showValidationIssue = (issue: ValidationIssue) => {
    setStep(issue.step);
    if (issue.step === 2) setAdvancedOpen(true);
    setValidationError(issue.message);
    setInvalidField(issue.fieldId);
    queueMicrotask(() => document.getElementById(issue.fieldId)?.focus());
  };

  const validateStep1 = (): ValidationIssue | null => {
    const normalizedHost = host().trim();
    if (!normalizedHost || HOST_FORBIDDEN_PATTERN.test(normalizedHost)) {
      return { message: "请输入合法的主机地址。", fieldId: "deploy-host", step: 1 };
    }
    if (parsePort(port()) === null) {
      return { message: "SSH 端口必须是 1 到 65535 的整数。", fieldId: "deploy-port", step: 1 };
    }
    if (!username().trim() || /\s/.test(username())) {
      return { message: "请输入不含空白字符的 SSH 用户名。", fieldId: "deploy-username", step: 1 };
    }
    if (authMethod() === "password" && !password()) {
      return { message: "请输入 SSH 密码。", fieldId: "deploy-password", step: 1 };
    }
    if (authMethod() === "key" && !privateKey().trim()) {
      return { message: "请粘贴 SSH 私钥。", fieldId: "deploy-private-key", step: 1 };
    }
    return null;
  };

  const validateStep2 = (): ValidationIssue | null => {
    if (parsePort(apiPort()) === null) {
      return { message: "API 端口必须是 1 到 65535 的整数。", fieldId: "deploy-api-port", step: 2 };
    }
    if (parsePort(gossipPort()) === null) {
      return { message: "Gossip 端口必须是 1 到 65535 的整数。", fieldId: "deploy-gossip-port", step: 2 };
    }
    const peerList = splitPeers(peers());
    if (peerList.some((peer) => !isValidPeer(peer))) {
      return { message: "Peer 必须使用 host:port 或 [IPv6]:port 格式。", fieldId: "deploy-peers", step: 2 };
    }
    if (!SERVICE_NAME_PATTERN.test(serviceName().trim())) {
      return { message: "服务名只能包含字母、数字和 . _ @ -。", fieldId: "deploy-service-name", step: 2 };
    }
    const normalizedVersion = version().trim();
    if (!normalizedVersion || /\s/.test(normalizedVersion)) {
      return { message: "版本不能为空或包含空白字符。", fieldId: "deploy-version", step: 2 };
    }
    const normalizedInstallDir = installDir().trim();
    if (
      !normalizedInstallDir.startsWith("/") ||
      /['"]/.test(normalizedInstallDir) ||
      normalizedInstallDir.includes("%") ||
      CONTROL_CHARACTER_PATTERN.test(normalizedInstallDir)
    ) {
      return { message: "安装目录必须是无引号、无控制字符、无 % 的绝对路径。", fieldId: "deploy-install-dir", step: 2 };
    }
    return null;
  };

  const handleDialogKeyDown = (event: KeyboardEvent) => {
    if (event.key === "Escape") {
      event.preventDefault();
      handleClose();
      return;
    }
    if (event.key !== "Tab" || !dialogEl) return;

    const focusable = Array.from(
      dialogEl.querySelectorAll<HTMLElement>(
        'button:not([disabled]), input:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])'
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

  onCleanup(() => {
    document.removeEventListener("keydown", handleDialogKeyDown);
    if (previousFocus?.isConnected) previousFocus.focus();
  });

  // 预填本机 gossip 地址作为默认 peer（仅在没有进行中部署、需要填表时）。
  onMount(async () => {
    if (deployStore.isActive()) return;
    try {
      const snap = await fetchLocal();
      const addr = resolveGossipAddr(snap.info.gossip_addr);
      if (addr && !peers().trim()) {
        setPeers(addr);
      }
    } catch {
      // 拿不到本机信息时留空，由后端回填。
    }
  });

  const buildRequest = (): DeployRequest => {
    const auth: DeployAuth =
      authMethod() === "password"
        ? { type: "password", password: password() }
        : {
            type: "key",
            private_key: privateKey(),
            passphrase: passphrase().length > 0 ? passphrase() : null,
          };
    const peerList = splitPeers(peers());
    return {
      host: host().trim(),
      port: Number(port()),
      username: username().trim(),
      auth,
      package: pkg(),
      api_port: Number(apiPort()),
      gossip_port: Number(gossipPort()),
      peers: peerList,
      service_name: serviceName().trim(),
      install_dir: installDir().trim(),
      version: version().trim(),
      repo: null,
      proxy: proxy().trim().length > 0 ? proxy().trim() : null,
    };
  };

  const advanceFromStep1 = () => {
    const issue = validateStep1();
    if (issue) return showValidationIssue(issue);
    clearValidation();
    setStep(2);
  };

  const advanceFromStep2 = () => {
    const issue = validateStep2();
    if (issue) return showValidationIssue(issue);
    clearValidation();
    setStep(3);
  };

  const startDeploy = () => {
    const issue = validateStep1() ?? validateStep2();
    if (issue) return showValidationIssue(issue);
    clearValidation();
    // 交给模块级 store 持有连接与状态；onDeployed 在成功时恰好触发一次。
    deployStore.start(buildRequest(), props.onDeployed);
    setStep(4);
  };

  // 完成后（success/error）确认结果：清空 store 并关闭对话框。
  const finishAndClose = () => {
    deployStore.reset();
    props.onClose();
  };

  // 真正中止进行中的部署：断开连接（服务端随之取消），清空并关闭。
  const abortAndClose = () => {
    deployStore.cancel();
    props.onClose();
  };

  return (
    <div class="modal-backdrop">
      <div
        class="add-node-dialog"
        ref={(element) => (dialogEl = element)}
        role="dialog"
        aria-modal="true"
        aria-labelledby={DIALOG_TITLE_ID}
        tabIndex={-1}
        onClick={(e) => e.stopPropagation()}
      >
        <div class="upgrade-dialog-head">
          <h2 id={DIALOG_TITLE_ID}>添加节点</h2>
          <button
            type="button"
            class="dialog-close"
            aria-label="关闭"
            onClick={handleClose}
          >
            ×
          </button>
        </div>

        <ol class="wizard-steps" aria-label="部署步骤">
          <li classList={{ active: step() === 1, done: step() > 1 }}>连接</li>
          <li classList={{ active: step() === 2, done: step() > 2 }}>配置</li>
          <li classList={{ active: step() === 3, done: step() > 3 }}>确认</li>
          <li classList={{ active: step() === 4 }}>部署</li>
        </ol>

        {/* 第一步：SSH 连接 */}
        <Show when={step() === 1}>
          <div class="wizard-body">
            <div class="form-grid">
              <label class="form-field span-2">
                <span>主机地址</span>
                <input
                  id="deploy-host"
                  type="text"
                  value={host()}
                  placeholder="10.0.0.12"
                  autocomplete="off"
                  aria-invalid={invalidField() === "deploy-host"}
                  onInput={(e) => setHost(e.currentTarget.value)}
                />
              </label>
              <label class="form-field">
                <span>SSH 端口</span>
                <input
                  id="deploy-port"
                  type="number"
                  min="1"
                  max="65535"
                  value={port()}
                  aria-invalid={invalidField() === "deploy-port"}
                  onInput={(e) => setPort(e.currentTarget.value)}
                />
              </label>
              <label class="form-field">
                <span>用户名</span>
                <input
                  id="deploy-username"
                  type="text"
                  value={username()}
                  autocomplete="off"
                  aria-invalid={invalidField() === "deploy-username"}
                  onInput={(e) => setUsername(e.currentTarget.value)}
                />
              </label>
            </div>

            <div class="package-toggle" role="group" aria-label="认证方式">
              <button
                type="button"
                classList={{ active: authMethod() === "password" }}
                onClick={() => setAuthMethod("password")}
              >
                密码
              </button>
              <button
                type="button"
                classList={{ active: authMethod() === "key" }}
                onClick={() => setAuthMethod("key")}
              >
                私钥
              </button>
            </div>

            <Show when={authMethod() === "password"}>
              <label class="form-field">
                <span>密码</span>
                <input
                  id="deploy-password"
                  type="password"
                  value={password()}
                  autocomplete="new-password"
                  aria-invalid={invalidField() === "deploy-password"}
                  onInput={(e) => setPassword(e.currentTarget.value)}
                />
              </label>
            </Show>

            <Show when={authMethod() === "key"}>
              <label class="form-field">
                <span>私钥（PEM）</span>
                <textarea
                  id="deploy-private-key"
                  class="key-input"
                  rows="6"
                  value={privateKey()}
                  placeholder="-----BEGIN OPENSSH PRIVATE KEY-----"
                  spellcheck={false}
                  aria-invalid={invalidField() === "deploy-private-key"}
                  onInput={(e) => setPrivateKey(e.currentTarget.value)}
                />
              </label>
              <label class="form-field">
                <span>私钥口令（可选）</span>
                <input
                  type="password"
                  value={passphrase()}
                  autocomplete="new-password"
                  onInput={(e) => setPassphrase(e.currentTarget.value)}
                />
              </label>
            </Show>
          </div>
        </Show>

        {/* 第二步：高级配置 */}
        <Show when={step() === 2}>
          <div class="wizard-body">
            <div class="form-field">
              <span>包类型</span>
              <div class="package-toggle" role="group" aria-label="包类型">
                <button
                  type="button"
                  classList={{ active: pkg() === "node" }}
                  onClick={() => setPkg("node")}
                >
                  Node
                </button>
                <button
                  type="button"
                  classList={{ active: pkg() === "full" }}
                  onClick={() => setPkg("full")}
                >
                  Full
                </button>
              </div>
            </div>

            <button
              type="button"
              class="advanced-toggle"
              aria-expanded={advancedOpen()}
              onClick={() => setAdvancedOpen((v) => !v)}
            >
              {advancedOpen() ? "▾" : "▸"} 高级配置
            </button>

            <Show when={advancedOpen()}>
              <div class="form-grid">
                <label class="form-field">
                  <span>API 端口</span>
                  <input
                    id="deploy-api-port"
                    type="number"
                    min="1"
                    max="65535"
                    value={apiPort()}
                    aria-invalid={invalidField() === "deploy-api-port"}
                    onInput={(e) => setApiPort(e.currentTarget.value)}
                  />
                </label>
                <label class="form-field">
                  <span>Gossip 端口</span>
                  <input
                    id="deploy-gossip-port"
                    type="number"
                    min="1"
                    max="65535"
                    value={gossipPort()}
                    aria-invalid={invalidField() === "deploy-gossip-port"}
                    onInput={(e) => setGossipPort(e.currentTarget.value)}
                  />
                </label>
                <label class="form-field span-2">
                  <span>Peers（逗号或空格分隔）</span>
                  <input
                    id="deploy-peers"
                    type="text"
                    value={peers()}
                    placeholder="192.168.1.10:7979"
                    autocomplete="off"
                    aria-invalid={invalidField() === "deploy-peers"}
                    onInput={(e) => setPeers(e.currentTarget.value)}
                  />
                </label>
                <label class="form-field">
                  <span>服务名</span>
                  <input
                    id="deploy-service-name"
                    type="text"
                    value={serviceName()}
                    autocomplete="off"
                    aria-invalid={invalidField() === "deploy-service-name"}
                    onInput={(e) => setServiceName(e.currentTarget.value)}
                  />
                </label>
                <label class="form-field">
                  <span>版本</span>
                  <input
                    id="deploy-version"
                    type="text"
                    value={version()}
                    placeholder="latest"
                    autocomplete="off"
                    aria-invalid={invalidField() === "deploy-version"}
                    onInput={(e) => setVersion(e.currentTarget.value)}
                  />
                </label>
                <label class="form-field span-2">
                  <span>安装目录</span>
                  <input
                    id="deploy-install-dir"
                    type="text"
                    value={installDir()}
                    autocomplete="off"
                    aria-invalid={invalidField() === "deploy-install-dir"}
                    onInput={(e) => setInstallDir(e.currentTarget.value)}
                  />
                </label>
                <label class="form-field span-2">
                  <span>下载代理（可选）</span>
                  <input
                    type="text"
                    value={proxy()}
                    placeholder="http://proxy.example:7890"
                    autocomplete="off"
                    onInput={(e) => setProxy(e.currentTarget.value)}
                  />
                </label>
              </div>
            </Show>
          </div>
        </Show>

        {/* 第三步：确认 */}
        <Show when={step() === 3}>
          <div class="wizard-body">
            <div class="upgrade-meta">
              <div>
                <span>目标主机</span>
                <strong>
                  {username()}@{host()}:{port()}
                </strong>
              </div>
              <div>
                <span>认证方式</span>
                <strong>{authMethod() === "password" ? "密码" : "私钥"}</strong>
              </div>
              <div>
                <span>包类型</span>
                <strong>{pkg()}</strong>
              </div>
              <div>
                <span>API / Gossip 端口</span>
                <strong>
                  {apiPort()} / {gossipPort()}
                </strong>
              </div>
              <div>
                <span>Peers</span>
                <strong>{peers().trim() || "（自动填本机）"}</strong>
              </div>
              <div>
                <span>服务名</span>
                <strong>{serviceName()}</strong>
              </div>
              <div>
                <span>安装目录</span>
                <strong>{installDir()}</strong>
              </div>
              <div>
                <span>版本</span>
                <strong>{version()}</strong>
              </div>
            </div>
            <div class="deploy-hint">
              部署需要目标机 root 或免密 sudo 权限。凭据仅在本次部署内存中使用，不会保存。
            </div>
          </div>
        </Show>

        {/* 第四步：部署（状态来自模块级 deployStore） */}
        <Show when={step() === 4}>
          <div class="wizard-body">
            <DeployProgress />
          </div>
        </Show>

        <Show when={validationError()}>
          {(message) => (
            <div
              id={VALIDATION_ERROR_ID}
              class="wizard-error"
              role="alert"
              aria-live="assertive"
            >
              {message()}
            </div>
          )}
        </Show>

        <div class="dialog-actions">
          {/* 第 1-3 步：取消（未开始部署，直接关） */}
          <Show when={step() < 4}>
            <button type="button" class="btn-secondary" onClick={handleClose}>
              取消
            </button>
          </Show>

          {/* 第 4 步进行中：后台运行（保留连接）+ 中止部署（断开取消） */}
          <Show when={step() === 4 && deployStore.isRunning()}>
            <button type="button" class="btn-secondary" onClick={handleClose}>
              后台运行
            </button>
            <button type="button" class="btn-danger" onClick={abortAndClose}>
              中止部署
            </button>
          </Show>

          {/* 第 4 步终态：关闭并清空 */}
          <Show when={step() === 4 && !deployStore.isRunning()}>
            <button type="button" class="btn-secondary" onClick={finishAndClose}>
              关闭
            </button>
          </Show>

          {/* 步骤导航 */}
          <Show when={step() > 1 && step() < 4}>
            <button
              type="button"
              class="btn-secondary"
              onClick={() => {
                clearValidation();
                setStep((s) => (s - 1) as WizardStep);
              }}
            >
              上一步
            </button>
          </Show>
          <Show when={step() === 1}>
            <button
              type="button"
              class="btn-primary"
              onClick={advanceFromStep1}
            >
              下一步
            </button>
          </Show>
          <Show when={step() === 2}>
            <button type="button" class="btn-primary" onClick={advanceFromStep2}>
              下一步
            </button>
          </Show>
          <Show when={step() === 3}>
            <button type="button" class="btn-primary" onClick={startDeploy}>
              开始部署
            </button>
          </Show>
        </div>
      </div>
    </div>
  );
}

/** 把 0.0.0.0/:: 的 gossip 地址回退到浏览器可达的 host。 */
function resolveGossipAddr(gossipAddr: string): string | null {
  if (!gossipAddr) return null;
  const match = gossipAddr.match(/^(.*):(\d+)$/);
  if (!match) return gossipAddr;
  let hostPart = match[1];
  const portPart = match[2];
  if (hostPart === "0.0.0.0" || hostPart === "::" || hostPart === "[::]") {
    hostPart = window.location.hostname;
  }
  return `${hostPart}:${portPart}`;
}
