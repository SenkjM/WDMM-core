//! Subtitle charset detection and decoding (audit item 4).
//!
//! Ported from `lib/utils/subtitle_encoding.dart`. The BOM handling, the
//! UTF-8/UTF-16/GBK/GB18030/Big5 candidate set, the `_score` heuristic, the
//! round-trip checks and the `_tooCorrupt` ratio are kept exactly as they were
//! in Dart; the hand-written GB18030 table is replaced by `encoding_rs`.
//!
//! Two consequences of using `encoding_rs` are documented instead of papered
//! over:
//!
//! - `encoding_rs::GBK` is backed by the GB18030 decoder, so it accepts 4-byte
//!   GB18030 sequences that the Dart `GbkCodec` rejected. The decoded text is
//!   the same; with the current candidate list it can only change the reported
//!   name, never the text.
//! - `SubtitleEncoding` has a single Simplified-Chinese variant [`Gbk`], so the
//!   FFI's `gb18030` wire name also decodes (with the same decoder) and reports
//!   `gbk`. No caller branches on the name.
//!
//! The `Latin1` variant is declared by the FFI contract but has no Dart
//! behaviour to port, so it is never used for detection and rejects forced
//! decoding.

use encoding_rs::{DecoderResult, EncoderResult, BIG5, Encoding, GBK, UTF_16BE, UTF_16LE, UTF_8};

/// Bytes scanned by the binary/UTF-16 plausibility probe.
const MAX_PROBE: usize = 4096;

/// UTF-8 byte order mark.
const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// The decoder replacement character; a strict decode never produces it, but a
/// payload may legitimately encode it and the Dart original rejects those.
const REPLACEMENT_CHAR: char = '\u{FFFD}';

/// The byte order mark as a character (after a UTF-16 decode).
const BOM_CHAR: char = '\u{FEFF}';

/// Charset names a decoded subtitle can report, matching the Dart
/// `SubtitleEncodingChoice` wire names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubtitleEncoding {
    /// UTF-8 with or without a BOM.
    Utf8,
    /// UTF-16 little endian.
    Utf16Le,
    /// UTF-16 big endian.
    Utf16Be,
    /// GBK / GB18030 (Simplified Chinese).
    Gbk,
    /// Big5 (Traditional Chinese).
    Big5,
    /// Any 8-bit fallback.
    ///
    /// Part of the scaffolded contract but unused: the Dart implementation had
    /// no Latin-1 path, and the FFI wire names do not include it.
    Latin1,
}

impl SubtitleEncoding {
    /// The Dart `_nameOf` name of this encoding.
    pub fn name(self) -> &'static str {
        match self {
            SubtitleEncoding::Utf8 => "utf-8",
            SubtitleEncoding::Utf16Le => "utf-16le",
            SubtitleEncoding::Utf16Be => "utf-16be",
            SubtitleEncoding::Gbk => "gbk",
            SubtitleEncoding::Big5 => "big5",
            SubtitleEncoding::Latin1 => "latin1",
        }
    }
}

/// Result of decoding one subtitle file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DecodedSubtitle {
    /// Decoded text.
    pub text: String,
    /// Detected encoding.
    pub encoding: SubtitleEncoding,
    /// `true` when the bytes were not valid in the detected encoding and had to
    /// be repaired rather than decoded strictly.
    ///
    /// Always `false` today: every decoder above is strict and a strict failure
    /// is reported as [`SubtitleDecodeError`] instead. The field is reserved for
    /// a future lenient fallback.
    pub lossy: bool,
}

impl DecodedSubtitle {
    /// The Dart `_nameOf` name of the encoding that won.
    pub fn name(&self) -> &'static str {
        self.encoding.name()
    }
}

/// Why a subtitle payload could not be decoded as text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleDecodeError {
    /// The payload is not text: empty input, a VobSub/MPEG `.sub` header, or
    /// NUL-heavy bytes that do not look like UTF-16.
    Binary,
    /// The payload is not valid in the encoding that was tried.
    NotText,
}

impl SubtitleDecodeError {
    /// The machine-readable reason used by the FFI JSON reply.
    pub fn reason(self) -> &'static str {
        match self {
            SubtitleDecodeError::Binary => "binary",
            SubtitleDecodeError::NotText => "not_text",
        }
    }
}

