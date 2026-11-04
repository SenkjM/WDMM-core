//! Slice: embedded tag/cover extraction owned by Rust (audit item 8).
//!
//! `wdmm_tags_extract(account_id, remote_path)` reads the minimum byte ranges a
//! container needs (ID3v2/FLAC/MP4/OGG headers) and returns the tags plus any
//! embedded cover. Replaces `lib/services/stream_cover_reader.dart`, which hand
//! rolled the same ranges in Dart over a sparse temp file.

use std::os::raw::c_char;

use crate::FfiString;

#[no_mangle]
pub extern "C" fn wdmm_tags_extract(_account_id: *const c_char, _remote_path: *const c_char) -> FfiString {
    FfiString::from("error:unimplemented:tags_extract")
}
