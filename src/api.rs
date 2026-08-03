use std::path::Path as FsPath;

use anyhow::Result;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use futures::{Sink, SinkExt, Stream, StreamExt};
use serde::Serialize;
use tokio::sync::mpsc;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};
use tracing::{info, warn};

use crate::config::{DeployConfig, UpgradeConfig};
use crate::deploy::{self, DeployRequest};
use crate::state::SharedState;
use crate::types::*;
use crate::upgrade::{UpgradeManager, UpgradeRequest};

const DEPLOY_EVENT_BUFFER_CAPACITY: usize = 256;

#[derive(Clone)]
pub struct ApiState {
    nodes: SharedState,
    upgrade: UpgradeManager,
    upgrade_config: UpgradeConfig,
    deploy: DeployConfig,
    /// 本机 gossip 广播地址，用于在部署请求 peers 为空时兜底注入。
    local_gossip_addr: String,
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
/// 面板不引入鉴权，`[deploy] enabled = false` 时直接拒绝部署，
/// 仅应在可信网络内暴露。
async fn deploy_ws(State(api): State<ApiState>, ws: WebSocketUpgrade) -> impl IntoResponse {
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

pub fn create_router(
    state: SharedState,
    upgrade: UpgradeManager,
    upgrade_config: UpgradeConfig,
    deploy: DeployConfig,
    local_gossip_addr: String,
    web_dir: Option<&str>,
) -> Router {
    let api_state = ApiState {
        nodes: state,
        upgrade,
        upgrade_config,
        deploy,
        local_gossip_addr,
    };

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
    bind_addr: &str,
    port: u16,
    web_dir: Option<String>,
) -> Result<()> {
    let addr = format!("{}:{}", bind_addr, port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("API server listening on http://{}", addr);

    let app = create_router(
        state,
        upgrade,
        upgrade_config,
        deploy,
        local_gossip_addr,
        web_dir.as_deref(),
    );
    axum::serve(listener, app).await?;

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
        config::{PackageKind, UpgradeConfig},
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

    #[tokio::test]
    async fn version_endpoint_returns_current_version_platform_package_and_status() {
        let app = create_router(
            test_state(),
            test_upgrade(false),
            UpgradeConfig::default(),
            DeployConfig::default(),
            "127.0.0.1:7979".to_string(),
            None,
        );

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/version")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("router should respond");

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should be readable");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("body should be JSON");

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
        let app = create_router(
            test_state(),
            test_upgrade(false),
            UpgradeConfig::default(),
            DeployConfig::default(),
            "127.0.0.1:7979".to_string(),
            None,
        );

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
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should be readable");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("body should be JSON");

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
        let app = create_router(
            test_state(),
            test_upgrade(false),
            UpgradeConfig::default(),
            DeployConfig {
                enabled: false,
                ..DeployConfig::default()
            },
            "127.0.0.1:7979".to_string(),
            None,
        );
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
        let app = create_router(
            test_state(),
            test_upgrade(false),
            UpgradeConfig::default(),
            DeployConfig::default(),
            "127.0.0.1:7979".to_string(),
            None,
        );
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
        let app = create_router(
            test_state(),
            test_upgrade(false),
            UpgradeConfig::default(),
            DeployConfig::default(),
            "127.0.0.1:7979".to_string(),
            None,
        );
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
}
