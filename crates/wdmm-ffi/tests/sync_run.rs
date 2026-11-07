//! Contract test for `wdmm_sync_run` — the single aggregate sync entry point.
//!
//! The function is declared `extern "C"` here on purpose: that is the ABI
//! surface `dart:ffi` binds to, so a symbol or signature drift fails the test.

use std::ffi::{c_char, CStr, CString};

use wdmm_ffi::FfiString;

extern "C" {
    fn wdmm_sync_run(json: *const c_char) -> FfiString;
    fn wdmm_ffi_free_string(value: FfiString);
}

/// Call the entry point and return its raw reply text.
fn run(payload: Option<&str>) -> String {
    let owned = payload.map(|text| CString::new(text).expect("payload must not contain NUL"));
    let pointer = owned
        .as_ref()
        .map(|value| value.as_ptr())
        .unwrap_or(std::ptr::null());
    let reply = unsafe { wdmm_sync_run(pointer) };
    let text = unsafe { CStr::from_ptr(reply.ptr) }
        .to_string_lossy()
        .into_owned();
    unsafe { wdmm_ffi_free_string(reply) };
    text
}

#[test]
fn a_null_payload_is_rejected() {
    assert_eq!(run(None), "error:invalid payload");
}

#[test]
fn malformed_json_is_rejected() {
    let reply = run(Some("{not json"));
    assert!(
        reply.starts_with("error:invalid payload:"),
        "unexpected reply: {reply}"
    );
}

#[test]
fn a_missing_account_is_rejected() {
    let reply = run(Some(r#"{"remote_dir":"/sync","mode":"full"}"#));
    assert_eq!(reply, "error:missing account_id");
    let reply = run(Some(r#"{"account_id":"","mode":"full"}"#));
    assert_eq!(reply, "error:missing account_id");
}

#[test]
fn an_unknown_mode_is_rejected() {
    let reply = run(Some(r#"{"account_id":"acc","mode":"sideways"}"#));
    assert_eq!(reply, "error:unknown mode: sideways");
}
