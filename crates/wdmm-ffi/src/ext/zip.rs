//! Slice: CBZ (ZIP) parsing over remote byte ranges owned by Rust (audit item 6).
//!
//! `wdmm_zip_open(account_id, remote_path)` returns the central directory, and
//! `wdmm_zip_read_entry(handle, index)` returns one entry's bytes. Replaces
//! `lib/utils/cbz_range_zip.dart`, which built its own `Dio` and re-implemented
//! EOCD/central-directory parsing in Dart.

use std::os::raw::c_char;

use crate::FfiString;

#[no_mangle]
pub extern "C" fn wdmm_zip_open(_account_id: *const c_char, _remote_path: *const c_char) -> FfiString {
    FfiString::from("error:unimplemented:zip_open")
}

#[no_mangle]
pub extern "C" fn wdmm_zip_read_entry(_handle: *const c_char, _index: *const c_char) -> FfiString {
    FfiString::from("error:unimplemented:zip_read_entry")
}
