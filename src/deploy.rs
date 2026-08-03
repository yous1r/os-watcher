//! 远程节点部署：通过 SSH 把 os-watcher 安装到目标主机。
//!
//! 面板端收到一个 [`DeployRequest`]（含 SSH 凭据），本模块用 russh 建连、
//! 上传内嵌的 `deploy.sh`、按需 sudo 提权执行，并把过程以 [`DeployEvent`]
//! 流式回传给前端。凭据只在单次部署的内存里存在，绝不落库、绝不写日志。
//!
//! 事件的 serde 标签与字段名必须与前端 `web/src/types.ts` 的
//! `DeployRequest` / `DeployEvent` 逐字对应，两端才能对上。
//!
//! 安全约束：
//! - 所有插入 shell 命令的值都做单引号转义（`'` → `'\''`），并以参数校验
//!   作为第二道防线。
//! - 认证失败与校验失败不重试；只有上传/安装/校验阶段的瞬时失败才整轮重试。
//! - 面板不引入鉴权，`[deploy] enabled` 是唯一开关，仅应在可信网络内暴露。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use russh::client::{self, Handle, Handler};
use russh::keys::PrivateKeyWithHashAlg;
use russh::{ChannelMsg, Disconnect};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::config::{DeployConfig, PackageKind};

const MAX_LOG_LINE_BYTES: usize = 16 * 1024;

/// SSH 认证方式。与前端 `DeployAuth` 对应，用内部标签 `type` 区分。
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum DeployAuth {
    /// 密码认证：密码经 stdin 传给 `sudo -S`，不出现在命令行。
    Password { password: String },
    /// 私钥认证：可选 passphrase 解密。
    Key {
        private_key: String,
        passphrase: Option<String>,
    },
}

/// 部署请求首帧，前端通过 WebSocket 发来。字段名与前端 `DeployRequest` 一致。
#[derive(Debug, Clone, Deserialize)]
pub struct DeployRequest {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: DeployAuth,
    pub package: PackageKind,
    pub api_port: u16,
    pub gossip_port: u16,
    pub peers: Vec<String>,
    pub service_name: String,
    pub install_dir: String,
    pub version: String,
    pub repo: Option<String>,
    pub proxy: Option<String>,
}

#[derive(Deserialize)]
struct DeployRequestPayload {
    host: String,
    port: Option<u16>,
    username: String,
    auth: DeployAuth,
    package: Option<PackageKind>,
    api_port: Option<u16>,
    gossip_port: Option<u16>,
    #[serde(default)]
    peers: Option<Vec<String>>,
    service_name: Option<String>,
    install_dir: Option<String>,
    version: Option<String>,
    repo: Option<String>,
    proxy: Option<String>,
}

pub(crate) fn parse_request(
    json: &str,
    config: &DeployConfig,
    default_service_name: &str,
    default_repo: &str,
    local_gossip_addr: &str,
) -> Result<DeployRequest> {
    let payload: DeployRequestPayload = serde_json::from_str(json)?;
    let peers = match payload.peers {
        Some(peers) if !peers.is_empty() => peers,
        _ if local_gossip_addr.is_empty() => Vec::new(),
        _ => vec![local_gossip_addr.to_string()],
    };

    Ok(DeployRequest {
        host: payload.host,
        port: payload.port.unwrap_or(22),
        username: payload.username,
        auth: payload.auth,
        package: payload.package.unwrap_or(PackageKind::Node),
        api_port: payload.api_port.unwrap_or(7980),
        gossip_port: payload.gossip_port.unwrap_or(7979),
        peers,
        service_name: payload
            .service_name
            .unwrap_or_else(|| default_service_name.to_string()),
        install_dir: payload
            .install_dir
            .unwrap_or_else(|| config.default_install_dir.clone()),
        version: payload.version.unwrap_or_else(|| "latest".to_string()),
        repo: Some(payload.repo.unwrap_or_else(|| default_repo.to_string())),
        proxy: payload.proxy,
    })
}

enum Privilege {
    Root,
    PasswordSudo(String),
    KeySudo,
}

impl Privilege {
    fn for_request(request: &DeployRequest) -> Self {
        if request.username == "root" {
            return Self::Root;
        }
        match &request.auth {
            DeployAuth::Password { password } => Self::PasswordSudo(password.clone()),
            DeployAuth::Key { .. } => Self::KeySudo,
        }
    }

    fn wrap(&self, command: &str) -> String {
        match self {
            Self::Root => command.to_string(),
            Self::PasswordSudo(_) => format!("sudo -S -p '' {command}"),
            Self::KeySudo => format!("sudo -n {command}"),
        }
    }

    fn wrap_shell(&self, command: &str) -> String {
        match self {
            Self::Root => command.to_string(),
            Self::PasswordSudo(_) | Self::KeySudo => {
                self.wrap(&format!("sh -c {}", shell_quote(command)))
            }
        }
    }

