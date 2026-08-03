import type {
  Alert,
  ApiResponse,
  NodeSnapshot,
  UpgradeRequest,
  UpgradeStatus,
  VersionInfo,
} from "./types";

const API_BASE = import.meta.env.VITE_API_BASE ?? "/api/v1";

async function readApiResponse<T>(resp: Response, path: string): Promise<T> {
  let body: ApiResponse<T> | null = null;
  try {
    body = (await resp.json()) as ApiResponse<T>;
  } catch {
    if (!resp.ok) {
      throw new Error(`HTTP ${resp.status} for ${path}`);
    }
  }

  if (!resp.ok) {
    throw new Error(body?.error ?? `HTTP ${resp.status} for ${path}`);
  }
  if (!body?.success) {
    throw new Error(body?.error ?? `API error for ${path}`);
  }
  return body.data;
}

async function getJson<T>(path: string, base = API_BASE): Promise<T> {
  const resp = await fetch(`${base}${path}`, {
    headers: { Accept: "application/json" },
  });
  return readApiResponse<T>(resp, path);
}

async function postJson<T>(
  path: string,
  payload: unknown,
  base = API_BASE
): Promise<T> {
  const resp = await fetch(`${base}${path}`, {
    method: "POST",
    headers: {
      Accept: "application/json",
      "Content-Type": "application/json",
    },
    body: JSON.stringify(payload),
  });
  return readApiResponse<T>(resp, path);
}

/** 拉取所有节点的最新快照（含指标）。 */
export function fetchSnapshots(): Promise<NodeSnapshot[]> {
  return getJson<NodeSnapshot[]>("/metrics");
}

/** 拉取本机节点信息（含 gossip 地址），用于部署向导预填 peers。 */
export function fetchLocal(): Promise<NodeSnapshot> {
  return getJson<NodeSnapshot>("/local");
}

/** 拉取当前活动告警。 */
export function fetchAlerts(): Promise<Alert[]> {
  return getJson<Alert[]>("/alerts");
}

/** 拉取当前节点的版本检测与升级状态。 */
export function fetchVersion(): Promise<VersionInfo> {
  return getJson<VersionInfo>("/version");
}

/** 向指定节点发起自升级请求。 */
export function triggerNodeUpgrade(
  apiAddr: string,
  request: UpgradeRequest
): Promise<UpgradeStatus> {
  return postJson<UpgradeStatus>("/upgrade", request, apiBaseForNode(apiAddr));
}

/** 拉取指定节点的自升级状态。 */
export function fetchNodeUpgradeStatus(apiAddr: string): Promise<UpgradeStatus> {
  return getJson<UpgradeStatus>("/upgrade", apiBaseForNode(apiAddr));
}

function apiBaseForNode(apiAddr: string): string {
  const withScheme = /^https?:\/\//i.test(apiAddr)
    ? apiAddr
    : `http://${apiAddr}`;
  const url = new URL(withScheme);
  if (url.hostname === "0.0.0.0" || url.hostname === "::") {
    url.hostname = window.location.hostname;
  }
  return `${url.origin}/api/v1`;
}

/** 计算部署 WebSocket 端点的绝对 URL，复用 API_BASE 的 host 解析思路。 */
function deployWebSocketUrl(): string {
  // API_BASE 可能是绝对地址（VITE_API_BASE）或相对路径（默认 /api/v1）。
  if (/^https?:\/\//i.test(API_BASE)) {
    const url = new URL(API_BASE);
    if (url.hostname === "0.0.0.0" || url.hostname === "::") {
      url.hostname = window.location.hostname;
    }
    url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
    url.pathname = `${url.pathname.replace(/\/$/, "")}/nodes/deploy`;
    url.search = "";
    url.hash = "";
    return url.toString();
  }
  const proto = window.location.protocol === "https:" ? "wss:" : "ws:";
  const base = API_BASE.replace(/\/$/, "");
  return `${proto}//${window.location.host}${base}/nodes/deploy`;
}

/** 建立部署 WebSocket 连接；调用方负责在 onopen 时发送 DeployRequest 首帧。 */
export function openDeployWebSocket(): WebSocket {
  return new WebSocket(deployWebSocketUrl());
}

export { API_BASE };
