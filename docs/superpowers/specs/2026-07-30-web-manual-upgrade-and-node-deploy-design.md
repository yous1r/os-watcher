# Web 端手动升级与远程节点部署 设计

日期：2026-07-30
分支：`feat/web-manual-upgrade-and-node-deploy`

## 背景

当前升级触发链路：

1. 每个节点后台的 `UpgradeManager` 按 `upgrade.check_interval_secs`（默认 1800s）轮询
   GitHub Releases，缓存 `latest` tag。
2. 面板通过本机 `GET /api/v1/version` 拿到 `latest`，前端把它和每个节点
   `NodeInfo.version` 比对，有新版就在卡片版本行渲染一个 `.update-dot` 按钮。
3. 点击弹出确认框，选择包型后 `POST http://<节点 api_addr>/api/v1/upgrade`。
4. 实际下载、备份、安装、重启、回滚全部由目标节点自己执行；面板只轮询
   `GET /api/v1/upgrade` 展示阶段。

本次要做三件事：把升级触发点移到节点卡片左上角的状态小点；新增 Web 端远程部署
新节点的能力；固定 Linux x86_64 的发布包平台。

## 目标

1. 节点卡片左上角状态小点承担「有更新可点」的职责，点击弹出既有升级确认框。
   升级仍由子节点自行完成，Web 端只发请求。
2. Web 端新增「添加节点」向导，通过 SSH 在新机器上部署 node 节点，部署日志通过
   WebSocket 实时回传。
3. `deploy.sh` 在 Linux x86_64 上固定使用 `linux-x86_64-musl` 包。

## 非目标

- 不做 API 鉴权（现状无鉴权，本次不引入）。
- 不做部署任务持久化、历史列表、并发多机部署。
- 不做失败自动回滚清理远端现场。
- 不改动节点自升级的内部实现。

## 一、状态小点触发升级

`web/src/views/Overview.tsx`：

- 卡片左上角 `.dot` 从 `<span>` 改为 `<button>`，保留 `dot-online/offline/unknown`
  三种状态色。
- 有更新且节点在线时追加 `dot-update` 修饰类：橙色底 + 呼吸光圈动画，`title`
  提示「发现新版本 vX.Y.Z，点击升级」，`aria-label` 给出节点名。
- 点击走现有 `openUpgradeDialog(snap)`；无更新时按钮 `disabled`，`cursor: default`，
  视觉与原状态点一致。
- 移除版本行里独立的 `.update-dot` 按钮，避免两个入口。版本行只留文本，
  有更新时补一句「→ vX.Y.Z」。

`web/src/styles.css`：`.dot` 增加 `button` 复位（`border:0; padding:0; appearance:none`），
新增 `.dot-update` 与 `pulse-update` 动画，`.dot:disabled` 不加 `not-allowed` 光标
（它此时只是纯指示器）。删除 `.update-dot` 相关规则。

升级请求、轮询、锁定逻辑完全复用现有实现，不改动。

## 二、远程节点部署

### 2.1 交互流程

`AddNodeDialog`（新建，四步向导）：

1. **SSH 连接**：主机、端口（默认 22）、用户名、认证方式（密码 / 私钥）。
   密码走 `type=password`；私钥用 textarea 粘贴 PEM，附可选 passphrase。
2. **高级配置**（默认折叠）：包型（node/full，默认 node）、API 端口、Gossip 端口、
   peers（默认预填本机 gossip 地址）、服务名、安装目录、版本、下载代理。
3. **确认**：配置摘要 + 「需要目标机 root 或免密 sudo」提示。
4. **部署**：`DeployProgress` 组件，四段进度 + 实时日志。

`DeployProgress`（新建）：建立 WebSocket，`onopen` 发送请求 JSON，按事件类型更新
进度条、追加日志（保留最近 500 行，自动滚底）、显示重试提示与最终结果。
部署中关闭按钮变「后台运行」（关闭对话框但保留连接直到结束），完成后变「关闭」。
成功后触发一次 `onDeployed` 让外层刷新节点列表。

### 2.2 协议

端点：`GET /api/v1/nodes/deploy`（WebSocket 升级）。

客户端首帧发送部署请求：

```json
{
  "host": "10.0.0.12",
  "port": 22,
  "username": "root",
  "auth": { "type": "password", "password": "***" },
  "package": "node",
  "api_port": 7980,
  "gossip_port": 7979,
  "peers": ["192.168.1.10:7979"],
  "service_name": "os-watcher",
  "install_dir": "/opt/os-watcher",
  "version": "latest",
  "repo": null,
  "proxy": null
}
```

服务端流式返回，`type` 为标签：

| type       | 字段                        | 说明                                   |
| ---------- | --------------------------- | -------------------------------------- |
| `progress` | `step`, `message`           | `connecting`/`uploading`/`installing`/`verifying` |
| `log`      | `stream`(`stdout`/`stderr`), `line` | 远端输出按行透传               |
| `retry`    | `attempt`, `max`, `message` | 第 N 次重试及上次失败原因              |
| `success`  | `message`                   | 终态，随后服务端关闭连接               |
| `error`    | `message`                   | 终态，随后服务端关闭连接               |

字段缺省值在后端补齐：`port=22`、`package=node`、`api_port=7980`、
`gossip_port=7979`、`service_name` 取 `upgrade.service_name`、
`install_dir` 取 `deploy.default_install_dir`、`version=latest`、
`repo` 取 `upgrade.github_repo`、`peers` 为空时填本机 `gossip_addr`。

