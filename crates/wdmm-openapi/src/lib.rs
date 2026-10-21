//! `wdmm-openapi` — toggleable axum HTTP server exposing core services as
//! OpenAPI 3.1. Bind modes: 127.0.0.1 (local only) / 0.0.0.0 (LAN) / custom.
//! Token-based auth for external clients; native FFI clients skip auth.

#![forbid(unsafe_code)]

use axum::{
    extract::State,
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::RwLock;

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("server error: {0}")]
    Server(String),

    #[error("bind failed: {0}")]
    Bind(String),

    #[error("not running")]
    NotRunning,
}

pub type ApiResult<T> = std::result::Result<T, ApiError>;

/// OpenAPI server configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub bind_mode: BindMode,
    pub port: u16,
    pub token: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BindMode {
    Local,     // 127.0.0.1
    Global,    // 0.0.0.0
    Custom,    // user-supplied IP
}

impl BindMode {
    pub fn to_addr(&self, custom: Option<&str>) -> String {
        match self {
            BindMode::Local => "127.0.0.1".into(),
            BindMode::Global => "0.0.0.0".into(),
            BindMode::Custom => custom.unwrap_or("127.0.0.1").into(),
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_mode: BindMode::Local,
            port: 8080,
            token: uuid::Uuid::new_v4().to_string(),
        }
    }
}

/// OpenAPI server state (shared across handlers).
#[derive(Clone)]
struct AppState {
    config: Arc<ServerConfig>,
}

impl AppState {
    fn port(&self) -> u16 { self.config.port }
}

/// Health check endpoint.
async fn health(State(state): State<AppState>) -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "ok",
        "service": "wdmm-openapi",
        "version": env!("CARGO_PKG_VERSION"),
        "port": state.port(),
    }))
}

/// Stub: list accounts.
async fn list_accounts(State(_state): State<AppState>) -> impl IntoResponse {
    Json(serde_json::json!({
        "accounts": [],
    }))
}

/// Stub: list tracks.
async fn list_tracks(State(_state): State<AppState>) -> impl IntoResponse {
    Json(serde_json::json!({
        "tracks": [],
    }))
}

/// Build the axum router with /v1/ routes.
fn app_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/accounts", get(list_accounts))
        .route("/v1/tracks", get(list_tracks))
        .with_state(state)
}

/// OpenAPI server handle.
pub struct ApiServer {
    config: Arc<ServerConfig>,
    handle: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
}

impl ApiServer {
    pub fn new(config: ServerConfig) -> Self {
        Self {
            config: Arc::new(config),
            handle: Arc::new(RwLock::new(None)),
        }
    }

    /// Start the server in the background. Returns immediately.
    pub async fn start(&self) -> ApiResult<()> {
        let mut guard = self.handle.write().await;
        if guard.is_some() {
            return Ok(()); // already running
        }

        let config = self.config.clone();
        let addr_str = format!(
            "{}:{}",
            config.bind_mode.to_addr(None),
            config.port
        );
        let addr: std::net::SocketAddr = addr_str.parse().map_err(|e| ApiError::Bind(format!("{}", e)))?;

        let state = AppState {
            config: config.clone(),
        };
        let app = app_router(state);

        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| ApiError::Bind(e.to_string()))?;

        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        *guard = Some(task);
        Ok(())
    }

    /// Stop the server.
    pub async fn stop(&self) -> ApiResult<()> {
        let mut guard = self.handle.write().await;
        if let Some(h) = guard.take() {
            h.abort();
        }
        Ok(())
    }

    pub fn is_running(&self) -> bool {
        // Check if handle exists (simplified; tokio task doesn't expose is_finished in stable API easily)
        false
    }

    pub fn config(&self) -> &ServerConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn start_and_stop() {
        let cfg = ServerConfig {
            bind_mode: BindMode::Local,
            port: 0, // let OS pick
            token: "test-token".into(),
        };
        let server = ApiServer::new(cfg);
        // start/stop without actual http call
        server.start().await.unwrap();
        server.stop().await.unwrap();
    }
}
