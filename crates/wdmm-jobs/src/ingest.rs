//! Library ingest composition: tag payload → `LibraryTrack` row → artist/album
//! upsert → aggregate/FTS rebuild. Ported from `LibraryService.ingestFromTags`
//! and friends; the identity-hash rules must stay byte-for-byte compatible so
//! existing rows keep matching.
//!
//! The three public entry points are the frozen contract other executors
//! compile against:
//!
//! * [`ingest_downloaded`] — a transfer finished; read tags from the local file
//!   (Dart `LibraryService.ingestDownloaded`) and write the row.
//! * [`ingest_remote_file`] — a scan item; probe tags through the VFS byte-range
//!   reader (Dart `LibraryService.ingestFromTags`), including CUE sheets.
//! * [`ingest_retag`] — re-read an existing cloud row (Dart `ingestFromTags`
//!   called from the retag executor).
//!
//! CUE albums keep the Dart slice semantics: one `CueAlbum` row plus one
//! `CueSlice` row per `TRACK`, with virtual remote paths (`<audio>#cue:<n>`),
//! clip ranges, and per-slice cover thumbnails.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use lofty::file::{AudioFile, TaggedFileExt};
use lofty::picture::PictureType;
use lofty::probe::Probe;
use lofty::tag::{Accessor, ItemKey, Tag};
use serde::{Deserialize, Serialize};

use wdmm_core::models::LibraryTrack;
use wdmm_drivers::WdmmVfs;
use wdmm_storage::{CueAlbum, CueSlice};
use wdmm_text::cue::{self, CueSheet, CueTrack};

use crate::thumbs::{self, COVER_THUMB_SIZE};
use crate::JobCtx;

/// What ingest produced, for diagnostics and the task's `result_json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IngestOutcome {
    /// Rows inserted or updated.
    pub rows: u32,
    /// Display title of the primary row, when there is exactly one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Whether cover art was stored (thumbnail written to `JobPaths::cover_dir`).
    #[serde(default)]
    pub cover: bool,
}

/// Embedded covers larger than this are ignored, matching
/// `wdmm-ffi/src/ext/tags.rs` (`MAX_COVER_BYTES`).
const MAX_COVER_BYTES: usize = 8 * 1024 * 1024;

/// Byte-range block size for the remote `Read + Seek` adapter. Same value as
/// the FFI tag reader so probe traffic stays comparable.
const READ_BLOCK: u64 = 256 * 1024;

/// KV key for the sidecar-cover toggle (`SettingsService` writes it).
const SIDECAR_ENABLED_KEY: &str = "audio_stream_sidecar_cover";
/// KV key holding the configured sidecar base names.
const SIDECAR_NAMES_KEY: &str = "audio_stream_sidecar_names";
/// Default sidecar base names (`stream_window.dart`).
const DEFAULT_SIDECAR_NAMES: &str = "cover, folder, front, album";
/// Extensions tried for sidecar covers (`stream_window.dart`).
const SIDECAR_IMAGE_EXTENSIONS: [&str; 4] = ["jpg", "jpeg", "png", "webp"];

/// `1970-01-01T00:00:00.000Z` — Dart's `DateTime.fromMillisecondsSinceEpoch(0)
/// .toUtc().toIso8601String()`.
const EPOCH0: &str = "1970-01-01T00:00:00.000Z";

/// Audio extensions considered by scans (`wdmm_ffi::ext::walk`).
pub(crate) const AUDIO_EXTENSIONS: [&str; 8] =
    ["mp3", "flac", "m4a", "aac", "wav", "ogg", "opus", "wma"];
/// Video extensions considered by scans (`wdmm_ffi::ext::walk`).
pub(crate) const VIDEO_EXTENSIONS: [&str; 13] = [
    "mp4", "mkv", "avi", "mov", "webm", "flv", "ts", "m4v", "wmv", "3gp", "mpg", "mpeg", "mp2",
];

/// Where the tag values come from. `TagsOnly` mirrors
/// `ingestDownloaded` (the freshly read tags are authoritative); `MergeExisting`
/// mirrors `ingestFromTags` (`tag ?? existing`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldPolicy {
    TagsOnly,
    MergeExisting,
}

// ── public entry points ─────────────────────────────────────────────────────

/// Ingest one finished download into the library.
///
/// `ctx.task` carries account/remote path/kind and the local file path the
/// transfer wrote; the target directory layout is the one `cache_paths` derives.
pub async fn ingest_downloaded(ctx: &JobCtx) -> Result<IngestOutcome, String> {
    let local = ctx
        .task
        .local_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| "err.ingest.noLocalPath".to_string())?;

    let bytes = tokio::fs::read(local)
        .await
        .map_err(|error| format!("err.ingest.readLocal|{local}: {error}"))?;

    let remote = if ctx.task.remote_path.trim().is_empty() {
        local
    } else {
        ctx.task.remote_path.as_str()
    };
    let file_name = if ctx.task.file_name.trim().is_empty() {
        basename(remote).to_string()
    } else {
        ctx.task.file_name.clone()
    };

    if is_cue_path(&file_name) {
        let (outcome, _) = ingest_cue_bytes(ctx, remote, &bytes, true).await?;
        return Ok(outcome);
    }

    let tags = extract_tags(io::Cursor::new(bytes.as_slice()))
        .map_err(|error| format!("err.ingest.tags|{local}: {error}"))?;
    upsert_single(ctx, remote, &file_name, tags, FieldPolicy::TagsOnly).await
}

/// Ingest one remote file discovered by a scan: fetch tags/sidecar cover through
/// the VFS, write the track row, artist/album rows, thumbnail, and CUE slices.
pub async fn ingest_remote_file(ctx: &JobCtx) -> Result<IngestOutcome, String> {
    let remote = ctx.task.remote_path.trim();
    if remote.is_empty() {
        return Err("err.ingest.noRemotePath".into());
    }
    let file_name = if ctx.task.file_name.trim().is_empty() {
        basename(remote).to_string()
    } else {
        ctx.task.file_name.clone()
    };
    ingest_path(ctx, remote, &file_name, false).await
}

/// Re-read the remote tags of an existing library row (retag jobs).
pub async fn ingest_retag(ctx: &JobCtx) -> Result<IngestOutcome, String> {
    // Retag is the same probe/write path as a scan item; the difference is that
    // the executor runs it for one already-known remote path.
    ingest_remote_file(ctx).await
}

// ── shared remote path ──────────────────────────────────────────────────────