    fn stdin(&self) -> Option<&str> {
        match self {
            Self::PasswordSudo(password) => Some(password),
            Self::Root | Self::KeySudo => None,
        }
    }

    fn check_command(&self) -> Option<String> {
        match self {
            Self::Root => None,
            Self::PasswordSudo(_) => Some("sudo -S -p '' -v".to_string()),
            Self::KeySudo => Some("sudo -n -v".to_string()),
        }
    }
}

/// 部署阶段，对应前端 `DeployStep`。
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeployStep {
    Connecting,
    Uploading,
    Installing,
    Verifying,
}

/// 流式回传给前端的部署事件。`type` 为外部标签，与前端 `DeployEvent` 对应。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum DeployEvent {
    /// 进入某个阶段。
    Progress { step: DeployStep, message: String },
    /// 远程命令的一行输出。
    Log { stream: LogStream, line: String },
    /// 整轮重试通知。
    Retry {
        attempt: u32,
        max: u32,
        message: String,
    },
    /// 部署成功，终态。
    Success { message: String },
    /// 部署失败，终态。
    Error { message: String },
}

/// 日志来源流，对应前端 `stream: "stdout" | "stderr"`。
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogStream {
    Stdout,
    Stderr,
}

/// 部署过程中的错误分类：决定是否重试。
enum DeployError {
    /// 认证/校验失败等永久错误，不重试。
    Fatal(anyhow::Error),
    /// 建连/上传/命令执行等瞬时错误，可整轮重试。
    Retryable(anyhow::Error),
}

/// russh 客户端 handler：远程部署默认接受服务器公钥（部署面板在可信网络内，
/// 且首次连接无已知 host key 可比对）。不做 TOFU 持久化。
struct ClientHandler;

impl Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

/// 部署入口。建连→上传→安装→校验，失败按分类决定是否整轮重试，
/// 全过程通过 `tx` 把事件推给调用方（WebSocket handler）。
///
/// `local_gossip_addr` 供调用方在请求 peers 为空时兜底注入本机地址，
/// 由 handler 在构造请求时处理；本函数只消费最终的 `request.peers`。
pub async fn run_deploy(
    mut request: DeployRequest,
    config: DeployConfig,
    local_gossip_addr: String,
    tx: mpsc::Sender<DeployEvent>,
) {
    if request.peers.is_empty() && !local_gossip_addr.is_empty() {
        request.peers.push(local_gossip_addr);
    }
    // 校验失败是永久错误，直接失败，绝不重试。
    if let Err(e) = validate_request(&request) {
        let _ = tx
            .send(DeployEvent::Error {
                message: format!("参数校验失败：{e}"),
            })
            .await;
        return;
    }

    let max = config.max_attempts.max(1);
    let mut attempt = 0u32;

    loop {
        attempt += 1;
        match deploy_once(&request, &config, &tx).await {
            Ok(()) => {
                let _ = tx
                    .send(DeployEvent::Success {
                        message: format!("节点 {} 部署完成", request.host),
                    })
                    .await;
                return;
            }
            Err(DeployError::Fatal(e)) => {
                let _ = tx
                    .send(DeployEvent::Error {
                        message: e.to_string(),
                    })
                    .await;
                return;
            }
            Err(DeployError::Retryable(e)) => {
                if attempt >= max {
                    let _ = tx
                        .send(DeployEvent::Error {
                            message: format!("已重试 {attempt} 次仍失败：{e}"),
                        })
                        .await;
                    return;
                }
                // 指数退避：2s、4s、8s……
                let backoff =
                    Duration::from_secs(2u64.saturating_mul(1u64 << (attempt - 1).min(5)));
                let _ = tx
                    .send(DeployEvent::Retry {
                        attempt,
                        max,
                        message: format!("{e}；{}s 后重试", backoff.as_secs()),
                    })
                    .await;
                tokio::time::sleep(backoff).await;
            }
        }
    }
}

