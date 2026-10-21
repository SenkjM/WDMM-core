//! `wdmm-ffi` — C ABI surface for flutter_rust_bridge (Android) and
//! csbindgen (Windows). Thin conversion layer, no business logic.

#![allow(dead_code)]

use std::ffi::CString;
use std::os::raw::c_char;

/// FFI-safe string buffer result.
#[repr(C)]
pub struct FfiString {
    pub ptr: *mut c_char,
}

impl FfiString {
    fn from_string(s: String) -> Self {
        Self {
            ptr: CString::new(s).unwrap().into_raw(),
        }
    }
}

/// Free an FfiString allocated by Rust.
#[no_mangle]
pub extern "C" fn wdmm_ffi_free_string(s: FfiString) {
    if !s.ptr.is_null() {
        unsafe {
            let _ = CString::from_raw(s.ptr);
        }
    }
}

/// Return the wdmm-core version string.
#[no_mangle]
pub extern "C" fn wdmm_version() -> FfiString {
    FfiString::from_string(env!("CARGO_PKG_VERSION").to_string())
}

/// Check if OpenAPI server is running.
#[no_mangle]
pub extern "C" fn wdmm_openapi_status() -> i32 {
    // TODO: integrate with wdmm-openapi server handle
    0 // 0 = not running
}

/// Simple hello function for smoke test.
#[no_mangle]
pub extern "C" fn wdmm_hello() -> FfiString {
    FfiString::from_string("Hello from WDMM Rust core!".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_smoke() {
        let v = wdmm_version();
        assert!(!v.ptr.is_null());
        wdmm_ffi_free_string(v);
    }

    #[test]
    fn hello_smoke() {
        let h = wdmm_hello();
        assert!(!h.ptr.is_null());
        wdmm_ffi_free_string(h);
    }
}
