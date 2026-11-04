//! Subtitle text handling shared by the transcode engine and the FFI layer.
//!
//! Two responsibilities, both pure Rust (no libav calls, host-testable):
//!
//! - classifying a libav subtitle codec name into "text track I can convert",
//!   "bitmap track I cannot render" or "other";
//! - converting decoded subtitle text + timings into a WebVTT document.
//!
//! The libav demux/decode loop itself lives in [`crate::transcoder`] behind the
//! `ffmpeg` feature; it calls [`cue_text_from_decoded`] with the decoder output
//! (`AVSubtitleRect.text` / `AVSubtitleRect.ass`).

/// Maximum cues kept from one subtitle track (safety cap for pathological files).
pub const MAX_CUES: usize = 100_000;

/// One subtitle cue: display window in milliseconds + plain-text body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cue {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

/// How a subtitle stream can be delivered to a DLNA renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    /// Text subtitle (SRT/ASS/SSA/MOV_TEXT/WebVTT) — convertible to WebVTT.
    Text,
    /// Bitmap subtitle (PGS/DVD/DVB) — cannot be converted without rendering.
    Bitmap,
    /// Anything else — not offered as an embedded candidate.
    Other,
}

/// Classify a libav codec name (`avcodec_get_name`) into a [`TrackKind`].
pub fn classify_codec(codec: &str) -> TrackKind {
    match codec.to_ascii_lowercase().as_str() {
        "subrip" | "srt" | "ass" | "ssa" | "mov_text" | "webvtt" | "text" | "subviewer"
        | "subviewer1" | "microdvd" | "sami" | "sami_utf8" | "realtext" | "vplayer" | "stl"
        | "mpl2" | "pjs" | "jacosub" | "scc" | "ttml" | "eia_608" => TrackKind::Text,
        "hdmv_pgs_subtitle" | "dvd_subtitle" | "dvb_subtitle" | "dvb_teletext" | "xsub" => {
            TrackKind::Bitmap
        }
        _ => TrackKind::Other,
    }
}

/// Human-readable reason a subtitle track cannot be delivered, or `None` when
/// it is a text track that [`to_webvtt`] can serve.
pub fn unsupported_reason(codec: &str) -> Option<&'static str> {
    match classify_codec(codec) {
        TrackKind::Text => None,
        TrackKind::Bitmap => Some("bitmap subtitles (PGS/DVD/DVB) cannot be converted to WebVTT"),
        TrackKind::Other => Some("subtitle format is not supported for HLS cast"),
    }
}

/// Container extensions whose embedded subtitle tracks are probed for a cast.
///
/// Probing opens the source file and enumerates its subtitle tracks. That is
/// cheap enough for MP4-family sources and pays off: an HLS cast cannot carry
/// the container's tracks, so text tracks are extracted and side-loaded as
/// WebVTT. Matroska/WebM/AVI/TS sources routinely carry dozens of tracks (30+
/// language variants are common), enumerating them is the most expensive part
/// of cast preparation, and those sources are cast direct so the renderer
/// demuxes its own tracks. They therefore get the external sidecar scan only
/// (contract `.tmp/contract/preset-encoder-tiers.md` §8).
pub const MP4_FAMILY_EXTS: &[&str] = &["mp4", "m4v", "mov", "3gp", "3g2", "m4a", "m4b", "mp4a"];

/// Whether `name` is an MP4-family container, i.e. one whose embedded subtitle
/// tracks may be probed for an HLS cast.
///
/// `name` may be a plain file name or a `/`-separated remote path; a trailing
/// `?query` / `#fragment` (WebDAV/DLNA signed URLs sometimes carry one) is
/// ignored. Case-insensitive. A name without an extension is not MP4-family.
pub fn container_supports_embedded_subs(name: &str) -> bool {
    let name = name.split(['?', '#']).next().unwrap_or(name);
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => {
            MP4_FAMILY_EXTS.contains(&ext.to_ascii_lowercase().as_str())
        }
        _ => false,
    }
}

