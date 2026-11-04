//! Host integration tests for `wdmm_backup_probe` (Dart→Rust cut-over, slice 1).
//!
//! The FFI functions are declared as `extern "C"` here on purpose: that is the
//! exact ABI surface `dart:ffi` binds to, so the test fails if a symbol name or
//! signature drifts.

use std::ffi::{c_char, CStr, CString};
use std::path::{Path, PathBuf};

use serde_json::Value;
use wdmm_ffi::FfiString;
use wdmm_storage::LibraryDatabase;

extern "C" {
    fn wdmm_backup_probe(data_path: *const c_char, passphrase: *const c_char) -> FfiString;
    fn wdmm_ffi_free_string(value: FfiString);
}

/// Call the probe and return its parsed JSON (errors keep the `error:` prefix).
fn probe(path: &CStr, passphrase: &CStr) -> Value {
    let reply = unsafe { wdmm_backup_probe(path.as_ptr(), passphrase.as_ptr()) };
    let text = unsafe { CStr::from_ptr(reply.ptr) }
        .to_string_lossy()
        .into_owned();
    unsafe { wdmm_ffi_free_string(reply) };
    if text.starts_with("error:") {
        return serde_json::json!({ "error": text });
    }
    serde_json::from_str(&text).expect("probe reply must be JSON")
}

fn temp_file(name: &str, bytes: &[u8]) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "wdmm_backup_probe_{}_{name}",
        std::process::id()
    ));
    std::fs::write(&path, bytes).expect("write temp archive");
    path
}

#[test]
fn probe_answers_json_for_a_real_archive() {
    let lib = LibraryDatabase::open(Path::new(":memory:")).expect("in-memory library");
    let bytes = wdmm_sync::build_backup_bytes(&lib, None, "pw", None).expect("build archive");
    let path = temp_file("archive.wdmm", &bytes);
    let path_c = CString::new(path.to_string_lossy().as_bytes()).unwrap();

    let reply = probe(&path_c, &CString::new("pw").unwrap());
    let _ = std::fs::remove_file(&path);

    assert_eq!(reply["ok"], true, "reply must be ok: {reply}");
    assert_eq!(reply["valid"], true, "correct passphrase: {reply}");
    assert_eq!(reply["version"], 7);
    assert!(reply["created_at"].is_string());
    assert!(reply["reason"].is_null());
}

#[test]
fn probe_reports_a_wrong_passphrase_as_json() {
    let lib = LibraryDatabase::open(Path::new(":memory:")).expect("in-memory library");
    let bytes = wdmm_sync::build_backup_bytes(&lib, None, "pw", None).expect("build archive");
    let path = temp_file("wrong-pass.wdmm", &bytes);
    let path_c = CString::new(path.to_string_lossy().as_bytes()).unwrap();

    let reply = probe(&path_c, &CString::new("nope").unwrap());
    let _ = std::fs::remove_file(&path);

    assert_eq!(reply["valid"], false);
    assert_eq!(reply["reason"], "bad-passphrase");
}

#[test]
fn probe_rejects_an_empty_path_argument() {
    let reply = probe(&CString::new("").unwrap(), &CString::new("").unwrap());
    assert_eq!(reply["error"], "error:invalid data_path");
}

#[test]
fn probe_reports_a_missing_file() {
    let path = std::env::temp_dir().join(format!(
        "wdmm_backup_probe_missing_{}.wdmm",
        std::process::id()
    ));
    let path_c = CString::new(path.to_string_lossy().as_bytes()).unwrap();
    let reply = probe(&path_c, &CString::new("").unwrap());
    let text = reply["error"].as_str().expect("error reply");
    assert!(text.starts_with("error:read backup:"), "got: {text}");
}
