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

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use tokio::runtime::Runtime;
use wdmm_queue::TaskQueue;
use wdmm_storage::{DmrPrefRow, DmrPrefsDatabase, LibraryDatabase};

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

/// Module-level Arc queue — shared between FFI operations and the OpenAPI server.
static QUEUE: OnceLock<std::sync::Arc<TaskQueue>> = OnceLock::new();

/// Global queue reference (created lazily; shared with `global_queue_arc()`).
fn global_queue() -> &'static TaskQueue {
    QUEUE.get_or_init(|| std::sync::Arc::new(TaskQueue::new()))
}

/// Owned Arc clone of the global queue, for passing to `ApiServer::start`.
fn global_queue_arc() -> std::sync::Arc<TaskQueue> {
    std::sync::Arc::clone(QUEUE.get_or_init(|| std::sync::Arc::new(TaskQueue::new())))
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

    // Share the same Arc<LibraryDatabase> + Arc<TaskQueue> as the rest of the FFI.
    let db = std::sync::Arc::clone(global_library_db());
    let queue = global_queue_arc();

    // Resolve the bearer token (file 0600 → KV → generate).
    let token = resolve_api_token(&db);

    let config = ServerConfig {
        bind_mode: BindMode::Local,
        port,
        token: token.clone(),
    };

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

/// Helper: look up an account by ID and construct its WdmmVfs.
fn driver_for_account(
    account_id_str: &str,
) -> Result<std::sync::Arc<dyn wdmm_drivers::WdmmVfs>, FfiString> {
    let account = match global_library_db().get_account(account_id_str) {
        Ok(Some(a)) => a,
        Ok(None) => return Err(FfiString::from("error:account not found")),
        Err(e) => return Err(FfiString::from(format!("error:{e}"))),
    };
    let config = match build_driver_config(&account) {
        Ok(c) => c,
        Err(e) => return Err(FfiString::from(format!("error:{e}"))),
    };
    match wdmm_drivers::DriverRegistry::create_driver(&account.provider_type, config) {
        Ok(d) => Ok(std::sync::Arc::from(d)),
        Err(e) => Err(FfiString::from(format!("error:driver: {e}"))),
    }
}

/// Trigger an incremental sync (credentials + library + manga + playlists).
///
/// Parameters (all null-terminated UTF-8 C strings):
/// - `account_id`: Cloud account ID looked up in the process-global library DB.
/// - `remote_dir`: Remote sync root directory (e.g. `"/wdmm_sync"`).
///
/// Returns a JSON summary (see [`wdmm_sync::run_sync_all`]) or
/// `"error:<message>"`. Caller must free the result.
///
/// # Safety
/// All pointer parameters must be valid null-terminated UTF-8 C strings.
#[no_mangle]
pub extern "C" fn wdmm_sync_trigger(
    account_id: *const c_char,
    remote_dir: *const c_char,
) -> FfiString {
    let account_id_str = match ptr_to_str(account_id) {
        Some(s) => s,
        None => return FfiString::from("error:invalid account_id"),
    };
    let dir = ptr_to_str(remote_dir).unwrap_or("/wdmm_sync");
    let driver = match driver_for_account(account_id_str) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let db = std::sync::Arc::clone(global_library_db());
    let result = rt().block_on(wdmm_sync::run_sync_all(db, driver, dir));
    match result {
        Ok(summary) => FfiString::from(summary.to_string()),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Full-rebuild sync — credentials + music library + manga + playlists.
///
/// Parameters (all null-terminated UTF-8 C strings):
/// - `account_id`:  Cloud account ID looked up in the process-global library DB.
/// - `remote_dir`:  Remote sync root directory (e.g. `"/wdmm_sync"`).
/// - `with_covers`: nonzero to embed cover blobs in the rebuilt base shards
///   (using cover roots from [`wdmm_covers_configure`]); `0` omits covers.
///
/// Returns a JSON summary (see [`wdmm_sync::run_sync_full`]) or
/// `"error:<message>"`. Caller must free the result.
///
/// # Safety
/// All pointer parameters must be valid null-terminated UTF-8 C strings.
#[no_mangle]
pub extern "C" fn wdmm_sync_full(
    account_id: *const c_char,
    remote_dir: *const c_char,
    with_covers: c_int,
) -> FfiString {
    let account_id_str = match ptr_to_str(account_id) {
        Some(s) => s,
        None => return FfiString::from("error:invalid account_id"),
    };
    let dir = ptr_to_str(remote_dir).unwrap_or("/wdmm_sync");
    let cover_roots = if with_covers != 0 {
        global_cover_roots().lock().unwrap().clone()
    } else {
        None
    };
    let driver = match driver_for_account(account_id_str) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let db = std::sync::Arc::clone(global_library_db());
    let result = rt().block_on(wdmm_sync::run_sync_full(db, driver, dir, cover_roots));
    match result {
        Ok(summary) => FfiString::from(summary.to_string()),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Audit the music library remote — cross-reference the manifest against the
/// live remote listing to find missing and orphaned shards.
///
/// - `account_id`: Cloud account ID.
/// - `remote_dir`: Remote sync root (e.g. `"/wdmm_sync"`).
///
/// Returns JSON `{"missing":[…],"orphans":[…],"healthy":bool,"summary":"…"}`
/// or `"error:<message>"`. Caller must free.
#[no_mangle]
pub extern "C" fn wdmm_sync_audit_music(
    account_id: *const c_char,
    remote_dir: *const c_char,
) -> FfiString {
    let account_id_str = match ptr_to_str(account_id) {
        Some(s) => s,
        None => return FfiString::from("error:invalid account_id"),
    };
    let dir = ptr_to_str(remote_dir).unwrap_or("/wdmm_sync");
    let driver = match driver_for_account(account_id_str) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let db = std::sync::Arc::clone(global_library_db());
    let engine = wdmm_sync::LibrarySyncEngine::new(db, driver, dir, None);
    match rt().block_on(engine.audit()) {
        Ok(r) => FfiString::from(serde_json::json!({
            "missing": r.missing,
            "orphans": r.orphans,
            "healthy": r.healthy,
            "summary": r.summary,
        }).to_string()),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Audit the manga library remote — cross-reference the manifest against the
/// live remote listing to find missing and orphaned shards.
///
/// - `account_id`: Cloud account ID.
/// - `remote_dir`: Remote sync root (e.g. `"/wdmm_sync"`).
///
/// Returns JSON `{"missing":[…],"orphans":[…],"healthy":bool,"summary":"…"}`
/// or `"error:<message>"`. Caller must free.
#[no_mangle]
pub extern "C" fn wdmm_sync_audit_manga(
    account_id: *const c_char,
    remote_dir: *const c_char,
) -> FfiString {
    let account_id_str = match ptr_to_str(account_id) {
        Some(s) => s,
        None => return FfiString::from("error:invalid account_id"),
    };
    let dir = ptr_to_str(remote_dir).unwrap_or("/wdmm_sync");
    let driver = match driver_for_account(account_id_str) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let db = std::sync::Arc::clone(global_library_db());
    let engine = wdmm_sync::MangaLibrarySyncEngine::new(db, driver, dir, None);
    match rt().block_on(engine.audit()) {
        Ok(r) => FfiString::from(serde_json::json!({
            "missing": r.missing,
            "orphans": r.orphans,
            "healthy": r.healthy,
            "summary": r.summary,
        }).to_string()),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Estimate the network cost of a full music library rebuild.
///
/// `tracks_per_shard`: target rows per shard; `0` → default (500).
///
/// Returns JSON `{"shards":N,"estimated_bytes":N,"fragment_count":N,
/// "fragment_bytes":N}` or `"error:<message>"`. Caller must free.
#[no_mangle]
pub extern "C" fn wdmm_sync_estimate_music(
    account_id: *const c_char,
    remote_dir: *const c_char,
    tracks_per_shard: c_int,
) -> FfiString {
    let account_id_str = match ptr_to_str(account_id) {
        Some(s) => s,
        None => return FfiString::from("error:invalid account_id"),
    };
    let dir = ptr_to_str(remote_dir).unwrap_or("/wdmm_sync");
    let n = if tracks_per_shard > 0 { tracks_per_shard as usize } else { 500 };
    let driver = match driver_for_account(account_id_str) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let db = std::sync::Arc::clone(global_library_db());
    let engine = wdmm_sync::LibrarySyncEngine::new(db, driver, dir, None);
    match rt().block_on(engine.estimate(n)) {
        Ok(r) => FfiString::from(serde_json::json!({
            "shards": r.shards,
            "estimated_bytes": r.estimated_bytes,
            "fragment_count": r.fragment_count,
            "fragment_bytes": r.fragment_bytes,
        }).to_string()),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Estimate the network cost of a full manga library rebuild.
///
/// `mangas_per_shard`: target rows per shard; `0` → default (200).
///
/// Returns JSON `{"shards":N,"estimated_bytes":N,"fragment_count":N,
/// "fragment_bytes":N}` or `"error:<message>"`. Caller must free.
#[no_mangle]
pub extern "C" fn wdmm_sync_estimate_manga(
    account_id: *const c_char,
    remote_dir: *const c_char,
    mangas_per_shard: c_int,
) -> FfiString {
    let account_id_str = match ptr_to_str(account_id) {
        Some(s) => s,
        None => return FfiString::from("error:invalid account_id"),
    };
    let dir = ptr_to_str(remote_dir).unwrap_or("/wdmm_sync");
    let n = if mangas_per_shard > 0 { mangas_per_shard as usize } else { 200 };
    let driver = match driver_for_account(account_id_str) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let db = std::sync::Arc::clone(global_library_db());
    let engine = wdmm_sync::MangaLibrarySyncEngine::new(db, driver, dir, None);
    match rt().block_on(engine.estimate(n)) {
        Ok(r) => FfiString::from(serde_json::json!({
            "shards": r.shards,
            "estimated_bytes": r.estimated_bytes,
            "fragment_count": r.fragment_count,
            "fragment_bytes": r.fragment_bytes,
        }).to_string()),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Delete orphan shards from the music library remote.
///
/// `orphans_json`: JSON array of filenames from `wdmm_sync_audit_music`.
/// Returns the count of deleted files as a decimal string, or
/// `"error:<message>"`. Caller must free.
#[no_mangle]
pub extern "C" fn wdmm_sync_tidy_orphans_music(
    account_id: *const c_char,
    remote_dir: *const c_char,
    orphans_json: *const c_char,
) -> FfiString {
    let account_id_str = match ptr_to_str(account_id) {
        Some(s) => s,
        None => return FfiString::from("error:invalid account_id"),
    };
    let dir = ptr_to_str(remote_dir).unwrap_or("/wdmm_sync");
    let raw = match ptr_to_str(orphans_json) {
        Some(s) => s,
        None => return FfiString::from("error:null orphans_json"),
    };
    let orphans: Vec<String> = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => return FfiString::from(format!("error:json parse: {e}")),
    };
    let driver = match driver_for_account(account_id_str) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let db = std::sync::Arc::clone(global_library_db());
    let engine = wdmm_sync::LibrarySyncEngine::new(db, driver, dir, None);
    match rt().block_on(engine.tidy_orphans(&orphans)) {
        Ok(n) => FfiString::from(n.to_string()),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Delete orphan shards from the manga library remote.
///
/// `orphans_json`: JSON array of filenames from `wdmm_sync_audit_manga`.
/// Returns the count of deleted files as a decimal string, or
/// `"error:<message>"`. Caller must free.
#[no_mangle]
pub extern "C" fn wdmm_sync_tidy_orphans_manga(
    account_id: *const c_char,
    remote_dir: *const c_char,
    orphans_json: *const c_char,
) -> FfiString {
    let account_id_str = match ptr_to_str(account_id) {
        Some(s) => s,
        None => return FfiString::from("error:invalid account_id"),
    };
    let dir = ptr_to_str(remote_dir).unwrap_or("/wdmm_sync");
    let raw = match ptr_to_str(orphans_json) {
        Some(s) => s,
        None => return FfiString::from("error:null orphans_json"),
    };
    let orphans: Vec<String> = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => return FfiString::from(format!("error:json parse: {e}")),
    };
    let driver = match driver_for_account(account_id_str) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let db = std::sync::Arc::clone(global_library_db());
    let engine = wdmm_sync::MangaLibrarySyncEngine::new(db, driver, dir, None);
    match rt().block_on(engine.tidy_orphans(&orphans)) {
        Ok(n) => FfiString::from(n.to_string()),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}




// ── WebDAV browse FFI ────────────────────────────────────────────────────────

/// List a WebDAV directory via [`wdmm_drivers::WebDavDriver::list`].
///
/// Parameters (all null-terminated UTF-8 C strings):
/// - `webdav_url`:  WebDAV base URL (e.g. `"https://nas.local/dav/"`)
/// - `webdav_user`: HTTP Basic username (empty string for no auth)
/// - `webdav_pass`: HTTP Basic password (empty string for no auth)
/// - `path`:        Remote directory to list (e.g. `"/Music"`; default `"/"`)
///
/// Returns a JSON array on success:
/// ```json
/// [{"name":"Song.mp3","path":"/Music/Song.mp3","is_dir":false,"size":1234,"modified":"2024-01-02T03:04:05Z"}]
/// ```
/// or `"error:<message>"`. Caller must free the result.
///
/// # Safety
/// All pointer parameters must be valid null-terminated UTF-8 C strings.
#[no_mangle]
pub extern "C" fn wdmm_webdav_list(
    webdav_url: *const c_char,
    webdav_user: *const c_char,
    webdav_pass: *const c_char,
    path: *const c_char,
) -> FfiString {
    let url = match ptr_to_str(webdav_url) {
        Some(s) => s,
        None => return FfiString::from("error:invalid webdav_url"),
    };
    let user = ptr_to_str(webdav_user).unwrap_or("");
    let pass = ptr_to_str(webdav_pass).unwrap_or("");
    let path = ptr_to_str(path).unwrap_or("/");

    let driver = match wdmm_drivers::WebDavDriver::new(url, user, pass) {
        Ok(d) => d,
        Err(e) => return FfiString::from(format!("error:driver: {e}")),
    };

    let items = match rt().block_on(wdmm_drivers::WdmmVfs::list(&driver, path)) {
        Ok(v) => v,
        Err(e) => return FfiString::from(format!("error:{e}")),
    };

    let parent = path.trim_end_matches('/');
    let parent = if parent.is_empty() { "" } else { parent };

    let arr: Vec<serde_json::Value> = items
        .into_iter()
        .map(|it| {
            let joined = if parent.is_empty() {
                format!("/{}", it.name.trim_start_matches('/'))
            } else {
                format!("{}/{}", parent, it.name.trim_start_matches('/'))
            };
            serde_json::json!({
                "name": it.name,
                "path": joined,
                "is_dir": it.is_dir,
                "size": it.size,
                "modified": it.modified.map(|t| t.to_rfc3339()),
            })
        })
        .collect();

    match serde_json::to_string(&arr) {
        Ok(s) => FfiString::from(s),
        Err(e) => FfiString::from(format!("error:serde: {e}")),
    }
}


/// Create a WebDAV collection (MKCOL). Returns `"ok"` or `"error:…"`.
#[no_mangle]
pub extern "C" fn wdmm_webdav_mkdir(
    webdav_url: *const c_char,
    webdav_user: *const c_char,
    webdav_pass: *const c_char,
    path: *const c_char,
) -> FfiString {
    let url = match ptr_to_str(webdav_url) {
        Some(s) => s,
        None => return FfiString::from("error:invalid webdav_url"),
    };
    let user = ptr_to_str(webdav_user).unwrap_or("");
    let pass = ptr_to_str(webdav_pass).unwrap_or("");
    let path = match ptr_to_str(path) {
        Some(s) if !s.is_empty() => s,
        _ => return FfiString::from("error:invalid path"),
    };
    let driver = match wdmm_drivers::WebDavDriver::new(url, user, pass) {
        Ok(d) => d,
        Err(e) => return FfiString::from(format!("error:driver: {e}")),
    };
    match rt().block_on(wdmm_drivers::WdmmVfs::mkdir(&driver, path)) {
        Ok(()) => FfiString::from("ok"),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Rename/move a WebDAV path. Returns `"ok"` or `"error:…"`.
#[no_mangle]
pub extern "C" fn wdmm_webdav_rename(
    webdav_url: *const c_char,
    webdav_user: *const c_char,
    webdav_pass: *const c_char,
    old_path: *const c_char,
    new_path: *const c_char,
) -> FfiString {
    let url = match ptr_to_str(webdav_url) {
        Some(s) => s,
        None => return FfiString::from("error:invalid webdav_url"),
    };
    let user = ptr_to_str(webdav_user).unwrap_or("");
    let pass = ptr_to_str(webdav_pass).unwrap_or("");
    let old = match ptr_to_str(old_path) {
        Some(s) if !s.is_empty() => s,
        _ => return FfiString::from("error:invalid old_path"),
    };
    let newp = match ptr_to_str(new_path) {
        Some(s) if !s.is_empty() => s,
        _ => return FfiString::from("error:invalid new_path"),
    };
    let driver = match wdmm_drivers::WebDavDriver::new(url, user, pass) {
        Ok(d) => d,
        Err(e) => return FfiString::from(format!("error:driver: {e}")),
    };
    match rt().block_on(wdmm_drivers::WdmmVfs::rename(&driver, old, newp)) {
        Ok(()) => FfiString::from("ok"),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Delete a WebDAV file or directory. Returns `"ok"` or `"error:…"`.
#[no_mangle]
pub extern "C" fn wdmm_webdav_remove(
    webdav_url: *const c_char,
    webdav_user: *const c_char,
    webdav_pass: *const c_char,
    path: *const c_char,
) -> FfiString {
    let url = match ptr_to_str(webdav_url) {
        Some(s) => s,
        None => return FfiString::from("error:invalid webdav_url"),
    };
    let user = ptr_to_str(webdav_user).unwrap_or("");
    let pass = ptr_to_str(webdav_pass).unwrap_or("");
    let path = match ptr_to_str(path) {
        Some(s) if !s.is_empty() => s,
        _ => return FfiString::from("error:invalid path"),
    };
    let driver = match wdmm_drivers::WebDavDriver::new(url, user, pass) {
        Ok(d) => d,
        Err(e) => return FfiString::from(format!("error:driver: {e}")),
    };
    match rt().block_on(wdmm_drivers::WdmmVfs::remove(&driver, path)) {
        Ok(()) => FfiString::from("ok"),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

// ── Generic Driver FFI ────────────────────────────────────────────────────────

/// List directory entries via DriverRegistry.
///
/// Returns JSON array matching `wdmm_webdav_list` shape:
/// ```json
/// [{"name":"file.mp3","path":"/path/file.mp3","is_dir":false,"size":1234,"modified":"2024-01-02T03:04:05Z"}]
/// ```
/// or `"error:<message>"`.
#[no_mangle]
pub extern "C" fn wdmm_driver_list(
    provider_type: *const c_char,
    config_json: *const c_char,
    path: *const c_char,
) -> FfiString {
    let provider = match ptr_to_str(provider_type) {
        Some(s) => s,
        None => return FfiString::from("error:invalid provider_type"),
    };
    let config_raw = match ptr_to_str(config_json) {
        Some(s) => s,
        None => return FfiString::from("error:invalid config_json"),
    };
    let config: serde_json::Value = match serde_json::from_str(config_raw) {
        Ok(v) => v,
        Err(e) => return FfiString::from(format!("error:config parse: {e}")),
    };
    let path = ptr_to_str(path).unwrap_or("/");

    let driver = match wdmm_drivers::DriverRegistry::create_driver(provider, config) {
        Ok(d) => d,
        Err(e) => return FfiString::from(format!("error:driver: {e}")),
    };

    let items = match rt().block_on(wdmm_drivers::WdmmVfs::list(&*driver, path)) {
        Ok(v) => v,
        Err(e) => return FfiString::from(format!("error:{e}")),
    };

    let parent = path.trim_end_matches('/');
    let parent = if parent.is_empty() { "" } else { parent };

    let arr: Vec<serde_json::Value> = items
        .into_iter()
        .map(|it| {
            let joined = if parent.is_empty() {
                format!("/{}", it.name.trim_start_matches('/'))
            } else {
                format!("{}/{}", parent, it.name.trim_start_matches('/'))
            };
            serde_json::json!({
                "name": it.name,
                "path": joined,
                "is_dir": it.is_dir,
                "size": it.size,
                "modified": it.modified.map(|t| t.to_rfc3339()),
            })
        })
        .collect();

    match serde_json::to_string(&arr) {
        Ok(s) => FfiString::from(s),
        Err(e) => FfiString::from(format!("error:serde: {e}")),
    }
}

/// Create a directory via DriverRegistry. Returns `"ok"` or `"error:…"`.
#[no_mangle]
pub extern "C" fn wdmm_driver_mkdir(
    provider_type: *const c_char,
    config_json: *const c_char,
    path: *const c_char,
) -> FfiString {
    let provider = match ptr_to_str(provider_type) {
        Some(s) => s,
        None => return FfiString::from("error:invalid provider_type"),
    };
    let config_raw = match ptr_to_str(config_json) {
        Some(s) => s,
        None => return FfiString::from("error:invalid config_json"),
    };
    let config: serde_json::Value = match serde_json::from_str(config_raw) {
        Ok(v) => v,
        Err(e) => return FfiString::from(format!("error:config parse: {e}")),
    };
    let path = match ptr_to_str(path) {
        Some(s) if !s.is_empty() => s,
        _ => return FfiString::from("error:invalid path"),
    };

    let driver = match wdmm_drivers::DriverRegistry::create_driver(provider, config) {
        Ok(d) => d,
        Err(e) => return FfiString::from(format!("error:driver: {e}")),
    };

    match rt().block_on(wdmm_drivers::WdmmVfs::mkdir(&*driver, path)) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Rename/move via DriverRegistry. Returns `"ok"` or `"error:…"`.
#[no_mangle]
pub extern "C" fn wdmm_driver_rename(
    provider_type: *const c_char,
    config_json: *const c_char,
    old_path: *const c_char,
    new_path: *const c_char,
) -> FfiString {
    let provider = match ptr_to_str(provider_type) {
        Some(s) => s,
        None => return FfiString::from("error:invalid provider_type"),
    };
    let config_raw = match ptr_to_str(config_json) {
        Some(s) => s,
        None => return FfiString::from("error:invalid config_json"),
    };
    let config: serde_json::Value = match serde_json::from_str(config_raw) {
        Ok(v) => v,
        Err(e) => return FfiString::from(format!("error:config parse: {e}")),
    };
    let old_path = match ptr_to_str(old_path) {
        Some(s) if !s.is_empty() => s,
        _ => return FfiString::from("error:invalid old_path"),
    };
    let new_path = match ptr_to_str(new_path) {
        Some(s) if !s.is_empty() => s,
        _ => return FfiString::from("error:invalid new_path"),
    };

    let driver = match wdmm_drivers::DriverRegistry::create_driver(provider, config) {
        Ok(d) => d,
        Err(e) => return FfiString::from(format!("error:driver: {e}")),
    };

    match rt().block_on(wdmm_drivers::WdmmVfs::rename(&*driver, old_path, new_path)) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Remove a file/directory via DriverRegistry. Returns `"ok"` or `"error:…"`.
#[no_mangle]
pub extern "C" fn wdmm_driver_remove(
    provider_type: *const c_char,
    config_json: *const c_char,
    path: *const c_char,
) -> FfiString {
    let provider = match ptr_to_str(provider_type) {
        Some(s) => s,
        None => return FfiString::from("error:invalid provider_type"),
    };
    let config_raw = match ptr_to_str(config_json) {
        Some(s) => s,
        None => return FfiString::from("error:invalid config_json"),
    };
    let config: serde_json::Value = match serde_json::from_str(config_raw) {
        Ok(v) => v,
        Err(e) => return FfiString::from(format!("error:config parse: {e}")),
    };
    let path = match ptr_to_str(path) {
        Some(s) if !s.is_empty() => s,
        _ => return FfiString::from("error:invalid path"),
    };

    let driver = match wdmm_drivers::DriverRegistry::create_driver(provider, config) {
        Ok(d) => d,
        Err(e) => return FfiString::from(format!("error:driver: {e}")),
    };

    match rt().block_on(wdmm_drivers::WdmmVfs::remove(&*driver, path)) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Get file/directory metadata via DriverRegistry.
///
/// Returns JSON with `CloudFileItem` fields:
/// ```json
/// {"name":"file.mp3","is_dir":false,"size":1234,"modified":"2024-01-02T03:04:05Z",
///  "delivery":"NetworkDirect","raw_url":"https://…","raw_headers":[["Authorization","Bearer …"]]}
/// ```
/// or `"error:<message>"`.
#[no_mangle]
pub extern "C" fn wdmm_driver_get(
    provider_type: *const c_char,
    config_json: *const c_char,
    path: *const c_char,
) -> FfiString {
    let provider = match ptr_to_str(provider_type) {
        Some(s) => s,
        None => return FfiString::from("error:invalid provider_type"),
    };
    let config_raw = match ptr_to_str(config_json) {
        Some(s) => s,
        None => return FfiString::from("error:invalid config_json"),
    };
    let config: serde_json::Value = match serde_json::from_str(config_raw) {
        Ok(v) => v,
        Err(e) => return FfiString::from(format!("error:config parse: {e}")),
    };
    let path = match ptr_to_str(path) {
        Some(s) if !s.is_empty() => s,
        _ => return FfiString::from("error:invalid path"),
    };

    let driver = match wdmm_drivers::DriverRegistry::create_driver(provider, config) {
        Ok(d) => d,
        Err(e) => return FfiString::from(format!("error:driver: {e}")),
    };

    let item = match rt().block_on(wdmm_drivers::WdmmVfs::get(&*driver, path)) {
        Ok(i) => i,
        Err(e) => return FfiString::from(format!("error:{e}")),
    };

    let delivery_str = match item.delivery {
        wdmm_drivers::ContentDeliveryKind::NetworkDirect => "NetworkDirect",
        wdmm_drivers::ContentDeliveryKind::LocalStream => "LocalStream",
        wdmm_drivers::ContentDeliveryKind::LocalFile => "LocalFile",
        _ => "Unknown",
    };

    let obj = serde_json::json!({
        "name": item.name,
        "is_dir": item.is_dir,
        "size": item.size,
        "modified": item.modified.map(|t| t.to_rfc3339()),
        "delivery": delivery_str,
        "raw_url": item.raw_url,
        "raw_headers": item.raw_headers,
        "local_path": item.local_path.as_ref().map(|p| p.display().to_string()),
    });

    match serde_json::to_string(&obj) {
        Ok(s) => FfiString::from(s),
        Err(e) => FfiString::from(format!("error:serde: {e}")),
    }
}

// ── Account-based FFI ─────────────────────────────────────────────────────────

/// List directory entries for an account by ID.
///
/// Returns JSON array matching `wdmm_webdav_list` shape or `"error:<message>"`.
#[no_mangle]
pub extern "C" fn wdmm_account_list_dir(
    account_id: *const c_char,
    path: *const c_char,
) -> FfiString {
    let account_id_str = match ptr_to_str(account_id) {
        Some(s) => s,
        None => return FfiString::from("error:invalid account_id"),
    };
    let path = ptr_to_str(path).unwrap_or("/");

    let mut account = match global_library_db().get_account(account_id_str) {
        Ok(Some(a)) => a,
        Ok(None) => return FfiString::from("error:account not found"),
        Err(e) => return FfiString::from(format!("error:{e}")),
    };

    let config = match build_driver_config(&account) {
        Ok(c) => c,
        Err(e) => return FfiString::from(format!("error:{e}")),
    };

    let driver = match wdmm_drivers::DriverRegistry::create_driver(&account.provider_type, config) {
        Ok(d) => d,
        Err(e) => return FfiString::from(format!("error:driver: {e}")),
    };

    let items = match rt().block_on(wdmm_drivers::WdmmVfs::list(&*driver, path)) {
        Ok(v) => v,
        Err(wdmm_drivers::DriverError::AuthFailed) => {
            // Token expired: refresh, persist new tokens to Core vault, retry once.
            let new_driver = match rt().block_on(refresh_and_rebuild(&mut account)) {
                Ok(d) => d,
                Err(_) => return FfiString::from("error:auth_failed"),
            };
            match rt().block_on(wdmm_drivers::WdmmVfs::list(&*new_driver, path)) {
                Ok(v) => v,
                Err(e) => return FfiString::from(format!("error:{e}")),
            }
        }
        Err(e) => return FfiString::from(format!("error:{e}")),
    };

    let parent = path.trim_end_matches('/');
    let parent = if parent.is_empty() { "" } else { parent };

    let arr: Vec<serde_json::Value> = items
        .into_iter()
        .map(|it| {
            let joined = if parent.is_empty() {
                format!("/{}", it.name.trim_start_matches('/'))
            } else {
                format!("{}/{}", parent, it.name.trim_start_matches('/'))
            };
            serde_json::json!({
                "name": it.name,
                "path": joined,
                "is_dir": it.is_dir,
                "size": it.size,
                "modified": it.modified.map(|t| t.to_rfc3339()),
            })
        })
        .collect();

    match serde_json::to_string(&arr) {
        Ok(s) => FfiString::from(s),
        Err(e) => FfiString::from(format!("error:serde: {e}")),
    }
}

/// Create a directory for an account. Returns `"ok"` or `"error:…"`.
#[no_mangle]
pub extern "C" fn wdmm_account_mkdir(
    account_id: *const c_char,
    path: *const c_char,
) -> FfiString {
    let account_id_str = match ptr_to_str(account_id) {
        Some(s) => s,
        None => return FfiString::from("error:invalid account_id"),
    };
    let path = match ptr_to_str(path) {
        Some(s) if !s.is_empty() => s,
        _ => return FfiString::from("error:invalid path"),
    };

    let mut account = match global_library_db().get_account(account_id_str) {
        Ok(Some(a)) => a,
        Ok(None) => return FfiString::from("error:account not found"),
        Err(e) => return FfiString::from(format!("error:{e}")),
    };

    let config = match build_driver_config(&account) {
        Ok(c) => c,
        Err(e) => return FfiString::from(format!("error:{e}")),
    };

    let driver = match wdmm_drivers::DriverRegistry::create_driver(&account.provider_type, config) {
        Ok(d) => d,
        Err(e) => return FfiString::from(format!("error:driver: {e}")),
    };

    match rt().block_on(wdmm_drivers::WdmmVfs::mkdir(&*driver, path)) {
        Ok(()) => FfiString::ok(),
        Err(wdmm_drivers::DriverError::AuthFailed) => {
            let new_driver = match rt().block_on(refresh_and_rebuild(&mut account)) {
                Ok(d) => d,
                Err(_) => return FfiString::from("error:auth_failed"),
            };
            match rt().block_on(wdmm_drivers::WdmmVfs::mkdir(&*new_driver, path)) {
                Ok(()) => FfiString::ok(),
                Err(e) => FfiString::from(format!("error:{e}")),
            }
        }
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Rename/move for an account. Returns `"ok"` or `"error:…"`.
#[no_mangle]
pub extern "C" fn wdmm_account_rename(
    account_id: *const c_char,
    old_path: *const c_char,
    new_path: *const c_char,
) -> FfiString {
    let account_id_str = match ptr_to_str(account_id) {
        Some(s) => s,
        None => return FfiString::from("error:invalid account_id"),
    };
    let old_path = match ptr_to_str(old_path) {
        Some(s) if !s.is_empty() => s,
        _ => return FfiString::from("error:invalid old_path"),
    };
    let new_path = match ptr_to_str(new_path) {
        Some(s) if !s.is_empty() => s,
        _ => return FfiString::from("error:invalid new_path"),
    };

    let mut account = match global_library_db().get_account(account_id_str) {
        Ok(Some(a)) => a,
        Ok(None) => return FfiString::from("error:account not found"),
        Err(e) => return FfiString::from(format!("error:{e}")),
    };

    let config = match build_driver_config(&account) {
        Ok(c) => c,
        Err(e) => return FfiString::from(format!("error:{e}")),
    };

    let driver = match wdmm_drivers::DriverRegistry::create_driver(&account.provider_type, config) {
        Ok(d) => d,
        Err(e) => return FfiString::from(format!("error:driver: {e}")),
    };

    match rt().block_on(wdmm_drivers::WdmmVfs::rename(&*driver, old_path, new_path)) {
        Ok(()) => FfiString::ok(),
        Err(wdmm_drivers::DriverError::AuthFailed) => {
            let new_driver = match rt().block_on(refresh_and_rebuild(&mut account)) {
                Ok(d) => d,
                Err(_) => return FfiString::from("error:auth_failed"),
            };
            match rt().block_on(wdmm_drivers::WdmmVfs::rename(&*new_driver, old_path, new_path)) {
                Ok(()) => FfiString::ok(),
                Err(e) => FfiString::from(format!("error:{e}")),
            }
        }
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Remove a file/directory for an account. Returns `"ok"` or `"error:…"`.
#[no_mangle]
pub extern "C" fn wdmm_account_remove(
    account_id: *const c_char,
    path: *const c_char,
) -> FfiString {
    let account_id_str = match ptr_to_str(account_id) {
        Some(s) => s,
        None => return FfiString::from("error:invalid account_id"),
    };
    let path = match ptr_to_str(path) {
        Some(s) if !s.is_empty() => s,
        _ => return FfiString::from("error:invalid path"),
    };

    let mut account = match global_library_db().get_account(account_id_str) {
        Ok(Some(a)) => a,
        Ok(None) => return FfiString::from("error:account not found"),
        Err(e) => return FfiString::from(format!("error:{e}")),
    };

    let config = match build_driver_config(&account) {
        Ok(c) => c,
        Err(e) => return FfiString::from(format!("error:{e}")),
    };

    let driver = match wdmm_drivers::DriverRegistry::create_driver(&account.provider_type, config) {
        Ok(d) => d,
        Err(e) => return FfiString::from(format!("error:driver: {e}")),
    };

    match rt().block_on(wdmm_drivers::WdmmVfs::remove(&*driver, path)) {
        Ok(()) => FfiString::ok(),
        Err(wdmm_drivers::DriverError::AuthFailed) => {
            let new_driver = match rt().block_on(refresh_and_rebuild(&mut account)) {
                Ok(d) => d,
                Err(_) => return FfiString::from("error:auth_failed"),
            };
            match rt().block_on(wdmm_drivers::WdmmVfs::remove(&*new_driver, path)) {
                Ok(()) => FfiString::ok(),
                Err(e) => FfiString::from(format!("error:{e}")),
            }
        }
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Get file/directory metadata for an account.
///
/// Returns JSON with `CloudFileItem` fields or `"error:<message>"`.
#[no_mangle]
pub extern "C" fn wdmm_account_get_entry(
    account_id: *const c_char,
    path: *const c_char,
) -> FfiString {
    let account_id_str = match ptr_to_str(account_id) {
        Some(s) => s,
        None => return FfiString::from("error:invalid account_id"),
    };
    let path = match ptr_to_str(path) {
        Some(s) if !s.is_empty() => s,
        _ => return FfiString::from("error:invalid path"),
    };

    let mut account = match global_library_db().get_account(account_id_str) {
        Ok(Some(a)) => a,
        Ok(None) => return FfiString::from("error:account not found"),
        Err(e) => return FfiString::from(format!("error:{e}")),
    };

    let config = match build_driver_config(&account) {
        Ok(c) => c,
        Err(e) => return FfiString::from(format!("error:{e}")),
    };

    let driver = match wdmm_drivers::DriverRegistry::create_driver(&account.provider_type, config) {
        Ok(d) => d,
        Err(e) => return FfiString::from(format!("error:driver: {e}")),
    };

    let item = match rt().block_on(wdmm_drivers::WdmmVfs::get(&*driver, path)) {
        Ok(i) => i,
        Err(wdmm_drivers::DriverError::AuthFailed) => {
            let new_driver = match rt().block_on(refresh_and_rebuild(&mut account)) {
                Ok(d) => d,
                Err(_) => return FfiString::from("error:auth_failed"),
            };
            match rt().block_on(wdmm_drivers::WdmmVfs::get(&*new_driver, path)) {
                Ok(i) => i,
                Err(e) => return FfiString::from(format!("error:{e}")),
            }
        }
        Err(e) => return FfiString::from(format!("error:{e}")),
    };

    let delivery_str = match item.delivery {
        wdmm_drivers::ContentDeliveryKind::NetworkDirect => "NetworkDirect",
        wdmm_drivers::ContentDeliveryKind::LocalStream => "LocalStream",
        wdmm_drivers::ContentDeliveryKind::LocalFile => "LocalFile",
        _ => "Unknown",
    };

    let obj = serde_json::json!({
        "name": item.name,
        "is_dir": item.is_dir,
        "size": item.size,
        "modified": item.modified.map(|t| t.to_rfc3339()),
        "delivery": delivery_str,
        "raw_url": item.raw_url,
        "raw_headers": item.raw_headers,
        "local_path": item.local_path.as_ref().map(|p| p.display().to_string()),
    });

    match serde_json::to_string(&obj) {
        Ok(s) => FfiString::from(s),
        Err(e) => FfiString::from(format!("error:serde: {e}")),
    }
}

/// Write bytes to a remote file for an account. `data_base64` is the file
/// content as standard base64 (no line breaks). Returns `"ok"` or `"error:…"`.
///
/// Mirrors the Dart `WdmmVfs.put_content` path. Used by sync engines to push
/// small JSON/metadata files — not the user-upload path (see drivers guide §8).
#[no_mangle]
pub extern "C" fn wdmm_account_put_content(
    account_id: *const c_char,
    path: *const c_char,
    data_base64: *const c_char,
) -> FfiString {
    let account_id_str = match ptr_to_str(account_id) {
        Some(s) => s,
        None => return FfiString::from("error:invalid account_id"),
    };
    let path = match ptr_to_str(path) {
        Some(s) if !s.is_empty() => s,
        _ => return FfiString::from("error:invalid path"),
    };
    let data_b64 = match ptr_to_str(data_base64) {
        Some(s) => s,
        None => return FfiString::from("error:invalid data_base64"),
    };

    let data = match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data_b64) {
        Ok(d) => d,
        Err(e) => return FfiString::from(format!("error:base64 decode: {e}")),
    };

    let mut account = match global_library_db().get_account(account_id_str) {
        Ok(Some(a)) => a,
        Ok(None) => return FfiString::from("error:account not found"),
        Err(e) => return FfiString::from(format!("error:{e}")),
    };
    let config = match build_driver_config(&account) {
        Ok(c) => c,
        Err(e) => return FfiString::from(format!("error:{e}")),
    };
    let driver = match wdmm_drivers::DriverRegistry::create_driver(&account.provider_type, config) {
        Ok(d) => d,
        Err(e) => return FfiString::from(format!("error:driver: {e}")),
    };

    match rt().block_on(wdmm_drivers::WdmmVfs::put_content(&*driver, path, &data)) {
        Ok(()) => FfiString::from("ok"),
        Err(wdmm_drivers::DriverError::AuthFailed) => {
            let new_driver = match rt().block_on(refresh_and_rebuild(&mut account)) {
                Ok(d) => d,
                Err(_) => return FfiString::from("error:auth_failed"),
            };
            match rt().block_on(wdmm_drivers::WdmmVfs::put_content(&*new_driver, path, &data)) {
                Ok(()) => FfiString::from("ok"),
                Err(e) => FfiString::from(format!("error:{e}")),
            }
        }
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Fetch the full raw bytes of a remote file. Returns a JSON object
/// `{"data":"<base64>"}` on success, or `"error:…"`.
#[no_mangle]
pub extern "C" fn wdmm_account_get_content(
    account_id: *const c_char,
    path: *const c_char,
) -> FfiString {
    let account_id_str = match ptr_to_str(account_id) {
        Some(s) => s,
        None => return FfiString::from("error:invalid account_id"),
    };
    let path = match ptr_to_str(path) {
        Some(s) if !s.is_empty() => s,
        _ => return FfiString::from("error:invalid path"),
    };

    let mut account = match global_library_db().get_account(account_id_str) {
        Ok(Some(a)) => a,
        Ok(None) => return FfiString::from("error:account not found"),
        Err(e) => return FfiString::from(format!("error:{e}")),
    };
    let config = match build_driver_config(&account) {
        Ok(c) => c,
        Err(e) => return FfiString::from(format!("error:{e}")),
    };
    let driver = match wdmm_drivers::DriverRegistry::create_driver(&account.provider_type, config) {
        Ok(d) => d,
        Err(e) => return FfiString::from(format!("error:driver: {e}")),
    };

    let data = match rt().block_on(wdmm_drivers::WdmmVfs::get_content(&*driver, path)) {
        Ok(d) => d,
        Err(wdmm_drivers::DriverError::AuthFailed) => {
            let new_driver = match rt().block_on(refresh_and_rebuild(&mut account)) {
                Ok(d) => d,
                Err(_) => return FfiString::from("error:auth_failed"),
            };
            match rt().block_on(wdmm_drivers::WdmmVfs::get_content(&*new_driver, path)) {
                Ok(d) => d,
                Err(e) => return FfiString::from(format!("error:{e}")),
            }
        }
        Err(e) => return FfiString::from(format!("error:{e}")),
    };
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &data);
    match serde_json::to_string(&serde_json::json!({ "data": b64 })) {
        Ok(s) => FfiString::from(s),
        Err(e) => FfiString::from(format!("error:serde: {e}")),
    }
}

/// Fetch a byte range `[start, end_inclusive]` from a remote file. `end_str`
/// may be the decimal string of `u64::MAX` to mean "to end of file". Returns a
/// JSON object `{"data":"<base64>"}` on success, or `"error:…"`.
#[no_mangle]
pub extern "C" fn wdmm_account_get_content_range(
    account_id: *const c_char,
    path: *const c_char,
    start_str: *const c_char,
    end_str: *const c_char,
) -> FfiString {
    let account_id_str = match ptr_to_str(account_id) {
        Some(s) => s,
        None => return FfiString::from("error:invalid account_id"),
    };
    let path = match ptr_to_str(path) {
        Some(s) if !s.is_empty() => s,
        _ => return FfiString::from("error:invalid path"),
    };
    let start: u64 = match ptr_to_str(start_str).and_then(|s| s.parse().ok()) {
        Some(v) => v,
        None => return FfiString::from("error:invalid start"),
    };
    let end_inclusive: u64 = match ptr_to_str(end_str).and_then(|s| s.parse().ok()) {
        Some(v) => v,
        None => return FfiString::from("error:invalid end"),
    };
    let end_opt = if end_inclusive == u64::MAX { None } else { Some(end_inclusive) };

    let mut account = match global_library_db().get_account(account_id_str) {
        Ok(Some(a)) => a,
        Ok(None) => return FfiString::from("error:account not found"),
        Err(e) => return FfiString::from(format!("error:{e}")),
    };
    let config = match build_driver_config(&account) {
        Ok(c) => c,
        Err(e) => return FfiString::from(format!("error:{e}")),
    };
    let driver = match wdmm_drivers::DriverRegistry::create_driver(&account.provider_type, config) {
        Ok(d) => d,
        Err(e) => return FfiString::from(format!("error:driver: {e}")),
    };

    let data = match rt().block_on(wdmm_drivers::WdmmVfs::get_content_range(
        &*driver, path, start, end_opt,
    )) {
        Ok(d) => d,
        Err(wdmm_drivers::DriverError::AuthFailed) => {
            let new_driver = match rt().block_on(refresh_and_rebuild(&mut account)) {
                Ok(d) => d,
                Err(_) => return FfiString::from("error:auth_failed"),
            };
            match rt().block_on(wdmm_drivers::WdmmVfs::get_content_range(
                &*new_driver, path, start, end_opt,
            )) {
                Ok(d) => d,
                Err(e) => return FfiString::from(format!("error:{e}")),
            }
        }
        Err(e) => return FfiString::from(format!("error:{e}")),
    };
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &data);
    match serde_json::to_string(&serde_json::json!({ "data": b64 })) {
        Ok(s) => FfiString::from(s),
        Err(e) => FfiString::from(format!("error:serde: {e}")),
    }
}

/// Server-side copy of `src` to `dst` (both account-absolute paths).
/// Returns `"ok"` or `"error:…"`.
#[no_mangle]
pub extern "C" fn wdmm_account_copy(
    account_id: *const c_char,
    src: *const c_char,
    dst: *const c_char,
) -> FfiString {
    let account_id_str = match ptr_to_str(account_id) {
        Some(s) => s,
        None => return FfiString::from("error:invalid account_id"),
    };
    let src = match ptr_to_str(src) {
        Some(s) if !s.is_empty() => s,
        _ => return FfiString::from("error:invalid src"),
    };
    let dst = match ptr_to_str(dst) {
        Some(s) if !s.is_empty() => s,
        _ => return FfiString::from("error:invalid dst"),
    };

    let mut account = match global_library_db().get_account(account_id_str) {
        Ok(Some(a)) => a,
        Ok(None) => return FfiString::from("error:account not found"),
        Err(e) => return FfiString::from(format!("error:{e}")),
    };
    let config = match build_driver_config(&account) {
        Ok(c) => c,
        Err(e) => return FfiString::from(format!("error:{e}")),
    };
    let driver = match wdmm_drivers::DriverRegistry::create_driver(&account.provider_type, config) {
        Ok(d) => d,
        Err(e) => return FfiString::from(format!("error:driver: {e}")),
    };

    match rt().block_on(wdmm_drivers::WdmmVfs::copy(&*driver, src, dst)) {
        Ok(()) => FfiString::from("ok"),
        Err(wdmm_drivers::DriverError::AuthFailed) => {
            let new_driver = match rt().block_on(refresh_and_rebuild(&mut account)) {
                Ok(d) => d,
                Err(_) => return FfiString::from("error:auth_failed"),
            };
            match rt().block_on(wdmm_drivers::WdmmVfs::copy(&*new_driver, src, dst)) {
                Ok(()) => FfiString::from("ok"),
                Err(e) => FfiString::from(format!("error:{e}")),
            }
        }
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Idempotent directory creation. Returns `"ok"` or `"error:…"`.
#[no_mangle]
pub extern "C" fn wdmm_account_ensure_dir(
    account_id: *const c_char,
    path: *const c_char,
) -> FfiString {
    let account_id_str = match ptr_to_str(account_id) {
        Some(s) => s,
        None => return FfiString::from("error:invalid account_id"),
    };
    let path = match ptr_to_str(path) {
        Some(s) if !s.is_empty() => s,
        _ => return FfiString::from("error:invalid path"),
    };

    let mut account = match global_library_db().get_account(account_id_str) {
        Ok(Some(a)) => a,
        Ok(None) => return FfiString::from("error:account not found"),
        Err(e) => return FfiString::from(format!("error:{e}")),
    };
    let config = match build_driver_config(&account) {
        Ok(c) => c,
        Err(e) => return FfiString::from(format!("error:{e}")),
    };
    let driver = match wdmm_drivers::DriverRegistry::create_driver(&account.provider_type, config) {
        Ok(d) => d,
        Err(e) => return FfiString::from(format!("error:driver: {e}")),
    };

    match rt().block_on(wdmm_drivers::WdmmVfs::ensure_dir(&*driver, path)) {
        Ok(()) => FfiString::from("ok"),
        Err(wdmm_drivers::DriverError::AuthFailed) => {
            let new_driver = match rt().block_on(refresh_and_rebuild(&mut account)) {
                Ok(d) => d,
                Err(_) => return FfiString::from("error:auth_failed"),
            };
            match rt().block_on(wdmm_drivers::WdmmVfs::ensure_dir(&*new_driver, path)) {
                Ok(()) => FfiString::from("ok"),
                Err(e) => FfiString::from(format!("error:{e}")),
            }
        }
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Short status labels for the accounts list (VIP tier, bandwidth, …).
/// Returns a JSON array `[{"key":"…","label":"…"}, …]` on success (empty array
/// when the driver has no status API), or `"error:…"`. Drivers must fail-soft.
#[no_mangle]
pub extern "C" fn wdmm_account_status_tags(account_id: *const c_char) -> FfiString {
    let account_id_str = match ptr_to_str(account_id) {
        Some(s) => s,
        None => return FfiString::from("error:invalid account_id"),
    };

    let mut account = match global_library_db().get_account(account_id_str) {
        Ok(Some(a)) => a,
        Ok(None) => return FfiString::from("error:account not found"),
        Err(e) => return FfiString::from(format!("error:{e}")),
    };
    let config = match build_driver_config(&account) {
        Ok(c) => c,
        Err(e) => return FfiString::from(format!("error:{e}")),
    };
    let driver = match wdmm_drivers::DriverRegistry::create_driver(&account.provider_type, config) {
        Ok(d) => d,
        Err(e) => return FfiString::from(format!("error:driver: {e}")),
    };

    let tags = match rt().block_on(wdmm_drivers::WdmmVfs::status_tags(&*driver)) {
        Ok(t) => t,
        Err(wdmm_drivers::DriverError::AuthFailed) => {
            let new_driver = match rt().block_on(refresh_and_rebuild(&mut account)) {
                Ok(d) => d,
                Err(_) => return FfiString::from("error:auth_failed"),
            };
            match rt().block_on(wdmm_drivers::WdmmVfs::status_tags(&*new_driver)) {
                Ok(t) => t,
                Err(e) => return FfiString::from(format!("error:{e}")),
            }
        }
        Err(e) => return FfiString::from(format!("error:{e}")),
    };
    match serde_json::to_string(&tags) {
        Ok(s) => FfiString::from(s),
        Err(e) => FfiString::from(format!("error:serde: {e}")),
    }
}

/// Read a byte range `[start, end_inclusive]` from a cloud account file.
///
/// `end_inclusive == u64::MAX` means "read to end of file".
///
/// On success returns 0 and writes to `*out_buf` a pointer to a Rust-heap
/// buffer (allocated via [`wdmm_proxy_stream_alloc_buf`], so the caller — or
/// the proxy stream machinery — frees it with [`wdmm_proxy_stream_free_buf`])
/// and to `*out_len` the byte count. Returns -1 on any error, with `*out_buf`
/// left null and `*out_len` 0.
///
/// Used by the Desktop DLNA cloud-cast path: the proxy range callback calls
/// this on a worker thread and forwards the buffer to
/// [`wdmm_proxy_stream_set_result`]. Mirrors Android
/// `DlnaCastService.readRange` / `RustProxyStreamBridge`.
#[no_mangle]
pub unsafe extern "C" fn wdmm_account_read_range(
    account_id: *const c_char,
    path: *const c_char,
    start: u64,
    end_inclusive: u64,
    out_buf: *mut *mut u8,
    out_len: *mut usize,
) -> i32 {
    *out_buf = std::ptr::null_mut();
    *out_len = 0;

    let account_id_str = match ptr_to_str(account_id) {
        Some(s) => s,
        None => return -1,
    };
    let path_str = match ptr_to_str(path) {
        Some(s) if !s.is_empty() => s,
        _ => return -1,
    };

    let mut account = match global_library_db().get_account(account_id_str) {
        Ok(Some(a)) => a,
        Ok(None) => return -1,
        Err(_) => return -1,
    };

    let config = match build_driver_config(&account) {
        Ok(c) => c,
        Err(_) => return -1,
    };

    let driver = match wdmm_drivers::DriverRegistry::create_driver(&account.provider_type, config) {
        Ok(d) => d,
        Err(_) => return -1,
    };

    let end_opt = if end_inclusive == u64::MAX { None } else { Some(end_inclusive) };
    let data = match rt().block_on(wdmm_drivers::WdmmVfs::get_content_range(
        &*driver, path_str, start, end_opt,
    )) {
        Ok(d) => d,
        Err(wdmm_drivers::DriverError::AuthFailed) => {
            // Token expired: refresh, persist new tokens to Core vault, retry once.
            let new_driver = match rt().block_on(refresh_and_rebuild(&mut account)) {
                Ok(d) => d,
                Err(_) => return -1,
            };
            match rt().block_on(wdmm_drivers::WdmmVfs::get_content_range(
                &*new_driver, path_str, start, end_opt,
            )) {
                Ok(d) => d,
                Err(_) => return -1,
            }
        }
        Err(_) => return -1,
    };

    let len = data.len();
    if len == 0 {
        return 0; // empty range is a valid (EOF) result
    }
    let buf = wdmm_proxy_stream_alloc_buf(len);
    if buf.is_null() {
        return -1;
    }
    std::ptr::copy_nonoverlapping(data.as_ptr(), buf, len);
    *out_buf = buf;
    *out_len = len;
    0
}

/// Helper: build driver config JSON from account credentials + base_url.
fn build_driver_config(account: &wdmm_core::models::CloudAccount) -> Result<serde_json::Value, String> {
    // Parse credentials_encrypted as UTF-8 JSON object (plaintext store)
    let creds_str = std::str::from_utf8(&account.credentials_encrypted)
        .map_err(|e| format!("credentials not UTF-8: {e}"))?;
    let creds: serde_json::Value = serde_json::from_str(creds_str)
        .map_err(|e| format!("credentials not valid JSON: {e}"))?;
    build_driver_config_from_creds(account, &creds)
}

/// Build driver config from an already-parsed credentials value. Extracted so
/// the FFI refresh helper can call it after patching tokens without re-reading
/// from the DB.
fn build_driver_config_from_creds(
    account: &wdmm_core::models::CloudAccount,
    creds: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    // WebDAV: build config from base_url + credentials
    if account.provider_type == "webdav" {
        let url = account.base_url.as_deref().unwrap_or("");
        if url.is_empty() {
            return Err("webdav: missing base_url".into());
        }
        let username = creds["username"].as_str().unwrap_or("");
        let password = creds["password"].as_str().unwrap_or("");
        return Ok(serde_json::json!({
            "url": url,
            "username": username,
            "password": password,
        }));
    }

    // Other providers: use credentials as config; inject base_url + account_id
    if !creds.is_object() {
        return Err("credentials must be a JSON object".into());
    }
    let mut config = creds.clone();
    if config.get("url").is_none() && config.get("base_url").is_none() {
        if let Some(ref base_url) = account.base_url {
            config["base_url"] = serde_json::Value::String(base_url.clone());
        }
    }
    // Inject account_id so drivers with refresh (aliyun, baidu) can persist
    // new tokens to the Core vault via update_account_credentials.
    config["account_id"] = serde_json::Value::String(account.id.clone());

    Ok(config)
}

/// On `AuthFailed`, refresh credentials for `account`, persist to Core vault,
/// and return a new driver built from the updated config. Mutates
/// `account.credentials_encrypted` in-place so the caller can rebuild.
///
/// Returns `Err(AuthFailed)` if the driver doesn't support refresh or if the
/// refresh network call itself fails.
async fn refresh_and_rebuild(
    account: &mut wdmm_core::models::CloudAccount,
) -> wdmm_drivers::Result<Box<dyn wdmm_drivers::WdmmVfs>> {
    use wdmm_drivers::DriverError;

    // Build a probe driver from current config to call refresh_credentials.
    let config = build_driver_config(account).map_err(|e| DriverError::Config(e))?;
    let probe = wdmm_drivers::DriverRegistry::create_driver(&account.provider_type, config)
        .map_err(|e| e)?;

    let (new_access, new_refresh) = probe.refresh_credentials().await?;

    // Patch access_token + refresh_token into the in-memory credentials blob.
    let creds_str = std::str::from_utf8(&account.credentials_encrypted)
        .map_err(|_| DriverError::Config("credentials not UTF-8".into()))?;
    let mut creds: serde_json::Value = serde_json::from_str(creds_str)
        .map_err(|_| DriverError::Config("credentials not JSON".into()))?;
    creds["access_token"] = serde_json::Value::String(new_access);
    creds["refresh_token"] = serde_json::Value::String(new_refresh);

    let new_bytes = serde_json::to_vec(&creds)
        .map_err(|_| DriverError::Config("failed to encode credentials".into()))?;

    // Persist to Core vault — best effort (don't abort on DB failure; new tokens
    // are still good for this call).
    let _ = global_library_db().update_account_credentials(&account.id, &new_bytes);

    // Update in-memory account so build_driver_config reads new tokens.
    account.credentials_encrypted = new_bytes;

    // Build fresh driver with refreshed tokens.
    let new_config = build_driver_config(account).map_err(|e| DriverError::Config(e))?;
    wdmm_drivers::DriverRegistry::create_driver(&account.provider_type, new_config)
}

// ── Account verify FFI ────────────────────────────────────────────────────────

/// Verify a (possibly new) cloud account by testing the connection and/or
/// exchanging an OAuth `refresh_token` for runtime credentials.
///
/// # Arguments
/// * `provider_type` — provider key string, e.g. `"aliyundrive"`, `"baidu_netdisk"`.
/// * `config_json`   — form-submitted config as a JSON object.  Must include all
///   fields the provider needs (e.g. `refresh_token` for aliyun/baidu).
///
/// # Return value
/// JSON `{"ok":true,"token_patches":{"access_token":"…","drive_id":"…",…}}` on
/// success, or `"error:<reason>"` on failure. Caller must free the result with
/// `wdmm_ffi_free_string`.
///
/// `token_patches` must be merged into the config that is passed to
/// `wdmm_account_upsert` so the driver can be reconstructed after an app
/// restart (aliyun: access_token + drive_id; baidu: access_token + refresh_token).
#[no_mangle]
pub extern "C" fn wdmm_account_verify(
    provider_type: *const c_char,
    config_json: *const c_char,
) -> FfiString {
    let ptype = match ptr_to_str(provider_type) {
        Some(s) => s,
        None => return FfiString::from("error:missing provider_type"),
    };
    let cfg_str = match ptr_to_str(config_json) {
        Some(s) => s,
        None => return FfiString::from("error:missing config_json"),
    };
    let config: serde_json::Value = match serde_json::from_str(cfg_str) {
        Ok(v) => v,
        Err(e) => return FfiString::from(format!("error:invalid config JSON: {e}")),
    };

    let result: wdmm_drivers::Result<HashMap<String, String>> = (|| {
        use wdmm_drivers::{AliyunConfig, AliyunDriver, BaiduConfig, BaiduDriver, DriverError};

        match ptype {
            // ── Aliyun: exchange refresh_token → access_token + drive_id ──────
            "aliyundrive" | "aliyun" | "aliyundrive_open" => {
                let cfg: AliyunConfig = serde_json::from_value(config.clone())
                    .map_err(|e| DriverError::Config(e.to_string()))?;
                let driver = AliyunDriver::new_for_verify(cfg)?;
                rt().block_on(driver.verify_initial_auth())
            }

            // ── Baidu: exchange refresh_token → access_token ──────────────────
            "baidu" | "baidu_netdisk" => {
                let cfg: BaiduConfig = serde_json::from_value(config.clone())
                    .map_err(|e| DriverError::Config(e.to_string()))?;
                let driver = BaiduDriver::new_for_verify(cfg)?;
                rt().block_on(driver.verify_initial_auth())
            }

            // ── All other providers: connection test via list("/") ─────────────
            _ => {
                // Build driver config inline (no DB account yet).
                let driver_config = if ptype == "webdav" {
                    let url = config["url"].as_str()
                        .or_else(|| config["base_url"].as_str())
                        .unwrap_or("");
                    serde_json::json!({
                        "url": url,
                        "username": config["username"].as_str().unwrap_or(""),
                        "password": config["password"].as_str().unwrap_or(""),
                    })
                } else {
                    config.clone()
                };
                let driver = wdmm_drivers::DriverRegistry::create_driver(ptype, driver_config)?;
                rt().block_on(wdmm_drivers::WdmmVfs::list(&*driver, "/"))
                    .map(|_| HashMap::new())
            }
        }
    })();

    match result {
        Ok(patches) => {
            let json = serde_json::json!({"ok": true, "token_patches": patches});
            FfiString::from(json.to_string())
        }
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

/// Start the local HTTP proxy with the given bind mode and port.
///
/// # Parameters
/// - `bind_mode_cstr`: C string specifying bind mode. Valid values:
///   - `"loopback"` — bind to `127.0.0.1` (localhost only; DLNA devices cannot reach)
///   - `"lan_any"` — bind to `0.0.0.0` (all interfaces; OS routes to appropriate one)
///   - `"lan_if:<ip>"` — bind to specific interface IP (e.g., `"lan_if:192.168.1.100"`)
///   - `"explicit:<addr>"` — bind to exact `SocketAddr` (e.g., `"explicit:192.168.1.100:8080"`)
/// - `port`: port number. Use `0` to let the OS assign a free port (ignored when `bind_mode` is `"explicit:<addr>"`).
///
/// Returns the assigned port as a decimal string, or `"error:<message>"`.
/// Idempotent — if the proxy is already running this is a no-op and returns
/// the existing port. Caller must free the result.
#[no_mangle]
pub extern "C" fn wdmm_proxy_start(bind_mode_cstr: *const c_char, port: u16) -> FfiString {
    use wdmm_proxy::BindMode;

    let bind_mode_str = unsafe {
        if bind_mode_cstr.is_null() {
            return FfiString::from("error:bind_mode is null");
        }
        match std::ffi::CStr::from_ptr(bind_mode_cstr).to_str() {
            Ok(s) => s,
            Err(_) => return FfiString::from("error:bind_mode is not valid UTF-8"),
        }
    };

    let bind_mode = match bind_mode_str {
        "loopback" => BindMode::Loopback,
        "lan_any" => BindMode::LanAny,
        s if s.starts_with("lan_if:") => {
            let ip = &s[7..];
            BindMode::LanIf(ip.to_string())
        }
        s if s.starts_with("explicit:") => {
            let addr_str = &s[9..];
            match addr_str.parse::<std::net::SocketAddr>() {
                Ok(addr) => BindMode::Explicit(addr),
                Err(_) => return FfiString::from(format!("error:invalid explicit address: {}", addr_str)),
            }
        }
        _ => return FfiString::from(format!("error:unknown bind_mode: {}", bind_mode_str)),
    };

    let mut guard = proxy_cell().lock().unwrap();
    // If already running, return the existing port.
    if let Some(ref h) = *guard {
        if let Some(addr) = h.addr() {
            return FfiString::from(addr.port().to_string());
        }
    }
    let handle = ProxyHandle::new(bind_mode, port);
    // Trigger the lazy server start with an ephemeral temp file.
    // We call register_file on a temp path just to ensure the server starts
    // and we can read back the port. The file registration is immediately
    // discarded (unregistered).
    let result = rt().block_on(async {
        let tmp = std::env::temp_dir()
            .join(format!("wdmm-proxy-init-{}.tmp", std::process::id()));
        std::fs::write(&tmp, b"").ok();
        let reg = handle.register_file(&tmp, None, None, None).await;
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
    let reg = rt().block_on(handle.register_file(path, name_opt, None, None));
    match reg {
        Ok(r) => {
            let addr = handle.addr();
            let port = addr.map(|a| a.port()).unwrap_or(0);
            let host = addr
                .map(|a| a.ip().to_string())
                .unwrap_or_else(|| "127.0.0.1".into());
            FfiString::from(
                serde_json::json!({
                    "token":         r.token,
                    "path_template": r.path_template,
                    "host":          host,
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

// ── Transcode preset CRUD ─────────────────────────────────────────────────────

/// Return all transcode presets as a JSON array.
///
/// Returns a JSON array on success, or `"error:<message>"` on failure.
/// Caller must free the result via [`wdmm_ffi_free_string`].
#[no_mangle]
pub extern "C" fn wdmm_transcode_presets_json() -> FfiString {
    let db = global_library_db();
    match db.list_transcode_presets() {
        Ok(presets) => match serde_json::to_string(&presets) {
            Ok(j) => FfiString::from(j),
            Err(e) => FfiString::from(format!("error:{e}")),
        },
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Save (insert or update) a transcode preset.
///
/// `json` must be a null-terminated JSON object matching [`TranscodePreset`].
/// Returns `"ok"` or `"error:<message>"`. Caller must free the result.
///
/// # Safety
/// `json` must be a valid null-terminated UTF-8 C string.
#[no_mangle]
pub extern "C" fn wdmm_transcode_preset_save(json: *const c_char) -> FfiString {
    let s = match ptr_to_str(json) {
        Some(s) => s,
        None => return FfiString::from("error:invalid json pointer"),
    };
    let preset: wdmm_storage::TranscodePreset = match serde_json::from_str(s) {
        Ok(p) => p,
        Err(e) => return FfiString::from(format!("error:parse:{e}")),
    };
    let db = global_library_db();
    match db.save_transcode_preset(&preset) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Delete a transcode preset by ID.
///
/// Built-in presets (`is_builtin = true`) are not deleted; the call returns
/// `"error:builtin"` in that case.  Returns `"ok"` or `"error:<message>"`.
/// Caller must free the result.
///
/// # Safety
/// `id` must be a valid null-terminated UTF-8 C string.
#[no_mangle]
pub extern "C" fn wdmm_transcode_preset_delete(id: *const c_char) -> FfiString {
    let s = match ptr_to_str(id) {
        Some(s) => s,
        None => return FfiString::from("error:invalid id pointer"),
    };
    let db = global_library_db();
    match db.delete_transcode_preset(s) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Register an HLS session with the proxy.
///
/// `device_id` — unique identifier for the player device (null-terminated UTF-8).
/// `input_url` — absolute file path or `http://127.0.0.1:{port}/…` loopback URL.
/// `preset_id` — ID of a [`TranscodePreset`] row (see `wdmm_transcode_presets_json`).
///
/// Returns JSON on success:
/// ```json
/// {"token":"3f7a…","playlist_template":"/h/3f7a…/index.m3u8","host":"127.0.0.1","port":43221}
/// ```
/// or `"error:<message>"` on failure. Caller must free the result via
/// [`wdmm_ffi_free_string`].
///
/// # Safety
/// All pointer arguments must be valid null-terminated UTF-8 C strings.
#[no_mangle]
pub extern "C" fn wdmm_proxy_register_hls(
    device_id: *const c_char,
    input_url: *const c_char,
    preset_id: *const c_char,
) -> FfiString {
    let device_str = match ptr_to_str(device_id) {
        Some(s) => s,
        None => return FfiString::from("error:invalid device_id"),
    };
    let url_str = match ptr_to_str(input_url) {
        Some(s) => s,
        None => return FfiString::from("error:invalid input_url"),
    };
    let pid = match ptr_to_str(preset_id) {
        Some(s) => s,
        None => return FfiString::from("error:invalid preset_id"),
    };

    let db = global_library_db();
    let preset = match db.get_transcode_preset(pid) {
        Ok(Some(p)) => p,
        Ok(None) => return FfiString::from(format!("error:preset not found: {pid}")),
        Err(e) => return FfiString::from(format!("error:{e}")),
    };

    let guard = proxy_cell().lock().unwrap();
    let handle = match guard.as_ref() {
        Some(h) => h,
        None => return FfiString::from("error:proxy not started — call wdmm_proxy_start first"),
    };
    let reg = rt().block_on(handle.register_hls(device_str, url_str, preset, None, None));
    match reg {
        Ok(r) => {
            let addr = handle.addr();
            let port = addr.map(|a| a.port()).unwrap_or(0);
            let host = addr
                .map(|a| a.ip().to_string())
                .unwrap_or_else(|| "127.0.0.1".into());
            FfiString::from(
                serde_json::json!({
                    "token":             r.token,
                    "playlist_template": r.playlist_template,
                    "host":              host,
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

// ── Proxy stream FFI ──────────────────────────────────────────────────────────
//
// Exposes `ProxyHandle::register_driver` as a C ABI for Android MustProxy /
// crypt-wrapper local stream playback. The callback is invoked from a Tokio
// request-handler thread. Two fill paths are supported:
//
//   * sync  — a native C callback fills `*out_buf` before returning `0`.
//   * async — a Dart `NativeCallable.listener` returns `0` with `*out_buf`
//     null; Rust parks the worker thread and waits for
//     `wdmm_proxy_stream_set_result(request_id, …)` to deposit the bytes.
//
// `request_id` is passed as an input parameter on the callback (thread-locals
// do not work: the Dart listener runs later on the Dart isolate, not the
// Tokio thread that entered the callback).

/// Range callback type for `wdmm_proxy_register_stream`.
///
/// Invoked from the proxy HTTP request-handler thread (a Tokio worker) for
/// every Range/full-content request on the registered token.
///
/// ## Sync fill path (native C callers)
/// Fill `*out_buf` (via [`wdmm_proxy_stream_alloc_buf`]) and `*out_len` before
/// returning `0`. Rust consumes the buffer immediately after the call returns.
/// Do **not** call [`wdmm_proxy_stream_set_result`] — the waiter is cancelled.
///
/// ## Async fill path (Dart `NativeCallable.listener`)
/// Return `0` and leave `*out_buf` null. Rust parks the request-handler thread
/// (up to [`PROXY_STREAM_TIMEOUT_SECS`] seconds) waiting for a call to
/// [`wdmm_proxy_stream_set_result`] with the given `request_id`. The Dart
/// listener should schedule `openContentRange`, allocate a buffer via
/// `wdmm_proxy_stream_alloc_buf`, copy the bytes, then call
/// `wdmm_proxy_stream_set_result(request_id, buf, len, 0)`. On error, call
/// `wdmm_proxy_stream_set_result(request_id, null, 0, -1)`.
///
/// ## Parameters
/// - `user_data`  — opaque pointer passed through from `wdmm_proxy_register_stream`.
/// - `start`      — inclusive byte offset.
/// - `end`        — inclusive end offset; **`-1` means read to EOF** (unknown size).
/// - `out_buf`    — sync path: set to a buffer allocated via
///   [`wdmm_proxy_stream_alloc_buf`]; Rust takes ownership and frees it after
///   consuming. Async path: leave null.
/// - `out_len`    — sync path: set to the byte count in `*out_buf`.
/// - `request_id` — async path: opaque `u64` id unique to this invocation; pass
///   it to [`wdmm_proxy_stream_set_result`] from the async reply. Always valid
///   for the lifetime of this invocation; cannot be derived from thread-locals.
///
/// Return `0` on success (sync-fill or async intent); any negative value is an
/// immediate error (the proxy emits an IO error to the HTTP client).
///
/// **Thread safety**: the callback and `user_data` must be safe to call from
/// any thread for the full lifetime of the registration.
pub type WdmmProxyRangeCallback = unsafe extern "C" fn(
    user_data: *mut std::os::raw::c_void,
    start: i64,
    end: i64,
    out_buf: *mut *mut u8,
    out_len: *mut usize,
    request_id: u64, // NEW — async reply channel id; see wdmm_proxy_stream_set_result
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

// ── Async reply channel ────────────────────────────────────────────────────────
//
// When a Dart NativeCallable.listener callback returns 0 with *out_buf == null,
// invoke_c_callback parks the Tokio worker thread waiting for
// wdmm_proxy_stream_set_result to deposit the bytes.  The waiter map links a
// per-invocation request_id to a (Mutex + Condvar) pair held by an Arc so
// both set_result and the timeout cleanup can access it safely.

/// Seconds invoke_c_callback will wait for wdmm_proxy_stream_set_result before
/// returning a timeout IO error to the HTTP client (30 s matches typical stream
/// decrypt latency budgets).
pub const PROXY_STREAM_TIMEOUT_SECS: u64 = 30;

/// Bytes (or error status) deposited by `wdmm_proxy_stream_set_result`.
struct WaiterResult {
    buf: *mut u8,
    len: usize,
    status: i32,
}
// Safety: buf was allocated on the Rust heap via wdmm_proxy_stream_alloc_buf.
// Exclusive ownership is transferred from set_result to invoke_c_callback.
unsafe impl Send for WaiterResult {}

struct Waiter {
    result: Mutex<Option<WaiterResult>>,
    condvar: Condvar,
}

/// Process-global, monotonically increasing request-id counter.
/// Starts at 1; 0 is reserved as a sentinel "no request".
static NEXT_REQ_ID: AtomicU64 = AtomicU64::new(1);

fn waiter_map() -> &'static Mutex<HashMap<u64, std::sync::Arc<Waiter>>> {
    static MAP: OnceLock<Mutex<HashMap<u64, std::sync::Arc<Waiter>>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
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
/// Supports two fill paths:
///
/// **Sync path** — callback fills `*out_buf` before returning `0`. Used by
/// native C callers (existing behaviour; existing C tests keep working).
///
/// **Async path** — callback returns `0` with `*out_buf` null. Used by Dart
/// `NativeCallable.listener` callers that schedule async decrypt. This function
/// parks the calling thread (up to [`PROXY_STREAM_TIMEOUT_SECS`]) until
/// [`wdmm_proxy_stream_set_result`] wakes it with the filled buffer.
fn invoke_c_callback(
    ctx: &StreamCallbackCtx,
    start: i64,
    end: i64,
) -> wdmm_proxy::BoxStream {
    use bytes::Bytes;
    use std::io;
    use std::sync::Arc;

    // Allocate a request id and register a waiter *before* calling the
    // callback so set_result has somewhere to deposit bytes even if it races.
    let req_id = NEXT_REQ_ID.fetch_add(1, Ordering::Relaxed);
    let waiter = Arc::new(Waiter {
        result: Mutex::new(None),
        condvar: Condvar::new(),
    });
    waiter_map().lock().unwrap().insert(req_id, Arc::clone(&waiter));

    let mut out_buf: *mut u8 = std::ptr::null_mut();
    let mut out_len: usize = 0;

    // Safety: caller holds Arc<StreamCallbackCtx> alive.
    let result = unsafe {
        (ctx.cb)(ctx.user_data.0, start, end, &mut out_buf, &mut out_len, req_id)
    };

    // ── Immediate error path ──────────────────────────────────────────────────
    if result < 0 {
        waiter_map().lock().unwrap().remove(&req_id);
        return Box::pin(futures::stream::once(futures::future::ready(
            Err(io::Error::new(
                io::ErrorKind::Other,
                format!("wdmm_proxy stream callback error {result}"),
            )),
        )));
    }

    // ── Sync fill path ────────────────────────────────────────────────────────
    if !out_buf.is_null() && out_len > 0 {
        // Callback filled the buffer synchronously — cancel the waiter.
        waiter_map().lock().unwrap().remove(&req_id);
        // Safety: buffer was allocated by wdmm_proxy_stream_alloc_buf (Rust heap).
        let chunk = unsafe { Vec::from_raw_parts(out_buf, out_len, out_len) };
        return Box::pin(futures::stream::once(futures::future::ready(
            Ok::<Bytes, io::Error>(Bytes::from(chunk)),
        )));
    }

    // ── Async fill path ───────────────────────────────────────────────────────
    // result == 0 but out_buf is null: Dart listener will call set_result.
    // Park this Tokio worker thread until the bytes arrive or timeout expires.
    let timeout = std::time::Duration::from_secs(PROXY_STREAM_TIMEOUT_SECS);
    let guard = waiter.result.lock().unwrap();
    let (mut guard, tout) = waiter
        .condvar
        .wait_timeout_while(guard, timeout, |opt| opt.is_none())
        .unwrap();

    // Remove from map regardless of outcome (set_result may have already done so).
    waiter_map().lock().unwrap().remove(&req_id);

    if tout.timed_out() || guard.is_none() {
        return Box::pin(futures::stream::once(futures::future::ready(
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("wdmm_proxy async callback timed out (req_id={req_id})"),
            )),
        )));
    }

    let wr = guard.take().unwrap();
    if wr.status < 0 || wr.buf.is_null() || wr.len == 0 {
        // Error status from the Dart side — free any orphaned buffer.
        if !wr.buf.is_null() && wr.len > 0 {
            unsafe { drop(Vec::from_raw_parts(wr.buf, wr.len, wr.len)) };
        }
        return Box::pin(futures::stream::once(futures::future::ready(
            Err(io::Error::new(
                io::ErrorKind::Other,
                format!("wdmm_proxy async callback error status {}", wr.status),
            )),
        )));
    }

    // Safety: buf was allocated via wdmm_proxy_stream_alloc_buf (Rust heap).
    let chunk = unsafe { Vec::from_raw_parts(wr.buf, wr.len, wr.len) };
    Box::pin(futures::stream::once(futures::future::ready(
        Ok::<Bytes, io::Error>(Bytes::from(chunk)),
    )))
}

/// Deliver async Range bytes to a parked [`invoke_c_callback`] invocation.
///
/// Called by the Dart listener (or any thread) after it has fetched and
/// decrypted the bytes for `request_id`.
///
/// - `request_id` — the id received by the [`WdmmProxyRangeCallback`].
/// - `buf`        — buffer allocated via [`wdmm_proxy_stream_alloc_buf`] and
///   filled with the requested bytes. Rust takes ownership on the success path
///   (`status == 0`). Pass `NULL` on the error path.
/// - `len`        — byte count in `buf` (ignored if `status < 0`).
/// - `status`     — `0` = success (buf/len valid); negative = error.
///
/// Wakes the waiter for `request_id`. Safe to call from any thread.
/// No-op (and safe) if `request_id` is unknown (already timed out or used).
///
/// # Safety
/// On the success path (`status == 0`, `buf` non-null), `buf` must have been
/// allocated via [`wdmm_proxy_stream_alloc_buf`]. Do not free it separately —
/// Rust reclaims it.
#[no_mangle]
pub extern "C" fn wdmm_proxy_stream_set_result(
    request_id: u64,
    buf: *mut u8,
    len: usize,
    status: i32,
) {
    // Remove first so a racing timeout cannot also try to use the waiter.
    let waiter = waiter_map().lock().unwrap().remove(&request_id);
    if let Some(w) = waiter {
        let mut guard = w.result.lock().unwrap();
        *guard = Some(WaiterResult { buf, len, status });
        w.condvar.notify_one();
    } else {
        // No waiter (timed out or double-call) — free an orphaned success buf.
        if !buf.is_null() && len > 0 {
            unsafe { drop(Vec::from_raw_parts(buf, len, len)) };
        }
    }
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
/// {"token":"3f7a…","path_template":"/s/3f7a…","host":"192.168.1.10","port":N}
/// ```
/// or `"error:<message>"` on failure. Caller must free the result via
/// [`wdmm_ffi_free_string`].
///
/// Caller builds the URL as `http://{host}:{port}{path_template}` using the
/// `host` and `port` values from the response.
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

    let reg = rt().block_on(handle.register_driver(size, name_opt, None, None, move |start, end| {
        invoke_c_callback(&ctx, start, end)
    }));

    match reg {
        Ok(r) => {
            let addr = handle.addr();
            let port = addr.map(|a| a.port()).unwrap_or(0);
            let host = addr
                .map(|a| a.ip().to_string())
                .unwrap_or_else(|| "127.0.0.1".into());
            FfiString::from(
                serde_json::json!({
                    "token":         r.token,
                    "path_template": r.path_template,
                    "host":          host,
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

// ── Token file (0600) ────────────────────────────────────────────────────────
//
// When the library DB is a real on-disk file, the API bearer token is also
// written to `<db_dir>/api-token` at mode 0600 so the token survives
// hot-restarts without a round-trip to the KV store.

/// Path to the 0600 token file, set once by `wdmm_library_init`.
/// `None` when the DB is `:memory:` (tests / smoke-runs).
static TOKEN_FILE_PATH: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();

fn api_token_file() -> Option<&'static std::path::PathBuf> {
    TOKEN_FILE_PATH.get().and_then(|o| o.as_ref())
}

/// Write `tok` to the 0600 token file (no-op when path unavailable).
fn write_token_file(tok: &str) {
    let Some(path) = api_token_file() else { return };
    use std::io::Write as _;
    if let Ok(mut f) = std::fs::File::create(path) {
        let _ = f.write_all(tok.as_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
    }
}

/// Resolve the API bearer token: token file → KV store → generate fresh.
///
/// Priority ensures the token survives app restarts (file) and migrates
/// gracefully from older KV-only builds (KV fallback with backfill).
fn resolve_api_token(db: &LibraryDatabase) -> String {
    // 1. Token file — most authoritative after first write.
    if let Some(path) = api_token_file() {
        if let Ok(tok) = std::fs::read_to_string(path) {
            let tok = tok.trim().to_string();
            if !tok.is_empty() {
                return tok;
            }
        }
    }
    // 2. KV store fallback (migrating from KV-only builds).
    if let Ok(Some(tok)) = db.load_kv(wdmm_openapi::TOKEN_KV_KEY) {
        if !tok.is_empty() {
            write_token_file(&tok); // backfill the file
            return tok;
        }
    }
    // 3. Generate a new token and persist to both destinations.
    let tok = uuid::Uuid::new_v4().to_string();
    let _ = db.save_kv(wdmm_openapi::TOKEN_KV_KEY, &tok);
    write_token_file(&tok);
    tok
}

// ── Library DB singleton ──────────────────────────────────────────────────────
//
// The FFI exposes a process-global library database. Real Android/Windows
// integration calls `wdmm_library_init(path)` once at app start before any
// library/KV operation. Production builds panic if the singleton is not
// initialised; test builds fall back to `:memory:` automatically.

static LIBRARY_DB: OnceLock<std::sync::Arc<LibraryDatabase>> = OnceLock::new();

fn global_library_db() -> &'static std::sync::Arc<LibraryDatabase> {
    #[cfg(test)]
    return LIBRARY_DB.get_or_init(|| {
        std::sync::Arc::new(
            LibraryDatabase::open(std::path::Path::new(":memory:"))
                .expect("FFI: failed to open in-memory library DB"),
        )
    });
    #[cfg(not(test))]
    LIBRARY_DB
        .get()
        .expect("BUG: wdmm_library_init must be called before any library operations")
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
    // Derive the token-file path from the DB path (only for real on-disk DBs).
    TOKEN_FILE_PATH.get_or_init(|| {
        if p == ":memory:" {
            None
        } else {
            std::path::Path::new(p).parent().map(|d| d.join("api-token"))
        }
    });
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

/// Delete all tracks for a given source/account. Returns `"ok"`.
#[no_mangle]
pub extern "C" fn wdmm_track_delete_by_source(source: *const c_char) -> FfiString {
    let source_str = match ptr_to_str(source) {
        Some(s) => s,
        None => return FfiString::from("error:null source pointer"),
    };
    match global_library_db().delete_tracks_by_account(source_str) {
        Ok(_) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

// ── Artist / Album aggregation FFI ────────────────────────────────────────────

/// List all artists as a JSON array.
/// Returns `"error:<message>"` on failure. Caller must free the result.
#[no_mangle]
pub extern "C" fn wdmm_artist_list() -> FfiString {
    match global_library_db().list_artists() {
        Ok(artists) => FfiString::from(
            serde_json::to_string(&artists).unwrap_or_else(|_| "[]".into()),
        ),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Get a single artist by ID as JSON.
/// Returns null if not found, or `"error:<message>"` on failure. Caller must free the result.
#[no_mangle]
pub extern "C" fn wdmm_artist_get(id: *const c_char) -> FfiString {
    let id_str = match ptr_to_str(id) {
        Some(s) => s,
        None => return FfiString::from("error:null id pointer"),
    };
    match global_library_db().get_artist(id_str) {
        Ok(Some(artist)) => FfiString::from(
            serde_json::to_string(&artist).unwrap_or_else(|_| "null".into()),
        ),
        Ok(None) => FfiString::from("null"),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// List all albums as a JSON array.
/// Returns `"error:<message>"` on failure. Caller must free the result.
#[no_mangle]
pub extern "C" fn wdmm_album_list() -> FfiString {
    match global_library_db().list_albums() {
        Ok(albums) => FfiString::from(
            serde_json::to_string(&albums).unwrap_or_else(|_| "[]".into()),
        ),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Get a single album by ID as JSON.
/// Returns null if not found, or `"error:<message>"` on failure. Caller must free the result.
#[no_mangle]
pub extern "C" fn wdmm_album_get(id: *const c_char) -> FfiString {
    let id_str = match ptr_to_str(id) {
        Some(s) => s,
        None => return FfiString::from("error:null id pointer"),
    };
    match global_library_db().get_album(id_str) {
        Ok(Some(album)) => FfiString::from(
            serde_json::to_string(&album).unwrap_or_else(|_| "null".into()),
        ),
        Ok(None) => FfiString::from("null"),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// List tracks for a given artist name as a JSON array.
/// Returns `"error:<message>"` on failure. Caller must free the result.
#[no_mangle]
pub extern "C" fn wdmm_tracks_by_artist(name: *const c_char) -> FfiString {
    let name_str = match ptr_to_str(name) {
        Some(s) => s,
        None => return FfiString::from("error:null name pointer"),
    };
    match global_library_db().tracks_by_artist_name(name_str) {
        Ok(tracks) => FfiString::from(
            serde_json::to_string(&tracks).unwrap_or_else(|_| "[]".into()),
        ),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// List tracks for a given album title as a JSON array.
/// Returns `"error:<message>"` on failure. Caller must free the result.
#[no_mangle]
pub extern "C" fn wdmm_tracks_by_album(title: *const c_char) -> FfiString {
    let title_str = match ptr_to_str(title) {
        Some(s) => s,
        None => return FfiString::from("error:null title pointer"),
    };
    match global_library_db().tracks_by_album_title(title_str) {
        Ok(tracks) => FfiString::from(
            serde_json::to_string(&tracks).unwrap_or_else(|_| "[]".into()),
        ),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Rebuild artist and album aggregates from tracks.
/// Returns `"ok"` on success or `"error:<message>"` on failure.
#[no_mangle]
pub extern "C" fn wdmm_library_rebuild_aggregates() -> FfiString {
    match global_library_db().rebuild_aggregates_from_tracks() {
        Ok(_) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Rebuild FTS index from all tracks. Returns `"ok"`.
#[no_mangle]
pub extern "C" fn wdmm_library_rebuild_fts() -> FfiString {
    match global_library_db().rebuild_fts_from_tracks() {
        Ok(_) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

// ── Library management FFI ────────────────────────────────────────────────────

/// Clear all library data except accounts and non-library KV keys. Returns `"ok"`.
#[no_mangle]
pub extern "C" fn wdmm_library_clear_all() -> FfiString {
    match global_library_db().clear_library_all() {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Clear library index (tracks, CUE, tombstones). Returns `"ok"`.
#[no_mangle]
pub extern "C" fn wdmm_library_clear_index() -> FfiString {
    match global_library_db().clear_library_index() {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Wipe manga index (mangas / pages / deleted_mangas / manga sync cursors).
/// Keeps manga_progress. Returns `"ok"` or `"error:…"`.
#[no_mangle]
pub extern "C" fn wdmm_manga_clear_index() -> FfiString {
    match global_library_db().clear_manga_index() {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}


/// Import tracks, playlists, playlist_entries, music tombstones
/// (`deleted_tracks`), and sync cursors (`sync_state`) from an old sqflite
/// database.
///
/// Two KV flags gate progress: `sqflite_imported` (legacy tracks/playlists/
/// entries) and `sqflite_imported_v6` (tombstones + sync_state). Devices that
/// already ran the legacy import get a v6 backfill (only the two new surfaces);
/// once both flags are set the call is a full no-op.
///
/// `old_db_path` must be a valid null-terminated UTF-8 path.
///
/// Returns a JSON object:
/// ```json
/// {"already_done":false,"tracks":42,"playlists":3,"entries":120,"deleted_tracks":5,"sync_state":2}
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

/// Compare the global library DB against an Android-schema sqflite DB at
/// `old_db_path`.
///
/// Reads the old DB read-only and compares the `tracks`, `deleted_tracks`, and
/// `sync_state` tables against the global Core library. Neither DB is mutated.
///
/// `old_db_path` must be a valid null-terminated UTF-8 path.
///
/// Returns a JSON `CompareReport`:
/// ```json
/// {"tracks":{"left_count":0,"right_count":0,"only_left":[],"only_right":[],"field_mismatches":[]},
///  "deleted_tracks":{…},"sync_state":{…},"ok":true}
/// ```
/// or `"error:<message>"` on failure. Caller must free the result.
#[no_mangle]
pub extern "C" fn wdmm_library_compare_with_sqflite(old_db_path: *const c_char) -> FfiString {
    let path_str = match ptr_to_str(old_db_path) {
        Some(s) => s,
        None => return FfiString::from("error:null path pointer"),
    };
    let old_path = std::path::Path::new(path_str);
    let db = global_library_db();
    match wdmm_storage::LibraryDatabase::compare_with_sqflite(old_path, db) {
        Ok(report) => FfiString::from(
            serde_json::to_string(&report).unwrap_or_else(|_| "{}".into()),
        ),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

// ── User folders FFI ──────────────────────────────────────────────────────────

/// List direct children of a folder.
///
/// `parent_id` may be null, `""`, or `"root"` — all mean the virtual root
/// (returns folders with `parent_id IS NULL`). Returns a JSON array of folder
/// objects, or `"error:<message>"`.
#[no_mangle]
pub extern "C" fn wdmm_user_folder_list_children(parent_id: *const c_char) -> FfiString {
    // null pointer → virtual root (same as "root")
    let pid: Option<&str> = if parent_id.is_null() {
        None
    } else {
        ptr_to_str(parent_id)
    };
    match global_library_db().list_user_folder_children(pid) {
        Ok(folders) => FfiString::from(
            serde_json::to_string(&folders).unwrap_or_else(|_| "[]".into()),
        ),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Create a user folder from JSON.
///
/// Input shape: `{"name":"…","parent_id":null|"…","sort_order":0}`
/// `parent_id` absent/null/`"root"` → virtual root.
/// Returns the new folder as JSON, or `"error:<message>"`.
#[no_mangle]
pub extern "C" fn wdmm_user_folder_create(json: *const c_char) -> FfiString {
    let s = match ptr_to_str(json) {
        Some(s) => s,
        None => return FfiString::from("error:null json pointer"),
    };
    let v: serde_json::Value = match serde_json::from_str(s) {
        Ok(v) => v,
        Err(e) => return FfiString::from(format!("error:json parse: {e}")),
    };
    let name = match v["name"].as_str() {
        Some(n) => n,
        None => return FfiString::from("error:missing 'name'"),
    };
    let parent_id: Option<&str> = v["parent_id"].as_str();
    let sort_order: Option<i64> = v["sort_order"].as_i64();
    match global_library_db().create_user_folder(name, parent_id, sort_order) {
        Ok(folder) => FfiString::from(
            serde_json::to_string(&folder).unwrap_or_else(|_| "{}".into()),
        ),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Rename a folder. Returns `"ok"` or `"error:<message>"`.
#[no_mangle]
pub extern "C" fn wdmm_user_folder_rename(
    id: *const c_char,
    name: *const c_char,
) -> FfiString {
    let id = match ptr_to_str(id) {
        Some(s) => s,
        None => return FfiString::from("error:null id pointer"),
    };
    let name = match ptr_to_str(name) {
        Some(s) => s,
        None => return FfiString::from("error:null name pointer"),
    };
    match global_library_db().rename_user_folder(id, name) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Move a folder to a new parent.
///
/// `new_parent_id` null/`""`/`"root"` → virtual root. Returns `"ok"` or
/// `"error:<message>"` (including `"error:…cycle…"` on invalid moves).
#[no_mangle]
pub extern "C" fn wdmm_user_folder_move(
    id: *const c_char,
    new_parent_id: *const c_char,
) -> FfiString {
    let id = match ptr_to_str(id) {
        Some(s) => s,
        None => return FfiString::from("error:null id pointer"),
    };
    let new_parent: Option<&str> = if new_parent_id.is_null() {
        None
    } else {
        ptr_to_str(new_parent_id)
    };
    match global_library_db().move_user_folder(id, new_parent) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Delete a folder and all descendants + their items recursively.
/// Returns `"ok"` or `"error:<message>"`.
#[no_mangle]
pub extern "C" fn wdmm_user_folder_delete(id: *const c_char) -> FfiString {
    let id = match ptr_to_str(id) {
        Some(s) => s,
        None => return FfiString::from("error:null id pointer"),
    };
    match global_library_db().delete_user_folder(id) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// List items in a folder. `folder_id = "root"` is valid (virtual root items).
/// Returns a JSON array or `"error:<message>"`.
#[no_mangle]
pub extern "C" fn wdmm_user_folder_list_items(folder_id: *const c_char) -> FfiString {
    let fid = match ptr_to_str(folder_id) {
        Some(s) => s,
        None => return FfiString::from("error:null folder_id pointer"),
    };
    match global_library_db().list_user_folder_items(fid) {
        Ok(items) => FfiString::from(
            serde_json::to_string(&items).unwrap_or_else(|_| "[]".into()),
        ),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Add (or update) an item in a folder from JSON.
///
/// Input shape: `{"folder_id":"…","ref_kind":"album|artist|genre|track|manga|mangaSeries",
/// "ref_key":"…","sort_order":0}`. `added_at` defaults to now if absent.
/// Returns `"ok"` or `"error:<message>"`.
#[no_mangle]
pub extern "C" fn wdmm_user_folder_add_item(json: *const c_char) -> FfiString {
    let s = match ptr_to_str(json) {
        Some(s) => s,
        None => return FfiString::from("error:null json pointer"),
    };
    let v: serde_json::Value = match serde_json::from_str(s) {
        Ok(v) => v,
        Err(e) => return FfiString::from(format!("error:json parse: {e}")),
    };
    let folder_id = match v["folder_id"].as_str() {
        Some(s) => s.to_string(),
        None => return FfiString::from("error:missing 'folder_id'"),
    };
    let ref_kind = match v["ref_kind"].as_str() {
        Some(s) => s.to_string(),
        None => return FfiString::from("error:missing 'ref_kind'"),
    };
    let ref_key = match v["ref_key"].as_str() {
        Some(s) => s.to_string(),
        None => return FfiString::from("error:missing 'ref_key'"),
    };
    let sort_order = v["sort_order"].as_i64().unwrap_or(0);
    let added_at = v["added_at"]
        .as_i64()
        .unwrap_or_else(|| chrono::Utc::now().timestamp());
    let item = wdmm_storage::UserFolderItem {
        folder_id,
        ref_kind,
        ref_key,
        sort_order,
        added_at,
    };
    match global_library_db().add_user_folder_item(&item) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Remove a specific item from a folder.
/// Returns `"ok"` or `"error:<message>"`.
#[no_mangle]
pub extern "C" fn wdmm_user_folder_remove_item(
    folder_id: *const c_char,
    ref_kind: *const c_char,
    ref_key: *const c_char,
) -> FfiString {
    let fid = match ptr_to_str(folder_id) {
        Some(s) => s,
        None => return FfiString::from("error:null folder_id pointer"),
    };
    let kind = match ptr_to_str(ref_kind) {
        Some(s) => s,
        None => return FfiString::from("error:null ref_kind pointer"),
    };
    let key = match ptr_to_str(ref_key) {
        Some(s) => s,
        None => return FfiString::from("error:null ref_key pointer"),
    };
    match global_library_db().remove_user_folder_item(fid, kind, key) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Atomically replace all items in a folder in one shot.
///
/// `folder_id` must be a valid null-terminated UTF-8 C string (`"root"` is
/// valid for the virtual root). `json_array` is a JSON array of item objects:
/// ```json
/// [{"ref_kind":"track","ref_key":"sha1…","sort_order":0,"added_at":1700000000}]
/// ```
/// `added_at` defaults to the current Unix timestamp if absent.
/// `sort_order` defaults to the element's array index if absent.
///
/// Previous items for the given `folder_id` are deleted atomically before the
/// new ones are inserted (single transaction). Other folders are unaffected.
///
/// Returns `"ok"` or `"error:<message>"`. Caller must free the result.
#[no_mangle]
pub extern "C" fn wdmm_user_folder_set_items(
    folder_id: *const c_char,
    json_array: *const c_char,
) -> FfiString {
    let fid = match ptr_to_str(folder_id) {
        Some(s) => s,
        None => return FfiString::from("error:null folder_id pointer"),
    };
    let s = match ptr_to_str(json_array) {
        Some(s) => s,
        None => return FfiString::from("error:null json_array pointer"),
    };
    let arr: serde_json::Value = match serde_json::from_str(s) {
        Ok(v) => v,
        Err(e) => return FfiString::from(format!("error:json parse: {e}")),
    };
    let items_json = match arr.as_array() {
        Some(a) => a,
        None => return FfiString::from("error:json_array must be a JSON array"),
    };
    let now = chrono::Utc::now().timestamp();
    let mut items = Vec::with_capacity(items_json.len());
    for (i, item) in items_json.iter().enumerate() {
        let ref_kind = match item["ref_kind"].as_str() {
            Some(s) => s.to_string(),
            None => return FfiString::from(format!("error:item[{i}] missing 'ref_kind'")),
        };
        let ref_key = match item["ref_key"].as_str() {
            Some(s) => s.to_string(),
            None => return FfiString::from(format!("error:item[{i}] missing 'ref_key'")),
        };
        let sort_order = item["sort_order"].as_i64().unwrap_or(i as i64);
        let added_at = item["added_at"].as_i64().unwrap_or(now);
        items.push(wdmm_storage::UserFolderItem {
            folder_id: fid.to_string(),
            ref_kind,
            ref_key,
            sort_order,
            added_at,
        });
    }
    match global_library_db().set_user_folder_items(fid, &items) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Parse a JSON value as a Unix timestamp (seconds).
///
/// Accepts either a JSON integer (already Unix seconds) or a JSON string in
/// RFC 3339 / ISO 8601 format (e.g. `"2024-11-01T12:00:00Z"`). Returns `now`
/// for missing (`Null`) or unrecognised values.
fn parse_timestamp_field(v: &serde_json::Value, now: i64) -> i64 {
    if let Some(i) = v.as_i64() {
        return i;
    }
    if let Some(s) = v.as_str() {
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
            return dt.timestamp();
        }
    }
    now
}

/// Atomically replace the entire user-folder tree from a cloud-sync snapshot.
///
/// Input JSON shape:
/// ```json
/// {
///   "folders": [
///     {"id":"…","parent_id":null|"…","name":"…","sort_order":0,
///      "created_at": <unix_i64 | ISO8601 string>}
///   ],
///   "items": [
///     {"folder_id":"…"|"root","ref_kind":"…","ref_key":"…","sort_order":0,
///      "added_at": <unix_i64 | ISO8601 string>}
///   ]
/// }
/// ```
///
/// * `parent_id` absent / `null` / `"root"` / `""` → virtual root (SQL NULL).
/// * `created_at` / `added_at`: Unix i64 **or** ISO 8601 string; missing → now.
/// * `sort_order`: missing / null → 0.
///
/// Returns `"ok"` on success or `"error:<message>"` on failure.
/// Caller must free the returned string with `wdmm_ffi_free_string`.
#[no_mangle]
pub extern "C" fn wdmm_user_folder_replace_snapshot(json: *const c_char) -> FfiString {
    let s = match ptr_to_str(json) {
        Some(s) => s,
        None => return FfiString::from("error:null json pointer"),
    };
    let v: serde_json::Value = match serde_json::from_str(s) {
        Ok(v) => v,
        Err(e) => return FfiString::from(format!("error:json parse: {e}")),
    };
    let now = chrono::Utc::now().timestamp();

    // ── Parse folders ─────────────────────────────────────────────────────────
    let folders_json = match v["folders"].as_array() {
        Some(a) => a,
        None => return FfiString::from("error:'folders' must be a JSON array"),
    };
    let mut folders = Vec::with_capacity(folders_json.len());
    for (i, f) in folders_json.iter().enumerate() {
        let id = match f["id"].as_str() {
            Some(s) => s.to_string(),
            None => return FfiString::from(format!("error:folders[{i}] missing 'id'")),
        };
        let name = match f["name"].as_str() {
            Some(s) => s.to_string(),
            None => return FfiString::from(format!("error:folders[{i}] missing 'name'")),
        };
        // parent_id: null JSON value, absent, "root", or "" all mean virtual root.
        let parent_id: Option<String> = f["parent_id"].as_str().and_then(|p| {
            if p.is_empty() || p == "root" { None } else { Some(p.to_string()) }
        });
        let sort_order = f["sort_order"].as_i64().unwrap_or(0);
        let created_at = parse_timestamp_field(&f["created_at"], now);
        folders.push(wdmm_storage::UserFolder {
            id,
            name,
            parent_id,
            sort_order,
            created_at,
        });
    }

    // ── Parse items ───────────────────────────────────────────────────────────
    let items_json = match v["items"].as_array() {
        Some(a) => a,
        None => return FfiString::from("error:'items' must be a JSON array"),
    };
    let mut items = Vec::with_capacity(items_json.len());
    for (i, item) in items_json.iter().enumerate() {
        let folder_id = match item["folder_id"].as_str() {
            Some(s) => s.to_string(),
            None => return FfiString::from(format!("error:items[{i}] missing 'folder_id'")),
        };
        let ref_kind = match item["ref_kind"].as_str() {
            Some(s) => s.to_string(),
            None => return FfiString::from(format!("error:items[{i}] missing 'ref_kind'")),
        };
        let ref_key = match item["ref_key"].as_str() {
            Some(s) => s.to_string(),
            None => return FfiString::from(format!("error:items[{i}] missing 'ref_key'")),
        };
        let sort_order = item["sort_order"].as_i64().unwrap_or(0);
        let added_at = parse_timestamp_field(&item["added_at"], now);
        items.push(wdmm_storage::UserFolderItem {
            folder_id,
            ref_kind,
            ref_key,
            sort_order,
            added_at,
        });
    }

    match global_library_db().replace_user_folders(&folders, &items) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

// ── CUE sheet FFI (v5) ────────────────────────────────────────────────────────

/// Upsert a CUE album from JSON. Returns `"ok"` or `"error:<message>"`.
///
/// Expected JSON shape:
/// ```json
/// {
///   "cue_id": "...",
///   "source_name": "...",
///   "cue_remote_path": "...",
///   "title": "...",
///   "performer": "...",
///   "cache_group_id": null,
///   "created_at": "2024-01-01T00:00:00Z"
/// }
/// ```
#[no_mangle]
pub extern "C" fn wdmm_cue_album_upsert(json: *const c_char) -> FfiString {
    let s = match ptr_to_str(json) {
        Some(s) => s,
        None => return FfiString::from("error:null json pointer"),
    };
    let album: wdmm_storage::CueAlbum = match serde_json::from_str(s) {
        Ok(a) => a,
        Err(e) => return FfiString::from(format!("error:parse CueAlbum: {e}")),
    };
    match global_library_db().upsert_cue_album(&album) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Upsert a CUE slice from JSON. Returns `"ok"` or `"error:<message>"`.
///
/// Expected JSON shape: see `CueSlice` struct fields.
#[no_mangle]
pub extern "C" fn wdmm_cue_slice_upsert(json: *const c_char) -> FfiString {
    let s = match ptr_to_str(json) {
        Some(s) => s,
        None => return FfiString::from("error:null json pointer"),
    };
    let slice: wdmm_storage::CueSlice = match serde_json::from_str(s) {
        Ok(sl) => sl,
        Err(e) => return FfiString::from(format!("error:parse CueSlice: {e}")),
    };
    match global_library_db().upsert_cue_slice(&slice) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Delete a CUE slice by music_id. Returns `"ok"` or `"error:<message>"`.
#[no_mangle]
pub extern "C" fn wdmm_cue_slice_delete(music_id: *const c_char) -> FfiString {
    let id = match ptr_to_str(music_id) {
        Some(s) => s,
        None => return FfiString::from("error:null music_id pointer"),
    };
    match global_library_db().delete_cue_slice(id) {
        Ok(_) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Delete a CUE album and its slices. Returns `"ok"` or `"error:<message>"`.
#[no_mangle]
pub extern "C" fn wdmm_cue_album_delete(
    source_name: *const c_char,
    cue_remote_path: *const c_char,
) -> FfiString {
    let src = match ptr_to_str(source_name) {
        Some(s) => s,
        None => return FfiString::from("error:null source_name pointer"),
    };
    let path = match ptr_to_str(cue_remote_path) {
        Some(s) => s,
        None => return FfiString::from("error:null cue_remote_path pointer"),
    };
    match global_library_db().delete_cue_album(src, path) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Count remaining slices for a CUE album. Returns the count as a decimal string
/// (e.g. `"5"`), or `"error:<message>"`.
#[no_mangle]
pub extern "C" fn wdmm_cue_remaining_slices(
    source_name: *const c_char,
    cue_remote_path: *const c_char,
) -> FfiString {
    let src = match ptr_to_str(source_name) {
        Some(s) => s,
        None => return FfiString::from("error:null source_name pointer"),
    };
    let path = match ptr_to_str(cue_remote_path) {
        Some(s) => s,
        None => return FfiString::from("error:null cue_remote_path pointer"),
    };
    match global_library_db().remaining_slices_for_cue(src, path) {
        Ok(count) => FfiString::from(count.to_string()),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Get a track (or CUE slice) by source name (`account_id`) and remote path.
///
/// Looks up `tracks` first; if found, returns the `LibraryTrack` as JSON. If no
/// track matches, looks up `cue_slices`; if a slice matches, returns the
/// `CueSlice` as JSON. Callers discriminate the two shapes by key presence:
/// a `LibraryTrack` carries `id` / `account_id`; a `CueSlice` carries
/// `music_id` / `cue_id`. Returns the literal `"null"` when neither matches,
/// or `"error:<message>"` on storage error.
#[no_mangle]
pub extern "C" fn wdmm_track_get_by_source_path(
    account_id: *const c_char,
    remote_path: *const c_char,
) -> FfiString {
    let acct = match ptr_to_str(account_id) {
        Some(s) => s,
        None => return FfiString::from("error:null account_id pointer"),
    };
    let path = match ptr_to_str(remote_path) {
        Some(s) => s,
        None => return FfiString::from("error:null remote_path pointer"),
    };
    let db = global_library_db();
    match db.get_track_by_source_path(acct, path) {
        Ok(Some(t)) => FfiString::from(
            serde_json::to_string(&t).unwrap_or_else(|_| "null".into()),
        ),
        Ok(None) => match db.get_cue_slice_by_source_path(acct, path) {
            Ok(Some(slice)) => FfiString::from(
                serde_json::to_string(&slice).unwrap_or_else(|_| "null".into()),
            ),
            Ok(None) => FfiString::from("null"),
            Err(e) => FfiString::from(format!("error:{e}")),
        },
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Get the CUE album that owns the slice with the given `music_id`.
/// Returns the `CueAlbum` as JSON, the literal `"null"` if not found, or
/// `"error:<message>"` on storage error.
#[no_mangle]
pub extern "C" fn wdmm_cue_album_for_slice(music_id: *const c_char) -> FfiString {
    let id = match ptr_to_str(music_id) {
        Some(s) => s,
        None => return FfiString::from("error:null music_id pointer"),
    };
    match global_library_db().cue_album_for_slice(id) {
        Ok(Some(album)) => FfiString::from(
            serde_json::to_string(&album).unwrap_or_else(|_| "null".into()),
        ),
        Ok(None) => FfiString::from("null"),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// List all CUE slices for a CUE album by `cue_id`, ordered by `track_index`.
/// Returns a JSON array of `CueSlice` (may be `[]`), or `"error:<message>"`.
#[no_mangle]
pub extern "C" fn wdmm_cue_slices_for_cue(cue_id: *const c_char) -> FfiString {
    let id = match ptr_to_str(cue_id) {
        Some(s) => s,
        None => return FfiString::from("error:null cue_id pointer"),
    };
    match global_library_db().list_cue_slices_for_cue(id) {
        Ok(slices) => FfiString::from(
            serde_json::to_string(&slices).unwrap_or_else(|_| "[]".into()),
        ),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// List all CUE albums, ordered by `cue_id`. Returns a JSON array of
/// `CueAlbum` (snake_case fields), or `"error:<message>"`.
#[no_mangle]
pub extern "C" fn wdmm_list_cue_albums() -> FfiString {
    match global_library_db().list_cue_albums() {
        Ok(albums) => FfiString::from(
            serde_json::to_string(&albums).unwrap_or_else(|_| "[]".into()),
        ),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

// ── Playlists FFI ─────────────────────────────────────────────────────────────

/// List all playlists. Returns a JSON array or `"error:<message>"`.
#[no_mangle]
pub extern "C" fn wdmm_playlist_list() -> FfiString {
    match global_library_db().list_playlists() {
        Ok(playlists) => FfiString::from(
            serde_json::to_string(&playlists).unwrap_or_else(|_| "[]".into()),
        ),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Create a playlist from JSON.
///
/// Input shape: `{"id":"optional-uuid","name":"…"}`.
/// If `id` is absent a UUID is generated.
/// Returns the new playlist as JSON, or `"error:<message>"`.
#[no_mangle]
pub extern "C" fn wdmm_playlist_create(json: *const c_char) -> FfiString {
    let s = match ptr_to_str(json) {
        Some(s) => s,
        None => return FfiString::from("error:null json pointer"),
    };
    let v: serde_json::Value = match serde_json::from_str(s) {
        Ok(v) => v,
        Err(e) => return FfiString::from(format!("error:json parse: {e}")),
    };
    let name = match v["name"].as_str() {
        Some(n) => n.to_string(),
        None => return FfiString::from("error:missing 'name'"),
    };
    let id = v["id"]
        .as_str()
        .map(|s| s.to_string())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let now = chrono::Utc::now();
    let playlist = wdmm_storage::Playlist {
        id,
        name,
        created_at: now,
        updated_at: now,
    };
    match global_library_db().upsert_playlist(&playlist) {
        Ok(()) => FfiString::from(
            serde_json::to_string(&playlist).unwrap_or_else(|_| "{}".into()),
        ),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Delete a playlist (and its entries via ON DELETE CASCADE).
/// Returns `"ok"` or `"error:<message>"`.
#[no_mangle]
pub extern "C" fn wdmm_playlist_delete(id: *const c_char) -> FfiString {
    let id = match ptr_to_str(id) {
        Some(s) => s,
        None => return FfiString::from("error:null id pointer"),
    };
    match global_library_db().delete_playlist(id) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// List entries of a playlist ordered by position.
/// Returns a JSON array or `"error:<message>"`.
#[no_mangle]
pub extern "C" fn wdmm_playlist_list_entries(id: *const c_char) -> FfiString {
    let id = match ptr_to_str(id) {
        Some(s) => s,
        None => return FfiString::from("error:null id pointer"),
    };
    match global_library_db().list_playlist_entries(id) {
        Ok(entries) => FfiString::from(
            serde_json::to_string(&entries).unwrap_or_else(|_| "[]".into()),
        ),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Replace all entries of a playlist from a JSON array.
///
/// Each element: `{"music_id":"…","source_name":"…","remote_path":"…",
/// "title":"…","duration_ms":0}`. Position is assigned by array order.
/// Returns `"ok"` or `"error:<message>"`.
#[no_mangle]
pub extern "C" fn wdmm_playlist_set_entries(
    id: *const c_char,
    json_array: *const c_char,
) -> FfiString {
    let playlist_id = match ptr_to_str(id) {
        Some(s) => s,
        None => return FfiString::from("error:null id pointer"),
    };
    let s = match ptr_to_str(json_array) {
        Some(s) => s,
        None => return FfiString::from("error:null json_array pointer"),
    };
    let arr: serde_json::Value = match serde_json::from_str(s) {
        Ok(v) => v,
        Err(e) => return FfiString::from(format!("error:json parse: {e}")),
    };
    let items = match arr.as_array() {
        Some(a) => a,
        None => return FfiString::from("error:json_array must be a JSON array"),
    };
    let mut entries = Vec::with_capacity(items.len());
    for (pos, item) in items.iter().enumerate() {
        let music_id = match item["music_id"].as_str() {
            Some(s) => s.to_string(),
            None => return FfiString::from(format!("error:entry[{pos}] missing 'music_id'")),
        };
        let source_name = match item["source_name"].as_str() {
            Some(s) => s.to_string(),
            None => return FfiString::from(format!("error:entry[{pos}] missing 'source_name'")),
        };
        let remote_path = match item["remote_path"].as_str() {
            Some(s) => s.to_string(),
            None => return FfiString::from(format!("error:entry[{pos}] missing 'remote_path'")),
        };
        let title = match item["title"].as_str() {
            Some(s) => s.to_string(),
            None => return FfiString::from(format!("error:entry[{pos}] missing 'title'")),
        };
        let duration_ms = item["duration_ms"].as_i64().unwrap_or(0);
        entries.push(wdmm_storage::PlaylistEntry {
            playlist_id: playlist_id.to_string(),
            position: pos as i64,
            music_id,
            source_name,
            remote_path,
            title,
            duration_ms,
        });
    }
    match global_library_db().set_playlist_entries(playlist_id, &entries) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

// ── KV store FFI ──────────────────────────────────────────────────────────────
//
// Thin wrappers over LibraryDatabase::save_kv / load_kv / delete_kv.
// The library DB must be initialised via `wdmm_library_init` before these are
// called; undefined behaviour otherwise.

/// Get a value from the KV store.
///
/// Returns the stored string value, the bare string `"null"` if the key is
/// absent, or `"error:<message>"` on failure. Caller must free the result.
///
/// `key` must be a valid null-terminated UTF-8 C string.
#[no_mangle]
pub extern "C" fn wdmm_kv_get(key: *const c_char) -> FfiString {
    let k = match ptr_to_str(key) {
        Some(s) => s,
        None => return FfiString::from("error:null key pointer"),
    };
    match global_library_db().load_kv(k) {
        Ok(Some(v)) => FfiString::from(v),
        Ok(None) => FfiString::from("null"),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Set a KV value (insert or replace).
///
/// Returns `"ok"` or `"error:<message>"`. Caller must free the result.
/// Both pointers must be valid null-terminated UTF-8 C strings.
#[no_mangle]
pub extern "C" fn wdmm_kv_set(key: *const c_char, value: *const c_char) -> FfiString {
    let k = match ptr_to_str(key) {
        Some(s) => s,
        None => return FfiString::from("error:null key pointer"),
    };
    let v = match ptr_to_str(value) {
        Some(s) => s,
        None => return FfiString::from("error:null value pointer"),
    };
    match global_library_db().save_kv(k, v) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Delete a KV entry.
///
/// Returns `"ok"` or `"error:<message>"`. Caller must free the result.
/// `key` must be a valid null-terminated UTF-8 C string.
#[no_mangle]
pub extern "C" fn wdmm_kv_delete(key: *const c_char) -> FfiString {
    let k = match ptr_to_str(key) {
        Some(s) => s,
        None => return FfiString::from("error:null key pointer"),
    };
    match global_library_db().delete_kv(k) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

// ── Settings FFI ──────────────────────────────────────────────────────────────
//
// Portable settings backed by the KV store.
//
// `network_last_path` is intentionally **session-only** (in-process Mutex,
// never written to the KV store). Android semantics require the path to reset
// on every app restart; the OnceLock below initialises to `"/"`.

fn session_network_last_path() -> &'static Mutex<String> {
    static PATH: OnceLock<Mutex<String>> = OnceLock::new();
    PATH.get_or_init(|| Mutex::new("/".to_string()))
}

const KEY_NETWORK_REMEMBER_LAST_PATH: &str = "network_remember_last_path";

/// Get the `network_remember_last_path` setting (persisted in KV).
///
/// Returns `"true"`, `"false"`, or `"error:<message>"`. Defaults to `"false"`
/// if the key has never been set. Caller must free the result.
#[no_mangle]
pub extern "C" fn wdmm_settings_get_network_remember_last_path() -> FfiString {
    match global_library_db().load_kv(KEY_NETWORK_REMEMBER_LAST_PATH) {
        Ok(Some(v)) => FfiString::from(v),
        Ok(None) => FfiString::from("false"),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Set the `network_remember_last_path` setting (persisted in KV).
///
/// `enabled`: non-zero → `"true"`, `0` → `"false"`.
/// Also strips the legacy key `network_last_path` from the KV store on every call.
/// Returns `"ok"` or `"error:<message>"`. Caller must free the result.
#[no_mangle]
pub extern "C" fn wdmm_settings_set_network_remember_last_path(enabled: i32) -> FfiString {
    let value = if enabled != 0 { "true" } else { "false" };
    // Strip legacy key unconditionally to clean up stale installs.
    let _ = global_library_db().delete_kv("network_last_path");
    match global_library_db().save_kv(KEY_NETWORK_REMEMBER_LAST_PATH, value) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Get the session-only `network_last_path` (never persisted to KV).
///
/// Returns the current in-process path, or `"/"` if never set this session.
/// Caller must free the result.
#[no_mangle]
pub extern "C" fn wdmm_settings_get_network_last_path() -> FfiString {
    let path = session_network_last_path().lock().unwrap().clone();
    FfiString::from(path)
}

/// Set the session-only `network_last_path` (in-process Mutex; never persisted).
///
/// `path` must be a valid null-terminated UTF-8 C string.
/// Returns `"ok"`. Caller must free the result.
#[no_mangle]
pub extern "C" fn wdmm_settings_set_network_last_path(path: *const c_char) -> FfiString {
    let p = match ptr_to_str(path) {
        Some(s) => s,
        None => return FfiString::from("error:null path pointer"),
    };
    *session_network_last_path().lock().unwrap() = p.to_string();
    FfiString::ok()
}

// ── Manga FFI ─────────────────────────────────────────────────────────────────

/// Return all mangas in the library as a JSON array.
/// Returns `"error:…"` on failure. Caller must free the result.
#[no_mangle]
pub extern "C" fn wdmm_manga_list() -> FfiString {
    let db = global_library_db();
    match db.list_mangas() {
        Ok(mangas) => match serde_json::to_string(&mangas) {
            Ok(j) => FfiString::from(j),
            Err(e) => FfiString::from(format!("error:{e}")),
        },
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Get a single manga by id. Returns the manga as JSON or `"null"` if not found.
/// `id` must be a valid null-terminated UTF-8 C string.
/// Returns `"error:…"` on failure. Caller must free the result.
#[no_mangle]
pub extern "C" fn wdmm_manga_get(id: *const c_char) -> FfiString {
    let id = match ptr_to_str(id) {
        Some(s) => s,
        None => return FfiString::from("error:null id pointer"),
    };
    let db = global_library_db();
    match db.get_manga(id) {
        Ok(Some(m)) => match serde_json::to_string(&m) {
            Ok(j) => FfiString::from(j),
            Err(e) => FfiString::from(format!("error:{e}")),
        },
        Ok(None) => FfiString::from("null"),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Upsert a manga. `json` is the manga serialised as a JSON object.
/// Returns `"ok"` or `"error:…"`. Caller must free the result.
#[no_mangle]
pub extern "C" fn wdmm_manga_upsert(json: *const c_char) -> FfiString {
    let raw = match ptr_to_str(json) {
        Some(s) => s,
        None => return FfiString::from("error:null json pointer"),
    };
    let manga: wdmm_storage::Manga = match serde_json::from_str(raw) {
        Ok(m) => m,
        Err(e) => return FfiString::from(format!("error:{e}")),
    };
    match global_library_db().upsert_manga(&manga) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Delete a manga (cascades to pages + progress; writes a `deleted_mangas` tombstone).
/// `id` must be a valid null-terminated UTF-8 C string.
/// Returns `"ok"`, `"not_found"`, or `"error:…"`. Caller must free the result.
#[no_mangle]
pub extern "C" fn wdmm_manga_delete(id: *const c_char) -> FfiString {
    let id = match ptr_to_str(id) {
        Some(s) => s,
        None => return FfiString::from("error:null id pointer"),
    };
    match global_library_db().delete_manga(id) {
        Ok(()) => FfiString::ok(),
        Err(wdmm_storage::StorageError::NotFound(_)) => FfiString::from("not_found"),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Clear a single `deleted_mangas` tombstone by its sync key.
/// `sync_key` must be a valid null-terminated UTF-8 C string.
/// Returns `"ok"` or `"error:…"`. Caller must free the result.
#[no_mangle]
pub extern "C" fn wdmm_deleted_manga_clear(sync_key: *const c_char) -> FfiString {
    let key = match ptr_to_str(sync_key) {
        Some(s) => s,
        None => return FfiString::from("error:null sync_key pointer"),
    };
    match global_library_db().clear_deleted_manga(key) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// List all `deleted_mangas` tombstones. Returns a JSON array of `DeletedManga`
/// (snake_case fields, ordered by rowid ASC), or `"error:<message>"`.
#[no_mangle]
pub extern "C" fn wdmm_deleted_manga_list() -> FfiString {
    match global_library_db().list_deleted_mangas() {
        Ok(rows) => match serde_json::to_string(&rows) {
            Ok(j) => FfiString::from(j),
            Err(e) => FfiString::from(format!("error:serialize: {e}")),
        },
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Upsert a `deleted_mangas` tombstone from JSON. Returns `"ok"` or
/// `"error:<message>"`.
///
/// Expected JSON shape: see `DeletedManga` struct fields.
#[no_mangle]
pub extern "C" fn wdmm_deleted_manga_upsert(json: *const c_char) -> FfiString {
    let s = match ptr_to_str(json) {
        Some(s) => s,
        None => return FfiString::from("error:null json pointer"),
    };
    let m: wdmm_storage::DeletedManga = match serde_json::from_str(s) {
        Ok(v) => v,
        Err(e) => return FfiString::from(format!("error:parse DeletedManga: {e}")),
    };
    match global_library_db().upsert_deleted_manga(&m) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// List all pages for a manga as a JSON array.
/// `manga_id` must be a valid null-terminated UTF-8 C string.
/// Returns `"error:…"` on failure. Caller must free the result.
#[no_mangle]
pub extern "C" fn wdmm_manga_list_pages(manga_id: *const c_char) -> FfiString {
    let id = match ptr_to_str(manga_id) {
        Some(s) => s,
        None => return FfiString::from("error:null manga_id pointer"),
    };
    match global_library_db().list_pages(id) {
        Ok(pages) => match serde_json::to_string(&pages) {
            Ok(j) => FfiString::from(j),
            Err(e) => FfiString::from(format!("error:{e}")),
        },
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Atomically replace all pages for a manga.
/// `manga_id` must be a valid null-terminated UTF-8 C string.
/// `json_array` is a JSON array of `{"manga_id","page_index","rel_path"}` objects.
/// Returns `"ok"` or `"error:…"`. Caller must free the result.
#[no_mangle]
pub extern "C" fn wdmm_manga_set_pages(
    manga_id: *const c_char,
    json_array: *const c_char,
) -> FfiString {
    let id = match ptr_to_str(manga_id) {
        Some(s) => s,
        None => return FfiString::from("error:null manga_id pointer"),
    };
    let raw = match ptr_to_str(json_array) {
        Some(s) => s,
        None => return FfiString::from("error:null json_array pointer"),
    };
    let pages: Vec<wdmm_storage::MangaPage> = match serde_json::from_str(raw) {
        Ok(p) => p,
        Err(e) => return FfiString::from(format!("error:{e}")),
    };
    match global_library_db().replace_pages(id, &pages) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Get the reading progress for a manga. Returns the progress as JSON or `"null"`.
/// `manga_id` must be a valid null-terminated UTF-8 C string.
/// Returns `"error:…"` on failure. Caller must free the result.
#[no_mangle]
pub extern "C" fn wdmm_manga_get_progress(manga_id: *const c_char) -> FfiString {
    let id = match ptr_to_str(manga_id) {
        Some(s) => s,
        None => return FfiString::from("error:null manga_id pointer"),
    };
    match global_library_db().get_progress(id) {
        Ok(Some(p)) => match serde_json::to_string(&p) {
            Ok(j) => FfiString::from(j),
            Err(e) => FfiString::from(format!("error:{e}")),
        },
        Ok(None) => FfiString::from("null"),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Set (upsert) the reading progress for a manga.
/// `manga_id` must be a valid null-terminated UTF-8 C string.
/// `page_index` is the current 0-based page index.
/// Returns `"ok"` or `"error:…"`. Caller must free the result.
#[no_mangle]
pub extern "C" fn wdmm_manga_set_progress(
    manga_id: *const c_char,
    page_index: i64,
) -> FfiString {
    let id = match ptr_to_str(manga_id) {
        Some(s) => s,
        None => return FfiString::from("error:null manga_id pointer"),
    };
    match global_library_db().set_progress(id, page_index) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// List reading progress for all mangas as a JSON array.
/// Returns `"error:…"` on failure. Caller must free the result.
#[no_mangle]
pub extern "C" fn wdmm_manga_list_progress() -> FfiString {
    match global_library_db().list_progress() {
        Ok(progress) => match serde_json::to_string(&progress) {
            Ok(j) => FfiString::from(j),
            Err(e) => FfiString::from(format!("error:{e}")),
        },
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// List distinct series tags as a JSON array of `{"id","title"}` objects.
///
/// The `id` field is `sha1("mangaSeries" ‖ title)` encoded as lower-case hex.
/// Returns `"error:…"` on failure. Caller must free the result.
#[no_mangle]
pub extern "C" fn wdmm_manga_list_series() -> FfiString {
    match global_library_db().list_series_tags() {
        Ok(tags) => match serde_json::to_string(&tags) {
            Ok(j) => FfiString::from(j),
            Err(e) => FfiString::from(format!("error:{e}")),
        },
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}


// ── Cloud account FFI ─────────────────────────────────────────────────────────

use wdmm_core::models::CloudAccount;
use wdmm_crypto::CredentialEncoder;

fn account_to_json(a: &CloudAccount) -> serde_json::Value {
    serde_json::json!({
        "id": a.id,
        "provider_type": a.provider_type,
        "display_name": a.display_name,
        "base_url": a.base_url,
        "credentials_b64": BASE64_STANDARD.encode(&a.credentials_encrypted),
    })
}

/// List all cloud accounts as a JSON array.
/// Each element includes `credentials_b64` (base64 of `credentials_encrypted`).
/// Returns `"error:…"` on failure. Caller must free the result.
#[no_mangle]
pub extern "C" fn wdmm_account_list() -> FfiString {
    match global_library_db().list_accounts() {
        Ok(accounts) => {
            let view: Vec<_> = accounts.iter().map(account_to_json).collect();
            match serde_json::to_string(&view) {
                Ok(j) => FfiString::from(j),
                Err(e) => FfiString::from(format!("error:{e}")),
            }
        }
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Get one account by id. Returns account JSON, `"null"` if missing, or `"error:…"`.
/// `id` must be a valid null-terminated UTF-8 C string.
#[no_mangle]
pub extern "C" fn wdmm_account_get(id: *const c_char) -> FfiString {
    let id = match ptr_to_str(id) {
        Some(s) => s,
        None => return FfiString::from("error:null id pointer"),
    };
    match global_library_db().get_account(id) {
        Ok(Some(a)) => match serde_json::to_string(&account_to_json(&a)) {
            Ok(j) => FfiString::from(j),
            Err(e) => FfiString::from(format!("error:{e}")),
        },
        Ok(None) => FfiString::from("null"),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Upsert a cloud account from JSON.
///
/// Shape:
/// ```json
/// {
///   "id": "optional-uuid",
///   "provider_type": "webdav",
///   "display_name": "…",
///   "base_url": "https://…",
///   "credentials_b64": "<optional>",
///   "credentials": { /* any JSON object */ },
///   "passphrase": "<optional; non-empty → encrypt via CredentialEncoder (requires flat string map)>"
/// }
/// ```
/// If `credentials` is present and `passphrase` empty/absent → store UTF-8 JSON bytes of the object
/// (supports nested structures like crypt `source:{provider_type,config}`).
/// If `credentials` is present and `passphrase` non-empty → parse as flat `HashMap<String,String>` and
/// encrypt via CredentialEncoder (nested structures not supported for encrypted vault).
/// If only `credentials_b64` → decode into `credentials_encrypted`.
/// Returns the account JSON (with `credentials_b64`) or `"error:…"`.
#[no_mangle]
pub extern "C" fn wdmm_account_upsert(json: *const c_char) -> FfiString {
    let raw = match ptr_to_str(json) {
        Some(s) => s,
        None => return FfiString::from("error:null json pointer"),
    };
    let v: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => return FfiString::from(format!("error:json parse: {e}")),
    };
    let provider_type = match v["provider_type"].as_str() {
        Some(s) => s.to_string(),
        None => return FfiString::from("error:missing 'provider_type'"),
    };
    let display_name = match v["display_name"].as_str() {
        Some(s) => s.to_string(),
        None => return FfiString::from("error:missing 'display_name'"),
    };
    let id = v["id"]
        .as_str()
        .map(|s| s.to_string())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let base_url = v["base_url"].as_str().map(|s| s.to_string());

    let credentials_encrypted = if let Some(obj) = v.get("credentials").filter(|x| x.is_object()) {
        let pass = v["passphrase"].as_str().unwrap_or("");
        if pass.is_empty() {
            // Plaintext path: accept any JSON object structure (nested objects, arrays, numbers, etc.)
            match serde_json::to_vec(obj) {
                Ok(b) => b,
                Err(e) => return FfiString::from(format!("error:credentials json: {e}")),
            }
        } else {
            // Encrypted path: parse as flat HashMap<String,String> for CredentialEncoder
            let map: std::collections::HashMap<String, String> = match serde_json::from_value(obj.clone())
            {
                Ok(m) => m,
                Err(e) => return FfiString::from(format!("error:credentials must be flat string map for encryption: {e}")),
            };
            match CredentialEncoder::encode(&map, pass) {
                Ok(b) => b,
                Err(e) => return FfiString::from(format!("error:encrypt: {e}")),
            }
        }
    } else if let Some(b64) = v["credentials_b64"].as_str() {
        match BASE64_STANDARD.decode(b64) {
            Ok(b) => b,
            Err(e) => return FfiString::from(format!("error:credentials_b64: {e}")),
        }
    } else {
        Vec::new()
    };

    let account = CloudAccount {
        id,
        provider_type,
        display_name,
        base_url,
        credentials_encrypted,
    };
    match global_library_db().insert_account(&account) {
        Ok(()) => match serde_json::to_string(&account_to_json(&account)) {
            Ok(j) => FfiString::from(j),
            Err(e) => FfiString::from(format!("error:{e}")),
        },
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Delete an account by id. Returns `"ok"` or `"error:…"`.
#[no_mangle]
pub extern "C" fn wdmm_account_delete(id: *const c_char) -> FfiString {
    let id = match ptr_to_str(id) {
        Some(s) => s,
        None => return FfiString::from("error:null id pointer"),
    };
    match global_library_db().delete_account(id) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}


// ── Queue concurrency FFI ─────────────────────────────────────────────────────

/// Return the current queue concurrency limit (1–8).
#[no_mangle]
pub extern "C" fn wdmm_queue_get_concurrency() -> i32 {
    global_queue().concurrency() as i32
}

/// Set the queue concurrency limit. `n` is clamped to `1..=8`.
/// Returns `"ok"`. Caller must free the result.
#[no_mangle]
pub extern "C" fn wdmm_queue_set_concurrency(n: i32) -> FfiString {
    global_queue().set_concurrency(n.max(1) as usize);
    FfiString::ok()
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
        album: v["album"].as_str().map(Into::into),
        album_artist: v["album_artist"].as_str().map(Into::into),
        duration_ms: v["duration_ms"].as_i64().unwrap_or(0),
        genre: v["genre"].as_str().map(Into::into),
        track_number: v["track_number"].as_i64().map(|n| n as i32),
        disc_number: v["disc_number"].as_i64().map(|n| n as i32),
        year: v["year"].as_i64().map(|n| n as i32),
        mime_type: v["mime_type"].as_str().map(Into::into),
        rev: v["rev"].as_i64().unwrap_or(0),
        cover_state: v["cover_state"].as_i64().unwrap_or(0),
    })
}


// ── DMR prefs (independent DB) ───────────────────────────────────────────────

static DMR_PREFS_DB: OnceLock<std::sync::Arc<DmrPrefsDatabase>> = OnceLock::new();

fn global_dmr_prefs_db() -> &'static std::sync::Arc<DmrPrefsDatabase> {
    DMR_PREFS_DB.get_or_init(|| {
        std::sync::Arc::new(
            DmrPrefsDatabase::open(std::path::Path::new(":memory:"))
                .expect("FFI: failed to open in-memory dmr_prefs DB"),
        )
    })
}

/// Initialise the independent DMR prefs DB. Path or `":memory:"`.
#[no_mangle]
pub extern "C" fn wdmm_dmr_prefs_init(path: *const c_char) -> FfiString {
    let p = match ptr_to_str(path) {
        Some(s) => s,
        None => return FfiString::from("error:invalid path pointer"),
    };
    DMR_PREFS_DB.get_or_init(|| {
        std::sync::Arc::new(
            DmrPrefsDatabase::open(std::path::Path::new(p))
                .expect("FFI: failed to open dmr_prefs DB"),
        )
    });
    FfiString::ok()
}

#[no_mangle]
pub extern "C" fn wdmm_dmr_prefs_list() -> FfiString {
    match global_dmr_prefs_db().list() {
        Ok(rows) => match serde_json::to_string(&rows) {
            Ok(j) => FfiString::from(j),
            Err(e) => FfiString::from(format!("error:{e}")),
        },
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

#[no_mangle]
pub extern "C" fn wdmm_dmr_prefs_get(usn: *const c_char) -> FfiString {
    let usn = match ptr_to_str(usn) {
        Some(s) => s,
        None => return FfiString::from("error:null usn"),
    };
    match global_dmr_prefs_db().get(usn) {
        Ok(Some(r)) => match serde_json::to_string(&r) {
            Ok(j) => FfiString::from(j),
            Err(e) => FfiString::from(format!("error:{e}")),
        },
        Ok(None) => FfiString::from("null"),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

#[no_mangle]
pub extern "C" fn wdmm_dmr_prefs_upsert(json: *const c_char) -> FfiString {
    let raw = match ptr_to_str(json) {
        Some(s) => s,
        None => return FfiString::from("error:null json"),
    };
    let mut pref: DmrPrefRow = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => return FfiString::from(format!("error:json: {e}")),
    };
    if pref.usn.trim().is_empty() {
        return FfiString::from("error:missing usn");
    }
    if pref.updated_at.is_empty() {
        pref.updated_at = chrono::Utc::now().to_rfc3339();
    }
    match global_dmr_prefs_db().upsert(&pref) {
        Ok(()) => match serde_json::to_string(&pref) {
            Ok(j) => FfiString::from(j),
            Err(e) => FfiString::from(format!("error:{e}")),
        },
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

#[no_mangle]
pub extern "C" fn wdmm_dmr_prefs_delete(usn: *const c_char) -> FfiString {
    let usn = match ptr_to_str(usn) {
        Some(s) => s,
        None => return FfiString::from("error:null usn"),
    };
    match global_dmr_prefs_db().delete(usn) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

// ── Backup archive FFI (WDMMBK01 / WDMMEN01) ──────────────────────────────────

/// Process-global cover roots set by [`wdmm_covers_configure`].
static COVER_ROOTS: OnceLock<Mutex<Option<wdmm_sync::CoverRoots>>> = OnceLock::new();

fn global_cover_roots() -> &'static Mutex<Option<wdmm_sync::CoverRoots>> {
    COVER_ROOTS.get_or_init(|| Mutex::new(None))
}

/// Configure the cover image directories used during backup create/restore.
///
/// Both paths must be valid UTF-8.  Pass NULL or empty string for either
/// argument to clear the roots (covers will be omitted from subsequent
/// backups and not written during restore).
///
/// Returns `"ok"` on success or `"error:<reason>"` on failure.
#[no_mangle]
pub extern "C" fn wdmm_covers_configure(
    music_covers_dir: *const c_char,
    manga_covers_dir: *const c_char,
) -> FfiString {
    let music = ptr_to_str(music_covers_dir).unwrap_or("").trim().to_string();
    let manga = ptr_to_str(manga_covers_dir).unwrap_or("").trim().to_string();
    let new_roots = if music.is_empty() || manga.is_empty() {
        None
    } else {
        Some(wdmm_sync::CoverRoots {
            music_covers_dir: std::path::PathBuf::from(&music),
            manga_covers_dir: std::path::PathBuf::from(&manga),
        })
    };
    match global_cover_roots().lock() {
        Ok(mut guard) => {
            *guard = new_roots;
            FfiString::ok()
        }
        Err(e) => FfiString::from(format!("error:cover_roots lock poisoned: {e}")),
    }
}

#[no_mangle]
pub extern "C" fn wdmm_backup_create(passphrase: *const c_char, out_path: *const c_char) -> FfiString {
    let pass = ptr_to_str(passphrase).unwrap_or("");
    let path = match ptr_to_str(out_path) {
        Some(s) => s,
        None => return FfiString::from("error:null out_path"),
    };
    let cover_roots = global_cover_roots()
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(std::clone::Clone::clone));
    match wdmm_sync::write_backup_file(
        global_library_db().as_ref(),
        Some(global_dmr_prefs_db().as_ref()),
        pass,
        std::path::Path::new(path),
        cover_roots.as_ref(),
    ) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

#[no_mangle]
pub extern "C" fn wdmm_backup_restore(passphrase: *const c_char, path: *const c_char) -> FfiString {
    let pass = ptr_to_str(passphrase).unwrap_or("");
    let path = match ptr_to_str(path) {
        Some(s) => s,
        None => return FfiString::from("error:null path"),
    };
    let cover_roots = global_cover_roots()
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(std::clone::Clone::clone));
    match wdmm_sync::restore_backup_file(
        global_library_db().as_ref(),
        Some(global_dmr_prefs_db().as_ref()),
        pass,
        std::path::Path::new(path),
        cover_roots.as_ref(),
    ) {
        Ok(msg) => FfiString::from(msg),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

#[no_mangle]
pub extern "C" fn wdmm_backup_upload(
    webdav_url: *const c_char,
    webdav_user: *const c_char,
    webdav_pass: *const c_char,
    remote_dir: *const c_char,
    passphrase: *const c_char,
) -> FfiString {
    let url = match ptr_to_str(webdav_url) {
        Some(s) => s,
        None => return FfiString::from("error:invalid webdav_url"),
    };
    let user = ptr_to_str(webdav_user).unwrap_or("");
    let wpass = ptr_to_str(webdav_pass).unwrap_or("");
    let dir = ptr_to_str(remote_dir).unwrap_or("/WebdavMediaManager/backup/");
    let pass = ptr_to_str(passphrase).unwrap_or("");
    let driver = match wdmm_drivers::WebDavDriver::new(url, user, wpass) {
        Ok(d) => std::sync::Arc::new(d) as std::sync::Arc<dyn wdmm_drivers::WdmmVfs>,
        Err(e) => return FfiString::from(format!("error:driver: {e}")),
    };
    let lib = std::sync::Arc::clone(global_library_db());
    let dmr = Some(std::sync::Arc::clone(global_dmr_prefs_db()));
    let cover_roots = global_cover_roots()
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(std::clone::Clone::clone));
    match rt().block_on(wdmm_sync::backup_upload(lib, dmr, driver, dir, pass, cover_roots.as_ref())) {
        Ok(path) => FfiString::from(path),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

#[no_mangle]
pub extern "C" fn wdmm_backup_list(
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
    let wpass = ptr_to_str(webdav_pass).unwrap_or("");
    let dir = ptr_to_str(remote_dir).unwrap_or("/WebdavMediaManager/backup/");
    let driver = match wdmm_drivers::WebDavDriver::new(url, user, wpass) {
        Ok(d) => std::sync::Arc::new(d) as std::sync::Arc<dyn wdmm_drivers::WdmmVfs>,
        Err(e) => return FfiString::from(format!("error:driver: {e}")),
    };
    match rt().block_on(wdmm_sync::backup_list(driver, dir)) {
        Ok(names) => match serde_json::to_string(&names) {
            Ok(j) => FfiString::from(j),
            Err(e) => FfiString::from(format!("error:{e}")),
        },
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

#[no_mangle]
pub extern "C" fn wdmm_backup_download_restore(
    webdav_url: *const c_char,
    webdav_user: *const c_char,
    webdav_pass: *const c_char,
    remote_file: *const c_char,
    passphrase: *const c_char,
) -> FfiString {
    let url = match ptr_to_str(webdav_url) {
        Some(s) => s,
        None => return FfiString::from("error:invalid webdav_url"),
    };
    let user = ptr_to_str(webdav_user).unwrap_or("");
    let wpass = ptr_to_str(webdav_pass).unwrap_or("");
    let remote = match ptr_to_str(remote_file) {
        Some(s) => s,
        None => return FfiString::from("error:null remote_file"),
    };
    let pass = ptr_to_str(passphrase).unwrap_or("");
    let driver = match wdmm_drivers::WebDavDriver::new(url, user, wpass) {
        Ok(d) => std::sync::Arc::new(d) as std::sync::Arc<dyn wdmm_drivers::WdmmVfs>,
        Err(e) => return FfiString::from(format!("error:driver: {e}")),
    };
    let lib = std::sync::Arc::clone(global_library_db());
    let dmr = Some(std::sync::Arc::clone(global_dmr_prefs_db()));
    let cover_roots = global_cover_roots()
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(std::clone::Clone::clone));
    match rt().block_on(wdmm_sync::backup_download_restore(lib, dmr, driver, remote, pass, cover_roots.as_ref())) {
        Ok(msg) => FfiString::from(msg),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}


// ── Deleted tracks (v6 tombstones) ────────────────────────────────────────────

/// Upsert a deleted track from JSON. Returns `"ok"` or `"error:<message>"`.
///
/// Expected JSON shape: see `DeletedTrack` struct fields.
#[no_mangle]
pub extern "C" fn wdmm_deleted_track_upsert(json: *const c_char) -> FfiString {
    let s = match ptr_to_str(json) {
        Some(s) => s,
        None => return FfiString::from("error:null json pointer"),
    };
    let track: wdmm_storage::DeletedTrack = match serde_json::from_str(s) {
        Ok(t) => t,
        Err(e) => return FfiString::from(format!("error:parse DeletedTrack: {e}")),
    };
    match global_library_db().upsert_deleted_track(&track) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Clear a deleted track by source and path. Returns `"ok"` or `"error:<message>"`.
#[no_mangle]
pub extern "C" fn wdmm_deleted_track_clear(
    source_name: *const c_char,
    remote_path: *const c_char,
) -> FfiString {
    let src = match ptr_to_str(source_name) {
        Some(s) => s,
        None => return FfiString::from("error:null source_name pointer"),
    };
    let path = match ptr_to_str(remote_path) {
        Some(s) => s,
        None => return FfiString::from("error:null remote_path pointer"),
    };
    match global_library_db().clear_deleted_track(src, path) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// List all deleted tracks. Returns JSON array or `"error:<message>"`.
#[no_mangle]
pub extern "C" fn wdmm_deleted_tracks_list() -> FfiString {
    match global_library_db().list_deleted_tracks() {
        Ok(tracks) => match serde_json::to_string(&tracks) {
            Ok(j) => FfiString::from(j),
            Err(e) => FfiString::from(format!("error:serialize: {e}")),
        },
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// List unpushed deleted tracks. Returns JSON array or `"error:<message>"`.
#[no_mangle]
pub extern "C" fn wdmm_deleted_tracks_unpushed() -> FfiString {
    match global_library_db().list_unpushed_deleted_tracks() {
        Ok(tracks) => match serde_json::to_string(&tracks) {
            Ok(j) => FfiString::from(j),
            Err(e) => FfiString::from(format!("error:serialize: {e}")),
        },
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Mark deleted tracks as pushed. `revs_json` is a JSON array of integers.
/// Returns `"ok"` or `"error:<message>"`.
#[no_mangle]
pub extern "C" fn wdmm_deleted_tracks_mark_pushed(revs_json: *const c_char) -> FfiString {
    let s = match ptr_to_str(revs_json) {
        Some(s) => s,
        None => return FfiString::from("error:null revs_json pointer"),
    };
    let revs: Vec<i64> = match serde_json::from_str(s) {
        Ok(r) => r,
        Err(e) => return FfiString::from(format!("error:parse revs: {e}")),
    };
    match global_library_db().mark_deleted_tracks_pushed(&revs) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Purge pushed deleted tracks up to and including the given rev.
/// Returns `"ok"` or `"error:<message>"`.
#[no_mangle]
pub extern "C" fn wdmm_deleted_tracks_purge_up_to(up_to: i64) -> FfiString {
    match global_library_db().purge_deleted_tracks_up_to(up_to) {
        Ok(_) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Get tombstoned remote paths for a source. Returns JSON array of strings
/// or `"error:<message>"`.
#[no_mangle]
pub extern "C" fn wdmm_tombstoned_remote_paths(source_name: *const c_char) -> FfiString {
    let src = match ptr_to_str(source_name) {
        Some(s) => s,
        None => return FfiString::from("error:null source_name pointer"),
    };
    match global_library_db().tombstoned_remote_paths(src) {
        Ok(paths) => match serde_json::to_string(&paths) {
            Ok(j) => FfiString::from(j),
            Err(e) => FfiString::from(format!("error:serialize: {e}")),
        },
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

// ── Sync state (v6 cursors) ───────────────────────────────────────────────────

/// Get sync state by remote_key. Returns JSON object, `"null"`, or `"error:<message>"`.
#[no_mangle]
pub extern "C" fn wdmm_sync_state_get(remote_key: *const c_char) -> FfiString {
    let key = match ptr_to_str(remote_key) {
        Some(s) => s,
        None => return FfiString::from("error:null remote_key pointer"),
    };
    match global_library_db().get_sync_state(key) {
        Ok(Some(state)) => match serde_json::to_string(&state) {
            Ok(j) => FfiString::from(j),
            Err(e) => FfiString::from(format!("error:serialize: {e}")),
        },
        Ok(None) => FfiString::from("null"),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Save sync state from JSON. Returns `"ok"` or `"error:<message>"`.
///
/// Expected JSON shape: see `SyncState` struct fields.
#[no_mangle]
pub extern "C" fn wdmm_sync_state_save(json: *const c_char) -> FfiString {
    let s = match ptr_to_str(json) {
        Some(s) => s,
        None => return FfiString::from("error:null json pointer"),
    };
    let state: wdmm_storage::SyncState = match serde_json::from_str(s) {
        Ok(st) => st,
        Err(e) => return FfiString::from(format!("error:parse SyncState: {e}")),
    };
    match global_library_db().save_sync_state(&state) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// Clear sync state. `prefix` is optional; if null, clears all.
/// Returns `"ok"` or `"error:<message>"`.
#[no_mangle]
pub extern "C" fn wdmm_sync_state_clear(prefix: *const c_char) -> FfiString {
    let pfx = ptr_to_str(prefix);
    match global_library_db().clear_sync_state(pfx) {
        Ok(()) => FfiString::ok(),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
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
    fn webdav_list_invalid_url_errors() {
        let url = CString::new("not-a-url").unwrap();
        let empty = CString::new("").unwrap();
        let path = CString::new("/").unwrap();
        let r = wdmm_webdav_list(url.as_ptr(), empty.as_ptr(), empty.as_ptr(), path.as_ptr());
        assert!(!r.ptr.is_null());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert!(s.starts_with("error:"), "expected error:…, got: {s}");
        wdmm_ffi_free_string(r);
    }

    #[test]
    fn webdav_mkdir_rename_remove_invalid_url_errors() {
        let url = CString::new("not-a-url").unwrap();
        let empty = CString::new("").unwrap();
        let path = CString::new("/x").unwrap();
        for r in [
            wdmm_webdav_mkdir(url.as_ptr(), empty.as_ptr(), empty.as_ptr(), path.as_ptr()),
            wdmm_webdav_rename(url.as_ptr(), empty.as_ptr(), empty.as_ptr(), path.as_ptr(), path.as_ptr()),
            wdmm_webdav_remove(url.as_ptr(), empty.as_ptr(), empty.as_ptr(), path.as_ptr()),
        ] {
            let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
            assert!(s.starts_with("error:"), "expected error:…, got: {s}");
            wdmm_ffi_free_string(r);
        }
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
    fn fts_search_comprehensive() {
        use std::ffi::CString;

        // Upsert tracks with varied metadata
        let tracks = [
            (r#"{"id":"ft1","title":"Yesterday","artist":"The Beatles","album":"Help!","album_artist":"The Beatles","remote_path":"/m/1.mp3","account_id":"acc","size":1000}"#),
            (r#"{"id":"ft2","title":"Tomorrow Never Knows","artist":"The Beatles","album":"Revolver","album_artist":"The Beatles","remote_path":"/m/2.mp3","account_id":"acc","size":1000}"#),
            (r#"{"id":"ft3","title":"Imagine","artist":"John Lennon","album":"Imagine","album_artist":"John Lennon","remote_path":"/m/3.mp3","account_id":"acc","size":1000}"#),
            (r#"{"id":"ft4","title":"Abbey Road Medley","artist":"Various","album":"Covers","album_artist":"Various","remote_path":"/m/4.mp3","account_id":"acc","size":1000}"#),
        ];

        for track_json in &tracks {
            let json = CString::new(*track_json).unwrap();
            let r = wdmm_track_upsert(json.as_ptr());
            let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
            assert_eq!(s, "ok", "upsert failed: {s}");
            wdmm_ffi_free_string(r);
        }

        // Search by title
        let q = CString::new("yesterday").unwrap();
        let r = wdmm_track_search(q.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        let arr: Vec<serde_json::Value> = serde_json::from_str(s).unwrap();
        assert_eq!(arr.len(), 1, "should find 1 track with 'yesterday' in title");
        assert_eq!(arr[0]["id"], "ft1");
        wdmm_ffi_free_string(r);

        // Search by artist
        let q = CString::new("beatles").unwrap();
        let r = wdmm_track_search(q.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        let arr: Vec<serde_json::Value> = serde_json::from_str(s).unwrap();
        assert_eq!(arr.len(), 2, "should find 2 tracks with 'beatles' in artist");
        wdmm_ffi_free_string(r);

        // Search by album
        let q = CString::new("imagine").unwrap();
        let r = wdmm_track_search(q.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        let arr: Vec<serde_json::Value> = serde_json::from_str(s).unwrap();
        assert_eq!(arr.len(), 1, "should find 1 track with 'imagine' (title or album)");
        wdmm_ffi_free_string(r);

        // Search by album_artist
        let q = CString::new("lennon").unwrap();
        let r = wdmm_track_search(q.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        let arr: Vec<serde_json::Value> = serde_json::from_str(s).unwrap();
        assert_eq!(arr.len(), 1, "should find 1 track with 'lennon'");
        assert_eq!(arr[0]["id"], "ft3");
        wdmm_ffi_free_string(r);

        // Multi-token search
        let q = CString::new("the beatles").unwrap();
        let r = wdmm_track_search(q.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        let arr: Vec<serde_json::Value> = serde_json::from_str(s).unwrap();
        assert_eq!(arr.len(), 2, "should find both Beatles tracks");
        wdmm_ffi_free_string(r);

        // Cleanup
        for id_str in ["ft1", "ft2", "ft3", "ft4"] {
            let id = CString::new(id_str).unwrap();
            let r = wdmm_track_delete(id.as_ptr());
            wdmm_ffi_free_string(r);
        }
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
    fn compare_sqflite_empty_old_db() {
        // Create a real temp old DB with the Android-schema tables, empty.
        let tmp = std::env::temp_dir().join(format!(
            "wdmm_ffi_compare_{}.db",
            std::process::id()
        ));
        {
            let conn = rusqlite::Connection::open(&tmp).expect("create old db");
            conn.execute_batch(
                r#"
                CREATE TABLE tracks (
                    music_id TEXT PRIMARY KEY, source_name TEXT NOT NULL,
                    remote_path TEXT NOT NULL, title TEXT NOT NULL, duration_ms INTEGER
                );
                CREATE TABLE deleted_tracks (
                    source_name TEXT NOT NULL, remote_path TEXT NOT NULL,
                    rev INTEGER NOT NULL, deleted_at TEXT NOT NULL,
                    pushed INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY (source_name, remote_path)
                );
                CREATE TABLE sync_state (
                    remote_key TEXT PRIMARY KEY, last_seq INTEGER NOT NULL DEFAULT 0,
                    base_up_to INTEGER NOT NULL DEFAULT 0,
                    parts TEXT NOT NULL DEFAULT '{}', updated_at TEXT NOT NULL
                );
                "#,
            )
            .expect("seed old db");
        }

        let path = CString::new(tmp.to_string_lossy().to_string()).unwrap();
        let r = wdmm_library_compare_with_sqflite(path.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert!(!s.starts_with("error:"), "compare failed: {s}");
        let v: serde_json::Value = serde_json::from_str(s).expect("valid JSON report");
        assert!(v.get("ok").is_some(), "missing ok: {s}");
        assert!(v.get("tracks").is_some(), "missing tracks: {s}");
        assert!(v.get("deleted_tracks").is_some(), "missing deleted_tracks: {s}");
        assert!(v.get("sync_state").is_some(), "missing sync_state: {s}");
        wdmm_ffi_free_string(r);

        let _ = std::fs::remove_file(&tmp);
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
            _request_id: u64, // sync fill — no set_result needed
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

    /// Simulates the Dart async listener path: the C callback returns `0`
    /// with `*out_buf` null and deposits its `request_id`; a helper thread
    /// then allocates a Rust-heap buffer, copies the bytes, and calls
    /// `wdmm_proxy_stream_set_result`. The HTTP GET through the proxy must
    /// return those bytes.
    #[test]
    fn proxy_register_stream_async_set_result() {
        let _guard = proxy_lock().lock().unwrap();

        // Bytes the "Dart" reply thread will deliver.
        static ADATA: &[u8] = b"async-set-result-bytes";

        // One-shot state: the C callback deposits req_id here so the reply
        // thread can call set_result with the right id.
        struct AsyncSlot {
            req_id: Mutex<Option<u64>>,
            ready: Condvar,
        }
        static SLOT: OnceLock<AsyncSlot> = OnceLock::new();
        fn slot() -> &'static AsyncSlot {
            SLOT.get_or_init(|| AsyncSlot {
                req_id: Mutex::new(None),
                ready: Condvar::new(),
            })
        }
        // Reset for this test run.
        *slot().req_id.lock().unwrap() = None;

        // Callback: returns 0, leaves out_buf null → triggers the async path.
        unsafe extern "C" fn async_cb(
            _user_data: *mut std::os::raw::c_void,
            _start: i64,
            _end: i64,
            _out_buf: *mut *mut u8,
            _out_len: *mut usize,
            request_id: u64,
        ) -> i32 {
            *slot().req_id.lock().unwrap() = Some(request_id);
            slot().ready.notify_one();
            0 // success intent — bytes not ready yet (async path)
        }

        // Start the proxy on an OS-assigned port.
        let port_str = wdmm_proxy_start(0);
        let port_s = unsafe { CStr::from_ptr(port_str.ptr) }.to_str().unwrap().to_string();
        wdmm_ffi_free_string(port_str);
        let port: u16 = port_s.parse().expect("proxy_start returns a port");

        // Register the stream with the async callback.
        let name = CString::new("async.bin").unwrap();
        let r = unsafe {
            wdmm_proxy_register_stream(
                ADATA.len() as u64,
                name.as_ptr(),
                async_cb,
                std::ptr::null_mut(),
            )
        };
        let body = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap().to_string();
        wdmm_ffi_free_string(r);

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

        // Spawn a reply thread that waits for req_id then calls set_result.
        let reply = std::thread::spawn(|| {
            let guard = slot().req_id.lock().unwrap();
            let (guard, tout) = slot()
                .ready
                .wait_timeout_while(guard, std::time::Duration::from_secs(5), |opt| {
                    opt.is_none()
                })
                .unwrap();
            assert!(!tout.timed_out(), "async_cb not invoked within 5 s");
            let req_id = guard.unwrap();
            // Allocate a Rust-heap buf and fill it with ADATA.
            let buf = wdmm_proxy_stream_alloc_buf(ADATA.len());
            assert!(!buf.is_null(), "alloc_buf returned null");
            unsafe { std::ptr::copy_nonoverlapping(ADATA.as_ptr(), buf, ADATA.len()) };
            wdmm_proxy_stream_set_result(req_id, buf, ADATA.len(), 0);
        });

        // GET — blocks invoke_c_callback until the reply thread calls set_result.
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

        reply.join().expect("reply thread panicked");
        assert_eq!(got.as_ref(), ADATA, "async stream bytes mismatch");

        // Cleanup.
        let tok = CString::new(token).unwrap();
        let r = wdmm_proxy_unregister(tok.as_ptr());
        wdmm_ffi_free_string(r);

        let r = wdmm_proxy_stop();
        wdmm_ffi_free_string(r);
        assert_eq!(wdmm_proxy_port(), -1);
    }

    /// Verifies the async error path: callback returns 0 + null out_buf, the
    /// reply thread calls set_result with status = -1. The proxy must surface
    /// an IO error to the HTTP client (no hang, no spurious bytes).
    #[test]
    fn proxy_register_stream_async_error_status() {
        let _guard = proxy_lock().lock().unwrap();

        struct AsyncSlot {
            req_id: Mutex<Option<u64>>,
            ready: Condvar,
        }
        static SLOT: OnceLock<AsyncSlot> = OnceLock::new();
        fn slot() -> &'static AsyncSlot {
            SLOT.get_or_init(|| AsyncSlot {
                req_id: Mutex::new(None),
                ready: Condvar::new(),
            })
        }
        *slot().req_id.lock().unwrap() = None;

        unsafe extern "C" fn async_err_cb(
            _user_data: *mut std::os::raw::c_void,
            _start: i64,
            _end: i64,
            _out_buf: *mut *mut u8,
            _out_len: *mut usize,
            request_id: u64,
        ) -> i32 {
            *slot().req_id.lock().unwrap() = Some(request_id);
            slot().ready.notify_one();
            0 // async intent
        }

        let port_str = wdmm_proxy_start(0);
        let port_s = unsafe { CStr::from_ptr(port_str.ptr) }.to_str().unwrap().to_string();
        wdmm_ffi_free_string(port_str);
        let port: u16 = port_s.parse().expect("proxy_start returns a port");

        let name = CString::new("async_err.bin").unwrap();
        let r = unsafe {
            wdmm_proxy_register_stream(
                16,
                name.as_ptr(),
                async_err_cb,
                std::ptr::null_mut(),
            )
        };
        let body = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap().to_string();
        wdmm_ffi_free_string(r);
        let v: serde_json::Value = serde_json::from_str(&body).expect("JSON");
        let path_template = v["path_template"].as_str().unwrap().to_string();
        let token = v["token"].as_str().unwrap().to_string();

        // Reply thread signals an error.
        let reply = std::thread::spawn(|| {
            let guard = slot().req_id.lock().unwrap();
            let (guard, tout) = slot()
                .ready
                .wait_timeout_while(guard, std::time::Duration::from_secs(5), |opt| {
                    opt.is_none()
                })
                .unwrap();
            assert!(!tout.timed_out());
            let req_id = guard.unwrap();
            wdmm_proxy_stream_set_result(req_id, std::ptr::null_mut(), 0, -1);
        });

        // GET — the proxy should surface an IO error (connection reset / partial
        // body). Accept either a send error or an empty/non-matching body.
        let url = format!("http://127.0.0.1:{port}{path_template}");
        let outcome = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let resp = reqwest::Client::new().get(&url).send().await;
                match resp {
                    Err(_) => Ok(()), // network-level error is acceptable
                    Ok(r) => match r.bytes().await {
                        Err(_) => Ok(()),
                        Ok(b) => Err(b),
                    },
                }
            });
        reply.join().expect("reply thread panicked");
        // The bytes (if any) must NOT be ADATA-like garbage; an error path
        // surfaces no valid payload. We accept network error or empty body.
        match outcome {
            Ok(()) => { /* error surfaced — expected */ }
            Err(b) => {
                assert!(
                    b.is_empty(),
                    "async error path must not deliver valid bytes, got {}B",
                    b.len()
                );
            }
        }

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

    // ── User folders + playlist FFI ──────────────────────────────────────────

    #[test]
    fn user_folder_ffi_crud() {
        // Create two root folders (UUID ids auto-generated).
        let j1 = CString::new(r#"{"name":"FFI Music","sort_order":0}"#).unwrap();
        let r = wdmm_user_folder_create(j1.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert!(!s.starts_with("error:"), "create1 failed: {s}");
        let v: serde_json::Value = serde_json::from_str(s).unwrap();
        let f1_id = v["id"].as_str().unwrap().to_string();
        wdmm_ffi_free_string(r);

        let j2 = CString::new(r#"{"name":"FFI Playlists","parent_id":null,"sort_order":1}"#).unwrap();
        let r = wdmm_user_folder_create(j2.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        let v: serde_json::Value = serde_json::from_str(s).unwrap();
        let f2_id = v["id"].as_str().unwrap().to_string();
        wdmm_ffi_free_string(r);

        // List root children via null pointer → at least our two new folders.
        let r = wdmm_user_folder_list_children(std::ptr::null());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        let arr: serde_json::Value = serde_json::from_str(s).unwrap();
        let names: Vec<&str> = arr
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|f| f["name"].as_str())
            .collect();
        assert!(names.contains(&"FFI Music"));
        assert!(names.contains(&"FFI Playlists"));
        wdmm_ffi_free_string(r);

        // "root" sentinel yields the same set.
        let root = CString::new("root").unwrap();
        let r = wdmm_user_folder_list_children(root.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        let arr: serde_json::Value = serde_json::from_str(s).unwrap();
        assert!(
            arr.as_array()
                .unwrap()
                .iter()
                .any(|f| f["id"].as_str() == Some(f1_id.as_str())),
            "f1 should be a root child"
        );
        wdmm_ffi_free_string(r);

        // Create child under f1.
        let child_json = CString::new(format!(
            r#"{{"name":"FFI Rock","parent_id":"{}"}}"#,
            f1_id
        ))
        .unwrap();
        let r = wdmm_user_folder_create(child_json.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        let v: serde_json::Value = serde_json::from_str(s).unwrap();
        let child_id = v["id"].as_str().unwrap().to_string();
        wdmm_ffi_free_string(r);

        // Add an item to the virtual root (folder_id = "root").
        let item_root = CString::new(
            r#"{"folder_id":"root","ref_kind":"track","ref_key":"ffi-track-1","sort_order":0}"#,
        )
        .unwrap();
        let r = wdmm_user_folder_add_item(item_root.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok");
        wdmm_ffi_free_string(r);

        // Add an item to the child folder.
        let item_child = CString::new(format!(
            r#"{{"folder_id":"{}","ref_kind":"album","ref_key":"ffi-alb-1","sort_order":0}}"#,
            child_id
        ))
        .unwrap();
        let r = wdmm_user_folder_add_item(item_child.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok");
        wdmm_ffi_free_string(r);

        // List root items — contains the track we added.
        let root2 = CString::new("root").unwrap();
        let r = wdmm_user_folder_list_items(root2.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        let arr: serde_json::Value = serde_json::from_str(s).unwrap();
        assert!(
            arr.as_array()
                .unwrap()
                .iter()
                .any(|it| it["ref_key"].as_str() == Some("ffi-track-1")),
            "root should contain ffi-track-1 item"
        );
        wdmm_ffi_free_string(r);

        // Rename f1.
        let id1 = CString::new(f1_id.clone()).unwrap();
        let new_name = CString::new("FFI Musik").unwrap();
        let r = wdmm_user_folder_rename(id1.as_ptr(), new_name.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok");
        wdmm_ffi_free_string(r);

        // Move child to virtual root (null pointer).
        let cid = CString::new(child_id.clone()).unwrap();
        let r = wdmm_user_folder_move(cid.as_ptr(), std::ptr::null());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok");
        wdmm_ffi_free_string(r);

        // Recursive delete f1 (no children now).
        let r = wdmm_user_folder_delete(id1.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok");
        wdmm_ffi_free_string(r);

        // Remove root item.
        let root3 = CString::new("root").unwrap();
        let kind = CString::new("track").unwrap();
        let key = CString::new("ffi-track-1").unwrap();
        let r = wdmm_user_folder_remove_item(root3.as_ptr(), kind.as_ptr(), key.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok");
        wdmm_ffi_free_string(r);

        // Clean up remaining folders + child.
        let cid2 = CString::new(child_id).unwrap();
        let r = wdmm_user_folder_delete(cid2.as_ptr());
        wdmm_ffi_free_string(r);
        let f2 = CString::new(f2_id).unwrap();
        let r = wdmm_user_folder_delete(f2.as_ptr());
        wdmm_ffi_free_string(r);
    }

    #[test]
    fn playlist_ffi_crud() {
        // Create a playlist (id auto-generated).
        let j = CString::new(r#"{"name":"FFI Playlist CRUD"}"#).unwrap();
        let r = wdmm_playlist_create(j.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert!(!s.starts_with("error:"), "create failed: {s}");
        let v: serde_json::Value = serde_json::from_str(s).unwrap();
        let pl_id = v["id"].as_str().unwrap().to_string();
        assert_eq!(v["name"].as_str(), Some("FFI Playlist CRUD"));
        wdmm_ffi_free_string(r);

        let id = CString::new(pl_id.clone()).unwrap();

        // Set entries — replace-all; positions assigned by array order.
        let entries = CString::new(
            r#"[
                {"music_id":"aa","source_name":"nas","remote_path":"/m/a.mp3","title":"A","duration_ms":1000},
                {"music_id":"bb","source_name":"nas","remote_path":"/m/b.mp3","title":"B","duration_ms":2000}
            ]"#,
        )
        .unwrap();
        let r = wdmm_playlist_set_entries(id.as_ptr(), entries.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok");
        wdmm_ffi_free_string(r);

        // List entries — verify order and fields.
        let r = wdmm_playlist_list_entries(id.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        let arr: serde_json::Value = serde_json::from_str(s).unwrap();
        let items = arr.as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["position"], 0);
        assert_eq!(items[0]["title"], "A");
        assert_eq!(items[1]["position"], 1);
        assert_eq!(items[1]["title"], "B");
        wdmm_ffi_free_string(r);

        // Playlist appears in list.
        let r = wdmm_playlist_list();
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        let arr: serde_json::Value = serde_json::from_str(s).unwrap();
        assert!(
            arr.as_array()
                .unwrap()
                .iter()
                .any(|p| p["id"].as_str() == Some(pl_id.as_str())),
            "playlist should be in list"
        );
        wdmm_ffi_free_string(r);

        // Replace-all: single entry replaces the previous two.
        let entries2 = CString::new(
            r#"[
                {"music_id":"cc","source_name":"nas","remote_path":"/m/c.mp3","title":"C","duration_ms":3000}
            ]"#,
        )
        .unwrap();
        let r = wdmm_playlist_set_entries(id.as_ptr(), entries2.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok");
        wdmm_ffi_free_string(r);
        let r = wdmm_playlist_list_entries(id.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        let arr: serde_json::Value = serde_json::from_str(s).unwrap();
        let items = arr.as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["title"], "C");
        wdmm_ffi_free_string(r);

        // Delete playlist.
        let r = wdmm_playlist_delete(id.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok");
        wdmm_ffi_free_string(r);

        // Entries gone (cascade).
        let r = wdmm_playlist_list_entries(id.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        let arr: serde_json::Value = serde_json::from_str(s).unwrap();
        assert_eq!(arr.as_array().unwrap().len(), 0);
        wdmm_ffi_free_string(r);
    }

    #[test]
    fn kv_ffi_roundtrip() {
        use std::ffi::CString;
        let key = CString::new("__test_kv_ffi_key").unwrap();
        let val = CString::new("hello-kv").unwrap();

        // set
        let r = wdmm_kv_set(key.as_ptr(), val.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok");
        wdmm_ffi_free_string(r);

        // get — returns the plain string value
        let r = wdmm_kv_get(key.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "hello-kv");
        wdmm_ffi_free_string(r);

        // delete
        let r = wdmm_kv_delete(key.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok");
        wdmm_ffi_free_string(r);

        // get after delete → "null"
        let r = wdmm_kv_get(key.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "null");
        wdmm_ffi_free_string(r);
    }

    #[test]
    fn settings_remember_last_path() {
        use std::ffi::CString;
        // enable
        let r = wdmm_settings_set_network_remember_last_path(1);
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok");
        wdmm_ffi_free_string(r);

        let r = wdmm_settings_get_network_remember_last_path();
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "true");
        wdmm_ffi_free_string(r);

        // disable
        let r = wdmm_settings_set_network_remember_last_path(0);
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok");
        wdmm_ffi_free_string(r);

        let r = wdmm_settings_get_network_remember_last_path();
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "false");
        wdmm_ffi_free_string(r);

        // Strip legacy key on set: ensure network_last_path is gone from KV.
        let legacy = CString::new("network_last_path").unwrap();
        let _ = wdmm_kv_set(legacy.as_ptr(), CString::new("stale").unwrap().as_ptr());
        let r = wdmm_settings_set_network_remember_last_path(1);
        wdmm_ffi_free_string(r);
        let r = wdmm_kv_get(legacy.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "null", "legacy network_last_path must be stripped on set");
        wdmm_ffi_free_string(r);

        // Reset to a clean state for other tests.
        let r = wdmm_settings_set_network_remember_last_path(0);
        wdmm_ffi_free_string(r);
    }

    #[test]
    fn settings_session_last_path_not_persisted() {
        use std::ffi::CString;
        let new_path = CString::new("/smb/server/share").unwrap();
        let r = wdmm_settings_set_network_last_path(new_path.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok");
        wdmm_ffi_free_string(r);

        let r = wdmm_settings_get_network_last_path();
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "/smb/server/share");
        wdmm_ffi_free_string(r);

        // Confirm the value is NOT persisted to the KV store.
        let kv_key = CString::new("network_last_path").unwrap();
        let r = wdmm_kv_get(kv_key.as_ptr());
        let kv_val = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(kv_val, "null", "session path must not be persisted to KV");
        wdmm_ffi_free_string(r);
    }

    #[test]
    fn user_folder_set_items_ffi() {
        use std::ffi::CString;

        // Create a dedicated folder to avoid contaminating the shared "root".
        let create_json = CString::new(
            r#"{"name":"SetItemsTestFolder","parent_id":null,"sort_order":99}"#,
        )
        .unwrap();
        let r = wdmm_user_folder_create(create_json.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert!(!s.starts_with("error:"), "create failed: {s}");
        let created: serde_json::Value = serde_json::from_str(s).expect("create must return JSON");
        let fid_str = created["id"].as_str().unwrap().to_string();
        wdmm_ffi_free_string(r);
        let fid = CString::new(fid_str.clone()).unwrap();

        // Bulk-set: genre + manga + mangaSeries items
        let json = CString::new(
            r#"[
                {"ref_kind":"genre","ref_key":"ffi-genre-1","sort_order":0},
                {"ref_kind":"manga","ref_key":"ffi-manga-1","sort_order":1},
                {"ref_kind":"mangaSeries","ref_key":"ffi-ms-1","sort_order":2}
            ]"#,
        )
        .unwrap();
        let r = wdmm_user_folder_set_items(fid.as_ptr(), json.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok", "set_items failed: {s}");
        wdmm_ffi_free_string(r);

        // List and verify all three expanded ref_kinds are present.
        let r = wdmm_user_folder_list_items(fid.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        let arr: serde_json::Value = serde_json::from_str(s).unwrap();
        let items = arr.as_array().unwrap();
        assert!(items.iter().any(|it| it["ref_kind"] == "genre" && it["ref_key"] == "ffi-genre-1"),
            "genre item missing");
        assert!(items.iter().any(|it| it["ref_kind"] == "manga" && it["ref_key"] == "ffi-manga-1"),
            "manga item missing");
        assert!(items.iter().any(|it| it["ref_kind"] == "mangaSeries" && it["ref_key"] == "ffi-ms-1"),
            "mangaSeries item missing");
        wdmm_ffi_free_string(r);

        // Bulk-replace: only one album item — previous three must be gone.
        let json2 = CString::new(
            r#"[{"ref_kind":"album","ref_key":"ffi-album-set","sort_order":0}]"#,
        )
        .unwrap();
        let r = wdmm_user_folder_set_items(fid.as_ptr(), json2.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok");
        wdmm_ffi_free_string(r);

        let r = wdmm_user_folder_list_items(fid.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        let arr: serde_json::Value = serde_json::from_str(s).unwrap();
        let items2 = arr.as_array().unwrap();
        assert!(
            !items2.iter().any(|it| it["ref_key"] == "ffi-genre-1"),
            "bulk replace did not clear previous items"
        );
        assert_eq!(items2.len(), 1, "expected exactly 1 item after replace");
        assert_eq!(items2[0]["ref_kind"], "album");
        wdmm_ffi_free_string(r);

        // Cleanup: delete the test folder (cascades to its items).
        let r = wdmm_user_folder_delete(fid.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok");
        wdmm_ffi_free_string(r);
    }

    // ── replace_snapshot FFI ─────────────────────────────────────────────────

    #[test]
    fn user_folder_replace_snapshot_ffi() {
        use std::ffi::CString;

        // ── First snapshot: two folders (unix + ISO8601 timestamps), two items ─
        let snap1 = serde_json::json!({
            "folders": [
                {
                    "id": "snap-f1",
                    "parent_id": null,
                    "name": "SnapMusic",
                    "sort_order": 0,
                    "created_at": 1_700_000_000_i64
                },
                {
                    "id": "snap-f2",
                    "parent_id": "snap-f1",
                    "name": "SnapRock",
                    "sort_order": 1,
                    "created_at": "2023-11-15T00:00:00Z"
                }
            ],
            "items": [
                {
                    "folder_id": "root",
                    "ref_kind": "track",
                    "ref_key": "snap-track-1",
                    "sort_order": 0,
                    "added_at": 1_700_000_100_i64
                },
                {
                    "folder_id": "snap-f2",
                    "ref_kind": "album",
                    "ref_key": "snap-alb-1",
                    "sort_order": 0,
                    "added_at": "2023-11-15T01:00:00Z"
                }
            ]
        }).to_string();
        let c1 = CString::new(snap1).unwrap();
        let r = wdmm_user_folder_replace_snapshot(c1.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok", "snapshot1 failed: {s}");
        wdmm_ffi_free_string(r);

        // Verify snap-f2 is a child of snap-f1.
        let fid1 = CString::new("snap-f1").unwrap();
        let r = wdmm_user_folder_list_children(fid1.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        let arr: serde_json::Value = serde_json::from_str(s).unwrap();
        let children = arr.as_array().unwrap();
        assert_eq!(children.len(), 1, "snap-f1 should have 1 child");
        assert_eq!(children[0]["id"], "snap-f2");
        assert_eq!(children[0]["name"], "SnapRock");
        // ISO8601 timestamp: 2023-11-15T00:00:00Z = 1700006400
        assert_eq!(
            children[0]["created_at"].as_i64().unwrap(),
            1_700_006_400_i64,
            "ISO8601 timestamp should round-trip as unix seconds"
        );
        wdmm_ffi_free_string(r);

        // Verify root item is present.
        let root_c = CString::new("root").unwrap();
        let r = wdmm_user_folder_list_items(root_c.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        let arr: serde_json::Value = serde_json::from_str(s).unwrap();
        assert!(
            arr.as_array().unwrap().iter().any(|it| it["ref_key"] == "snap-track-1"),
            "root should contain snap-track-1"
        );
        wdmm_ffi_free_string(r);

        // Verify snap-f2 item.
        let fid2 = CString::new("snap-f2").unwrap();
        let r = wdmm_user_folder_list_items(fid2.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        let arr: serde_json::Value = serde_json::from_str(s).unwrap();
        assert!(
            arr.as_array().unwrap().iter().any(|it| it["ref_key"] == "snap-alb-1"),
            "snap-f2 should contain snap-alb-1"
        );
        wdmm_ffi_free_string(r);

        // ── Second snapshot (different ids) wipes first ───────────────────────
        let snap2 = serde_json::json!({
            "folders": [
                {"id": "snap-new-1", "parent_id": null, "name": "NewFolder",
                 "sort_order": 0, "created_at": 1_700_020_000_i64}
            ],
            "items": []
        }).to_string();
        let c2 = CString::new(snap2).unwrap();
        let r = wdmm_user_folder_replace_snapshot(c2.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok", "snapshot2 failed: {s}");
        wdmm_ffi_free_string(r);

        // Old snap-f1 must be gone.
        let r = wdmm_user_folder_list_children(fid1.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        let arr: serde_json::Value = serde_json::from_str(s).unwrap();
        assert!(
            arr.as_array().unwrap().is_empty(),
            "snap-f1 children must be empty after second snapshot"
        );
        wdmm_ffi_free_string(r);

        // Root must have no track item from snapshot 1.
        let root_c2 = CString::new("root").unwrap();
        let r = wdmm_user_folder_list_items(root_c2.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        let arr: serde_json::Value = serde_json::from_str(s).unwrap();
        assert!(
            !arr.as_array().unwrap().iter().any(|it| it["ref_key"] == "snap-track-1"),
            "snap-track-1 must be gone after second snapshot"
        );
        wdmm_ffi_free_string(r);

        // ── Wipe snapshot (empty arrays) ─────────────────────────────────────
        let snap_empty = r#"{"folders":[],"items":[]}"#;
        let c3 = CString::new(snap_empty).unwrap();
        let r = wdmm_user_folder_replace_snapshot(c3.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok", "wipe snapshot failed: {s}");
        wdmm_ffi_free_string(r);

        // new-1 must now be gone.
        let new1 = CString::new("snap-new-1").unwrap();
        let r = wdmm_user_folder_list_children(new1.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        let arr: serde_json::Value = serde_json::from_str(s).unwrap();
        assert!(
            arr.as_array().unwrap().is_empty(),
            "snap-new-1 must have no children after wipe"
        );
        wdmm_ffi_free_string(r);
    }

    // ── Manga FFI smoke tests ────────────────────────────────────────────────

    #[test]
    fn manga_ffi_smoke() {
        use std::ffi::CString;

        // ── list (empty) ──────────────────────────────────────────────────────
        let r = wdmm_manga_list();
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert!(
            !s.starts_with("error:"),
            "wdmm_manga_list failed: {s}"
        );
        let arr: serde_json::Value = serde_json::from_str(s).expect("list must be JSON");
        assert!(arr.is_array(), "list must be an array");
        wdmm_ffi_free_string(r);

        // ── upsert ────────────────────────────────────────────────────────────
        let manga_id = "ffi-manga-1";
        let json = serde_json::json!({
            "id": manga_id,
            "source_name": "nas",
            "remote_path": "/manga/test.cbz",
            "title": "FFI Test Manga",
            "author": "Author",
            "series": "FFI Series",
            "volume": "1",
            "chapter_number": "1",
            "page_count": 2,
            "cover_state": 0,
            "format": "cbz",
            "remote_missing": 0,
            "page_turn": "ltr",
            "rev": 0,
            "updated_at": "2026-01-01T00:00:00+00:00"
        })
        .to_string();
        let cjson = CString::new(json).unwrap();
        let r = wdmm_manga_upsert(cjson.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok", "wdmm_manga_upsert failed: {s}");
        wdmm_ffi_free_string(r);

        // ── get ───────────────────────────────────────────────────────────────
        let cid = CString::new(manga_id).unwrap();
        let r = wdmm_manga_get(cid.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert!(!s.starts_with("error:"), "wdmm_manga_get failed: {s}");
        assert_ne!(s, "null", "manga must exist after upsert");
        let got: serde_json::Value = serde_json::from_str(s).unwrap();
        assert_eq!(got["title"], "FFI Test Manga");
        wdmm_ffi_free_string(r);

        // ── list (now 1) ──────────────────────────────────────────────────────
        let r = wdmm_manga_list();
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        let arr: serde_json::Value = serde_json::from_str(s).unwrap();
        assert!(arr.as_array().unwrap().len() >= 1);
        wdmm_ffi_free_string(r);

        // ── set_pages ─────────────────────────────────────────────────────────
        let pages_json = serde_json::json!([
            {"manga_id": manga_id, "page_index": 0, "rel_path": "p0.jpg"},
            {"manga_id": manga_id, "page_index": 1, "rel_path": "p1.jpg"}
        ])
        .to_string();
        let cmid = CString::new(manga_id).unwrap();
        let cpages = CString::new(pages_json).unwrap();
        let r = wdmm_manga_set_pages(cmid.as_ptr(), cpages.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok", "wdmm_manga_set_pages failed: {s}");
        wdmm_ffi_free_string(r);

        // ── list_pages ────────────────────────────────────────────────────────
        let cmid = CString::new(manga_id).unwrap();
        let r = wdmm_manga_list_pages(cmid.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert!(!s.starts_with("error:"), "wdmm_manga_list_pages failed: {s}");
        let pages_arr: serde_json::Value = serde_json::from_str(s).unwrap();
        assert_eq!(pages_arr.as_array().unwrap().len(), 2);
        wdmm_ffi_free_string(r);

        // ── set_progress ──────────────────────────────────────────────────────
        let cmid = CString::new(manga_id).unwrap();
        let r = wdmm_manga_set_progress(cmid.as_ptr(), 1);
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok", "wdmm_manga_set_progress failed: {s}");
        wdmm_ffi_free_string(r);

        // ── get_progress ──────────────────────────────────────────────────────
        let cmid = CString::new(manga_id).unwrap();
        let r = wdmm_manga_get_progress(cmid.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert!(!s.starts_with("error:"), "wdmm_manga_get_progress failed: {s}");
        assert_ne!(s, "null", "progress must exist after set");
        let prog: serde_json::Value = serde_json::from_str(s).unwrap();
        assert_eq!(prog["page_index"], 1);
        wdmm_ffi_free_string(r);

        // ── list_progress ─────────────────────────────────────────────────────
        let r = wdmm_manga_list_progress();
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert!(!s.starts_with("error:"), "wdmm_manga_list_progress failed: {s}");
        let prog_arr: serde_json::Value = serde_json::from_str(s).unwrap();
        assert!(prog_arr.as_array().unwrap().len() >= 1);
        wdmm_ffi_free_string(r);

        // ── list_series ───────────────────────────────────────────────────────
        let r = wdmm_manga_list_series();
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert!(!s.starts_with("error:"), "wdmm_manga_list_series failed: {s}");
        let series_arr: serde_json::Value = serde_json::from_str(s).unwrap();
        let series = series_arr.as_array().unwrap();
        assert!(
            series.iter().any(|it| it["title"] == "FFI Series"),
            "series tag 'FFI Series' missing from: {series_arr}"
        );
        wdmm_ffi_free_string(r);

        // ── delete ────────────────────────────────────────────────────────────
        let cid = CString::new(manga_id).unwrap();
        let r = wdmm_manga_delete(cid.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok", "wdmm_manga_delete failed: {s}");
        wdmm_ffi_free_string(r);

        // ── get after delete returns null ─────────────────────────────────────
        let cid = CString::new(manga_id).unwrap();
        let r = wdmm_manga_get(cid.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "null", "manga must be gone after delete");
        wdmm_ffi_free_string(r);
    }

    #[test]
    fn manga_delete_not_found() {
        let cid = std::ffi::CString::new("does-not-exist").unwrap();
        let r = wdmm_manga_delete(cid.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "not_found");
        wdmm_ffi_free_string(r);
    }


    // ── Account FFI tests ────────────────────────────────────────────────────

    #[test]
    fn account_ffi_crud_roundtrip() {
        let upsert = CString::new(
            r#"{"provider_type":"webdav","display_name":"NAS","base_url":"https://nas.local/dav","credentials":{"username":"u","password":"p","remote_dir":"/wdmm"}}"#,
        )
        .unwrap();
        let r = wdmm_account_upsert(upsert.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap().to_string();
        wdmm_ffi_free_string(r);
        assert!(!s.starts_with("error:"), "upsert failed: {s}");
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        let id = v["id"].as_str().unwrap().to_string();
        assert_eq!(v["display_name"], "NAS");
        assert!(v["credentials_b64"].as_str().unwrap().len() > 0);

        let r = wdmm_account_list();
        let list_s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap().to_string();
        wdmm_ffi_free_string(r);
        assert!(!list_s.starts_with("error:"), "list failed: {list_s}");
        let arr: Vec<serde_json::Value> = serde_json::from_str(&list_s).unwrap();
        assert!(arr.iter().any(|a| a["id"] == id));

        let cid = CString::new(id.clone()).unwrap();
        let r = wdmm_account_get(cid.as_ptr());
        let get_s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap().to_string();
        wdmm_ffi_free_string(r);
        assert!(!get_s.starts_with("error:") && get_s != "null", "get failed: {get_s}");
        let got: serde_json::Value = serde_json::from_str(&get_s).unwrap();
        let raw = BASE64_STANDARD
            .decode(got["credentials_b64"].as_str().unwrap())
            .unwrap();
        let map: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(map["username"], "u");
        assert_eq!(map["password"], "p");
        assert_eq!(map["remote_dir"], "/wdmm");

        let r = wdmm_account_delete(cid.as_ptr());
        let del_s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap().to_string();
        wdmm_ffi_free_string(r);
        assert_eq!(del_s, "ok", "delete failed: {del_s}");

        let r = wdmm_account_get(cid.as_ptr());
        let after = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap().to_string();
        wdmm_ffi_free_string(r);
        assert_eq!(after, "null");
    }

    // ── Queue concurrency FFI tests ──────────────────────────────────────────

    #[test]
    fn queue_concurrency_ffi() {
        // get default
        let initial = wdmm_queue_get_concurrency();
        assert!(initial >= 1, "default concurrency must be >= 1");

        // set to 3
        let r = wdmm_queue_set_concurrency(3);
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok", "set_concurrency failed: {s}");
        wdmm_ffi_free_string(r);
        assert_eq!(wdmm_queue_get_concurrency(), 3);

        // clamp: 0 → 1
        let r = wdmm_queue_set_concurrency(0);
        wdmm_ffi_free_string(r);
        assert_eq!(wdmm_queue_get_concurrency(), 1);

        // clamp: 100 → 8
        let r = wdmm_queue_set_concurrency(100);
        wdmm_ffi_free_string(r);
        assert_eq!(wdmm_queue_get_concurrency(), 8);

        // restore initial
        let r = wdmm_queue_set_concurrency(initial);
        wdmm_ffi_free_string(r);
    }

    // ── Deleted tracks FFI tests ─────────────────────────────────────────────

    #[test]
    fn deleted_track_ffi_roundtrip() {
        let json = CString::new(
            r#"{"source_name":"src1","remote_path":"/music/track.flac","rev":100,"deleted_at":"2024-01-01T00:00:00Z","pushed":0}"#,
        )
        .unwrap();
        let r = wdmm_deleted_track_upsert(json.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok", "upsert failed: {s}");
        wdmm_ffi_free_string(r);

        let r = wdmm_deleted_tracks_list();
        let list_s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap().to_string();
        wdmm_ffi_free_string(r);
        assert!(!list_s.starts_with("error:"));
        let arr: Vec<serde_json::Value> = serde_json::from_str(&list_s).unwrap();
        assert!(arr.iter().any(|t| t["source_name"] == "src1"));

        let r = wdmm_deleted_tracks_unpushed();
        let unpushed_s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap().to_string();
        wdmm_ffi_free_string(r);
        let unpushed: Vec<serde_json::Value> = serde_json::from_str(&unpushed_s).unwrap();
        assert!(unpushed.iter().any(|t| t["rev"] == 100));

        let revs = CString::new("[100]").unwrap();
        let r = wdmm_deleted_tracks_mark_pushed(revs.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok");
        wdmm_ffi_free_string(r);

        let r = wdmm_deleted_tracks_unpushed();
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap().to_string();
        wdmm_ffi_free_string(r);
        let u: Vec<serde_json::Value> = serde_json::from_str(&s).unwrap();
        assert!(u.iter().all(|t| t["rev"] != 100));

        let r = wdmm_deleted_tracks_purge_up_to(100);
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok");
        wdmm_ffi_free_string(r);

        let src = CString::new("src2").unwrap();
        let path = CString::new("/music/del.mp3").unwrap();
        let r = wdmm_deleted_track_clear(src.as_ptr(), path.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok");
        wdmm_ffi_free_string(r);
    }

    #[test]
    fn tombstoned_remote_paths_ffi() {
        let json1 = CString::new(
            r#"{"source_name":"src_x","remote_path":"/a.mp3","rev":1,"deleted_at":"2024-01-01T00:00:00Z","pushed":0}"#,
        )
        .unwrap();
        let json2 = CString::new(
            r#"{"source_name":"src_x","remote_path":"/b.mp3","rev":2,"deleted_at":"2024-01-01T00:00:00Z","pushed":0}"#,
        )
        .unwrap();
        wdmm_ffi_free_string(wdmm_deleted_track_upsert(json1.as_ptr()));
        wdmm_ffi_free_string(wdmm_deleted_track_upsert(json2.as_ptr()));

        let src = CString::new("src_x").unwrap();
        let r = wdmm_tombstoned_remote_paths(src.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap().to_string();
        wdmm_ffi_free_string(r);
        let paths: Vec<String> = serde_json::from_str(&s).unwrap();
        assert!(paths.contains(&"/a.mp3".to_string()));
        assert!(paths.contains(&"/b.mp3".to_string()));
    }

    // ── Sync state FFI tests ─────────────────────────────────────────────────

    #[test]
    fn sync_state_ffi_roundtrip() {
        let json = CString::new(
            r#"{"remote_key":"library:test","last_seq":50,"base_up_to":25,"parts":"{\"x\":1}","updated_at":"2024-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        let r = wdmm_sync_state_save(json.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok", "save failed: {s}");
        wdmm_ffi_free_string(r);

        let key = CString::new("library:test").unwrap();
        let r = wdmm_sync_state_get(key.as_ptr());
        let get_s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap().to_string();
        wdmm_ffi_free_string(r);
        assert!(!get_s.starts_with("error:") && get_s != "null");
        let state: serde_json::Value = serde_json::from_str(&get_s).unwrap();
        assert_eq!(state["last_seq"], 50);
        assert_eq!(state["base_up_to"], 25);
        assert_eq!(state["parts"], r#"{"x":1}"#);

        let missing = CString::new("missing_key").unwrap();
        let r = wdmm_sync_state_get(missing.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "null");
        wdmm_ffi_free_string(r);

        let prefix = CString::new("library:").unwrap();
        let r = wdmm_sync_state_clear(prefix.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok");
        wdmm_ffi_free_string(r);

        let r = wdmm_sync_state_get(key.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "null");
        wdmm_ffi_free_string(r);
    }

    #[test]
    fn artist_album_aggregation_ffi() {
        // Clear library first
        let r = wdmm_library_clear_all();
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok");
        wdmm_ffi_free_string(r);

        // Upsert track with album+artist
        let track_json = CString::new(
            r#"{"id":"t1","title":"Song One","artist":"Artist A","album":"Album X","album_artist":"Artist A","remote_path":"/music/song1.mp3","account_id":"acc1","size":5000,"duration_ms":180000,"track_number":1,"year":2024}"#,
        )
        .unwrap();
        let r = wdmm_track_upsert(track_json.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok", "track upsert failed: {s}");
        wdmm_ffi_free_string(r);

        // Second track, same artist different album
        let track2_json = CString::new(
            r#"{"id":"t2","title":"Song Two","artist":"Artist A","album":"Album Y","remote_path":"/music/song2.mp3","account_id":"acc1","size":6000,"duration_ms":200000,"track_number":2}"#,
        )
        .unwrap();
        let r = wdmm_track_upsert(track2_json.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok", "track2 upsert failed: {s}");
        wdmm_ffi_free_string(r);

        // List artists
        let r = wdmm_artist_list();
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap().to_string();
        wdmm_ffi_free_string(r);
        let artists: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(artists.as_array().unwrap().len(), 1);
        assert_eq!(artists[0]["name"], "Artist A");

        // List albums
        let r = wdmm_album_list();
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap().to_string();
        wdmm_ffi_free_string(r);
        let albums: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(albums.as_array().unwrap().len(), 2);

        // Tracks by artist
        let artist_name = CString::new("Artist A").unwrap();
        let r = wdmm_tracks_by_artist(artist_name.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap().to_string();
        wdmm_ffi_free_string(r);
        let tracks: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(tracks.as_array().unwrap().len(), 2);

        // Tracks by album
        let album_title = CString::new("Album X").unwrap();
        let r = wdmm_tracks_by_album(album_title.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap().to_string();
        wdmm_ffi_free_string(r);
        let tracks: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(tracks.as_array().unwrap().len(), 1);
        assert_eq!(tracks[0]["id"], "t1");

        // Delete one track, album should be pruned
        let id = CString::new("t1").unwrap();
        let r = wdmm_track_delete(id.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok");
        wdmm_ffi_free_string(r);

        let r = wdmm_album_list();
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap().to_string();
        wdmm_ffi_free_string(r);
        let albums: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(albums.as_array().unwrap().len(), 1);

        // Delete last track, artist should be pruned
        let id2 = CString::new("t2").unwrap();
        let r = wdmm_track_delete(id2.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok");
        wdmm_ffi_free_string(r);

        let r = wdmm_artist_list();
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap().to_string();
        wdmm_ffi_free_string(r);
        let artists: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(artists.as_array().unwrap().len(), 0);
    }

    #[test]
    fn track_get_by_source_path_ffi() {
        // Arrange: insert a track (unique IDs — no clear needed)
        let track_json = CString::new(
            r#"{"id":"ffi-src-1","title":"FFI Source Track","remote_path":"/src/track.mp3","account_id":"acc-ffi","size":2000,"duration_ms":90000}"#,
        )
        .unwrap();
        let r = wdmm_track_upsert(track_json.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok", "upsert failed: {s}");
        wdmm_ffi_free_string(r);

        // Hit — should return track JSON with "id" key
        let acct = CString::new("acc-ffi").unwrap();
        let path = CString::new("/src/track.mp3").unwrap();
        let r = wdmm_track_get_by_source_path(acct.as_ptr(), path.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap().to_string();
        wdmm_ffi_free_string(r);
        assert!(!s.starts_with("error:"), "unexpected error: {s}");
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["id"], "ffi-src-1");
        assert_eq!(v["account_id"], "acc-ffi");

        // Miss — returns "null"
        let path_miss = CString::new("/src/missing.mp3").unwrap();
        let r = wdmm_track_get_by_source_path(acct.as_ptr(), path_miss.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "null");
        wdmm_ffi_free_string(r);
    }

    #[test]
    fn cue_album_for_slice_and_slices_for_cue_ffi() {
        // Arrange: insert album + 2 slices (unique IDs — no clear needed)
        let album_json = CString::new(
            r#"{"cue_id":"cue-ffi-1","source_name":"nas-ffi","cue_remote_path":"/ffi/disc.cue","title":"FFI Disc","performer":"FFI Artist","cache_group_id":null,"created_at":"2024-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        let r = wdmm_cue_album_upsert(album_json.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok", "album upsert failed: {s}");
        wdmm_ffi_free_string(r);

        for (mid_str, idx_val) in [("slice-ffi-1", 1i64), ("slice-ffi-2", 2i64)].iter() {
            let start = (idx_val - 1) * 60_000;
            let end = idx_val * 60_000;
            let slice_json = CString::new(format!(
                r#"{{"music_id":"{mid_str}","cue_id":"cue-ffi-1","audio_music_id":"audio-ffi","source_name":"nas-ffi","remote_path":"/ffi/t{idx_val}.virtual","file_name":"t{idx_val}","track_index":{idx_val},"title":"T{idx_val}","artist":null,"album_artist":null,"album":"FFI Disc","duration_ms":60000,"track_number":{idx_val},"track_total":2,"disc_number":null,"disc_total":null,"year":null,"genre":null,"bitrate":null,"sample_rate":null,"cover_state":0,"audio_remote_path":"/ffi/disc.flac","clip_start_ms":{start},"clip_end_ms":{end},"cache_group_id":null,"rev":0,"last_downloaded_at":"2024-01-01T00:00:00Z","last_tag_read_at":"2024-01-01T00:00:00Z","remote_missing":0}}"#,
            ))
            .unwrap();
            let r = wdmm_cue_slice_upsert(slice_json.as_ptr());
            let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
            assert_eq!(s, "ok", "slice upsert failed: {s}");
            wdmm_ffi_free_string(r);
        }

        // wdmm_cue_album_for_slice — hit
        let mid = CString::new("slice-ffi-1").unwrap();
        let r = wdmm_cue_album_for_slice(mid.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap().to_string();
        wdmm_ffi_free_string(r);
        assert!(!s.starts_with("error:"), "unexpected error: {s}");
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["cue_id"], "cue-ffi-1");
        assert_eq!(v["title"], "FFI Disc");

        // wdmm_cue_album_for_slice — miss
        let mid_miss = CString::new("no-such-slice").unwrap();
        let r = wdmm_cue_album_for_slice(mid_miss.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "null");
        wdmm_ffi_free_string(r);

        // wdmm_cue_slices_for_cue — count + track_index order
        let cid = CString::new("cue-ffi-1").unwrap();
        let r = wdmm_cue_slices_for_cue(cid.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap().to_string();
        wdmm_ffi_free_string(r);
        assert!(!s.starts_with("error:"), "unexpected error: {s}");
        let slices: serde_json::Value = serde_json::from_str(&s).unwrap();
        let arr = slices.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["track_index"], 1);
        assert_eq!(arr[1]["track_index"], 2);

        // wdmm_cue_slices_for_cue — unknown cue_id returns []
        let cid_miss = CString::new("no-cue").unwrap();
        let r = wdmm_cue_slices_for_cue(cid_miss.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap().to_string();
        wdmm_ffi_free_string(r);
        let empty: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert!(empty.as_array().unwrap().is_empty());

        // wdmm_track_get_by_source_path falls through to cue_slice when no track
        let acct = CString::new("nas-ffi").unwrap();
        let slice_path = CString::new("/ffi/t1.virtual").unwrap();
        let r = wdmm_track_get_by_source_path(acct.as_ptr(), slice_path.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap().to_string();
        wdmm_ffi_free_string(r);
        assert!(!s.starts_with("error:"), "unexpected error: {s}");
        let sv: serde_json::Value = serde_json::from_str(&s).unwrap();
        // CueSlice discriminant: has "music_id" and "cue_id"
        assert!(sv.get("music_id").is_some(), "expected CueSlice JSON, got: {s}");
        assert_eq!(sv["cue_id"], "cue-ffi-1");
    }

    // ── Deleted manga FFI tests ──────────────────────────────────────────────

    #[test]
    fn deleted_manga_ffi_upsert_and_list() {
        // Upsert a tombstone without a rev (rev = null).
        let json = CString::new(
            r#"{"manga_id":"dm-ffi-1","source_name":"nas-dm","remote_path":"/m/x.cbz","sync_key":"nas-dm/m/x.cbz","rev":null,"deleted_at":"2024-03-01T00:00:00Z","pushed":0}"#,
        )
        .unwrap();
        let r = wdmm_deleted_manga_upsert(json.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok", "upsert failed: {s}");
        wdmm_ffi_free_string(r);

        // list — row is visible
        let r = wdmm_deleted_manga_list();
        let list_s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap().to_string();
        wdmm_ffi_free_string(r);
        assert!(!list_s.starts_with("error:"), "list error: {list_s}");
        let arr: Vec<serde_json::Value> = serde_json::from_str(&list_s).unwrap();
        let row = arr.iter().find(|v| v["manga_id"] == "dm-ffi-1")
            .expect("tombstone not found in list");
        assert_eq!(row["source_name"], "nas-dm");
        assert_eq!(row["pushed"], 0);
        assert!(row["rev"].is_null());

        // Replace — assign a rev + mark pushed
        let json2 = CString::new(
            r#"{"manga_id":"dm-ffi-1","source_name":"nas-dm","remote_path":"/m/x.cbz","sync_key":"nas-dm/m/x.cbz","rev":77,"deleted_at":"2024-03-01T00:00:00Z","pushed":1}"#,
        )
        .unwrap();
        let r = wdmm_deleted_manga_upsert(json2.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok", "second upsert failed: {s}");
        wdmm_ffi_free_string(r);

        let r = wdmm_deleted_manga_list();
        let list_s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap().to_string();
        wdmm_ffi_free_string(r);
        let arr: Vec<serde_json::Value> = serde_json::from_str(&list_s).unwrap();
        let updated = arr.iter().filter(|v| v["manga_id"] == "dm-ffi-1").count();
        assert_eq!(updated, 1, "INSERT OR REPLACE must not duplicate the row");
        let row = arr.iter().find(|v| v["manga_id"] == "dm-ffi-1").unwrap();
        assert_eq!(row["rev"], 77);
        assert_eq!(row["pushed"], 1);
    }

    // ── CUE album list FFI test ──────────────────────────────────────────────

    #[test]
    fn list_cue_albums_ffi_non_empty_after_upsert() {
        // Insert an album with a unique id so the test is independent of ordering.
        let album_json = CString::new(
            r#"{"cue_id":"cue-list-ffi-99","source_name":"nas-list","cue_remote_path":"/list/disc.cue","title":"List Disc","performer":null,"cache_group_id":null,"created_at":"2024-06-01T00:00:00Z"}"#,
        )
        .unwrap();
        let r = wdmm_cue_album_upsert(album_json.as_ptr());
        let s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap();
        assert_eq!(s, "ok", "album upsert failed: {s}");
        wdmm_ffi_free_string(r);

        let r = wdmm_list_cue_albums();
        let list_s = unsafe { CStr::from_ptr(r.ptr) }.to_str().unwrap().to_string();
        wdmm_ffi_free_string(r);
        assert!(!list_s.starts_with("error:"), "list_cue_albums error: {list_s}");
        let albums: Vec<serde_json::Value> = serde_json::from_str(&list_s).unwrap();
        assert!(
            albums.iter().any(|a| a["cue_id"] == "cue-list-ffi-99"),
            "inserted album not found in list: {list_s}"
        );
        let a = albums.iter().find(|a| a["cue_id"] == "cue-list-ffi-99").unwrap();
        assert_eq!(a["source_name"], "nas-list");
        assert_eq!(a["title"], "List Disc");
    }
}