/// One remote path, already resolved to a file name. Shared with `scan.rs` so a
/// scan item and a download ingest cannot drift apart.
pub(crate) async fn ingest_path(
    ctx: &JobCtx,
    remote_path: &str,
    file_name: &str,
    downloaded: bool,
) -> Result<IngestOutcome, String> {
    if is_cue_path(file_name) || is_cue_path(remote_path) {
        let bytes = ctx
            .vfs
            .get_content(remote_path)
            .await
            .map_err(|error| format!("err.cloud.notFound|{remote_path}: {error}"))?;
        let (outcome, _) = ingest_cue_bytes(ctx, remote_path, &bytes, downloaded).await?;
        return Ok(outcome);
    }

    let item = ctx
        .vfs
        .get(remote_path)
        .await
        .map_err(|error| format!("err.cloud.notFound|{remote_path}: {error}"))?;
    let mut tags = read_remote_tags(ctx, remote_path, item.size).await?;
    if tags.cover.is_none() {
        if let Some(cover) = sidecar_cover(ctx, remote_path).await {
            tags.cover = Some(cover);
        }
    }
    let policy = if downloaded {
        FieldPolicy::TagsOnly
    } else {
        FieldPolicy::MergeExisting
    };
    upsert_single(ctx, remote_path, file_name, tags, policy).await
}

// ── single-track upsert ─────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct ReadTags {
    title: Option<String>,
    artist: Option<String>,
    album_artist: Option<String>,
    album: Option<String>,
    duration_ms: Option<i64>,
    track_number: Option<i32>,
    disc_number: Option<i32>,
    year: Option<i32>,
    genre: Option<String>,
    cover: Option<Vec<u8>>,
}

async fn upsert_single(
    ctx: &JobCtx,
    remote_path: &str,
    file_name: &str,
    tags: ReadTags,
    policy: FieldPolicy,
) -> Result<IngestOutcome, String> {
    let source = ctx.task.source_name.as_str();
    let existing = ctx
        .db
        .get_track_by_source_path(source, remote_path)
        .map_err(|error| format!("err.storage.track|{remote_path}: {error}"))?;
    let old = existing.as_ref();

    // Dart: `coverState = existing?.coverState ?? 0`, then 2 when saveThumb
    // succeeds. A missing cover in this read never downgrades an existing row.
    let mut cover_state = old.map(|track| track.cover_state).unwrap_or(0);
    let mut cover_written = false;
    if let Some(bytes) = tags.cover.as_deref() {
        let dest = thumbs::thumb_path(&ctx.paths.cover_dir, source, remote_path);
        match thumbs::write_thumb_at(bytes, &dest, COVER_THUMB_SIZE) {
            Ok(()) => {
                cover_state = 2;
                cover_written = true;
            }
            Err(error) => ctx
                .progress
                .note(&format!("cover thumb failed: {remote_path}: {error}")),
        }
    }

    let merged = |tag: Option<String>, old: Option<String>| match policy {
        FieldPolicy::TagsOnly => tag,
        FieldPolicy::MergeExisting => tag.or(old),
    };

    // Dart falls back to the file name when the tags carry no title; the column
    // is NOT NULL, so never write an empty string.
    let title = merged(non_blank(tags.title), non_blank(old.map(|track| track.title.clone())))
        .or_else(|| non_blank(Some(file_name.to_string())))
        .or_else(|| non_blank(Some(basename(remote_path).to_string())))
        .unwrap_or_else(|| remote_path.to_string());

    let track = LibraryTrack {
        id: old
            .map(|track| track.id.clone())
            .filter(|id| !id.trim().is_empty())
            .unwrap_or_else(|| thumbs::music_id_for_remote(source, remote_path)),
        title,
        artist: merged(non_blank(tags.artist), old.and_then(|track| track.artist.clone())),
        remote_path: remote_path.to_string(),
        // The library's `account_id` column stores the display source name; the
        // FFI `json_to_track` mapping does the same (`account_id: sourceName`).
        account_id: source.to_string(),
        size: old.map(|track| track.size).unwrap_or(0),
        modified: old.and_then(|track| track.modified),
        album_id: None,
        artist_id: None,
        album: merged(non_blank(tags.album), old.and_then(|track| track.album.clone())),
        album_artist: merged(
            non_blank(tags.album_artist),
            old.and_then(|track| track.album_artist.clone()),
        ),
        duration_ms: tags
            .duration_ms
            .or_else(|| old.map(|track| track.duration_ms))
            .unwrap_or(0)
            .max(0),
        genre: merged(non_blank(tags.genre), old.and_then(|track| track.genre.clone())),
        track_number: tags
            .track_number
            .or_else(|| old.and_then(|track| track.track_number)),
        disc_number: tags
            .disc_number
            .or_else(|| old.and_then(|track| track.disc_number)),
        year: tags
            .year
            .filter(|year| *year > 0)
            .or_else(|| old.and_then(|track| track.year)),
        mime_type: old.and_then(|track| track.mime_type.clone()),
        rev: now_millis(),
        cover_state,
    };

    ctx.db
        .upsert_track(&track)
        .map_err(|error| format!("err.storage.upsert|{remote_path}: {error}"))?;

    Ok(IngestOutcome {
        rows: 1,
        title: Some(track.title.clone()),
        cover: cover_written,
    })
}

fn non_blank(value: Option<String>) -> Option<String> {
    value.filter(|text| !text.trim().is_empty())
}

// ── CUE albums ──────────────────────────────────────────────────────────────

/// Decode, parse, and ingest a CUE sheet. Returns the outcome plus the resolved
/// audio remote paths (the scan executor adds them to its `coveredByCue` set).
pub(crate) async fn ingest_cue_bytes(
    ctx: &JobCtx,
    cue_remote_path: &str,
    bytes: &[u8],
    downloaded: bool,
) -> Result<(IngestOutcome, Vec<String>), String> {
    let text = decode_cue_text(bytes);
    let sheet = cue::parse(&text).map_err(|_| format!("err.cueMalformed|{cue_remote_path}"))?;
    let audio = audio_remote_paths(cue_remote_path, &sheet.files);
    let outcome = ingest_cue_sheet(ctx, cue_remote_path, &sheet, &audio, downloaded).await?;
    Ok((outcome, audio))
}

