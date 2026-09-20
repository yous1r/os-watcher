#!/usr/bin/env bash
#
# os-watcher 卸载脚本（Linux）
#
# 用法：
#   sudo ./uninstall.sh                       # 交互确认，备份 config.toml
#   sudo ./uninstall.sh --yes                 # 跳过确认
#   sudo ./uninstall.sh --no-backup           # 不备份，直接删除
#   sudo ./uninstall.sh --backup-dir /tmp/x   # 指定备份目录
#   sudo ./uninstall.sh --keep-config         # 保留 config.toml（不随目录删除）
#
# Windows 不在本脚本实现：服务注销与「占用文件登记重启删除」都要调用
# PowerShell，在 bash 里拼内联命令只能靠引号与转义，出错还被重定向掩盖。
# Windows 请运行 uninstall.cmd（或直接跑 uninstall.ps1）；在 Git Bash 里运行
# 本脚本时也会自动转交 uninstall.ps1，两者是同一份实现。
#
# 选项：
#   --service-name <NAME>   服务名，默认 os-watcher
#   --backup-dir <DIR>      备份目录，默认 <安装目录同级>/os-watcher-backup-<时间戳>
#   --backup                备份（默认行为，显式写出便于脚本化）
#   --no-backup             不备份
#   --keep-config           保留 config.toml，仅删除程序文件
#   --yes, -y               跳过交互确认
#   -h, --help              显示帮助
#
# 说明：卸载会停止并注销 systemd 服务、删除安装目录，不可撤销。

set -Eeuo pipefail

SERVICE_NAME="os-watcher"
BACKUP_DIR=""
DO_BACKUP=1
KEEP_CONFIG=0
ASSUME_YES=0

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"

# 离开安装目录：Linux 不允许删除任何进程的当前工作目录，而文档里的用法正是
# `cd <安装目录> && sudo ./uninstall.sh`，停在目录里会让最后一步 rmdir 失败，
# 留下一个空壳目录。脚本内所有路径都基于 $SCRIPT_DIR，切走不影响任何操作。
cd / 2>/dev/null || true

c_info() { printf '\033[36m[INFO]\033[0m %s\n' "$*"; }
c_ok() { printf '\033[32m[ OK ]\033[0m %s\n' "$*"; }
c_warn() { printf '\033[33m[WARN]\033[0m %s\n' "$*"; }
c_err() { printf '\033[31m[FAIL]\033[0m %s\n' "$*" >&2; }
fail() { c_err "$*"; exit 1; }

usage() {
  sed -n '2,25p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  exit 0
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --service-name)
      SERVICE_NAME="${2:-}"; shift 2 ;;
    --backup-dir)
      BACKUP_DIR="${2:-}"; shift 2 ;;
    --backup)
      DO_BACKUP=1; shift ;;
    --no-backup)
      DO_BACKUP=0; shift ;;
    --keep-config)
      KEEP_CONFIG=1; shift ;;
    --yes|-y)
      ASSUME_YES=1; shift ;;
    -h|--help)
      usage ;;
    *)
      fail "未知参数：$1；使用 --help 查看用法" ;;
  esac
done

is_windows() {
  case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*) return 0 ;;
    *) return 1 ;;
  esac
}

require_privilege() {
  [[ "$(id -u)" -eq 0 ]] || fail "Linux 卸载需要 root 权限，请使用 sudo"
}

# 服务名会进 systemctl 与文件名，限制字符集避免被当成选项或路径。
validate_service_name() {
  [[ "$SERVICE_NAME" =~ ^[A-Za-z0-9_.@-]+$ ]] ||
    fail "服务名只能包含字母、数字、点、下划线、@ 和连字符：$SERVICE_NAME"
}

# Windows 不是本脚本的职责：服务注销与「占用文件登记重启删除」都要调 Win32 API，
# 在 bash 里拼内联 PowerShell 只能靠引号与转义，出错还会被重定向掩盖。
# Windows 请用 uninstall.cmd（或 uninstall.ps1），那是同一份实现的唯一入口。
refuse_on_windows() {
  if is_windows; then
    fail "Windows 请运行 uninstall.cmd（或 uninstall.ps1），本脚本只处理 Linux"
  fi
}

