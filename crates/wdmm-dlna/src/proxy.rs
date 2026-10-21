//! Unified local HTTP range proxy.
//!
//! Ownership: the Rust core owns this proxy (constraints §3). Platform
//! frontends never serve bytes themselves; they ask the core to `register` a
//! file and hand the returned URL to the player / DLNA renderer.
//!
//! Security model:
//! - Only files explicitly registered via [`ProxyHandle::register`] are served,
//!   addressed by an unguessable random token — never by raw filesystem path.
//! - Default bind is `127.0.0.1`. DLNA renderers live on the LAN, so casting
//!   requires the caller to bind a LAN address explicitly; tokens are then the
//!   only access control, matching the OpenAPI token model.

use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
    routing::get,
    Router,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_util::io::ReaderStream;
use tracing::{error, info};

use crate::DlnaResult;

type Registry = Arc<RwLock<HashMap<String, PathBuf>>>;

/// Running proxy. Dropping it (or calling [`ProxyHandle::stop`]) shuts down.
pub struct ProxyHandle {
    addr: SocketAddr,
    registry: Registry,
    shutdown: Option<oneshot::Sender<()>>,
}

impl ProxyHandle {
    /// Start the proxy on `bind` (use port 0 for an ephemeral port).
    pub async fn start(bind: SocketAddr) -> DlnaResult<Self> {
        let registry: Registry = Arc::default();
        let app = Router::new()
            .route("/stream/:token", get(serve))
            .with_state(Arc::clone(&registry));

        let listener = TcpListener::bind(bind).await?;
        let addr = listener.local_addr()?;
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let server = axum::serve(listener, app).with_graceful_shutdown(async {
                let _ = rx.await;
            });
            if let Err(e) = server.await {
                error!("local proxy error: {e}");
            }
        });
        info!("local proxy listening on {addr}");

        Ok(Self {
            addr,
            registry,
            shutdown: Some(tx),
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Register a local file; returns the URL that serves it.
    pub fn register(&self, path: impl Into<PathBuf>) -> String {
        let token = format!("{:032x}", rand::random::<u128>());
        self.registry
            .write()
            .unwrap()
            .insert(token.clone(), path.into());
        format!("http://{}/stream/{}", self.addr, token)
    }

    /// Revoke a URL previously returned by [`register`](Self::register).
    pub fn unregister(&self, url: &str) {
        if let Some(token) = url.rsplit('/').next() {
            self.registry.write().unwrap().remove(token);
        }
    }

    pub fn stop(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

async fn serve(
    State(registry): State<Registry>,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    let path = registry
        .read()
        .unwrap()
        .get(&token)
        .cloned()
        .ok_or(StatusCode::NOT_FOUND)?;

    let mut file = tokio::fs::File::open(&path)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let size = file
        .metadata()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .len();

    let range = headers
        .get(header::RANGE)
        .and_then(|h| h.to_str().ok())
        .map(|h| resolve_range(h, size));

    let builder = Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::ACCEPT_RANGES, "bytes");

    let response = match range {
        None => builder
            .status(StatusCode::OK)
            .header(header::CONTENT_LENGTH, size)
            .body(Body::from_stream(ReaderStream::new(file))),
        Some(None) => builder
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(header::CONTENT_RANGE, format!("bytes */{size}"))
            .body(Body::empty()),
        Some(Some((start, end))) => {
            file.seek(std::io::SeekFrom::Start(start))
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            let len = end - start + 1;
            builder
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_LENGTH, len)
                .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"))
                .body(Body::from_stream(ReaderStream::new(file.take(len))))
        }
    };
    response.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// Resolve a single-range `Range` header against a file of `size` bytes.
///
/// Returns `None` when unsatisfiable (→ 416). Supports `a-b`, `a-` and the
/// suffix form `-n`. Multi-range requests are not supported (→ 416).
pub(crate) fn resolve_range(header: &str, size: u64) -> Option<(u64, u64)> {
    let spec = header.trim().strip_prefix("bytes=")?;
    if spec.contains(',') || size == 0 {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (start, end) = if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        if n == 0 {
            return None;
        }
        (size.saturating_sub(n), size - 1)
    } else {
        let start: u64 = a.parse().ok()?;
        let end = if b.is_empty() {
            size - 1
        } else {
            b.parse::<u64>().ok()?.min(size - 1)
        };
        (start, end)
    };
    (start <= end && start < size).then_some((start, end))
}
