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
//! A [`wdmm_core::models::TranscodePreset`] drives the job through a per-stream
//! decision (see [`SessionDecision`]): each stream is either copied when the
//! source is already MPEG-TS compatible and the preset allows it, or encoded
//! with the preset's hardware/software encoder. The encoder itself is opened
//! with the `AVDictionary` built by [`encoder_av_options`] (structured rate
//! control only, contract `preset-editor-slimming` §2.1), and
//! [`video_encoder_catalog`] is the closed encoder list the preset editor
//! renders. The output container is always MPEG-TS (HLS segments); direct
//! playback never reaches this crate.
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
mod hwdecode; // hardware (MediaCodec) decode helpers for the transcode path
#[cfg(feature = "ffmpeg")]
mod transcoder;

#[cfg(feature = "ffmpeg")]
pub use hwdecode::{attach, is_hw_frame, mediacodec_decoder_name, software_format, HwDecode};

use std::path::{Path, PathBuf};
#[cfg(feature = "ffmpeg")]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
#[cfg(feature = "ffmpeg")]
use std::sync::Arc;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub use plan::{
    decide_audio_action, decide_video_action, default_video_bitrate, display_width,
    encoder_av_options, encoder_frame_rate, fit_display_dimensions, hw_gop_size, options_line,
    remux_dropped_streams, remux_plan, remux_stream_selection, segment_gop_size,
    sw_fallback_downgrades_hevc, target_video_bitrate, video_encoder_catalog, video_plan_diag,
    AudioAction, EncoderFamily, RateControl, RemuxPlan, StreamAction, VideoEncoderInfo,
    INVARIANT_X264_PARAMS, SHIPPED_AUDIO_ENCODERS, SHIPPED_VIDEO_ENCODERS,
};
pub use subtitle::{classify_codec, Cue, TrackKind};
pub use vod::{
    exact_plan, keyframe_plan, PlanAlignment, SegEntry, SegmentPlan, MIN_KEYFRAMES_FOR_PLAN,
};
pub use wdmm_core::models::TranscodePreset;

/// Serialized [`video_encoder_catalog`] for the FFI boundary (contract
/// `preset-editor-slimming` §2): `{"codecs":[{id,label,hardware,output_codec,
/// rate_control}]}`.
///
/// The catalog is static data, so serialization cannot fail; an `Err` here would
/// be a code bug rather than runtime input.
pub fn video_encoder_catalog_json() -> Result<String, serde_json::Error> {
    #[derive(serde::Serialize)]
    struct Catalog<'a> {
        codecs: &'a [plan::VideoEncoderInfo],
    }
    serde_json::to_string(&Catalog {
        codecs: plan::video_encoder_catalog(),
    })
}

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

/// Whether libavcodec can resolve an encoder with this exact name.
///
/// This is the runtime half of the shipped-encoder contract (§13.6): the
/// Android build trims `libavcodec` down to an explicit `--enable-encoder`
/// list, so every name a preset may carry (#[`SHIPPED_VIDEO_ENCODERS`],
/// [`SHIPPED_AUDIO_ENCODERS`]) must still resolve there. A name that stops
/// resolving makes the editor option look merely "greyed out" instead of
/// failing loudly, which is exactly what this probe prevents.
#[cfg(feature = "ffmpeg")]
pub fn encoder_available(name: &str) -> bool {
    let Ok(name_c) = std::ffi::CString::new(name) else {
        return false;
    };
    unsafe { !crate::sys::avcodec_find_encoder_by_name(name_c.as_ptr()).is_null() }
}

/// Stub: without the `ffmpeg` feature no encoder is linked in.
#[cfg(not(feature = "ffmpeg"))]
pub const fn encoder_available(_name: &str) -> bool {
    false
}

// ── Progress telemetry (§1) ───────────────────────────────────────────────────

