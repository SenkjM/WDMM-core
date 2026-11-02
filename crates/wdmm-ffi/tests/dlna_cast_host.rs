//! Host integration tests for the Rust-owned DLNA cast path.
//!
//! These tests are `#[ignore]`d because they need real infrastructure: a DLNA
//! MediaRenderer on the LAN plus a WebDAV account to pull media bytes from.
//! Every test skips cleanly (prints `skipped: …` and returns) when the env vars
//! it needs are unset, so `cargo test -p wdmm-ffi -- --ignored` stays green on
//! a bare machine and in CI.
//!
//! Run against a real renderer:
//!
//! ```powershell
//! $env:WDMM_DLNA_TEST_RENDERER        = 'http://192.168.0.101:1892/'
//! $env:WDMM_DLNA_TEST_WEBDAV_URL      = 'http://192.168.0.107:5244/dav'
//! $env:WDMM_DLNA_TEST_WEBDAV_USER     = 'guest'
//! $env:WDMM_DLNA_TEST_WEBDAV_PASSWORD = 'guest'
//! $env:WDMM_DLNA_TEST_REMOTE_PATH     = '/Local/Downloads/video_2022-07-16_17-43-22.mp4'
//! cargo test -p wdmm-ffi --test dlna_cast_host -- --ignored --nocapture
//! ```
//!
//! Optional knobs:
//! * `WDMM_DLNA_TEST_SIZE` — file size in bytes (skips the driver size lookup)
//! * `WDMM_DLNA_TEST_PRESET_ID` — cast through the HLS transcode path
//! * `WDMM_DLNA_TEST_FORCE_DIRECT` — `true`/`false`, default `true`
//! * `WDMM_DLNA_TEST_UI_LANG` — sidecar subtitle language, e.g. `zh`
//! * `WDMM_DLNA_TEST_HOLD_SECS` — how long to poll state, default 10
//!
//! The FFI functions are declared as `extern "C"` here on purpose: that is the
//! exact ABI surface `dart:ffi` binds to, so the test fails if a symbol name or
//! signature drifts.

use std::ffi::{c_char, CStr, CString};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use wdmm_core::models::CloudAccount;
use wdmm_ffi::FfiString;
use wdmm_storage::LibraryDatabase;

extern "C" {
    fn wdmm_library_init(path: *const c_char) -> FfiString;
    fn wdmm_ffi_free_string(result: FfiString);
    fn wdmm_dlna_session_start(config_json: *const c_char) -> FfiString;
    fn wdmm_dlna_session_state(handle: u32) -> FfiString;
    fn wdmm_dlna_session_close(handle: u32) -> FfiString;
    fn wdmm_dlna_sink_protocol_info_json(renderer_location: *const c_char) -> FfiString;
}

/// Account row the tests insert into their throwaway library DB.
const ACCOUNT_ID: &str = "host-dlna-test";

fn env_non_empty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Renderer description URL, or `None` to skip the test.
fn renderer_location() -> Option<String> {
    env_non_empty("WDMM_DLNA_TEST_RENDERER")
}

fn webdav_config() -> Option<(String, String, String)> {
    Some((
        env_non_empty("WDMM_DLNA_TEST_WEBDAV_URL")?,
        env_non_empty("WDMM_DLNA_TEST_WEBDAV_USER").unwrap_or_default(),
        env_non_empty("WDMM_DLNA_TEST_WEBDAV_PASSWORD").unwrap_or_default(),
    ))
}

/// Path to a throwaway library DB seeded with the test WebDAV account.
///
/// Created once per test process: `wdmm_library_init` is a process-global
/// singleton, so both tests must point at the same seeded DB.
fn seeded_library_db() -> &'static PathBuf {
    static DB: OnceLock<PathBuf> = OnceLock::new();
    DB.get_or_init(|| {
        let path = std::env::temp_dir().join(format!("wdmm-dlna-host-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let (url, username, password) = webdav_config().expect("webdav env checked by caller");
        let db = LibraryDatabase::open(&path).expect("open throwaway library db");
        let credentials = json!({ "username": username, "password": password });
        db.insert_account(&CloudAccount {
            id: ACCOUNT_ID.to_string(),
            provider_type: "webdav".to_string(),
            display_name: "host dlna test".to_string(),
            base_url: Some(url),
            credentials_encrypted: serde_json::to_vec(&credentials).expect("credentials json"),
        })
        .expect("insert webdav account");
        path
    })
}

/// Initialise the process-global library DB at the seeded path (idempotent).
fn init_library() {
    let path = seeded_library_db();
    let c_path = CString::new(path.to_string_lossy().as_bytes()).expect("db path cstring");
    let result = read_ffi(unsafe { wdmm_library_init(c_path.as_ptr()) });
    assert_eq!(result, "ok", "wdmm_library_init failed: {result}");
}

/// Read and free an FFI string result.
fn read_ffi(result: FfiString) -> String {
    let text = if result.ptr.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(result.ptr) }
            .to_str()
            .expect("utf-8 FFI string")
            .to_string()
    };
    unsafe { wdmm_ffi_free_string(result) };
    text
}

fn json_ffi(result: FfiString) -> Value {
    let text = read_ffi(result);
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("expected JSON result, got {text:?}: {e}"))
}

