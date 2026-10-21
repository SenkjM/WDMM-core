//! `wdmm-openapi` — toggleable axum HTTP server exposing core services as
//! OpenAPI 3.1. Bind modes: 127.0.0.1 (local only) / 0.0.0.0 (LAN) / custom.
//! Token-based auth for external clients; native FFI clients skip auth.
//!
//! ## Auth
//! Every request **except** `/health` must carry `Authorization: Bearer <token>`.
//! Tokens are generated at server creation; the native (FFI) frontend can read
//! the token from [`ServerConfig::token`] without network round-trips.

#![forbid(unsafe_code)]

use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Json, Response},
    routing::{delete, get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{oneshot, RwLock};
use wdmm_core::models::CloudAccount;
use wdmm_queue::TaskQueue;
use wdmm_storage::LibraryDatabase;

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
    /// Bearer token required by external clients.
    /// Native (FFI) frontends can read this directly; there is no separate
    /// auth flow for same-machine callers.
    pub token: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BindMode {
    Local,  // 127.0.0.1
    Global, // 0.0.0.0
    Custom, // user-supplied IP in `custom_ip`
}

impl BindMode {
    pub fn to_addr(&self, custom: Option<&str>) -> &'static str {
        match self {
            BindMode::Local => "127.0.0.1",
            BindMode::Global => "0.0.0.0",
            BindMode::Custom => {
                // custom IP is dynamic, so we return a sentinel; callers use
                // `custom_ip` directly when the mode is Custom.
                let _ = custom;
                "127.0.0.1"
            }
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_mode: BindMode::Local,
            port: 0,
            token: uuid::Uuid::new_v4().to_string(),
        }
    }
}

// ── Shared state ─────────────────────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    config: Arc<ServerConfig>,
    db: Arc<LibraryDatabase>,
    queue: Arc<TaskQueue>,
}

// ── Auth middleware ───────────────────────────────────────────────────────────

async fn require_token(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match token {
        Some(t) if t == state.config.token => Ok(next.run(req).await),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

// ── Handlers ─────────────────────────────────────────────────────────────────

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "ok",
        "service": "wdmm-openapi",
        "version": env!("CARGO_PKG_VERSION"),
        "port": state.config.port,
    }))
}

