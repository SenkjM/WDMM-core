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
}
