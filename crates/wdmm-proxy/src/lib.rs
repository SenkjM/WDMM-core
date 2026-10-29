//! `wdmm-proxy` — Unified local HTTP range proxy.
//!
//! Serves registered streams over HTTP with `Range` support. Two back-source
//! modes (aligned with Dart `LocalStreamBridge`):
//!
//! 1. **Local file** — Rust reads the file directly from disk.
//! 2. **Driver callback** — caller supplies a `ranges` fn that fetches bytes
//!    (e.g. a cloud driver with Range support or a decrypting bridge).
//!
//! ## Token-only register API
//! `register_*` returns `Registration { token, path_template }`. Callers compose
//! `http://{bind_host}:{port}{path_template}` themselves. The proxy never returns
//! a full URL; HTTPS and custom certs are out of scope for this crate.
//!
//! ## Idle TTL
//! Server shuts down after the configured idle timeout (default 10 min). Every
//! incoming request resets the timer. The server auto-restarts on the next
//! `register_*` call.
//!
//! ## Security model
//! - Only explicitly registered tokens are served. Raw filesystem paths are
//!   never exposed in URLs.
//! - Default bind is `127.0.0.1`. DLNA renderers need a LAN bind address; tokens
//!   are the only access control in that configuration, matching the constraint
//!   that OpenAPI auth is separate from the proxy.

#![forbid(unsafe_code)]

use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
    routing::get,
    Router,
};
use bytes::Bytes;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_util::io::ReaderStream;
use tracing::{error, info};

// ── Error ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("ffmpeg error: {0}")]
    Ffmpeg(String),
}

pub type ProxyResult<T> = std::result::Result<T, ProxyError>;

// ── BindMode ──────────────────────────────────────────────────────────────────

/// Proxy server bind mode — controls which network interfaces the server listens on.
///
/// - `Loopback` — bind to `127.0.0.1` (localhost only; DLNA devices cannot reach)
/// - `LanAny` — bind to `0.0.0.0` (all interfaces; OS routes to the appropriate one)
/// - `LanIf(ip)` — bind to specific interface IP (e.g., `"192.168.1.100"`)
/// - `Explicit(addr)` — bind to exact `SocketAddr` (for advanced control)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindMode {
    Loopback,
    LanAny,
    LanIf(String),
    Explicit(SocketAddr),
}

impl BindMode {
    /// Resolve to a bind address string for use with `TcpListener::bind`.
    pub fn to_bind_addr(&self, port: u16) -> String {
        match self {
            BindMode::Loopback => format!("127.0.0.1:{}", port),
            BindMode::LanAny => format!("0.0.0.0:{}", port),
            BindMode::LanIf(ip) => format!("{}:{}", ip, port),
            BindMode::Explicit(addr) => addr.to_string(),
        }
    }
}

// ── BoxStream ─────────────────────────────────────────────────────────────────

/// Pinned, boxed byte stream — same item type as `tokio_util::io::ReaderStream`.
pub type BoxStream =
    std::pin::Pin<Box<dyn futures_core::Stream<Item = std::io::Result<Bytes>> + Send>>;

// ── Registry entry ────────────────────────────────────────────────────────────

enum BackSource {
    LocalFile(PathBuf),
    DriverCallback(Arc<dyn Fn(i64, i64) -> BoxStream + Send + Sync>),
}

struct Entry {
    name: Option<String>,
    /// Known file size in bytes. `0` means unknown (streams to EOF).
    size: u64,
    source: BackSource,
}

// ── HLS session ───────────────────────────────────────────────────────────────

/// An active HLS transcode session keyed by a proxy-assigned token.
///
/// Dropped (killing the underlying process and removing the segment directory)
/// when `unregister_hls` is called or when replaced by a seek restart.
struct HlsSession {
    /// Directory where the transcode job writes `index.m3u8` + numbered segments.
    segment_dir: std::path::PathBuf,
    /// Active transcode job. Replaced atomically on seek.
    job: wdmm_ffmpeg::JobHandle,
    /// Original input path — preserved so seek can restart the job.
    input: std::path::PathBuf,
    /// Transcode mode — preserved for seek restarts.
    mode: wdmm_ffmpeg::HlsMode,
}

