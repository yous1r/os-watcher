import type { NodeSnapshot, Alert, ApiResponse } from "./types";

const API_BASE = import.meta.env.VITE_API_BASE ?? "/api/v1";

async function getJson<T>(path: string): Promise<T> {
  const resp = await fetch(`${API_BASE}${path}`, {
    headers: { Accept: "application/json" },
  });
  if (!resp.ok) {
    throw new Error(`HTTP ${resp.status} for ${path}`);
  }
  const body = (await resp.json()) as ApiResponse<T>;
  if (!body.success) {
    throw new Error(`API error for ${path}`);
  }
  return body.data;
}

/** 拉取所有节点的最新快照（含指标）。 */
export function fetchSnapshots(): Promise<NodeSnapshot[]> {
  return getJson<NodeSnapshot[]>("/metrics");
}

/** 拉取当前活动告警。 */
export function fetchAlerts(): Promise<Alert[]> {
  return getJson<Alert[]>("/alerts");
}

export { API_BASE };
