//! Slice: CUE sheet parsing owned by Rust (audit item 2).
//!
//! `wdmm_cue_parse(text)` returns the parsed sheet as JSON so the 222-line Dart
//! state machine in `lib/utils/cue_sheet.dart` can be deleted. The parser itself
//! lives in `wdmm_text::cue`.

use std::os::raw::c_char;

use crate::FfiString;

#[no_mangle]
pub extern "C" fn wdmm_cue_parse(_text: *const c_char) -> FfiString {
    FfiString::from("error:unimplemented:cue_parse")
}
