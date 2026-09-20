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