impl std::fmt::Display for SubtitleDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason())
    }
}

impl std::error::Error for SubtitleDecodeError {}

/// Decode subtitle bytes, detecting the charset.
///
/// Returns [`SubtitleDecodeError::Binary`] for payloads that are not text and
/// [`SubtitleDecodeError::NotText`] when no candidate encoding decodes the
/// payload cleanly.
pub fn decode(bytes: &[u8]) -> Result<DecodedSubtitle, SubtitleDecodeError> {
    decode_with(bytes, None)
}

/// Decode subtitle bytes as `encoding`, skipping detection.
///
/// The forced path applies the same corruption check as detection, exactly like
/// the Dart `decodeSubtitleBytes(bytes, override: ...)`.
pub fn decode_as(bytes: &[u8], encoding: SubtitleEncoding) -> Result<DecodedSubtitle, SubtitleDecodeError> {
    decode_with(bytes, Some(encoding))
}

fn decode_with(
    bytes: &[u8],
    forced: Option<SubtitleEncoding>,
) -> Result<DecodedSubtitle, SubtitleDecodeError> {
    if bytes.is_empty() {
        return Err(SubtitleDecodeError::Binary);
    }
    if looks_like_binary_subtitle(bytes)
        && !has_text_bom(bytes)
        && !looks_like_utf16(bytes, bytes.len().min(MAX_PROBE))
    {
        return Err(SubtitleDecodeError::Binary);
    }
    if let Some(encoding) = forced {
        return match decode_as_encoding(encoding, bytes) {
            Some(text) if !too_corrupt(&text) => Ok(DecodedSubtitle {
                text,
                encoding,
                lossy: false,
            }),
            // Dart's caller picked the reason from the binary sniff; do the same
            // so a `.sub` that fails a forced decode still reports `binary`.
            _ => Err(decode_failure_reason(bytes)),
        };
    }
    auto_detect(bytes)
}

fn decode_failure_reason(bytes: &[u8]) -> SubtitleDecodeError {
    if looks_like_binary_subtitle(bytes) {
        SubtitleDecodeError::Binary
    } else {
        SubtitleDecodeError::NotText
    }
}

fn auto_detect(bytes: &[u8]) -> Result<DecodedSubtitle, SubtitleDecodeError> {
    // A textual BOM wins before scoring, exactly like the Dart original.
    if let Some(candidate) = decode_bom(bytes) {
        if !too_corrupt(&candidate.text) {
            return Ok(candidate);
        }
    }

    let mut candidates = Vec::new();
    consider_candidate(&mut candidates, SubtitleEncoding::Utf8, strip_utf8_bom(bytes));
    if should_try_utf16(bytes) {
        consider_candidate(&mut candidates, SubtitleEncoding::Utf16Le, bytes);
        consider_candidate(&mut candidates, SubtitleEncoding::Utf16Be, bytes);
    }
    consider_candidate(&mut candidates, SubtitleEncoding::Gbk, bytes);
    consider_candidate(&mut candidates, SubtitleEncoding::Big5, bytes);
    if candidates.is_empty() {
        return Err(SubtitleDecodeError::NotText);
    }

    // Dart sorts ascending: lower score wins, ties go to the lower preference.
    candidates.sort_by(|a, b| {
        score(a, bytes)
            .cmp(&score(b, bytes))
            .then_with(|| preference(a.name()).cmp(&preference(b.name())))
    });
    Ok(candidates.remove(0))
}

fn consider_candidate(
    candidates: &mut Vec<DecodedSubtitle>,
    encoding: SubtitleEncoding,
    bytes: &[u8],
) {
    let Some(text) = decode_as_encoding(encoding, bytes) else {
        return;
    };
    if too_corrupt(&text) {
        return;
    }
    candidates.push(DecodedSubtitle {
        text,
        encoding,
        lossy: false,
    });
}

