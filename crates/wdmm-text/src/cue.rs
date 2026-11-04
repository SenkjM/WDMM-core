//! CUE sheet parsing (audit item 2).
//!
//! Ported from `lib/utils/cue_sheet.dart` (`class CueSheetParser`) with the same
//! tolerance and defaults: case-insensitive keywords, tabs/repeated whitespace,
//! quoted or bare `FILE` names, one or more `FILE` sections, `REM GENRE`/`REM
//! DATE`, `INDEX 00`/`INDEX 01` in `mm:ss:ff` (75 frames per second), and a
//! UTF-8 BOM on the first line. A sheet without a complete `TRACK` (number +
//! `FILE` + `INDEX 01`) is rejected as a whole, exactly like the Dart state
//! machine did (it flagged `incomplete` and returned `null`).
//!
//! `wdmm-ffi`'s `ext::cue` serialises [`CueSheet`] to JSON; the Dart side keeps
//! only the model and calls `wdmm_cue_parse`.

use thiserror::Error;

/// Parse error for a CUE sheet.
#[derive(Debug, Error)]
pub enum CueError {
    /// The text contained no `TRACK` entries.
    #[error("no tracks in cue sheet")]
    NoTracks,
}

impl CueError {
    /// Stable machine code used by the FFI `{"ok":false,"error":…}` reply.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoTracks => "no_tracks",
        }
    }
}

/// One parsed track (`TRACK nn AUDIO` + its `INDEX 01` / `TITLE` / `PERFORMER`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CueTrack {
    /// Track number as written in the sheet.
    pub number: u32,
    /// `TITLE` of the track, empty when absent.
    pub title: String,
    /// `PERFORMER` of the track, empty when absent.
    pub performer: String,
    /// `INDEX 01` expressed in milliseconds from the start of the file.
    pub start_ms: i64,
    /// `INDEX 00` (pregap) in milliseconds, `None` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pregap_ms: Option<i64>,
    /// `FILE` this track belongs to (a sheet may reference several files).
    pub file_name: String,
    /// `ISRC` line of the track, `None` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isrc: Option<String>,
}

/// A parsed sheet: the album-level fields plus every track.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CueSheet {
    /// Album `TITLE`, empty when absent.
    pub title: String,
    /// Album `PERFORMER`, empty when absent.
    pub performer: String,
    /// First `FILE` name as written (`files[0]`), empty when absent.
    pub file_name: String,
    /// Every distinct `FILE` name in sheet order.
    #[serde(default)]
    pub files: Vec<String>,
    /// `REM GENRE` value (quotes stripped), `None` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rem_genre: Option<String>,
    /// `REM DATE` value (quotes stripped), `None` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rem_date: Option<String>,
    /// Parsed tracks in sheet order.
    pub tracks: Vec<CueTrack>,
}

/// Parse CUE sheet text; `NoTracks` when the text is not a usable sheet.
pub fn parse(text: &str) -> Result<CueSheet, CueError> {
    try_parse(text).ok_or(CueError::NoTracks)
}

