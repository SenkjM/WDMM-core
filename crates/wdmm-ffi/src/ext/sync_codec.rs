//! Slice: credential-vault and playlist wire formats owned by Rust (audit item 3).
//!
//! These four symbols let the `wdmm-sync` engine speak the same WMP containers
//! the app already uses (`wdmm_sync::wmp`), so Dart's second encode/decode engine
//! (`credential_vault_codec.dart`, `playlist_codec.dart`,
//! `playlist_deletion_codec.dart`, `library_shard_codec.dart`) can be deleted.

use std::os::raw::c_uchar;

use crate::FfiString;

#[no_mangle]
pub extern "C" fn wdmm_vault_encode(_records_json: *const std::os::raw::c_char) -> FfiString {
    FfiString::from("error:unimplemented:vault_encode")
}

#[no_mangle]
pub extern "C" fn wdmm_vault_decode(_data: *const c_uchar, _len: usize) -> FfiString {
    FfiString::from("error:unimplemented:vault_decode")
}

#[no_mangle]
pub extern "C" fn wdmm_playlist_encode(_playlist_json: *const std::os::raw::c_char) -> FfiString {
    FfiString::from("error:unimplemented:playlist_encode")
}

#[no_mangle]
pub extern "C" fn wdmm_playlist_decode(_data: *const c_uchar, _len: usize) -> FfiString {
    FfiString::from("error:unimplemented:playlist_decode")
}