/// 单轮部署：任一阶段失败即返回，由 `run_deploy` 决定是否重试。
async fn deploy_once(
    request: &DeployRequest,
    config: &DeployConfig,
    tx: &mpsc::Sender<DeployEvent>,
) -> Result<(), DeployError> {
    // —— 阶段 1：建连 ——
    let _ = tx
        .send(DeployEvent::Progress {
            step: DeployStep::Connecting,
            message: format!("连接 {}:{} …", request.host, request.port),
        })
        .await;

    let handle = connect_and_auth(request, config).await?;

    let privilege = Privilege::for_request(request);

    if let Some(command) = privilege.check_command() {
        run_command(&handle, &command, privilege.stdin(), tx)
            .await
            .map_err(|error| {
                DeployError::Fatal(anyhow!(
                    "远端用户必须是 root，或具备可用 sudo；密钥认证需要免密 sudo：{error}"
                ))
            })?;
    }

    // —— 阶段 2：上传 deploy.sh ——
    let _ = tx
        .send(DeployEvent::Progress {
            step: DeployStep::Uploading,
            message: "上传部署脚本 …".to_string(),
        })
        .await;

    let install_dir = &request.install_dir;
    let script_path = format!("{}/deploy.sh", install_dir.trim_end_matches('/'));

    // 建目录 + 落盘脚本，都走 sudo（安装目录通常需要 root）。
    let mkdir_cmd = privilege.wrap(&format!("mkdir -p {}", shell_quote(install_dir)));
    run_command(&handle, &mkdir_cmd, privilege.stdin(), tx)
        .await
        .map_err(DeployError::Retryable)?;

    upload_script(&handle, &script_path, DEPLOY_SCRIPT, &privilege, tx)
        .await
        .map_err(DeployError::Retryable)?;

    // —— 阶段 3：安装 ——
    let _ = tx
        .send(DeployEvent::Progress {
            step: DeployStep::Installing,
            message: "执行部署脚本 …".to_string(),
        })
        .await;

    let install_cmd = build_install_command(request, &script_path, &privilege);
    run_command(&handle, &install_cmd, privilege.stdin(), tx)
        .await
        .map_err(DeployError::Retryable)?;

    // —— 阶段 4：校验 ——
    let _ = tx
        .send(DeployEvent::Progress {
            step: DeployStep::Verifying,
            message: "校验服务状态 …".to_string(),
        })
        .await;

    let verify_cmd = build_verify_command(&request.service_name, &privilege);
    run_command(&handle, &verify_cmd, privilege.stdin(), tx)
        .await
        .map_err(DeployError::Retryable)?;

    // 优雅断开，忽略断开错误。
    let _ = handle
        .disconnect(Disconnect::ByApplication, "deploy finished", "")
        .await;

    Ok(())
}

/// 建连并认证。认证失败是永久错误（`Fatal`），不重试；建连超时/网络错误可重试。
async fn connect_and_auth(
    request: &DeployRequest,
    config: &DeployConfig,
) -> Result<Handle<ClientHandler>, DeployError> {
    let ssh_config = Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(300)),
        ..Default::default()
    });

    let addr = (request.host.as_str(), request.port);
    let connect = client::connect(ssh_config, addr, ClientHandler);
    let mut handle =
        match tokio::time::timeout(Duration::from_secs(config.connect_timeout_secs), connect).await
        {
            Err(_) => {
                return Err(DeployError::Retryable(anyhow!(
                    "连接 {}:{} 超时",
                    request.host,
                    request.port
                )))
            }
            Ok(Err(e)) => {
                return Err(DeployError::Retryable(anyhow!(
                    "连接 {}:{} 失败：{e}",
                    request.host,
                    request.port
                )))
            }
            Ok(Ok(h)) => h,
        };

    let authenticated = match &request.auth {
        DeployAuth::Password { password } => handle
            .authenticate_password(&request.username, password)
            .await
            .map_err(|e| DeployError::Fatal(anyhow!("SSH 密码认证出错：{e}")))?,
        DeployAuth::Key {
            private_key,
            passphrase,
        } => {
            let key = russh::keys::decode_secret_key(private_key, passphrase.as_deref())
                .map_err(|e| DeployError::Fatal(anyhow!("私钥解析失败：{e}")))?;
            let hash = if key.algorithm().is_rsa() {
                handle
                    .best_supported_rsa_hash()
                    .await
                    .map_err(|e| DeployError::Fatal(anyhow!("RSA 签名算法协商失败：{e}")))?
                    .flatten()
            } else {
                None
            };
            let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
            handle
                .authenticate_publickey(&request.username, key)
                .await
                .map_err(|e| DeployError::Fatal(anyhow!("SSH 密钥认证出错：{e}")))?
        }
    };

    if !authenticated.success() {
        // 认证被拒是永久错误：重试只会重复失败。
        return Err(DeployError::Fatal(anyhow!(
            "SSH 认证失败：请检查用户名与凭据"
        )));
    }

    Ok(handle)
}

/// 在远端执行一条命令，逐行把 stdout/stderr 作为 Log 事件回传。
/// 退出码非 0 视为可重试错误（调用方按阶段决定）。
///
/// 若命令需要 sudo 密码（`sudo -S`），密码经 stdin 写入，不出现在命令行/日志。
async fn run_command(
    handle: &Handle<ClientHandler>,
    command: &str,
    sudo_password: Option<&str>,
    tx: &mpsc::Sender<DeployEvent>,
) -> Result<()> {
    let mut channel = handle.channel_open_session().await?;
    channel.exec(true, command).await?;

    // 命令以 sudo -S 提权时，先把密码喂给 stdin（附换行）。
    if let Some(pw) = sudo_password {
        let mut data = pw.as_bytes().to_vec();
        data.push(b'\n');
        channel.data(&data[..]).await?;
        channel.eof().await?;
    }

    let mut exit_code: Option<u32> = None;
    let mut stdout_buf = LineBuffer::new(LogStream::Stdout);
    let mut stderr_buf = LineBuffer::new(LogStream::Stderr);

    loop {
        match channel.wait().await {
            Some(ChannelMsg::Data { data }) => {
                stdout_buf.push(&data, tx).await;
            }
            Some(ChannelMsg::ExtendedData { data, ext }) => {
                // ext == 1 为 stderr。
                if ext == 1 {
                    stderr_buf.push(&data, tx).await;
                } else {
                    stdout_buf.push(&data, tx).await;
                }
            }
            Some(ChannelMsg::ExitStatus { exit_status }) => {
                exit_code = Some(exit_status);
            }
            Some(ChannelMsg::Eof) => {}
            None => break,
            Some(ChannelMsg::Close) => break,
            _ => {}
        }
    }

    stdout_buf.flush(tx).await;
    stderr_buf.flush(tx).await;

    validate_exit_status(exit_code)
}

