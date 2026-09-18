import { createSignal, onCleanup, onMount, Show } from "solid-js";
import { ApiError } from "../api";
import { authStore } from "../authStore";

const DIALOG_TITLE_ID = "login-dialog-title";
const ERROR_ID = "login-error";
const PASSWORD_INPUT_ID = "login-password";

/**
 * 管理登录对话框。监控页面访客可读，涉及部署、推送渠道等管理动作时弹出。
 * 会话由后端 HttpOnly Cookie 维持，前端不保存口令。
 */
export function LoginDialog(props: { onClose: () => void }) {
  const [password, setPassword] = createSignal("");
  const [error, setError] = createSignal<string | null>(null);
  const [submitting, setSubmitting] = createSignal(false);
  let dialogEl: HTMLDivElement | undefined;
  let inputEl: HTMLInputElement | undefined;
  let previousFocus: HTMLElement | null = null;

  const handleSubmit = async (event: SubmitEvent) => {
    event.preventDefault();
    if (submitting()) return;

    const value = password();
    if (!value) {
      setError("请输入管理口令。");
      inputEl?.focus();
      return;
    }

    setSubmitting(true);
    setError(null);
    try {
      await authStore.login(value);
      setPassword("");
      props.onClose();
    } catch (err) {
      setError(err instanceof ApiError ? err.message : "登录失败，请重试。");
      setPassword("");
      inputEl?.focus();
    } finally {
      setSubmitting(false);
    }
  };

  const handleDialogKeyDown = (event: KeyboardEvent) => {
    if (event.key === "Escape") {
      event.preventDefault();
      if (!submitting()) props.onClose();
      return;
    }
    if (event.key !== "Tab" || !dialogEl) return;

    const focusable = Array.from(
      dialogEl.querySelectorAll<HTMLElement>(
        'button:not([disabled]), input:not([disabled]), [tabindex]:not([tabindex="-1"])'
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
    queueMicrotask(() => inputEl?.focus());
  });

  onCleanup(() => {
    document.removeEventListener("keydown", handleDialogKeyDown);
    if (previousFocus?.isConnected) previousFocus.focus();
  });

  return (
    <div class="modal-backdrop" onClick={() => !submitting() && props.onClose()}>
      <div
        class="login-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby={DIALOG_TITLE_ID}
        ref={(element) => (dialogEl = element)}
        tabIndex={-1}
        onClick={(event) => event.stopPropagation()}
      >
        <div class="upgrade-dialog-head">
          <h2 id={DIALOG_TITLE_ID}>管理员登录</h2>
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

        <form onSubmit={(event) => void handleSubmit(event)}>
          <p class="login-hint">
            监控数据所有人可读；部署节点、推送渠道等管理操作需要管理员口令。
          </p>

          <label class="form-field" for={PASSWORD_INPUT_ID}>
            <span>管理口令</span>
            <input
              id={PASSWORD_INPUT_ID}
              ref={(element) => (inputEl = element)}
              type="password"
              autocomplete="current-password"
              value={password()}
              aria-invalid={error() ? "true" : undefined}
              aria-describedby={error() ? ERROR_ID : undefined}
              disabled={submitting()}
              onInput={(event) => setPassword(event.currentTarget.value)}
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
            <button type="submit" class="btn-primary" disabled={submitting()}>
              {submitting() ? "登录中…" : "登录"}
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}