fn session_start(config: &Value) -> Value {
    let c_config = CString::new(config.to_string()).expect("config cstring");
    json_ffi(unsafe { wdmm_dlna_session_start(c_config.as_ptr()) })
}

fn session_state(handle: u32) -> Value {
    json_ffi(unsafe { wdmm_dlna_session_state(handle) })
}

fn session_close(handle: u32) -> String {
    read_ffi(unsafe { wdmm_dlna_session_close(handle) })
}

fn http_get_range(url: &str) -> (u16, usize) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("http client");
        let response = client
            .get(url)
            .header(reqwest::header::RANGE, "bytes=0-4095")
            .send()
            .await
            .unwrap_or_else(|e| panic!("proxy GET {url} failed: {e}"));
        let status = response.status().as_u16();
        let bytes = response.bytes().await.expect("range body");
        (status, bytes.len())
    })
}

/// Full cast round trip: start a session, prove the proxy serves real bytes,
/// poll the renderer until it reports PLAYING with an advancing position, then
/// close the session.
#[test]
#[ignore = "needs a real DLNA renderer and WebDAV account (env-gated)"]
fn cast_to_real_renderer() {
    let Some(renderer) = renderer_location() else {
        println!("skipped: set WDMM_DLNA_TEST_RENDERER to a renderer description URL");
        return;
    };
    if webdav_config().is_none() {
        println!("skipped: set WDMM_DLNA_TEST_WEBDAV_URL (and _USER/_PASSWORD)");
        return;
    }
    let Some(remote_path) = env_non_empty("WDMM_DLNA_TEST_REMOTE_PATH") else {
        println!("skipped: set WDMM_DLNA_TEST_REMOTE_PATH to a WebDAV video path");
        return;
    };
    let size: u64 = env_non_empty("WDMM_DLNA_TEST_SIZE")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let force_direct = env_non_empty("WDMM_DLNA_TEST_FORCE_DIRECT")
        .map(|value| value.eq_ignore_ascii_case("true"))
        .unwrap_or(true);
    let hold = env_non_empty("WDMM_DLNA_TEST_HOLD_SECS")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(10);

    init_library();

    let name = remote_path
        .rsplit('/')
        .next()
        .unwrap_or("video")
        .to_string();
    let config = json!({
        "renderer_location": renderer,
        "account_id": ACCOUNT_ID,
        "remote_path": remote_path,
        "name": name,
        "size": size,
        "preset_id": env_non_empty("WDMM_DLNA_TEST_PRESET_ID"),
        "force_direct": force_direct,
        "ui_lang": env_non_empty("WDMM_DLNA_TEST_UI_LANG"),
    });
    println!("start config: {config}");

    let started = session_start(&config);
    println!("start result: {started}");
    assert_eq!(
        started["ok"], true,
        "wdmm_dlna_session_start failed: {started}"
    );
    let handle = started["handle"]
        .as_u64()
        .expect("session handle in start result") as u32;
    let media_url = started["url"]
        .as_str()
        .expect("media url in start result")
        .to_string();

    // The renderer must be able to pull the bytes: verify the proxy serves a
    // real Range request before blaming the renderer for anything.
    let (status, len) = http_get_range(&media_url);
    println!("proxy GET {media_url} -> HTTP {status}, {len} bytes");
    assert!(
        (200..300).contains(&status),
        "proxy did not serve the media url {media_url}: HTTP {status}"
    );
    assert!(len > 0, "proxy served an empty body for {media_url}");

    // Poll until the renderer plays, then confirm the position advances.
    let deadline = Instant::now() + Duration::from_secs(hold);
    let mut first_playing_ms: Option<i64> = None;
    let mut last = json!(null);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(500));
        let state = session_state(handle);
        println!("state: {state}");
        assert_eq!(state["ok"], true, "session state failed: {state}");
        let transport = state["transport"].as_str().unwrap_or_default().to_string();
        let position = state["position_ms"].as_i64().unwrap_or(0);
        last = state;
        if transport == "PLAYING" {
            match first_playing_ms {
                Some(first) if position > first => break,
                Some(_) => {}
                None => first_playing_ms = Some(position),
            }
        }
    }
    let played = first_playing_ms.is_some();
    session_close(handle);
    assert!(
        played,
        "renderer never reported PLAYING; last state: {last}"
    );
}

/// `GetProtocolInfo` against a real renderer, without starting a cast.
#[test]
#[ignore = "needs a real DLNA renderer (env-gated)"]
fn sink_protocol_info_of_real_renderer() {
    let Some(renderer) = renderer_location() else {
        println!("skipped: set WDMM_DLNA_TEST_RENDERER to a renderer description URL");
        return;
    };
    let c_renderer = CString::new(renderer).expect("renderer cstring");
    let value = json_ffi(unsafe { wdmm_dlna_sink_protocol_info_json(c_renderer.as_ptr()) });
    println!("sink protocol info: {value}");
    assert_eq!(value["ok"], true, "GetProtocolInfo failed: {value}");
    assert!(
        value["sink_count"].as_u64().unwrap_or(0) > 0,
        "renderer reported no SinkProtocolInfo entries: {value}"
    );
}