/// Result of a successful `register_hls` call.
///
/// Callers compose the playlist URL as:
/// ```text
/// http://{bind_host}:{port}{playlist_template}
/// ```
/// The proxy never returns a full URL.
#[derive(Debug, Clone)]
pub struct HlsRegistration {
    /// 32-hex, 128-bit random token.
    pub token: String,
    /// URL path to the master playlist, e.g. `"/h/3f7a…/index.m3u8"`.
    pub playlist_template: String,
}

// ── Shared server state ───────────────────────────────────────────────────────

struct Shared {
    entries: HashMap<String, Entry>,
    /// Active HLS transcode sessions keyed by their proxy token.
    hls_sessions: HashMap<String, HlsSession>,
    /// Track active device+media pairs to enforce one-transcode-per-combo.
    /// Maps (device_id, media_path) -> token. When a new request arrives for the
    /// same combo, the old session is killed and replaced.
    device_media_map: HashMap<(String, PathBuf), String>,
    addr: Option<SocketAddr>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    /// Drop the old sender to cancel the previous idle task; assign new one.
    idle_cancel_tx: Option<oneshot::Sender<()>>,
    idle_timeout: Duration,
}

// ── Registration ─────────────────────────────────────────────────────────────

/// Result of a successful `register_*` call.
///
/// Callers compose the playback URL as:
/// ```text
/// http://{bind_host}:{port}{path_template}
/// ```
/// The proxy never constructs a full URL (no scheme/host/port embedded).
#[derive(Debug, Clone)]
pub struct Registration {
    /// 32-hex, 128-bit random token.
    pub token: String,
    /// URL path component, e.g. `"/s/3f7a…"`.
    pub path_template: String,
}

// ── ProxyHandle ───────────────────────────────────────────────────────────────

/// Unified local HTTP range proxy handle.
///
/// The server starts lazily on the first `register_*` and shuts down after
/// `idle_timeout` of inactivity. Dropping the handle stops the server.
pub struct ProxyHandle {
    bind_mode: BindMode,
    port: u16,
    /// Stored so `ensure_server` can pass it to `arm_idle_timer` on first start.
    #[allow(dead_code)]
    idle_timeout: Duration,
    shared: Arc<Mutex<Shared>>,
}

impl ProxyHandle {
    /// Create a handle with the given bind mode and port, using the default 10-minute idle timeout.
    ///
    /// Use `port = 0` to let the OS assign a free port.
    pub fn new(bind_mode: BindMode, port: u16) -> Self {
        Self::with_idle_timeout(bind_mode, port, Duration::from_secs(600))
    }

    /// Create a handle with a custom idle timeout.
    pub fn with_idle_timeout(bind_mode: BindMode, port: u16, idle_timeout: Duration) -> Self {
        let shared = Arc::new(Mutex::new(Shared {
            entries: HashMap::new(),
            hls_sessions: HashMap::new(),
            device_media_map: HashMap::new(),
            addr: None,
            shutdown_tx: None,
            idle_cancel_tx: None,
            idle_timeout,
        }));
        Self { bind_mode, port, idle_timeout, shared }
    }

    /// Return the bound `SocketAddr`, or `None` when the server is not running.
    pub fn addr(&self) -> Option<SocketAddr> {
        self.shared.lock().unwrap().addr
    }

    // ── Register ──────────────────────────────────────────────────────────────

    /// Register a local file. Returns a `Registration`.
    ///
    /// `name` is used only for Content-Type guessing (e.g. `"song.mp3"`).
    pub async fn register_file(
        &self,
        path: impl Into<PathBuf>,
        name: Option<String>,
    ) -> ProxyResult<Registration> {
        let path = path.into();
        let size = tokio::fs::metadata(&path).await.map(|m| m.len()).unwrap_or(0);
        self.insert_entry(Entry {
            name,
            size,
            source: BackSource::LocalFile(path),
        })
        .await
    }