async fn ingest_cue_sheet(
    ctx: &JobCtx,
    cue_remote_path: &str,
    sheet: &CueSheet,
    audio: &[String],
    downloaded: bool,
) -> Result<IngestOutcome, String> {
    let source = ctx.task.source_name.as_str();
    let album_title = first_non_empty(&[sheet.title.as_str()])
        .unwrap_or_else(|| basename(cue_remote_path).to_string());

    let existing = ctx
        .db
        .remaining_slices_for_cue(source, cue_remote_path)
        .map_err(|error| format!("err.storage.cue|{cue_remote_path}: {error}"))?;

    if existing > 0 {
        // Dart only re-reads when the album is absent, or when a slice claims a
        // cover but its thumbnail file is gone. Then only thumbs are refreshed.
        if cue_cover_missing(ctx, source, audio, sheet)? {
            if let Some(bytes) = cue_cover(ctx, audio).await {
                let refreshed = refresh_cue_covers(ctx, source, audio, sheet, &bytes)?;
                return Ok(IngestOutcome {
                    rows: 0,
                    title: Some(album_title),
                    cover: refreshed > 0,
                });
            }
        }
        return Ok(IngestOutcome {
            rows: 0,
            title: Some(album_title),
            cover: false,
        });
    }

    let cover = cue_cover(ctx, audio).await;
    let ranges = cue_clip_ranges(&sheet.tracks);
    let now = iso8601_now();
    let rev = now_millis();
    let cache_group = thumbs::cue_cache_group_id(source, cue_remote_path);
    let cue_id = thumbs::cue_id_for(source, cue_remote_path);
    let track_total = i64::try_from(sheet.tracks.len()).unwrap_or(i64::MAX);

    let mut slices = Vec::with_capacity(sheet.tracks.len());
    let mut covers_written = false;
    for (index, track) in sheet.tracks.iter().enumerate() {
        let resolved = resolve_audio_remote(audio, track);
        let virtual_path = thumbs::cue_virtual_remote_path(&resolved, track.number);
        let mut cover_state = 0;
        if let Some(bytes) = cover.as_deref() {
            let dest = thumbs::thumb_path(&ctx.paths.cover_dir, source, &virtual_path);
            match thumbs::write_thumb_at(bytes, &dest, COVER_THUMB_SIZE) {
                Ok(()) => {
                    cover_state = 2;
                    covers_written = true;
                }
                Err(error) => ctx
                    .progress
                    .note(&format!("cue cover failed: {virtual_path}: {error}")),
            }
        }

        let title = first_non_empty(&[track.title.as_str()]);
        let artist = first_non_empty(&[track.performer.as_str(), sheet.performer.as_str()]);
        let album_artist = first_non_empty(&[sheet.performer.as_str()]);
        let (clip_start_ms, clip_end_ms) = ranges[index];

        slices.push(CueSlice {
            music_id: thumbs::music_id_for_cue_slice(source, cue_remote_path, track.number),
            cue_id: cue_id.clone(),
            audio_music_id: thumbs::music_id_for_remote(source, &resolved),
            source_name: source.to_string(),
            remote_path: virtual_path,
            file_name: title
                .clone()
                .unwrap_or_else(|| track.number.to_string()),
            track_index: i64::from(track.number),
            title,
            artist,
            album_artist,
            album: first_non_empty(&[sheet.title.as_str()]),
            // Scrape path has no file durations; the slice duration stays null
            // and playback uses the clip range.
            duration_ms: None,
            track_number: Some(i64::from(track.number)),
            track_total: Some(track_total),
            disc_number: None,
            disc_total: None,
            year: cue_year(sheet.rem_date.as_deref()).map(i64::from),
            genre: first_non_empty(&[sheet.rem_genre.as_deref().unwrap_or("")]),
            bitrate: None,
            sample_rate: None,
            cover_state,
            audio_remote_path: Some(resolved),
            clip_start_ms: Some(clip_start_ms),
            clip_end_ms,
            cache_group_id: Some(cache_group.clone()),
            rev,
            last_downloaded_at: if downloaded {
                now.clone()
            } else {
                EPOCH0.to_string()
            },
            last_tag_read_at: now.clone(),
            remote_missing: 0,
        });
    }

    let album = CueAlbum {
        cue_id,
        source_name: source.to_string(),
        cue_remote_path: cue_remote_path.to_string(),
        title: slices.first().and_then(|slice| slice.album.clone()),
        performer: slices
            .first()
            .and_then(|slice| slice.album_artist.clone().or_else(|| slice.artist.clone())),
        cache_group_id: Some(cache_group),
        created_at: now,
    };

    // Dart deletes first (ignoring "not found") so removed tracks disappear.
    ctx.db
        .delete_cue_album(source, cue_remote_path)
        .map_err(|error| format!("err.storage.cue|{cue_remote_path}: {error}"))?;
    ctx.db
        .upsert_cue_album(&album)
        .map_err(|error| format!("err.storage.cue|{cue_remote_path}: {error}"))?;
    for slice in &slices {
        ctx.db
            .upsert_cue_slice(slice)
            .map_err(|error| format!("err.storage.cue|{}: {error}", slice.remote_path))?;
    }

    Ok(IngestOutcome {
        rows: u32::try_from(slices.len()).unwrap_or(u32::MAX),
        title: Some(album.title.clone().unwrap_or(album_title)),
        cover: covers_written,
    })
}

/// Dart `CueSheet.audioRemotePaths`: join each `FILE` under the cue's directory,
/// deduplicated in sheet order.
fn audio_remote_paths(cue_remote_path: &str, files: &[String]) -> Vec<String> {
    let dir = posix_dirname(cue_remote_path);
    let mut out: Vec<String> = Vec::new();
    for file in files {
        let joined = join_remote(&dir, file);
        if !out.contains(&joined) {
            out.push(joined);
        }
    }
    out
}

/// Dart `CueSheet.joinRemote`: strip any directory part of `FILE`, then put it
/// next to the cue sheet.
pub(crate) fn join_remote(dir: &str, file_name: &str) -> String {
    let cleaned = file_name.replace('\\', "/");
    let base = cleaned.rsplit('/').next().unwrap_or(&cleaned);
    if dir.is_empty() || dir == "/" {
        format!("/{base}")
    } else {
        format!("{}/{}", dir.trim_end_matches('/'), base)
    }
}

fn posix_dirname(path: &str) -> String {
    match path.rfind('/') {
        None => ".".to_string(),
        Some(0) => "/".to_string(),
        Some(index) => path[..index].to_string(),
    }
}

fn resolve_audio_remote(audio: &[String], track: &CueTrack) -> String {
    let cleaned = track.file_name.replace('\\', "/");
    let wanted = basename(&cleaned);
    audio
        .iter()
        .find(|path| basename(path) == wanted)
        .cloned()
        .or_else(|| audio.first().cloned())
        .unwrap_or_else(|| track.file_name.clone())
}

/// Dart `cueClipRanges`: end is the next track *of the same FILE*, else `None`.
fn cue_clip_ranges(tracks: &[CueTrack]) -> Vec<(i64, Option<i64>)> {
    let mut ranges = vec![(0_i64, None); tracks.len()];
    for (index, track) in tracks.iter().enumerate() {
        let end = tracks[index + 1..]
            .iter()
            .find(|other| other.file_name == track.file_name)
            .map(|other| other.start_ms);
        ranges[index] = (track.start_ms, end);
    }
    ranges
}