fn validate_exit_status(exit_code: Option<u32>) -> Result<()> {
    match exit_code {
        Some(0) => Ok(()),
        Some(code) => Err(anyhow!("远程命令退出码 {code}")),
        None => Err(anyhow!("远程命令关闭但未返回退出状态")),
    }
}

/// 把 deploy.sh 内容通过 quoted heredoc 原样写到远端 `script_path`，
/// 避免脚本内容发生变量替换，落地后再 chmod +x。
///
/// heredoc 定界符按序递增，杜绝与脚本中的完整行冲突。
async fn upload_script(
    handle: &Handle<ClientHandler>,
    script_path: &str,
    script_content: &str,
    privilege: &Privilege,
    tx: &mpsc::Sender<DeployEvent>,
) -> Result<()> {
    let command = build_upload_command(script_path, script_content, privilege);
    run_command(handle, &command, privilege.stdin(), tx).await
}

fn build_upload_command(script_path: &str, script_content: &str, privilege: &Privilege) -> String {
    // include_str! preserves checkout line endings. A Windows build can therefore
    // embed CRLF, which Linux bash treats as part of tokens. Normalize before upload.
    let script_content = script_content.replace("\r\n", "\n").replace('\r', "\n");
    let delimiter = heredoc_delim(&script_content);
    let path = shell_quote(script_path);
    let separator = if script_content.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let inner = format!(
        "cat > {path} <<'{delimiter}'\n{script_content}{separator}{delimiter}\nchmod 0755 {path}"
    );
    privilege.wrap_shell(&inner)
}

/// 组装安装命令：`deploy.sh --package … --version … --force …`，
/// 所有参数值单引号转义。密码认证走 `sudo -S`，密钥认证走 `sudo -n`。
fn build_install_command(
    request: &DeployRequest,
    script_path: &str,
    privilege: &Privilege,
) -> String {
    let mut parts: Vec<String> = vec![
        format!("bash {}", shell_quote(script_path)),
        "--force".to_string(),
        format!("--package {}", shell_quote(request.package.as_str())),
        format!("--platform {}", shell_quote("linux-x86_64-musl")),
        format!("--version {}", shell_quote(&request.version)),
        format!("--port {}", shell_quote(&request.api_port.to_string())),
        format!(
            "--gossip-port {}",
            shell_quote(&request.gossip_port.to_string())
        ),
        format!("--service-name {}", shell_quote(&request.service_name)),
    ];

    if !request.peers.is_empty() {
        let joined = request.peers.join(",");
        parts.push(format!("--peers {}", shell_quote(&joined)));
    }
    if let Some(repo) = &request.repo {
        if !repo.is_empty() {
            parts.push(format!("--repo {}", shell_quote(repo)));
        }
    }
    if let Some(proxy) = &request.proxy {
        if !proxy.is_empty() {
            parts.push(format!("--proxy {}", shell_quote(proxy)));
        }
    }

    let inner = parts.join(" ");
    privilege.wrap(&inner)
}

/// 校验命令：确认 systemd 服务处于 active 状态。
fn build_verify_command(service_name: &str, privilege: &Privilege) -> String {
    privilege.wrap(&format!(
        "systemctl is-active --quiet {}",
        shell_quote(service_name)
    ))
}

