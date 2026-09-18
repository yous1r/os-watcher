use std::path::Path as FsPath;

use anyhow::Result;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use chrono::{DateTime, Utc};
use futures::{Sink, SinkExt, Stream, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};
use tracing::{info, warn};
use uuid::Uuid;

use crate::auth::{self, AuthManager, LoginError};
use crate::config::{DeployConfig, UpgradeConfig};
use crate::deploy::{self, DeployRequest};
use crate::notify::{self, NotificationService};
use crate::state::SharedState;
use crate::storage::Database;
use crate::types::*;
use crate::upgrade::{UpgradeManager, UpgradeRequest};

const DEPLOY_EVENT_BUFFER_CAPACITY: usize = 256;

/// Message shown when authentication is on but no password was configured. It is
/// deliberately a 503, not a silent allow: management endpoints must never be
/// reachable just because `[auth]` is missing from the config.
const AUTH_UNCONFIGURED_MESSAGE: &str =
    "管理鉴权已启用但未配置口令：请在 config.toml 的 [auth] password 中设置";

#[derive(Clone)]
pub struct ApiState {
    nodes: SharedState,
    upgrade: UpgradeManager,
    upgrade_config: UpgradeConfig,
    deploy: DeployConfig,
    /// 本机 gossip 广播地址，用于在部署请求 peers 为空时兜底注入。
    local_gossip_addr: String,
    db: std::sync::Arc<Database>,
    notify: NotificationService,
    auth: AuthManager,
    /// `[storage] alert_history_minutes`, surfaced so the panel's 最近恢复 hint
    /// shows the window the server actually enforces.
    alert_history_minutes: u64,
}

impl ApiState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        nodes: SharedState,
        upgrade: UpgradeManager,
        upgrade_config: UpgradeConfig,
        deploy: DeployConfig,
        local_gossip_addr: String,
        db: std::sync::Arc<Database>,
        notify: NotificationService,
        auth: AuthManager,
        alert_history_minutes: u64,
    ) -> Self {
        Self {
            nodes,
            upgrade,
            upgrade_config,
            deploy,
            local_gossip_addr,
            db,
            notify,
            auth,
            alert_history_minutes,
        }
    }
}

#[derive(Debug, Serialize)]
struct ApiResponse<T: Serialize> {
    success: bool,
    data: T,
}

impl<T: Serialize> ApiResponse<T> {
    fn ok(data: T) -> Json<Self> {
        Json(Self {
            success: true,
            data,
        })
    }
}

#[derive(Debug, Serialize)]
struct ApiError {
    success: bool,
    error: String,
}

impl ApiError {
    fn not_found(msg: impl Into<String>) -> (StatusCode, Json<Self>) {
        Self::with_status(StatusCode::NOT_FOUND, msg)
    }

    fn bad_request(msg: impl Into<String>) -> (StatusCode, Json<Self>) {
        Self::with_status(StatusCode::BAD_REQUEST, msg)
    }

    fn forbidden(msg: impl Into<String>) -> (StatusCode, Json<Self>) {
        Self::with_status(StatusCode::FORBIDDEN, msg)
    }

    fn internal(msg: impl Into<String>) -> (StatusCode, Json<Self>) {
        Self::with_status(StatusCode::INTERNAL_SERVER_ERROR, msg)
    }

    fn with_status(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<Self>) {
        (
            status,
            Json(Self {
                success: false,
                error: msg.into(),
            }),
        )
    }
}

type ApiRejection = (StatusCode, Json<ApiError>);

/// Reject requests a browser marked as cross-site.
///
/// Cookies are already `SameSite=Strict`, but the deploy WebSocket handshake
/// cannot set custom headers, so this header check is the CSRF backstop for it.
/// Non-browser clients omit the header entirely and are allowed through.
fn reject_cross_site(headers: &HeaderMap) -> Result<(), ApiRejection> {
    if let Some(site) = headers.get("sec-fetch-site") {
        if site.as_bytes() == b"cross-site" {
            return Err(ApiError::forbidden("跨站请求被拒绝"));
        }
    }
    Ok(())
}

/// Extractor for management endpoints: monitoring stays readable by guests, every
/// action that mutates state or handles credentials requires an admin session.
struct Admin;

#[axum::async_trait]
impl axum::extract::FromRequestParts<ApiState> for Admin {
    type Rejection = ApiRejection;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &ApiState,
    ) -> Result<Self, Self::Rejection> {
        reject_cross_site(&parts.headers)?;

        if !state.auth.enabled() {
            return Ok(Admin);
        }

        let authenticated = match auth::session_token_from(&parts.headers) {
            Some(token) => state.auth.validate(&token).await,
            None => false,
        };

        if authenticated {
            return Ok(Admin);
        }

        if !state.auth.password_configured() {
            return Err(ApiError::with_status(
                StatusCode::SERVICE_UNAVAILABLE,
                AUTH_UNCONFIGURED_MESSAGE,
            ));
        }

        Err(ApiError::with_status(
            StatusCode::UNAUTHORIZED,
            "需要管理员登录",
        ))
    }
}