fn cue_year(rem_date: Option<&str>) -> Option<u32> {
    let date = rem_date?;
    let digits = date
        .split(|ch: char| !ch.is_ascii_digit())
        .find(|part| !part.is_empty())?;
    digits.parse().ok()
}

fn first_non_empty(values: &[&str]) -> Option<String> {
    values
        .iter()
        .map(|value| value.trim())
        .find(|value| !value.is_empty())
        .map(str::to_string)
}

async fn cue_cover(ctx: &JobCtx, audio: &[String]) -> Option<Vec<u8>> {
    for remote in audio {
        let Ok(item) = ctx.vfs.get(remote).await else {
            continue;
        };
        let Ok(tags) = read_remote_tags(ctx, remote, item.size).await else {
            continue;
        };
        if let Some(cover) = tags.cover {
            return Some(cover);
        }
    }
    None
}

fn cue_cover_missing(
    ctx: &JobCtx,
    source: &str,
    audio: &[String],
    sheet: &CueSheet,
) -> Result<bool, String> {
    for track in &sheet.tracks {
        let resolved = resolve_audio_remote(audio, track);
        let virtual_path = thumbs::cue_virtual_remote_path(&resolved, track.number);
        let Some(slice) = ctx
            .db
            .get_cue_slice_by_source_path(source, &virtual_path)
            .map_err(|error| format!("err.storage.cue|{virtual_path}: {error}"))?
        else {
            return Ok(true);
        };
        // 1 = None; only rows that claim a cover can have a missing thumb.
        if slice.cover_state != 1 {
            let dest = thumbs::thumb_path(&ctx.paths.cover_dir, source, &virtual_path);
            if !dest.exists() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn refresh_cue_covers(
    ctx: &JobCtx,
    source: &str,
    audio: &[String],
    sheet: &CueSheet,
    bytes: &[u8],
) -> Result<u32, String> {
    let mut refreshed = 0;
    for track in &sheet.tracks {
        let resolved = resolve_audio_remote(audio, track);
        let virtual_path = thumbs::cue_virtual_remote_path(&resolved, track.number);
        let Some(mut slice) = ctx
            .db
            .get_cue_slice_by_source_path(source, &virtual_path)
            .map_err(|error| format!("err.storage.cue|{virtual_path}: {error}"))?
        else {
            continue;
        };
        let dest = thumbs::thumb_path(&ctx.paths.cover_dir, source, &virtual_path);
        match thumbs::write_thumb_at(bytes, &dest, COVER_THUMB_SIZE) {
            Ok(()) => {
                slice.cover_state = 2;
                slice.rev = now_millis();
                slice.last_tag_read_at = iso8601_now();
                ctx.db
                    .upsert_cue_slice(&slice)
                    .map_err(|error| format!("err.storage.cue|{virtual_path}: {error}"))?;
                refreshed += 1;
            }
            Err(error) => ctx
                .progress
                .note(&format!("cue cover refresh failed: {virtual_path}: {error}")),
        }
    }
    Ok(refreshed)
}

// ── CUE text decoding ───────────────────────────────────────────────────────

/// Dart `decodeCueText`: UTF-8 BOM → lossy UTF-8, UTF-16 BOM → UTF-16,
/// otherwise strict UTF-8 with a lossy fallback.
fn decode_cue_text(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return decode_utf16(rest, true);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return decode_utf16(rest, false);
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            if little_endian {
                u16::from_le_bytes(*pair)
            } else {
                u16::from_be_bytes(*pair)
            }
        })
        .collect();
    String::from_utf16_lossy(&units)
}

// ── remote tag probing (VFS byte ranges) ────────────────────────────────────

trait RangeFetcher {
    /// Fetch `start..=end_inclusive`.
    fn fetch(&mut self, start: u64, end_inclusive: u64) -> Result<Vec<u8>, String>;
}

/// Bridges the synchronous `Read + Seek` adapter to async `WdmmVfs`.
struct VfsRangeFetcher {
    vfs: Arc<dyn WdmmVfs>,
    path: String,
    runtime: tokio::runtime::Handle,
}

impl RangeFetcher for VfsRangeFetcher {
    fn fetch(&mut self, start: u64, end_inclusive: u64) -> Result<Vec<u8>, String> {
        let (sender, receiver) = std::sync::mpsc::channel();
        let vfs = Arc::clone(&self.vfs);
        let path = self.path.clone();
        self.runtime.spawn(async move {
            let result = vfs
                .get_content_range(&path, start, Some(end_inclusive))
                .await
                .map_err(|error| error.to_string());
            // The receiver may already be gone when the probe aborts; that is
            // fine and must not panic.
            let _ = sender.send(result);
        });
        receiver
            .recv()
            .map_err(|_| "range fetch cancelled".to_string())?
    }
}

/// `Read + Seek` view over a remote file with a single-block cache, so lofty's
/// many small seeks do not turn into one request each. Ported from
/// `wdmm-ffi/src/ext/tags.rs`.
struct RemoteReader<F: RangeFetcher> {
    fetcher: F,
    size: u64,
    pos: u64,
    block_start: u64,
    block: Vec<u8>,
}

impl<F: RangeFetcher> RemoteReader<F> {
    fn new(fetcher: F, size: u64) -> Self {
        Self {
            fetcher,
            size,
            pos: 0,
            block_start: 0,
            block: Vec::new(),
        }
    }

    fn ensure_block(&mut self) -> io::Result<()> {
        if self.pos >= self.size {
            return Ok(());
        }
        let in_block =
            self.pos >= self.block_start && (self.pos - self.block_start) < self.block.len() as u64;
        if in_block {
            return Ok(());
        }
        let start = self.pos;
        let end = (start + READ_BLOCK - 1).min(self.size.saturating_sub(1));
        let block = self
            .fetcher
            .fetch(start, end)
            .map_err(io::Error::other)?;
        if block.is_empty() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "empty range"));
        }
        self.block_start = start;
        self.block = block;
        Ok(())
    }
}

impl<F: RangeFetcher> Read for RemoteReader<F> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.pos >= self.size {
            return Ok(0);
        }
        self.ensure_block()?;
        let offset = (self.pos - self.block_start) as usize;
        let available = self.block.len().saturating_sub(offset);
        if available == 0 {
            return Ok(0);
        }
        let read = available.min(buf.len());
        buf[..read].copy_from_slice(&self.block[offset..offset + read]);
        self.pos += read as u64;
        Ok(read)
    }
}

impl<F: RangeFetcher> Seek for RemoteReader<F> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let target: i128 = match from {
            SeekFrom::Start(offset) => i128::from(offset),
            SeekFrom::End(offset) => i128::from(self.size) + i128::from(offset),
            SeekFrom::Current(offset) => i128::from(self.pos) + i128::from(offset),
        };
        if target < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "negative seek"));
        }
        self.pos = u64::try_from(target)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "seek position overflow"))?;
        Ok(self.pos)
    }
}

