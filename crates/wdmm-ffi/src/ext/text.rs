//! Slice: subtitle charset detection + decoding owned by Rust (audit item 4).
//!
//! `wdmm_subtitle_decode(bytes, len)` replaces `lib/utils/subtitle_encoding.dart`
//! (585 lines: charset sniffing, a hand-written GB18030 table and the
//! score/round-trip heuristics). The detector lives in `wdmm_text::subtitle`.
//!
//! `wdmm_subtitle_decode_as(bytes, len, encoding)` is the forced path behind the
//! user's encoding override. Wire names are `utf8`, `utf16le`, `utf16be`, `gbk`,
//! `gb18030` and `big5`; an unknown name is a decode failure
//! (`{"ok":false,"reason":"unknown_encoding"}`), not an FFI error, because the
//! name comes from UI state rather than from a broken binding.
//!
//! Detection failures are data, not `error:` FFI failures: the reply is
//! `{"ok":false,"reason":"binary"|"not_text"}` and the Dart binder maps it back
//! to its old `onError('binary')` / `onError('decode')` calls.

use std::os::raw::{c_char, c_uchar};

use serde_json::{json, Value};
use wdmm_text::subtitle::{self, SubtitleEncoding};

use super::json_reply;
use crate::FfiString;

/// Map a Dart wire name to the encoding it forces.
fn parse_encoding(name: &str) -> Option<SubtitleEncoding> {
    Some(match name {
        "utf8" => SubtitleEncoding::Utf8,
        "utf16le" => SubtitleEncoding::Utf16Le,
        "utf16be" => SubtitleEncoding::Utf16Be,
        // `encoding_rs` decodes GBK with its GB18030 decoder, so both names are
        // the same decoder and both report `gbk`.
        "gbk" | "gb18030" => SubtitleEncoding::Gbk,
        "big5" => SubtitleEncoding::Big5,
        _ => return None,
    })
}

/// Decode subtitle bytes, auto-detecting the charset.
///
/// Empty input is a decode failure (`binary`), not an argument error: an empty
/// subtitle file is legal input.
#[no_mangle]
pub extern "C" fn wdmm_subtitle_decode(data: *const c_uchar, len: usize) -> FfiString {
    let Some(bytes) = bytes_arg(data, len) else {
        return FfiString::from("error:invalid data");
    };
    json_reply(Ok(decode_reply(&bytes, None)))
}

/// Decode subtitle bytes as the given wire-name encoding, skipping detection.
#[no_mangle]
pub extern "C" fn wdmm_subtitle_decode_as(
    data: *const c_uchar,
    len: usize,
    encoding: *const c_char,
) -> FfiString {
    let Some(bytes) = bytes_arg(data, len) else {
        return FfiString::from("error:invalid data");
    };
    json_reply(Ok(decode_as_reply(&bytes, crate::ptr_to_str(encoding))))
}

/// A null pointer is an FFI misuse; `len == 0` is an empty payload.
fn bytes_arg(data: *const c_uchar, len: usize) -> Option<Vec<u8>> {
    if data.is_null() {
        return None;
    }
    if len == 0 {
        return Some(Vec::new());
    }
    Some(unsafe { std::slice::from_raw_parts(data, len).to_vec() })
}

fn decode_reply(bytes: &[u8], forced: Option<SubtitleEncoding>) -> Value {
    let result = match forced {
        Some(encoding) => subtitle::decode_as(bytes, encoding),
        None => subtitle::decode(bytes),
    };
    match result {
        Ok(decoded) => json!({
            "ok": true,
            "text": decoded.text,
            "encoding": decoded.name(),
            // Strict decoders only; reserved for a future lenient fallback.
            "lossy": decoded.lossy,
        }),
        Err(error) => json!({ "ok": false, "reason": error.reason() }),
    }
}