/// Attach a `Set-Cookie` header to a response.
fn with_set_cookie(mut response: Response, cookie: &str) -> Response {
    match HeaderValue::from_str(cookie) {
        Ok(value) => {
            response.headers_mut().insert(header::SET_COOKIE, value);
        }
        Err(error) => warn!("Failed to build Set-Cookie header: {error}"),
    }
    response
}

/// GET /api/v1/nodes — list all known nodes
async fn list_nodes(State(api): State<ApiState>) -> impl IntoResponse {
    let s = api.nodes.read().await;
    let nodes: Vec<NodeInfo> = s.peers.values().cloned().collect();
    ApiResponse::ok(nodes)
}

/// GET /api/v1/nodes/:node_id — get info about a specific node
async fn get_node(State(api): State<ApiState>, Path(node_id): Path<String>) -> impl IntoResponse {
    let id = match uuid::Uuid::parse_str(&node_id) {
        Ok(id) => id,
        Err(_) => return Err(ApiError::not_found("Invalid node ID")),
    };

    let s = api.nodes.read().await;
    match s.peers.get(&id) {
        Some(node) => Ok(ApiResponse::ok(node.clone()).into_response()),
        None => Err(ApiError::not_found(format!("Node {} not found", node_id))),
    }
}

/// GET /api/v1/nodes/:node_id/metrics — get latest metrics for a node
async fn get_node_metrics(
    State(api): State<ApiState>,
    Path(node_id): Path<String>,
) -> impl IntoResponse {
    let id = match uuid::Uuid::parse_str(&node_id) {
        Ok(id) => id,
        Err(_) => return Err(ApiError::not_found("Invalid node ID")),
    };

    let s = api.nodes.read().await;
    if !s.peers.contains_key(&id) {
        return Err(ApiError::not_found(format!("Node {} not found", node_id)));
    }

    match s.metrics.get(&id) {
        Some(metrics) => Ok(ApiResponse::ok(metrics.clone()).into_response()),
        None => Err(ApiError::not_found("No metrics available for this node")),
    }
}

/// GET /api/v1/metrics — get latest metrics for all nodes
async fn get_all_metrics(State(api): State<ApiState>) -> impl IntoResponse {
    let s = api.nodes.read().await;
    let snapshots = s.node_snapshots();
    ApiResponse::ok(snapshots)
}

/// GET /api/v1/alerts — get active alerts
async fn get_alerts(State(api): State<ApiState>) -> impl IntoResponse {
    let s = api.nodes.read().await;
    let alerts: Vec<Alert> = s.active_alerts().into_iter().cloned().collect();
    ApiResponse::ok(alerts)
}

/// GET /api/v1/local — get local node info and metrics
async fn get_local(State(api): State<ApiState>) -> impl IntoResponse {
    let s = api.nodes.read().await;
    let local_id = s.local_node.id;
    let snapshot = NodeSnapshot {
        info: s.local_node.clone(),
        metrics: s.metrics.get(&local_id).cloned(),
    };
    ApiResponse::ok(snapshot)
}

/// GET /api/v1/version — get local version and cached latest release info
async fn get_version(State(api): State<ApiState>) -> impl IntoResponse {
    ApiResponse::ok(api.upgrade.version_info().await)
}

/// GET /api/v1/upgrade — get the latest self-upgrade status
async fn get_upgrade_status(State(api): State<ApiState>) -> impl IntoResponse {
    ApiResponse::ok(api.upgrade.upgrade_status().await)
}

/// POST /api/v1/upgrade — start a self-upgrade in the background
async fn trigger_upgrade(
    State(api): State<ApiState>,
    Json(request): Json<UpgradeRequest>,
) -> impl IntoResponse {
    match api.upgrade.trigger_upgrade(request).await {
        Ok(status) => Ok(ApiResponse::ok(status)),
        Err(err) => {
            let message = err.to_string();
            let status = if message.contains("already running") {
                StatusCode::CONFLICT
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            };
            Err(ApiError::with_status(status, message))
        }
    }
}

/// GET /api/v1/health — health check
async fn health_check() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "ok",
        "service": "os-watcher"
    }))
}

/// GET /api/v1/nodes/deploy — 远程节点部署的 WebSocket 端点。
///
/// 协议：客户端建连后发送一帧 [`DeployRequest`] JSON；服务端把部署过程的
/// [`deploy::DeployEvent`] 逐条以文本帧回传，遇终态（success/error）后关闭。
///
/// 需要管理员会话：该端点接收 SSH 凭据并执行提权命令。`WebSocketUpgrade`
/// 必须是最后一个提取器，`Admin` 放在它之前。
async fn deploy_ws(
    State(api): State<ApiState>,
    _admin: Admin,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_deploy_socket(socket, api))
}