async fn read_remote_tags(ctx: &JobCtx, remote: &str, size: u64) -> Result<ReadTags, String> {
    let fetcher = VfsRangeFetcher {
        vfs: Arc::clone(&ctx.vfs),
        path: remote.to_string(),
        runtime: tokio::runtime::Handle::current(),
    };
    tokio::task::spawn_blocking(move || extract_tags(RemoteReader::new(fetcher, size)))
        .await
        .map_err(|error| format!("err.ingest.tagTask|{remote}: {error}"))?
}

// ── tag extraction ──────────────────────────────────────────────────────────

fn extract_tags<R: Read + Seek>(reader: R) -> Result<ReadTags, String> {
    let tagged = {
        let probe = Probe::new(io::BufReader::new(reader))
            .guess_file_type()
            .map_err(|error| format!("probe: {error}"))?;
        probe.read().map_err(|error| format!("parse: {error}"))?
    };

    let properties = tagged.properties();
    let mut tags = ReadTags {
        duration_ms: Some(i64::try_from(properties.duration().as_millis()).unwrap_or(i64::MAX)),
        ..ReadTags::default()
    };

    let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) else {
        return Ok(tags);
    };

    tags.title = tag.title().map(|value| value.into_owned());
    tags.artist = tag.artist().map(|value| value.into_owned());
    tags.album_artist = tag.get_string(ItemKey::AlbumArtist).map(str::to_string);
    tags.album = tag.album().map(|value| value.into_owned());
    tags.genre = genre_of(tag);
    tags.track_number = tag.track().and_then(|value| i32::try_from(value).ok());
    tags.disc_number = tag.disk().and_then(|value| i32::try_from(value).ok());
    tags.year = year_of(tag).and_then(|value| i32::try_from(value).ok());
    tags.cover = cover_bytes(tag);
    Ok(tags)
}

/// Dart `ReadTags.genre`: all non-empty genres joined with `", "`.
fn genre_of(tag: &Tag) -> Option<String> {
    let genres: Vec<&str> = tag
        .get_strings(ItemKey::Genre)
        .map(str::trim)
        .filter(|genre| !genre.is_empty())
        .collect();
    if !genres.is_empty() {
        return Some(genres.join(", "));
    }
    tag.genre()
        .map(|genre| genre.trim().to_string())
        .filter(|genre| !genre.is_empty())
}

fn year_of(tag: &Tag) -> Option<u32> {
    if let Some(date) = tag.date() {
        return Some(u32::from(date.year));
    }
    for key in [
        ItemKey::Year,
        ItemKey::RecordingDate,
        ItemKey::OriginalReleaseDate,
    ] {
        let Some(text) = tag.get_string(key) else {
            continue;
        };
        let digits: String = text
            .chars()
            .filter(char::is_ascii_digit)
            .take(4)
            .collect();
        if digits.len() == 4 {
            if let Ok(year) = digits.parse::<u32>() {
                return Some(year);
            }
        }
    }
    None
}

/// Prefer the front cover; fall back to the first picture that fits the cap.
fn cover_bytes(tag: &Tag) -> Option<Vec<u8>> {
    let fits = |picture: &&lofty::picture::Picture| picture.data().len() <= MAX_COVER_BYTES;
    let picture = tag
        .pictures()
        .iter()
        .find(|picture| picture.pic_type() == PictureType::CoverFront && fits(picture))
        .or_else(|| tag.pictures().iter().find(fits))?;
    Some(picture.data().to_vec())
}

// ── sidecar covers ──────────────────────────────────────────────────────────

/// Probe a remote file for a cover without touching the library row.
///
/// Returns `Ok(Some(bytes))` when embedded or sidecar art is available,
/// `Ok(None)` when the read succeeded and the source has none, and `Err` when
/// the read itself failed. Callers must not treat the error path as "no cover":
/// a network blip would then permanently downgrade the row.
pub(crate) async fn probe_cover(
    ctx: &JobCtx,
    remote_path: &str,
    size: u64,
) -> Result<Option<Vec<u8>>, String> {
    let mut tags = read_remote_tags(ctx, remote_path, size).await?;
    if tags.cover.is_none() {
        if let Some(cover) = sidecar_cover(ctx, remote_path).await {
            tags.cover = Some(cover);
        }
    }
    Ok(tags.cover)
}

async fn sidecar_cover(ctx: &JobCtx, remote_path: &str) -> Option<Vec<u8>> {
    let enabled = ctx
        .db
        .load_kv(SIDECAR_ENABLED_KEY)
        .ok()
        .flatten()
        .map(|value| value.trim().eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if !enabled {
        return None;
    }

    let configured = ctx
        .db
        .load_kv(SIDECAR_NAMES_KEY)
        .ok()
        .flatten()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_SIDECAR_NAMES.to_string());

    let dir = posix_dirname(remote_path);
    let entries = ctx.vfs.list(&dir).await.ok()?;
    let names: Vec<String> = entries
        .into_iter()
        .filter(|item| !item.is_dir)
        .map(|item| item.name)
        .collect();
    let matched = match_sidecar_cover(&names, basename(remote_path), &configured)?;
    let cover_remote = join_remote(&dir, &matched);
    ctx.vfs
        .get_content(&cover_remote)
        .await
        .ok()
        .filter(|bytes| !bytes.is_empty())
}

/// Dart `parseSidecarNames`: split on `[,，;\s]+`, trim, strip separators.
fn parse_sidecar_names(raw: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for part in raw.split(|ch: char| ch == ',' || ch == '，' || ch == ';' || ch.is_whitespace()) {
        let cleaned = part.trim().replace(['\\', '/'], "");
        if cleaned.is_empty() {
            continue;
        }
        if !names
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&cleaned))
        {
            names.push(cleaned);
        }
    }
    names
}

/// Dart `matchSidecarCover`: configured base names first, then the audio stem.
fn match_sidecar_cover(
    file_names: &[String],
    audio_file_name: &str,
    configured: &str,
) -> Option<String> {
    let mut by_lower: Vec<(String, &str)> = Vec::new();
    for name in file_names {
        let lower = name.to_lowercase();
        if !by_lower.iter().any(|(existing, _)| existing == &lower) {
            by_lower.push((lower, name.as_str()));
        }
    }

    let mut bases = parse_sidecar_names(configured);
    let stem = match audio_file_name.rfind('.') {
        Some(index) if index > 0 => &audio_file_name[..index],
        _ => "",
    };
    if !stem.is_empty() && !bases.iter().any(|base| base.eq_ignore_ascii_case(stem)) {
        bases.push(stem.to_string());
    }

    for base in bases {
        for extension in SIDECAR_IMAGE_EXTENSIONS {
            let key = format!("{}.{}", base.to_lowercase(), extension);
            if let Some((_, original)) = by_lower.iter().find(|(lower, _)| lower == &key) {
                return Some((*original).to_string());
            }
        }
    }
    None
}

