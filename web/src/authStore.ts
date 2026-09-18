import { createSignal } from "solid-js";
import {
  fetchSession,
  login as loginRequest,
  logout as logoutRequest,
} from "./api";
import type { SessionInfo } from "./types";

/**
 * 管理鉴权状态。与 deployStore 一样是模块级 store：登录对话框挂在 App 上，
 * 任何组件都可以直接调用守卫与登录入口。
 *
 * session 首次查询前为 null（未知）。未知必须按「不可管理」处理，否则管理
 * 组件会在拿到真实鉴权状态之前就发请求，收到 401 后反而弹出登录框。
 */
const [session, setSession] = createSignal<SessionInfo | null>(null);
const [loginOpen, setLoginOpen] = createSignal(false);

// 刷新失败（后端暂时不可达）时保留上次状态，避免把已登录的用户踢回访客态。
async function refresh() {
  try {
    setSession(await fetchSession());
  } catch {
    // 保留上次状态。
  }
}

function openLogin() {
  setLoginOpen(true);
}

function closeLogin() {
  setLoginOpen(false);
}

/** 后端是否要求登录；查询完成前返回 false（顶栏不显示鉴权状态）。 */
function authRequired(): boolean {
  return session()?.auth_required ?? false;
}

function authenticated(): boolean {
  return session()?.authenticated ?? false;
}

/**
 * 只读判定：开放模式或已登录时为 true。鉴权状态未知时返回 false（宁可少发
 * 一个管理请求，也不要在登录状态确认前触达管理接口）。不会弹出登录框。
 */
function canManage(): boolean {
  const current = session();
  if (!current) return false;
  return !current.auth_required || current.authenticated;
}

/**
 * 管理动作入口守卫：可管理时返回 true；否则弹出登录框并返回 false。
 * 调用方据此决定是否继续，例如 `if (!authStore.allowed()) return;`。
 */
function allowed(): boolean {
  if (canManage()) return true;
  openLogin();
  return false;
}

/** 任意管理接口返回 401 时调用：标记未登录并弹出登录框。 */
function handleUnauthorized() {
  setSession({ auth_required: true, authenticated: false });
  openLogin();
}

async function login(password: string) {
  await loginRequest(password);
  await refresh();
  closeLogin();
}

async function logout() {
  await logoutRequest();
  await refresh();
}

export const authStore = {
  session,
  loginOpen,
  authRequired,
  authenticated,
  refresh,
  openLogin,
  closeLogin,
  canManage,
  allowed,
  handleUnauthorized,
  login,
  logout,
};
