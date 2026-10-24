//! `wdmm-ffi` — C ABI surface for flutter_rust_bridge (Android) and
//! csbindgen (Windows). Thin conversion layer, no business logic.
//!
//! ## Design
//! - All functions are `extern "C"` `#[no_mangle]`.
//! - Strings are returned as `FfiString` (owned, caller must free via
//!   [`wdmm_ffi_free_string`]).
//! - Queue status is returned as JSON (small, easy to decode in Dart/C#).
//! - The OpenAPI server is started/stopped via a module-level Tokio runtime
//!   so it keeps running across FFI calls.

#![allow(dead_code)]

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::sync::OnceLock;
use tokio::runtime::Runtime;
use wdmm_queue::TaskQueue;
use wdmm_storage::LibraryDatabase;

// ── Runtime singleton ─────────────────────────────────────────────────────────

fn rt() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| Runtime::new().expect("tokio runtime"))
}

// ── FFI string ────────────────────────────────────────────────────────────────

/// Owned C string. Caller must pass it back to [`wdmm_ffi_free_string`].
#[repr(C)]
pub struct FfiString {
    pub ptr: *mut c_char,
}

impl FfiString {
    fn from(s: impl Into<Vec<u8>>) -> Self {
        let s = CString::new(s).unwrap_or_else(|_| CString::new("?").unwrap());
        Self { ptr: s.into_raw() }
    }

    fn ok() -> Self {
        Self::from("ok")
    }
}

/// Free an `FfiString` allocated by Rust.
#[no_mangle]
pub extern "C" fn wdmm_ffi_free_string(s: FfiString) {
    if !s.ptr.is_null() {
        unsafe { drop(CString::from_raw(s.ptr)) }
    }
}

// ── Version / smoke ───────────────────────────────────────────────────────────

/// Return the wdmm-core version string.
#[no_mangle]
pub extern "C" fn wdmm_version() -> FfiString {
    FfiString::from(env!("CARGO_PKG_VERSION"))
}

/// Smoke-test — returns `"Hello from WDMM Rust core!"`.
#[no_mangle]
pub extern "C" fn wdmm_hello() -> FfiString {
    FfiString::from("Hello from WDMM Rust core!")
}

// ── Queue status ──────────────────────────────────────────────────────────────

/// Global queue (created lazily on first call to a queue function).
fn global_queue() -> &'static TaskQueue {
    static Q: OnceLock<TaskQueue> = OnceLock::new();
    Q.get_or_init(TaskQueue::new)
}

/// Return queue tasks as a JSON array.
///
/// Returned string is owned — caller must pass it to [`wdmm_ffi_free_string`].
///
/// Schema per task:
/// ```json
/// {"id":"…","kind":"…","status":"…","progress":0.5,"total_bytes":1000,
///  "transferred_bytes":500,"error":null}
/// ```
#[no_mangle]
pub extern "C" fn wdmm_queue_status_json() -> FfiString {
    let tasks = rt().block_on(global_queue().list());
    let arr: Vec<_> = tasks
        .iter()
        .map(|t| {
            serde_json::json!({
                "id":                 t.id,
                "kind":               format!("{:?}", t.kind),
                "status":             format!("{:?}", t.status),
                "progress":           t.progress,
                "total_bytes":        t.total_bytes,
                "transferred_bytes":  t.transferred_bytes,
                "error":              t.error,
            })
        })
        .collect();
    FfiString::from(serde_json::to_string(&arr).unwrap_or_else(|_| "[]".into()))
}

/// Cancel a task by id. Returns `"ok"` on success, `"not_found"` if the id is
/// unknown, or an error message otherwise.
///
/// `task_id` must be a valid UTF-8 null-terminated C string.
/// Returns an owned `FfiString`; caller must free it.
#[no_mangle]
pub extern "C" fn wdmm_queue_cancel(task_id: *const c_char) -> FfiString {
    let id = match ptr_to_str(task_id) {
        Some(s) => s,
        None => return FfiString::from("invalid utf-8"),
    };
    let result = rt().block_on(global_queue().cancel(id));
    match result {
        Ok(()) => FfiString::ok(),
        Err(wdmm_queue::QueueError::NotFound(_)) => FfiString::from("not_found"),
        Err(e) => FfiString::from(e.to_string()),
    }
}

/// Enqueue a new download task from JSON.
///
/// Expected JSON shape (all fields optional except `id` and `kind`):
/// ```json
/// {
///   "id":          "uuid-...",
///   "kind":        "download_track",
///   "remote_path": "/Music/foo.mp3",
///   "local_path":  "/data/.../foo.mp3",
///   "total_bytes": 5000000
/// }
/// ```
///
/// Returns `"ok"` on success, `"error:<message>"` on failure.
/// `json` must be a valid null-terminated UTF-8 C string.
///
/// ## Feature flag
/// This function is always available via FFI. On the Dart side, it is gated
/// by `kRustQueueEnabled` (default `false`). Flip the flag to `true` in
/// settings to route single-file downloads through the Rust `SerialPump`.
/// The Dart `_pump` loop remains active while the flag is `false`.
#[no_mangle]
pub extern "C" fn wdmm_queue_enqueue(json: *const c_char) -> FfiString {
    let s = match ptr_to_str(json) {
        Some(s) => s,
        None => return FfiString::from("error:null json pointer"),
    };
    let v: serde_json::Value = match serde_json::from_str(s) {
        Ok(v) => v,
        Err(e) => return FfiString::from(format!("error:json parse: {e}")),
    };
    let id = match v["id"].as_str() {
        Some(id) => id.to_string(),
        None => return FfiString::from("error:missing 'id'"),
    };
    let kind_str = v["kind"].as_str().unwrap_or("download_track");
    let kind = match kind_str {
        "download_track"    => wdmm_core::models::TaskKind::DownloadTrack,
        "download_playlist" => wdmm_core::models::TaskKind::DownloadPlaylist,
        "upload_file"       => wdmm_core::models::TaskKind::UploadFile,
        "backup"            => wdmm_core::models::TaskKind::Backup,
        "sync_credentials"  => wdmm_core::models::TaskKind::SyncCredentials,
        "sync_library"      => wdmm_core::models::TaskKind::SyncLibrary,
        "sync_playlists"    => wdmm_core::models::TaskKind::SyncPlaylists,
        "sync_manga"        => wdmm_core::models::TaskKind::SyncManga,
        _                   => wdmm_core::models::TaskKind::Other,
    };
    let task = wdmm_core::models::QueueTask {
        id,
        kind,
        status: wdmm_core::models::TaskStatus::Pending,
        progress: 0.0,
        total_bytes: v["total_bytes"].as_u64().unwrap_or(0),
        transferred_bytes: 0,
        created: chrono::Utc::now(),
        updated: chrono::Utc::now(),
        attempts: 0,
        error: None,
        remote_path: v["remote_path"].as_str().map(Into::into),
        local_path: v["local_path"].as_str().map(Into::into),
    };
    match rt().block_on(global_queue().enqueue(task)) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Return the number of pending tasks.
#[no_mangle]
pub extern "C" fn wdmm_queue_pending_count() -> i32 {
    rt().block_on(async {
        global_queue()
            .list_by_status(wdmm_core::models::TaskStatus::Pending)
            .await
            .len() as i32
    })
}

// ── OpenAPI server ────────────────────────────────────────────────────────────

use wdmm_openapi::{ApiServer, BindMode, ServerConfig};

/// Shared server handle.
fn server_cell() -> &'static std::sync::Mutex<Option<ApiServer>> {
    static S: OnceLock<std::sync::Mutex<Option<ApiServer>>> = OnceLock::new();
    S.get_or_init(|| std::sync::Mutex::new(None))
}