    /// Register a driver Range callback back-source.
    ///
    /// `size` — known file size in bytes; pass `0` if unknown.
    /// `ranges_fn(start, end)` — `start` and `end` are both inclusive byte
    /// offsets. `end = -1` means read to EOF (size unknown).
    pub async fn register_driver<F>(
        &self,
        size: u64,
        name: Option<String>,
        ranges_fn: F,
    ) -> ProxyResult<Registration>
    where
        F: Fn(i64, i64) -> BoxStream + Send + Sync + 'static,
    {
        self.insert_entry(Entry {
            name,
            size,
            source: BackSource::DriverCallback(Arc::new(ranges_fn)),
        })
        .await
    }

    /// Revoke a token. The entry is removed immediately; in-flight requests
    /// will receive a 404 on the next sub-request after removal.
    pub fn unregister(&self, token: &str) {
        self.shared.lock().unwrap().entries.remove(token);
    }

    /// Renew the driver callback for an existing token (e.g. after re-auth).
    /// No-op if the token is unknown or its source is a local file.
    pub fn renew_callback<F>(&self, token: &str, ranges_fn: F)
    where
        F: Fn(i64, i64) -> BoxStream + Send + Sync + 'static,
    {
        let mut g = self.shared.lock().unwrap();
        if let Some(entry) = g.entries.get_mut(token) {
            entry.source = BackSource::DriverCallback(Arc::new(ranges_fn));
        }
    }

    // ── HLS register / seek / unregister ─────────────────────────────────────

    /// Register an HLS session for `input`.
    ///
    /// Calls [`wdmm_ffmpeg::open_job`] which creates the segment directory and
    /// (once vendored binaries are wired) spawns the transcode process. Use
    /// [`HlsMode::DirectCopy`] (the default) for codec-compatible media and
    /// [`HlsMode::Compat`] for transcoding. For the 「单文件直通」 compat option
    /// use [`register_file`](Self::register_file) with byte Range instead.
    ///
    /// Returns [`HlsRegistration`]; callers compose the playlist URL as
    /// `http://{bind_host}:{port}{playlist_template}`.
    /// Register an HLS transcode session.
    ///
    /// **Enforcement:** One transcode per `(device_id, input_path)` pair. If a
    /// session already exists for this combo, the old session is killed and
    /// replaced by the new one. Pass a unique `device_id` per player to avoid
    /// killing other devices' transcodes.
    ///
    /// Returns an [`HlsRegistration`] with a `playlist_template` ready to serve.
    pub async fn register_hls(
        &self,
        device_id: impl Into<String>,
        input: impl Into<std::path::PathBuf>,
        mode: wdmm_ffmpeg::HlsMode,
    ) -> ProxyResult<HlsRegistration> {
        self.ensure_server().await?;
        let device_id = device_id.into();
        let input = input.into();

        // Check if there's already a transcode for this device+media pair
        let old_token = {
            let g = self.shared.lock().unwrap();
            g.device_media_map.get(&(device_id.clone(), input.clone())).cloned()
        };

        // Kill the old session if it exists
        if let Some(token) = old_token {
            info!("replacing existing HLS session for device={device_id}, input={input:?}");
            self.unregister_hls(&token);
        }

        let job = wdmm_ffmpeg::open_job(wdmm_ffmpeg::JobOpts {
            input: input.clone(),
            mode: mode.clone(),
            subtitles: wdmm_ffmpeg::SubtitleMode::Off,
            start_offset_secs: 0.0,
            segment_duration_secs: 6,
        })
        .map_err(|e| ProxyError::Ffmpeg(e.to_string()))?;
        let segment_dir = job.segment_dir.clone();
        let token = new_token();
        let playlist_template = format!("/h/{token}/index.m3u8");
        {
            let mut g = self.shared.lock().unwrap();
            g.hls_sessions.insert(
                token.clone(),
                HlsSession { segment_dir, job, input: input.clone(), mode },
            );
            g.device_media_map.insert((device_id, input), token.clone());
            arm_idle_timer(&mut g, Arc::clone(&self.shared));
        }
        Ok(HlsRegistration { token, playlist_template })
    }

