import { createSignal, onCleanup, onMount, Show } from "solid-js";
import { ApiError, triggerUninstall } from "../api";
import { authStore } from "../authStore";

const DIALOG_TITLE_ID = "uninstall-dialog-title";
const ERROR_ID = "uninstall-error";
const CONFIRM_WORD = "uninstall";

/**
 * 卸载本机安装。管理员专属：后端会注销服务并删除安装目录。
 *
 * 与升级不同，卸载没有可轮询的完成状态——服务随后就停了，没人能回报结果。
 * 因此请求成功即终态，对话框只负责把「接下来会发生什么」讲清楚。
 * 输入确认词是这里唯一能挡住误点的东西，不设默认值。
 */
export function UninstallDialog(props: { onClose: () => void }) {
  const [backup, setBackup] = createSignal(true);
  const [keepConfig, setKeepConfig] = createSignal(false);
  const [confirmation, setConfirmation] = createSignal("");
  const [error, setError] = createSignal<string | null>(null);
  const [submitting, setSubmitting] = createSignal(false);
  const [done, setDone] = createSignal<string | null>(null);

  let dialogEl: HTMLDivElement | undefined;
  let previousFocus: HTMLElement | null = null;

  const confirmed = () => confirmation().trim() === CONFIRM_WORD;

  const handleSubmit = async (event: SubmitEvent) => {
    event.preventDefault();
    if (submitting() || !confirmed()) return;
    // 双保险：即便按钮被绕过，这里也再问一次管理员身份。
    if (!authStore.allowed()) return;

    setSubmitting(true);
    setError(null);
    try {
      const result = await triggerUninstall({
        backup: backup(),
        keep_config: keepConfig(),
      });
      setDone(result.message);
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) {
        authStore.handleUnauthorized();
        props.onClose();
        return;
      }
      setError(err instanceof Error ? err.message : "卸载请求失败");
    } finally {
      setSubmitting(false);
    }
  };

  const handleDialogKeyDown = (event: KeyboardEvent) => {
    if (event.key === "Escape" && !submitting()) {
      event.preventDefault();
      props.onClose();
    }
  };

  onMount(() => {
    previousFocus =
      document.activeElement instanceof HTMLElement
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
    <div class="modal-backdrop" onClick={() => !submitting() && props.onClose()}>
      <div
        class="upgrade-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby={DIALOG_TITLE_ID}
        ref={(element) => (dialogEl = element)}
        tabIndex={-1}
        onClick={(event) => event.stopPropagation()}
      >
        <div class="upgrade-dialog-head">
          <h2 id={DIALOG_TITLE_ID}>卸载 os-watcher</h2>
          <button
            type="button"
            class="dialog-close"
            aria-label="关闭"
            disabled={submitting()}
            onClick={props.onClose}
          >
            ×
          </button>
        </div>

        <Show
          when={done()}
          fallback={
            <form onSubmit={(event) => void handleSubmit(event)}>
              <p class="notice-warn">
                本机上的服务注册与安装目录都会被删除，监控数据（os-watcher.db）
                与 web-dist 不会保留。正在运行的可执行文件、日志等被占用的文件，
                会在下次重启时删除。
              </p>

              <label class="form-field form-field-inline">
                <input
                  type="checkbox"
                  checked={backup()}
                  disabled={submitting()}
                  onChange={(event) => setBackup(event.currentTarget.checked)}
                />
                <span>卸载前备份 config.toml（含口令与节点配置）</span>
              </label>

              <label class="form-field form-field-inline">
                <input
                  type="checkbox"
                  checked={keepConfig()}
                  disabled={submitting()}
                  onChange={(event) => setKeepConfig(event.currentTarget.checked)}
                />
                <span>保留 config.toml，不随安装目录删除</span>
              </label>

              <Show when={backup()}>
                <p class="notify-hint">
                  备份写入安装目录同级的 os-watcher-backup-&lt;时间戳&gt;/，不在被删除的
                  目录内。若同时保留 config.toml，两者都会留在原处。
                </p>
              </Show>

              <label class="form-field" for="uninstall-confirm">
                <span>输入 {CONFIRM_WORD} 以确认</span>
                <input
                  id="uninstall-confirm"
                  type="text"
                  autocomplete="off"
                  spellcheck={false}
                  value={confirmation()}
                  aria-invalid={error() ? "true" : undefined}
                  aria-describedby={error() ? ERROR_ID : undefined}
                  disabled={submitting()}
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
                  disabled={submitting()}
                  onClick={props.onClose}
                >
                  取消
                </button>
                <button
                  type="submit"
                  class="btn-danger"
                  disabled={submitting() || !confirmed()}
                >
                  {submitting() ? "提交中…" : "确认卸载"}
                </button>
              </div>
            </form>
          }
        >
          {(message) => (
            <>
              <p class="notice-warn" role="status">
                {message()}
              </p>
              <p class="notify-hint">
                面板将无法再连上本机，此页面可以关闭了。
              </p>
              <div class="dialog-actions">
                <button type="button" class="btn-secondary" onClick={props.onClose}>
                  关闭
                </button>
              </div>
            </>
          )}
        </Show>
      </div>
    </div>
  );
}
