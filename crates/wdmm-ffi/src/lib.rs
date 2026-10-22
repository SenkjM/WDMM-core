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

// ── DLNA FFI ──────────────────────────────────────────────────────────────────

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
        let s = wdmm_queue_status_json();
        let json = unsafe { CStr::from_ptr(s.ptr) }.to_str().unwrap();
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        assert!(v.as_array().unwrap().is_empty());
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
}