/// 单引号转义：把值安全嵌入 POSIX shell 单引号串。
/// `'` → `'\''`（闭合引号、转义单引号、重开引号）。
fn shell_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// 生成一个不会出现在载荷中的 heredoc 定界符。
fn heredoc_delim(payload: &str) -> String {
    let mut n = 0u32;
    loop {
        let candidate = format!("OSW_EOF_{n}");
        if !payload.lines().any(|line| line == candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// 参数校验：作为 shell 转义之外的第二道防线，同时挡掉明显非法的输入。
/// 校验失败为永久错误，不重试。
fn validate_request(request: &DeployRequest) -> Result<()> {
    // host：非空、无空白、无 shell 元字符。
    if request.host.trim().is_empty() {
        bail!("主机地址不能为空");
    }
    if request.host.chars().any(|c| c.is_whitespace()) {
        bail!("主机地址不能包含空白字符");
    }
    if request.host.chars().any(is_shell_meta) {
        bail!("主机地址包含非法字符");
    }

    // 端口：1..=65535（u16 已保证上界，这里挡掉 0）。
    if request.port == 0 {
        bail!("SSH 端口无效");
    }
    if request.api_port == 0 {
        bail!("API 端口无效");
    }
    if request.gossip_port == 0 {
        bail!("Gossip 端口无效");
    }

    // username：非空、无空白。
    if request.username.trim().is_empty() {
        bail!("用户名不能为空");
    }
    if request.username.chars().any(|c| c.is_whitespace()) {
        bail!("用户名不能包含空白字符");
    }

    // service_name：仅允许 [A-Za-z0-9._@-]。
    if request.service_name.is_empty() {
        bail!("服务名不能为空");
    }
    if !request
        .service_name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '@' | '-'))
    {
        bail!("服务名只能包含字母、数字和 . _ @ - 字符");
    }

    // install_dir：绝对路径、不含引号。
    if !request.install_dir.starts_with('/') {
        bail!("安装目录必须是绝对路径");
    }
    if request.install_dir.contains(['\'', '"']) {
        bail!("安装目录不能包含引号");
    }
    if request.install_dir.chars().any(char::is_control) {
        bail!("安装目录不能包含控制字符");
    }
    if request.install_dir.contains('%') {
        bail!("安装目录不能包含 systemd specifier 字符 %");
    }

    // version：非空、无空白。
    if request.version.trim().is_empty() {
        bail!("版本号不能为空");
    }
    if request.version.chars().any(|c| c.is_whitespace()) {
        bail!("版本号不能包含空白字符");
    }

    // peers：每项形如 host:port。
    for peer in &request.peers {
        validate_peer(peer)?;
    }

    Ok(())
}

/// 校验单个 peer 形如 `host:port`（IPv6 允许 `[::1]:port`）。
fn validate_peer(peer: &str) -> Result<()> {
    if peer.trim().is_empty() {
        bail!("peer 不能为空");
    }
    if peer.chars().any(|c| c.is_whitespace()) {
        bail!("peer 不能包含空白字符：{peer}");
    }
    // 逗号是 --peers 的分隔符，单个 peer 里不允许。
    if peer.contains(',') {
        bail!("单个 peer 不能包含逗号：{peer}");
    }
    if peer.chars().any(is_shell_meta) {
        bail!("peer 包含非法字符：{peer}");
    }
    let (host, port) = if let Some(bracketed) = peer.strip_prefix('[') {
        let (host, port) = bracketed
            .split_once("]:")
            .ok_or_else(|| anyhow!("IPv6 peer 必须是 [host]:port 形式：{peer}"))?;
        if port.contains(':') {
            bail!("peer 端口无效：{peer}");
        }
        (host, port)
    } else {
        let (host, port) = peer
            .split_once(':')
            .ok_or_else(|| anyhow!("peer 必须是 host:port 形式：{peer}"))?;
        if host.contains(':') || port.contains(':') {
            bail!("IPv6 peer 必须使用方括号：{peer}");
        }
        (host, port)
    };
    if host.is_empty() {
        bail!("peer 主机不能为空：{peer}");
    }
    let port = port
        .parse::<u16>()
        .map_err(|_| anyhow!("peer 端口无效：{peer}"))?;
    if port == 0 {
        bail!("peer 端口无效：{peer}");
    }
    Ok(())
}

/// shell 元字符：这些字符即便被单引号包裹也应拒绝，作为纵深防御。
fn is_shell_meta(c: char) -> bool {
    matches!(
        c,
        '`' | '$'
            | ';'
            | '&'
            | '|'
            | '<'
            | '>'
            | '('
            | ')'
            | '{'
            | '}'
            | '\\'
            | '%'
            | '"'
            | '\''
            | '\n'
            | '\r'
    )
}

/// 把远端字节流按行切分成 Log 事件的缓冲器。
struct LineBuffer {
    stream: LogStream,
    buf: String,
}

impl LineBuffer {
    fn new(stream: LogStream) -> Self {
        Self {
            stream,
            buf: String::new(),
        }
    }

    async fn push(&mut self, data: &[u8], tx: &mpsc::Sender<DeployEvent>) {
        self.buf.push_str(&String::from_utf8_lossy(data));

        loop {
            if let Some(idx) = self.buf.find('\n') {
                if idx < MAX_LOG_LINE_BYTES {
                    let line: String = self.buf.drain(..=idx).collect();
                    let line = line.trim_end_matches(['\n', '\r']).to_string();
                    self.emit(line, tx).await;
                    continue;
                }
            }

            if self.buf.len() <= MAX_LOG_LINE_BYTES {
                break;
            }

            let mut split_at = MAX_LOG_LINE_BYTES;
            while !self.buf.is_char_boundary(split_at) {
                split_at -= 1;
            }
            let mut line: String = self.buf.drain(..split_at).collect();
            line.push_str(" [continued]");
            self.emit(line, tx).await;
        }
    }

    async fn flush(&mut self, tx: &mpsc::Sender<DeployEvent>) {
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            let line = line.trim_end_matches(['\n', '\r']).to_string();
            if !line.is_empty() {
                self.emit(line, tx).await;
            }
        }
    }

    async fn emit(&self, line: String, tx: &mpsc::Sender<DeployEvent>) {
        let _ = tx
            .send(DeployEvent::Log {
                stream: self.stream,
                line,
            })
            .await;
    }
}

