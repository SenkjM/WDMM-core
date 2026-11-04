//! Extension FFI modules for the Dart→Rust cut-over
//! (`docs/ARCHITECTURE.md`: Rust owns the execution layer, Dart only renders and
//! carries user-level control).
//!
//! Each submodule owns one migrated capability end to end: the business logic
//! lives in a `wdmm-*` crate, and the file here is a thin `#[no_mangle]` edge
//! that parses C strings, calls the crate, and serialises JSON. Keeping one file
//! per capability means the cut-over slices never edit the same file.

pub mod backup_probe;
pub mod crypto;
pub mod cue;
pub mod sync_codec;
pub mod tags;
pub mod text;
pub mod walk;
pub mod zip;

use std::os::raw::c_char;

use crate::{ptr_to_str, FfiString};

/// Read a decimal argument that the Dart side sends as a string.
pub(crate) fn parse_u64_arg(p: *const c_char, what: &str) -> Result<u64, FfiString> {
    match ptr_to_str(p).and_then(|s| s.parse().ok()) {
        Some(v) => Ok(v),
        None => Err(FfiString::from(format!("error:invalid {what}"))),
    }
}

/// Read a required, non-empty UTF-8 argument.
pub(crate) fn require_arg(p: *const c_char, what: &str) -> Result<&'static str, FfiString> {
    match ptr_to_str(p) {
        Some(s) if !s.is_empty() => Ok(s),
        _ => Err(FfiString::from(format!("error:invalid {what}"))),
    }
}

/// Serialise a `Result<Value, String>` into the FFI JSON convention.
pub(crate) fn json_reply(result: Result<serde_json::Value, String>) -> FfiString {
    match result {
        Ok(value) => match serde_json::to_string(&value) {
            Ok(text) => FfiString::from(text),
            Err(e) => FfiString::from(format!("error:serde: {e}")),
        },
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}