/// Strict decode of `bytes` as `encoding`; `None` when the payload is invalid.
fn decode_as_encoding(encoding: SubtitleEncoding, bytes: &[u8]) -> Option<String> {
    match encoding {
        SubtitleEncoding::Utf8 => {
            strict_decode(UTF_8, strip_utf8_bom(bytes)).map(remove_first_bom_char)
        }
        SubtitleEncoding::Utf16Le | SubtitleEncoding::Utf16Be => {
            if bytes.len() < 2 || !bytes.len().is_multiple_of(2) {
                return None;
            }
            let decoder = if matches!(encoding, SubtitleEncoding::Utf16Le) {
                UTF_16LE
            } else {
                UTF_16BE
            };
            let text = strict_decode(decoder, bytes)?;
            // The Dart decoder substitutes U+FFFD and then rejects any text that
            // contains it, so a payload that legitimately encodes U+FFFD is
            // rejected as well.
            if text.contains(REPLACEMENT_CHAR) {
                return None;
            }
            Some(remove_first_bom_char(text))
        }
        SubtitleEncoding::Gbk => strict_decode(GBK, bytes),
        SubtitleEncoding::Big5 => strict_decode(BIG5, bytes),
        SubtitleEncoding::Latin1 => None,
    }
}

/// Strict (non-replacing) decode of `bytes` as `encoding`.
///
/// The workspace disables the `alloc` feature of `encoding_rs`, so the
/// `Cow`-returning convenience API is unavailable; the incremental API writes
/// into a caller-sized buffer instead. `None` means the payload is not valid in
/// that encoding.
fn strict_decode(encoding: &'static Encoding, bytes: &[u8]) -> Option<String> {
    let mut decoder = encoding.new_decoder_without_bom_handling();
    let capacity = decoder.max_utf8_buffer_length_without_replacement(bytes.len())?;
    let mut output = vec![0_u8; capacity];
    let (result, _read, written) =
        decoder.decode_to_utf8_without_replacement(bytes, &mut output, true);
    if result != DecoderResult::InputEmpty {
        return None;
    }
    output.truncate(written);
    String::from_utf8(output).ok()
}

/// Strict encode of `text` as `encoding`; `None` when a character is unmappable.
fn strict_encode(encoding: &'static Encoding, text: &str) -> Option<Vec<u8>> {
    let mut encoder = encoding.new_encoder();
    let capacity = encoder.max_buffer_length_from_utf8_without_replacement(text.len())?;
    let mut output = vec![0_u8; capacity];
    let (result, read, written) =
        encoder.encode_from_utf8_without_replacement(text, &mut output, true);
    if result != EncoderResult::InputEmpty || read != text.len() {
        return None;
    }
    output.truncate(written);
    Some(output)
}

/// Remove the first U+FEFF the way Dart's `String.replaceFirst('\uFEFF', '')`
/// does (first occurrence anywhere, not only a prefix).
fn remove_first_bom_char(text: String) -> String {
    match text.find(BOM_CHAR) {
        Some(index) => {
            let mut text = text;
            text.replace_range(index..index + BOM_CHAR.len_utf8(), "");
            text
        }
        None => text,
    }
}

/// Decode a payload that starts with a textual BOM.
fn decode_bom(bytes: &[u8]) -> Option<DecodedSubtitle> {
    if bytes.len() >= 3 && bytes[0..3] == UTF8_BOM {
        let text = decode_as_encoding(SubtitleEncoding::Utf8, &bytes[3..])?;
        return Some(DecodedSubtitle {
            text,
            encoding: SubtitleEncoding::Utf8,
            lossy: false,
        });
    }
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        let text = decode_as_encoding(SubtitleEncoding::Utf16Le, bytes)?;
        return Some(DecodedSubtitle {
            text,
            encoding: SubtitleEncoding::Utf16Le,
            lossy: false,
        });
    }
    if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
        let text = decode_as_encoding(SubtitleEncoding::Utf16Be, bytes)?;
        return Some(DecodedSubtitle {
            text,
            encoding: SubtitleEncoding::Utf16Be,
            lossy: false,
        });
    }
    None
}

fn has_text_bom(bytes: &[u8]) -> bool {
    if bytes.len() >= 3 && bytes[0..3] == UTF8_BOM {
        return true;
    }
    bytes.len() >= 2
        && ((bytes[0] == 0xFF && bytes[1] == 0xFE)
            || (bytes[0] == 0xFE && bytes[1] == 0xFF))
}

