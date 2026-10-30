//! `wdmm-ffmpeg` — in-process libav bindings (B3.5).
//!
//! Provides:
//! - [`probe`] — read container/codec metadata via in-process libav
//!   (`avformat_open_input` + `avformat_find_stream_info`).
//! - [`open_job`] — start an HLS transcode/remux job writing `index.m3u8` +
//!   numbered `.ts` segments into a caller-provided directory. The job runs in
//!   a background OS thread; [`JobHandle`] cancels it on drop.
//!
//! ## Input model
//!
//! `input_url` accepts both local file paths (`/path/to/file.mp4`) and HTTP
//! URLs (e.g. `http://127.0.0.1:{port}/s/{token}` — the loopback proxy stream
//! token URL). `avformat_open_input` handles both natively.
//!
//! ## Preset model
//!
//! A [`wdmm_core::models::TranscodePreset`] drives the job:
//! - `container = "original"` → pass-through (no HLS wrapping).
//! - `remux_first = true` → try remux (`-c copy` equivalent) first, fall back
//!   to transcode on codec incompatibility.
//! - `video_codec = None` → audio-only.
//! - `audio_codec = "copy"` → copy audio unchanged.
//!
//! ## License
//! This crate links against FFmpeg shared libraries, licensed under LGPL v2.1+
//! (optionally GPL v2+ when certain components are enabled). See
//! `docs/FFMPEG-ATTRIBUTION.md` for the full third-party attribution note.
//! WDMM is distributed under AGPL-3.0.

#![allow(dead_code)]

pub mod preset;
mod sys;
mod transcoder;

use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use thiserror::Error;

pub use wdmm_core::models::TranscodePreset;