/// Parse CUE sheet text, returning `None` when the Dart `tryParse` would.
pub fn try_parse(text: &str) -> Option<CueSheet> {
    if text.trim().is_empty() {
        return None;
    }
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut parser = Parser::default();
    for raw in normalized.split('\n') {
        let mut line = raw.trim();
        if let Some(rest) = line.strip_prefix('\u{feff}') {
            line = rest.trim();
        }
        if line.is_empty() {
            continue;
        }
        let upper = line.to_uppercase();

        if upper.starts_with("REM ") {
            let rem = after_prefix(line, 4).trim();
            let rem_upper = rem.to_uppercase();
            if rem_upper.starts_with("GENRE ") {
                parser.rem_genre = Some(unquote(after_prefix(rem, 6).trim()));
            } else if rem_upper.starts_with("DATE ") {
                parser.rem_date = Some(unquote(after_prefix(rem, 5).trim()));
            }
            continue;
        }
        if upper.starts_with("TITLE ") {
            let value = unquote(after_prefix(line, 6).trim());
            if parser.in_track {
                parser.track_title = value;
            } else {
                parser.album_title = value;
            }
            continue;
        }
        if upper.starts_with("PERFORMER ") {
            let value = unquote(after_prefix(line, 10).trim());
            if parser.in_track {
                parser.track_performer = value;
            } else {
                parser.album_performer = value;
            }
            continue;
        }
        if upper.starts_with("FILE ") {
            parser.flush();
            let file = parse_file_name(after_prefix(line, 5).trim())?;
            if file.is_empty() {
                return None;
            }
            if !parser.files.contains(&file) {
                parser.files.push(file.clone());
            }
            parser.current_file = Some(file);
            continue;
        }
        if upper.starts_with("TRACK ") {
            parser.flush();
            let current_file = parser.current_file.as_deref()?;
            if current_file.is_empty() {
                return None;
            }
            let number: u32 = after_prefix(line, 6)
                .split_whitespace()
                .next()?
                .parse()
                .ok()?;
            if number < 1 {
                return None;
            }
            parser.in_track = true;
            parser.track_number = Some(number);
            continue;
        }
        if upper.starts_with("INDEX ") {
            if !parser.in_track {
                continue;
            }
            let mut parts = after_prefix(line, 6).split_whitespace();
            let Some(index_raw) = parts.next() else {
                continue;
            };
            let Some(time_raw) = parts.next() else {
                continue;
            };
            let Ok(index) = index_raw.parse::<i64>() else {
                continue;
            };
            if index == 1 {
                parser.start_ms = Some(parse_index_time(time_raw)?);
            } else if index == 0 {
                // Pregap: the Dart parser ignored INDEX 00 entirely, so an
                // unparsable one is tolerated rather than fatal.
                parser.pregap_ms = parse_index_time(time_raw);
            }
            continue;
        }
        if upper.starts_with("ISRC ") && parser.in_track {
            parser.track_isrc = Some(unquote(after_prefix(line, 5).trim()));
        }
    }
    parser.flush();
    if parser.incomplete || parser.files.is_empty() || parser.tracks.is_empty() {
        return None;
    }
    let file_name = parser.files.first().cloned().unwrap_or_default();
    Some(CueSheet {
        title: parser.album_title,
        performer: parser.album_performer,
        file_name,
        files: parser.files,
        rem_genre: parser.rem_genre,
        rem_date: parser.rem_date,
        tracks: parser.tracks,
    })
}

/// Incremental state of the line scanner (mirrors the Dart local variables).
#[derive(Default)]
struct Parser {
    album_title: String,
    album_performer: String,
    rem_genre: Option<String>,
    rem_date: Option<String>,
    current_file: Option<String>,
    files: Vec<String>,
    tracks: Vec<CueTrack>,
    track_number: Option<u32>,
    track_title: String,
    track_performer: String,
    track_isrc: Option<String>,
    start_ms: Option<i64>,
    pregap_ms: Option<i64>,
    in_track: bool,
    incomplete: bool,
}

impl Parser {
    /// Close the current track. A track without `FILE`/`INDEX 01` marks the
    /// whole sheet incomplete, matching the Dart `flush()` closure.
    fn flush(&mut self) {
        if !self.in_track || self.track_number.is_none() {
            return;
        }
        let file = self.current_file.clone().unwrap_or_default();
        match (self.start_ms, file.is_empty()) {
            (Some(start_ms), false) => self.tracks.push(CueTrack {
                number: self.track_number.take().unwrap_or_default(),
                title: std::mem::take(&mut self.track_title),
                performer: std::mem::take(&mut self.track_performer),
                start_ms,
                pregap_ms: self.pregap_ms.take(),
                file_name: file,
                isrc: self.track_isrc.take(),
            }),
            _ => {
                self.incomplete = true;
                self.pregap_ms = None;
                self.track_isrc = None;
            }
        }
        self.in_track = false;
        self.track_number = None;
        self.start_ms = None;
        self.track_title.clear();
        self.track_performer.clear();
    }
}