    /// Seek an active HLS session to `offset_secs`.
    ///
    /// Kills the current transcode job and restarts it with `-ss offset_secs`.
    /// The old segment directory is removed when the old [`JobHandle`] drops.
    /// No-op if the token is unknown.
    pub async fn seek_hls(&self, token: &str, offset_secs: f64) -> ProxyResult<()> {
        // Extract input + mode without holding the lock across an await point.
        let (input, mode) = {
            let g = self.shared.lock().unwrap();
            match g.hls_sessions.get(token) {
                None => return Ok(()),
                Some(s) => (s.input.clone(), s.mode.clone()),
            }
        };
        let new_job = wdmm_ffmpeg::open_job(wdmm_ffmpeg::JobOpts {
            input: input.clone(),
            mode: mode.clone(),
            subtitles: wdmm_ffmpeg::SubtitleMode::Off,
            start_offset_secs: offset_secs,
            segment_duration_secs: 6,
        })
        .map_err(|e| ProxyError::Ffmpeg(e.to_string()))?;
        let new_segment_dir = new_job.segment_dir.clone();
        // Atomically replace the session — old job drops here, killing the old process.
        let mut g = self.shared.lock().unwrap();
        if let Some(session) = g.hls_sessions.get_mut(token) {
            session.segment_dir = new_segment_dir;
            session.job = new_job;
        }
        Ok(())
    }

    /// Revoke an HLS session token. The transcode job is killed and the
    /// segment directory is removed (on [`JobHandle`] drop).
    pub fn unregister_hls(&self, token: &str) {
        let mut g = self.shared.lock().unwrap();
        // Remove from device_media_map by scanning for the token
        g.device_media_map.retain(|_, t| t != token);
        g.hls_sessions.remove(token);
    }

    /// Gracefully stop the proxy. The handle is still usable; the server will
    /// restart automatically on the next `register_*`.
    pub fn stop(&self) {
        let mut g = self.shared.lock().unwrap();
        // Cancel idle timer.
        g.idle_cancel_tx = None;
        // Signal axum shutdown.
        if let Some(tx) = g.shutdown_tx.take() {
            let _ = tx.send(());
        }
        g.addr = None;
    }

    // ── Internal ──────────────────────────────────────────────────────────────

    async fn insert_entry(&self, entry: Entry) -> ProxyResult<Registration> {
        self.ensure_server().await?;
        let token = new_token();
        let path_template = format!("/s/{token}");
        {
            let mut g = self.shared.lock().unwrap();
            g.entries.insert(token.clone(), entry);
            arm_idle_timer(&mut g, Arc::clone(&self.shared));
        }
        Ok(Registration { token, path_template })
    }

    async fn ensure_server(&self) -> ProxyResult<()> {
        if self.shared.lock().unwrap().addr.is_some() {
            return Ok(());
        }

        let bind_addr = self.bind_mode.to_bind_addr(self.port);
        let listener = TcpListener::bind(&bind_addr).await?;
        let addr = listener.local_addr()?;
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let shared_for_router = Arc::clone(&self.shared);
        let app = Router::new()
            .route("/s/:token", get(serve))
            .route("/h/:token/*seg", get(serve_hls))
            .with_state(shared_for_router);

        let shared_for_cleanup = Arc::clone(&self.shared);
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app)
                .with_graceful_shutdown(async { let _ = shutdown_rx.await; })
                .await
            {
                error!("wdmm-proxy: server error: {e}");
            }
            let mut g = shared_for_cleanup.lock().unwrap();
            g.addr = None;
            g.shutdown_tx = None;
        });

        info!("wdmm-proxy: listening on {addr}");
        let mut g = self.shared.lock().unwrap();
        g.addr = Some(addr);
        g.shutdown_tx = Some(shutdown_tx);
        // Start idle timer now so a server that never gets a request still stops.
        arm_idle_timer(&mut g, Arc::clone(&self.shared));
        Ok(())
    }
}

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