/// 处理已升级的部署 WebSocket 连接：读首帧请求，跑部署，转发事件。
async fn handle_deploy_socket(mut socket: WebSocket, api: ApiState) {
    if !api.deploy.enabled {
        send_error_and_close(
            &mut socket,
            "远程部署已禁用（deploy.enabled = false）".to_string(),
        )
        .await;
        return;
    }

    // 读取首帧：必须是一段 DeployRequest JSON 文本。
    let first = match socket.recv().await {
        Some(Ok(Message::Text(text))) => text,
        Some(Ok(Message::Binary(bytes))) => match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => {
                send_error_and_close(&mut socket, "首帧不是合法 UTF-8 文本".to_string()).await;
                return;
            }
        },
        // 客户端在发请求前就断开 / 关闭，无事可做。
        _ => return,
    };

    let request: DeployRequest = match deploy::parse_request(
        &first,
        &api.deploy,
        &api.upgrade_config.service_name,
        &api.upgrade_config.github_repo,
        &api.local_gossip_addr,
    ) {
        Ok(req) => req,
        Err(e) => {
            send_error_and_close(&mut socket, format!("部署请求解析失败：{e}")).await;
            return;
        }
    };

    // 有界通道把 WebSocket 的消费速度反压到 SSH 读取，避免高速日志撑爆内存。
    let (tx, rx) = mpsc::channel::<deploy::DeployEvent>(DEPLOY_EVENT_BUFFER_CAPACITY);
    let deploy_cfg = api.deploy.clone();
    let local_gossip_addr = api.local_gossip_addr.clone();
    let deploy_task = tokio::spawn(async move {
        deploy::run_deploy(request, deploy_cfg, local_gossip_addr, tx).await;
    });

    let (sender, receiver) = socket.split();
    relay_deploy_events(sender, receiver, rx, deploy_task).await;
}

async fn relay_deploy_events<S, R, SendError, ReceiveError>(
    mut sender: S,
    mut receiver: R,
    mut events: mpsc::Receiver<deploy::DeployEvent>,
    deploy_task: tokio::task::JoinHandle<()>,
) where
    S: Sink<Message, Error = SendError> + Unpin,
    R: Stream<Item = std::result::Result<Message, ReceiveError>> + Unpin,
{
    loop {
        tokio::select! {
            client_message = receiver.next() => {
                match client_message {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => {
                        deploy_task.abort();
                        return;
                    }
                    Some(Ok(_)) => {}
                }
            }
            event = events.recv() => {
                match event {
                    Some(event) => {
                        let text = serialize_event(&event);
                        if sender.send(Message::Text(text)).await.is_err() {
                            deploy_task.abort();
                            return;
                        }
                    }
                    None => {
                        let _ = sender.send(Message::Close(None)).await;
                        return;
                    }
                }
            }
        }
    }
}

/// 把一个部署事件序列化成 JSON 文本帧发给前端。
async fn send_event(
    socket: &mut WebSocket,
    event: &deploy::DeployEvent,
) -> Result<(), axum::Error> {
    let text = serialize_event(event);
    socket.send(Message::Text(text)).await
}

async fn send_error_and_close(socket: &mut WebSocket, message: String) {
    let _ = send_event(socket, &deploy::DeployEvent::Error { message }).await;
    let _ = socket.send(Message::Close(None)).await;
}