// ── path helpers (shared with scan) ─────────────────────────────────────────

pub(crate) fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn extension_of(path: &str) -> &str {
    let name = basename(path);
    match name.rfind('.') {
        Some(index) if index + 1 < name.len() => &name[index + 1..],
        _ => "",
    }
}

pub(crate) fn is_cue_path(path: &str) -> bool {
    extension_of(path).eq_ignore_ascii_case("cue")
}

/// Audio or video, matching the scanner's extension filter.
pub(crate) fn is_music_path(path: &str) -> bool {
    let extension = extension_of(path).to_ascii_lowercase();
    AUDIO_EXTENSIONS.contains(&extension.as_str()) || VIDEO_EXTENSIONS.contains(&extension.as_str())
}

// ── timestamps ──────────────────────────────────────────────────────────────

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn iso8601_now() -> String {
    format_iso8601(now_millis())
}

/// `DateTime.fromMillisecondsSinceEpoch(ms, isUtc: true).toIso8601String()`.
fn format_iso8601(millis: i64) -> String {
    let millis = millis.max(0);
    let seconds = millis / 1000;
    let remainder = millis % 1000;
    let days = seconds / 86_400;
    let second_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{remainder:03}Z",
        second_of_day / 3600,
        (second_of_day % 3600) / 60,
        second_of_day % 60
    )
}

/// Howard Hinnant's `civil_from_days` (days since 1970-01-01).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month as u32, day as u32)
}