// ── Idle timer ────────────────────────────────────────────────────────────────

fn arm_idle_timer(g: &mut Shared, shared: Arc<Mutex<Shared>>) {
    // Drop the old cancel sender — previous timer task will receive Err and exit.
    let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
    g.idle_cancel_tx = Some(cancel_tx);
    let timeout = g.idle_timeout;

    tokio::spawn(async move {
        tokio::select! {
            _ = tokio::time::sleep(timeout) => {
                let mut g = shared.lock().unwrap();
                g.idle_cancel_tx = None;
                if let Some(tx) = g.shutdown_tx.take() {
                    let _ = tx.send(());
                }
                g.addr = None;
                info!("wdmm-proxy: idle timeout, server stopped");
            }
            _ = cancel_rx => {
                // Cancelled by a new touch or explicit stop().
            }
        }
    });
}

// ── Token ─────────────────────────────────────────────────────────────────────

fn new_token() -> String {
    format!("{:032x}", rand::random::<u128>())
}

// ── Content-Type ─────────────────────────────────────────────────────────────

/// Guess MIME type from filename extension.
/// Table matches Dart `LocalStreamBridge._contentTypeFor`.
pub fn content_type_for(name: Option<&str>) -> &'static str {
    let ext = name
        .and_then(|n| n.rfind('.').map(|i| &n[i + 1..]))
        .unwrap_or("");
    match ext.to_ascii_lowercase().as_str() {
        "mp4" | "m4v"  => "video/mp4",
        "mkv"          => "video/x-matroska",
        "webm"         => "video/webm",
        "avi"          => "video/x-msvideo",
        "mov"          => "video/quicktime",
        "ts"           => "video/mp2t",
        "mp3"          => "audio/mpeg",
        "flac"         => "audio/flac",
        "m4a"          => "audio/mp4",
        "aac"          => "audio/aac",
        "wav"          => "audio/wav",
        "ogg" | "opus" => "audio/ogg",
        _              => "application/octet-stream",
    }
}

// ── Range parsing ─────────────────────────────────────────────────────────────

/// Parse a single-range `Range` header against `size` bytes.
///
/// Returns `None` when the range is unsatisfiable (→ 416). Supports
/// `a-b`, `a-` (open-ended), and `-n` (suffix). Multi-range → unsatisfiable.
pub fn resolve_range(header: &str, size: u64) -> Option<(u64, u64)> {
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

// ── axum handler ─────────────────────────────────────────────────────────────

async fn serve(
    State(shared): State<Arc<Mutex<Shared>>>,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    // Extract what we need and reset the idle timer — all under one lock.
    enum Src {
        File(PathBuf),
        Cb(Arc<dyn Fn(i64, i64) -> BoxStream + Send + Sync>),
    }
    let (src, size, content_type) = {
        let mut g = shared.lock().unwrap();
        let entry = g.entries.get(&token).ok_or(StatusCode::NOT_FOUND)?;
        let src = match &entry.source {
            BackSource::LocalFile(p)      => Src::File(p.clone()),
            BackSource::DriverCallback(f) => Src::Cb(Arc::clone(f)),
        };
        let ct = content_type_for(entry.name.as_deref());
        let sz = entry.size;
        arm_idle_timer(&mut g, Arc::clone(&shared));
        (src, sz, ct)
    };

    let range_hdr = headers
        .get(header::RANGE)
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_owned());

    let builder = Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ACCEPT_RANGES, "bytes");

    match src {
        Src::File(path) => serve_file(builder, path, size, range_hdr.as_deref()).await,
        Src::Cb(cb)     => serve_callback(builder, cb, size, range_hdr.as_deref()),
    }
}

