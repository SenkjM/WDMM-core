//! Subtitle charset detection and decoding (audit item 4).
//!
//! Slice owner: port the behaviour of `lib/utils/subtitle_encoding.dart`
//! (BOM/UTF-8/UTF-16/GBK/Big5 sniffer, the `_score` heuristic and the
//! round-trip checks) on top of `encoding_rs` instead of the hand-written
//! GB18030 table.

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
    Latin1,
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
    pub lossy: bool,
}

/// Decode subtitle bytes, detecting the charset.
pub fn decode(_bytes: &[u8]) -> DecodedSubtitle {
    DecodedSubtitle {
        text: String::new(),
        encoding: SubtitleEncoding::Utf8,
        lossy: true,
    }
}