/// Start the OpenAPI server on 127.0.0.1 at the given port.
///
/// Returns the bearer token as an owned `FfiString` on success, or an error
/// message string on failure. Caller must free the returned string.
/// If the server is already running this is a no-op and returns the existing
/// token.
#[no_mangle]
pub extern "C" fn wdmm_openapi_start(port: u16) -> FfiString {
    let mut guard = server_cell().lock().unwrap();
    if let Some(ref s) = *guard {
        return FfiString::from(s.token().to_string());
    }

    // Open in-memory DB for the server (real paths injected by platform later)
    let db = match LibraryDatabase::open(std::path::Path::new(":memory:")) {
        Ok(db) => std::sync::Arc::new(db),
        Err(e) => return FfiString::from(format!("db error: {e}")),
    };
    let queue = std::sync::Arc::new(TaskQueue::new());

    let config = ServerConfig {
        bind_mode: BindMode::Local,
        port,
        token: uuid::Uuid::new_v4().to_string(),
    };
    let token = config.token.clone();

    match rt().block_on(ApiServer::start(config, db, queue)) {
        Ok(server) => {
            *guard = Some(server);
            FfiString::from(token)
        }
        Err(e) => FfiString::from(format!("start error: {e}")),
    }
}

/// Stop the OpenAPI server. No-op if not running.
/// Returns `"ok"`.
#[no_mangle]
pub extern "C" fn wdmm_openapi_stop() -> FfiString {
    let mut guard = server_cell().lock().unwrap();
    if let Some(server) = guard.take() {
        rt().block_on(server.stop());
    }
    FfiString::ok()
}

/// Returns `1` if the OpenAPI server is running, `0` otherwise.
#[no_mangle]
pub extern "C" fn wdmm_openapi_is_running() -> i32 {
    i32::from(server_cell().lock().unwrap().is_some())
}

