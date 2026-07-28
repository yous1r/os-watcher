// os-watcher web dashboard
// Polls the agent's REST API and renders node overview, detail, and alerts.

const API_BASE = "/api/v1";
const REFRESH_MS = 3000;

let state = {
  snapshots: [],   // from /metrics : [{ info, metrics }]
  alerts: [],      // from /alerts
  selectedNode: null,
  view: "overview",
  connected: false,
};

// ---- Utilities ----

function fmtBytes(bytes) {
  const GB = 1073741824;
  const MB = 1048576;
  if (bytes >= GB) return (bytes / GB).toFixed(1) + " GB";
  if (bytes >= MB) return (bytes / MB).toFixed(1) + " MB";
  return (bytes / 1024).toFixed(0) + " KB";
}

function fmtDuration(secs) {
  const d = Math.floor(secs / 86400);
  const h = Math.floor((secs % 86400) / 3600);
  const m = Math.floor((secs % 3600) / 60);
  if (d > 0) return `${d}天 ${h}小时`;
  if (h > 0) return `${h}小时 ${m}分`;
  return `${m}分`;
}

function usageClass(pct) {
  if (pct >= 90) return "crit";
  if (pct >= 70) return "warn";
  return "ok";
}

function maxDiskUsage(metrics) {
  if (!metrics || !metrics.disks || metrics.disks.length === 0) return 0;
  return metrics.disks.reduce((mx, d) => Math.max(mx, d.usage_percent), 0);
}

function escapeHtml(s) {
  const div = document.createElement("div");
  div.textContent = s == null ? "" : String(s);
  return div.innerHTML;
}

// ---- Data fetching ----

async function fetchJson(path) {
  const resp = await fetch(API_BASE + path, { cache: "no-store" });
  if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
  const body = await resp.json();
  return body.data !== undefined ? body.data : body;
}

async function poll() {
  try {
    const [snapshots, alerts] = await Promise.all([
      fetchJson("/metrics"),
      fetchJson("/alerts"),
    ]);
    state.snapshots = Array.isArray(snapshots) ? snapshots : [];
    state.alerts = Array.isArray(alerts) ? alerts : [];
    state.connected = true;

    // Keep a valid selection
    if (
      state.selectedNode === null ||
      !state.snapshots.some((s) => s.info.id === state.selectedNode)
    ) {
      state.selectedNode = state.snapshots.length
        ? state.snapshots[0].info.id
        : null;
    }
  } catch (e) {
    state.connected = false;
  }
  render();
}

// ---- Rendering ----

function render() {
  renderStatusLine();
  if (state.view === "overview") renderOverview();
  else if (state.view === "detail") renderDetail();
  else if (state.view === "alerts") renderAlerts();
}

function renderStatusLine() {
  const dot = document.getElementById("conn-dot");
  const text = document.getElementById("conn-text");
  if (state.connected) {
    dot.className = "dot dot-online";
    text.textContent = "已连接";
  } else {
    dot.className = "dot dot-offline";
    text.textContent = "连接失败";
  }
  document.getElementById("node-count").textContent = state.snapshots.length;
  const activeAlerts = state.alerts.filter((a) => !a.resolved_at);
  const badge = document.getElementById("alert-count");
  badge.textContent = activeAlerts.length;
  badge.classList.toggle("has-alerts", activeAlerts.length > 0);
  document.getElementById("last-update").textContent =
    new Date().toLocaleTimeString("zh-CN", { hour12: false });
}

function gauge(label, pct, sub) {
  const cls = usageClass(pct);
  const p = Math.min(100, Math.max(0, pct));
  return `
    <div class="metric">
      <div class="metric-head">
        <span class="metric-label">${label}</span>
        <span class="metric-val ${cls}">${pct.toFixed(1)}%</span>
      </div>
      <div class="bar"><div class="bar-fill ${cls}" style="width:${p}%"></div></div>
      ${sub ? `<div class="metric-sub">${sub}</div>` : ""}
    </div>`;
}