fn strip_utf8_bom(bytes: &[u8]) -> &[u8] {
    if bytes.len() >= 3 && bytes[0..3] == UTF8_BOM {
        &bytes[3..]
    } else {
        bytes
    }
}

fn should_try_utf16(bytes: &[u8]) -> bool {
    if bytes.len() < 4 || !bytes.len().is_multiple_of(2) {
        return false;
    }
    looks_like_utf16(bytes, bytes.len().min(MAX_PROBE))
}

/// VobSub / MPEG packet `.sub` payloads are not text. UTF-16 text is not binary.
fn looks_like_binary_subtitle(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    if bytes.len() >= 4 && bytes[0] == 0x00 && bytes[1] == 0x00 && bytes[2] == 0x01 {
        return true;
    }
    let probe = bytes.len().min(MAX_PROBE);
    let nuls = bytes[..probe].iter().filter(|byte| **byte == 0).count();
    if nuls == 0 {
        return false;
    }
    !looks_like_utf16(bytes, probe)
}

fn looks_like_utf16(bytes: &[u8], probe: usize) -> bool {
    if bytes.len() >= 2
        && ((bytes[0] == 0xFF && bytes[1] == 0xFE) || (bytes[0] == 0xFE && bytes[1] == 0xFF))
    {
        return bytes.len().is_multiple_of(2);
    }
    if probe < 8 || !bytes.len().is_multiple_of(2) {
        return false;
    }
    let pairs = probe / 2;
    let mut zero_even = 0;
    let mut zero_odd = 0;
    for pair in 0..pairs {
        if bytes[pair * 2] == 0 {
            zero_even += 1;
        }
        if bytes[pair * 2 + 1] == 0 {
            zero_odd += 1;
        }
    }
    (zero_even as f64) > pairs as f64 * 0.6 || (zero_odd as f64) > pairs as f64 * 0.6
}

/// Preference order for equal scores; lower wins.
fn preference(name: &str) -> u8 {
    match name {
        "utf-8" => 0,
        "utf-16le" => 1,
        "utf-16be" => 2,
        "gbk" => 3,
        "gb18030" => 4,
        "big5" => 5,
        _ => 9,
    }
}

fn score(decoded: &DecodedSubtitle, original: &[u8]) -> i64 {
    let mut bad = 0_i64;
    let mut cjk = 0_i64;
    let mut runes = 0_i64;
    for rune in decoded.text.chars() {
        runes += 1;
        let code = rune as u32;
        if code == 0xFFFD {
            bad += 8;
        }
        if code < 0x20 && code != 0x09 && code != 0x0A && code != 0x0D {
            bad += 4;
        }
        if (0x4E00..=0x9FFF).contains(&code) {
            cjk += 1;
        }
    }
    let mut score = bad * 100 - cjk;
    if round_trips(decoded, original) {
        score -= 10_000;
    }
    if runes == 0 {
        score += 100_000;
    }
    score
}

fn round_trips(decoded: &DecodedSubtitle, original: &[u8]) -> bool {
    let (encoding, expected) = match decoded.encoding {
        SubtitleEncoding::Utf8 => (UTF_8, strip_utf8_bom(original)),
        SubtitleEncoding::Gbk => (GBK, original),
        SubtitleEncoding::Big5 => (BIG5, original),
        _ => return false,
    };
    strict_encode(encoding, &decoded.text).is_some_and(|bytes| bytes == expected)
}

