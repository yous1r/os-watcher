import type {
  Alert,
  AlertRetention,
  ApiResponse,
  NodeSnapshot,
  NotifyChannel,
  NotifyChannelPayload,
  NotifyDefaults,
  SessionInfo,
  UninstallRequest,
  UninstallResult,
  UpgradeRequest,
  UpgradeStatus,
  VersionInfo,
} from "./types";

const API_BASE = import.meta.env.VITE_API_BASE ?? "/api/v1";

/** 带 HTTP 状态码的接口错误，调用方据此区分 401（需登录）等情况。 */
export class ApiError extends Error {
  readonly status: number;

  constructor(status: number, message: string) {
    super(message);
    this.name = "ApiError";
    this.status = status;
  }
}

async function readApiResponse<T>(resp: Response, path: string): Promise<T> {
  let body: ApiResponse<T> | null = null;
  try {
    body = (await resp.json()) as ApiResponse<T>;
  } catch {
    if (!resp.ok) {
      throw new ApiError(resp.status, `HTTP ${resp.status} for ${path}`);
    }
  }

  if (!resp.ok) {
    throw new ApiError(resp.status, body?.error ?? `HTTP ${resp.status} for ${path}`);
  }
  if (!body?.success) {
    throw new ApiError(resp.status, body?.error ?? `API error for ${path}`);
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

async function putJson<T>(
  path: string,
  payload: unknown,
  base = API_BASE
): Promise<T> {
  const resp = await fetch(`${base}${path}`, {
    method: "PUT",
    headers: {
      Accept: "application/json",
      "Content-Type": "application/json",
    },
    body: JSON.stringify(payload),
  });
  return readApiResponse<T>(resp, path);
}

async function deleteJson<T>(path: string, base = API_BASE): Promise<T> {
  const resp = await fetch(`${base}${path}`, {
    method: "DELETE",
    headers: { Accept: "application/json" },
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

/** 拉取最近消除的告警（默认 50 条）。 */
export function fetchAlertsHistory(limit = 50): Promise<Alert[]> {
  return getJson<Alert[]>(`/alerts/history?limit=${limit}`);
}

/** 拉取「最近恢复」的保留时长（分钟），由服务端 [storage] alert_history_minutes 决定。 */
export function fetchAlertRetention(): Promise<AlertRetention> {
  return getJson<AlertRetention>("/alerts/retention");
}

/** 查询管理鉴权状态：是否需要登录、当前是否已登录。 */
export function fetchSession(): Promise<SessionInfo> {
  return getJson<SessionInfo>("/auth/session");
}

/** 用管理口令换取会话 Cookie。 */
export async function login(password: string): Promise<void> {
  await postJson<{ authenticated: boolean }>("/auth/login", { password });
}

/** 注销当前管理会话。 */
export async function logout(): Promise<void> {
  await postJson<{ authenticated: boolean }>("/auth/logout", {});
}

/** 拉取全部推送渠道（仅管理员）。 */
export function fetchNotifyChannels(): Promise<NotifyChannel[]> {
  return getJson<NotifyChannel[]>("/notify/channels");
}

/** 拉取服务端配置的推送默认值（仅管理员）。 */
export function fetchNotifyDefaults(): Promise<NotifyDefaults> {
  return getJson<NotifyDefaults>("/notify/defaults");
}

export function createNotifyChannel(
  payload: NotifyChannelPayload
): Promise<NotifyChannel> {
  return postJson<NotifyChannel>("/notify/channels", payload);
}

export function updateNotifyChannel(
  id: string,
  payload: NotifyChannelPayload
): Promise<NotifyChannel> {
  return putJson<NotifyChannel>(`/notify/channels/${id}`, payload);
}

export async function deleteNotifyChannel(id: string): Promise<void> {
  await deleteJson<{ deleted: boolean }>(`/notify/channels/${id}`);
}

/** 向指定渠道发送一条测试推送。 */
export async function testNotifyChannel(id: string): Promise<void> {
  await postJson<{ sent: boolean }>(`/notify/channels/${id}/test`, {});
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

/**
 * 卸载本机安装。仅管理员可调用：后端会删除服务注册与安装目录。
 * 请求成功即代表卸载程序已启动，服务随后停止，因此没有可轮询的完成状态。
 */
export function triggerUninstall(request: UninstallRequest): Promise<UninstallResult> {
  return postJson<UninstallResult>("/uninstall", request);
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