function renderOverview() {
  const grid = document.getElementById("node-grid");
  const empty = document.getElementById("empty-overview");

  if (state.snapshots.length === 0) {
    grid.innerHTML = "";
    empty.style.display = "block";
    return;
  }
  empty.style.display = "none";

  grid.innerHTML = state.snapshots
    .map((snap) => {
      const info = snap.info;
      const m = snap.metrics;
      const statusCls =
        info.status === "Online"
          ? "online"
          : info.status === "Offline"
          ? "offline"
          : "unknown";

      if (!m) {
        return `
          <div class="node-card">
            <div class="node-card-head">
              <span class="dot dot-${statusCls}"></span>
              <span class="node-name">${escapeHtml(info.hostname)}</span>
            </div>
            <div class="node-addr">${escapeHtml(info.api_addr)}</div>
            <div class="empty-small">暂无指标</div>
          </div>`;
      }

      const disk = maxDiskUsage(m);
      const memSub = `${fmtBytes(m.memory.used_bytes)} / ${fmtBytes(m.memory.total_bytes)}`;
      return `
        <div class="node-card clickable" data-node="${info.id}">
          <div class="node-card-head">
            <span class="dot dot-${statusCls}"></span>
            <span class="node-name">${escapeHtml(info.hostname)}</span>
            <span class="node-uptime">↑ ${fmtDuration(m.uptime_seconds)}</span>
          </div>
          <div class="node-addr">${escapeHtml(m.os_name || info.api_addr)}</div>
          ${gauge("CPU", m.cpu.usage_percent, `${m.cpu.core_count} 核`)}
          ${gauge("内存", m.memory.usage_percent, memSub)}
          ${gauge("磁盘", disk, `${m.disks.length} 个挂载点`)}
        </div>`;
    })
    .join("");

  grid.querySelectorAll(".node-card.clickable").forEach((card) => {
    card.addEventListener("click", () => {
      state.selectedNode = card.dataset.node;
      switchView("detail");
    });
  });
}

function renderDetail() {
  const select = document.getElementById("node-select");
  const body = document.getElementById("detail-body");

  // Populate selector
  select.innerHTML = state.snapshots
    .map(
      (s) =>
        `<option value="${s.info.id}" ${
          s.info.id === state.selectedNode ? "selected" : ""
        }>${escapeHtml(s.info.hostname)}</option>`
    )
    .join("");

  const snap = state.snapshots.find((s) => s.info.id === state.selectedNode);
  if (!snap) {
    body.innerHTML = `<div class="empty">未选择节点</div>`;
    return;
  }
  const m = snap.metrics;
  if (!m) {
    body.innerHTML = `<div class="empty">节点 ${escapeHtml(
      snap.info.hostname
    )} 暂无指标数据</div>`;
    return;
  }

  const cores = m.cpu.core_usages
    .map(
      (u, i) =>
        `<div class="core"><span class="core-idx">#${i}</span>
         <div class="bar mini"><div class="bar-fill ${usageClass(
           u
         )}" style="width:${Math.min(100, u)}%"></div></div>
         <span class="core-val">${u.toFixed(0)}%</span></div>`
    )
    .join("");

  const disks = m.disks
    .map(
      (d) => `
      <tr>
        <td>${escapeHtml(d.mount_point)}</td>
        <td>${escapeHtml(d.fs_type)}</td>
        <td>${fmtBytes(d.used_bytes)} / ${fmtBytes(d.total_bytes)}</td>
        <td><div class="bar mini"><div class="bar-fill ${usageClass(
          d.usage_percent
        )}" style="width:${Math.min(100, d.usage_percent)}%"></div></div></td>
        <td class="${usageClass(d.usage_percent)}">${d.usage_percent.toFixed(1)}%</td>
      </tr>`
    )
    .join("");

  const procs = m.top_processes
    .map(
      (p) => `
      <tr>
        <td>${p.pid}</td>
        <td>${escapeHtml(p.name)}</td>
        <td class="${p.cpu_usage > 50 ? "crit" : p.cpu_usage > 20 ? "warn" : "ok"}">${p.cpu_usage.toFixed(
        1
      )}%</td>
        <td>${fmtBytes(p.memory_bytes)}</td>
      </tr>`
    )
    .join("");

  const nets = m.networks
    .map(
      (n) => `
      <tr>
        <td>${escapeHtml(n.name)}</td>
        <td>↓ ${fmtBytes(n.total_received_bytes)}</td>
        <td>↑ ${fmtBytes(n.total_transmitted_bytes)}</td>
      </tr>`
    )
    .join("");

  const la = m.load_average;
  const loadStr = la
    ? `${la.one.toFixed(2)} / ${la.five.toFixed(2)} / ${la.fifteen.toFixed(2)}`
    : "N/A";

  body.innerHTML = `
    <div class="detail-grid">
      <div class="panel">
        <h3>概要</h3>
        <div class="kv"><span>主机名</span><b>${escapeHtml(m.hostname)}</b></div>
        <div class="kv"><span>系统</span><b>${escapeHtml(m.os_name)}</b></div>
        <div class="kv"><span>运行时长</span><b>${fmtDuration(m.uptime_seconds)}</b></div>
        <div class="kv"><span>负载 (1/5/15)</span><b>${loadStr}</b></div>
        <div class="kv"><span>API 地址</span><b>${escapeHtml(snap.info.api_addr)}</b></div>
      </div>
      <div class="panel">
        <h3>CPU · ${m.cpu.usage_percent.toFixed(1)}% · ${m.cpu.core_count} 核</h3>
        <div class="cores">${cores}</div>
      </div>
      <div class="panel">
        <h3>内存</h3>
        ${gauge("物理内存", m.memory.usage_percent, `${fmtBytes(m.memory.used_bytes)} / ${fmtBytes(m.memory.total_bytes)}`)}
        ${
          m.memory.swap_total_bytes > 0
            ? gauge(
                "Swap",
                (m.memory.swap_used_bytes / m.memory.swap_total_bytes) * 100,
                `${fmtBytes(m.memory.swap_used_bytes)} / ${fmtBytes(m.memory.swap_total_bytes)}`
              )
            : ""
        }
      </div>
    </div>

    <div class="panel">
      <h3>磁盘</h3>
      <table class="data-table">
        <thead><tr><th>挂载点</th><th>类型</th><th>使用</th><th></th><th>占用</th></tr></thead>
        <tbody>${disks || '<tr><td colspan="5" class="empty-small">无</td></tr>'}</tbody>
      </table>
    </div>

    <div class="detail-grid">
      <div class="panel">
        <h3>Top 进程</h3>
        <table class="data-table">
          <thead><tr><th>PID</th><th>名称</th><th>CPU</th><th>内存</th></tr></thead>
          <tbody>${procs || '<tr><td colspan="4" class="empty-small">无</td></tr>'}</tbody>
        </table>
      </div>
      <div class="panel">
        <h3>网络接口</h3>
        <table class="data-table">
          <thead><tr><th>接口</th><th>累计接收</th><th>累计发送</th></tr></thead>
          <tbody>${nets || '<tr><td colspan="3" class="empty-small">无</td></tr>'}</tbody>
        </table>
      </div>
    </div>`;
}

