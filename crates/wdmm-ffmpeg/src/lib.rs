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

pub mod diag; // always compiled — diagnostics must work without libav
pub mod plan; // always compiled — pure remux/hybrid decision rules, host-testable
pub mod preset;
pub mod subtitle; // always compiled — pure text/timing → WebVTT conversion, host-testable
mod sys;
pub mod vod; // always compiled — pure VOD segment planning, host-testable

#[cfg(feature = "ffmpeg")]
mod transcoder;

use std::path::{Path, PathBuf};
#[cfg(feature = "ffmpeg")]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
#[cfg(feature = "ffmpeg")]
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub use plan::{
    hybrid_audio_codec_id, remux_dropped_streams, remux_plan, remux_stream_selection, RemuxPlan,
};
pub use subtitle::{classify_codec, Cue, TrackKind};
pub use vod::{
    approximate_plan, calibration_plan, exact_plan, keyframe_index, keyframe_plan,
    measure_gop_from_durations, KeyframeInfo, PlanAlignment, SegEntry, SegmentPlan,
};
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

/// Whether the in-process FFmpeg pipeline is compiled in.
///
/// Callers use this to skip transcode work (and to explain *why* it is
/// unavailable) without probing [`open_job`].
pub const fn is_available() -> bool {
    cfg!(feature = "ffmpeg")
}

// ── Progress telemetry (§1) ───────────────────────────────────────────────────

/// Snapshot of an HLS job's write progress.
///
/// `content_ms` is monotonic: it only grows as packets are written, and stays
/// `0` until the muxer has accepted its first packet. `wall_ms` counts from the
/// moment the job thread actually started working (not from `open_job`).
#[derive(Clone, Copy, Debug, Default)]
pub struct ProgressSnapshot {
    /// Media time of the last packet handed to the muxer, in milliseconds.
    pub content_ms: i64,
    /// Wall-clock milliseconds since the job thread started.
    pub wall_ms: u64,
    /// Packets written (remux) or encoded frames (transcode).
    pub packets: u64,
}

/// Encoder selection actually used by a job (§7.3).
#[derive(Clone, Debug, Default)]
pub struct EncoderInfo {
    /// `"remux"`/`"hybrid"` for the copy paths, `"hw"` when a MediaCodec
    /// hardware encoder was actually opened, `"sw"` when libx264 encodes.
    pub mode: String,
    /// Actual video encoder name; `None` for a remux, `"copy"` for hybrid
    /// remux (video packets are copied, not re-encoded).
    pub video: Option<String>,
    /// Actual audio encoder name, or `"copy"` for stream copy.
    pub audio: Option<String>,
    /// The preset asked for a hardware encoder.
    pub hw_requested: bool,
    /// A hardware encoder actually opened successfully (presence in libavcodec
    /// alone is not enough — `avcodec_open2` must have succeeded).
    pub hw_available: bool,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Shared progress cell written by the job thread and read by [`JobHandle`].
#[derive(Debug, Default)]
pub(crate) struct ProgressState {
    started_epoch_ms: AtomicU64,
    content_ms: AtomicI64,
    packets: AtomicU64,
    encoder: Mutex<EncoderInfo>,
    warnings: Mutex<Vec<String>>,
}

impl ProgressState {
    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// Start the wall clock — called by the job thread when it begins working.
    pub(crate) fn start(&self) {
        self.started_epoch_ms
            .store(Self::now_ms(), Ordering::Relaxed);
    }