async fn serve_file(
    builder: axum::http::response::Builder,
    path: PathBuf,
    registered_size: u64,
    range_hdr: Option<&str>,
) -> Result<Response, StatusCode> {
    let mut file = tokio::fs::File::open(&path)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let file_size = tokio::fs::metadata(&path)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .len();
    let size = if registered_size > 0 { registered_size } else { file_size };

    match range_hdr.map(|h| resolve_range(h, size)) {
        None => builder
            .status(StatusCode::OK)
            .header(header::CONTENT_LENGTH, size)
            .body(Body::from_stream(ReaderStream::new(file)))
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR),
        Some(None) => builder
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(header::CONTENT_RANGE, format!("bytes */{size}"))
            .body(Body::empty())
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR),
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
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

fn serve_callback(
    builder: axum::http::response::Builder,
    cb: Arc<dyn Fn(i64, i64) -> BoxStream + Send + Sync>,
    size: u64,
    range_hdr: Option<&str>,
) -> Result<Response, StatusCode> {
    let known = size > 0;

    if let Some(range_str) = range_hdr {
        if known {
            return match resolve_range(range_str, size) {
                None => builder
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header(header::CONTENT_RANGE, format!("bytes */{size}"))
                    .body(Body::empty())
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR),
                Some((start, end)) => {
                    let len = end - start + 1;
                    builder
                        .status(StatusCode::PARTIAL_CONTENT)
                        .header(header::CONTENT_LENGTH, len)
                        .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"))
                        .body(Body::from_stream(cb(start as i64, end as i64)))
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
                }
            };
        }
    }

    // No range (or unknown size) — stream everything from 0.
    let end_arg: i64 = if known { size as i64 - 1 } else { -1 };
    let mut resp = builder.status(StatusCode::OK);
    if known {
        resp = resp.header(header::CONTENT_LENGTH, size);
    }
    resp.body(Body::from_stream(cb(0, end_arg)))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

// ── HLS handler ───────────────────────────────────────────────────────────────

async fn serve_hls(
    State(shared): State<Arc<Mutex<Shared>>>,
    Path((token, seg)): Path<(String, String)>,
) -> Result<Response, StatusCode> {
    // Reject path traversal attempts — HLS segment names are always flat.
    if seg.contains('/') || seg.contains("..") {
        return Err(StatusCode::BAD_REQUEST);
    }

    let (segment_dir, content_type) = {
        let mut g = shared.lock().unwrap();
        let session = g.hls_sessions.get(&token).ok_or(StatusCode::NOT_FOUND)?;
        let segment_dir = session.segment_dir.clone();
        let ct: &'static str = if seg.ends_with(".m3u8") {
            "application/vnd.apple.mpegurl"
        } else {
            "video/mp2t"
        };
        arm_idle_timer(&mut g, Arc::clone(&shared));
        (segment_dir, ct)
    };

    let file_path = segment_dir.join(&seg);
    let file = tokio::fs::File::open(&file_path)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let file_len = tokio::fs::metadata(&file_path)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .len();

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, file_len)
        .body(Body::from_stream(ReaderStream::new(file)))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