/// GET /v1/accounts — list all cloud accounts (credentials omitted).
async fn list_accounts(State(state): State<AppState>) -> impl IntoResponse {
    match state.db.list_accounts() {
        Ok(accounts) => {
            let view: Vec<_> = accounts
                .iter()
                .map(|a| {
                    serde_json::json!({
                        "id": a.id,
                        "provider_type": a.provider_type,
                        "display_name": a.display_name,
                        "base_url": a.base_url,
                    })
                })
                .collect();
            (StatusCode::OK, Json(serde_json::json!({ "accounts": view }))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// POST /v1/accounts — create a new account.
async fn create_account(
    State(state): State<AppState>,
    Json(payload): Json<CreateAccountRequest>,
) -> impl IntoResponse {
    let id = uuid::Uuid::new_v4().to_string();
    let account = CloudAccount {
        id: id.clone(),
        provider_type: payload.provider_type,
        display_name: payload.display_name,
        base_url: payload.base_url,
        credentials_encrypted: payload.credentials_encrypted.unwrap_or_default(),
    };
    match state.db.insert_account(&account) {
        Ok(()) => (
            StatusCode::CREATED,
            Json(serde_json::json!({ "id": id })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct CreateAccountRequest {
    provider_type: String,
    display_name: String,
    base_url: Option<String>,
    credentials_encrypted: Option<Vec<u8>>,
}

/// DELETE /v1/accounts/:id
async fn delete_account(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    match state.db.delete_account(&id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// GET /v1/tasks — list all queue tasks.
async fn list_tasks(State(state): State<AppState>) -> impl IntoResponse {
    let tasks = state.queue.list().await;
    let view: Vec<_> = tasks
        .iter()
        .map(|t| {
            serde_json::json!({
                "id": t.id,
                "kind": format!("{:?}", t.kind),
                "status": format!("{:?}", t.status),
                "progress": t.progress,
                "total_bytes": t.total_bytes,
                "transferred_bytes": t.transferred_bytes,
                "error": t.error,
                "created": t.created,
                "updated": t.updated,
            })
        })
        .collect();
    Json(serde_json::json!({ "tasks": view }))
}

/// POST /v1/tasks/:id/cancel
async fn cancel_task(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    match state.queue.cancel(&id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(wdmm_queue::QueueError::NotFound(_)) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// GET /v1/tasks/:id — get a single task.
async fn get_task(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    let tasks = state.queue.list().await;
    match tasks.into_iter().find(|t| t.id == id) {
        Some(t) => Json(serde_json::json!({
            "id": t.id,
            "kind": format!("{:?}", t.kind),
            "status": format!("{:?}", t.status),
            "progress": t.progress,
            "total_bytes": t.total_bytes,
            "transferred_bytes": t.transferred_bytes,
            "error": t.error,
            "created": t.created,
            "updated": t.updated,
        }))
        .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ── Router ────────────────────────────────────────────────────────────────────

fn build_router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/v1/accounts", get(list_accounts).post(create_account))
        .route("/v1/accounts/:id", delete(delete_account))
        .route("/v1/tasks", get(list_tasks))
        .route("/v1/tasks/:id", get(get_task))
        .route("/v1/tasks/:id/cancel", post(cancel_task))
        .layer(middleware::from_fn_with_state(state.clone(), require_token));

    Router::new()
        .route("/health", get(health))
        .merge(protected)
        .with_state(state)
}

// ── Server handle ─────────────────────────────────────────────────────────────

/// Running OpenAPI server. Drop (or call `.stop()`) to shut it down.
pub struct ApiServer {
    config: Arc<ServerConfig>,
    bound_port: u16,
    shutdown: Arc<RwLock<Option<oneshot::Sender<()>>>>,
}

impl ApiServer {
    /// Create and start the server. `config.port = 0` assigns an ephemeral port
    /// (read the assigned port back via [`ApiServer::port`]).
    pub async fn start(
        config: ServerConfig,
        db: Arc<LibraryDatabase>,
        queue: Arc<TaskQueue>,
    ) -> ApiResult<Self> {
        let addr = format!(
            "{}:{}",
            config.bind_mode.to_addr(None),
            config.port
        );
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| ApiError::Bind(e.to_string()))?;
        let bound_port = listener.local_addr().map_err(|e| ApiError::Bind(e.to_string()))?.port();

        let state = AppState {
            config: Arc::new(config.clone()),
            db,
            queue,
        };
        let app = build_router(state);

        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async { let _ = rx.await; })
                .await;
        });

        Ok(Self {
            config: Arc::new(config),
            bound_port,
            shutdown: Arc::new(RwLock::new(Some(tx))),
        })
    }

    pub fn port(&self) -> u16 {
        self.bound_port
    }

    pub fn token(&self) -> &str {
        &self.config.token
    }

    pub async fn stop(&self) {
        if let Some(tx) = self.shutdown.write().await.take() {
            let _ = tx.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use wdmm_core::models::TaskKind;
    use wdmm_queue::TaskQueue;

    async fn start_test_server() -> (ApiServer, reqwest::Client, String) {
        let db = Arc::new(LibraryDatabase::open(Path::new(":memory:")).unwrap());
        let queue = Arc::new(TaskQueue::new());
        let config = ServerConfig {
            bind_mode: BindMode::Local,
            port: 0,
            token: "test-token".into(),
        };
        let server = ApiServer::start(config, db, queue).await.unwrap();
        let base = format!("http://127.0.0.1:{}", server.port());
        let client = reqwest::Client::new();
        (server, client, base)
    }

    #[tokio::test]
    async fn health_no_auth() {
        let (server, client, base) = start_test_server().await;
        let resp = client.get(format!("{base}/health")).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["status"], "ok");
        server.stop().await;
    }

    #[tokio::test]
    async fn accounts_requires_token() {
        let (server, client, base) = start_test_server().await;
        // Without token → 401
        assert_eq!(
            client.get(format!("{base}/v1/accounts")).send().await.unwrap().status(),
            401
        );
        // Wrong token → 401
        assert_eq!(
            client.get(format!("{base}/v1/accounts"))
                .bearer_auth("wrong")
                .send().await.unwrap().status(),
            401
        );
        // Correct token → 200
        assert_eq!(
            client.get(format!("{base}/v1/accounts"))
                .bearer_auth("test-token")
                .send().await.unwrap().status(),
            200
        );
        server.stop().await;
    }

    #[tokio::test]
    async fn accounts_crud() {
        let (server, client, base) = start_test_server().await;
        let tok = server.token().to_string();

        // Create
        let resp = client.post(format!("{base}/v1/accounts"))
            .bearer_auth(&tok)
            .json(&serde_json::json!({
                "provider_type": "webdav",
                "display_name": "My NAS",
                "base_url": "https://nas.local/dav/",
            }))
            .send().await.unwrap();
        assert_eq!(resp.status(), 201);
        let created: serde_json::Value = resp.json().await.unwrap();
        let id = created["id"].as_str().unwrap().to_string();

        // List
        let list: serde_json::Value = client.get(format!("{base}/v1/accounts"))
            .bearer_auth(&tok)
            .send().await.unwrap()
            .json().await.unwrap();
        assert_eq!(list["accounts"].as_array().unwrap().len(), 1);
        assert_eq!(list["accounts"][0]["id"], id);

        // Delete
        assert_eq!(
            client.delete(format!("{base}/v1/accounts/{id}"))
                .bearer_auth(&tok)
                .send().await.unwrap().status(),
            204
        );

        // List again — empty
        let list2: serde_json::Value = client.get(format!("{base}/v1/accounts"))
            .bearer_auth(&tok)
            .send().await.unwrap()
            .json().await.unwrap();
        assert_eq!(list2["accounts"].as_array().unwrap().len(), 0);

        server.stop().await;
    }

    #[tokio::test]
    async fn tasks_crud() {
        use wdmm_core::models::TaskStatus;
        let db = Arc::new(LibraryDatabase::open(Path::new(":memory:")).unwrap());
        let queue = Arc::new(TaskQueue::new());

        // Pre-populate a task
        queue.enqueue(wdmm_core::models::QueueTask {
            id: "t1".into(),
            kind: TaskKind::DownloadTrack,
            status: TaskStatus::Pending,
            progress: 0.0,
            total_bytes: 1000,
            transferred_bytes: 0,
            created: chrono::Utc::now(),
            updated: chrono::Utc::now(),
            attempts: 0,
            error: None,
            remote_path: Some("/music/x.mp3".into()),
            local_path: None,
        }).await.unwrap();

        let config = ServerConfig {
            bind_mode: BindMode::Local,
            port: 0,
            token: "tok2".into(),
        };
        let server = ApiServer::start(config, db, Arc::clone(&queue)).await.unwrap();
        let base = format!("http://127.0.0.1:{}", server.port());
        let client = reqwest::Client::new();

        let tasks: serde_json::Value = client.get(format!("{base}/v1/tasks"))
            .bearer_auth("tok2")
            .send().await.unwrap()
            .json().await.unwrap();
        assert_eq!(tasks["tasks"].as_array().unwrap().len(), 1);

        // Get single task
        let task: serde_json::Value = client.get(format!("{base}/v1/tasks/t1"))
            .bearer_auth("tok2")
            .send().await.unwrap()
            .json().await.unwrap();
        assert_eq!(task["id"], "t1");

        // Cancel
        assert_eq!(
            client.post(format!("{base}/v1/tasks/t1/cancel"))
                .bearer_auth("tok2")
                .send().await.unwrap().status(),
            204
        );

        server.stop().await;
    }
}
