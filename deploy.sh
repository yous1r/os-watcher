#!/usr/bin/env bash
#
# os-watcher Release 包部署脚本
#
# 用法：
#   sudo ./deploy.sh --package node
#   sudo ./deploy.sh --package full --port 7980 --gossip-port 7979
#   sudo GITHUB_REPO=yous1r/os-watcher HTTPS_PROXY=http://127.0.0.1:7890 ./deploy.sh --package full
#
# Windows 请在「以管理员身份运行」的 Git Bash / MSYS2 中执行。
#
# 选项：
#   --package <node|full>      安装包类型，默认 node
#   --version <tag|latest>     Release 版本，默认 latest
#   --repo <owner/repo>        GitHub 仓库，默认 yous1r/os-watcher
#   --platform <name>          覆盖自动平台检测
#   --port <PORT>              REST API / Web 面板端口，默认 7980
#   --gossip-port <PORT>       Gossip UDP 端口，默认 7979
#   --peers <LIST>             对等节点列表，逗号分隔 host:port
#   --service-name <NAME>      服务名，默认 os-watcher
#   --proxy <URL>              下载代理；也可使用 HTTP_PROXY/HTTPS_PROXY
#   --force                    跳过确认直接覆盖安装
#   -h, --help                 显示帮助

set -Eeuo pipefail

GITHUB_REPO="${GITHUB_REPO:-yous1r/os-watcher}"
PACKAGE="node"
VERSION="latest"
PLATFORM=""
API_PORT=7980
GOSSIP_PORT=7979
PEERS=""
SERVICE_NAME="os-watcher"
PROXY=""
FORCE=0

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
cd "$SCRIPT_DIR"

TMP_DIR=""

c_info() { printf '\033[36m[INFO]\033[0m %s\n' "$*"; }
c_ok() { printf '\033[32m[ OK ]\033[0m %s\n' "$*"; }
c_warn() { printf '\033[33m[WARN]\033[0m %s\n' "$*"; }
c_err() { printf '\033[31m[FAIL]\033[0m %s\n' "$*" >&2; }
fail() { c_err "$*"; exit 1; }

usage() {
  sed -n '2,28p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  exit 0
}

cleanup() {
  if [[ -n "$TMP_DIR" && -d "$TMP_DIR" ]]; then
    rm -rf "$TMP_DIR"
  fi
}
trap cleanup EXIT

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "缺少依赖：$1"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --package)
      PACKAGE="${2:-}"; shift 2 ;;
    --version)
      VERSION="${2:-}"; shift 2 ;;
    --repo)
      GITHUB_REPO="${2:-}"; shift 2 ;;
    --platform)
      PLATFORM="${2:-}"; shift 2 ;;
    --port)
      API_PORT="${2:-}"; shift 2 ;;
    --gossip-port)
      GOSSIP_PORT="${2:-}"; shift 2 ;;
    --peers)
      PEERS="${2:-}"; shift 2 ;;
    --service-name)
      SERVICE_NAME="${2:-}"; shift 2 ;;
    --proxy)
      PROXY="${2:-}"; shift 2 ;;
    --force)
      FORCE=1; shift ;;
    -h|--help)
      usage ;;
    *)
      fail "未知参数：$1；使用 --help 查看用法" ;;
  esac
done

[[ "$PACKAGE" == "node" || "$PACKAGE" == "full" ]] ||
  fail "--package 只能是 node 或 full"