fn serialize_event(event: &deploy::DeployEvent) -> String {
    serde_json::to_string(event)
        .unwrap_or_else(|_| r#"{"type":"error","message":"事件序列化失败"}"#.to_string())
}

/// GET /api/v1/alerts/history — recently resolved alerts
async fn get_alert_history(
    State(api): State<ApiState>,
    Query(query): Query<AlertHistoryQuery>,
) -> impl IntoResponse {
    let limit = query.limit.unwrap_or(50).clamp(1, 200);

    match api.db.recent_resolved_alerts(limit).await {
        Ok(alerts) => Ok(ApiResponse::ok(alerts)),
        Err(error) => {
            warn!("Failed to read alert history: {error}");
            Err(ApiError::internal("读取告警历史失败"))
        }
    }
}

#[derive(Debug, Deserialize)]
struct AlertHistoryQuery {
    limit: Option<i64>,
}

/// GET /api/v1/alerts/retention — how long resolved alerts are kept
async fn get_alert_retention(State(api): State<ApiState>) -> impl IntoResponse {
    ApiResponse::ok(AlertRetention {
        history_minutes: api.alert_history_minutes,
    })
}

#[derive(Debug, Serialize)]
struct AlertRetention {
    /// `[storage] alert_history_minutes`; `0` deletes entries as they resolve.
    history_minutes: u64,
}

/// GET /api/v1/auth/session — whether management actions need a login
async fn get_session(State(api): State<ApiState>, headers: HeaderMap) -> impl IntoResponse {
    let authenticated = if !api.auth.enabled() {
        true
    } else {
        match auth::session_token_from(&headers) {
            Some(token) => api.auth.validate(&token).await,
            None => false,
        }
    };

    ApiResponse::ok(serde_json::json!({
        "auth_required": api.auth.enabled(),
        "authenticated": authenticated,
    }))
}

#[derive(Debug, Deserialize)]
struct LoginRequest {
    password: String,
}

/// POST /api/v1/auth/login — exchange the admin password for a session cookie
async fn post_login(
    State(api): State<ApiState>,
    headers: HeaderMap,
    Json(request): Json<LoginRequest>,
) -> Result<Response, ApiRejection> {
    reject_cross_site(&headers)?;

    match api.auth.login(&request.password).await {
        Ok(token) => {
            let cookie = auth::session_cookie(&token, api.auth.session_ttl_secs());
            Ok(with_set_cookie(
                ApiResponse::ok(serde_json::json!({ "authenticated": true })).into_response(),
                &cookie,
            ))
        }
        Err(LoginError::BadPassword) => Err(ApiError::with_status(
            StatusCode::UNAUTHORIZED,
            "管理口令错误",
        )),
        Err(LoginError::RateLimited) => Err(ApiError::with_status(
            StatusCode::TOO_MANY_REQUESTS,
            "登录尝试次数过多，请稍后再试",
        )),
        Err(LoginError::Disabled) => Err(ApiError::with_status(
            StatusCode::SERVICE_UNAVAILABLE,
            "未启用管理鉴权，无需登录",
        )),
        Err(LoginError::NotConfigured) => Err(ApiError::with_status(
            StatusCode::SERVICE_UNAVAILABLE,
            AUTH_UNCONFIGURED_MESSAGE,
        )),
    }
}

/// POST /api/v1/auth/logout — drop the current session
async fn post_logout(State(api): State<ApiState>, headers: HeaderMap) -> Response {
    if let Err(rejection) = reject_cross_site(&headers) {
        return rejection.into_response();
    }

    if let Some(token) = auth::session_token_from(&headers) {
        api.auth.logout(&token).await;
    }

    with_set_cookie(
        ApiResponse::ok(serde_json::json!({ "authenticated": false })).into_response(),
        &auth::cleared_session_cookie(),
    )
}

#[derive(Debug, Deserialize)]
struct NotifyChannelRequest {
    name: String,
    enabled: bool,
    min_severity: NotifySeverity,
    config: ChannelConfig,
}

/// Validate a channel request and fold it into a storable channel. An empty
/// `server_url` means "use the configured default" (`[notify] server_url`).
fn build_channel(
    id: Uuid,
    created_at: DateTime<Utc>,
    request: NotifyChannelRequest,
    default_server_url: &str,
) -> Result<NotifyChannel, ApiRejection> {
    let name = request.name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError::bad_request("渠道名称不能为空"));
    }

    let ChannelConfig::Bark(bark) = request.config;
    let submitted_url = notify::normalize_server_url(&bark.server_url);
    let bark = BarkChannelConfig {
        server_url: if submitted_url.is_empty() {
            default_server_url.to_string()
        } else {
            submitted_url
        },
        device_key: bark.device_key.trim().to_string(),
        encryption: bark.encryption,
    };
    notify::validate_bark_config(&bark).map_err(ApiError::bad_request)?;

    Ok(NotifyChannel {
        id,
        name,
        enabled: request.enabled,
        min_severity: request.min_severity,
        config: ChannelConfig::Bark(bark),
        created_at,
        updated_at: Utc::now(),
        last_sent_at: None,
        last_error: None,
    })
}

/// GET /api/v1/notify/defaults — server address new channels default to
async fn get_notify_defaults(State(api): State<ApiState>, _admin: Admin) -> impl IntoResponse {
    ApiResponse::ok(serde_json::json!({
        "server_url": api.notify.default_server_url(),
    }))
}

/// GET /api/v1/notify/channels — list push channels
async fn list_channels(State(api): State<ApiState>, _admin: Admin) -> impl IntoResponse {
    match api.db.list_notify_channels().await {
        Ok(channels) => Ok(ApiResponse::ok(channels)),
        Err(error) => {
            warn!("Failed to list notify channels: {error}");
            Err(ApiError::internal("读取推送渠道失败"))
        }
    }
}

/// POST /api/v1/notify/channels — create a push channel
async fn create_channel(
    State(api): State<ApiState>,
    _admin: Admin,
    Json(request): Json<NotifyChannelRequest>,
) -> impl IntoResponse {
    let channel = match build_channel(
        Uuid::new_v4(),
        Utc::now(),
        request,
        api.notify.default_server_url(),
    ) {
        Ok(channel) => channel,
        Err(rejection) => return Err(rejection),
    };

    match api.db.upsert_notify_channel(&channel).await {
        Ok(()) => Ok(ApiResponse::ok(channel)),
        Err(error) => {
            warn!("Failed to create notify channel: {error}");
            Err(ApiError::internal("保存推送渠道失败"))
        }
    }
}