// ── test fixtures ───────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use lofty::config::WriteOptions;
    use lofty::id3::v2::Id3v2Tag;
    use lofty::picture::{MimeType, Picture, PictureType};
    use lofty::tag::{Accessor, ItemKey, Tag, TagExt, TagType};
    use wdmm_drivers::{
        CloudFileItem, ContentDeliveryKind, DriverError, Result as VfsResult, WdmmVfs,
    };
    use wdmm_queue::model::{CancelToken, QueueTask, TaskKind, TaskTarget};
    use wdmm_storage::LibraryDatabase;

    use crate::{JobCtx, JobPaths, PlatformClient, ProgressSink};

    /// In-memory VFS: `path -> bytes`. Directories are derived from the paths.
    pub(crate) struct FakeVfs {
        files: Mutex<BTreeMap<String, Vec<u8>>>,
    }

    impl FakeVfs {
        pub(crate) fn new() -> Self {
            Self {
                files: Mutex::new(BTreeMap::new()),
            }
        }

        pub(crate) fn with(mut self, path: &str, bytes: Vec<u8>) -> Self {
            self.files
                .get_mut()
                .unwrap()
                .insert(path.to_string(), bytes);
            self
        }
    }

    impl Default for FakeVfs {
        fn default() -> Self {
            Self::new()
        }
    }

    #[async_trait]
    impl WdmmVfs for FakeVfs {
        async fn list(&self, path: &str) -> VfsResult<Vec<CloudFileItem>> {
            let files = self.files.lock().unwrap();
            let dir = normalize_dir(path);
            let prefix = if dir == "/" {
                "/".to_string()
            } else {
                format!("{dir}/")
            };
            let mut entries: BTreeMap<String, bool> = BTreeMap::new();
            for file_path in files.keys() {
                let Some(rest) = file_path.strip_prefix(&prefix) else {
                    continue;
                };
                if rest.is_empty() {
                    continue;
                }
                match rest.split_once('/') {
                    Some((head, _)) => {
                        entries.insert(head.to_string(), true);
                    }
                    None => {
                        entries.entry(rest.to_string()).or_insert(false);
                    }
                }
            }
            Ok(entries
                .into_iter()
                .map(|(name, is_dir)| {
                    let joined = if dir == "/" {
                        format!("/{name}")
                    } else {
                        format!("{dir}/{name}")
                    };
                    let size = if is_dir {
                        0
                    } else {
                        files.get(&joined).map_or(0, |bytes| bytes.len() as u64)
                    };
                    CloudFileItem {
                        name,
                        is_dir,
                        size,
                        modified: None,
                        raw_url: None,
                        raw_headers: Vec::new(),
                        delivery: ContentDeliveryKind::LocalStream,
                        local_path: None,
                    }
                })
                .collect())
        }

        async fn get(&self, path: &str) -> VfsResult<CloudFileItem> {
            let files = self.files.lock().unwrap();
            let size = files
                .get(path)
                .map(|bytes| bytes.len() as u64)
                .ok_or_else(|| DriverError::NotFound(path.to_string()))?;
            Ok(CloudFileItem {
                name: super::basename(path).to_string(),
                is_dir: false,
                size,
                modified: None,
                raw_url: None,
                raw_headers: Vec::new(),
                delivery: ContentDeliveryKind::LocalStream,
                local_path: None,
            })
        }

        async fn mkdir(&self, _path: &str) -> VfsResult<()> {
            Ok(())
        }

        async fn rename(&self, _old_path: &str, _new_path: &str) -> VfsResult<()> {
            Ok(())
        }

        async fn remove(&self, _path: &str) -> VfsResult<()> {
            Ok(())
        }

        async fn put_content(&self, path: &str, data: &[u8]) -> VfsResult<()> {
            self.files
                .lock()
                .unwrap()
                .insert(path.to_string(), data.to_vec());
            Ok(())
        }

        async fn get_content(&self, path: &str) -> VfsResult<Vec<u8>> {
            self.files
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .ok_or_else(|| DriverError::NotFound(path.to_string()))
        }

        async fn get_content_range(
            &self,
            path: &str,
            start: u64,
            end: Option<u64>,
        ) -> VfsResult<Vec<u8>> {
            let files = self.files.lock().unwrap();
            let data = files
                .get(path)
                .ok_or_else(|| DriverError::NotFound(path.to_string()))?;
            if start >= data.len() as u64 {
                return Ok(Vec::new());
            }
            let last = end
                .unwrap_or(data.len() as u64 - 1)
                .min(data.len() as u64 - 1);
            Ok(data[start as usize..=last as usize].to_vec())
        }
    }

    fn normalize_dir(path: &str) -> String {
        let trimmed = path.trim();
        if trimmed.is_empty() || trimmed == "/" {
            return "/".to_string();
        }
        let mut dir = trimmed.replace('\\', "/");
        if !dir.starts_with('/') {
            dir.insert(0, '/');
        }
        while dir.len() > 1 && dir.ends_with('/') {
            dir.pop();
        }
        dir
    }

    /// Progress sink that records unit updates and diag notes.
    #[derive(Default)]
    pub(crate) struct Recorder {
        pub(crate) units: Mutex<Vec<(u32, Option<u32>)>>,
        pub(crate) notes: Mutex<Vec<String>>,
    }

    impl ProgressSink for Recorder {
        fn bytes(&self, _done: u64, _total: Option<u64>) {}

        fn units(&self, done: u32, total: Option<u32>) {
            self.units.lock().unwrap().push((done, total));
        }

        fn note(&self, message: &str) {
            self.notes.lock().unwrap().push(message.to_string());
        }
    }

    pub(crate) fn test_task(
        kind: TaskKind,
        source_name: &str,
        remote_path: &str,
        file_name: &str,
    ) -> QueueTask {
        QueueTask::new(
            "task-1".to_string(),
            kind,
            source_name.to_string(),
            "acc".to_string(),
            remote_path.to_string(),
            file_name.to_string(),
            TaskTarget::Cache,
            "batch".to_string(),
            0,
        )
    }

    pub(crate) fn test_ctx(
        dir: &Path,
        db: Arc<LibraryDatabase>,
        vfs: Arc<dyn WdmmVfs>,
        task: QueueTask,
        progress: Arc<Recorder>,
    ) -> JobCtx {
        JobCtx {
            task,
            cancel: CancelToken::new(),
            progress,
            platform: Arc::new(PlatformClient::disabled()),
            db,
            vfs,
            paths: JobPaths::new(dir.join("docs"), dir.join("cache")),
        }
    }

    pub(crate) fn memory_db() -> Arc<LibraryDatabase> {
        Arc::new(LibraryDatabase::open(Path::new(":memory:")).unwrap())
    }

    /// Two valid MPEG-1 Layer III frames (128 kbps, 44.1 kHz). lofty needs the
    /// next frame header to confirm the frame sync, and at least one frame to
    /// read properties; a tag alone is not a file.
    pub(crate) fn mp3_frame() -> Vec<u8> {
        let mut frame = vec![0xFF, 0xFB, 0x90, 0x64];
        frame.resize(417, 0);
        let mut file = frame.clone();
        file.extend_from_slice(&frame);
        file
    }

    pub(crate) fn png_cover() -> Vec<u8> {
        let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(16, 16, |x, y| {
            image::Rgb([(x * 16) as u8, (y * 16) as u8, 64])
        }));
        let mut bytes = Vec::new();
        image
            .write_with_encoder(image::codecs::png::PngEncoder::new(&mut bytes))
            .unwrap();
        bytes
    }

    pub(crate) fn tagged_mp3(title: &str, with_cover: bool) -> Vec<u8> {
        let mut tag = Tag::new(TagType::Id3v2);
        tag.set_title(title.to_string());
        tag.set_artist("Artist".to_string());
        tag.set_album("Album".to_string());
        tag.set_genre("Rock".to_string());
        tag.set_track(3);
        tag.set_disk(1);
        tag.insert_text(ItemKey::AlbumArtist, "Album Artist".to_string());
        tag.set_date(lofty::tag::items::Timestamp {
            year: 2021,
            ..Default::default()
        });
        if with_cover {
            tag.push_picture(
                Picture::unchecked(png_cover())
                    .pic_type(PictureType::CoverFront)
                    .mime_type(MimeType::Png)
                    .build(),
            );
        }

        let mut file = Vec::new();
        Id3v2Tag::from(tag)
            .dump_to(&mut file, WriteOptions::default())
            .unwrap();
        file.extend_from_slice(&mp3_frame());
        file
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::Arc;

    use image::GenericImageView as _;
    use wdmm_queue::model::TaskKind;

    use super::test_support::{
        memory_db, mp3_frame, png_cover, tagged_mp3, test_ctx, test_task, FakeVfs, Recorder,
    };
    use super::*;

    #[test]
    fn iso8601_matches_dart_format() {
        assert_eq!(format_iso8601(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_iso8601(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
    }

    #[test]
    fn sidecar_matching_prefers_configured_names() {
        let files = vec![
            "Folder.jpg".to_string(),
            "track.png".to_string(),
            "cover.jpg".to_string(),
        ];
        assert_eq!(
            match_sidecar_cover(&files, "track.flac", "cover, folder"),
            Some("cover.jpg".to_string())
        );
        // The audio stem is appended last, so "track.png" is only reached if no
        // configured base matched.
        assert_eq!(
            match_sidecar_cover(&["track.PNG".to_string()], "track.flac", "cover"),
            Some("track.PNG".to_string())
        );
        assert!(match_sidecar_cover(&[], "track.flac", "cover").is_none());
    }

    #[test]
    fn cue_text_decoding_handles_boms() {
        assert_eq!(decode_cue_text(b"TITLE \"x\""), "TITLE \"x\"");
        assert_eq!(decode_cue_text(&[0xEF, 0xBB, 0xBF, b'a']), "a");
        let utf16le = [0xFF, 0xFE, b'a', 0x00, b'b', 0x00];
        assert_eq!(decode_cue_text(&utf16le), "ab");
        let utf16be = [0xFE, 0xFF, 0x00, b'a', 0x00, b'b'];
        assert_eq!(decode_cue_text(&utf16be), "ab");
    }

    #[tokio::test]
    async fn ingest_remote_file_writes_track_and_cover() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        let vfs = Arc::new(FakeVfs::new().with("/Music/Album/01 - Song.mp3", tagged_mp3("Title", true)));
        let progress = Arc::new(Recorder::default());
        let task = test_task(
            TaskKind::Scan,
            "My Disk",
            "/Music/Album/01 - Song.mp3",
            "01 - Song.mp3",
        );
        let ctx = test_ctx(dir.path(), Arc::clone(&db), vfs, task, Arc::clone(&progress));

        let outcome = ingest_remote_file(&ctx).await.unwrap();
        assert_eq!(outcome.rows, 1);
        assert!(outcome.cover);
        assert_eq!(outcome.title.as_deref(), Some("Title"));

        let track = db
            .get_track_by_source_path("My Disk", "/Music/Album/01 - Song.mp3")
            .unwrap()
            .unwrap();
        assert_eq!(track.title, "Title");
        assert_eq!(track.artist.as_deref(), Some("Artist"));
        assert_eq!(track.album.as_deref(), Some("Album"));
        assert_eq!(track.album_artist.as_deref(), Some("Album Artist"));
        assert_eq!(track.year, Some(2021));
        assert_eq!(track.track_number, Some(3));
        assert_eq!(track.disc_number, Some(1));
        assert_eq!(track.cover_state, 2);
        assert_eq!(track.account_id, "My Disk");

        let thumb = thumbs::thumb_path(
            &ctx.paths.cover_dir,
            "My Disk",
            "/Music/Album/01 - Song.mp3",
        );
        assert_eq!(image::open(&thumb).unwrap().dimensions(), (300, 300));
    }

    #[tokio::test]
    async fn ingest_remote_file_falls_back_to_the_file_name() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        let vfs = Arc::new(FakeVfs::new().with("/Music/Album/02 - Untagged.mp3", mp3_frame()));
        let progress = Arc::new(Recorder::default());
        let task = test_task(
            TaskKind::Scan,
            "My Disk",
            "/Music/Album/02 - Untagged.mp3",
            "02 - Untagged.mp3",
        );
        let ctx = test_ctx(dir.path(), Arc::clone(&db), vfs, task, progress);

        let outcome = ingest_remote_file(&ctx).await.unwrap();
        assert_eq!(outcome.rows, 1);
        let track = db
            .get_track_by_source_path("My Disk", "/Music/Album/02 - Untagged.mp3")
            .unwrap()
            .unwrap();
        assert_eq!(track.title, "02 - Untagged.mp3");
        assert!(track.artist.is_none());
        assert_eq!(track.cover_state, 0);
        assert_eq!(
            track.id,
            thumbs::music_id_for_remote("My Disk", "/Music/Album/02 - Untagged.mp3")
        );
    }

    #[tokio::test]
    async fn sidecar_cover_is_used_when_tags_have_none() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        db.save_kv(SIDECAR_ENABLED_KEY, "true").unwrap();
        let vfs = Arc::new(
            FakeVfs::new()
                .with("/Music/Album/track.mp3", mp3_frame())
                .with("/Music/Album/cover.png", png_cover()),
        );
        let progress = Arc::new(Recorder::default());
        let task = test_task(TaskKind::Scan, "My Disk", "/Music/Album/track.mp3", "track.mp3");
        let ctx = test_ctx(dir.path(), Arc::clone(&db), vfs, task, progress);

        let outcome = ingest_remote_file(&ctx).await.unwrap();
        assert!(outcome.cover);
        let track = db
            .get_track_by_source_path("My Disk", "/Music/Album/track.mp3")
            .unwrap()
            .unwrap();
        assert_eq!(track.cover_state, 2);
    }

    #[tokio::test]
    async fn ingest_downloaded_reads_the_local_file() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        let local = dir.path().join("downloaded.mp3");
        std::fs::write(&local, tagged_mp3("Downloaded", false)).unwrap();
        let vfs = Arc::new(FakeVfs::new());
        let progress = Arc::new(Recorder::default());
        let mut task = test_task(
            TaskKind::DownloadCache,
            "My Disk",
            "/Music/Album/downloaded.mp3",
            "downloaded.mp3",
        );
        task.local_path = Some(local.to_string_lossy().into_owned());
        let ctx = test_ctx(dir.path(), Arc::clone(&db), vfs, task, progress);

        let outcome = ingest_downloaded(&ctx).await.unwrap();
        assert_eq!(outcome.rows, 1);
        let track = db
            .get_track_by_source_path("My Disk", "/Music/Album/downloaded.mp3")
            .unwrap()
            .unwrap();
        assert_eq!(track.title, "Downloaded");
        assert_eq!(track.artist.as_deref(), Some("Artist"));
    }

    #[tokio::test]
    async fn cue_album_is_sliced_with_clip_ranges() {
        let cue_text = "\
REM GENRE Jazz
REM DATE 2003
PERFORMER \"Album Artist\"
TITLE \"Album Title\"
FILE \"album.flac\" WAVE
  TRACK 01 AUDIO
    TITLE \"First\"
    PERFORMER \"Singer\"
    INDEX 01 00:00:00
  TRACK 02 AUDIO
    TITLE \"Second\"
    INDEX 01 03:15:00
";
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        let vfs = Arc::new(FakeVfs::new().with(
            "/Music/Album/album.cue",
            cue_text.as_bytes().to_vec(),
        ));
        let progress = Arc::new(Recorder::default());
        let task = test_task(
            TaskKind::Scan,
            "My Disk",
            "/Music/Album/album.cue",
            "album.cue",
        );
        let ctx = test_ctx(dir.path(), Arc::clone(&db), vfs, task, progress);

        let outcome = ingest_remote_file(&ctx).await.unwrap();
        assert_eq!(outcome.rows, 2);
        assert_eq!(outcome.title.as_deref(), Some("Album Title"));

        let slices = db.list_cue_slices().unwrap();
        assert_eq!(slices.len(), 2);
        assert_eq!(slices[0].remote_path, "/Music/Album/album.flac#cue:1");
        assert_eq!(slices[0].track_index, 1);
        assert_eq!(slices[0].clip_start_ms, Some(0));
        assert_eq!(slices[0].clip_end_ms, Some(195_000));
        assert_eq!(slices[0].title.as_deref(), Some("First"));
        assert_eq!(slices[0].artist.as_deref(), Some("Singer"));
        assert_eq!(slices[1].clip_start_ms, Some(195_000));
        assert_eq!(slices[1].clip_end_ms, None);
        assert_eq!(slices[1].artist.as_deref(), Some("Album Artist"));
        assert_eq!(slices[1].album.as_deref(), Some("Album Title"));
        assert_eq!(slices[1].year, Some(2003));
        assert_eq!(slices[1].genre.as_deref(), Some("Jazz"));
        assert_eq!(slices[1].track_total, Some(2));
        assert_eq!(slices[1].last_downloaded_at, EPOCH0);
        assert_eq!(slices[1].source_name, "My Disk");
    }

    #[tokio::test]
    async fn reingesting_a_cue_album_keeps_existing_slices() {
        let cue_text = "FILE \"album.flac\" WAVE\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n";
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        let vfs = Arc::new(FakeVfs::new().with(
            "/Music/Album/album.cue",
            cue_text.as_bytes().to_vec(),
        ));
        let progress = Arc::new(Recorder::default());
        let task = test_task(
            TaskKind::Scan,
            "My Disk",
            "/Music/Album/album.cue",
            "album.cue",
        );
        let ctx = test_ctx(dir.path(), Arc::clone(&db), vfs, task, progress);

        let first = ingest_remote_file(&ctx).await.unwrap();
        assert_eq!(first.rows, 1);
        let second = ingest_remote_file(&ctx).await.unwrap();
        assert_eq!(second.rows, 0);
        assert!(!second.cover);
    }

    #[test]
    fn extract_tags_from_a_local_cursor() {
        let bytes = tagged_mp3("Cursor", true);
        let tags = extract_tags(Cursor::new(bytes)).unwrap();
        assert_eq!(tags.title.as_deref(), Some("Cursor"));
        assert_eq!(tags.artist.as_deref(), Some("Artist"));
        assert_eq!(tags.genre.as_deref(), Some("Rock"));
        assert!(tags.cover.is_some());
    }
}
