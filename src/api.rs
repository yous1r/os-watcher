use std::path::Path as FsPath;

use anyhow::Result;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde::Serialize;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};
use tracing::{info, warn};

use crate::state::SharedState;
use crate::types::*;
use crate::upgrade::{UpgradeManager, UpgradeRequest};

#[derive(Clone)]
pub struct ApiState {
    nodes: SharedState,
    upgrade: UpgradeManager,
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

pub fn create_router(state: SharedState, upgrade: UpgradeManager, web_dir: Option<&str>) -> Router {
    let api_state = ApiState {
        nodes: state,
        upgrade,
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

pub async fn run_api_server(
    state: SharedState,
    upgrade: UpgradeManager,
    bind_addr: &str,
    port: u16,
    web_dir: Option<String>,
) -> Result<()> {
    let addr = format!("{}:{}", bind_addr, port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("API server listening on http://{}", addr);

    let app = create_router(state, upgrade, web_dir.as_deref());
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
    use tower::ServiceExt;
    use uuid::Uuid;

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
        let app = create_router(test_state(), test_upgrade(false), None);

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
        let app = create_router(test_state(), test_upgrade(false), None);

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
}