/// PUT /api/v1/notify/channels/:id — update a push channel
async fn update_channel(
    State(api): State<ApiState>,
    _admin: Admin,
    Path(id): Path<String>,
    Json(request): Json<NotifyChannelRequest>,
) -> impl IntoResponse {
    let id = match Uuid::parse_str(&id) {
        Ok(id) => id,
        Err(_) => return Err(ApiError::not_found("渠道不存在")),
    };

    let existing = match api.db.get_notify_channel(&id).await {
        Ok(Some(channel)) => channel,
        Ok(None) => return Err(ApiError::not_found("渠道不存在")),
        Err(error) => {
            warn!("Failed to load notify channel {id}: {error}");
            return Err(ApiError::internal("读取推送渠道失败"));
        }
    };

    let mut updated = match build_channel(
        existing.id,
        existing.created_at,
        request,
        api.notify.default_server_url(),
    ) {
        Ok(channel) => channel,
        Err(rejection) => return Err(rejection),
    };
    updated.last_sent_at = existing.last_sent_at;
    updated.last_error = existing.last_error;

    match api.db.upsert_notify_channel(&updated).await {
        Ok(()) => Ok(ApiResponse::ok(updated)),
        Err(error) => {
            warn!("Failed to update notify channel {id}: {error}");
            Err(ApiError::internal("保存推送渠道失败"))
        }
    }
}

/// DELETE /api/v1/notify/channels/:id — remove a push channel
async fn delete_channel(
    State(api): State<ApiState>,
    _admin: Admin,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let id = match Uuid::parse_str(&id) {
        Ok(id) => id,
        Err(_) => return Err(ApiError::not_found("渠道不存在")),
    };

    match api.db.delete_notify_channel(&id).await {
        Ok(0) => Err(ApiError::not_found("渠道不存在")),
        Ok(_) => Ok(ApiResponse::ok(serde_json::json!({ "deleted": true }))),
        Err(error) => {
            warn!("Failed to delete notify channel {id}: {error}");
            Err(ApiError::internal("删除推送渠道失败"))
        }
    }
}

/// POST /api/v1/notify/channels/:id/test — send a test push
async fn test_channel(
    State(api): State<ApiState>,
    _admin: Admin,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let id = match Uuid::parse_str(&id) {
        Ok(id) => id,
        Err(_) => return Err(ApiError::not_found("渠道不存在")),
    };

    let channel = match api.db.get_notify_channel(&id).await {
        Ok(Some(channel)) => channel,
        Ok(None) => return Err(ApiError::not_found("渠道不存在")),
        Err(error) => {
            warn!("Failed to load notify channel {id}: {error}");
            return Err(ApiError::internal("读取推送渠道失败"));
        }
    };

    match api.notify.send_test(&channel).await {
        Ok(()) => Ok(ApiResponse::ok(serde_json::json!({ "sent": true }))),
        Err(error) => Err(ApiError::with_status(
            StatusCode::BAD_GATEWAY,
            format!("推送失败：{error}"),
        )),
    }
}

pub fn create_router(api_state: ApiState, web_dir: Option<&str>) -> Router {
    let api = Router::new()
        .route("/api/v1/health", get(health_check))
        .route("/api/v1/local", get(get_local))
        .route("/api/v1/version", get(get_version))
        .route(
            "/api/v1/upgrade",
            get(get_upgrade_status).post(trigger_upgrade),
        )
        .route("/api/v1/nodes", get(list_nodes))
        .route("/api/v1/nodes/deploy", get(deploy_ws))
        .route("/api/v1/nodes/:node_id", get(get_node))
        .route("/api/v1/nodes/:node_id/metrics", get(get_node_metrics))
        .route("/api/v1/metrics", get(get_all_metrics))
        .route("/api/v1/alerts", get(get_alerts))
        .route("/api/v1/alerts/history", get(get_alert_history))
        .route("/api/v1/alerts/retention", get(get_alert_retention))
        .route("/api/v1/auth/session", get(get_session))
        .route("/api/v1/auth/login", post(post_login))
        .route("/api/v1/auth/logout", post(post_logout))
        .route("/api/v1/notify/defaults", get(get_notify_defaults))
        .route(
            "/api/v1/notify/channels",
            get(list_channels).post(create_channel),
        )
        .route(
            "/api/v1/notify/channels/:id",
            put(update_channel).delete(delete_channel),
        )
        .route("/api/v1/notify/channels/:id/test", post(test_channel))
        .with_state(api_state);

    let mut app = api;

    // Optionally serve the built web dashboard (SPA). Unknown paths fall back
    // to index.html so client-side routing works.
    if let Some(dir) = web_dir {
        if FsPath::new(dir).is_dir() {
            let index = format!("{}/index.html", dir);
            let serve = ServeDir::new(dir).not_found_service(ServeFile::new(index));
            app = app.fallback_service(serve);
            info!("Web dashboard enabled, serving static files from '{}'", dir);
        } else {
            warn!(
                "Web dashboard requested but directory '{}' not found; \
                 build the frontend first (cd web && npm install && npm run build)",
                dir
            );
        }
    }

    app.layer(CorsLayer::permissive())
}

