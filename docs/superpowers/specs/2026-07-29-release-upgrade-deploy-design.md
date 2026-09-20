# Release 升级与部署方案

## 目标

本方案用于把 os-watcher 的发布、部署、运行中升级串成闭环：

- GitHub Actions 在推送版本 tag 后构建 Linux/Windows 多平台产物，并同时发布 `node` 与 `full` 两种包。
- Release 发布完成后自动把 `latest` tag 强制指向当前版本，降低人工维护标签的出错概率。
- 每个节点后台轮询 GitHub Releases，缓存当前版本、最新版本与升级状态，并暴露 REST API。
- Web Overview 页面在节点卡片显示版本号和橙色更新提示，允许用户二次确认后远程触发目标节点自升级。
- `deploy.sh` 不再本地编译，直接按平台、架构、包类型下载预构建 Release 包，安装并注册服务。

## 架构

系统由四个模块组成：

1. **CI/CD 流水线**
   - 触发条件：推送 `v*` tag。
   - 构建矩阵：`linux-x86_64`、`linux-x86_64-musl`、`linux-aarch64`、`windows-x86_64`。
   - 每个平台输出两种包：
     - `os-watcher-{platform}-node.{tar.gz|zip}`：节点二进制、`config.node.example.toml`。
     - `os-watcher-{platform}-full.{tar.gz|zip}`：节点二进制、Web dist、`config.full.example.toml`。
   - 每个包附带 `.sha256`。
   - Publish job 上传所有产物并通过 GitHub API 更新或创建 `refs/tags/latest`；如果发布 tag 是 annotated tag，会先剥离到最终 commit，避免 `latest` 指向 tag object。

2. **节点后端升级服务**
   - `UpgradeManager` 读取 `[upgrade]` 配置，启动版本检测循环。
   - `GET /api/v1/version` 返回当前版本、最新版本、平台、包类型、升级状态。
   - `GET /api/v1/upgrade` 返回当前升级状态。
   - `POST /api/v1/upgrade` 接收 `{ package, proxy }`，后台执行下载、备份、安装、重启。
   - 平台识别结果与 Release 资产命名保持一致：`linux-x86_64`、`linux-x86_64-musl`、`linux-aarch64`、`windows-x86_64`。

3. **Web UI 升级控件**
   - Overview 节点卡片显示 `版本 {version}`。
   - 当 GitHub 最新版本高于节点版本时显示橙色圆点。
   - 点击圆点弹出确认框，展示节点、当前版本、最新版本，并允许选择 `Node` 或 `Full` 包。
   - 提交后调用目标节点的 `/api/v1/upgrade`，按钮进入加载态，返回后显示结果，期间禁止重复提交。

4. **部署脚本**
   - `deploy.sh` 以脚本所在目录为工作目录。
   - Linux 要求 root/sudo；Windows 提供 `deploy.ps1`（原生 PowerShell）与 `deploy.cmd`（UAC 自动提权），`deploy.sh` 在 Windows 上把服务注册委派给 `deploy.ps1`。
   - 参数支持 `--package node|full`、`--version`、`--repo`、`--platform`、`--proxy`、端口、peers、服务名等。
   - 自动检测平台，构造 Release 下载 URL，优先用 `curl`，否则用 `wget`。
   - 如果下载到 `.sha256`，使用 `sha256sum` 或 `certutil` 校验。
   - 安装前备份当前二进制、配置和 `web-dist`。
   - Linux 写入 systemd 服务，`WorkingDirectory` 指向部署目录。
   - Windows 用原生 `sc.exe`/SCM 注册服务（`New-Service` 写入 binPath），二进制自身实现 `StartServiceCtrlDispatcher` 协议，不依赖 NSSM；运行中自升级通过隐藏 PowerShell 进程调用 `sc.exe stop/start` 控制该服务。

## 后端升级流程

`POST /api/v1/upgrade` 只负责提交后台任务，实际升级状态通过 `GET /api/v1/upgrade` 或 `GET /api/v1/version` 轮询。

流程如下：

1. 检查 `[upgrade].enabled` 和当前是否已有升级任务在运行。
2. 访问 GitHub latest release API，缓存最新 tag。
3. 按当前编译目标识别平台，按请求包类型选择精确资产名。
4. 创建临时目录，下载资产，失败最多重试 3 次。
5. 解析当前可执行文件和安装目录。
6. 备份当前二进制、`config.toml`、`config.example.toml`、`web-dist`。
7. 解压 Release 包，复制 payload：
   - Linux 直接用 `.new` 临时文件替换当前二进制并设置可执行权限。
   - Windows 先写入 `.new`，重启阶段再停服务、替换、启动。
   - payload 只做覆盖，不删除既有文件；因此切到 `node` 包时，
     `install_payload` 之后由 `apply_package_layout` 删掉 `full` 包留下的
     `web-dist`。
   - 随后按新包类型调和 `config.toml`（见下节「包类型与配置的一致性」）。
