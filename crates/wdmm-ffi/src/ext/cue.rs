//! Slice: CUE sheet parsing owned by Rust (audit item 2).
//!
//! `wdmm_cue_parse(text)` returns the parsed sheet as JSON so the 222-line Dart
//! state machine in `lib/utils/cue_sheet.dart` can be deleted. The parser itself
//! lives in `wdmm_text::cue`.

use std::os::raw::c_char;

use super::json_reply;
use crate::{ptr_to_str, FfiString};

/// Parse CUE sheet text into `{"ok":true,"sheet":{…}}`.
///
/// A sheet with no complete `TRACK` answers `{"ok":false,"error":"no_tracks"}`
/// (also for empty text — an empty CUE file is a legal input, not a bad
/// argument). A null pointer is the only `error:` case.
#[no_mangle]
pub extern "C" fn wdmm_cue_parse(text: *const c_char) -> FfiString {
    let Some(text) = ptr_to_str(text) else {
        return FfiString::from("error:invalid text");
    };
    let value = match wdmm_text::cue::parse(text) {
        Ok(sheet) => serde_json::json!({ "ok": true, "sheet": sheet }),
        Err(error) => serde_json::json!({ "ok": false, "error": error.code() }),
    };
    json_reply(Ok(value))
}