#[allow(clippy::too_many_arguments)]
pub async fn run_api_server(
    state: SharedState,
    upgrade: UpgradeManager,
    upgrade_config: UpgradeConfig,
    deploy: DeployConfig,
    local_gossip_addr: String,
    db: std::sync::Arc<Database>,
    notify: NotificationService,
    auth: AuthManager,
    alert_history_minutes: u64,
    bind_addr: &str,
    port: u16,
    web_dir: Option<String>,
) -> Result<()> {
    let addr = format!("{}:{}", bind_addr, port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("API server listening on http://{}", addr);

    let api_state = ApiState::new(
        state,
        upgrade,
        upgrade_config,
        deploy,
        local_gossip_addr,
        db,
        notify,
        auth,
        alert_history_minutes,
    );
    axum::serve(listener, create_router(api_state, web_dir.as_deref())).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use chrono::Utc;
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::connect_async;
    use tower::ServiceExt;
    use uuid::Uuid;

    struct NotifyOnDrop(Option<tokio::sync::oneshot::Sender<()>>);

    impl Drop for NotifyOnDrop {
        fn drop(&mut self) {
            if let Some(tx) = self.0.take() {
                let _ = tx.send(());
            }
        }
    }

    use crate::{
        config::{AuthConfig, NotifyConfig, PackageKind, UpgradeConfig},
        state::new_shared_state,
        types::{NodeInfo, NodeStatus},
    };

    fn test_state() -> SharedState {
        new_shared_state(NodeInfo {
            id: Uuid::new_v4(),
            hostname: "api-test".to_string(),
            api_addr: "127.0.0.1:7980".to_string(),
            gossip_addr: "127.0.0.1:7979".to_string(),
            status: NodeStatus::Online,
            last_seen: Utc::now(),
            version: "0.1.0".to_string(),
        })
    }

    fn test_upgrade(enabled: bool) -> UpgradeManager {
        UpgradeManager::new(
            UpgradeConfig {
                enabled,
                package: PackageKind::Node,
                ..UpgradeConfig::default()
            },
            "0.1.0",
        )
        .expect("upgrade manager should be created")
    }

    /// A router state backed by a throwaway database file and a notifier whose
    /// pushes are disabled, so tests never touch the network.
    async fn test_api_state(deploy: DeployConfig, auth: AuthConfig) -> ApiState {
        test_api_state_with_notify(deploy, auth, NotifyConfig::default()).await
    }

    async fn test_api_state_with_notify(
        deploy: DeployConfig,
        auth: AuthConfig,
        notify_config: NotifyConfig,
    ) -> ApiState {
        let db_path = std::env::temp_dir().join(format!("osw-api-test-{}.db", Uuid::new_v4()));
        let db = std::sync::Arc::new(
            Database::new(db_path.to_str().expect("temp path should be utf-8"))
                .await
                .expect("test database should initialize"),
        );
        let notify = NotificationService::new(
            std::sync::Arc::clone(&db),
            &NotifyConfig {
                enabled: false,
                ..notify_config
            },
        )
        .expect("notifier should build");

        ApiState::new(
            test_state(),
            test_upgrade(false),
            UpgradeConfig::default(),
            deploy,
            "127.0.0.1:7979".to_string(),
            db,
            notify,
            AuthManager::new(&auth),
            crate::config::default_alert_history_minutes(),
        )
    }

    /// Router with authentication turned off, matching deployments that opt out.
    async fn open_router(deploy: DeployConfig) -> axum::Router {
        let auth = AuthConfig {
            enabled: false,
            ..AuthConfig::default()
        };
        create_router(test_api_state(deploy, auth).await, None)
    }

    async fn response_json(response: axum::response::Response) -> serde_json::Value {
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should be readable");
        serde_json::from_slice(&body).expect("body should be JSON")
    }

    fn get_request(uri: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .body(Body::empty())
            .expect("request should build")
    }

    #[tokio::test]
    async fn version_endpoint_returns_current_version_platform_package_and_status() {
        let app = open_router(DeployConfig::default()).await;

        let response = app
            .oneshot(get_request("/api/v1/version"))
            .await
            .expect("router should respond");

        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response).await;

        assert_eq!(json["success"], true);
        assert_eq!(json["data"]["current"], "0.1.0");
        assert_eq!(json["data"]["package"], "node");
        assert_eq!(json["data"]["upgrade"]["phase"], "idle");
        assert!(json["data"]["platform"]
            .as_str()
            .is_some_and(|v| !v.is_empty()));
    }

    #[tokio::test]
    async fn disabled_upgrade_endpoint_rejects_without_starting_background_work() {
        let app = open_router(DeployConfig::default()).await;

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/upgrade")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"package":"node"}"#))
                    .expect("request should build"),
            )
            .await
            .expect("router should respond");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let json = response_json(response).await;

        assert_eq!(json["success"], false);
        assert!(json["error"]
            .as_str()
            .is_some_and(|message| message.contains("disabled")));
    }

    #[tokio::test]
    async fn disabled_deploy_upgrades_then_sends_error_and_closes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener should bind");
        let addr = listener.local_addr().expect("listener should have address");
        let app = open_router(DeployConfig {
            enabled: false,
            ..DeployConfig::default()
        })
        .await;
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("server should run");
        });

        let (mut socket, response) = connect_async(format!("ws://{addr}/api/v1/nodes/deploy"))
            .await
            .expect("disabled deployment must still upgrade to websocket");
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);

        let message = socket
            .next()
            .await
            .expect("server should send an error frame")
            .expect("error frame should be readable");
        let text = message.into_text().expect("error event should be text");
        let event: serde_json::Value = serde_json::from_str(&text).expect("event should be JSON");
        assert_eq!(event["type"], "error");
        assert!(event["message"]
            .as_str()
            .is_some_and(|message| message.contains("禁用")));

        let closed = socket.next().await;
        assert!(closed.is_none() || closed.is_some_and(|frame| frame.is_ok_and(|m| m.is_close())));
        server.abort();
    }

    #[tokio::test]
    async fn partial_deploy_request_is_normalized_before_validation() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener should bind");
        let addr = listener.local_addr().expect("listener should have address");
        let app = open_router(DeployConfig::default()).await;
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("server should run");
        });
        let (mut socket, _) = connect_async(format!("ws://{addr}/api/v1/nodes/deploy"))
            .await
            .expect("websocket should connect");

        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                r#"{
                    "host":"127.0.0.1",
                    "port":0,
                    "username":"root",
                    "auth":{"type":"password","password":"dummy"}
                }"#
                .to_string(),
            ))
            .await
            .expect("request should send");
        let message = socket
            .next()
            .await
            .expect("server should reply")
            .expect("reply should be readable")
            .into_text()
            .expect("event should be text");
        let event: serde_json::Value =
            serde_json::from_str(&message).expect("event should be JSON");
        assert_eq!(event["type"], "error");
        assert!(event["message"]
            .as_str()
            .is_some_and(|message| message.contains("参数校验失败")));
        assert!(!event["message"]
            .as_str()
            .is_some_and(|message| message.contains("请求解析失败")));

        server.abort();
    }

    #[tokio::test]
    async fn malformed_deploy_request_sends_error_then_close_frame() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener should bind");
        let addr = listener.local_addr().expect("listener should have address");
        let app = open_router(DeployConfig::default()).await;
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("server should run");
        });
        let (mut socket, _) = connect_async(format!("ws://{addr}/api/v1/nodes/deploy"))
            .await
            .expect("websocket should connect");

        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                "{".to_string(),
            ))
            .await
            .expect("malformed request should send");

        let message = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
            .await
            .expect("error frame should arrive promptly")
            .expect("server should send an error frame")
            .expect("error frame should be readable");
        let event: serde_json::Value =
            serde_json::from_str(&message.into_text().expect("error event should be text"))
                .expect("error event should be JSON");
        assert_eq!(event["type"], "error");
        assert!(event["message"]
            .as_str()
            .is_some_and(|message| message.contains("请求解析失败")));

        let close = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
            .await
            .expect("close frame should arrive promptly")
            .expect("server should send a close frame")
            .expect("close frame should be readable");
        assert!(close.is_close());
        server.abort();
    }

    #[tokio::test]
    async fn client_disconnect_aborts_spawned_deploy_task() {
        let (event_tx, event_rx) = mpsc::channel(1);
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let guard = NotifyOnDrop(Some(dropped_tx));
        let deploy_task = tokio::spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        });
        let client_messages =
            futures::stream::iter([Ok::<_, std::convert::Infallible>(Message::Close(None))]);
        let sink = futures::sink::drain();

        relay_deploy_events(sink, client_messages, event_rx, deploy_task).await;

        tokio::time::timeout(std::time::Duration::from_secs(1), dropped_rx)
            .await
            .expect("deploy task should be aborted immediately on disconnect")
            .expect("drop signal should arrive");
        drop(event_tx);
    }

    async fn login(app: &axum::Router, password: &str) -> (StatusCode, Option<String>) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/auth/login")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({ "password": password }).to_string(),
                    ))
                    .expect("request should build"),
            )
            .await
            .expect("router should respond");

        let status = response.status();
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::to_string);

        (status, cookie)
    }

    fn with_cookie(uri: &str, cookie: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .header("cookie", cookie)
            .body(Body::empty())
            .expect("request should build")
    }

    fn protected_auth() -> AuthConfig {
        AuthConfig {
            enabled: true,
            password: "test-pass".to_string(),
            session_ttl_hours: 1,
        }
    }

    #[tokio::test]
    async fn enabled_auth_without_a_password_refuses_management_with_a_hint() {
        let app = create_router(
            test_api_state(DeployConfig::default(), AuthConfig::default()).await,
            None,
        );

        let response = app
            .clone()
            .oneshot(get_request("/api/v1/auth/session"))
            .await
            .expect("router should respond");
        let json = response_json(response).await;
        assert_eq!(json["data"]["auth_required"], true);
        assert_eq!(json["data"]["authenticated"], false);

        let response = app
            .clone()
            .oneshot(get_request("/api/v1/alerts"))
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::OK, "guests still read alerts");

        let response = app
            .oneshot(get_request("/api/v1/notify/channels"))
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response_json(response).await["error"], AUTH_UNCONFIGURED_MESSAGE);
    }

    #[tokio::test]
    async fn management_endpoints_require_a_session() {
        let app = create_router(
            test_api_state(DeployConfig::default(), protected_auth()).await,
            None,
        );

        let response = app
            .clone()
            .oneshot(get_request("/api/v1/notify/channels"))
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response_json(response).await["error"], "需要管理员登录");

        let response = app
            .clone()
            .oneshot(get_request("/api/v1/notify/defaults"))
            .await
            .expect("router should respond");
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "the configured server address is only offered to admins"
        );

        let response = app
            .clone()
            .oneshot(get_request("/api/v1/nodes/deploy"))
            .await
            .expect("router should respond");
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "the deploy websocket needs a session before it upgrades"
        );

        let (status, cookie) = login(&app, "wrong").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(cookie.is_none(), "a rejected login must not set a cookie");

        let (status, cookie) = login(&app, "test-pass").await;
        assert_eq!(status, StatusCode::OK);
        let cookie = cookie.expect("login must set a session cookie");
        assert!(cookie.starts_with("osw_session="));

        let response = app
            .clone()
            .oneshot(with_cookie("/api/v1/notify/channels", &cookie))
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::OK);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/notify/channels")
                    .header("cookie", &cookie)
                    .header("sec-fetch-site", "cross-site")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/auth/logout")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::OK);

        let response = app
            .oneshot(with_cookie("/api/v1/notify/channels", &cookie))
            .await
            .expect("router should respond");
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "logout must invalidate the session"
        );
    }

    #[tokio::test]
    async fn notify_channel_crud_round_trip() {
        let app = create_router(
            test_api_state(DeployConfig::default(), protected_auth()).await,
            None,
        );
        let (status, cookie) = login(&app, "test-pass").await;
        assert_eq!(status, StatusCode::OK);
        let cookie = cookie.expect("session cookie");

        let payload = serde_json::json!({
            "name": "我的 iPhone",
            "enabled": true,
            "min_severity": "warning",
            "config": {
                "kind": "bark",
                "server_url": "https://api.day.app/",
                "device_key": "device-key",
                "encryption": {
                    "algorithm": "aes256",
                    "mode": "gcm",
                    "key": "0123456789abcdef0123456789abcdef",
                    "iv": "0123456789ab"
                }
            }
        });

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/notify/channels")
                    .header("cookie", &cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(payload.to_string()))
                    .expect("request should build"),
            )
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::OK);
        let created = response_json(response).await;
        assert_eq!(
            created["data"]["config"]["server_url"], "https://api.day.app",
            "the stored server url has its trailing slash trimmed"
        );
        let id = created["data"]["id"].as_str().expect("channel id").to_string();

        let response = app
            .clone()
            .oneshot(with_cookie("/api/v1/notify/channels", &cookie))
            .await
            .expect("router should respond");
        assert_eq!(
            response_json(response).await["data"]
                .as_array()
                .expect("channel list")
                .len(),
            1
        );

        let mut renamed = payload.clone();
        renamed["name"] = serde_json::json!("客厅 iPad");
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/api/v1/notify/channels/{id}"))
                    .header("cookie", &cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(renamed.to_string()))
                    .expect("request should build"),
            )
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response_json(response).await["data"]["name"], "客厅 iPad");

        let mut broken = payload.clone();
        broken["config"]["encryption"]["iv"] = serde_json::json!("short");
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/notify/channels")
                    .header("cookie", &cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(broken.to_string()))
                    .expect("request should build"),
            )
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response_json(response).await["error"],
            "GCM 模式必须提供 12 个字符的 IV"
        );

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/v1/notify/channels/{id}"))
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::OK);

        let response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/v1/notify/channels/{id}"))
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// A self-hosted Bark server is configured once in `[notify] server_url`: the
    /// panel reads it for new channels and an empty channel address falls back to
    /// it, path prefix included.
    #[tokio::test]
    async fn channels_default_to_the_configured_self_hosted_server() {
        let app = create_router(
            test_api_state_with_notify(
                DeployConfig::default(),
                protected_auth(),
                NotifyConfig {
                    server_url: "https://bark.example.com/bark/".to_string(),
                    ..NotifyConfig::default()
                },
            )
            .await,
            None,
        );

        let (_, cookie) = login(&app, "test-pass").await;
        let cookie = cookie.expect("login must set a session cookie");

        let response = app
            .clone()
            .oneshot(with_cookie("/api/v1/notify/defaults", &cookie))
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response_json(response).await["data"]["server_url"],
            "https://bark.example.com/bark",
            "the configured default is offered to the panel without a trailing slash"
        );

        let payload = serde_json::json!({
            "name": "自建服务器",
            "enabled": true,
            "min_severity": "warning",
            "config": {
                "kind": "bark",
                "server_url": "  ",
                "device_key": "device-key",
                "encryption": null
            }
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/notify/channels")
                    .header("cookie", &cookie)
                    .header("content-type", "application/json")
                    .body(Body::from(payload.to_string()))
                    .expect("request should build"),
            )
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response_json(response).await["data"]["config"]["server_url"],
            "https://bark.example.com/bark"
        );
    }
}
