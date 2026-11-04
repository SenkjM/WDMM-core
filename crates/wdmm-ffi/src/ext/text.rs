//! Slice: subtitle charset detection + decoding owned by Rust (audit item 4).
//!
//! `wdmm_subtitle_decode(bytes, len)` replaces `lib/utils/subtitle_encoding.dart`
//! (585 lines: charset sniffing, a hand-written GB18030 table and the
//! score/round-trip heuristics). The detector lives in `wdmm_text::subtitle`.

use std::os::raw::c_uchar;

use crate::FfiString;

#[no_mangle]
pub extern "C" fn wdmm_subtitle_decode(_data: *const c_uchar, _len: usize) -> FfiString {
    FfiString::from("error:unimplemented:subtitle_decode")
}