/// Normalize decoded subtitle text: drop ASS override tags, normalize line
/// breaks, trim trailing blanks and collapse blank lines.
///
/// Returns `None` when nothing visible remains.
pub fn normalize_text(text: &str) -> Option<String> {
    let stripped = strip_ass_tags(text);
    let mut lines: Vec<String> = Vec::new();
    for line in stripped
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
    {
        let line = line.trim_end().to_string();
        if line.trim().is_empty() {
            // Keep an intentional blank line only between text lines.
            if !lines.is_empty() && !lines.last().is_some_and(|l: &String| l.is_empty()) {
                lines.push(String::new());
            }
            continue;
        }
        lines.push(line);
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    if lines.iter().all(|l| l.is_empty()) {
        return None;
    }
    Some(lines.join("\n"))
}

/// Build a cue body from decoder output.
///
/// `text` is `AVSubtitleRect.text` (plain UTF-8), `ass` is
/// `AVSubtitleRect.ass` (ASS/SSA event line). `text` wins when it is
/// non-empty; the ASS line is parsed (first 9 comma fields dropped) as a
/// fallback. ASS styling is dropped — text content and line breaks are kept.
pub fn cue_text_from_decoded(text: Option<&str>, ass: Option<&str>) -> Option<String> {
    if let Some(text) = text {
        if let Some(normalized) = normalize_text(text) {
            return Some(normalized);
        }
    }
    ass.and_then(ass_event_body)
}

/// Extract the visible text from an ASS/SSA event line.
///
/// Accepts both a full `Dialogue: 0,0:00:01.00,0:00:04.00,Default,,0,0,0,,Text`
/// line and a bare event body (`Text`). The event body is the 10th
/// comma-separated field.
pub fn ass_event_body(line: &str) -> Option<String> {
    let body = if line.len() >= 9 && line[..9].eq_ignore_ascii_case("Dialogue:") {
        split_ass_event(&line[9..])
    } else {
        line
    };
    normalize_text(body)
}

/// Take the 10th comma-separated field of an ASS event (text may contain commas).
fn split_ass_event(event: &str) -> &str {
    let event = event.trim_start_matches(' ');
    let mut rest = event;
    for _ in 0..9 {
        match rest.find(',') {
            Some(idx) => rest = &rest[idx + 1..],
            None => return rest,
        }
    }
    rest
}

/// Remove ASS override blocks (`{\i1}`, `{\pos(1,2)}`) and decode the
/// in-line escapes: `\N`/`\n` → newline, `\h` → non-breaking space.
pub fn strip_ass_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' => {
                // Drop everything up to the closing brace (if any).
                for inner in chars.by_ref() {
                    if inner == '}' {
                        break;
                    }
                }
            }
            '\\' => match chars.peek() {
                Some('N') | Some('n') => {
                    chars.next();
                    out.push('\n');
                }
                Some('h') => {
                    chars.next();
                    out.push('\u{00a0}');
                }
                _ => out.push(c),
            },
            _ => out.push(c),
        }
    }
    out
}

/// Sort cues by start time, drop empty/invalid ones, merge exact duplicates and
/// cap the result at [`MAX_CUES`].
pub fn normalize_cues(mut cues: Vec<Cue>) -> Vec<Cue> {
    for cue in &mut cues {
        if cue.end_ms <= cue.start_ms {
            cue.end_ms = cue.start_ms.saturating_add(1_000);
        }
        cue.text = cue.text.trim().to_string();
    }
    cues.retain(|c| !c.text.is_empty());
    cues.sort_by_key(|cue| (cue.start_ms, cue.end_ms));
    cues.dedup_by(|a, b| a.start_ms == b.start_ms && a.end_ms == b.end_ms && a.text == b.text);
    cues.truncate(MAX_CUES);
    cues
}

/// WebVTT timestamp: `HH:MM:SS.mmm`.
pub fn format_timestamp(ms: u64) -> String {
    let ms = ms.min(99 * 3_600_000 + 59 * 60_000 + 59_999);
    let hours = ms / 3_600_000;
    let minutes = (ms % 3_600_000) / 60_000;
    let seconds = (ms % 60_000) / 1_000;
    let millis = ms % 1_000;
    format!("{hours:02}:{minutes:02}:{seconds:02}.{millis:03}")
}

