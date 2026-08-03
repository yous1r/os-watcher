# os-watcher

os-watcher 是一个去中心化主机资源监控工具，提供节点采集、Gossip 同步、Web 面板、自升级与远程节点部署能力。

## 配置

- 普通采集节点：[config.node.example.toml](config.node.example.toml)
- 带 Web 面板的节点：[config.full.example.toml](config.full.example.toml)

## 安全警告

Web 面板与 API 当前没有内置鉴权。远程部署端点会接收 SSH 凭据并在目标主机执行 root 或 sudo 命令，因此必须仅在可信网络内暴露面板。不使用远程部署时，请设置 `[deploy] enabled = false`。
