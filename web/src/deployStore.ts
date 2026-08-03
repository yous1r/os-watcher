import { createSignal } from "solid-js";
import { openDeployWebSocket } from "./api";
import type { DeployEvent, DeployRequest, DeployStep } from "./types";

const MAX_LOG_LINES = 500;

/**
 * 部署阶段：
 * - idle    无进行中或未确认的部署
 * - running 部署进行中（连接存活，即使对话框已关闭也不断开）
 * - success 已成功，等待用户确认后 reset
 * - error   已失败，等待用户确认后 reset
 */
export type DeployPhase = "idle" | "running" | "success" | "error";

export interface DeployLogLine {
  stream: "stdout" | "stderr";
  line: string;
}

// 模块级状态：脱离任何组件生命周期，因此对话框卸载不会影响它。
const [phase, setPhase] = createSignal<DeployPhase>("idle");
const [currentStep, setCurrentStep] = createSignal<DeployStep>("connecting");
const [statusMessage, setStatusMessage] = createSignal("");
const [retryInfo, setRetryInfo] = createSignal<string | null>(null);
const [logs, setLogs] = createSignal<DeployLogLine[]>([]);

let socket: WebSocket | undefined;
let onDeployedCb: (() => void) | undefined;
let deployedFired = false;

function appendLog(entry: DeployLogLine) {
  setLogs((prev) => {
    const next = prev.length >= MAX_LOG_LINES ? prev.slice(1) : prev.slice();
    next.push(entry);
    return next;
  });
}

function detachSocketHandlers(ws: WebSocket) {
  ws.onopen = null;
  ws.onmessage = null;
  ws.onerror = null;
  ws.onclose = null;
}

function closeSocket(target = socket) {
  if (!target || target !== socket) return;

  // 先清掉全局引用和全部回调。即使浏览器随后派发旧连接的迟到事件，
  // 也无法再改写下一次部署的模块级状态。
  socket = undefined;
  detachSocketHandlers(target);
  if (target.readyState <= WebSocket.OPEN) {
    target.close();
  }
}

function handleEvent(ws: WebSocket, event: DeployEvent) {
  if (ws !== socket) return;

  switch (event.type) {
    case "progress":
      setCurrentStep(event.step);
      setStatusMessage(event.message);
      setRetryInfo(null);
      break;
    case "log":
      appendLog({ stream: event.stream, line: event.line });
      break;
    case "retry":
      setRetryInfo(`第 ${event.attempt}/${event.max} 次重试：${event.message}`);
      break;
    case "success":
      setPhase("success");
      setStatusMessage(event.message || "部署完成");
      if (!deployedFired) {
        deployedFired = true;
        onDeployedCb?.();
      }
      closeSocket(ws);
      break;
    case "error":
      setPhase("error");
      setStatusMessage(event.message || "部署失败");
      closeSocket(ws);
      break;
  }
}

/**
 * 启动一次部署：建立 WebSocket，onopen 发送首帧请求。
 * 连接与状态都在模块级持有，因此关闭对话框只是隐藏 UI，部署继续进行。
 * onDeployed 在成功时恰好触发一次，与对话框是否打开无关。
 */
function start(request: DeployRequest, onDeployed?: () => void) {
  if (phase() === "running") return; // 单部署假设：已有进行中则忽略

  closeSocket();

  setLogs([]);
  setRetryInfo(null);
  setCurrentStep("connecting");
  setStatusMessage("正在建立连接…");
  setPhase("running");
  deployedFired = false;
  onDeployedCb = onDeployed;

  let ws: WebSocket;
  try {
    ws = openDeployWebSocket();
  } catch (err) {
    setPhase("error");
    setStatusMessage(err instanceof Error ? err.message : "无法建立连接");
    return;
  }
  socket = ws;

  ws.onopen = () => {
    if (ws !== socket || phase() !== "running") return;
    try {
      ws.send(JSON.stringify(request));
    } catch {
      setPhase("error");
      setStatusMessage("部署请求发送失败");
      closeSocket(ws);
    }
  };
  ws.onmessage = (msg) => {
    if (ws !== socket) return;
    try {
      handleEvent(ws, JSON.parse(String(msg.data)) as DeployEvent);
    } catch {
      // 忽略无法解析的帧。
    }
  };
  ws.onerror = () => {
    if (ws === socket && phase() === "running") {
      setPhase("error");
      setStatusMessage("连接发生错误");
      closeSocket(ws);
    }
  };
  ws.onclose = () => {
    if (ws !== socket) return;
    socket = undefined;
    detachSocketHandlers(ws);
    if (phase() === "running") {
      setPhase("error");
      setStatusMessage("连接已断开，部署未完成");
    }
  };
}

/** 用户主动中止进行中的部署：断开连接并回到 idle。 */
function cancel() {
  closeSocket();
  reset();
}

/** 确认终态结果后清空，让入口回到「添加节点」。 */
function reset() {
  closeSocket();
  setPhase("idle");
  setStatusMessage("");
  setRetryInfo(null);
  setLogs([]);
  setCurrentStep("connecting");
  onDeployedCb = undefined;
  deployedFired = false;
}

export const deployStore = {
  phase,
  currentStep,
  statusMessage,
  retryInfo,
  logs,
  isActive: () => phase() !== "idle",
  isRunning: () => phase() === "running",
  start,
  cancel,
  reset,
};