/// Serialize cues as a WebVTT document (empty tracks still get a valid header).
pub fn to_webvtt(cues: &[Cue]) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for (offset, cue) in cues.iter().enumerate() {
        let index = offset + 1;
        // A cue payload must not contain the cue-separator sequence `-->`.
        let text = cue
            .text
            .replace("-->", "--\u{200b}>")
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        out.push_str(&format!(
            "{index}\n{} --> {}\n{}\n\n",
            format_timestamp(cue.start_ms),
            format_timestamp(cue.end_ms),
            text
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_codecs_by_family() {
        for text in ["subrip", "ass", "ssa", "mov_text", "webvtt", "text"] {
            assert_eq!(classify_codec(text), TrackKind::Text, "{text}");
            assert_eq!(unsupported_reason(text), None, "{text}");
        }
        for bitmap in [
            "hdmv_pgs_subtitle",
            "dvd_subtitle",
            "dvb_subtitle",
            "dvb_teletext",
        ] {
            assert_eq!(classify_codec(bitmap), TrackKind::Bitmap, "{bitmap}");
            assert!(unsupported_reason(bitmap).is_some(), "{bitmap}");
        }
        assert_eq!(classify_codec("unknown_thing"), TrackKind::Other);
        assert!(unsupported_reason("unknown_thing").is_some());
        // Case-insensitive: avcodec names are lower-case but callers may pass
        // stream metadata verbatim.
        assert_eq!(classify_codec("SubRip"), TrackKind::Text);
    }

    #[test]
    fn strips_ass_override_blocks() {
        assert_eq!(strip_ass_tags("{\\i1}Hello{\\i0} world"), "Hello world");
        assert_eq!(strip_ass_tags("{\\pos(10,20)}Top"), "Top");
        assert_eq!(strip_ass_tags("no tags here"), "no tags here");
        assert_eq!(strip_ass_tags("{\\b1}"), "");
    }

    #[test]
    fn decodes_ass_line_breaks() {
        assert_eq!(strip_ass_tags("line1\\Nline2"), "line1\nline2");
        assert_eq!(strip_ass_tags("line1\\nline2"), "line1\nline2");
        assert_eq!(strip_ass_tags("a\\hb"), "a\u{00a0}b");
    }

    #[test]
    fn parses_ass_dialogue_body() {
        let line = "Dialogue: 0,0:00:01.00,0:00:04.00,Default,,0,0,0,,Hello, world";
        assert_eq!(ass_event_body(line).as_deref(), Some("Hello, world"));
        // Bare bodies are accepted too.
        assert_eq!(ass_event_body("Just text").as_deref(), Some("Just text"));
        // Timing-only / empty dialogues produce nothing.
        assert_eq!(
            ass_event_body("Dialogue: 0,0:00:01.00,0:00:04.00,Default,,0,0,0,,"),
            None
        );
        assert_eq!(ass_event_body("   "), None);
    }

    #[test]
    fn parses_ass_dialogue_with_commas_in_style_fields() {
        let line = "Dialogue: 0,0:00:01.00,0:00:04.00,Default,,0,0,0,,{\\an8}Think, fast";
        assert_eq!(ass_event_body(line).as_deref(), Some("Think, fast"));
    }

    #[test]
    fn cue_text_prefers_plain_text_over_ass() {
        assert_eq!(
            cue_text_from_decoded(
                Some("plain"),
                Some("Dialogue: 0,0:00:01.00,0:00:04.00,D,,0,0,0,,ass")
            )
            .as_deref(),
            Some("plain")
        );
        assert_eq!(
            cue_text_from_decoded(
                Some("   "),
                Some("Dialogue: 0,0:00:01.00,0:00:04.00,D,,0,0,0,,ass")
            )
            .as_deref(),
            Some("ass")
        );
        assert_eq!(cue_text_from_decoded(Some(""), Some("")), None);
        assert_eq!(cue_text_from_decoded(None, None), None);
    }

    #[test]
    fn keeps_line_breaks_and_drops_trailing_blank_lines() {
        assert_eq!(
            normalize_text("first\r\nsecond\n\n").as_deref(),
            Some("first\nsecond")
        );
        assert_eq!(normalize_text("a\n\n\nb").as_deref(), Some("a\n\nb"));
        assert_eq!(normalize_text("   \n\t "), None);
    }

    #[test]
    fn formats_webvtt_timestamps() {
        assert_eq!(format_timestamp(0), "00:00:00.000");
        assert_eq!(format_timestamp(1_000), "00:00:01.000");
        assert_eq!(format_timestamp(61_500), "00:01:01.500");
        assert_eq!(format_timestamp(3_600_000), "01:00:00.000");
        assert_eq!(format_timestamp(3_661_234), "01:01:01.234");
        assert_eq!(format_timestamp(u64::MAX), "99:59:59.999");
    }

    #[test]
    fn normalizes_cues_sorts_and_fixes_invalid_ranges() {
        let cues = normalize_cues(vec![
            Cue {
                start_ms: 5_000,
                end_ms: 6_000,
                text: " second ".into(),
            },
            Cue {
                start_ms: 1_000,
                end_ms: 4_000,
                text: "first".into(),
            },
            Cue {
                start_ms: 9_000,
                end_ms: 9_000,
                text: "zero length".into(),
            },
            Cue {
                start_ms: 10_000,
                end_ms: 11_000,
                text: "   ".into(),
            },
        ]);
        assert_eq!(cues.len(), 3);
        assert_eq!(cues[0].text, "first");
        assert_eq!(cues[1].text, "second");
        assert_eq!(cues[2].text, "zero length");
        assert_eq!(cues[2].end_ms, 10_000);
    }

    #[test]
    fn caps_cue_count() {
        let cues = (0..MAX_CUES as u64 + 10)
            .map(|i| Cue {
                start_ms: i * 1_000,
                end_ms: i * 1_000 + 500,
                text: format!("c{i}"),
            })
            .collect();
        assert_eq!(normalize_cues(cues).len(), MAX_CUES);
    }

    #[test]
    fn emits_webvtt_document() {
        let vtt = to_webvtt(&[
            Cue {
                start_ms: 1_000,
                end_ms: 4_000,
                text: "Hello".into(),
            },
            Cue {
                start_ms: 6_500,
                end_ms: 10_000,
                text: "line one\nline two".into(),
            },
        ]);
        assert!(vtt.starts_with("WEBVTT\n\n"));
        assert!(vtt.contains("1\n00:00:01.000 --> 00:00:04.000\nHello\n\n"));
        assert!(vtt.contains("2\n00:00:06.500 --> 00:00:10.000\nline one\nline two\n\n"));
    }

    #[test]
    fn escapes_webvtt_payload() {
        let vtt = to_webvtt(&[Cue {
            start_ms: 0,
            end_ms: 1_000,
            text: "a < b & c > d --> e".into(),
        }]);
        // The zero-width space breaks the forbidden `-->` sequence inside a
        // cue payload; the `>` is still entity-escaped like every other one.
        assert!(vtt.contains("a &lt; b &amp; c &gt; d --\u{200b}&gt; e"));
        // Only the two timestamp lines may contain the cue separator.
        assert_eq!(vtt.matches("-->").count(), 1);
    }

    #[test]
    fn empty_track_is_still_valid_webvtt() {
        assert_eq!(to_webvtt(&[]), "WEBVTT\n\n");
    }

    #[test]
    fn only_mp4_family_containers_support_embedded_probing() {
        for name in [
            "movie.mp4",
            "MOVIE.MP4",
            "clip.m4v",
            "trailer.mov",
            "shot.3gp",
            "old.3g2",
            "song.m4a",
            "book.m4b",
            "audio.mp4a",
            "/Remote/Downloads/Movie.Mov",
            "movie.mp4?token=abc123",
            // A synthetic embedded candidate path must not crash the check.
            "movie.MP4#embedded:2",
        ] {
            assert!(
                container_supports_embedded_subs(name),
                "{name} should be MP4-family"
            );
        }

        for name in [
            "movie.mkv",
            "MOVIE.MKV",
            "movie.webm",
            "movie.avi",
            "movie.ts",
            "movie.m2ts",
            "movie.flv",
            "movie.wmv",
            "movie.mpg",
            "movie.mka",
            "movie.srt",
            "movie.mp4.srt",
            "no_extension",
            "",
            ".mp4",
        ] {
            assert!(
                !container_supports_embedded_subs(name),
                "{name} must not be probed for embedded subtitles"
            );
        }
    }
}