fn too_corrupt(text: &str) -> bool {
    if text.is_empty() {
        return true;
    }
    let mut bad = 0_usize;
    let mut total = 0_usize;
    for rune in text.chars() {
        total += 1;
        let code = rune as u32;
        if code == 0xFFFD || (code < 0x20 && code != 9 && code != 10 && code != 13) {
            bad += 1;
        }
    }
    total > 0 && bad as f64 / total as f64 > 0.25
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded_bytes(encoding: &'static Encoding, text: &str) -> Vec<u8> {
        strict_encode(encoding, text).unwrap_or_else(|| panic!("{text:?} cannot be encoded losslessly"))
    }

    fn gbk_bytes(text: &str) -> Vec<u8> {
        encoded_bytes(GBK, text)
    }

    fn big5_bytes(text: &str) -> Vec<u8> {
        encoded_bytes(BIG5, text)
    }

    fn utf16le_bytes(text: &str) -> Vec<u8> {
        text.encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    fn utf16be_bytes(text: &str) -> Vec<u8> {
        text.encode_utf16()
            .flat_map(u16::to_be_bytes)
            .collect()
    }

    #[test]
    fn empty_payload_is_binary() {
        assert_eq!(decode(&[]), Err(SubtitleDecodeError::Binary));
    }

    #[test]
    fn vobsub_header_is_binary() {
        assert_eq!(decode(&[0x00, 0x00, 0x01, 0xBA]), Err(SubtitleDecodeError::Binary));
    }

    #[test]
    fn nul_heavy_utf16_without_bom_is_still_text() {
        let bytes = utf16le_bytes("hello");
        let decoded = decode(&bytes).expect("utf16 text");
        // Inherited Dart quirk, not a porting mistake: without a BOM each ASCII
        // UTF-16LE code unit turns into a CJK code point when read as
        // UTF-16BE, and `_score` rewards CJK characters, so the big-endian
        // candidate outscores the correct little-endian one. The point of this
        // test is that the binary probe lets null-heavy UTF-16 through at all.
        assert_eq!(decoded.encoding, SubtitleEncoding::Utf16Be);
    }

    #[test]
    fn utf8_bom_is_stripped() {
        let mut bytes = UTF8_BOM.to_vec();
        bytes.extend_from_slice(b"hello");
        let decoded = decode(&bytes).expect("utf8 text");
        assert_eq!(decoded.encoding, SubtitleEncoding::Utf8);
        assert_eq!(decoded.text, "hello");
    }

    #[test]
    fn plain_utf8_text_decodes() {
        let decoded = decode(b"hello world").expect("utf8 text");
        assert_eq!(decoded.encoding, SubtitleEncoding::Utf8);
        assert_eq!(decoded.text, "hello world");
        assert!(!decoded.lossy);
    }

    #[test]
    fn utf8_chinese_decodes_as_utf8() {
        // Odd byte length: a GBK decode would leave a dangling lead byte, so the
        // GBK candidate cannot win this payload.
        let bytes = "你好吗".as_bytes();
        assert_eq!(bytes.len() % 2, 1);
        let decoded = decode(bytes).expect("utf8 chinese");
        assert_eq!(decoded.encoding, SubtitleEncoding::Utf8);
        assert_eq!(decoded.text, "你好吗");
    }

    #[test]
    fn gbk_chinese_decodes_as_gbk() {
        let bytes = gbk_bytes("你好");
        let decoded = decode(&bytes).expect("gbk chinese");
        assert_eq!(decoded.encoding, SubtitleEncoding::Gbk);
        assert_eq!(decoded.text, "你好");
    }

    #[test]
    fn bytes_valid_in_gbk_and_big5_pick_gbk_by_preference() {
        // 0xB0 0xA1 is a legal GBK pair (啊) and a legal Big5 pair, both
        // round-tripping. Scores tie, so the preference table decides: gbk
        // (3) before big5 (5), which is the shipped Dart behaviour.
        let bytes = [0xB0, 0xA1];
        assert!(decode_as(&bytes, SubtitleEncoding::Big5).is_ok(), "payload must be Big5-valid too");
        let decoded = decode(&bytes).expect("gbk pair");
        assert_eq!(decoded.encoding, SubtitleEncoding::Gbk);
        assert_eq!(decoded.text, "啊");
    }

    #[test]
    fn utf16le_with_bom_decodes() {
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend_from_slice(&utf16le_bytes("hi"));
        let decoded = decode(&bytes).expect("utf16le");
        assert_eq!(decoded.encoding, SubtitleEncoding::Utf16Le);
        assert_eq!(decoded.text, "hi");
    }

    #[test]
    fn utf16be_with_bom_decodes() {
        let mut bytes = vec![0xFE, 0xFF];
        bytes.extend_from_slice(&utf16be_bytes("hi"));
        let decoded = decode(&bytes).expect("utf16be");
        assert_eq!(decoded.encoding, SubtitleEncoding::Utf16Be);
        assert_eq!(decoded.text, "hi");
    }

    #[test]
    fn forced_decode_rejects_wrong_encoding() {
        let bytes = gbk_bytes("你好");
        assert_eq!(
            decode_as(&bytes, SubtitleEncoding::Utf8),
            Err(SubtitleDecodeError::NotText)
        );
    }

    #[test]
    fn forced_decode_accepts_explicit_encoding() {
        let bytes = gbk_bytes("你好");
        let decoded = decode_as(&bytes, SubtitleEncoding::Gbk).expect("forced gbk");
        assert_eq!(decoded.text, "你好");
        assert_eq!(decoded.encoding, SubtitleEncoding::Gbk);

        let traditional = big5_bytes("繁體中文");
        let decoded = decode_as(&traditional, SubtitleEncoding::Big5).expect("forced big5");
        assert_eq!(decoded.text, "繁體中文");
        assert_eq!(decoded.encoding, SubtitleEncoding::Big5);
    }

    #[test]
    fn forced_utf16le_strips_bom() {
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend_from_slice(&utf16le_bytes("hi"));
        let decoded = decode_as(&bytes, SubtitleEncoding::Utf16Le).expect("forced utf16le");
        assert_eq!(decoded.text, "hi");
    }

    #[test]
    fn forced_latin1_is_not_implemented() {
        assert_eq!(
            decode_as(b"hello", SubtitleEncoding::Latin1),
            Err(SubtitleDecodeError::NotText)
        );
    }

    #[test]
    fn four_byte_gb18030_decodes_via_the_gbk_variant() {
        // Non-BMP text encodes as 4-byte GB18030 sequences; `encoding_rs::GBK`
        // is the GB18030 decoder, so it accepts them (the Dart GbkCodec did
        // not). The text must survive; only the reported name differs.
        let bytes = encoded_bytes(encoding_rs::GB18030, "😀");
        assert_eq!(bytes.len(), 4);
        let decoded = decode(&bytes).expect("four byte gb18030");
        assert_eq!(decoded.text, "😀");
        assert_eq!(decoded.encoding, SubtitleEncoding::Gbk);
    }

    #[test]
    fn control_character_payload_is_not_text() {
        assert_eq!(decode(&[0x01, 0x02, 0x03, 0x04]), Err(SubtitleDecodeError::NotText));
    }

    #[test]
    fn too_corrupt_threshold_is_strictly_greater_than_a_quarter() {
        assert!(!too_corrupt("ab\u{1}d"), "1/4 is not above the threshold");
        assert!(too_corrupt("a\u{1}\u{2}d"), "2/4 is above the threshold");
        assert!(too_corrupt(""));
    }

    #[test]
    fn preference_orders_simplified_before_traditional() {
        assert_eq!(preference("utf-8"), 0);
        assert_eq!(preference("gbk"), 3);
        assert_eq!(preference("gb18030"), 4);
        assert_eq!(preference("big5"), 5);
        assert!(preference("gbk") < preference("big5"));
    }

    #[test]
    fn score_rewards_round_trips_and_cjk() {
        let decoded = DecodedSubtitle {
            text: "你好".to_string(),
            encoding: SubtitleEncoding::Utf8,
            lossy: false,
        };
        let bytes = "你好".as_bytes();
        // bad = 0, cjk = 2, round-trip bonus = -10000.
        assert_eq!(score(&decoded, bytes), -10_002);
    }

    #[test]
    fn utf16_plausibility_requires_even_length_and_zero_pattern() {
        assert!(!looks_like_utf16(b"abc", 3));
        assert!(!looks_like_utf16(b"hello world", 11));
        assert!(looks_like_utf16(&utf16le_bytes("hello"), 10));
        let mut bom = vec![0xFF, 0xFE];
        bom.extend_from_slice(&utf16le_bytes("hi"));
        assert!(looks_like_utf16(&bom, bom.len()));
        assert!(!looks_like_utf16(&[0xFF, 0xFE, 0x00], 3));
    }
}