// ── Tests ─────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    // ── resolve_range ─────────────────────────────────────────────────────────

    #[test]
    fn range_exact() {
        assert_eq!(resolve_range("bytes=0-9", 100), Some((0, 9)));
    }
    #[test]
    fn range_open_end() {
        assert_eq!(resolve_range("bytes=90-", 100), Some((90, 99)));
    }
    #[test]
    fn range_suffix() {
        assert_eq!(resolve_range("bytes=-10", 100), Some((90, 99)));
    }
    #[test]
    fn range_clamped() {
        assert_eq!(resolve_range("bytes=50-500", 100), Some((50, 99)));
    }
    #[test]
    fn range_out_of_bounds() {
        assert_eq!(resolve_range("bytes=100-", 100), None);
    }
    #[test]
    fn range_inverted() {
        assert_eq!(resolve_range("bytes=9-0", 100), None);
    }
    #[test]
    fn range_multirange_rejected() {
        assert_eq!(resolve_range("bytes=0-1,5-6", 100), None);
    }
    #[test]
    fn range_junk() {
        assert_eq!(resolve_range("bytes=abc", 100), None);
    }
    #[test]
    fn range_zero_size() {
        assert_eq!(resolve_range("bytes=0-0", 0), None);
    }

    // ── content_type_for ──────────────────────────────────────────────────────

    #[test]
    fn content_type_video() {
        assert_eq!(content_type_for(Some("movie.mp4")), "video/mp4");
        assert_eq!(content_type_for(Some("clip.MKV")), "video/x-matroska");
    }
    #[test]
    fn content_type_audio() {
        assert_eq!(content_type_for(Some("track.mp3")), "audio/mpeg");
        assert_eq!(content_type_for(Some("lossless.flac")), "audio/flac");
    }
    #[test]
    fn content_type_fallback() {
        assert_eq!(content_type_for(Some("data.bin")), "application/octet-stream");
        assert_eq!(content_type_for(None), "application/octet-stream");
    }

    // ── full integration: local file ──────────────────────────────────────────

    #[tokio::test]
    async fn proxy_file_full_and_range() {
        let path =
            std::env::temp_dir().join(format!("wdmm-proxy-test-{}.bin", std::process::id()));
        let data: Vec<u8> = (0..=255u8).collect();
        std::fs::write(&path, &data).unwrap();

        let proxy = ProxyHandle::new(([127, 0, 0, 1], 0).into());
        let reg = proxy.register_file(&path, Some("test.bin".into())).await.unwrap();
        let addr = proxy.addr().unwrap();
        let url = format!("http://{addr}{}", reg.path_template);
        let client = reqwest::Client::new();

        // Full GET
        let r = client.get(&url).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.bytes().await.unwrap().as_ref(), data.as_slice());

        // Range GET
        let r = client.get(&url).header("Range", "bytes=10-19").send().await.unwrap();
        assert_eq!(r.status(), 206);
        assert_eq!(r.headers()["content-range"], "bytes 10-19/256");
        assert_eq!(r.bytes().await.unwrap().as_ref(), &data[10..20]);

        // Unsatisfiable range → 416
        let r = client.get(&url).header("Range", "bytes=300-").send().await.unwrap();
        assert_eq!(r.status(), 416);

        // Unknown token → 404
        let r = client
            .get(format!("http://{addr}/s/deadbeef00deadbeef00deadbeef00aa"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404);

        // Unregister → 404
        proxy.unregister(&reg.token);
        assert_eq!(client.get(&url).send().await.unwrap().status(), 404);

        proxy.stop();
        let _ = std::fs::remove_file(&path);
    }

    // ── integration: driver callback ──────────────────────────────────────────

    #[tokio::test]
    async fn proxy_driver_callback_range() {
        use bytes::Bytes;
        use std::io;

        let data: Arc<Vec<u8>> = Arc::new((0u8..=255).collect());

        let proxy = ProxyHandle::new(([127, 0, 0, 1], 0).into());
        let d = Arc::clone(&data);
        let reg = proxy
            .register_driver(256, Some("audio.mp3".into()), move |start, end| {
                let chunk = if end < 0 {
                    d[start as usize..].to_vec()
                } else {
                    d[start as usize..=(end as usize).min(d.len() - 1)].to_vec()
                };
                // Build a single-item stream: emit the chunk then end.
                let stream = futures::stream::once(async move {
                    Ok::<Bytes, io::Error>(Bytes::from(chunk))
                });
                Box::pin(stream) as BoxStream
            })
            .await
            .unwrap();

        let addr = proxy.addr().unwrap();
        let url = format!("http://{addr}{}", reg.path_template);
        let client = reqwest::Client::new();

        // Full
        let r = client.get(&url).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.bytes().await.unwrap().as_ref(), data.as_slice());

        // Range
        let r = client.get(&url).header("Range", "bytes=0-3").send().await.unwrap();
        assert_eq!(r.status(), 206);
        assert_eq!(r.bytes().await.unwrap().as_ref(), &[0u8, 1, 2, 3]);

        proxy.stop();
    }

    // ── Registration token is not a URL ───────────────────────────────────────

    #[tokio::test]
    async fn registration_path_only_no_url() {
        let proxy = ProxyHandle::new(([127, 0, 0, 1], 0).into());
        let path =
            std::env::temp_dir().join(format!("wdmm-path-test-{}.bin", std::process::id()));
        std::fs::write(&path, b"hello").unwrap();
        let reg = proxy.register_file(&path, None).await.unwrap();
        // path_template must start with "/" and not contain "http"
        assert!(reg.path_template.starts_with('/'), "path_template must start with /");
        assert!(!reg.path_template.contains("http"), "path_template must not embed scheme");
        proxy.stop();
        let _ = std::fs::remove_file(&path);
    }

    // ── HLS skeleton ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn proxy_hls_register_serve_unregister() {
        let path = std::env::temp_dir().join(format!(
            "wdmm-hls-reg-{}.mp4",
            std::process::id()
        ));
        std::fs::write(&path, b"stub").unwrap();

        let proxy = ProxyHandle::new(([127, 0, 0, 1], 0).into());
        let reg = proxy
            .register_hls("device-1", &path, wdmm_ffmpeg::HlsMode::DirectCopy)
            .await
            .unwrap();

        assert!(!reg.token.is_empty());
        assert!(reg.playlist_template.starts_with("/h/"));
        assert!(reg.playlist_template.ends_with("/index.m3u8"));

        // Stub: segment dir is created but index.m3u8 is never written → 404.
        let addr = proxy.addr().unwrap();
        let url = format!("http://{addr}{}", reg.playlist_template);
        let r = reqwest::Client::new().get(&url).send().await.unwrap();
        assert_eq!(r.status(), 404, "stub m3u8 must not exist yet");

        proxy.unregister_hls(&reg.token);
        proxy.stop();
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn proxy_hls_seek_ok() {
        let path = std::env::temp_dir().join(format!(
            "wdmm-hls-seek-{}.mp4",
            std::process::id()
        ));
        std::fs::write(&path, b"stub").unwrap();

        let proxy = ProxyHandle::new(([127, 0, 0, 1], 0).into());
        let reg = proxy
            .register_hls("device-1", &path, wdmm_ffmpeg::HlsMode::DirectCopy)
            .await
            .unwrap();

        // Seek to 30 s — creates a new job+segment_dir; must not error in stub mode.
        proxy.seek_hls(&reg.token, 30.0).await.unwrap();

        proxy.unregister_hls(&reg.token);
        proxy.stop();
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn proxy_hls_one_transcode_per_device_media() {
        let path = std::env::temp_dir().join(format!(
            "wdmm-hls-collision-{}.mp4",
            std::process::id()
        ));
        std::fs::write(&path, b"stub").unwrap();

        let proxy = ProxyHandle::new(([127, 0, 0, 1], 0).into());

        // First registration
        let reg1 = proxy
            .register_hls("device-1", &path, wdmm_ffmpeg::HlsMode::DirectCopy)
            .await
            .unwrap();
        assert_eq!(proxy.shared.lock().unwrap().hls_sessions.len(), 1);

        // Second registration for same device+media kills the first
        let reg2 = proxy
            .register_hls("device-1", &path, wdmm_ffmpeg::HlsMode::DirectCopy)
            .await
            .unwrap();
        assert_eq!(proxy.shared.lock().unwrap().hls_sessions.len(), 1);
        assert_ne!(reg1.token, reg2.token, "new token must replace old");

        // Different device can transcode the same file
        let _reg3 = proxy
            .register_hls("device-2", &path, wdmm_ffmpeg::HlsMode::DirectCopy)
            .await
            .unwrap();
        assert_eq!(proxy.shared.lock().unwrap().hls_sessions.len(), 2);

        proxy.stop();
        let _ = std::fs::remove_file(&path);
    }
}