fn decode_as_reply(bytes: &[u8], wire: Option<&str>) -> Value {
    let Some(forced) = wire.and_then(parse_encoding) else {
        return json!({ "ok": false, "reason": "unknown_encoding" });
    };
    decode_reply(bytes, Some(forced))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "你好" in GBK.
    const GBK_NIHAO: &[u8] = &[0xC4, 0xE3, 0xBA, 0xC3];
    /// "测试" in GBK; not valid UTF-8 (0xB2 is a lead byte followed by 0xE2).
    const GBK_CESHI: &[u8] = &[0xB2, 0xE2, 0xCA, 0xD4];
    /// "中文" in Big5.
    const BIG5_ZHONGWEN: &[u8] = &[0xA4, 0xA4, 0xA4, 0xE5];
    /// U+1F600 ("😀") as a 4-byte GB18030 sequence.
    const GB18030_EMOJI: &[u8] = &[0x94, 0x39, 0xFC, 0x36];

    fn read_and_free(reply: FfiString) -> Value {
        let text = unsafe {
            std::ffi::CStr::from_ptr(reply.ptr)
                .to_str()
                .expect("utf-8 reply")
                .to_owned()
        };
        crate::wdmm_ffi_free_string(reply);
        serde_json::from_str(&text).expect("json reply")
    }

    #[test]
    fn auto_detect_reply_has_ok_text_and_name() {
        let reply = decode_reply(b"hello", None);
        assert_eq!(reply["ok"], json!(true));
        assert_eq!(reply["text"], json!("hello"));
        assert_eq!(reply["encoding"], json!("utf-8"));
        assert_eq!(reply["lossy"], json!(false));
    }

    #[test]
    fn auto_detect_gbk_payload() {
        let reply = decode_reply(GBK_NIHAO, None);
        assert_eq!(reply["ok"], json!(true));
        assert_eq!(reply["text"], json!("你好"));
        assert_eq!(reply["encoding"], json!("gbk"));
    }

    #[test]
    fn binary_payload_reports_binary_reason() {
        let reply = decode_reply(&[0x00, 0x00, 0x01, 0xBA], None);
        assert_eq!(reply["ok"], json!(false));
        assert_eq!(reply["reason"], json!("binary"));
    }

    #[test]
    fn empty_payload_reports_binary_reason() {
        let reply = decode_reply(&[], None);
        assert_eq!(reply["ok"], json!(false));
        assert_eq!(reply["reason"], json!("binary"));
    }

    #[test]
    fn forced_big5_wire_name_decodes() {
        let reply = decode_as_reply(BIG5_ZHONGWEN, Some("big5"));
        assert_eq!(reply["ok"], json!(true));
        assert_eq!(reply["text"], json!("中文"));
        assert_eq!(reply["encoding"], json!("big5"));
    }

    #[test]
    fn forced_gb18030_wire_name_reports_gbk() {
        let reply = decode_as_reply(GB18030_EMOJI, Some("gb18030"));
        assert_eq!(reply["ok"], json!(true));
        assert_eq!(reply["text"], json!("😀"));
        assert_eq!(reply["encoding"], json!("gbk"));
    }

    #[test]
    fn forced_wrong_encoding_reports_not_text() {
        let reply = decode_as_reply(GBK_CESHI, Some("utf8"));
        assert_eq!(reply["ok"], json!(false));
        assert_eq!(reply["reason"], json!("not_text"));
    }

    #[test]
    fn unknown_wire_name_reports_unknown_encoding() {
        for wire in [Some("latin1"), Some("gb2312"), Some(""), None] {
            let reply = decode_as_reply(b"hello", wire);
            assert_eq!(reply["ok"], json!(false), "wire {wire:?}");
            assert_eq!(reply["reason"], json!("unknown_encoding"), "wire {wire:?}");
        }
    }

    #[test]
    fn ffi_entry_points_read_byte_arguments() {
        let reply = wdmm_subtitle_decode(GBK_NIHAO.as_ptr(), GBK_NIHAO.len());
        let value = read_and_free(reply);
        assert_eq!(value["ok"], json!(true));
        assert_eq!(value["text"], json!("你好"));

        let wire = std::ffi::CString::new("big5").expect("cstring");
        let reply = wdmm_subtitle_decode_as(
            BIG5_ZHONGWEN.as_ptr(),
            BIG5_ZHONGWEN.len(),
            wire.as_ptr(),
        );
        let value = read_and_free(reply);
        assert_eq!(value["ok"], json!(true));
        assert_eq!(value["text"], json!("中文"));
    }

    #[test]
    fn null_data_pointer_is_an_ffi_error() {
        let reply = wdmm_subtitle_decode(std::ptr::null(), 10);
        let text = unsafe {
            std::ffi::CStr::from_ptr(reply.ptr)
                .to_str()
                .expect("utf-8 reply")
                .to_owned()
        };
        crate::wdmm_ffi_free_string(reply);
        assert_eq!(text, "error:invalid data");
    }
}