/// Substring after `chars` characters, as Dart `line.substring(n)`.
fn after_prefix(line: &str, chars: usize) -> &str {
    match line.char_indices().nth(chars) {
        Some((index, _)) => &line[index..],
        None => "",
    }
}

/// Parse a `FILE` name: double-quoted, then single-quoted, then first token.
fn parse_file_name(rest: &str) -> Option<String> {
    if let Some(body) = rest.strip_prefix('"') {
        if let Some(end) = body.find('"') {
            return Some(body[..end].to_string());
        }
    }
    if let Some(body) = rest.strip_prefix('\'') {
        if let Some(end) = body.find('\'') {
            return Some(body[..end].to_string());
        }
    }
    rest.split_whitespace().next().map(str::to_string)
}

/// Strip one pair of matching quotes, like the Dart `_uq` helper.
fn unquote(value: &str) -> String {
    if value.len() >= 2 {
        if let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
            return inner.to_string();
        }
        if let Some(inner) = value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
            return inner.to_string();
        }
    }
    value.to_string()
}

/// Parse `mm:ss:ff` (75 frames per second) into milliseconds.
fn parse_index_time(raw: &str) -> Option<i64> {
    let mut parts = raw.trim().split(':');
    let minutes: i64 = parts.next()?.parse().ok()?;
    let seconds: i64 = parts.next()?.parse().ok()?;
    let frames: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    if minutes < 0 || !(0..=59).contains(&seconds) || !(0..=74).contains(&frames) {
        return None;
    }
    let frame_ms = (frames as f64 * 1000.0 / 75.0).round() as i64;
    Some((minutes * 60 + seconds) * 1000 + frame_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
REM GENRE \"Rock\"
REM DATE 1999
PERFORMER \"Album Artist\"
TITLE \"Great Album\"
FILE \"great.flac\" WAVE
  TRACK 01 AUDIO
    TITLE \"Intro\"
    PERFORMER \"Band\"
    INDEX 01 00:00:00
  TRACK 02 AUDIO
    TITLE \"Main Song\"
    INDEX 01 01:30:00
";

    #[test]
    fn parses_standard_cue() {
        let sheet = parse(SAMPLE).expect("standard sheet");
        assert_eq!(sheet.tracks.len(), 2);
        assert_eq!(sheet.title, "Great Album");
        assert_eq!(sheet.performer, "Album Artist");
        assert_eq!(sheet.file_name, "great.flac");
        assert_eq!(sheet.files, vec!["great.flac".to_string()]);
        assert_eq!(sheet.rem_genre.as_deref(), Some("Rock"));
        assert_eq!(sheet.rem_date.as_deref(), Some("1999"));
        assert_eq!(sheet.tracks[0].number, 1);
        assert_eq!(sheet.tracks[0].title, "Intro");
        assert_eq!(sheet.tracks[0].performer, "Band");
        assert_eq!(sheet.tracks[0].start_ms, 0);
        assert_eq!(sheet.tracks[0].file_name, "great.flac");
        assert_eq!(sheet.tracks[1].number, 2);
        assert_eq!(sheet.tracks[1].title, "Main Song");
        assert_eq!(sheet.tracks[1].start_ms, 90_000);
    }

    #[test]
    fn keeps_album_performer_for_track_fallback() {
        // `mergeCueOverFileTags` falls back to the album performer; the parse
        // step must keep the album value separate from the empty track value.
        let sheet = parse(SAMPLE).expect("standard sheet");
        assert_eq!(sheet.tracks[1].performer, "");
        assert_eq!(sheet.performer, "Album Artist");
    }

    #[test]
    fn parses_frame_offsets_at_75_fps() {
        let sheet = parse("FILE \"a.flac\" WAVE\n  TRACK 01 AUDIO\n    INDEX 01 01:30:37\n")
            .expect("frame offsets");
        // 37 frames = 493.33 ms, rounded to 493 like the Dart `round()`.
        assert_eq!(sheet.tracks[0].start_ms, 90_493);
        let sheet = parse("FILE \"a.flac\" WAVE\n  TRACK 01 AUDIO\n    INDEX 01 00:00:01\n")
            .expect("one frame");
        assert_eq!(sheet.tracks[0].start_ms, 13);
    }

    #[test]
    fn captures_index_00_as_pregap() {
        let sheet = parse(
            "FILE \"a.flac\" WAVE\n  TRACK 01 AUDIO\n    INDEX 00 00:00:02\n    INDEX 01 00:00:37\n",
        )
        .expect("pregap sheet");
        assert_eq!(sheet.tracks[0].start_ms, 493);
        // 2 frames = 26.67 ms, rounded to 27 like the Dart `round()`.
        assert_eq!(sheet.tracks[0].pregap_ms, Some(27));
    }

    #[test]
    fn tolerates_bad_index_00() {
        // The Dart parser only validated the INDEX 01 line; INDEX 00 was skipped.
        let sheet = parse(
            "FILE \"a.flac\" WAVE\n  TRACK 01 AUDIO\n    INDEX 00 nonsense\n    INDEX 01 00:01:00\n",
        )
        .expect("bad pregap tolerated");
        assert_eq!(sheet.tracks[0].start_ms, 1000);
        assert_eq!(sheet.tracks[0].pregap_ms, None);
    }

    #[test]
    fn tracks_multiple_files_in_sheet_order() {
        let sheet = parse(
            "TITLE \"Album\"\n\
             FILE \"first.flac\" WAVE\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n\
             FILE \"second.flac\" WAVE\n  TRACK 02 AUDIO\n    INDEX 01 00:00:00\n",
        )
        .expect("multi file");
        assert_eq!(
            sheet.files,
            vec!["first.flac".to_string(), "second.flac".to_string()]
        );
        assert_eq!(sheet.file_name, "first.flac");
        assert_eq!(sheet.tracks.len(), 2);
        assert_eq!(sheet.tracks[0].file_name, "first.flac");
        assert_eq!(sheet.tracks[1].file_name, "second.flac");
    }

    #[test]
    fn dedupes_repeated_file_sections() {
        let sheet = parse(
            "FILE \"a.flac\" WAVE\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n\
             FILE \"a.flac\" WAVE\n  TRACK 02 AUDIO\n    INDEX 01 00:01:00\n",
        )
        .expect("repeated file");
        assert_eq!(sheet.files, vec!["a.flac".to_string()]);
    }

    #[test]
    fn accepts_quoted_and_bare_file_names() {
        let quux =
            parse("FILE \"with space.flac\" WAVE\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n")
                .expect("double quoted");
        assert_eq!(quux.file_name, "with space.flac");
        let single = parse("FILE 'single.flac' WAVE\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n")
            .expect("single quoted");
        assert_eq!(single.file_name, "single.flac");
        let bare =
            parse("FILE plain.flac WAVE\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n").expect("bare");
        assert_eq!(bare.file_name, "plain.flac");
    }

    #[test]
    fn is_case_insensitive_and_space_tolerant() {
        // The keyword itself must be followed by a space (the Dart parser also
        // required `FILE `, `TRACK ` …); tabs are tolerated elsewhere.
        let sheet = parse(
            "  rem genre   \"Jazz\"\r\n\
             \tfile \t\"a.flac\"\tWAVE\r\n\
             \ttrack \t01\taudio\r\n\
             \t\ttitle \t\t\"Song\"\r\n\
             \t\tindex 1 00:00:00\r\n",
        )
        .expect("case and whitespace tolerance");
        assert_eq!(sheet.rem_genre.as_deref(), Some("Jazz"));
        assert_eq!(sheet.title, "");
        assert_eq!(sheet.tracks[0].title, "Song");
        assert_eq!(sheet.tracks[0].start_ms, 0);
    }

    #[test]
    fn strips_utf8_bom_from_first_line() {
        let sheet = parse("\u{feff}TITLE \"Album\"\nFILE \"a.flac\" WAVE\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n")
            .expect("bom");
        assert_eq!(sheet.title, "Album");
        assert_eq!(sheet.tracks.len(), 1);
    }

    #[test]
    fn captures_track_isrc() {
        let sheet = parse(
            "FILE \"a.flac\" WAVE\n  TRACK 01 AUDIO\n    ISRC \"USABC1234567\"\n    INDEX 01 00:00:00\n",
        )
        .expect("isrc");
        assert_eq!(sheet.tracks[0].isrc.as_deref(), Some("USABC1234567"));
        let unquoted =
            parse("FILE \"a.flac\" WAVE\n  TRACK 01 AUDIO\n    ISRC USABC1234567\n    INDEX 01 00:00:00\n")
                .expect("isrc unquoted");
        assert_eq!(unquoted.tracks[0].isrc.as_deref(), Some("USABC1234567"));
    }

    #[test]
    fn ignores_unknown_lines_and_index_02() {
        let sheet = parse(
            "CATALOG 12345\n\
             FILE \"a.flac\" WAVE\n\
               TRACK 01 AUDIO\n\
                 FLAGS DCP\n\
                 INDEX 02 00:00:10\n\
                 INDEX 01 00:00:00\n",
        )
        .expect("unknown lines");
        assert_eq!(sheet.tracks.len(), 1);
        assert_eq!(sheet.tracks[0].start_ms, 0);
    }

    #[test]
    fn rejects_empty_text() {
        assert!(parse("").is_err());
        assert!(parse("   \n\t\n").is_err());
        assert!(try_parse("").is_none());
    }

    #[test]
    fn rejects_text_without_tracks() {
        assert!(parse("TITLE only").is_err());
        assert_eq!(parse("TITLE only").unwrap_err().code(), "no_tracks");
    }

    #[test]
    fn rejects_track_without_index_01() {
        let text = "FILE \"a.flac\" WAVE\n  TRACK 01 AUDIO\n    TITLE \"A\"\n  TRACK 02 AUDIO\n    INDEX 01 00:00:00\n";
        assert!(try_parse(text).is_none());
    }

    #[test]
    fn rejects_track_before_file() {
        assert!(try_parse("  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n").is_none());
    }

    #[test]
    fn rejects_bad_index_time() {
        for time in [
            "nonsense",
            "00:00",
            "00:00:00:00",
            "00:60:00",
            "00:00:75",
            "-1:00:00",
        ] {
            let text = format!("FILE \"a.flac\" WAVE\n  TRACK 01 AUDIO\n    INDEX 01 {time}\n");
            assert!(try_parse(&text).is_none(), "accepted bad index time {time}");
        }
    }

    #[test]
    fn rejects_bad_track_numbers() {
        for number in ["0", "abc", "-1"] {
            let text =
                format!("FILE \"a.flac\" WAVE\n  TRACK {number} AUDIO\n    INDEX 01 00:00:00\n");
            assert!(
                try_parse(&text).is_none(),
                "accepted bad track number {number}"
            );
        }
    }

    #[test]
    fn rejects_empty_file_name() {
        assert!(try_parse("FILE \"\" WAVE\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n").is_none());
    }

    #[test]
    fn parses_index_without_zero_padding() {
        let sheet = parse("FILE \"a.flac\" WAVE\n  TRACK 1 AUDIO\n    INDEX 1 00:02:00\n")
            .expect("unpadded index");
        assert_eq!(sheet.tracks[0].number, 1);
        assert_eq!(sheet.tracks[0].start_ms, 2000);
    }

    #[test]
    fn track_title_after_index_applies_to_the_track() {
        // Dart kept `inTrack` true until the next FILE/TRACK, so a TITLE after
        // INDEX 01 still belongs to the track.
        let sheet = parse(
            "FILE \"a.flac\" WAVE\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n    TITLE \"Late\"\n",
        )
        .expect("late title");
        assert_eq!(sheet.tracks[0].title, "Late");
        assert_eq!(sheet.title, "");
    }
}
