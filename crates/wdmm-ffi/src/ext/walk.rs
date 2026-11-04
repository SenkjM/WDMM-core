//! Slice: recursive account walk owned by Rust (audit item 5).
//!
//! `wdmm_account_walk(account_id, root, max_depth, max_entries, kinds_json)`
//! replaces `WebDavService.collectFilesRecursive`, the isomorphic copy in
//! `CloudDriveService` and `lib/utils/directory_walk.dart`, with the same depth
//! and entry circuit breakers in one place.

use std::os::raw::c_char;

use crate::FfiString;

#[no_mangle]
pub extern "C" fn wdmm_account_walk(
    _account_id: *const c_char,
    _root: *const c_char,
    _max_depth: *const c_char,
    _max_entries: *const c_char,
    _kinds_json: *const c_char,
) -> FfiString {
    FfiString::from("error:unimplemented:account_walk")
}