    /// Record a packet at media time `pts_ms` (monotonic, never decreases).
    pub(crate) fn note_written(&self, pts_ms: i64) {
        self.content_ms.fetch_max(pts_ms.max(0), Ordering::Relaxed);
        self.packets.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> ProgressSnapshot {
        let started = self.started_epoch_ms.load(Ordering::Relaxed);
        let wall_ms = if started == 0 {
            0
        } else {
            Self::now_ms().saturating_sub(started)
        };
        ProgressSnapshot {
            content_ms: self.content_ms.load(Ordering::Relaxed),
            wall_ms,
            packets: self.packets.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn set_encoder(&self, info: EncoderInfo) {
        *lock(&self.encoder) = info;
    }

    pub(crate) fn encoder(&self) -> EncoderInfo {
        lock(&self.encoder).clone()
    }

    /// Record a deduplicated warning (max 8 retained).
    pub(crate) fn warn(&self, msg: impl Into<String>) {
        let msg = msg.into();
        let mut warnings = lock(&self.warnings);
        if warnings.len() < 8 && !warnings.iter().any(|w| w == &msg) {
            warnings.push(msg);
        }
    }

    pub(crate) fn warnings(&self) -> Vec<String> {
        lock(&self.warnings).clone()
    }
}

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
#[cfg(feature = "ffmpeg")]
pub fn probe(url: &str) -> FfmpegResult<ProbeResult> {
    transcoder::probe_url(url)
}

/// Stub: returns [`FfmpegError::Unavailable`] when `ffmpeg` feature is off.
#[cfg(not(feature = "ffmpeg"))]
pub fn probe(_url: &str) -> FfmpegResult<ProbeResult> {
    Err(FfmpegError::Unavailable(
        "FFmpeg not linked (build with --features ffmpeg)".into(),
    ))
}

/// Extract one embedded **text** subtitle stream and convert it to WebVTT.
///
/// Demuxes `url`, decodes the subtitle stream at `stream_index` through
/// libavcodec (`avcodec_decode_subtitle2`, no rendering), then converts the
/// decoded cues to WebVTT with [`subtitle::to_webvtt`]. Bitmap subtitle codecs
/// (PGS/DVD/DVB) are rejected up front with a [`subtitle::unsupported_reason`]
/// message — they need rendering, not text conversion.
#[cfg(feature = "ffmpeg")]
pub fn extract_subtitle_track(url: &str, stream_index: u32) -> FfmpegResult<String> {
    transcoder::extract_subtitle_track(url, stream_index)
}

/// Stub: returns [`FfmpegError::Unavailable`] when `ffmpeg` feature is off.
#[cfg(not(feature = "ffmpeg"))]
pub fn extract_subtitle_track(_url: &str, _stream_index: u32) -> FfmpegResult<String> {
    Err(FfmpegError::Unavailable(
        "FFmpeg not linked (build with --features ffmpeg)".into(),
    ))
}

// ── Measured segment durations / segment counts ──────────────────────────────

/// Read the measured segment durations (milliseconds) from a muxer-written
/// `index.m3u8`, in playlist order.
///
/// This is the calibration input (§3.1): the HLS muxer records the real segment
/// lengths as `#EXTINF` lines, which [`measure_gop_from_durations`] then turns
/// into a GOP estimate. Returns an empty vector when the playlist does not exist
/// yet or has no `#EXTINF` lines.
pub fn measure_segment_durations(dir: &Path) -> Vec<i64> {
    match std::fs::read_to_string(dir.join("index.m3u8")) {
        Ok(playlist) => parse_extinf_durations(&playlist),
        Err(_) => Vec::new(),
    }
}

/// Parse `#EXTINF:<secs>,` lines into milliseconds (rounded).
fn parse_extinf_durations(playlist: &str) -> Vec<i64> {
    let mut durations = Vec::new();
    for line in playlist.lines() {
        let Some(rest) = line.trim().strip_prefix("#EXTINF:") else {
            continue;
        };
        let secs = rest.split(',').next().unwrap_or("").trim();
        if let Ok(secs) = secs.parse::<f64>() {
            if secs.is_finite() && secs >= 0.0 {
                durations.push((secs * 1000.0).round() as i64);
            }
        }
    }
    durations
}

/// Count finished `seg<digits>.ts` files in `dir`.
fn count_ready_segments(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| is_segment_name(&entry.file_name().to_string_lossy()))
        .count()
}

/// Whether `name` is a finished HLS segment written by the muxer.
fn is_segment_name(name: &str) -> bool {
    let Some(stem) = name
        .strip_prefix("seg")
        .and_then(|rest| rest.strip_suffix(".ts"))
    else {
        return false;
    };
    !stem.is_empty() && stem.bytes().all(|byte| byte.is_ascii_digit())
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
    ///
    /// Ignored when [`out_dir`](JobOpts::out_dir) is set.
    pub segment_dir: PathBuf,
    /// Segment number of the first segment (`hlsenc start_number`). The muxer
    /// then writes `seg<start_number>.ts`, `seg<start_number + 1>.ts`, … which
    /// lets a pull-based job restart mid-film and cover the requested `k`.
    pub start_number: u64,
    /// Persistent job directory (VOD segment cache).
    ///
    /// When `Some`, the job writes here, creates the directory if needed, and
    /// [`JobHandle`]'s `Drop` **does not** delete it (the cache outlives the
    /// job). When `None`, the legacy behaviour applies: a temporary/caller
    /// directory whose contents are removed on drop.
    pub out_dir: Option<PathBuf>,
    /// Override for the HLS muxer `hls_time` (seconds, decimals allowed),
    /// expressed in milliseconds. Used after calibration to align `hls_time` to
    /// a multiple of the measured GOP so segment cuts stop drifting.
    pub hls_time_override_ms: Option<u32>,
}

impl Default for JobOpts {
    fn default() -> Self {
        Self {
            input_url: String::new(),
            preset: preset::preset_passthrough(),
            start_offset_secs: 0.0,
            segment_dir: PathBuf::new(),
            start_number: 0,
            out_dir: None,
            hls_time_override_ms: None,
        }
    }
}

/// Handle to an active (or completed) HLS transcode/remux job.
///
/// The job writes `index.m3u8` + numbered `.ts` segments into
/// [`segment_dir`](JobHandle::segment_dir). The proxy reads from that
/// directory. On drop the job is cancelled; the directory itself is removed only
/// for legacy jobs that own it (`JobOpts::out_dir == None`).
#[cfg(feature = "ffmpeg")]
#[derive(Debug)]
pub struct JobHandle {
    pub segment_dir: PathBuf,
    cancel: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<FfmpegResult<()>>>,
    /// Live progress + encoder info shared with the job thread.
    telemetry: Arc<ProgressState>,
    /// Whether `Drop` owns (and may delete) [`segment_dir`](JobHandle::segment_dir).
    /// False for persistent VOD cache directories (`JobOpts::out_dir`).
    owns_dir: bool,
}

/// Stub handle used when the `ffmpeg` feature is off. `open_job` always
/// returns `Err(FfmpegError::Unavailable)`, so this struct is never
/// constructed; it exists only so callers compile.
#[cfg(not(feature = "ffmpeg"))]
#[non_exhaustive]
#[derive(Debug)]
pub struct JobHandle {
    pub segment_dir: PathBuf,
}

#[cfg(feature = "ffmpeg")]
impl JobHandle {
    /// Directory where the job writes `index.m3u8` + segments.
    pub fn segment_dir(&self) -> &Path {
        &self.segment_dir
    }

    /// Directory the job writes into (the `JobOpts::out_dir` form).
    ///
    /// Identical to [`segment_dir`](JobHandle::segment_dir); both names exist
    /// because the VOD cache side speaks in "out dirs" while the legacy proxy
    /// path speaks in "segment dirs".
    pub fn out_dir(&self) -> &Path {
        &self.segment_dir
    }

    /// Number of finished `seg<k>.ts` files currently on disk.
    ///
    /// A non-blocking directory snapshot: the muxer may be mid-write, so this is
    /// a lower/upper bound by at most one segment.
    pub fn ready_segments(&self) -> usize {
        count_ready_segments(&self.segment_dir)
    }

    /// Measured segment durations (milliseconds) from the muxer-written
    /// `index.m3u8`, in playlist order. Empty until the first `#EXTINF` lands.
    pub fn segment_durations(&self) -> Vec<i64> {
        measure_segment_durations(&self.segment_dir)
    }

    /// Request cancellation. The background thread stops after its next loop
    /// tick (non-blocking — does not wait).
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Non-blocking check: is the transcode thread still running?
    ///
    /// Used by callers that need to fail fast when a job dies before writing its
    /// first output (e.g. unsupported input) instead of waiting out a timeout.
    pub fn is_running(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    /// Take the finished thread's result without consuming the handle.
    ///
    /// Returns `None` while the job is still running. Dropping the handle still
    /// removes the segment directory, so this only surfaces the error text.
    pub fn take_result(&mut self) -> Option<FfmpegResult<()>> {
        let thread = self.thread.take()?;
        Some(
            thread
                .join()
                .unwrap_or_else(|_| Err(FfmpegError::Io(std::io::Error::other("thread panic")))),
        )
    }

    /// Block until the background thread finishes; return its result.
    pub fn join(mut self) -> FfmpegResult<()> {
        if let Some(t) = self.thread.take() {
            t.join()
                .map_err(|_| FfmpegError::Io(std::io::Error::other("thread panic")))?
        } else {
            Ok(())
        }
    }

    /// Monotonic write progress (§1). Safe to call while the job runs.
    pub fn progress(&self) -> ProgressSnapshot {
        self.telemetry.snapshot()
    }

    /// Encoder selection used by the job (§7.3).
    pub fn encoder_info(&self) -> EncoderInfo {
        self.telemetry.encoder()
    }

    /// Deduplicated warnings collected by the job (§7.3).
    pub fn warnings(&self) -> Vec<String> {
        self.telemetry.warnings()
    }
}

#[cfg(feature = "ffmpeg")]
impl Drop for JobHandle {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // Legacy jobs own their segment directory and clean it up. Persistent
        // VOD cache dirs (`JobOpts::out_dir`) must survive the job: dropping a
        // handle only cancels, it never evicts cached segments.
        if self.owns_dir && self.segment_dir.exists() {
            let _ = std::fs::remove_dir_all(&self.segment_dir);
        }
    }
}

#[cfg(not(feature = "ffmpeg"))]
impl JobHandle {
    pub fn segment_dir(&self) -> &Path {
        &self.segment_dir
    }
    pub fn out_dir(&self) -> &Path {
        &self.segment_dir
    }
    pub fn ready_segments(&self) -> usize {
        0
    }
    pub fn segment_durations(&self) -> Vec<i64> {
        Vec::new()
    }
    pub fn cancel(&self) {}
    pub fn is_running(&self) -> bool {
        false
    }
    pub fn take_result(&mut self) -> Option<FfmpegResult<()>> {
        None
    }
    pub fn join(self) -> FfmpegResult<()> {
        Ok(())
    }
    pub fn progress(&self) -> ProgressSnapshot {
        ProgressSnapshot::default()
    }
    pub fn encoder_info(&self) -> EncoderInfo {
        EncoderInfo::default()
    }
    pub fn warnings(&self) -> Vec<String> {
        Vec::new()
    }
}

#[cfg(not(feature = "ffmpeg"))]
impl Drop for JobHandle {
    fn drop(&mut self) {}
}

/// Open a new HLS transcode/remux job.
///
/// Spawns a background OS thread that runs the in-process libav pipeline.
/// Returns a [`JobHandle`] whose [`Drop`] cancels the job and removes the
/// segment directory.
#[cfg(feature = "ffmpeg")]
pub fn open_job(opts: JobOpts) -> FfmpegResult<JobHandle> {
    // Generate a session id for tracing; the directory is caller-provided.
    let id = format!("{:016x}", rand::random::<u64>());
    // A persistent `out_dir` (VOD cache) is owned by the caller and survives
    // the job; a legacy `segment_dir`/temp dir is owned (and cleaned) by the
    // handle.
    let owns_dir = opts.out_dir.is_none();
    let segment_dir = if let Some(dir) = opts.out_dir.clone() {
        std::fs::create_dir_all(&dir)?;
        dir
    } else if opts.segment_dir.as_os_str().is_empty() {
        let d = std::env::temp_dir().join(format!("wdmm-hls-{}", id));
        std::fs::create_dir_all(&d)?;
        d
    } else {
        opts.segment_dir.clone()
    };

    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_t = cancel.clone();
    let telemetry = Arc::new(ProgressState::default());
    let telemetry_t = Arc::clone(&telemetry);
    let input_url = opts.input_url.clone();
    let preset = opts.preset.clone();
    let preset_id = preset.id.clone();
    let start_offset = opts.start_offset_secs;
    let start_number = opts.start_number;
    let hls_time_override_ms = opts.hls_time_override_ms;
    let dir = segment_dir.clone();

    let thread = std::thread::Builder::new()
        .name(format!("wdmm-hls-{}", id))
        .spawn(move || {
            transcoder::run_hls_job(
                &input_url,
                &preset,
                start_offset,
                &dir,
                cancel_t,
                &telemetry_t,
                &id,
                start_number,
                hls_time_override_ms,
            )
        })?;

    tracing::debug!(
        "wdmm-ffmpeg: open_job preset={} seg_dir={} start_number={} hls_time_override_ms={:?} owns_dir={}",
        preset_id,
        segment_dir.display(),
        start_number,
        hls_time_override_ms,
        owns_dir
    );

    Ok(JobHandle {
        segment_dir,
        cancel,
        thread: Some(thread),
        telemetry,
        owns_dir,
    })
}

/// Stub: returns [`FfmpegError::Unavailable`] when `ffmpeg` feature is off.
#[cfg(not(feature = "ffmpeg"))]
pub fn open_job(_opts: JobOpts) -> FfmpegResult<JobHandle> {
    Err(FfmpegError::Unavailable(
        "FFmpeg not linked (build with --features ffmpeg)".into(),
    ))
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
    #[cfg(feature = "ffmpeg")]
    fn fit_dimensions_scales_down() {
        let (w, h) = transcoder::fit_dimensions(1920, 1080, Some(1280), Some(720));
        assert_eq!(w, 1280);
        assert_eq!(h, 720);
    }

    #[test]
    #[cfg(feature = "ffmpeg")]
    fn fit_dimensions_preserves_aspect() {
        // 1920x1080 → max 1280 wide
        let (w, h) = transcoder::fit_dimensions(1920, 1080, Some(1280), None);
        assert_eq!(w, 1280);
        assert_eq!(h, 720);
    }

    #[test]
    #[cfg(feature = "ffmpeg")]
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

    // ── Progress telemetry (§1) ───────────────────────────────────────────────

    #[test]
    fn progress_is_monotonic_and_counts_packets() {
        let state = ProgressState::default();
        let initial = state.snapshot();
        assert_eq!(initial.content_ms, 0, "no output yet ⇒ content_ms 0");
        assert_eq!(initial.packets, 0);
        assert_eq!(initial.wall_ms, 0, "wall clock starts with the job thread");

        state.start();
        state.note_written(1_000);
        let first = state.snapshot();
        assert_eq!(first.content_ms, 1_000);
        assert_eq!(first.packets, 1);

        // A packet with an older PTS must never move content_ms backwards.
        state.note_written(500);
        let second = state.snapshot();
        assert_eq!(second.content_ms, 1_000);
        assert_eq!(second.packets, 2);
        assert!(second.wall_ms >= first.wall_ms);

        state.note_written(2_500);
        assert_eq!(state.snapshot().content_ms, 2_500);
    }

    #[test]
    fn warnings_are_deduplicated_and_capped() {
        let state = ProgressState::default();
        state.warn("h264_mediacodec 不可用，已回退 libx264");
        state.warn("h264_mediacodec 不可用，已回退 libx264");
        assert_eq!(state.warnings().len(), 1);
        for i in 0..20 {
            state.warn(format!("warn-{i}"));
        }
        assert_eq!(state.warnings().len(), 8);
    }
}