/// Snapshot of an HLS job's progress.
///
/// `content_ms` is monotonic *within one run*: it only grows as packets are
/// written, and stays `0` until the muxer has accepted its first packet. It is
/// measured from the run's seek anchor, so the absolute media time of the write
/// front is `start_ms + content_ms` (the single-session proxy reports exactly
/// that as `produced_ms`). `wall_ms` counts from the moment the job thread
/// actually started working (not from `open_job`).
#[derive(Clone, Copy, Debug, Default)]
pub struct JobProgress {
    /// Media time of the last packet handed to the muxer, relative to the run's
    /// seek anchor, in milliseconds.
    pub content_ms: i64,
    /// Seek anchor of this run: absolute media time the job started from, in
    /// milliseconds (`JobOpts::start_offset_secs * 1000`).
    pub start_ms: i64,
    /// Wall-clock milliseconds since the job thread started.
    pub wall_ms: i64,
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
    /// Video stream action as reported to the session status JSON:
    /// `"copy"` / `"hw"` / `"sw"` / `"none"`.
    pub video_action: Option<String>,
    /// Audio stream action: `"copy"` / `"sw"` / `"none"`.
    pub audio_action: Option<String>,
}

/// What one stream of a job does (contract §2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum StreamMode {
    /// No such stream in the output (source has none, or it is dropped).
    #[default]
    None,
    /// Packets are copied through unchanged.
    Copy,
    /// Encoded with a hardware encoder.
    Hardware,
    /// Encoded with a software encoder.
    Software,
}

impl StreamMode {
    /// Stable status-JSON token: `"none"`/`"copy"`/`"hw"`/`"sw"`.
    pub fn as_str(self) -> &'static str {
        match self {
            StreamMode::None => "none",
            StreamMode::Copy => "copy",
            StreamMode::Hardware => "hw",
            StreamMode::Software => "sw",
        }
    }
}

/// Per-stream decision for one HLS job, decided once from the source codecs and
/// the preset (contract §2).
///
/// The decision is deterministic for a given `(preset, source codecs, hardware
/// availability)` triple, so re-opening the same session (seek / pull-driven
/// restart) reproduces exactly the same stream layout — the segment grid must
/// never drift between runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionDecision {
    /// Video stream action.
    pub video: StreamMode,
    /// Video encoder name when [`video`](Self::video) is an `Encode` variant.
    pub video_encoder: Option<String>,
    /// Audio stream action.
    pub audio: StreamMode,
    /// Audio encoder name when [`audio`](Self::audio) is [`StreamMode::Software`].
    pub audio_encoder: Option<String>,
}

impl SessionDecision {
    /// Decide both streams for a source whose best video stream is
    /// `video_codec_id` (0 = no video stream) and best audio stream is
    /// `audio_codec_id` (0 = no audio stream).
    ///
    /// `hw_encoder_available` must mean "found by name *and* `avcodec_open2`
    /// succeeded" — presence in libavcodec is not proof on device. It is only
    /// consulted when the video stream must be re-encoded.
    pub fn decide(
        preset: &TranscodePreset,
        video_codec_id: u32,
        audio_codec_id: u32,
        hw_encoder_available: bool,
    ) -> Result<Self, String> {
        let (video, video_encoder) = if video_codec_id == 0 {
            (StreamMode::None, None)
        } else {
            match decide_video_action(preset, video_codec_id, hw_encoder_available)? {
                StreamAction::Copy => (StreamMode::Copy, Some("copy".to_string())),
                StreamAction::Encode { hardware, encoder } => (
                    if hardware {
                        StreamMode::Hardware
                    } else {
                        StreamMode::Software
                    },
                    Some(encoder.to_string()),
                ),
            }
        };
        let (audio, audio_encoder) = if audio_codec_id == 0 {
            (StreamMode::None, None)
        } else {
            match decide_audio_action(preset, audio_codec_id) {
                AudioAction::Copy => (StreamMode::Copy, Some("copy".to_string())),
                AudioAction::Encode { encoder } => {
                    (StreamMode::Software, Some(encoder.to_string()))
                }
            }
        };
        Ok(Self {
            video,
            video_encoder,
            audio,
            audio_encoder,
        })
    }

    /// Whether the video stream is copied through unchanged.
    pub fn video_is_copy(&self) -> bool {
        self.video == StreamMode::Copy
    }

    /// Whether the audio stream is copied through unchanged.
    pub fn audio_is_copy(&self) -> bool {
        self.audio == StreamMode::Copy
    }