// ── Error ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum FfmpegError {
    #[error("ffmpeg not available: {0}")]
    Unavailable(String),
    #[error("probe failed: {0}")]
    Probe(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type FfmpegResult<T> = std::result::Result<T, FfmpegError>;

// ── Probe results ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VideoStream {
    pub index: u32,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AudioStream {
    pub index: u32,
    pub codec: String,
    pub channels: u32,
    pub sample_rate: u32,
    pub language: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SubtitleStream {
    pub index: u32,
    pub codec: String,
    pub language: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProbeResult {
    /// Duration in seconds.
    pub duration_secs: f64,
    pub video: Vec<VideoStream>,
    pub audio: Vec<AudioStream>,
    pub subtitles: Vec<SubtitleStream>,
    /// Detected container format name (e.g. `"mov,mp4…"`, `"matroska"`).
    pub format_name: String,
}

/// Probe a media URL (local path or http URL) via in-process libav.
pub fn probe(url: &str) -> FfmpegResult<ProbeResult> {
    transcoder::probe_url(url)
}

// ── HLS job ───────────────────────────────────────────────────────────────────

/// Options for opening a new HLS transcode/remux job.
#[derive(Debug)]
pub struct JobOpts {
    /// Input URL — local file path or `http://…` URL.
    pub input_url: String,
    /// Transcode preset driving the job.
    pub preset: wdmm_core::models::TranscodePreset,
    /// Start offset in seconds (seek). `0.0` = from the beginning.
    pub start_offset_secs: f64,
    /// Caller-provided directory to write `index.m3u8` + segments into.
    /// Must exist; the job does not create it.
    pub segment_dir: PathBuf,
}

impl Default for JobOpts {
    fn default() -> Self {
        Self {
            input_url: String::new(),
            preset: preset::preset_passthrough(),
            start_offset_secs: 0.0,
            segment_dir: PathBuf::new(),
        }
    }
}

/// Handle to an active (or completed) HLS transcode/remux job.
///
/// The job writes `index.m3u8` + numbered `.ts` segments into
/// [`segment_dir`](JobHandle::segment_dir). The proxy reads from that
/// directory. On drop the job is cancelled and the segment directory removed.
#[derive(Debug)]
pub struct JobHandle {
    pub segment_dir: PathBuf,
    cancel: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<FfmpegResult<()>>>,
}

impl JobHandle {
    /// Directory where the job writes `index.m3u8` + segments.
    pub fn segment_dir(&self) -> &Path {
        &self.segment_dir
    }

    /// Request cancellation. The background thread stops after its next loop
    /// tick (non-blocking — does not wait).
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Block until the background thread finishes; return its result.
    pub fn join(mut self) -> FfmpegResult<()> {
        if let Some(t) = self.thread.take() {
            let r = t.join().map_err(|_| {
                FfmpegError::Io(std::io::Error::new(std::io::ErrorKind::Other, "thread panic"))
            })?;
            r
        } else {
            Ok(())
        }
    }
}

impl Drop for JobHandle {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // Best-effort cleanup of the segment directory.
        if self.segment_dir.exists() {
            let _ = std::fs::remove_dir_all(&self.segment_dir);
        }
    }
}

/// Open a new HLS transcode/remux job.
///
/// Spawns a background OS thread that runs the in-process libav pipeline.
/// Returns a [`JobHandle`] whose [`Drop`] cancels the job and removes the
/// segment directory.
pub fn open_job(opts: JobOpts) -> FfmpegResult<JobHandle> {
    // Generate a session id for tracing; the directory is caller-provided.
    let id = format!("{:016x}", rand::random::<u64>());
    let segment_dir = if opts.segment_dir.as_os_str().is_empty() {
        let d = std::env::temp_dir().join(format!("wdmm-hls-{}", id));
        std::fs::create_dir_all(&d)?;
        d
    } else {
        opts.segment_dir.clone()
    };

    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_t = cancel.clone();
    let input_url = opts.input_url.clone();
    let preset = opts.preset.clone();
    let preset_id = preset.id.clone();
    let start_offset = opts.start_offset_secs;
    let dir = segment_dir.clone();

    let thread = std::thread::Builder::new()
        .name(format!("wdmm-hls-{}", id))
        .spawn(move || transcoder::run_hls_job(&input_url, &preset, start_offset, &dir, cancel_t))?;

    tracing::debug!(
        "wdmm-ffmpeg: open_job preset={} seg_dir={}",
        preset_id,
        segment_dir.display()
    );

    Ok(JobHandle {
        segment_dir,
        cancel,
        thread: Some(thread),
    })
}

// ── Small helper for FFI: resolve a codec name from a codec pointer ───────────

/// Return the `name` field of an `AVCodec` (offset +8 on LP64, after the
/// `AVClass*` at +0). Returns the numeric codec id if the pointer is null.
#[allow(dead_code)]
pub(crate) fn codec_name(codec: *const sys::AVCodec, codec_id: u32) -> String {
    unsafe {
        if codec.is_null() {
            return format!("{}", codec_id);
        }
        let name_ptr = *(codec as *const u8).add(8).cast::<*const std::ffi::c_char>();
        if name_ptr.is_null() {
            return format!("{}", codec_id);
        }
        CStr::from_ptr(name_ptr).to_string_lossy().into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_passthrough_is_audio_copy() {
        let p = preset::preset_passthrough();
        assert_eq!(p.container, "original");
        assert_eq!(p.audio_codec, "copy");
        assert!(!p.remux_first);
    }

    #[test]
    fn all_builtin_presets_have_5() {
        let all = preset::all_builtin_presets();
        assert_eq!(all.len(), 5);
        assert_eq!(all[0].id, "passthrough");
        assert_eq!(all[4].id, "audio-only");
    }

    #[test]
    fn fit_dimensions_scales_down() {
        let (w, h) = transcoder::fit_dimensions(1920, 1080, Some(1280), Some(720));
        assert_eq!(w, 1280);
        assert_eq!(h, 720);
    }

    #[test]
    fn fit_dimensions_preserves_aspect() {
        // 1920x1080 → max 1280 wide
        let (w, h) = transcoder::fit_dimensions(1920, 1080, Some(1280), None);
        assert_eq!(w, 1280);
        assert_eq!(h, 720);
    }

    #[test]
    fn fit_dimensions_rounds_even() {
        // 641x361 → must round to even
        let (w, h) = transcoder::fit_dimensions(641, 361, None, None);
        assert_eq!(w % 2, 0);
        assert_eq!(h % 2, 0);
    }

    #[test]
    fn jobopts_default() {
        let o = JobOpts::default();
        assert!(o.input_url.is_empty());
        assert_eq!(o.preset.id, "passthrough");
        assert_eq!(o.start_offset_secs, 0.0);
    }
}