/// 内嵌的部署脚本：编译期把仓库根的 deploy.sh 打进二进制，
/// 部署时写到远端 `<install_dir>/deploy.sh`。
const DEPLOY_SCRIPT: &str = include_str!("../deploy.sh");

#[cfg(test)]
mod tests {
    use super::*;

    fn base_request() -> DeployRequest {
        DeployRequest {
            host: "192.168.1.50".to_string(),
            port: 22,
            username: "root".to_string(),
            auth: DeployAuth::Password {
                password: "secret".to_string(),
            },
            package: PackageKind::Full,
            api_port: 7980,
            gossip_port: 7979,
            peers: vec!["10.0.0.2:7979".to_string()],
            service_name: "os-watcher".to_string(),
            install_dir: "/opt/os-watcher".to_string(),
            version: "0.0.8".to_string(),
            repo: None,
            proxy: None,
        }
    }

    #[test]
    fn partial_request_is_normalized_with_runtime_defaults() {
        let json = r#"{
            "host": "192.168.1.50",
            "username": "root",
            "auth": {"type": "password", "password": "dummy"}
        }"#;
        let request = parse_request(
            json,
            &DeployConfig {
                default_install_dir: "/srv/os-watcher".to_string(),
                ..DeployConfig::default()
            },
            "watcher.service",
            "example/os-watcher",
            "10.0.0.1:7979",
        )
        .expect("partial request should normalize");

