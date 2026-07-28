use std::path::Path as FsPath;

use axum::{
    Router,
    extract::{State, Path},
    routing::get,
    Json,
    response::IntoResponse,
    http::StatusCode,
};
use serde::Serialize;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};
use tracing::{info, warn};
use anyhow::Result;

use crate::state::SharedState;
use crate::types::*;

#[derive(Debug, Serialize)]
struct ApiResponse<T: Serialize> {
    success: bool,
    data: T,
}

impl<T: Serialize> ApiResponse<T> {
    fn ok(data: T) -> Json<Self> {
        Json(Self { success: true, data })
    }
}

#[derive(Debug, Serialize)]
struct ApiError {
    success: bool,
    error: String,
}

impl ApiError {
    fn not_found(msg: impl Into<String>) -> (StatusCode, Json<Self>) {
        (
            StatusCode::NOT_FOUND,
            Json(Self { success: false, error: msg.into() }),
        )
    }
}

/// GET /api/v1/nodes — list all known nodes
async fn list_nodes(State(state): State<SharedState>) -> impl IntoResponse {
    let s = state.read().await;
    let nodes: Vec<NodeInfo> = s.peers.values().cloned().collect();
    ApiResponse::ok(nodes)
}

/// GET /api/v1/nodes/:node_id — get info about a specific node
async fn get_node(
    State(state): State<SharedState>,
    Path(node_id): Path<String>,
) -> impl IntoResponse {
    let id = match uuid::Uuid::parse_str(&node_id) {
        Ok(id) => id,
        Err(_) => return Err(ApiError::not_found("Invalid node ID")),
    };

    let s = state.read().await;
    match s.peers.get(&id) {
        Some(node) => Ok(ApiResponse::ok(node.clone()).into_response()),
        None => Err(ApiError::not_found(format!("Node {} not found", node_id))),
    }
}

/// GET /api/v1/nodes/:node_id/metrics — get latest metrics for a node
async fn get_node_metrics(
    State(state): State<SharedState>,
    Path(node_id): Path<String>,
) -> impl IntoResponse {
    let id = match uuid::Uuid::parse_str(&node_id) {
        Ok(id) => id,
        Err(_) => return Err(ApiError::not_found("Invalid node ID")),
    };

    let s = state.read().await;
    if !s.peers.contains_key(&id) {
        return Err(ApiError::not_found(format!("Node {} not found", node_id)));
    }

    match s.metrics.get(&id) {
        Some(metrics) => Ok(ApiResponse::ok(metrics.clone()).into_response()),
        None => Err(ApiError::not_found("No metrics available for this node")),
    }
}

/// GET /api/v1/metrics — get latest metrics for all nodes
async fn get_all_metrics(State(state): State<SharedState>) -> impl IntoResponse {
    let s = state.read().await;
    let snapshots = s.node_snapshots();
    ApiResponse::ok(snapshots)
}

/// GET /api/v1/alerts — get active alerts
async fn get_alerts(State(state): State<SharedState>) -> impl IntoResponse {
    let s = state.read().await;
    let alerts: Vec<Alert> = s.active_alerts().into_iter().cloned().collect();
    ApiResponse::ok(alerts)
}

/// GET /api/v1/local — get local node info and metrics
async fn get_local(State(state): State<SharedState>) -> impl IntoResponse {
    let s = state.read().await;
    let local_id = s.local_node.id;
    let snapshot = NodeSnapshot {
        info: s.local_node.clone(),
        metrics: s.metrics.get(&local_id).cloned(),
    };
    ApiResponse::ok(snapshot)
}

/// GET /api/v1/health — health check
async fn health_check() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "ok",
        "service": "os-watcher"
    }))
}

pub fn create_router(state: SharedState, web_dir: Option<&str>) -> Router {
    let api = Router::new()
        .route("/api/v1/health", get(health_check))
        .route("/api/v1/local", get(get_local))
        .route("/api/v1/nodes", get(list_nodes))
        .route("/api/v1/nodes/:node_id", get(get_node))
        .route("/api/v1/nodes/:node_id/metrics", get(get_node_metrics))
        .route("/api/v1/metrics", get(get_all_metrics))
        .route("/api/v1/alerts", get(get_alerts))
        .with_state(state);

    let mut app = api;

    // Optionally serve the built web dashboard (SPA). Unknown paths fall back
    // to index.html so client-side routing works.
    if let Some(dir) = web_dir {
        if FsPath::new(dir).is_dir() {
            let index = format!("{}/index.html", dir);
            let serve = ServeDir::new(dir)
                .not_found_service(ServeFile::new(index));
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
    bind_addr: &str,
    port: u16,
    web_dir: Option<String>,
) -> Result<()> {
    let addr = format!("{}:{}", bind_addr, port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("API server listening on http://{}", addr);

    let app = create_router(state, web_dir.as_deref());
    axum::serve(listener, app).await?;

    Ok(())
}