8. 调度服务重启：
   - Linux 使用 `systemctl restart <service>`。
   - Windows 使用隐藏 PowerShell 进程调用 `sc.exe stop/start`。
9. 任一安装或重启调度错误会尝试回滚备份，并在状态中保留 `rolled_back` 或 `failed`。

## 配置

新增 `[upgrade]` 配置段：

```toml
[upgrade]
enabled = true
github_repo = "yous1r/os-watcher"
check_interval_secs = 1800
package = "node"
service_name = "os-watcher"
# proxy = "http://127.0.0.1:7890"
```

`node` 包模板默认 `package = "node"` 且不启用 Web；`full` 包模板默认 `package = "full"` 且启用 `web-dist`。

### 包类型与配置的一致性

安装一个包只覆盖它带来的文件，用户的 `config.toml` 始终保留。这在同包升级时
是对的，但切换 `node`/`full` 后配置会与新装的包不符：装了 `full` 面板仍
`enabled = false`（页面 404），降到 `node` 后 `web.dir` 还指向残留目录。

因此自升级（`src/upgrade.rs`）和两个部署脚本在安装完 payload 后都会调用同一
套调和逻辑 `config::reconcile_package_config`，只改写包类型决定的键：

| 键 | `node` | `full` |
| --- | --- | --- |
| `[web] enabled` | `false` | `true` |
| `[web] dir` | 不变 | `web-dist`（仅当原值为相对路径） |
| `[upgrade] package` | `"node"` | `"full"` |

其余设置与注释原样保留；`[web] dir` 是绝对路径时视为运维自建的前端目录，不改。
调和是幂等的（同包重复执行不产生 diff），并保留原文件的行尾（CRLF/LF）。

部署脚本通过 `os-watcher reconcile-config --package <node|full>` 复用该实现，
二进制缺失或执行失败只告警、不中断安装。

## API

版本接口：

```http
GET /api/v1/version
```

响应沿用项目统一包装：

```json
{
  "success": true,
  "data": {
    "current": "0.1.0",
    "latest": "v0.2.0",
    "update_available": true,
    "checked_at": "2026-07-29T00:00:00Z",
    "platform": "linux-x86_64",
    "package": "node",
    "upgrade": {
      "running": false,
      "phase": "idle",
      "message": "idle"
    }
  }
}
```

升级接口：

```http
POST /api/v1/upgrade
Content-Type: application/json

{"package":"full","proxy":"http://127.0.0.1:7890"}
```

`package` 和 `proxy` 均可省略。已有升级任务时返回 `409 Conflict`，未启用升级时返回 `503 Service Unavailable`。

## 卸载

卸载接口：

```http
POST /api/v1/uninstall
Content-Type: application/json

{"backup":true,"keep_config":false}
```

`backup` 与 `keep_config` 均可省略（默认 `false`）。该接口需要管理员会话：未登录返回 `401 Unauthorized`；`[auth] enabled = true` 但未设置口令时返回 `503 Service Unavailable`，不会静默放行。响应先于实际删除返回，因此 `success: true` 只代表「卸载已排程」，面板据此提示节点即将离线。

设计要点：

- **必须用脱离进程执行**。Windows 不允许删除正在运行的可执行文件，Linux 上停掉服务也会连带杀死服务进程的子进程。Windows 经 WMI `Win32_Process Create` 拉起 PowerShell（脚本以 base64 UTF-16LE 的 `-EncodedCommand` 传递，不落盘），Linux 用 `systemd-run --collect` 给 helper 单独分配 unit，使其不受 `systemctl stop` 影响。
- **备份失败即中止**，一个文件都不删；默认只备份 `config.toml`。服务停止排在备份之后：备份失败时既不删文件也不停服务，不会因一次失败的卸载造成停机。
- **helper 自己停服务并注销**（Linux `systemctl stop/disable` + 删 unit + `daemon-reload` + `reset-failed`，Windows `sc.exe stop/delete`）。早期实现漏掉了这一步：面板触发的卸载删掉了文件，却把 systemd unit 留在已启用状态、进程仍在运行，重启后服务还会被拉起。卸载脚本走的是同一套顺序。
- **备份目录默认在安装目录的同级**（`os-watcher-backup-<时间戳>`）。早期实现把它放在安装目录内，会被同一次卸载删除——这是实测发现的真实数据丢失缺陷。
- **占用中的文件登记为重启时删除**：Windows 用 `MoveFileExW(path, NULL, MOVEFILE_DELAY_UNTIL_REBOOT)`。调用必须走 `IntPtr` 重载——PowerShell 把 `$null` 传给 `[string]` 参数时会 marshal 成空字符串而非 NULL 指针，删除语义要求真正的 NULL，否则静默失败（返回 `ERROR_PATH_NOT_FOUND` 且什么都不登记）。
- **`keep_config` 时不登记安装目录**，否则重启清理会把用户明确要求保留的 `config.toml` 一并删掉。
- **`uninstall.cmd` 自身不能立即删除**：cmd.exe 逐行读取批处理文件，删除它会以「系统找不到指定的路径」中止并丢掉退出码，故与脚本自身一样登记重启清理。
- Windows 逻辑只用 cmd / PowerShell 实现；`uninstall.sh` 只处理 Linux，检测到 Windows 会拒绝执行并指向 `uninstall.cmd`。

