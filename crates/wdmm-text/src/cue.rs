//! CUE sheet parsing (audit item 2).
//!
//! Slice owner: port `lib/utils/cue_sheet.dart:99 class CueSheetParser` here
//! verbatim in behaviour (same tolerance, same defaults) and return the sheet as
//! `serde` structs so `wdmm-ffi`'s `ext::cue` can serialise it.

use thiserror::Error;

/// Parse error for a CUE sheet.
#[derive(Debug, Error)]
pub enum CueError {
    /// The text contained no `TRACK` entries.
    #[error("no tracks in cue sheet")]
    NoTracks,
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
    pub pregap_ms: Option<i64>,
}

/// A parsed sheet: the album-level fields plus every track.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CueSheet {
    /// Album `TITLE`, empty when absent.
    pub title: String,
    /// Album `PERFORMER`, empty when absent.
    pub performer: String,
    /// `FILE` name as written, empty when absent.
    pub file_name: String,
    /// Parsed tracks in sheet order.
    pub tracks: Vec<CueTrack>,
}

/// Parse CUE sheet text.
pub fn parse(_text: &str) -> Result<CueSheet, CueError> {
    Err(CueError::NoTracks)
}