        assert_eq!(request.port, 22);
        assert_eq!(request.package, PackageKind::Node);
        assert_eq!(request.api_port, 7980);
        assert_eq!(request.gossip_port, 7979);
        assert_eq!(request.service_name, "watcher.service");
        assert_eq!(request.install_dir, "/srv/os-watcher");
        assert_eq!(request.version, "latest");
        assert_eq!(request.repo.as_deref(), Some("example/os-watcher"));
        assert_eq!(request.peers, ["10.0.0.1:7979"]);
    }

    #[test]
    fn normalization_preserves_explicit_invalid_zero_port() {
        let json = r#"{
            "host": "192.168.1.50",
            "port": 0,
            "username": "root",
            "auth": {"type": "password", "password": "dummy"}
        }"#;
        let request = parse_request(
            json,
            &DeployConfig::default(),
            "os-watcher",
            "example/os-watcher",
            "10.0.0.1:7979",
        )
        .expect("shape should deserialize before validation");

        assert_eq!(request.port, 0);
        assert!(validate_request(&request).is_err());
    }

    #[test]
    fn shell_quote_wraps_plain_value() {
        assert_eq!(shell_quote("os-watcher"), "'os-watcher'");
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        // a'b → 'a'\''b'
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn shell_quote_neutralizes_injection_attempt() {
        // 试图注入命令的值应被完整包裹，不产生可执行的 shell 结构。
        let quoted = shell_quote("'; rm -rf / #");
        assert!(quoted.starts_with('\''));
        assert!(quoted.ends_with('\''));
        // 原始的裸单引号都被转义成 '\''，不存在未转义的裸 '。
        assert!(!quoted.contains("; rm -rf / #'") || quoted.contains("'\\''"));
    }

    #[test]
    fn install_command_contains_expected_flags() {
        let req = base_request();
        let cmd = build_install_command(
            &req,
            "/opt/os-watcher/deploy.sh",
            &Privilege::PasswordSudo("dummy".to_string()),
        );
        assert!(cmd.contains("--force"));
        assert!(cmd.contains("--package 'full'"));
        assert!(cmd.contains("--platform 'linux-x86_64-musl'"));
        assert!(cmd.contains("--version '0.0.8'"));
        assert!(cmd.contains("--port '7980'"));
        assert!(cmd.contains("--gossip-port '7979'"));
        assert!(cmd.contains("--service-name 'os-watcher'"));
        assert!(cmd.contains("--peers '10.0.0.2:7979'"));
        assert!(cmd.contains("bash '/opt/os-watcher/deploy.sh'"));
    }

    #[test]
    fn install_command_omits_empty_optionals() {
        let mut req = base_request();
        req.peers = vec![];
        req.repo = None;
        req.proxy = None;
        let cmd = build_install_command(
            &req,
            "/opt/os-watcher/deploy.sh",
            &Privilege::PasswordSudo("dummy".to_string()),
        );
        assert!(!cmd.contains("--peers"));
        assert!(!cmd.contains("--repo"));
        assert!(!cmd.contains("--proxy"));
    }

    #[test]
    fn install_command_includes_repo_and_proxy_when_present() {
        let mut req = base_request();
        req.repo = Some("acme/os-watcher".to_string());
        req.proxy = Some("http://127.0.0.1:7890".to_string());
        let cmd = build_install_command(&req, "/opt/os-watcher/deploy.sh", &Privilege::KeySudo);
        assert!(cmd.contains("--repo 'acme/os-watcher'"));
        assert!(cmd.contains("--proxy 'http://127.0.0.1:7890'"));
    }

    #[test]
    fn root_privilege_runs_commands_directly() {
        let privilege = Privilege::for_request(&base_request());
        assert_eq!(privilege.wrap("whoami"), "whoami");
        assert!(privilege.stdin().is_none());
    }

    #[test]
    fn non_root_password_auth_uses_password_sudo() {
        let mut request = base_request();
        request.username = "deployer".to_string();
        let privilege = Privilege::for_request(&request);
        assert_eq!(privilege.wrap("whoami"), "sudo -S -p '' whoami");
        assert_eq!(privilege.stdin(), Some("secret"));
    }

    #[test]
    fn non_root_key_auth_uses_noninteractive_sudo() {
        let mut request = base_request();
        request.username = "deployer".to_string();
        request.auth = DeployAuth::Key {
            private_key: "dummy key".to_string(),
            passphrase: None,
        };
        let privilege = Privilege::for_request(&request);
        assert_eq!(privilege.wrap("whoami"), "sudo -n whoami");
        assert!(privilege.stdin().is_none());
    }

    #[test]
    fn verify_command_checks_service_is_active() {
        let cmd = build_verify_command("os-watcher", &Privilege::PasswordSudo("dummy".to_string()));
        assert!(cmd.contains("systemctl is-active --quiet 'os-watcher'"));
        assert!(cmd.starts_with("sudo -S -p ''"));
    }

    #[test]
    fn heredoc_delimiter_is_unique_against_payload() {
        // 载荷里塞入默认定界符，生成器必须让步到别的编号。
        let payload = "OSW_EOF_0\necho payload";
        let delim = heredoc_delim(payload);
        assert!(!payload.contains(&delim));
        assert_ne!(delim, "OSW_EOF_0");
    }

    #[test]
    fn heredoc_delimiter_default_when_no_conflict() {
        assert_eq!(heredoc_delim("plain script payload"), "OSW_EOF_0");
    }

    #[test]
    fn upload_command_uses_quoted_heredoc_with_script_content() {
        let script = "#!/bin/sh\necho hello\n";
        let command = build_upload_command("/opt/os watcher/deploy.sh", script, &Privilege::Root);

        assert!(command.contains("<<'OSW_EOF_0'"));
        assert!(command.contains(script));
        assert!(command.contains("> '/opt/os watcher/deploy.sh'"));
        assert!(command.contains("chmod 0755 '/opt/os watcher/deploy.sh'"));
        assert!(!command.contains("base64"));
    }

    #[test]
    fn upload_command_normalizes_windows_line_endings_for_linux() {
        let script = "#!/bin/bash\r\nset -Eeuo pipefail\r\necho ready\r\n";
        let command = build_upload_command("/opt/os-watcher/deploy.sh", script, &Privilege::Root);

        assert!(command.contains("#!/bin/bash\nset -Eeuo pipefail\necho ready\n"));
        assert!(!command.contains('\r'));
    }

    #[tokio::test]
    async fn line_buffer_limits_unterminated_remote_output() {
        let (tx, mut rx) = mpsc::channel(2);
        let mut buffer = LineBuffer::new(LogStream::Stdout);

        buffer.push(&vec![b'x'; 32 * 1024], &tx).await;

        assert!(buffer.buf.len() <= MAX_LOG_LINE_BYTES);
        assert!(
            rx.try_recv().is_ok(),
            "overflow should be emitted as a log chunk"
        );
    }

    #[test]
    fn validation_accepts_well_formed_request() {
        assert!(validate_request(&base_request()).is_ok());
    }

    #[test]
    fn validation_rejects_host_with_shell_metacharacters() {
        let mut req = base_request();
        req.host = "10.0.0.1; rm -rf /".to_string();
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn validation_rejects_empty_host() {
        let mut req = base_request();
        req.host = "   ".to_string();
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn validation_rejects_relative_install_dir() {
        let mut req = base_request();
        req.install_dir = "opt/os-watcher".to_string();
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn validation_rejects_install_dir_with_quote() {
        let mut req = base_request();
        req.install_dir = "/opt/os'watcher".to_string();
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn validation_rejects_install_dir_with_double_quote() {
        let mut req = base_request();
        req.install_dir = "/opt/os\"watcher".to_string();
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn validation_rejects_install_dir_with_control_characters() {
        let mut req = base_request();
        req.install_dir = "/opt/os-watcher\nEnvironment=INJECTED=1".to_string();
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn validation_rejects_install_dir_with_systemd_specifier() {
        let mut req = base_request();
        req.install_dir = "/opt/os-watcher-%n".to_string();
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn validation_rejects_peer_with_systemd_specifier() {
        let mut req = base_request();
        req.peers = vec!["node-%n.example:7979".to_string()];
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn validation_rejects_bad_service_name() {
        let mut req = base_request();
        req.service_name = "os watcher!".to_string();
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn validation_rejects_zero_port() {
        let mut req = base_request();
        req.port = 0;
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn validation_rejects_malformed_peer() {
        let mut req = base_request();
        req.peers = vec!["not-a-peer".to_string()];
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn validation_rejects_peer_with_bad_port() {
        let mut req = base_request();
        req.peers = vec!["10.0.0.2:notaport".to_string()];
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn validation_rejects_peer_with_empty_host() {
        let mut req = base_request();
        req.peers = vec![":7979".to_string()];
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn validation_rejects_peer_with_zero_port() {
        let mut req = base_request();
        req.peers = vec!["10.0.0.2:0".to_string()];
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn validation_rejects_unbracketed_ipv6_or_extra_colons() {
        let mut req = base_request();
        req.peers = vec!["host:extra:7979".to_string()];
        assert!(validate_request(&req).is_err());
        req.peers = vec!["::1:7979".to_string()];
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn validation_accepts_ipv6_peer() {
        let mut req = base_request();
        req.peers = vec!["[::1]:7979".to_string()];
        // 注意：[ ] 不是 shell 元字符集合的一员，应通过。
        assert!(validate_request(&req).is_ok());
    }

    #[test]
    fn missing_remote_exit_status_is_an_error() {
        assert!(validate_exit_status(None).is_err());
        assert!(validate_exit_status(Some(0)).is_ok());
        assert!(validate_exit_status(Some(1)).is_err());
    }

    #[test]
    fn deploy_event_progress_serializes_with_type_tag() {
        let ev = DeployEvent::Progress {
            step: DeployStep::Connecting,
            message: "hi".to_string(),
        };
        let json = serde_json::to_value(&ev).expect("serialize");
        assert_eq!(json["type"], "progress");
        assert_eq!(json["step"], "connecting");
        assert_eq!(json["message"], "hi");
    }

    #[test]
    fn deploy_event_log_serializes_stream_lowercase() {
        let ev = DeployEvent::Log {
            stream: LogStream::Stderr,
            line: "boom".to_string(),
        };
        let json = serde_json::to_value(&ev).expect("serialize");
        assert_eq!(json["type"], "log");
        assert_eq!(json["stream"], "stderr");
        assert_eq!(json["line"], "boom");
    }

    #[test]
    fn deploy_event_retry_serializes_fields() {
        let ev = DeployEvent::Retry {
            attempt: 2,
            max: 3,
            message: "retrying".to_string(),
        };
        let json = serde_json::to_value(&ev).expect("serialize");
        assert_eq!(json["type"], "retry");
        assert_eq!(json["attempt"], 2);
        assert_eq!(json["max"], 3);
    }

    #[test]
    fn deploy_event_success_and_error_tags() {
        let ok = serde_json::to_value(&DeployEvent::Success {
            message: "done".to_string(),
        })
        .expect("serialize");
        assert_eq!(ok["type"], "success");

        let err = serde_json::to_value(&DeployEvent::Error {
            message: "nope".to_string(),
        })
        .expect("serialize");
        assert_eq!(err["type"], "error");
    }

    #[test]
    fn deploy_request_deserializes_password_auth() {
        let json = r#"{
            "host": "1.2.3.4", "port": 22, "username": "root",
            "auth": {"type": "password", "password": "pw"},
            "package": "node", "api_port": 7980, "gossip_port": 7979,
            "peers": [], "service_name": "os-watcher",
            "install_dir": "/opt/os-watcher", "version": "0.0.8",
            "repo": null, "proxy": null
        }"#;
        let req: DeployRequest = serde_json::from_str(json).expect("deserialize");
        assert_eq!(req.host, "1.2.3.4");
        assert!(matches!(req.auth, DeployAuth::Password { .. }));
        assert_eq!(req.package, PackageKind::Node);
    }

    #[test]
    fn deploy_request_deserializes_key_auth() {
        let json = r#"{
            "host": "1.2.3.4", "port": 22, "username": "root",
            "auth": {"type": "key", "private_key": "KEY", "passphrase": null},
            "package": "full", "api_port": 7980, "gossip_port": 7979,
            "peers": [], "service_name": "os-watcher",
            "install_dir": "/opt/os-watcher", "version": "0.0.8",
            "repo": null, "proxy": null
        }"#;
        let req: DeployRequest = serde_json::from_str(json).expect("deserialize");
        assert!(matches!(req.auth, DeployAuth::Key { .. }));
    }
}