[[ -n "$VERSION" ]] || fail "--version 不能为空"
[[ "$GITHUB_REPO" == */* ]] || fail "--repo 必须是 owner/repo 格式"

is_windows() {
  case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*) return 0 ;;
    *) return 1 ;;
  esac
}

require_privilege() {
  if is_windows; then
    require_cmd powershell.exe
    local is_admin
    is_admin="$(powershell.exe -NoProfile -Command "([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)" | tr -d '\r')"
    [[ "$is_admin" == "True" ]] || fail "Windows 部署需要管理员权限，请以管理员身份运行终端"
  else
    [[ "$(id -u)" -eq 0 ]] || fail "Linux 部署需要 root 权限，请使用 sudo"
  fi
}

detect_platform() {
  if is_windows; then
    case "$(uname -m)" in
      x86_64|amd64|AMD64) printf '%s\n' "windows-x86_64" ;;
      *) fail "不支持的 Windows 架构：$(uname -m)" ;;
    esac
    return
  fi

  [[ "$(uname -s)" == "Linux" ]] || fail "当前脚本仅支持 Linux 和 Windows"
  case "$(uname -m)" in
    x86_64|amd64)
      if ldd --version 2>&1 | grep -qi musl; then
        printf '%s\n' "linux-x86_64-musl"
      else
        printf '%s\n' "linux-x86_64"
      fi
      ;;
    aarch64|arm64)
      printf '%s\n' "linux-aarch64" ;;
    *)
      fail "不支持的 Linux 架构：$(uname -m)" ;;
  esac
}

asset_extension() {
  case "$1" in
    windows-*) printf '%s\n' "zip" ;;
    *) printf '%s\n' "tar.gz" ;;
  esac
}

release_asset_url() {
  local asset="$1"
  if [[ "$VERSION" == "latest" ]]; then
    printf 'https://github.com/%s/releases/latest/download/%s\n' "$GITHUB_REPO" "$asset"
  else
    printf 'https://github.com/%s/releases/download/%s/%s\n' "$GITHUB_REPO" "$VERSION" "$asset"
  fi
}

download_file() {
  local url="$1"
  local output="$2"

  if [[ -n "$PROXY" ]]; then
    export HTTP_PROXY="$PROXY"
    export HTTPS_PROXY="$PROXY"
  fi

  if command -v curl >/dev/null 2>&1; then
    curl -fL --retry 3 --retry-delay 2 --connect-timeout 20 -o "$output" "$url"
  elif command -v wget >/dev/null 2>&1; then
    wget --tries=3 --timeout=120 -O "$output" "$url"
  else
    fail "缺少下载工具：请安装 curl 或 wget"
  fi
}

verify_sha256_if_present() {
  local archive="$1"
  local sha_file="$2"

  [[ -s "$sha_file" ]] || return 0

  if command -v sha256sum >/dev/null 2>&1; then
    (cd "$(dirname "$archive")" && sha256sum -c "$(basename "$sha_file")")
  elif is_windows && command -v certutil.exe >/dev/null 2>&1; then
    local expected actual
    expected="$(awk '{print tolower($1)}' "$sha_file")"
    actual="$(certutil.exe -hashfile "$(to_windows_path "$archive")" SHA256 | tr -d '\r' | awk 'NR==2 {print tolower($0)}')"
    [[ "$expected" == "$actual" ]] || fail "SHA256 校验失败：$archive"
  else
    c_warn "未找到 sha256sum/certutil，跳过校验"
  fi
}

extract_archive() {
  local archive="$1"
  local dest="$2"
  mkdir -p "$dest"

  case "$archive" in
    *.tar.gz)
      require_cmd tar
      tar -xzf "$archive" -C "$dest" ;;
    *.zip)
      if command -v unzip >/dev/null 2>&1; then
        unzip -q "$archive" -d "$dest"
      elif is_windows; then
        powershell.exe -NoProfile -ExecutionPolicy Bypass -Command "Expand-Archive -LiteralPath '$(to_windows_path "$archive")' -DestinationPath '$(to_windows_path "$dest")' -Force"
      else
        fail "缺少 unzip，无法解压 zip 包"
      fi
      ;;
    *)
      fail "不支持的归档格式：$archive" ;;
  esac
}

payload_root() {
  local extract_dir="$1"
  local root
  root="$(find "$extract_dir" -mindepth 1 -maxdepth 1 -type d | sort | head -n 1)"
  [[ -n "$root" ]] || fail "Release 包内未找到负载目录"
  printf '%s\n' "$root"
}

to_windows_path() {
  if command -v cygpath >/dev/null 2>&1; then
    cygpath -aw "$1"
  else
    printf '%s\n' "$1"
  fi
}

backup_current_install() {
  local bin_name="$1"
  local backup_dir="$SCRIPT_DIR/backups/deploy-$(date +%Y%m%d%H%M%S)"
  mkdir -p "$backup_dir"

  for item in "$bin_name" config.toml config.example.toml web-dist; do
    if [[ -e "$SCRIPT_DIR/$item" ]]; then
      cp -a "$SCRIPT_DIR/$item" "$backup_dir/"
    fi
  done

  c_ok "已备份当前部署文件：$backup_dir"
}

install_payload() {
  local root="$1"
  local bin_name="$2"

  [[ -f "$root/$bin_name" ]] || fail "Release 包缺少可执行文件：$bin_name"

  cp -f "$root/$bin_name" "$SCRIPT_DIR/$bin_name"
  if ! is_windows; then
    chmod 0755 "$SCRIPT_DIR/$bin_name"
  fi

  for item in README.md deploy.sh config.example.toml; do
    if [[ -f "$root/$item" ]]; then
      cp -f "$root/$item" "$SCRIPT_DIR/$item"
    fi
  done

  if [[ ! -f "$SCRIPT_DIR/config.toml" && -f "$SCRIPT_DIR/config.example.toml" ]]; then
    cp "$SCRIPT_DIR/config.example.toml" "$SCRIPT_DIR/config.toml"
    c_ok "已创建默认配置：$SCRIPT_DIR/config.toml"
  else
    c_info "保留现有 config.toml，仅更新 config.example.toml"
  fi

  if [[ -d "$root/web-dist" ]]; then
    rm -rf "$SCRIPT_DIR/web-dist"
    cp -a "$root/web-dist" "$SCRIPT_DIR/web-dist"
  fi

  c_ok "Release 包已安装到：$SCRIPT_DIR"
}

write_linux_service() {
  local bin_path="$SCRIPT_DIR/os-watcher"
  local config_path="$SCRIPT_DIR/config.toml"
  local service_file="/etc/systemd/system/${SERVICE_NAME}.service"
  local peers_arg=""
  if [[ -n "$PEERS" ]]; then
    peers_arg=" --peers ${PEERS}"
  fi

  cat > "$service_file" <<EOF
[Unit]
Description=os-watcher decentralized host monitor
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory=$SCRIPT_DIR
ExecStart="$bin_path" --config "$config_path" start --api-port $API_PORT --gossip-port $GOSSIP_PORT$peers_arg
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF

  systemctl daemon-reload
  systemctl enable "$SERVICE_NAME"
  systemctl restart "$SERVICE_NAME"
  c_ok "systemd 服务已启动：systemctl status $SERVICE_NAME"
}

install_windows_service() {
  local bin_win config_win
  bin_win="$(to_windows_path "$SCRIPT_DIR/os-watcher.exe")"
  config_win="$(to_windows_path "$SCRIPT_DIR/config.toml")"

  if command -v nssm.exe >/dev/null 2>&1; then
    nssm.exe stop "$SERVICE_NAME" >/dev/null 2>&1 || true
    nssm.exe remove "$SERVICE_NAME" confirm >/dev/null 2>&1 || true
    nssm.exe install "$SERVICE_NAME" "$bin_win" --config "$config_win" start --api-port "$API_PORT" --gossip-port "$GOSSIP_PORT" ${PEERS:+--peers "$PEERS"}
    nssm.exe set "$SERVICE_NAME" AppDirectory "$(to_windows_path "$SCRIPT_DIR")" >/dev/null
    nssm.exe set "$SERVICE_NAME" Start SERVICE_AUTO_START >/dev/null
    nssm.exe start "$SERVICE_NAME"
    c_ok "Windows 服务已通过 NSSM 启动：$SERVICE_NAME"
  else
    fail "Windows 自升级依赖可由 sc.exe 管理的服务；请安装 nssm.exe 后重跑脚本"
  fi
}

confirm_install() {
  [[ "$FORCE" -eq 1 ]] && return
  cat <<EOF
即将安装 os-watcher：
  仓库：$GITHUB_REPO
  版本：$VERSION
  平台：$PLATFORM
  包型：$PACKAGE
  目录：$SCRIPT_DIR
  服务：$SERVICE_NAME
EOF
  read -r -p "继续？[y/N] " answer
  case "$answer" in
    y|Y|yes|YES) ;;
    *) fail "已取消" ;;
  esac
}

main() {
  require_privilege

  if [[ -z "$PLATFORM" ]]; then
    PLATFORM="$(detect_platform)"
  fi

  local ext asset url sha_url archive sha_file extract_dir root bin_name
  ext="$(asset_extension "$PLATFORM")"
  asset="os-watcher-${PLATFORM}-${PACKAGE}.${ext}"
  url="$(release_asset_url "$asset")"
  sha_url="$(release_asset_url "${asset}.sha256")"
  bin_name="os-watcher"
  if [[ "$PLATFORM" == windows-* ]]; then
    bin_name="os-watcher.exe"
  fi

  confirm_install

  TMP_DIR="$(mktemp -d 2>/dev/null || mktemp -d -t os-watcher-deploy)"
  archive="$TMP_DIR/$asset"
  sha_file="$TMP_DIR/${asset}.sha256"
  extract_dir="$TMP_DIR/extract"

  c_info "下载 Release 包：$url"
  download_file "$url" "$archive" || fail "下载失败：$url"

  if download_file "$sha_url" "$sha_file"; then
    verify_sha256_if_present "$archive" "$sha_file"
    c_ok "SHA256 校验通过"
  else
    c_warn "未下载到 SHA256 文件，继续安装"
  fi

  c_info "解压 Release 包"
  extract_archive "$archive" "$extract_dir"
  root="$(payload_root "$extract_dir")"

  backup_current_install "$bin_name"
  install_payload "$root" "$bin_name"

  if [[ "$PLATFORM" == windows-* ]]; then
    install_windows_service
  else
    write_linux_service
  fi
}

main "$@"
