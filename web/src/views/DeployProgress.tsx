import { createEffect, For, Show } from "solid-js";
import { deployStore } from "../deployStore";
import type { DeployStep } from "../types";

const STEPS: { key: DeployStep; label: string }[] = [
  { key: "connecting", label: "连接" },
  { key: "uploading", label: "上传" },
  { key: "installing", label: "安装" },
  { key: "verifying", label: "校验" },
];

/**
 * 部署进度：纯展示组件，只从模块级 deployStore 读取状态。
 * WebSocket 连接与状态都由 deployStore 持有，因此本组件的挂载/卸载
 * （即对话框的打开/关闭）不会影响进行中的部署——关闭对话框后部署继续。
 * 重新打开时会看到累积日志与当前步骤，而非空白面板。
 */
export function DeployProgress() {
  let logEl: HTMLDivElement | undefined;

  const stepIndex = (step: DeployStep) => STEPS.findIndex((s) => s.key === step);

  // 日志变化后滚到底部（组件重新挂载时也会补一次，恢复到最新位置）。
  createEffect(() => {
    deployStore.logs();
    queueMicrotask(() => {
      if (logEl) logEl.scrollTop = logEl.scrollHeight;
    });
  });

  return (
    <div class="deploy-progress">
      <ol class="deploy-steps">
        <For each={STEPS}>
          {(step) => {
            const idx = stepIndex(step.key);
            const state = () => {
              const phase = deployStore.phase();
              const cur = stepIndex(deployStore.currentStep());
              if (phase === "success") return "done";
              if (idx < cur) return "done";
              if (idx === cur) return phase === "error" ? "error" : "active";
              return "pending";
            };
            return (
              <li class="deploy-step" classList={{ [state()]: true }}>
                <span class="deploy-step-dot" />
                <span class="deploy-step-label">{step.label}</span>
              </li>
            );
          }}
        </For>
      </ol>

      <div
        class="deploy-status"
        classList={{
          "is-success": deployStore.phase() === "success",
          "is-error": deployStore.phase() === "error",
        }}
      >
        {deployStore.statusMessage()}
      </div>

      <Show when={deployStore.retryInfo()}>
        {(info) => <div class="deploy-retry">{info()}</div>}
      </Show>

      <div
        class="deploy-log"
        ref={logEl}
        role="log"
        aria-live="polite"
        aria-label="部署日志"
      >
        <Show
          when={deployStore.logs().length > 0}
          fallback={<div class="deploy-log-empty">等待远端输出…</div>}
        >
          <For each={deployStore.logs()}>
            {(entry) => (
              <div
                class="deploy-log-line"
                classList={{ stderr: entry.stream === "stderr" }}
              >
                {entry.line}
              </div>
            )}
          </For>
        </Show>
      </div>
    </div>
  );
}