function renderAlerts() {
  const list = document.getElementById("alerts-list");
  const empty = document.getElementById("empty-alerts");
  const active = state.alerts.filter((a) => !a.resolved_at);

  if (active.length === 0) {
    list.innerHTML = "";
    empty.style.display = "block";
    return;
  }
  empty.style.display = "none";

  const sevCls = { Critical: "crit", Warning: "warn", Info: "ok" };
  const sevText = { Critical: "严重", Warning: "警告", Info: "提示" };

  list.innerHTML = active
    .map((a) => {
      const node = state.snapshots.find((s) => s.info.id === a.node_id);
      const host = node ? node.info.hostname : a.node_id;
      const t = new Date(a.triggered_at).toLocaleString("zh-CN", { hour12: false });
      return `
        <div class="alert-item ${sevCls[a.severity] || "warn"}">
          <span class="alert-sev">${sevText[a.severity] || a.severity}</span>
          <div class="alert-main">
            <div class="alert-title">${escapeHtml(a.rule_name)} · ${escapeHtml(host)}</div>
            <div class="alert-msg">${escapeHtml(a.message)}</div>
          </div>
          <span class="alert-time">${t}</span>
        </div>`;
    })
    .join("");
}

// ---- View switching ----

function switchView(view) {
  state.view = view;
  document.querySelectorAll(".tab").forEach((t) => {
    t.classList.toggle("active", t.dataset.view === view);
  });
  document.querySelectorAll(".view").forEach((v) => {
    v.classList.toggle("active", v.id === "view-" + view);
  });
  render();
}

// ---- Init ----

document.addEventListener("DOMContentLoaded", () => {
  document.getElementById("refresh-secs").textContent = String(REFRESH_MS / 1000);
  document.getElementById("api-base").textContent = API_BASE;

  document.querySelectorAll(".tab").forEach((tab) => {
    tab.addEventListener("click", () => switchView(tab.dataset.view));
  });

  document.getElementById("node-select").addEventListener("change", (e) => {
    state.selectedNode = e.target.value;
    renderDetail();
  });

  poll();
  setInterval(poll, REFRESH_MS);
});
