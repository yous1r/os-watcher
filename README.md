# os-watcher

os-watcher 是一个去中心化主机资源监控工具，提供节点采集、Gossip 同步、Web 面板、自升级与远程节点部署能力。

## 配置

- 普通采集节点：[config.node.example.toml](config.node.example.toml)
- 带 Web 面板的节点：[config.full.example.toml](config.full.example.toml)

发布包分 `node`（只采集）与 `full`（带 Web 面板）两种，同一台机器可以在两者之间切换：用对应包重新执行 `deploy.sh` / `deploy.ps1`，或在面板里发起自升级并指定包类型。安装时会同步 `config.toml` 中由包类型决定的键（`[web] enabled`、`[web] dir`、`[upgrade] package`），其余设置与注释原样保留；`full` 降级到 `node` 时还会清理包内的 `web-dist`。切换后需重启服务生效。

也可以手工同步（脚本内部调用的就是它）：

```bash
os-watcher --config config.toml reconcile-config --package full
```

## 卸载

各平台的卸载脚本随发布包一起分发，放在安装目录内，用**本平台自己的入口**执行：

| 平台 | 入口 | 说明 |
| --- | --- | --- |
| Linux | `sudo ./uninstall.sh` | 纯 shell 实现；在 Windows 上运行会直接报错并指向 `uninstall.cmd` |
| Windows | `uninstall.cmd`（或 `uninstall.ps1`） | 纯 cmd / PowerShell 实现，非管理员会自动经 UAC 提权 |

Windows 不要用 Git Bash 跑 `uninstall.sh`：它只处理 Linux，检测到 Windows 会拒绝执行。

常用选项（两平台同名，写法按各自惯例）：

| 用途 | Linux | Windows |
| --- | --- | --- |
| 跳过交互确认 | `--yes` / `-y` | `-Yes` |
| 不备份 | `--no-backup` | `-NoBackup` |
| 保留配置 | `--keep-config` | `-KeepConfig` |
| 指定备份目录 | `--backup-dir DIR` | `-BackupDir DIR` |
| 指定服务名 | `--service-name NAME` | `-ServiceName NAME` |

行为约定：

- **默认只备份 `config.toml`**（含口令、推送渠道与节点拓扑，重装后可直接复用）。数据库 `os-watcher.db`（可能数百 MB）与 `web-dist` 不备份，需要时请手工拷贝。
- 备份默认落在**安装目录的同级** `os-watcher-backup-<时间戳>/`。备份必须在安装目录之外——放在目录内会被同一次卸载一并删掉。
- 卸载顺序是「确认 → 备份 → 停服务 → 删文件」。**备份失败即中止**，不会删除任何文件。
- 被占用的文件（Windows 上正在运行的 `os-watcher.exe`、被日志句柄占用的文件、以及脚本自身）无法立即删除，会登记为**下次重启时删除**；`--keep-config` 时安装目录本身不会被登记，否则会连保留的配置一起删掉。

面板顶栏也提供「卸载」按钮，需要管理员登录；未登录返回 401，`[auth]` 开启但未设口令返回 503。面板触发的卸载与脚本等价，同样会登记重启清理。


## 安全警告

监控视图（概览 / 节点详情 / 告警）对访客只读；管理动作——自升级、远程节点部署、推送渠道管理——需要管理员登录，口令在 `[auth] password` 中设置。未设置口令时管理接口会拒绝所有请求并提示配置，不会静默放行。

远程部署端点会接收 SSH 凭据并在目标主机执行 root 或 sudo 命令，因此仍必须仅在可信网络内暴露面板。不使用远程部署时，请设置 `[deploy] enabled = false`。

## 告警与推送

- 告警会随条件自动解除：条件恢复、节点离线、节点被移除、指标消失（如磁盘被卸载）各对应一个解除原因；面板「告警」页同时展示当前告警与最近恢复记录（也可经 `GET /api/v1/alerts/history` 获取）。最近恢复的保留时长由 `[storage] alert_history_minutes` 决定（默认 10 分钟），过期记录自动从内存与数据库删除；设为 `0` 则解除即删。
- 推送渠道在面板「推送设置」页维护，落库保存，支持 Bark：填设备 Key 即可。Bark 服务地址默认取 `[notify] server_url`，自建 bark-server 只改这一处（支持带路径前缀，如 `https://example.com/bark`），也可在面板里为单个渠道覆盖。若 Bark App 中开启了内容加密，需选择与 App 完全一致的算法（AES128/AES192/AES256）、模式（CBC/GCM/ECB）、密钥与 IV；GCM 仅支持 AES128/AES256。渠道可单独设置最低推送级别，并可随时「测试推送」验证连通性。