    /// Whether any stream is present in the output.
    pub fn has_any_stream(&self) -> bool {
        self.video != StreamMode::None || self.audio != StreamMode::None
    }

    /// Status-JSON token for the video stream (`"copy"`/`"hw"`/`"sw"`/`"none"`).
    pub fn video_action_str(&self) -> &'static str {
        self.video.as_str()
    }

    /// Status-JSON token for the audio stream (`"copy"`/`"sw"`/`"none"`).
    pub fn audio_action_str(&self) -> &'static str {
        self.audio.as_str()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Shared progress cell written by the job thread and read by [`JobHandle`].
#[derive(Debug, Default)]
pub(crate) struct ProgressState {
    started_epoch_ms: AtomicU64,
    start_ms: AtomicI64,
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

    /// Record this run's seek anchor (absolute media milliseconds).
    pub(crate) fn set_start_ms(&self, start_ms: i64) {
        self.start_ms.store(start_ms, Ordering::Relaxed);
    }

    /// Record a packet at media time `pts_ms` (monotonic, never decreases).
    pub(crate) fn note_written(&self, pts_ms: i64) {
        self.content_ms.fetch_max(pts_ms.max(0), Ordering::Relaxed);
        self.packets.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> JobProgress {
        let started = self.started_epoch_ms.load(Ordering::Relaxed);
        let wall_ms = if started == 0 {
            0
        } else {
            Self::now_ms().saturating_sub(started) as i64
        };
        JobProgress {
            content_ms: self.content_ms.load(Ordering::Relaxed),
            start_ms: self.start_ms.load(Ordering::Relaxed),
            wall_ms,
            packets: self.packets.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn set_encoder(&self, mut info: EncoderInfo) {
        let mut cur = lock(&self.encoder);
        // The per-stream action is decided before the job opens any encoder and
        // is reported alongside the encoder choice, so keep it across updates.
        if info.video_action.is_none() {
            info.video_action = cur.video_action.clone();
        }
        if info.audio_action.is_none() {
            info.audio_action = cur.audio_action.clone();
        }
        *cur = info;
    }

    /// Record the per-stream actions reported through the session status JSON
    /// (contract §11.1): `copy`/`hw`/`sw`/`none`.
    pub(crate) fn set_actions(&self, video: &str, audio: &str) {
        let mut enc = lock(&self.encoder);
        enc.video_action = Some(video.to_string());
        enc.audio_action = Some(audio.to_string());
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

// ── Pause control (§ single-session throttling) ──────────────────────────────

/// Cooperative pause latch shared between [`JobHandle`] and the job thread.
///
/// The job loops call [`wait_if_paused`](PauseControl::wait_if_paused) at packet
/// boundaries; while paused they block on the condvar (no busy wait) and wake
/// immediately on [`resume`](PauseControl::resume), [`wake`](PauseControl::wake)
/// or a cancellation request. `pause`/`resume` are idempotent.
///
/// The condvar wait is bounded (100 ms) so a cancellation that races with the
/// pause cannot leave the thread sleeping: the loop re-checks `cancel` on every
/// wakeup.
#[derive(Debug, Default)]
pub(crate) struct PauseControl {
    paused: std::sync::atomic::AtomicBool,
    lock: Mutex<()>,
    cv: Condvar,
}

impl PauseControl {
    /// Ask the job to stop at its next packet boundary. Idempotent.
    pub(crate) fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }

    /// Let a paused job continue. Idempotent (also wakes waiters).
    pub(crate) fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
        self.wake();
    }

    /// Wake any waiter without changing the pause state (used by `cancel`).
    pub(crate) fn wake(&self) {
        let _guard = lock(&self.lock);
        self.cv.notify_all();
    }

    pub(crate) fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    /// Block while paused. Returns `true` when the caller must stop (cancel was
    /// requested, either before or during the wait).
    pub(crate) fn wait_if_paused(&self, cancel: &std::sync::atomic::AtomicBool) -> bool {
        if !self.paused.load(Ordering::Acquire) {
            return cancel.load(Ordering::Relaxed);
        }
        let mut guard = lock(&self.lock);
        while self.paused.load(Ordering::Acquire) {
            if cancel.load(Ordering::Relaxed) {
                return true;
            }
            let (next, _) = self
                .cv
                .wait_timeout(guard, Duration::from_millis(100))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard = next;
        }
        cancel.load(Ordering::Relaxed)
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
    /// BCP-47 / ISO 639 language tag from the container (`metadata=language`).
    pub language: Option<String>,
    /// Human readable track title from the container (`metadata=title`).
    ///
    /// Japanese releases commonly ship many subtitle tracks whose only
    /// identifiable difference is this field, so it has to reach the UI.
    #[serde(default)]
    pub title: Option<String>,
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

/// List the embedded subtitle tracks of a media URL.
///
/// This is the metadata API for the cast UI: it returns the same track records
/// that [`probe`] produces (index, codec, `language`, `title`), so callers do
/// not need a second demux pass just to populate a track picker. It is a thin
/// wrapper over [`probe`] on purpose — one libav open, one
/// `avformat_find_stream_info`.
#[cfg(feature = "ffmpeg")]
pub fn embedded_subtitle_tracks(url: &str) -> FfmpegResult<Vec<SubtitleStream>> {
    probe(url).map(|probed| probed.subtitles)
}

/// Stub: returns [`FfmpegError::Unavailable`] when `ffmpeg` feature is off.
#[cfg(not(feature = "ffmpeg"))]
pub fn embedded_subtitle_tracks(_url: &str) -> FfmpegResult<Vec<SubtitleStream>> {
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

/// Extract several embedded **text** subtitle streams in a single pass.
///
/// One `avformat_open_input` + one `avformat_find_stream_info` + one demux loop
/// feeds every requested stream into its own decoder, so a media file with many
/// embedded tracks costs one read pass instead of one per track. Each entry of
/// the returned map is the WebVTT text for that `stream_index`.
///
/// Text codecs only: bitmap subtitle tracks (PGS/DVD/DVB) are skipped and never
/// appear in the result (they need rendering, not text conversion). Unknown
/// indexes are skipped too; an empty `stream_indexes` returns an empty map
/// without opening the media at all.
///
/// Per-track failures are reported through [`crate::diag`] and simply omit that
/// track from the map; an unreadable media URL is still the only hard error.
#[cfg(feature = "ffmpeg")]
pub fn extract_subtitle_tracks(
    url: &str,
    stream_indexes: &[u32],
) -> FfmpegResult<std::collections::HashMap<u32, String>> {
    transcoder::extract_subtitle_tracks(url, stream_indexes)
}

/// Stub: returns [`FfmpegError::Unavailable`] when `ffmpeg` feature is off.
#[cfg(not(feature = "ffmpeg"))]
pub fn extract_subtitle_tracks(
    _url: &str,
    _stream_indexes: &[u32],
) -> FfmpegResult<std::collections::HashMap<u32, String>> {
    Err(FfmpegError::Unavailable(
        "FFmpeg not linked (build with --features ffmpeg)".into(),
    ))
}

// ── Source plan (decided before any segment is written) ─────────────────────

/// The segment plan for a source, decided from a single container open.
///
/// The proxy calls [`source_plan`] before it starts an HLS job so the synthesized
/// VOD playlist matches what the muxer will actually produce:
///
/// - video-copyable source (remux/hybrid) with a usable keyframe index →
///   [`PlanAlignment::Keyframe`], boundaries are the muxer's real cut points;
/// - video that must be re-encoded → [`PlanAlignment::Exact`], and the encoder is
///   forced to emit one keyframe per segment;
/// - no usable keyframe index (or no known duration) → `None`, the caller keeps
///   the warm-up/calibration fallback.
#[derive(Debug, Clone)]
pub struct SourcePlan {
    /// The plan the playlist should be synthesized from.
    pub plan: SegmentPlan,
    /// Whether the video stream is copied (remux/hybrid) rather than re-encoded.
    pub video_copied: bool,
    /// Number of keyframes the plan was built from (0 for exact plans).
    pub keyframe_count: usize,
    /// The per-stream decision this plan was built for. Passing it to every
    /// [`JobOpts::decision`] of the same session keeps re-opened runs on the
    /// exact same stream layout (contract §2).
    pub decision: SessionDecision,
}

/// Decide the segment plan for `url` and `preset` before any segment is written.
///
/// Opens the input exactly once: probes streams, picks the copy/encode path with
/// the same rules the job uses, and (for copy paths) reads the container's
/// keyframe index — one bounded `av_seek_frame` is performed only when the index
/// looks unparsed, so no media is demuxed for planning.
///
/// Returns `Ok(None)` when the source cannot be planned up front (unknown/short
/// duration, no video stream, unusable keyframe index); callers must then fall
/// back to measuring the real output.
#[cfg(feature = "ffmpeg")]
pub fn source_plan(url: &str, preset: &TranscodePreset) -> FfmpegResult<Option<SourcePlan>> {
    transcoder::source_plan_impl(url, preset)
}

/// Stub: returns [`FfmpegError::Unavailable`] when `ffmpeg` feature is off.
#[cfg(not(feature = "ffmpeg"))]
pub fn source_plan(_url: &str, _preset: &TranscodePreset) -> FfmpegResult<Option<SourcePlan>> {
    Err(FfmpegError::Unavailable(
        "FFmpeg not linked (build with --features ffmpeg)".into(),
    ))
}

// ── Measured segment durations / segment counts ──────────────────────────────

/// Read the measured segment durations (milliseconds) from a muxer-written
/// `index.m3u8`, in playlist order.
///
/// Diagnostic helper: the muxer records the real segment lengths as `#EXTINF`
/// lines. The VOD plan is decided up front (`source_plan`), never calibrated
/// from these measurements. Returns an empty vector when the playlist does not
/// exist yet or has no `#EXTINF` lines.
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
    /// expressed in milliseconds.
    ///
    /// The single-session producer sets this to `1` for `Keyframe`/`Exact`
    /// plans: the muxer then cuts on every video keyframe (copy paths) or every
    /// encoder IDR (`Exact`), so the real cut grid equals the plan regardless of
    /// which segment the run was anchored at. Audio-only runs keep the nominal
    /// segment length and use `hls_flags=split_by_time` instead.
    pub hls_time_override_ms: Option<u32>,
    /// Pre-decided per-stream layout (contract §2).
    ///
    /// When `Some`, the job uses it verbatim instead of deciding again — this is
    /// what keeps a seek / pull-driven restart on the same stream layout as the
    /// session's first run. When `None`, the job decides from the source codecs
    /// and the preset.
    pub decision: Option<SessionDecision>,
}

impl Default for JobOpts {
    fn default() -> Self {
        Self {
            input_url: String::new(),
            preset: preset::preset_h264(),
            start_offset_secs: 0.0,
            segment_dir: PathBuf::new(),
            start_number: 0,
            out_dir: None,
            hls_time_override_ms: None,
            decision: None,
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
    /// Cooperative pause latch shared with the job thread (single-session
    /// throttling: the proxy pauses a run that is far ahead of the client).
    pause: Arc<PauseControl>,
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
    /// tick (non-blocking — does not wait). Wakes a paused job so cancellation
    /// never deadlocks behind the pause latch.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.pause.wake();
    }

    /// Pause the job at its next packet boundary (single-session throttling).
    ///
    /// Idempotent. A paused job still counts as running (`is_running()` stays
    /// `true`) and never busy-waits: the producer thread blocks on a condvar
    /// until [`resume`](JobHandle::resume) or cancellation.
    pub fn pause(&self) {
        self.pause.pause();
    }

    /// Resume a paused job. Idempotent.
    pub fn resume(&self) {
        self.pause.resume();
    }

    /// Whether [`pause`](JobHandle::pause) is currently in effect.
    pub fn is_paused(&self) -> bool {
        self.pause.is_paused()
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

    /// Monotonic run progress: `start_ms` + `content_ms` is the absolute media
    /// time of the write front. Safe to call while the job runs (also while
    /// paused).
    pub fn progress(&self) -> JobProgress {
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
        self.pause.wake();
        // Never wait for the job thread here. The pull scheduler replaces a run
        // from inside the request path while holding the proxy's shared mutex,
        // and a cancelled job can take seconds to unwind (a blocked read on the
        // remote source is only bounded by the open/rw timeouts). Joining inline
        // would freeze every other request, so the wait happens on a helper
        // thread.
        //
        // Legacy jobs own their segment directory and clean it up after the
        // thread is gone. Persistent VOD cache dirs (`JobOpts::out_dir`) must
        // survive the job: dropping a handle only cancels, it never evicts
        // cached segments.
        let owned_dir = if self.owns_dir {
            Some(std::mem::take(&mut self.segment_dir))
        } else {
            None
        };
        let thread = self.thread.take();
        if thread.is_none() && owned_dir.is_none() {
            return;
        }
        std::thread::spawn(move || {
            if let Some(t) = thread {
                let _ = t.join();
            }
            if let Some(dir) = owned_dir {
                if dir.exists() {
                    let _ = std::fs::remove_dir_all(&dir);
                }
            }
        });
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
    pub fn pause(&self) {}
    pub fn resume(&self) {}
    pub fn is_paused(&self) -> bool {
        false
    }
    pub fn is_running(&self) -> bool {
        false
    }
    pub fn take_result(&mut self) -> Option<FfmpegResult<()>> {
        None
    }
    pub fn join(self) -> FfmpegResult<()> {
        Ok(())
    }
    pub fn progress(&self) -> JobProgress {
        JobProgress::default()
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
    let pause = Arc::new(PauseControl::default());
    let pause_t = Arc::clone(&pause);
    let input_url = opts.input_url.clone();
    let preset = opts.preset.clone();
    let preset_id = preset.id.clone();
    let start_offset = opts.start_offset_secs;
    let start_number = opts.start_number;
    let hls_time_override_ms = opts.hls_time_override_ms;
    let decision = opts.decision.clone();
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
                &pause_t,
                &id,
                start_number,
                hls_time_override_ms,
                decision,
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
        pause,
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
    fn builtin_presets_are_h264_and_hevc() {
        let all = preset::all_builtin_presets();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, "h264");
        assert_eq!(all[1].id, "hevc");
        assert!(all.iter().all(|p| p.is_builtin && p.validate().is_ok()));
    }

    // ── Per-stream decision (contract §2) ────────────────────────────────────

    fn decision_preset() -> TranscodePreset {
        preset::preset_h264()
    }

    #[test]
    fn decision_copy_copy_when_source_is_mpegts_compatible() {
        let p = decision_preset();
        let d = SessionDecision::decide(&p, preset::codec_ids::H264, preset::codec_ids::AAC, false)
            .expect("decision");
        assert_eq!(d.video, StreamMode::Copy);
        assert_eq!(d.audio, StreamMode::Copy);
        assert_eq!(d.video_action_str(), "copy");
        assert_eq!(d.audio_action_str(), "copy");
    }

    #[test]
    fn decision_encode_copy_uses_the_sw_encoder_and_copies_audio() {
        let mut p = decision_preset();
        p.video_copy = false;
        p.sw_video_encoder = "mpeg2video".to_string();
        let d = SessionDecision::decide(&p, preset::codec_ids::HEVC, preset::codec_ids::AAC, false)
            .expect("decision");
        assert_eq!(d.video, StreamMode::Software);
        assert_eq!(d.video_encoder.as_deref(), Some("mpeg2video"));
        assert_eq!(d.audio, StreamMode::Copy);
    }

    #[test]
    fn decision_copy_encode_re_encodes_audio_only() {
        let mut p = decision_preset();
        p.audio_codec = "ac3".to_string();
        let d = SessionDecision::decide(&p, preset::codec_ids::H264, preset::codec_ids::MP2, false)
            .expect("decision");
        assert_eq!(d.video, StreamMode::Copy);
        assert_eq!(d.audio, StreamMode::Software);
        assert_eq!(d.audio_encoder.as_deref(), Some("ac3"));
    }

    #[test]
    fn decision_hardware_when_available() {
        let mut p = decision_preset();
        p.video_copy = false;
        let d = SessionDecision::decide(&p, preset::codec_ids::HEVC, preset::codec_ids::AAC, true)
            .expect("decision");
        assert_eq!(d.video, StreamMode::Hardware);
        assert_eq!(d.video_encoder.as_deref(), Some("h264_mediacodec"));
    }

    #[test]
    fn decision_is_deterministic_for_reopened_runs() {
        let p = decision_preset();
        let first = SessionDecision::decide(&p, 27, 86018, false).expect("decision");
        let second = SessionDecision::decide(&p, 27, 86018, false).expect("decision");
        assert_eq!(first, second, "a reopen must not drift the stream layout");
    }

    #[test]
    fn decision_handles_audio_only_and_video_only_sources() {
        let p = decision_preset();
        let audio_only =
            SessionDecision::decide(&p, 0, preset::codec_ids::AAC, false).expect("decision");
        assert_eq!(audio_only.video, StreamMode::None);
        assert_eq!(audio_only.video_action_str(), "none");
        assert!(!audio_only.video_is_copy());
        assert!(audio_only.has_any_stream());

        let video_only =
            SessionDecision::decide(&p, preset::codec_ids::H264, 0, false).expect("decision");
        assert_eq!(video_only.audio, StreamMode::None);
        assert_eq!(video_only.audio_action_str(), "none");
        assert!(video_only.has_any_stream());
    }

    #[test]
    fn jobopts_default() {
        let o = JobOpts::default();
        assert!(o.input_url.is_empty());
        assert_eq!(o.preset.id, "h264");
        assert_eq!(o.start_offset_secs, 0.0);
        assert!(o.decision.is_none());
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
    fn progress_snapshot_carries_start_anchor() {
        let state = ProgressState::default();
        state.set_start_ms(90_000);
        let snap = state.snapshot();
        assert_eq!(snap.start_ms, 90_000, "start_ms is the job's seek anchor");
        assert_eq!(snap.content_ms, 0);
    }

    // ── Pause control (single-session throttling) ────────────────────────────

    #[test]
    fn pause_and_resume_are_idempotent() {
        let gate = PauseControl::default();
        assert!(!gate.is_paused());
        gate.pause();
        gate.pause();
        assert!(gate.is_paused());
        gate.resume();
        gate.resume();
        assert!(!gate.is_paused());
    }

    #[test]
    fn pause_gate_blocks_until_resumed() {
        use std::sync::atomic::AtomicBool;
        use std::sync::{mpsc, Arc};
        use std::time::Duration;

        let gate = Arc::new(PauseControl::default());
        let cancel = Arc::new(AtomicBool::new(false));
        gate.pause();

        let (tx, rx) = mpsc::channel();
        let gate_thread = Arc::clone(&gate);
        let cancel_thread = Arc::clone(&cancel);
        let waiter = std::thread::spawn(move || {
            let stopped = gate_thread.wait_if_paused(&cancel_thread);
            tx.send(stopped).unwrap();
        });

        // The gate must be blocked while paused (no busy wait, no early return).
        assert!(
            rx.recv_timeout(Duration::from_millis(150)).is_err(),
            "wait_if_paused must block while paused"
        );

        gate.resume();
        let stopped = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("resume must wake the pause gate");
        waiter.join().unwrap();
        assert!(!stopped, "a resumed run keeps going");
    }

    #[test]
    fn pause_gate_stops_when_cancelled_while_paused() {
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
        use std::sync::{mpsc, Arc};
        use std::time::Duration;

        let gate = Arc::new(PauseControl::default());
        let cancel = Arc::new(AtomicBool::new(false));
        gate.pause();

        let (tx, rx) = mpsc::channel();
        let gate_thread = Arc::clone(&gate);
        let cancel_thread = Arc::clone(&cancel);
        let waiter = std::thread::spawn(move || {
            let stopped = gate_thread.wait_if_paused(&cancel_thread);
            tx.send(stopped).unwrap();
        });

        // Still paused and not cancelled: the waiter must stay blocked.
        assert!(rx.recv_timeout(Duration::from_millis(120)).is_err());

        // Cancel has to wake the gate even though it is paused.
        cancel.store(true, AtomicOrdering::SeqCst);
        gate.wake();
        let stopped = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("cancel must wake the pause gate");
        waiter.join().unwrap();
        assert!(stopped, "cancel while paused stops the run");
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