### 2.3 后端实现

新增 `src/deploy.rs`，对外只暴露 `DeployRequest`、`DeployEvent`、
`run_deploy(request, config, local_gossip_addr, tx)`。

执行阶段：

1. **connecting** — russh 建连（`deploy.connect_timeout_secs`，默认 20s），按
   `auth` 做密码或公钥认证。服务器主机密钥一律接受（首次部署没有 known_hosts 可依）。
2. **uploading** — `include_str!("../deploy.sh")` 编译期内嵌脚本，用带引号的
   heredoc 写入 `<install_dir>/deploy.sh` 并 `chmod 0755`。脚本以自身所在目录作为
   安装目录，因此必须落在 install_dir 而非 /tmp。
3. **installing** — 执行
   `<install_dir>/deploy.sh --force --package … --platform linux-x86_64-musl
   --version … --repo … --port … --gossip-port … --service-name …
   [--peers …] [--proxy …]`，stdout/stderr 按行推 `log` 事件。
4. **verifying** — `systemctl is-active --quiet <service>` 确认服务已拉起。

提权：`username == "root"` 时不加前缀；否则密码认证用 `sudo -S -p ''` 并把密码
写入 stdin，私钥认证用 `sudo -n`，遇到需要密码时返回可读错误（提示配置免密 sudo）。

重试：`uploading`/`installing`/`verifying` 任一步失败即重跑整轮（含重连），
上限 `deploy.max_attempts`（默认 3），退避 2s、4s。认证失败与参数校验失败
不重试，直接报错——重试对凭据错误无意义。

参数校验：主机名/IP 非空且不含空白与 shell 元字符；端口 1..=65535；
`service_name` 只允许 `[A-Za-z0-9._@-]`；`install_dir` 必须是绝对路径且不含引号；
peers 每项形如 `host:port`。所有拼进 shell 的值统一走单引号转义
（`'` → `'\''`），校验是第二道防线。

`src/config.rs` 新增：

```toml
[deploy]
enabled = true
default_install_dir = "/opt/os-watcher"
connect_timeout_secs = 20
max_attempts = 3
```

`enabled = false` 时端点直接以 `error` 事件拒绝并关闭。

`src/api.rs`：axum 开启 `ws` feature，`ApiState` 增加 `deploy: DeployConfig`，
新增路由与 handler；handler 负责收首帧、建 mpsc 通道、把 `DeployEvent` 序列化
下发，任一侧断开即取消部署任务。

### 2.4 安全

`/api/v1/nodes/deploy` 接收 SSH 凭据并在远端以 root 执行命令，而现有 API 无任何
鉴权。面板一旦暴露在不可信网络，这个端点等于一个开放的 SSH 执行代理。缓解措施：

- `[deploy] enabled` 开关，可整体关闭；
- 配置模板与 README 明确标注「仅在可信网络内暴露面板」；
- 凭据只在内存中停留一次部署，不落库、不写日志；日志事件不回显密码与私钥。

这不是完备方案。API 鉴权是独立议题，本设计不承担。

## 三、deploy.sh 平台固定

`detect_platform()` 中 Linux x86_64 分支删掉 `ldd | grep musl` 探测，固定输出
`linux-x86_64-musl`。musl 包静态链接，不依赖目标机 glibc 版本，远程部署最稳。
aarch64 与 Windows 分支不变，`--platform` 显式覆盖仍然有效。

## 四、顺带修复

`src/main.rs` clap 属性里硬编码 `version = "0.0.7"`，与 Cargo.toml 的 `0.0.8`
不一致。改为 `env!("CARGO_PKG_VERSION")`，避免再次出现版本漂移。

## 五、测试

`cargo test`（`src/deploy.rs` + `src/api.rs`）：

- 请求缺省值补齐：port/package/端口/service_name/install_dir/peers 回填本机 gossip 地址；
- 安装命令参数拼装：peers 与 proxy 为空时不带对应 flag，非空时带且被正确引用；
- sudo 前缀：root 不加、密码认证用 `sudo -S`、密钥认证用 `sudo -n`；
- shell 引用：含单引号/空格/`;` 的值被安全转义；
- 参数校验拒绝非法主机、端口、服务名、非绝对安装目录、畸形 peer；
- 脚本上传命令包含唯一 heredoc 定界符且内嵌脚本内容不含该定界符行；
- `DeployEvent` 序列化出正确的 `type` 标签与字段名；
- `deploy.enabled = false` 时端点拒绝。

前端：`pnpm build`（`tsc --noEmit && vite build`）做类型与构建验证。项目当前无
前端测试框架，本次不引入。

## 六、涉及文件

新增：
- `src/deploy.rs`
- `web/src/views/AddNodeDialog.tsx`
- `web/src/views/DeployProgress.tsx`

修改：
- `Cargo.toml`（russh、russh-keys、axum ws feature）
- `src/main.rs`（挂载 deploy 模块、版本号、配置注入）
- `src/api.rs`（路由、ApiState、handler）
- `src/config.rs`（`[deploy]` 段）
- `config.node.example.toml`、`config.full.example.toml`
- `deploy.sh`（平台固定）
- `web/src/types.ts`、`web/src/api.ts`、`web/src/views/Overview.tsx`、`web/src/styles.css`