### 远程卸载（面板控制任意节点）

本地卸载只能删掉面板自己所在机器的服务，管不到其它节点。远程卸载复用既有的 SSH 部署通道：
WebSocket `/api/v1/nodes/deploy` 的首帧多一个 `action` 字段（`"deploy"` 默认 / `"uninstall"`），
`uninstall` 时把 `uninstall.sh` 用 heredoc 推到目标节点的 `install_dir` 下执行。

- **脚本由面板自带（`include_str!("../uninstall.sh")`），与目标节点版本无关**。这正是该能力的关键：
  节点上装的是多老的版本都不影响卸载，因为执行的脚本来自面板，不是节点自己那份。
  若改用节点上的 `uninstall.sh`，老版本缺参数或行为不一致都会让卸载失败。
- **预检先于建目录**：先跑 `test -e '<install_dir>/os-watcher'`，不像安装目录就 `Fatal` 结束（不重试）。
  否则 `install_dir` 填错时会先 `mkdir -p` 留下一个空目录和一份脚本，再把错误报给用户。
- **`action` 省略默认 `deploy`**，旧前端不传该字段时行为不变。
- **卸载选项**：`backup`（默认 `true`，与本地接口的默认值不同——远程卸载误删后无法补救）、
  `keep_config`、`backup_dir`。`backup_dir` 必须是绝对路径、不含引号/控制字符/`%`（`%` 会被 systemd 当 specifier 展开）。
- **卸载校验用 `test ! -e '<install_dir>/os-watcher'`**：`--keep-config` 只保留 `config.toml`，
  可执行文件在所有分支下都会被删，故该断言对所有分支成立。
- **`uninstall.sh` 加安装目录护栏**（`verify_install_dir`）：远程卸载时 `SCRIPT_DIR` 完全来自请求里的
  `install_dir`，填错会让删除循环清空那个目录。黑名单拦掉 `/`、`/etc`、`/usr`、`/var` 等 18 个系统目录，
  并要求目录下至少存在 `os-watcher`、`os-watcher.exe`、`config.toml` 之一。
- **仅支持 Linux 远程卸载**。与既有 deploy 通道的能力对齐（该通道本就是 Linux/SSH/systemd）；
  Windows 远程卸载未实现，UI 与文档都不声称支持。

## 安全边界

本版本按需求不加入登录认证，也不做节点侧鉴权。安全边界依赖部署网络、反向代理、系统防火墙或内网访问控制。UI 弹窗只用于降低误操作风险，不作为安全机制。

后续可以在保持 API 形态不变的前提下增加：

- 节点升级 token。
- 管理端签名请求。
- Release 包签名验证。
- 只允许指定来源触发升级。

## 测试方案

- 后端单元测试：版本比较、资产命名和选择、平台枚举、API 版本接口、禁用升级时的拒绝响应、后台错误不覆盖回滚状态。
- 脚本静态验证：`bash -n deploy.sh`。
- 前端构建验证：`npm run build`，确保 Solid/TypeScript 编译通过。
- 集成测试建议：用本地模拟 Release 包和临时安装目录覆盖下载、备份、替换、回滚路径；真实 systemd/Windows 服务重启在隔离测试机验证。
- 网络代理验证：分别用 `[upgrade].proxy`、`--proxy`、`HTTPS_PROXY` 覆盖下载路径。

## 上线计划

1. 在开发分支完成代码合并和测试。
2. 推送版本 tag，确认 Release 同时包含所有平台的 `node` 与 `full` 产物。
3. 检查 `latest` tag 是否已指向新版本。
4. 新机器优先用 `deploy.sh --package node|full` 安装。
5. 已部署节点先小批量从 UI 触发升级，观察状态、服务重启和日志。
6. 扩大升级范围，保留备份目录直到确认稳定。
