#!/usr/bin/env bash
#
# os-watcher 一键部署脚本
#
# 用法:
#   ./deploy.sh                  # 仅部署监控服务节点（默认，不含前端）
#   ./deploy.sh --web            # 部署监控服务节点 + 构建并托管前端面板
#   ./deploy.sh --web --port 7980 --gossip-port 7979
#   ./deploy.sh --peers 192.168.1.10:7979,192.168.1.11:7979
#
# 选项:
#   --web                启用 Web 前端面板（构建 SolidJS 项目并由节点托管）
#   --port <PORT>        REST API / Web 面板端口（默认 7980）
#   --gossip-port <PORT> Gossip UDP 端口（默认 7979）
#   --peers <LIST>       手动指定对等节点，逗号分隔的 host:port
#   --name <NAME>        节点名称（默认使用主机名）
#   --release            以 release 模式构建（默认 debug）
#   --install-service    安装为 systemd 服务（需要 root，仅 Linux）
#   -h, --help           显示帮助

set -euo pipefail

# ---- 默认参数 ----
ENABLE_WEB=0
API_PORT=7980
GOSSIP_PORT=7979
PEERS=""
NODE_NAME=""
BUILD_MODE="debug"
INSTALL_SERVICE=0

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WEB_DIR="$SCRIPT_DIR/web"
WEB_DIST="$WEB_DIR/dist"

# ---- 颜色输出 ----
c_info()  { printf '\033[36m[INFO]\033[0m %s\n' "$*"; }
c_ok()    { printf '\033[32m[ OK ]\033[0m %s\n' "$*"; }
c_warn()  { printf '\033[33m[WARN]\033[0m %s\n' "$*"; }
c_err()   { printf '\033[31m[FAIL]\033[0m %s\n' "$*" >&2; }

usage() {
  sed -n '2,40p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  exit 0
}

# ---- 解析参数 ----
while [[ $# -gt 0 ]]; do
  case "$1" in
    --web)             ENABLE_WEB=1; shift ;;
    --port)            API_PORT="$2"; shift 2 ;;
    --gossip-port)     GOSSIP_PORT="$2"; shift 2 ;;
    --peers)           PEERS="$2"; shift 2 ;;
    --name)            NODE_NAME="$2"; shift 2 ;;
    --release)         BUILD_MODE="release"; shift ;;
    --install-service) INSTALL_SERVICE=1; shift ;;
    -h|--help)         usage ;;
    *) c_err "未知参数: $1"; echo "使用 --help 查看用法"; exit 1 ;;
  esac
done

# ---- 前置检查 ----
require() {
  command -v "$1" >/dev/null 2>&1 || { c_err "缺少依赖: $1，请先安装"; exit 1; }
}

c_info "检查构建依赖…"
require cargo

# ---- 构建后端 ----
c_info "构建 os-watcher 监控节点（$BUILD_MODE 模式）…"
if [[ "$BUILD_MODE" == "release" ]]; then
  cargo build --release
  BIN="$SCRIPT_DIR/target/release/os-watcher"
else
  cargo build
  BIN="$SCRIPT_DIR/target/debug/os-watcher"
fi

[[ -x "$BIN" ]] || { c_err "构建产物未找到: $BIN"; exit 1; }
c_ok "后端构建完成: $BIN"

# ---- 构建前端（仅 --web） ----
if [[ "$ENABLE_WEB" -eq 1 ]]; then
  c_info "启用 Web 面板，开始构建前端…"
  require node
  require npm

  pushd "$WEB_DIR" >/dev/null
  if [[ ! -d node_modules ]]; then
    c_info "安装前端依赖（npm install）…"
    npm install
  fi
  c_info "构建前端产物（npm run build）…"
  npm run build
  popd >/dev/null

  [[ -f "$WEB_DIST/index.html" ]] || { c_err "前端构建产物缺失: $WEB_DIST/index.html"; exit 1; }
  c_ok "前端构建完成: $WEB_DIST"
fi

# ---- 组装启动命令 ----
ARGS=(start --api-port "$API_PORT" --gossip-port "$GOSSIP_PORT")
[[ -n "$PEERS" ]]     && ARGS+=(--peers "$PEERS")
if [[ "$ENABLE_WEB" -eq 1 ]]; then
  ARGS+=(--web --web-dir "$WEB_DIST")
fi

# 节点名通过配置或环境变量传递；这里用环境变量覆盖主机名（可选）
[[ -n "$NODE_NAME" ]] && export OS_WATCHER_NODE_NAME="$NODE_NAME"

# ---- 安装为 systemd 服务（可选） ----
if [[ "$INSTALL_SERVICE" -eq 1 ]]; then
  if [[ "$(uname -s)" != "Linux" ]]; then
    c_err "--install-service 仅支持 Linux"; exit 1
  fi
  if [[ "$(id -u)" -ne 0 ]]; then
    c_err "安装 systemd 服务需要 root 权限（请用 sudo）"; exit 1
  fi

  SERVICE_FILE="/etc/systemd/system/os-watcher.service"
  c_info "写入 systemd 服务: $SERVICE_FILE"
  cat > "$SERVICE_FILE" <<EOF
[Unit]
Description=os-watcher 去中心化主机监控节点
After=network.target

[Service]
Type=simple
ExecStart=$BIN ${ARGS[*]}
WorkingDirectory=$SCRIPT_DIR
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF

  systemctl daemon-reload
  systemctl enable os-watcher
  systemctl restart os-watcher
  c_ok "服务已安装并启动: systemctl status os-watcher"
  exit 0
fi

# ---- 直接前台启动 ----
c_info "启动 os-watcher…"
c_info "  API 端口:    $API_PORT"
c_info "  Gossip 端口: $GOSSIP_PORT"
if [[ "$ENABLE_WEB" -eq 1 ]]; then
  c_ok  "  Web 面板:    http://localhost:$API_PORT/"
else
  c_info "  Web 面板:    未启用（使用 --web 开启）"
fi
[[ -n "$PEERS" ]] && c_info "  对等节点:    $PEERS"

exec "$BIN" "${ARGS[@]}"
