import { createSignal, For, onCleanup, onMount, Show } from "solid-js";
import { ApiError } from "../api";
import { authStore } from "../authStore";
import { deployStore } from "../deployStore";
import type { DeployAuth, DeployRequest, NodeSnapshot } from "../types";
import { DeployProgress } from "./DeployProgress";

type AuthMethod = "password" | "key";

const DEFAULT_INSTALL_DIR = "/opt/os-watcher";
const DEFAULT_SERVICE_NAME = "os-watcher";
const DIALOG_TITLE_ID = "remote-uninstall-dialog-title";
const ERROR_ID = "remote-uninstall-error";
const CONFIRM_WORD = "uninstall";
const SERVICE_NAME_PATTERN = /^[A-Za-z0-9._@-]+$/;
const HOST_FORBIDDEN_PATTERN = /[\s`$;&|<>(){}\\%'"\r\n]/;
const CONTROL_CHARACTER_PATTERN = /[\u0000-\u001F\u007F]/;

type ValidationIssue = {
  message: string;
  fieldId: string;
};

function parsePort(value: string): number | null {
  if (!/^\d+$/.test(value.trim())) return null;
  const parsed = Number(value);
  return Number.isInteger(parsed) && parsed >= 1 && parsed <= 65535
    ? parsed
    : null;
}

/**
 * 路径校验与 AddNodeDialog 的安装目录同规则：必须是绝对路径，且不含引号、
 * % 与控制字符——这些字符会被后端拼进远端 shell 命令，或让 heredoc 失效。
 */
function pathIssue(value: string, label: string): string | null {
  if (
    !value.startsWith("/") ||
    /['"]/.test(value) ||
    value.includes("%") ||
    CONTROL_CHARACTER_PATTERN.test(value)
  ) {
    return `${label}必须是无引号、无控制字符、无 % 的绝对路径。`;
  }
  return null;
}

/**
 * 从 api_addr（`host:port`，IPv6 为 `[host]:port`）取出 host 部分。
 * 通配绑定地址在 SSH 里没有意义，回退到面板自身的 host，与 apiBaseForNode 一致。
 */
function apiAddrHost(apiAddr: string): string {
  const value = apiAddr.trim();
  if (!value) return "";
  let hostPart = value;
  if (value.startsWith("[")) {
    const end = value.indexOf("]");
    hostPart = end > 0 ? value.slice(1, end) : value;
  } else {
    const separator = value.lastIndexOf(":");
    if (separator > 0) hostPart = value.slice(0, separator);
  }
  if (hostPart === "0.0.0.0" || hostPart === "::" || hostPart === "") {
    return window.location.hostname;
  }
  return hostPart;
}

/**
 * 远程卸载向导：通过 SSH 把面板自带的 uninstall.sh 上传到目标节点并执行。
 *
 * 脚本随面板发布（后端 include_str! 嵌入），因此目标节点上的版本多老都不影响
 * 卸载——这正是走这条通道而不是调用节点自身接口的原因。
 *
 * 连接与状态由模块级 deployStore 持有，脱离本组件生命周期：关闭对话框不会断开
 * 连接（后台继续卸载），重新打开时直接展示进行中的进度。
 */
export function RemoteUninstallDialog(props: {
  nodes: NodeSnapshot[];
  initialHost?: string;
  onClose: () => void;
  onDone?: () => void;
}) {
  // 选项列表在打开对话框时固定：App 每 3 秒换一批新的快照数组，跟着它重渲染
  // 会把用户已选中的项弹回第一项。这里只是预填主机地址的便利入口，短暂不变
  // 更符合预期；真正的目标仍以表单里的主机地址为准。
  const knownNodes: NodeSnapshot[] = props.nodes;
  // initialHost 传的是节点的 api_addr（host:port），这里只取 host 部分：
  // SSH 端口与 API 端口无关，默认仍用 22。
  const [host, setHost] = createSignal(
    props.initialHost ? apiAddrHost(props.initialHost) : ""
  );
  const [port, setPort] = createSignal("22");
  const [username, setUsername] = createSignal("root");
  const [authMethod, setAuthMethod] = createSignal<AuthMethod>("password");
  const [password, setPassword] = createSignal("");
  const [privateKey, setPrivateKey] = createSignal("");
  const [passphrase, setPassphrase] = createSignal("");
  const [serviceName, setServiceName] = createSignal(DEFAULT_SERVICE_NAME);
  const [installDir, setInstallDir] = createSignal(DEFAULT_INSTALL_DIR);
  const [backup, setBackup] = createSignal(true);
  const [keepConfig, setKeepConfig] = createSignal(false);
  const [backupDir, setBackupDir] = createSignal("");
  const [confirmation, setConfirmation] = createSignal("");
  const [error, setError] = createSignal<string | null>(null);
  const [invalidField, setInvalidField] = createSignal<string | null>(null);
  const [started, setStarted] = createSignal(false);

  let dialogEl: HTMLDivElement | undefined;
  let previousFocus: HTMLElement | null = null;

  const confirmed = () => confirmation().trim() === CONFIRM_WORD;

  // 展示进度面板的条件：本次会话发起的卸载，或 store 里正持有一个卸载任务
  // （关闭对话框再打开时回到进度，而不是让用户重填一遍表单）。
  // 部署任务不在此列：那是「添加节点」向导的进度，显示在卸载对话框里会误导。
  const showProgress = () =>
    started() || (deployStore.isActive() && deployStore.action() === "uninstall");

  // 单任务假设：store 同一时刻只跑一个远程任务，正在跑时不能再提交。
  // 用 isRunning 而非 isActive：后者把终态（另一个对话框留下的未确认结果）
  // 也算作占用，会把表单永久锁死。
  const busy = () => deployStore.isRunning();

  const showValidationIssue = (issue: ValidationIssue) => {
    setError(issue.message);
    setInvalidField(issue.fieldId);
    queueMicrotask(() => document.getElementById(issue.fieldId)?.focus());
  };

  const clearValidation = () => {
    setError(null);
    setInvalidField(null);
  };

  // backup_dir 输入框仅在勾选「卸载前备份」时渲染；未勾选时既不校验也不发送，
  // 否则报错会指向一个不可见、且不重新勾选就无法清空的控件。
  const effectiveBackupDir = () => (backup() ? backupDir().trim() : "");

  const validate = (): ValidationIssue | null => {
    const normalizedHost = host().trim();
    if (!normalizedHost || HOST_FORBIDDEN_PATTERN.test(normalizedHost)) {
      return { message: "请输入合法的主机地址。", fieldId: "remote-uninstall-host" };
    }
    if (parsePort(port()) === null) {
      return { message: "SSH 端口必须是 1 到 65535 的整数。", fieldId: "remote-uninstall-port" };
    }
    if (!username().trim() || /\s/.test(username())) {
      return { message: "请输入不含空白字符的 SSH 用户名。", fieldId: "remote-uninstall-username" };
    }
    if (authMethod() === "password" && !password()) {
      return { message: "请输入 SSH 密码。", fieldId: "remote-uninstall-password" };
    }
    if (authMethod() === "key" && !privateKey().trim()) {
      return { message: "请粘贴 SSH 私钥。", fieldId: "remote-uninstall-private-key" };
    }
    if (!SERVICE_NAME_PATTERN.test(serviceName().trim())) {
      return { message: "服务名只能包含字母、数字和 . _ @ -。", fieldId: "remote-uninstall-service-name" };
    }
    const installDirProblem = pathIssue(installDir().trim(), "安装目录");
    if (installDirProblem) {
      return { message: installDirProblem, fieldId: "remote-uninstall-install-dir" };
    }
    const explicitBackupDir = effectiveBackupDir();
    if (explicitBackupDir) {
      const backupDirProblem = pathIssue(explicitBackupDir, "备份目录");
      if (backupDirProblem) {
        return { message: backupDirProblem, fieldId: "remote-uninstall-backup-dir" };
      }
    }
    return null;
  };

  const buildRequest = (): DeployRequest => {
    const auth: DeployAuth =
      authMethod() === "password"
        ? { type: "password", password: password() }
        : {
            type: "key",
            private_key: privateKey(),
            passphrase: passphrase().length > 0 ? passphrase() : null,
          };
    const explicitBackupDir = effectiveBackupDir();
    return {
      action: "uninstall",
      host: host().trim(),
      port: Number(port()),
      username: username().trim(),
      auth,
      service_name: serviceName().trim(),
      install_dir: installDir().trim(),
      backup: backup(),
      keep_config: keepConfig(),
      // 留空交给脚本按「安装目录同级 + 时间戳」自行决定。
      backup_dir: explicitBackupDir.length > 0 ? explicitBackupDir : null,
    };
  };

  const handleSubmit = (event: SubmitEvent) => {
    event.preventDefault();
    if (busy() || !confirmed()) return;
    // 双保险：即便按钮被绕过，这里也再问一次管理员身份。
    if (!authStore.allowed()) return;

    const issue = validate();
    if (issue) return showValidationIssue(issue);
    clearValidation();

    try {
      // 交给模块级 store 持有连接与状态；onDone 在成功时恰好触发一次。
      deployStore.start(buildRequest(), props.onDone);
      setStarted(true);
    } catch (err) {
      // 通道是 WebSocket：握手被拒时浏览器只给一个错误事件，拿不到状态码。
      // 这里兜住 start 可能抛出的接口错误，与其它对话框一致地处理会话失效。
      if (err instanceof ApiError && err.status === 401) {
        authStore.handleUnauthorized();
        props.onClose();
        return;
      }
      setError(err instanceof Error ? err.message : "卸载请求失败");
    }
  };

  // 终态确认：清空 store 并关闭，让入口回到「远程卸载」。
  const finishAndClose = () => {
    deployStore.reset();
    props.onClose();
  };

  // 真正中止进行中的卸载：断开连接（服务端随之取消），清空并关闭。
  const abortAndClose = () => {
    deployStore.cancel();
    props.onClose();
  };

  const handleDialogKeyDown = (event: KeyboardEvent) => {
    if (event.key === "Escape") {
      event.preventDefault();
      // 关闭不中断：进行中的卸载继续在后台跑（与「添加节点」一致）。
      props.onClose();
      return;
    }
    if (event.key !== "Tab" || !dialogEl) return;

    const focusable = Array.from(
      dialogEl.querySelectorAll<HTMLElement>(
        'button:not([disabled]), input:not([disabled]), textarea:not([disabled]), select:not([disabled]), [tabindex]:not([tabindex="-1"])'
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

  return (
    <div class="modal-backdrop">
      <div
        class="upgrade-dialog remote-uninstall-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby={DIALOG_TITLE_ID}
        ref={(element) => (dialogEl = element)}
        tabIndex={-1}
        onClick={(event) => event.stopPropagation()}
      >
        <div class="upgrade-dialog-head">
          <h2 id={DIALOG_TITLE_ID}>远程卸载节点</h2>
          <button
            type="button"
            class="dialog-close"
            aria-label="关闭"
            onClick={props.onClose}
          >
            ×
          </button>
        </div>

        <Show
          when={showProgress()}
          fallback={
            <form onSubmit={handleSubmit}>
              <p class="notice-warn">
                目标节点上的服务注册与安装目录都会被删除，面板将失去它的监控数据。
                卸载脚本由面板自带并上传执行，目标节点上的版本再老也不影响。
              </p>

              <Show when={busy()}>
                <div class="deploy-hint">
                  已有进行中的远程任务，完成或中止后才能发起新的卸载。
                </div>
              </Show>

              <label class="form-field">
                <span>目标节点（可选，仅用于预填主机地址）</span>
                {/* 选项列表在打开对话框时固定下来：App 每 3 秒换一批新的快照数组，
                    跟着它重渲染会把用户已选中的项弹回第一项。这里只是预填的
                    便利入口，清单短暂不变更符合预期。 */}
                <select
                  id="remote-uninstall-node"
                  disabled={busy()}
                  onChange={(event) => {
                    const node = knownNodes.find(
                      (item) => item.info.id === event.currentTarget.value
                    );
                    if (node) setHost(apiAddrHost(node.info.api_addr));
                  }}
                >
                  <option value="">手动填写主机地址</option>
                  <For each={knownNodes}>
                    {(node) => (
                      <option value={node.info.id}>
                        {node.info.hostname}（{node.info.api_addr}）
                      </option>
                    )}
                  </For>
                </select>
              </label>

              <div class="form-grid">
                <label class="form-field span-2">
                  <span>主机地址</span>
                  <input
                    id="remote-uninstall-host"
                    type="text"
                    value={host()}
                    placeholder="10.0.0.12"
                    autocomplete="off"
                    disabled={busy()}
                    aria-invalid={invalidField() === "remote-uninstall-host"}
                    onInput={(event) => setHost(event.currentTarget.value)}
                  />
                </label>
                <label class="form-field">
                  <span>SSH 端口</span>
                  <input
                    id="remote-uninstall-port"
                    type="number"
                    min="1"
                    max="65535"
                    value={port()}
                    disabled={busy()}
                    aria-invalid={invalidField() === "remote-uninstall-port"}
                    onInput={(event) => setPort(event.currentTarget.value)}
                  />
                </label>
                <label class="form-field">
                  <span>用户名</span>
                  <input
                    id="remote-uninstall-username"
                    type="text"
                    value={username()}
                    autocomplete="off"
                    disabled={busy()}
                    aria-invalid={invalidField() === "remote-uninstall-username"}
                    onInput={(event) => setUsername(event.currentTarget.value)}
                  />
                </label>
              </div>

              <div class="package-toggle" role="group" aria-label="认证方式">
                <button
                  type="button"
                  classList={{ active: authMethod() === "password" }}
                  disabled={busy()}
                  onClick={() => setAuthMethod("password")}
                >
                  密码
                </button>
                <button
                  type="button"
                  classList={{ active: authMethod() === "key" }}
                  disabled={busy()}
                  onClick={() => setAuthMethod("key")}
                >
                  私钥
                </button>
              </div>

              <Show when={authMethod() === "password"}>
                <label class="form-field">
                  <span>密码</span>
                  <input
                    id="remote-uninstall-password"
                    type="password"
                    value={password()}
                    autocomplete="new-password"
                    disabled={busy()}
                    aria-invalid={invalidField() === "remote-uninstall-password"}
                    onInput={(event) => setPassword(event.currentTarget.value)}
                  />
                </label>
              </Show>

              <Show when={authMethod() === "key"}>
                <label class="form-field">
                  <span>私钥（PEM）</span>
                  <textarea
                    id="remote-uninstall-private-key"
                    class="key-input"
                    rows="6"
                    value={privateKey()}
                    placeholder="-----BEGIN OPENSSH PRIVATE KEY-----"
                    spellcheck={false}
                    disabled={busy()}
                    aria-invalid={invalidField() === "remote-uninstall-private-key"}
                    onInput={(event) => setPrivateKey(event.currentTarget.value)}
                  />
                </label>
                <label class="form-field">
                  <span>私钥口令（可选）</span>
                  <input
                    type="password"
                    value={passphrase()}
                    autocomplete="new-password"
                    disabled={busy()}
                    onInput={(event) => setPassphrase(event.currentTarget.value)}
                  />
                </label>
              </Show>

              <div class="form-grid">
                <label class="form-field">
                  <span>服务名</span>
                  <input
                    id="remote-uninstall-service-name"
                    type="text"
                    value={serviceName()}
                    autocomplete="off"
                    disabled={busy()}
                    aria-invalid={invalidField() === "remote-uninstall-service-name"}
                    onInput={(event) => setServiceName(event.currentTarget.value)}
                  />
                </label>
                <label class="form-field">
                  <span>安装目录</span>
                  <input
                    id="remote-uninstall-install-dir"
                    type="text"
                    value={installDir()}
                    autocomplete="off"
                    disabled={busy()}
                    aria-invalid={invalidField() === "remote-uninstall-install-dir"}
                    onInput={(event) => setInstallDir(event.currentTarget.value)}
                  />
                </label>
              </div>

              <label class="form-field form-field-inline">
                <input
                  type="checkbox"
                  checked={backup()}
                  disabled={busy()}
                  onChange={(event) => setBackup(event.currentTarget.checked)}
                />
                <span>卸载前备份 config.toml（含口令与节点配置）</span>
              </label>

              <label class="form-field form-field-inline">
                <input
                  type="checkbox"
                  checked={keepConfig()}
                  disabled={busy()}
                  onChange={(event) => setKeepConfig(event.currentTarget.checked)}
                />
                <span>保留 config.toml，不随安装目录删除</span>
              </label>

              <Show when={backup()}>
                <label class="form-field">
                  <span>备份目录（可选）</span>
                  <input
                    id="remote-uninstall-backup-dir"
                    type="text"
                    value={backupDir()}
                    placeholder="留空则备份到安装目录同级的 os-watcher-backup-<时间戳>/"
                    autocomplete="off"
                    disabled={busy()}
                    aria-invalid={invalidField() === "remote-uninstall-backup-dir"}
                    onInput={(event) => setBackupDir(event.currentTarget.value)}
                  />
                </label>
              </Show>

              <label class="form-field" for="remote-uninstall-confirm">
                <span>输入 {CONFIRM_WORD} 以确认</span>
                <input
                  id="remote-uninstall-confirm"
                  type="text"
                  autocomplete="off"
                  spellcheck={false}
                  value={confirmation()}
                  disabled={busy()}
                  aria-invalid={error() ? "true" : undefined}
                  aria-describedby={error() ? ERROR_ID : undefined}
                  onInput={(event) => setConfirmation(event.currentTarget.value)}
                />
              </label>

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
                  disabled={busy()}
                  onClick={props.onClose}
                >
                  取消
                </button>
                <button
                  type="submit"
                  class="btn-danger"
                  disabled={busy() || !confirmed()}
                >
                  确认远程卸载
                </button>
              </div>
            </form>
          }
        >
          <div class="wizard-body">
            <DeployProgress />
          </div>
          <div class="dialog-actions">
            {/* 进行中：关闭只是隐藏 UI，卸载在后台继续 */}
            <Show when={deployStore.isRunning()}>
              <button type="button" class="btn-secondary" onClick={props.onClose}>
                后台运行
              </button>
              <button type="button" class="btn-danger" onClick={abortAndClose}>
                中止卸载
              </button>
            </Show>
            {/* 终态：确认结果后清空并关闭 */}
            <Show when={!deployStore.isRunning()}>
              <button type="button" class="btn-secondary" onClick={finishAndClose}>
                关闭
              </button>
            </Show>
          </div>
        </Show>
      </div>
    </div>
  );
}