stop_and_remove_service() {
  if ! command -v systemctl >/dev/null 2>&1; then
    c_warn "未找到 systemctl，跳过服务处理"
    return 0
  fi

  if ! systemctl list-unit-files "${SERVICE_NAME}.service" >/dev/null 2>&1; then
    c_info "服务未注册：$SERVICE_NAME"
    return 0
  fi

  c_info "停止并注销服务：$SERVICE_NAME"
  systemctl stop "$SERVICE_NAME" 2>/dev/null || c_warn "停止服务失败，继续卸载"
  systemctl disable "$SERVICE_NAME" 2>/dev/null || true
  rm -f "/etc/systemd/system/${SERVICE_NAME}.service"
  systemctl daemon-reload || true
  systemctl reset-failed "$SERVICE_NAME" 2>/dev/null || true
  c_ok "服务已注销：$SERVICE_NAME"
}

# 默认只备份用户配置：它含口令、推送渠道与节点拓扑，重装后可直接复用。
# 数据库（可能数百 MB）与 web-dist（发布包自带）不备份，需要时手工拷贝。
#
# 备份目录默认放在安装目录的**同级**位置：卸载会删除安装目录，备份若放在
# 目录内会被同一次卸载一并删掉。
backup_install() {
  local target="$BACKUP_DIR"
  [[ -n "$target" ]] || target="$(dirname "$SCRIPT_DIR")/os-watcher-backup-$(date +%Y%m%d%H%M%S)"
  mkdir -p "$target"

  if [[ ! -f "$SCRIPT_DIR/config.toml" ]]; then
    c_warn "没有可备份的文件（$SCRIPT_DIR/config.toml 不存在）"
    rmdir "$target" 2>/dev/null || true
    return 0
  fi

  cp -a "$SCRIPT_DIR/config.toml" "$target/" || fail "备份 config.toml 失败"
  c_ok "已备份到：$target"
  c_info "重新安装后把 config.toml 放回安装目录即可恢复设置"
}

confirm_uninstall() {
  [[ "$ASSUME_YES" -eq 1 ]] && return
  cat <<EOF
即将卸载 os-watcher：
  目录：$SCRIPT_DIR
  服务：$SERVICE_NAME
  备份：$([[ "$DO_BACKUP" -eq 1 ]] && echo "是（仅 config.toml）" || echo "否")
  $([[ "$KEEP_CONFIG" -eq 1 ]] && echo "保留 config.toml：是" || echo "保留 config.toml：否")

该操作会删除安装目录，且不可撤销。
EOF
  read -r -p "继续？[y/N] " answer
  case "$answer" in
    y|Y|yes|YES) ;;
    *) fail "已取消" ;;
  esac
}

# 删除安装目录内容，保留脚本自身（正在执行的脚本无法删除）。
# Linux 允许 unlink 运行中的可执行文件，因此这里没有「占用删不掉」的情况；
# 真有残留只报告，不假装成功。
remove_install_files() {
  local self_name
  self_name="$(basename "${BASH_SOURCE[0]}")"
  local leftover=()

  local entry name
  # 隐藏文件（.os-watcher-upgrade-status.json 等）不在 *. 通配里，单独扫一遍。
  for entry in "$SCRIPT_DIR"/* "$SCRIPT_DIR"/.[!.]*; do
    [[ -e "$entry" ]] || continue
    name="$(basename "$entry")"
    [[ "$name" == "$self_name" ]] && continue
    if [[ "$KEEP_CONFIG" -eq 1 && "$name" == "config.toml" ]]; then
      continue
    fi
    rm -rf "$entry" 2>/dev/null || leftover+=("$entry")
  done

  rm -f "$SCRIPT_DIR/$self_name" 2>/dev/null || leftover+=("$SCRIPT_DIR/$self_name")
  rmdir "$SCRIPT_DIR" 2>/dev/null || true

  if [[ "${#leftover[@]}" -gt 0 ]]; then
    c_warn "以下文件未能删除，请手动清理："
    printf '       %s\n' "${leftover[@]}"
  fi
}

main() {
  [[ -n "$SERVICE_NAME" ]] || fail "--service-name 不能为空"
  validate_service_name
  refuse_on_windows
  require_privilege
  c_info "安装目录：$SCRIPT_DIR"

  confirm_uninstall

  # 备份在停服务之前：配置是静态文件，此时服务仍能正常写入，不会读到半截状态。
  if [[ "$DO_BACKUP" -eq 1 ]]; then
    backup_install
  else
    c_warn "已跳过备份（--no-backup），config.toml 将随目录一起删除"
  fi

  stop_and_remove_service
  remove_install_files

  c_ok "os-watcher 已卸载"
}

main "$@"