/// Return the port the OpenAPI server is bound to, or `-1` if not running.
#[no_mangle]
pub extern "C" fn wdmm_openapi_port() -> i32 {
    match server_cell().lock().unwrap().as_ref() {
        Some(s) => s.port() as i32,
        None => -1,
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn ptr_to_str(p: *const c_char) -> Option<&'static str> {
    if p.is_null() {
        return None;
    }
    let cs = unsafe { CStr::from_ptr(p) };
    cs.to_str().ok().map(|s| {
        // Safety: string lives as long as caller keeps it alive; only used
        // within the same synchronous FFI call.
        unsafe { &*(s as *const str) }
    })
}

fn ptr_to_bytes(p: *const u8, len: usize) -> Option<Vec<u8>> {
    if p.is_null() || len == 0 {
        return None;
    }
    Some(unsafe { std::slice::from_raw_parts(p, len).to_vec() })
}

// ── Crypto FFI ────────────────────────────────────────────────────────────────

use base64::prelude::*;
use wdmm_crypto::CredentialVault;

/// Encrypt `data` bytes with AES-256-GCM using `passphrase`.
///
/// Returns base64-encoded ciphertext as an owned `FfiString`, or
/// `"error:<message>"` on failure. Caller must free the result.
///
/// # Safety
/// `passphrase` must be a valid null-terminated UTF-8 C string.
/// `data` must point to `data_len` readable bytes (or be null).
#[no_mangle]
pub extern "C" fn wdmm_crypto_encrypt(
    passphrase: *const c_char,
    data: *const u8,
    data_len: usize,
) -> FfiString {
    let pass = match ptr_to_str(passphrase) {
        Some(s) => s,
        None => return FfiString::from("error:invalid passphrase pointer"),
    };
    let bytes = match ptr_to_bytes(data, data_len) {
        Some(b) => b,
        None => return FfiString::from("error:null data"),
    };
    match CredentialVault::encrypt(&bytes, pass) {
        Ok(enc) => FfiString::from(BASE64_STANDARD.encode(&enc)),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Decrypt base64-encoded ciphertext with `passphrase`.
///
/// Returns the plaintext bytes as a UTF-8 string (best-effort; binary payloads
/// should use the dedicated bytes API coming in a later round). Returns
/// `"error:<message>"` on failure. Caller must free the result.
///
/// # Safety
/// Both pointers must be valid null-terminated UTF-8 C strings.
#[no_mangle]
pub extern "C" fn wdmm_crypto_decrypt(
    passphrase: *const c_char,
    data_b64: *const c_char,
) -> FfiString {
    let pass = match ptr_to_str(passphrase) {
        Some(s) => s,
        None => return FfiString::from("error:invalid passphrase pointer"),
    };
    let b64 = match ptr_to_str(data_b64) {
        Some(s) => s,
        None => return FfiString::from("error:null data_b64 pointer"),
    };
    let encrypted = match BASE64_STANDARD.decode(b64) {
        Ok(b) => b,
        Err(e) => return FfiString::from(format!("error:base64 decode: {e}")),
    };
    match CredentialVault::decrypt(&encrypted, pass) {
        Ok(plain) => FfiString::from(String::from_utf8_lossy(&plain).to_string()),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

// ── Sync FFI ─────────────────────────────────────────────────────────────────

/// Trigger a sync run (credentials + library + playlists) against a remote
/// WebDAV driver.
///
/// Parameters (all null-terminated UTF-8 C strings):
/// - `webdav_url`:  WebDAV base URL (e.g. `"https://nas.local/dav/"`)
/// - `webdav_user`: HTTP Basic username (empty string for no auth)
/// - `webdav_pass`: HTTP Basic password (empty string for no auth)
/// - `remote_dir`:  Remote directory used as the sync root (e.g. `"/wdmm_sync"`)
///
/// Returns a JSON summary string (see [`wdmm_sync::run_sync_all`]) or
/// `"error:<message>"`. Caller must free the result.
///
/// # Safety
/// All pointer parameters must be valid null-terminated UTF-8 C strings.
#[no_mangle]
pub extern "C" fn wdmm_sync_trigger(
    webdav_url: *const c_char,
    webdav_user: *const c_char,
    webdav_pass: *const c_char,
    remote_dir: *const c_char,
) -> FfiString {
    let url = match ptr_to_str(webdav_url) {
        Some(s) => s,
        None => return FfiString::from("error:invalid webdav_url"),
    };
    let user = ptr_to_str(webdav_user).unwrap_or("");
    let pass = ptr_to_str(webdav_pass).unwrap_or("");
    let dir = ptr_to_str(remote_dir).unwrap_or("/wdmm_sync");

    let db = match LibraryDatabase::open(std::path::Path::new(":memory:")) {
        Ok(db) => std::sync::Arc::new(db),
        Err(e) => return FfiString::from(format!("error:db: {e}")),
    };
    let driver = match wdmm_drivers::WebDavDriver::new(url, user, pass) {
        Ok(d) => std::sync::Arc::new(d) as std::sync::Arc<dyn wdmm_drivers::CloudDriver>,
        Err(e) => return FfiString::from(format!("error:driver: {e}")),
    };

    let result = rt().block_on(wdmm_sync::run_sync_all(db, driver, dir));
    match result {
        Ok(summary) => FfiString::from(summary.to_string()),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

// ── Proxy FFI ─────────────────────────────────────────────────────────────────
//
// The proxy is a process-global singleton, like the OpenAPI server. It starts
// lazily on the first `wdmm_proxy_start` call and stops on `wdmm_proxy_stop`
// (or on Drop / process exit). The idle-TTL inside ProxyHandle also stops it
// automatically after inactivity; the next `wdmm_proxy_register_file` call
// will restart it transparently.

use wdmm_proxy::ProxyHandle;

fn proxy_cell() -> &'static std::sync::Mutex<Option<ProxyHandle>> {
    static P: OnceLock<std::sync::Mutex<Option<ProxyHandle>>> = OnceLock::new();
    P.get_or_init(|| std::sync::Mutex::new(None))
}

/// Start the local HTTP proxy bound to `127.0.0.1` on the given port.
/// Use `port = 0` to let the OS assign a free port.
///
/// Returns the assigned port as a decimal string, or `"error:<message>"`.
/// Idempotent — if the proxy is already running this is a no-op and returns
/// the existing port. Caller must free the result.
#[no_mangle]
pub extern "C" fn wdmm_proxy_start(port: u16) -> FfiString {
    let mut guard = proxy_cell().lock().unwrap();
    // If already running, return the existing port.
    if let Some(ref h) = *guard {
        if let Some(addr) = h.addr() {
            return FfiString::from(addr.port().to_string());
        }
    }
    let bind: std::net::SocketAddr = ([127, 0, 0, 1], port).into();
    let handle = ProxyHandle::new(bind);
    // Trigger the lazy server start with an ephemeral temp file.
    // We call register_file on a temp path just to ensure the server starts
    // and we can read back the port. The file registration is immediately
    // discarded (unregistered).
    let result = rt().block_on(async {
        let tmp = std::env::temp_dir()
            .join(format!("wdmm-proxy-init-{}.tmp", std::process::id()));
        std::fs::write(&tmp, b"").ok();
        let reg = handle.register_file(&tmp, None).await;
        if let Ok(r) = &reg {
            handle.unregister(&r.token);
        }
        let _ = std::fs::remove_file(&tmp);
        handle.addr()
    });
    *guard = Some(handle);
    match result {
        Some(addr) => FfiString::from(addr.port().to_string()),
        None => FfiString::from("error:server failed to bind"),
    }
}

/// Stop the local HTTP proxy. No-op if not running. Returns `"ok"`.
#[no_mangle]
pub extern "C" fn wdmm_proxy_stop() -> FfiString {
    if let Some(h) = proxy_cell().lock().unwrap().take() {
        h.stop();
    }
    FfiString::ok()
}

/// Returns the bound port of the proxy, or `-1` if not running.
#[no_mangle]
pub extern "C" fn wdmm_proxy_port() -> i32 {
    proxy_cell()
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|h| h.addr())
        .map(|a| a.port() as i32)
        .unwrap_or(-1)
}

/// Register a local file with the proxy.
///
/// Parameters (null-terminated UTF-8 C strings):
/// - `file_path`: absolute path to the local file.
/// - `name`: optional display name used for Content-Type guessing (e.g.
///   `"movie.mp4"`). Pass `NULL` to omit.
///
/// Returns a JSON object:
/// ```json
/// {"token":"3f7a…","path_template":"/s/3f7a…","port":43221}
/// ```
/// or `"error:<message>"` on failure. Caller must free the result.
///
/// The caller builds the URL as `http://127.0.0.1:{port}{path_template}`.
///
/// # Safety
/// `file_path` must be a valid null-terminated UTF-8 C string.
/// `name` may be `NULL` (treated as no name).
#[no_mangle]
pub extern "C" fn wdmm_proxy_register_file(
    file_path: *const std::os::raw::c_char,
    name: *const std::os::raw::c_char,
) -> FfiString {
    let path_str = match ptr_to_str(file_path) {
        Some(s) => s,
        None => return FfiString::from("error:invalid file_path"),
    };
    let name_opt: Option<String> = ptr_to_str(name).map(|s| s.to_string());

    let guard = proxy_cell().lock().unwrap();
    let handle = match guard.as_ref() {
        Some(h) => h,
        None => return FfiString::from("error:proxy not started — call wdmm_proxy_start first"),
    };
    let path = std::path::Path::new(path_str);
    let reg = rt().block_on(handle.register_file(path, name_opt));
    match reg {
        Ok(r) => {
            let port = handle.addr().map(|a| a.port()).unwrap_or(0);
            FfiString::from(
                serde_json::json!({
                    "token":         r.token,
                    "path_template": r.path_template,
                    "port":          port,
                })
                .to_string(),
            )
        }
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Unregister a token previously returned by `wdmm_proxy_register_file`.
///
/// `token` must be a valid null-terminated UTF-8 C string.
/// Returns `"ok"`.
#[no_mangle]
pub extern "C" fn wdmm_proxy_unregister(token: *const std::os::raw::c_char) -> FfiString {
    let tok = match ptr_to_str(token) {
        Some(s) => s,
        None => return FfiString::from("error:invalid token"),
    };
    if let Some(h) = proxy_cell().lock().unwrap().as_ref() {
        h.unregister(tok);
    }
    FfiString::ok()
}

// ── Proxy HLS FFI ─────────────────────────────────────────────────────────────
//
// Thin wrappers around `ProxyHandle::register_hls`, `seek_hls`, and
// `unregister_hls`. The proxy singleton must already be started via
// `wdmm_proxy_start`. DLNA cast takes the URL from the caller — the proxy only
// supplies the token path; the caller composes the full URL.

/// Register an HLS session with the proxy.
///
/// `device_id` — unique identifier for the player device (null-terminated UTF-8).
/// `file_path` — absolute path to the input media file (null-terminated UTF-8).
/// `mode_json` — transcode mode as a JSON object (may be `NULL` for the default):
///
/// ```json
/// {"mode":"direct_copy"}
/// {"mode":"compat","video_codec":"libx264","audio_codec":"aac"}
/// ```
///
/// Returns JSON on success:
/// ```json
/// {"token":"3f7a…","playlist_template":"/h/3f7a…/index.m3u8","port":43221}
/// ```
/// or `"error:<message>"` on failure. Caller must free the result via
/// [`wdmm_ffi_free_string`].
///
/// # Safety
/// `device_id` and `file_path` must be valid null-terminated UTF-8 C strings.
/// `mode_json` may be `NULL` (treated as `{"mode":"direct_copy"}`).
#[no_mangle]
pub extern "C" fn wdmm_proxy_register_hls(
    device_id: *const c_char,
    file_path: *const c_char,
    mode_json: *const c_char,
) -> FfiString {
    let device_str = match ptr_to_str(device_id) {
        Some(s) => s,
        None => return FfiString::from("error:invalid device_id"),
    };
    let path_str = match ptr_to_str(file_path) {
        Some(s) => s,
        None => return FfiString::from("error:invalid file_path"),
    };
    let mode_str = ptr_to_str(mode_json).unwrap_or(r#"{"mode":"direct_copy"}"#);
    let mode = parse_hls_mode(mode_str);

    let guard = proxy_cell().lock().unwrap();
    let handle = match guard.as_ref() {
        Some(h) => h,
        None => return FfiString::from("error:proxy not started — call wdmm_proxy_start first"),
    };
    let path = std::path::Path::new(path_str);
    let reg = rt().block_on(handle.register_hls(device_str, path, mode));
    match reg {
        Ok(r) => {
            let port = handle.addr().map(|a| a.port()).unwrap_or(0);
            FfiString::from(
                serde_json::json!({
                    "token":             r.token,
                    "playlist_template": r.playlist_template,
                    "port":              port,
                })
                .to_string(),
            )
        }
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Seek an active HLS session to a new position.
///
/// Kills the current transcode job for `token` and restarts it from
/// `offset_secs` seconds.  Returns `"ok"` or `"error:<message>"`.
/// Caller must free the result.
///
/// # Safety
/// `token` must be a valid null-terminated UTF-8 C string.
#[no_mangle]
pub extern "C" fn wdmm_proxy_hls_seek(
    token: *const c_char,
    offset_secs: f64,
) -> FfiString {
    let tok = match ptr_to_str(token) {
        Some(s) => s,
        None => return FfiString::from("error:invalid token"),
    };
    let guard = proxy_cell().lock().unwrap();
    let handle = match guard.as_ref() {
        Some(h) => h,
        None => return FfiString::from("error:proxy not started"),
    };
    match rt().block_on(handle.seek_hls(tok, offset_secs)) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Unregister an HLS session. The transcode job is killed and the segment
/// directory is removed. Returns `"ok"`.
///
/// # Safety
/// `token` must be a valid null-terminated UTF-8 C string.
#[no_mangle]
pub extern "C" fn wdmm_proxy_unregister_hls(token: *const c_char) -> FfiString {
    let tok = match ptr_to_str(token) {
        Some(s) => s,
        None => return FfiString::from("error:invalid token"),
    };
    if let Some(h) = proxy_cell().lock().unwrap().as_ref() {
        h.unregister_hls(tok);
    }
    FfiString::ok()
}

/// Parse an HLS mode JSON object.
///
/// ```json
/// {"mode":"direct_copy"}
/// {"mode":"compat","video_codec":"libx264","audio_codec":"aac"}
/// ```
///
/// Unknown or missing `mode` field → [`HlsMode::DirectCopy`].
fn parse_hls_mode(json: &str) -> wdmm_ffmpeg::HlsMode {
    let v: serde_json::Value = serde_json::from_str(json).unwrap_or_default();
    match v["mode"].as_str().unwrap_or("direct_copy") {
        "compat" => wdmm_ffmpeg::HlsMode::Compat {
            video_codec: v["video_codec"].as_str().map(Into::into),
            audio_codec: v["audio_codec"].as_str().map(Into::into),
        },
        _ => wdmm_ffmpeg::HlsMode::DirectCopy,
    }
}

// ── Proxy stream FFI ──────────────────────────────────────────────────────────
//
// Exposes `ProxyHandle::register_driver` as a C ABI for Android MustProxy /
// crypt-wrapper local stream playback. The callback is invoked synchronously
// from the Tokio request-handler thread; Dart callers needing true async
// decrypt should use a NativeCallable.listener port bridge.

/// Range callback type for `wdmm_proxy_register_stream`.
///
/// Invoked from the proxy HTTP request-handler thread (a Tokio worker) for
/// every Range/full-content request on the registered token.
///
/// Parameters:
/// - `user_data` — opaque pointer passed through from `wdmm_proxy_register_stream`.
/// - `start`     — inclusive byte offset.
/// - `end`       — inclusive end offset; **`-1` means read to EOF** (unknown size).
/// - `out_buf`   — on success (`return 0`), set to a buffer allocated via
///   [`wdmm_proxy_stream_alloc_buf`] and filled with the requested bytes.
///   Rust takes ownership and frees it after consuming.
/// - `out_len`   — on success, set to the byte count in `*out_buf`.
///
/// Return `0` on success, any negative value on error (the proxy will emit
/// an IO error to the HTTP client).
///
/// **Thread safety**: the callback and `user_data` must be safe to call from
/// any thread for the full lifetime of the registration.
pub type WdmmProxyRangeCallback = unsafe extern "C" fn(
    user_data: *mut std::os::raw::c_void,
    start: i64,
    end: i64,
    out_buf: *mut *mut u8,
    out_len: *mut usize,
) -> i32;

/// Allocate `len` bytes in Rust's heap for use as a callback output buffer.
///
/// The C callback must call this to allocate the output buffer it sets as
/// `*out_buf`. Rust reclaims the memory automatically after consuming it.
///
/// Returns a non-null pointer on success, null if `len == 0`.
///
/// ## Usage in the callback
/// ```c
/// uint8_t* buf = wdmm_proxy_stream_alloc_buf(chunk_len);
/// memcpy(buf, source, chunk_len);
/// *out_buf = buf;
/// *out_len = chunk_len;
/// return 0; // success
/// ```
#[no_mangle]
pub extern "C" fn wdmm_proxy_stream_alloc_buf(len: usize) -> *mut u8 {
    if len == 0 {
        return std::ptr::null_mut();
    }
    let mut v = Vec::<u8>::with_capacity(len);
    let ptr = v.as_mut_ptr();
    std::mem::forget(v); // caller takes ownership
    ptr
}

/// Free a buffer previously returned by [`wdmm_proxy_stream_alloc_buf`].
///
/// Rust calls this automatically after consuming the callback output. Also
/// exported so C can call it on error paths (e.g. when a callback succeeded
/// but the registration failed).
///
/// No-op if `buf` is null or `len` is 0.
///
/// # Safety
/// `buf` must have been allocated via [`wdmm_proxy_stream_alloc_buf`] and
/// must not have been freed already.
#[no_mangle]
pub unsafe extern "C" fn wdmm_proxy_stream_free_buf(buf: *mut u8, len: usize) {
    if !buf.is_null() && len > 0 {
        drop(Vec::from_raw_parts(buf, len, len));
    }
}

// ── C callback helpers ────────────────────────────────────────────────────────

/// Wraps `*mut c_void` to implement `Send + Sync`.
///
/// Safety invariant: the C caller guarantees that `user_data` is accessible
/// from any thread for the lifetime of the registration.
struct UnsafeUserData(*mut std::os::raw::c_void);
unsafe impl Send for UnsafeUserData {}
unsafe impl Sync for UnsafeUserData {}

/// Thread-safe wrapper for the C range callback + user_data.
struct StreamCallbackCtx {
    cb: WdmmProxyRangeCallback,
    user_data: UnsafeUserData,
}
unsafe impl Send for StreamCallbackCtx {}
unsafe impl Sync for StreamCallbackCtx {}

/// Build a `BoxStream` from one invocation of the C callback.
///
/// Called on each Range (or full-content) HTTP request. The callback fills a
/// Rust-heap buffer; we consume it as a single-chunk stream.
fn invoke_c_callback(
    ctx: &StreamCallbackCtx,
    start: i64,
    end: i64,
) -> wdmm_proxy::BoxStream {
    use bytes::Bytes;
    use std::io;

    let mut out_buf: *mut u8 = std::ptr::null_mut();
    let mut out_len: usize = 0;

    // Safety: caller holds Arc<StreamCallbackCtx> alive.
    let result = unsafe { (ctx.cb)(ctx.user_data.0, start, end, &mut out_buf, &mut out_len) };

    if result < 0 || out_buf.is_null() || out_len == 0 {
        return Box::pin(futures::stream::once(futures::future::ready(
            Err(io::Error::new(
                io::ErrorKind::Other,
                format!("wdmm_proxy stream callback returned {result}"),
            )),
        )));
    }

    // Safety: buffer was allocated by wdmm_proxy_stream_alloc_buf (Rust heap).
    let chunk = unsafe { Vec::from_raw_parts(out_buf, out_len, out_len) };
    Box::pin(futures::stream::once(futures::future::ready(
        Ok::<Bytes, io::Error>(Bytes::from(chunk)),
    )))
}

/// Register an in-process Range/byte stream with the proxy.
///
/// This is the C ABI equivalent of `ProxyHandle::register_driver`. Use it for
/// `ContentDeliveryKind::LocalStream` items (e.g. crypt-wrapper drivers whose
/// `get()` returns no `raw_url`).
///
/// ## Parameters
/// - `size`      — known file size in bytes; `0` if unknown (streams to EOF).
/// - `name`      — nullable; used only for Content-Type guessing (`"video.mp4"`
///   etc.). Does not affect routing or auth.
/// - `cb`        — Range callback; see [`WdmmProxyRangeCallback`].
/// - `user_data` — opaque pointer passed to every `cb` invocation.
///
/// ## Returns
/// JSON on success:
/// ```json
/// {"token":"3f7a…","path_template":"/s/3f7a…","port":N}
/// ```
/// or `"error:<message>"` on failure. Caller must free the result via
/// [`wdmm_ffi_free_string`].
///
/// Caller builds the URL as `http://127.0.0.1:{port}{path_template}`.
///
/// ## Dart/Flutter note
/// The callback is invoked **synchronously** from a Tokio worker thread.
/// For async decrypt (e.g. calling `openContentRange` across a Dart isolate),
/// use a `NativeCallable.listener` port bridge: the Dart side enqueues the
/// request and signals back; the C callback blocks until the Dart side replies.
/// A fully async two-step bridge is a future improvement.
///
/// # Safety
/// `cb` must be a valid function pointer for the lifetime of the registration.
/// `user_data` must remain valid from any thread for the lifetime of the
/// registration. `name` may be null.
#[no_mangle]
pub unsafe extern "C" fn wdmm_proxy_register_stream(
    size: u64,
    name: *const c_char,
    cb: WdmmProxyRangeCallback,
    user_data: *mut std::os::raw::c_void,
) -> FfiString {
    use std::sync::Arc;

    let name_opt: Option<String> = if name.is_null() {
        None
    } else {
        ptr_to_str(name).map(|s| s.to_string())
    };

    let guard = proxy_cell().lock().unwrap();
    let handle = match guard.as_ref() {
        Some(h) => h,
        None => return FfiString::from("error:proxy not started — call wdmm_proxy_start first"),
    };

    let ctx = Arc::new(StreamCallbackCtx { cb, user_data: UnsafeUserData(user_data) });

    let reg = rt().block_on(handle.register_driver(size, name_opt, move |start, end| {
        invoke_c_callback(&ctx, start, end)
    }));

    match reg {
        Ok(r) => {
            let port = handle.addr().map(|a| a.port()).unwrap_or(0);
            FfiString::from(
                serde_json::json!({
                    "token":         r.token,
                    "path_template": r.path_template,
                    "port":          port,
                })
                .to_string(),
            )
        }
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Renew the Range callback for an existing stream registration.
///
/// Use after credential refresh or when the decryption key has changed. The
/// new callback will be used for all subsequent Range requests on `token`.
/// No-op if `token` is unknown or refers to a file (not stream) registration.
///
/// Returns `"ok"`. Caller must free the result.
///
/// # Safety
/// Same requirements as [`wdmm_proxy_register_stream`].
#[no_mangle]
pub unsafe extern "C" fn wdmm_proxy_renew_stream_callback(
    token: *const c_char,
    cb: WdmmProxyRangeCallback,
    user_data: *mut std::os::raw::c_void,
) -> FfiString {
    use std::sync::Arc;

    let tok = match ptr_to_str(token) {
        Some(s) => s,
        None => return FfiString::from("error:invalid token"),
    };
    let guard = proxy_cell().lock().unwrap();
    if let Some(h) = guard.as_ref() {
        let ctx = Arc::new(StreamCallbackCtx { cb, user_data: UnsafeUserData(user_data) });
        h.renew_callback(tok, move |start, end| invoke_c_callback(&ctx, start, end));
    }
    FfiString::ok()
}

/// Discover DLNA/UPnP MediaRenderer devices on the LAN using SSDP.
///
/// `timeout_ms` — discovery window in milliseconds (clamped to 200–10000).
///
/// Returns a JSON array of renderer objects:
/// ```json
/// [{"usn":"uuid:…","location":"http://…","server":"…"}]
/// ```
/// or `"error:<message>"` on failure. Caller must free the result.
#[no_mangle]
pub extern "C" fn wdmm_dlna_discover_json(timeout_ms: u32) -> FfiString {
    use std::time::Duration;
    use wdmm_dlna::SsdpDiscovery;

    let ms = timeout_ms.clamp(200, 10_000) as u64;
    let result = rt().block_on(SsdpDiscovery::new().discover(Duration::from_millis(ms)));
    match result {
        Ok(renderers) => {
            let arr: Vec<_> = renderers
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "usn": r.usn,
                        "location": r.location,
                        "server": r.server,
                    })
                })
                .collect();
            FfiString::from(serde_json::to_string(&arr).unwrap_or_else(|_| "[]".into()))
        }
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Cast a media URL to a DLNA renderer using AVTransport SOAP.
///
/// Parameters (null-terminated UTF-8 C strings):
/// - `renderer_location`: the renderer description URL from SSDP discovery
/// - `media_url`:         the URL of the media to play (e.g. the local proxy URL)
///
/// Returns `"ok"` on success, `"error:<message>"` on failure. Caller must free.
///
/// # Safety
/// Both pointers must be valid null-terminated UTF-8 C strings.
#[no_mangle]
pub extern "C" fn wdmm_dlna_cast(
    renderer_location: *const c_char,
    media_url: *const c_char,
) -> FfiString {
    use wdmm_dlna::DlnaRenderer;

    let location = match ptr_to_str(renderer_location) {
        Some(s) => s,
        None => return FfiString::from("error:invalid renderer_location"),
    };
    let url = match ptr_to_str(media_url) {
        Some(s) => s,
        None => return FfiString::from("error:invalid media_url"),
    };

    let renderer = DlnaRenderer {
        usn: String::new(), // not needed for cast
        location: location.to_string(),
        server: None,
    };
    let cast_service = wdmm_dlna::DlnaCastService::new();
    let result = rt().block_on(cast_service.cast(&renderer, url));
    match result {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

// ── Token FFI ─────────────────────────────────────────────────────────────────

/// Get the current OpenAPI bearer token.
///
/// Returns the token string, or `""` if the server is not running.
/// Caller must free the result.
#[no_mangle]
pub extern "C" fn wdmm_token_get() -> FfiString {
    let guard = server_cell().lock().unwrap();
    match guard.as_ref() {
        Some(s) => FfiString::from(rt().block_on(s.current_token())),
        None => FfiString::from(""),
    }
}

/// Rotate the OpenAPI bearer token.
///
/// The old token is immediately invalidated. The new token is persisted in
/// storage so it survives a server restart.
///
/// Returns the new token, or `"error:not_running"` if the server is not
/// running. Caller must free the result.
#[no_mangle]
pub extern "C" fn wdmm_token_rotate() -> FfiString {
    // We must release the Mutex before the async call to avoid a deadlock
    // (the server task may also lock it internally).
    let server_opt = {
        let guard = server_cell().lock().unwrap();
        guard.as_ref().map(|s| s.live_token_arc())
    };
    match server_opt {
        None => FfiString::from("error:not_running"),
        Some(token_lock) => {
            let new_tok = uuid::Uuid::new_v4().to_string();
            rt().block_on(async {
                *token_lock.write().await = new_tok.clone();
            });
            FfiString::from(new_tok)
        }
    }
}

// ── Library DB singleton ──────────────────────────────────────────────────────
//
// The FFI exposes a process-global library database. Real Android/Windows
// integration calls `wdmm_library_init(path)` at app start; falls back to
// ":memory:" for testing. All track/artist/album operations use this.

static LIBRARY_DB: OnceLock<std::sync::Arc<LibraryDatabase>> = OnceLock::new();

fn global_library_db() -> &'static std::sync::Arc<LibraryDatabase> {
    LIBRARY_DB.get_or_init(|| {
        std::sync::Arc::new(
            LibraryDatabase::open(std::path::Path::new(":memory:"))
                .expect("FFI: failed to open in-memory library DB"),
        )
    })
}

/// Initialise the library database at a custom path.
///
/// Must be called once before any other `wdmm_track_*` / `wdmm_artist_*` /
/// `wdmm_album_*` functions. If called after the singleton is already
/// initialised (e.g. from tests), this is a no-op — the path is ignored and
/// `"ok"` is returned.
///
/// `path` must be a valid null-terminated UTF-8 C string (file path or
/// `":memory:"`).
///
/// Returns `"ok"` on success or `"error:<message>"` on failure.
#[no_mangle]
pub extern "C" fn wdmm_library_init(path: *const c_char) -> FfiString {
    let p = match ptr_to_str(path) {
        Some(s) => s,
        None => return FfiString::from("error:invalid path pointer"),
    };
    // OnceLock: if already set this call is a no-op.
    LIBRARY_DB.get_or_init(|| {
        std::sync::Arc::new(
            LibraryDatabase::open(std::path::Path::new(p))
                .expect("FFI: failed to open library DB at given path"),
        )
    });
    FfiString::ok()
}

// ── Track CRUD ────────────────────────────────────────────────────────────────

/// Upsert a track from a JSON object.
///
/// Expected JSON shape (all optional fields may be absent):
/// ```json
/// {
///   "id":           "sha1hex",
///   "title":        "Song Title",
///   "artist":       "Artist Name",
///   "remote_path":  "/path/on/cloud",
///   "account_id":   "acc-uuid",
///   "size":         12345,
///   "album_id":     "alb-uuid",
///   "artist_id":    "art-uuid",
///   "duration_ms":  210000,
///   "genre":        "Rock",
///   "track_number": 3,
///   "disc_number":  1,
///   "year":         2024,
///   "mime_type":    "audio/mpeg"
/// }
/// ```
///
/// Returns `"ok"` on success, `"error:<message>"` otherwise.
/// `json` must be a valid null-terminated UTF-8 C string.
#[no_mangle]
pub extern "C" fn wdmm_track_upsert(json: *const c_char) -> FfiString {
    let s = match ptr_to_str(json) {
        Some(s) => s,
        None => return FfiString::from("error:null json pointer"),
    };
    let v: serde_json::Value = match serde_json::from_str(s) {
        Ok(v) => v,
        Err(e) => return FfiString::from(format!("error:json parse: {e}")),
    };
    let track = match json_to_track(&v) {
        Ok(t) => t,
        Err(e) => return FfiString::from(format!("error:{e}")),
    };
    match global_library_db().upsert_track(&track) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Get a track by id. Returns the track as JSON or `"null"` if not found.
/// Returns `"error:<message>"` on storage error.
#[no_mangle]
pub extern "C" fn wdmm_track_get(id: *const c_char) -> FfiString {
    let id = match ptr_to_str(id) {
        Some(s) => s,
        None => return FfiString::from("error:null id pointer"),
    };
    match global_library_db().get_track(id) {
        Ok(Some(t)) => FfiString::from(serde_json::to_string(&t).unwrap_or_else(|_| "{}".into())),
        Ok(None) => FfiString::from("null"),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// List all tracks. Returns a JSON array.
#[no_mangle]
pub extern "C" fn wdmm_track_list() -> FfiString {
    match global_library_db().list_tracks() {
        Ok(tracks) => FfiString::from(
            serde_json::to_string(&tracks).unwrap_or_else(|_| "[]".into()),
        ),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Search tracks by title/artist. `query` is a case-insensitive substring.
/// Returns a JSON array.
#[no_mangle]
pub extern "C" fn wdmm_track_search(query: *const c_char) -> FfiString {
    let q = match ptr_to_str(query) {
        Some(s) => s,
        None => return FfiString::from("error:null query pointer"),
    };
    match global_library_db().search_tracks(q) {
        Ok(tracks) => FfiString::from(
            serde_json::to_string(&tracks).unwrap_or_else(|_| "[]".into()),
        ),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Delete a track by id. Returns `"ok"`.
#[no_mangle]
pub extern "C" fn wdmm_track_delete(id: *const c_char) -> FfiString {
    let id = match ptr_to_str(id) {
        Some(s) => s,
        None => return FfiString::from("error:null id pointer"),
    };
    match global_library_db().delete_track(id) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Import tracks, playlists, and playlist_entries from an old sqflite
/// database (one-shot — skipped if KV flag `sqflite_imported = 1` is set).
///
/// `old_db_path` must be a valid null-terminated UTF-8 path.
///
/// Returns a JSON object:
/// ```json
/// {"already_done":false,"tracks":42,"playlists":3,"entries":120}
/// ```
/// or `"error:<message>"` on failure. Caller must free the result.
#[no_mangle]
pub extern "C" fn wdmm_library_import_from_sqflite(old_db_path: *const c_char) -> FfiString {
    let path_str = match ptr_to_str(old_db_path) {
        Some(s) => s,
        None => return FfiString::from("error:null path pointer"),
    };
    let old_path = std::path::Path::new(path_str);
    let db = global_library_db();
    match wdmm_storage::LibraryDatabase::import_from_sqflite(old_path, db) {
        Ok(summary) => FfiString::from(
            serde_json::to_string(&summary).unwrap_or_else(|_| "{}".into()),
        ),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

// ── JSON → model helpers ──────────────────────────────────────────────────────

fn json_to_track(v: &serde_json::Value) -> Result<wdmm_core::models::LibraryTrack, String> {
    let id = v["id"].as_str().ok_or("missing 'id'")?.to_string();
    let title = v["title"].as_str().ok_or("missing 'title'")?.to_string();
    let account_id = v["account_id"]
        .as_str()
        .ok_or("missing 'account_id'")?
        .to_string();
    let remote_path = v["remote_path"]
        .as_str()
        .ok_or("missing 'remote_path'")?
        .to_string();
    Ok(wdmm_core::models::LibraryTrack {
        id,
        title,
        artist: v["artist"].as_str().map(Into::into),
        remote_path,
        account_id,
        size: v["size"].as_u64().unwrap_or(0),
        modified: None, // callers can omit for now
        album_id: v["album_id"].as_str().map(Into::into),
        artist_id: v["artist_id"].as_str().map(Into::into),
        duration_ms: v["duration_ms"].as_i64().unwrap_or(0),
        genre: v["genre"].as_str().map(Into::into),
        track_number: v["track_number"].as_i64().map(|n| n as i32),
        disc_number: v["disc_number"].as_i64().map(|n| n as i32),
        year: v["year"].as_i64().map(|n| n as i32),
        mime_type: v["mime_type"].as_str().map(Into::into),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::sync::Mutex;

    // Proxy tests share the process-global proxy_cell() singleton.  Run them
    // sequentially behind this lock to avoid one test's stop() tearing down the
    // proxy mid-request of another test.
    fn proxy_lock() -> &'static Mutex<()> {
        static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn version_smoke() {
        let v = wdmm_version();
        assert!(!v.ptr.is_null());
        let s = unsafe { CStr::from_ptr(v.ptr) }.to_str().unwrap();
        assert!(!s.is_empty());
        wdmm_ffi_free_string(v);
    }

    #[test]
    fn hello_smoke() {
        let h = wdmm_hello();
        assert!(!h.ptr.is_null());
        wdmm_ffi_free_string(h);
    }

    #[test]
    fn queue_status_empty() {
        // Queue is a global singleton — may contain tasks from other tests.
        // Only verify it parses as a JSON array.
        let s = wdmm_queue_status_json();
        let json = unsafe { CStr::from_ptr(s.ptr) }.to_str().unwrap();
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        assert!(v.is_array(), "expected JSON array, got: {json}");
        wdmm_ffi_free_string(s);
    }

    #[test]
    fn openapi_start_stop() {
        // port 0 → OS assigns a free port
        let tok = wdmm_openapi_start(0);
        assert!(!tok.ptr.is_null());
        let token = unsafe { CStr::from_ptr(tok.ptr) }.to_str().unwrap();
        assert!(!token.is_empty());
        wdmm_ffi_free_string(tok);

        assert_eq!(wdmm_openapi_is_running(), 1);
        assert!(wdmm_openapi_port() > 0);

        let r = wdmm_openapi_stop();
        assert!(!r.ptr.is_null());
        wdmm_ffi_free_string(r);
        assert_eq!(wdmm_openapi_is_running(), 0);
        assert_eq!(wdmm_openapi_port(), -1);
    }

    #[test]
    fn queue_cancel_not_found() {
        let id = CString::new("nonexistent").unwrap();
        let result = wdmm_queue_cancel(id.as_ptr());
        let s = unsafe { CStr::from_ptr(result.ptr) }.to_str().unwrap();
        assert_eq!(s, "not_found");
        wdmm_ffi_free_string(result);
    }

    #[test]
    fn track_ffi_crud() {
        let json = CString::new(r#"{"id":"aabbcc","title":"Test Song","artist":"Artist","remote_path":"/m/test.mp3","account_id":"acc1","size":5000,"duration_ms":180000,"mime_type":"audio/mpeg"}"#).unwrap();

        // Upsert
        let r = wdmm_track_upsert(json.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok", "upsert failed: {s}");
        wdmm_ffi_free_string(r);

        // Get
        let id = CString::new("aabbcc").unwrap();
        let r = wdmm_track_get(id.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert!(s != "null" && !s.starts_with("error:"), "get failed: {s}");
        let v: serde_json::Value = serde_json::from_str(s).unwrap();
        assert_eq!(v["title"], "Test Song");
        wdmm_ffi_free_string(r);

        // List
        let r = wdmm_track_list();
        let arr_s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        let arr: serde_json::Value = serde_json::from_str(arr_s).unwrap();
        assert!(arr.as_array().map(|a| !a.is_empty()).unwrap_or(false));
        wdmm_ffi_free_string(r);

        // Search
        let q = CString::new("song").unwrap();
        let r = wdmm_track_search(q.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        let arr: serde_json::Value = serde_json::from_str(s).unwrap();
        assert!(!arr.as_array().unwrap().is_empty());
        wdmm_ffi_free_string(r);

        // Delete
        let r = wdmm_track_delete(id.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok");
        wdmm_ffi_free_string(r);

        // Gone
        let r = wdmm_track_get(id.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "null");
        wdmm_ffi_free_string(r);
    }

    #[test]
    fn import_sqflite_nonexistent_path() {
        let path = CString::new("/tmp/wdmm_does_not_exist_r6.db").unwrap();
        let r = wdmm_library_import_from_sqflite(path.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        // Either "error:..." or already_done (if prev test left the flag)
        assert!(s.starts_with("error:") || s.contains("already_done"));
        wdmm_ffi_free_string(r);
    }

    #[test]
    fn queue_enqueue_and_status() {
        // Enqueue a download_track task
        let json = CString::new(
            r#"{"id":"qtest-001","kind":"download_track","remote_path":"/m/test.mp3","total_bytes":500000}"#
        ).unwrap();
        let r = wdmm_queue_enqueue(json.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok", "enqueue failed: {s}");
        wdmm_ffi_free_string(r);

        // Verify it shows up in status
        let s = wdmm_queue_status_json();
        let json_str = unsafe { CStr::from_ptr(s.ptr) }.to_str().unwrap();
        let arr: serde_json::Value = serde_json::from_str(json_str).unwrap();
        let found = arr.as_array().unwrap().iter().any(|t| t["id"] == "qtest-001");
        assert!(found, "enqueued task not in status: {json_str}");
        wdmm_ffi_free_string(s);

        // Pending count must be at least 1
        assert!(wdmm_queue_pending_count() >= 1);

        // Cancel it
        let id = CString::new("qtest-001").unwrap();
        let r = wdmm_queue_cancel(id.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok");
        wdmm_ffi_free_string(r);
    }

    #[test]
    fn proxy_register_stream_smoke() {
        let _guard = proxy_lock().lock().unwrap();
        // Start proxy on an OS-assigned port.
        let port_str = wdmm_proxy_start(0);
        let port_s = unsafe { CStr::from_ptr(port_str.ptr) }.to_str().unwrap().to_string();
        wdmm_ffi_free_string(port_str);
        let port: u16 = port_s.parse().expect("proxy_start returns a port");

        // Fixed data served by the C callback.
        static DATA: &[u8] = b"hello-register-stream";

        unsafe extern "C" fn range_cb(
            _user_data: *mut std::os::raw::c_void,
            start: i64,
            end: i64,
            out_buf: *mut *mut u8,
            out_len: *mut usize,
        ) -> i32 {
            let s = start as usize;
            let e = if end < 0 {
                DATA.len()
            } else {
                (end as usize + 1).min(DATA.len())
            };
            if s >= DATA.len() {
                return -1;
            }
            let chunk = &DATA[s..e];
            let buf = wdmm_proxy_stream_alloc_buf(chunk.len());
            if buf.is_null() {
                return -1;
            }
            std::ptr::copy_nonoverlapping(chunk.as_ptr(), buf, chunk.len());
            *out_buf = buf;
            *out_len = chunk.len();
            0
        }

        let name = CString::new("hello.bin").unwrap();
        let r = unsafe {
            wdmm_proxy_register_stream(
                DATA.len() as u64,
                name.as_ptr(),
                range_cb,
                std::ptr::null_mut(),
            )
        };
        let body = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap().to_string();
        wdmm_ffi_free_string(r);

        // Must return JSON with token, path_template, port.
        let v: serde_json::Value =
            serde_json::from_str(&body).expect("register_stream must return JSON");
        assert!(v["token"].is_string(), "missing token in: {body}");
        assert!(
            v["path_template"].as_str().unwrap().starts_with("/s/"),
            "bad path_template in: {body}"
        );
        assert_eq!(v["port"], port as i64);

        let path_template = v["path_template"].as_str().unwrap().to_string();
        let token = v["token"].as_str().unwrap().to_string();

        // GET full content through the proxy — use a fresh single-threaded
        // runtime so we don't block the global rt() that runs the proxy server.
        let url = format!("http://127.0.0.1:{port}{path_template}");
        let got = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                reqwest::Client::new()
                    .get(&url)
                    .send()
                    .await
                    .unwrap()
                    .bytes()
                    .await
                    .unwrap()
            });
        assert_eq!(got.as_ref(), DATA, "stream bytes mismatch");

        // Range request — bytes=5-9.
        let url2 = format!("http://127.0.0.1:{port}{path_template}");
        let got_range = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                reqwest::Client::new()
                    .get(&url2)
                    .header("Range", "bytes=5-9")
                    .send()
                    .await
                    .unwrap()
                    .bytes()
                    .await
                    .unwrap()
            });
        assert_eq!(got_range.as_ref(), &DATA[5..10], "range bytes mismatch");

        // Unregister and stop.
        let tok = CString::new(token).unwrap();
        let r = wdmm_proxy_unregister(tok.as_ptr());
        wdmm_ffi_free_string(r);

        let r = wdmm_proxy_stop();
        wdmm_ffi_free_string(r);
        assert_eq!(wdmm_proxy_port(), -1);
    }

    #[test]
    fn proxy_start_stop_and_register_file() {
        let _guard = proxy_lock().lock().unwrap();
        // Start on an OS-assigned port.
        let port_str = wdmm_proxy_start(0);
        let port_s = unsafe { CStr::from_ptr(port_str.ptr) }.to_str().unwrap().to_string();
        wdmm_ffi_free_string(port_str);
        let port: u16 = port_s.parse().expect("proxy_start returns a port");

        // Proxy is running and reports the same port.
        assert_eq!(wdmm_proxy_port(), port as i32);

        // Register a temp file and expect a JSON {token,path_template,port}.
        let tmp = std::env::temp_dir().join(format!(
            "wdmm-ffi-proxy-{}.bin",
            std::process::id()
        ));
        std::fs::write(&tmp, b"hello-proxy").unwrap();
        let path = CString::new(tmp.to_string_lossy().to_string()).unwrap();
        let name = CString::new("audio.mp3").unwrap();
        let r = wdmm_proxy_register_file(path.as_ptr(), name.as_ptr());
        let body = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap().to_string();
        wdmm_ffi_free_string(r);
        let v: serde_json::Value = serde_json::from_str(&body).expect("register returns JSON");
        assert!(v["token"].is_string(), "body: {body}");
        assert!(v["path_template"].as_str().unwrap().starts_with("/s/"));
        assert_eq!(v["port"], port);
        let token = v["token"].as_str().unwrap().to_string();

        // Unregister.
        let tok = CString::new(token).unwrap();
        let r = wdmm_proxy_unregister(tok.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok");
        wdmm_ffi_free_string(r);

        // Stop.
        let r = wdmm_proxy_stop();
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok");
        wdmm_ffi_free_string(r);
        assert_eq!(wdmm_proxy_port(), -1);

        let _ = std::fs::remove_file(&tmp);
    }
}
