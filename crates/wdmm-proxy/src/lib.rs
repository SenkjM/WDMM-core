//! `wdmm-proxy` — Unified local HTTP range proxy.
//!
//! Serves registered streams over HTTP with `Range` support. Two back-source
//! modes (aligned with Dart `LocalStreamBridge`):
//!
//! 1. **Local file** — Rust reads the file directly from disk.
//! 2. **Driver callback** — caller supplies a `ranges` fn that fetches bytes
//!    (e.g. a cloud driver with Range support or a decrypting bridge).
//!
//! ## Token-only register API
//! `register_*` returns `Registration { token, path_template }`. Callers compose
//! `http://{bind_host}:{port}{path_template}` themselves. The proxy never returns
//! a full URL; HTTPS and custom certs are out of scope for this crate.
//!
//! ## Idle TTL
//! Server shuts down after the configured idle timeout (default 10 min). Every
//! incoming request resets the timer. The server auto-restarts on the next
//! `register_*` call.
//!
//! ## Security model
//! - Only explicitly registered tokens are served. Raw filesystem paths are
//!   never exposed in URLs.
//! - Default bind is `127.0.0.1`. DLNA renderers need a LAN bind address; tokens
//!   are the only access control in that configuration, matching the constraint
//!   that OpenAPI auth is separate from the proxy.

#![forbid(unsafe_code)]

use axum::{
    body::Body,
    extract::{ConnectInfo, Path as AxumPath, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
    routing::get,
    Router,
};
use bytes::Bytes;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_util::io::ReaderStream;
use tracing::{error, info, warn};

mod gate;
mod transcode_cache;
mod vod;

pub use gate::{
    JobGate, JobPermit, BUSY_ERROR, CAST_PRIORITY_ERROR, JOB_ACQUIRE_TIMEOUT,
    MAX_CONCURRENT_BACKGROUND_JOBS, MAX_CONCURRENT_HLS_JOBS,
};
pub use transcode_cache::{
    muxer_playlist_segments, segment_closed, CacheKey, SegmentLookup, TranscodeCache,
    TranscodeCacheStats, CACHE_PURGE_AFTER_SECS, DEFAULT_CACHE_BUDGET_BYTES, DEFAULT_CACHE_SUBDIR,
    GUARD_BACK_SEGMENTS,
};
pub use vod::{render_playlist, PlaylistSegment, PLAYLIST_CONTENT_TYPE, SEGMENT_CONTENT_TYPE};
pub use wdmm_ffmpeg::vod::{PlanAlignment, SegmentPlan};

/// Preset type re-exported for callers that build [`HlsSessionOpts`].
pub use wdmm_ffmpeg::TranscodePreset;
/// Progress telemetry re-exported from [`wdmm_ffmpeg`] so callers (FFI) only
/// depend on this crate for cast progress.
pub use wdmm_ffmpeg::{EncoderInfo, JobProgress};

// ── Error ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("ffmpeg error: {0}")]
    Ffmpeg(String),
    #[error("transcode cache error: {0}")]
    Cache(String),
    #[error("{0}")]
    Busy(String),
}

pub type ProxyResult<T> = std::result::Result<T, ProxyError>;

// ── BindMode ──────────────────────────────────────────────────────────────────

/// Proxy server bind mode — controls which network interfaces the server listens on.
///
/// - `Loopback` — bind to `127.0.0.1` (localhost only; DLNA devices cannot reach)
/// - `LanAny` — bind to `0.0.0.0` (all interfaces; OS routes to the appropriate one)
/// - `LanIf(ip)` — bind to specific interface IP (e.g., `"192.168.1.100"`)
/// - `Explicit(addr)` — bind to exact `SocketAddr` (for advanced control)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindMode {
    Loopback,
    LanAny,
    LanIf(String),
    Explicit(SocketAddr),
}

impl BindMode {
    /// Resolve to a bind address string for use with `TcpListener::bind`.
    pub fn to_bind_addr(&self, port: u16) -> String {
        match self {
            BindMode::Loopback => format!("127.0.0.1:{}", port),
            BindMode::LanAny => format!("0.0.0.0:{}", port),
            BindMode::LanIf(ip) => format!("{}:{}", ip, port),
            BindMode::Explicit(addr) => addr.to_string(),
        }
    }
}

// ── BoxStream ─────────────────────────────────────────────────────────────────

/// Pinned, boxed byte stream — same item type as `tokio_util::io::ReaderStream`.
pub type BoxStream =
    std::pin::Pin<Box<dyn futures_core::Stream<Item = std::io::Result<Bytes>> + Send>>;

// ── Registry entry ────────────────────────────────────────────────────────────

enum BackSource {
    LocalFile(PathBuf),
    DriverCallback(Arc<dyn Fn(i64, i64) -> BoxStream + Send + Sync>),
}

struct Entry {
    name: Option<String>,
    /// Known file size in bytes. `0` means unknown (streams to EOF).
    size: u64,
    source: BackSource,
}

// ── Token metadata ────────────────────────────────────────────────────────────

/// Security metadata for a registered token.
///
/// Tracks expiration and peer IP restrictions. Stored alongside `Entry` in
/// the `Shared` registry. Absence of a `TokenMeta` for an existing token
/// implies no expiration and no peer restrictions (backwards compat).
struct TokenMeta {
    /// Issuance time. Retained for diagnostics/future TTL-relative logic;
    /// not currently read by the gate checks (which use `expires_at`).
    #[allow(dead_code)]
    created_at: SystemTime,
    expires_at: Option<SystemTime>,
    /// If `Some`, only requests from these IPs are allowed. `None` = any peer.
    allowed_peers: Option<Vec<IpAddr>>,
}

// ── HLS session ───────────────────────────────────────────────────────────────

/// Cache configuration for one HLS session (contract §3.5).
#[derive(Debug, Clone, Default)]
pub struct HlsCacheOptions {
    /// Cache root; `None` → `<temp_dir>/wdmm-transcode`.
    pub root: Option<PathBuf>,
    /// Budget in bytes; `None` → 2 GiB.
    pub budget_bytes: Option<u64>,
    pub account_id: String,
    pub remote_path: String,
    pub preset_id: String,
    pub size: u64,
    /// Stable file identity (etag, or an mtime string). Empty = ephemeral key.
    pub ident: String,
}

impl HlsCacheOptions {
    /// Identity this session's segments are cached under.
    pub fn cache_key(&self) -> CacheKey {
        CacheKey::new(
            &self.account_id,
            &self.remote_path,
            &self.preset_id,
            self.size,
            &self.ident,
        )
    }
}

/// Everything needed to register one HLS cast session.
#[derive(Debug, Clone)]
pub struct HlsSessionOpts {
    pub device_id: String,
    pub input_url: String,
    pub preset: TranscodePreset,
    pub ttl_secs: Option<u64>,
    pub allowed_peers: Option<Vec<IpAddr>>,
    pub cache: HlsCacheOptions,
}

/// One running transcode job plus its bookkeeping.
struct HlsRun {
    job: wdmm_ffmpeg::JobHandle,
    /// The job reached the end of the media.
    ended: bool,
    /// Global-job-gate permit.
    ///
    /// Released as soon as the job thread finishes (a finished job holds no
    /// encoder), while the run itself stays around to keep reporting telemetry.
    /// Without this a completed session would keep the single gate slot forever
    /// and every other cast would fail with `transcoding busy`.
    permit: Option<JobPermit>,
}

/// An active HLS VOD session keyed by a proxy token.
///
/// The transcode job no longer owns the playlist: the playlist is synthesized
/// from [`SegmentPlan`] and segments are produced on demand. Dropping the
/// session kills the job but leaves its cached segments on disk (contract §3.5).
struct HlsSession {
    /// Segment plan, decided once when the session is created.
    ///
    /// There is deliberately no "plan pending" state: a playlist served before
    /// the grid is final can never be corrected (a renderer reads a VOD playlist
    /// once), which is exactly what made seeks land on the wrong content.
    plan: Arc<SegmentPlan>,
    cache: Arc<TranscodeCache>,
    cache_key: CacheKey,
    cache_dir: PathBuf,
    preset: TranscodePreset,
    input_url: String,
    seg_secs: f64,
    run: Option<HlsRun>,
    /// Error text of the last failed run.
    job_error: Option<String>,
    /// Newest segment index the renderer asked for.
    max_requested_k: u64,
    /// Cache hits served for this session.
    hits: u64,
    /// Last job (re)start; at most one reopen per [`HLS_REOPEN_INTERVAL`].
    last_reopen: Option<Instant>,
    /// The plan has been fully produced.
    ended: bool,
    /// `hls_time` override (ms) that makes the muxer cut on the plan's grid.
    hls_time_override_ms: Option<u32>,
    /// Segment index the current run is anchored at.
    ///
    /// Requests below it, or far ahead of the produced front, need a fresh job;
    /// anything in between simply waits for the running one.
    anchor_k: u64,
}

/// Result of a successful [`ProxyHandle::register_hls`] call.
///
/// Callers compose the playlist URL as:
/// ```text
/// http://{bind_host}:{port}{playlist_template}
/// ```
/// The proxy never returns a full URL.
#[derive(Debug, Clone)]
pub struct HlsRegistration {
    /// 32-hex, 128-bit random token.
    pub token: String,
    /// URL path to the master playlist, e.g. `"/h/3f7a…/index.m3u8"`.
    pub playlist_template: String,
}

/// Live progress of one HLS session.
#[derive(Debug, Clone)]
pub struct HlsProgress {
    /// The segment plan exists, so the playlist can be served.
    pub playlist_ready: bool,
    /// Number of `seg*.ts` files already on disk for this session.
    pub segments: u32,
    /// Media time produced so far (monotonic).
    pub content_ms: i64,
    /// Wall-clock time since the job thread started.
    pub wall_ms: u64,
    /// `content_ms / wall_ms`; `0.0` before the wall clock starts.
    pub speed: f64,
    /// The plan has been fully produced.
    pub ended: bool,
    /// Error text when the job failed.
    pub error: Option<String>,
    /// Encoder selection actually used (§7.3).
    pub encoder: EncoderInfo,
    /// Deduplicated encoder warnings, capped at 8 (§7.3).
    pub warnings: Vec<String>,
    /// `"active"` (job running) / `"idle"` (parked, runway sufficient) / `"error"`.
    pub run_state: &'static str,
    /// Plan alignment strategy, when the plan exists.
    pub alignment: Option<&'static str>,
    /// Highest segment index known complete.
    pub ready_k: Option<u64>,
    /// Newest segment index the renderer asked for.
    pub max_requested_k: Option<u64>,
    /// Media duration from the plan (FFI `duration_ms` source).
    pub plan_duration_ms: Option<i64>,
    /// Nominal segment length.
    pub seg_secs: f64,
    /// Number of segments in the plan.
    pub segment_count: usize,
    /// `(actual produced media ms) - (planned ms)` at the ready front.
    pub drift_ms: Option<i64>,
    /// Cache hits served for this session.
    pub cache_hits: u64,
    /// Bytes attributed to this session's cache directory.
    pub cache_bytes: u64,
    /// Cache accounting is over budget because the play guard band is kept.
    pub over_budget: bool,
}

/// Derive [`HlsProgress`] from a live session.
///
/// Reads the cache directory directly: the job thread is the only writer, so a
/// directory listing is enough to know which segments are complete.
fn derive_hls_progress(session: &HlsSession) -> HlsProgress {
    let plan = session.plan.as_ref();
    let plan_len = plan.segment_count() as u64;
    let (ready_k, ready_count) = scan_segments(&session.cache_dir);

    let (run_content_ms, wall_ms, encoder, warnings, running) = match session.run.as_ref() {
        Some(run) => {
            let snapshot = run.job.progress();
            (
                snapshot.content_ms,
                snapshot.wall_ms.max(0) as u64,
                run.job.encoder_info(),
                run.job.warnings(),
                run.job.is_running(),
            )
        }
        None => (0, 0, EncoderInfo::default(), Vec::new(), false),
    };

    // Report *absolute media time*, not this run's own output length: the UI asks
    // "where in the film are we", and after a seek the run is anchored in the
    // middle of the media and starts counting at zero again. `planned_ms` is the
    // end of the ready front measured on the plan grid.
    // `scan_segments` reports the highest closed segment in the whole cache
    // directory, which after a seek can be far *behind* the anchor: the segments
    // of the previous run are still on disk. Using it verbatim placed the buffer
    // twenty minutes before the anchor and made `drift_ms` report exactly the
    // seek position, so clamp the front to the current anchor.
    let front_k = front_segment(ready_k, session.anchor_k, plan_len);
    let planned_ms = plan
        .entries
        .iter()
        .take(front_k.min(plan_len.saturating_sub(1)) as usize + 1)
        .map(|entry| entry.duration_ms)
        .sum::<i64>();
    let content_ms = planned_ms.max(0);
    // Encode speed is a property of the job, so it keeps using the run's own
    // produced duration over its wall clock.
    let speed = if wall_ms < SPEED_WINDOW_MS {
        // A job that just started (and any copy-mode job) can hand the muxer a
        // whole segment within a few milliseconds, so the ratio is not yet
        // meaningful; report "unknown" instead of a four-digit multiplier.
        0.0
    } else {
        (run_content_ms as f64 / wall_ms as f64).clamp(0.0, MAX_REPORTED_SPEED)
    };

    // Deduplicate while preserving order, keep at most 8.
    let mut seen: Vec<String> = Vec::new();
    let warnings: Vec<String> = warnings
        .into_iter()
        .filter(|w| {
            if seen.len() >= 8 || seen.iter().any(|s| s == w) {
                false
            } else {
                seen.push(w.clone());
                true
            }
        })
        .collect();

    let run_state = if session.job_error.is_some() {
        "error"
    } else if running {
        "active"
    } else {
        "idle"
    };

    let cache_stats = session.cache.stats();
    // Both sides of the subtraction are now absolute media time, so drift is
    // bounded by one segment. The previous version compared a run-relative
    // duration with an absolute plan position, which is why the reported value
    // jumped to minutes within seconds of a mid-film seek.
    let drift_ms = session.run.as_ref().map(|_run| {
        let absolute = plan.start_ms(session.anchor_k).unwrap_or(0) + run_content_ms;
        absolute - planned_ms
    });

    HlsProgress {
        // The playlist exists before the session is registered: the renderer can
        // always be pointed at it, and every advertised segment is served (the
        // job is started on demand), so there is no "preparing" state any more.
        playlist_ready: true,
        segments: ready_count,
        content_ms,
        wall_ms,
        speed,
        ended: session.ended || (plan_len > 0 && front_k + 1 >= plan_len),
        error: session.job_error.clone(),
        encoder,
        warnings,
        run_state,
        alignment: Some(alignment_label(plan.alignment)),
        ready_k,
        max_requested_k: Some(session.max_requested_k),
        plan_duration_ms: Some(plan.duration_ms),
        seg_secs: session.seg_secs,
        segment_count: plan_len as usize,
        drift_ms,
        cache_hits: session.hits,
        cache_bytes: session.cache.bytes_of(&session.cache_key),
        over_budget: cache_stats.over_budget,
    }
}

/// Which plan segment the buffered front reaches, never behind `anchor_k`.
///
/// `ready_k` comes from a directory scan, so it is a global maximum: after a
/// seek into the middle of the film the old run's segments are still there and
/// the scan happily reports segment 20 while the session is anchored at segment
/// 900. Telemetry must describe the *current* anchor, so anything below it is
/// ignored, and the index is clamped to the plan length.
fn front_segment(ready_k: Option<u64>, anchor_k: u64, plan_len: u64) -> u64 {
    let front = ready_k.unwrap_or(anchor_k).max(anchor_k);
    front.min(plan_len.saturating_sub(1))
}

/// Human/FFI label of a plan alignment.
pub fn alignment_label(alignment: PlanAlignment) -> &'static str {
    match alignment {
        PlanAlignment::Exact => "exact",
        PlanAlignment::Keyframe => "keyframe",
    }
}

/// Playlist lines for a plan.
fn playlist_segments(plan: &SegmentPlan) -> Vec<PlaylistSegment> {
    plan.entries
        .iter()
        .map(|entry| PlaylistSegment::new(entry.start_ms, entry.duration_ms))
        .collect()
}

/// `(highest complete index, number of complete files)` in a segment directory.
///
/// "Complete" means the muxer closed the file and listed it in its own playlist:
/// a segment that is still being written has a file but is not listed yet, and
/// counting it would let telemetry (and the `ended` flag) run ahead of reality.
fn scan_segments(dir: &Path) -> (Option<u64>, u32) {
    let closed = muxer_playlist_segments(dir);
    let Ok(read) = std::fs::read_dir(dir) else {
        return (None, 0);
    };
    let mut ready_k: Option<u64> = None;
    let mut count = 0u32;
    for entry in read.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(rest) = name.strip_prefix("seg") else {
            continue;
        };
        let Some(digits) = rest.strip_suffix(".ts") else {
            continue;
        };
        let Ok(k) = digits.parse::<u64>() else {
            continue;
        };
        if !segment_closed(&closed, k) {
            continue;
        }
        let non_empty = entry.metadata().map(|meta| meta.len() > 0).unwrap_or(false);
        if !non_empty {
            continue;
        }
        count = count.saturating_add(1);
        ready_k = Some(ready_k.map_or(k, |current| current.max(k)));
    }
    (ready_k, count)
}

/// How long [`serve_hls`] waits for a segment the running job is about to
/// produce before answering `504`.
///
/// A 504 with an explicit error is far better than 404: the renderer retries a
/// 5xx, while a 404 makes it drop the session. The window must cover one job
/// restart plus one segment: a restart re-opens the input (~2.5 s on a remote
/// WebDAV source) and the first segment then needs `seg_secs` of *content*
/// time, which on a software encoder can take several seconds of wall time.
/// Renderers hold the connection open for this long, so err generous.
const HLS_SEGMENT_WAIT: Duration = Duration::from_secs(20);
/// How far ahead of the newest request the single job may run before it is
/// throttled. One session means the spare capacity belongs to this cast, so the
/// window only exists to stop a fast encoder from filling the disk unbounded.
pub const AHEAD_SEGMENTS: u64 = 12;

/// Poll interval used by the HLS waits.
const HLS_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Minimum interval between two transcode job (re)starts of one session.
const HLS_REOPEN_INTERVAL: Duration = Duration::from_secs(1);

/// On-disk shape version of [`StoredPlan`].
///
/// Bump whenever the plan semantics change so cached plans from an older build
/// are rejected instead of trusted: `version` has **no** serde default, so a
/// `plan.json` written by an older build fails to deserialize and that cache
/// entry is dropped (see `TranscodeCache::load_plan`).
///
/// v3: the plan may now be the source's keyframe grid (`alignment: "keyframe"`),
/// decided before any segment is written.
///
/// v4: the sparse-keyframe-index test now compares the index against the media
/// duration instead of a fixed entry count. Plans cached by v3 were produced by
/// that broken test and are therefore full re-encode grids for sources that do
/// have a usable index (a 24-minute MKV with 3 lazy `Cues` entries, or a 2.3 h
/// MKV whose index only looked dense). Bumping the version throws them away once
/// so the source is planned again; re-planning costs one open + index read.
const PLAN_FORMAT_VERSION: u32 = 4;

/// Plan record persisted as `<cache_dir>/plan.json`.
///
/// `SegmentPlan` itself lives in `wdmm-ffmpeg` and is not `serde`-derived, so
/// the proxy owns the stable on-disk shape.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StoredPlan {
    version: u32,
    duration_ms: i64,
    seg_secs: f64,
    alignment: String,
    gop_ms: Option<i64>,
    entries: Vec<StoredSeg>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StoredSeg {
    start_ms: i64,
    duration_ms: i64,
}

impl StoredPlan {
    fn from_plan(plan: &SegmentPlan) -> Self {
        Self {
            version: PLAN_FORMAT_VERSION,
            duration_ms: plan.duration_ms,
            seg_secs: plan.seg_secs,
            alignment: alignment_label(plan.alignment).to_string(),
            gop_ms: plan.gop_ms,
            entries: plan
                .entries
                .iter()
                .map(|entry| StoredSeg {
                    start_ms: entry.start_ms,
                    duration_ms: entry.duration_ms,
                })
                .collect(),
        }
    }

    /// Rebuild the plan, refusing anything an older build wrote.
    ///
    /// The on-disk version is a hard gate, not a debug assertion: a plan written
    /// with different grid semantics used to be trusted silently, which is how a
    /// pre-keyframe build left an approximate 4 s grid on a device and every
    /// seek landed on the wrong content (`source_plan` was never even called).
    fn into_plan(self) -> Option<SegmentPlan> {
        if self.version != PLAN_FORMAT_VERSION {
            return None;
        }
        let alignment = match self.alignment.as_str() {
            "exact" => PlanAlignment::Exact,
            "keyframe" => PlanAlignment::Keyframe,
            _ => return None,
        };
        let plan = SegmentPlan {
            duration_ms: self.duration_ms,
            seg_secs: self.seg_secs,
            alignment,
            gop_ms: self.gop_ms,
            entries: self
                .entries
                .into_iter()
                .map(|entry| wdmm_ffmpeg::vod::SegEntry {
                    start_ms: entry.start_ms,
                    duration_ms: entry.duration_ms,
                })
                .collect(),
        };
        // An empty plan describes nothing, and one entry is not a seekable grid.
        if plan.entries.len() < 2 {
            return None;
        }
        Some(plan)
    }
}

/// `hls_time` (ms) that makes the HLS muxer cut on every video keyframe.
///
/// `hlsenc` cuts when `pkt_pts - run_start >= number * hls_time`. A 1 ms
/// threshold is satisfied by every keyframe after the segment's first packet, so
/// the cuts become exactly the keyframe sequence and no longer depend on where a
/// run was started — which is what makes keyframe/transcode plans restart-safe.
const KEYFRAME_CUT_HLS_TIME_MS: u32 = 1;

/// Wall-clock window a run must be alive for before `speed` is reported.
///
/// Below it the ratio is dominated by start-up: a copy-mode run can write a whole
/// keyframe segment in the first few milliseconds, and reporting that as "450x"
/// made the warm-up panel look broken.
const SPEED_WINDOW_MS: u64 = 2_000;

/// Upper bound for a reported encode speed. Real remux runs reach 20-40x, but
/// anything above this is still start-up noise rather than a useful measurement.
const MAX_REPORTED_SPEED: f64 = 100.0;

/// Start (or restart) a session's transcode run. See
/// [`ProxyHandle::spawn_run`].
async fn spawn_run_shared(
    shared: &Arc<Mutex<Shared>>,
    token: &str,
    first_k: u64,
) -> ProxyResult<()> {
    wdmm_ffmpeg::diag::log(format!("[proxy] hls run start at segment {first_k}"));
    let gate = { Arc::clone(&shared.lock().unwrap().gate) };
    // A previous job may have finished while its session is still alive.
    release_finished_permits(shared);
    {
        let g = shared.lock().unwrap();
        if let Some(session) = g.hls_sessions.get(token) {
            let reusable = session.anchor_k == first_k
                && session.run.as_ref().is_some_and(|run| run.job.is_running());
            if reusable {
                return Ok(());
            }
        }
    }
    // A *paused* job still reports `is_running()`, so it would keep the single
    // gate slot and the acquire below would time out with `transcoding busy` —
    // this is what made every seek fail. The jump replaces the run anyway, so
    // drop it first; `JobHandle::drop` cancels and joins on a helper thread, so
    // this returns immediately.
    drop_previous_run(shared, token);
    // Claim the reopen slot before the open: a seek makes the renderer ask for
    // several segments at once, and without this mark every one of them would
    // cancel the other's freshly opened job.
    {
        let mut g = shared.lock().unwrap();
        if let Some(session) = g.hls_sessions.get_mut(token) {
            session.last_reopen = Some(Instant::now());
        }
    }
    let permit = gate
        .acquire_async(JOB_ACQUIRE_TIMEOUT)
        .await
        .map_err(|error| {
            if let Ok(mut g) = shared.lock() {
                if let Some(session) = g.hls_sessions.get_mut(token) {
                    session.job_error = Some(error.clone());
                }
            }
            ProxyError::Busy(error)
        })?;
    if permit.preempted() {
        // Contract §3.6: the cast owns the slot; the in-flight background job is
        // revoked but keeps running until its libav call returns.
        tracing::info!(
            token,
            preemptions = gate.preemptions(),
            "cast transcode preempted a background subtitle job"
        );
    }

    let (input_url, preset, cache_dir, start_ms, hls_time_override_ms) = {
        let g = shared.lock().unwrap();
        let session = g
            .hls_sessions
            .get(token)
            .ok_or_else(|| ProxyError::Cache("HLS session vanished".to_string()))?;
        let start_ms = {
            let plan = session.plan.as_ref();
            let start = plan.start_ms(first_k).unwrap_or(0);
            if first_k > 0 && plan.alignment == wdmm_ffmpeg::PlanAlignment::Keyframe {
                // A keyframe plan's starts come from the container index, whose
                // timestamps can lag the packet PTS (MP4 indexes store DTS), so
                // seeking exactly to one can land on the previous keyframe. A
                // nudge inside the interval still lands on the intended
                // keyframe, and the muxer cuts on every keyframe, so where the
                // run is anchored no longer changes the produced segment grid.
                //
                // The nudge must stay well inside *this* interval: a segment can
                // be much shorter than the median GOP, and overshooting its end
                // would anchor the run on a later keyframe than segment
                // `first_k` names.
                let gap = plan.duration_of(first_k).unwrap_or(0).max(0);
                let half_gop = plan.gop_ms.unwrap_or(0).max(0) / 2;
                start + (gap / 2).min(half_gop)
            } else {
                start
            }
        };
        (
            session.input_url.clone(),
            session.preset.clone(),
            session.cache_dir.clone(),
            start_ms,
            session.hls_time_override_ms,
        )
    };

    let job = wdmm_ffmpeg::open_job(wdmm_ffmpeg::JobOpts {
        input_url,
        preset,
        start_offset_secs: start_ms as f64 / 1000.0,
        segment_dir: cache_dir.clone(),
        start_number: first_k,
        out_dir: Some(cache_dir),
        hls_time_override_ms,
        // Each run decides its stream layout from the source codecs + preset;
        // the proxy does not pin a decision across restarts.
        decision: None,
    })
    .map_err(|e| ProxyError::Ffmpeg(e.to_string()))?;

    let mut g = shared.lock().unwrap();
    if let Some(session) = g.hls_sessions.get_mut(token) {
        session.run = Some(HlsRun {
            job,
            ended: false,
            permit: Some(permit),
        });
        // A run is anchored where it starts; progress and drift are reported
        // relative to this anchor on the absolute media timeline.
        session.anchor_k = first_k;
        session.ended = false;
        session.last_reopen = Some(Instant::now());
        if session.job_error.as_deref() == Some(BUSY_ERROR) {
            session.job_error = None;
        }
    }
    Ok(())
}

/// Throttle the running job once it is far enough ahead of the newest request.
///
/// The job is *paused*, not cancelled: a cancel closes the container and throws
/// away the encoder, and reopening costs a full remote read plus a fresh
/// keyframe anchor. Pausing keeps the process (and its decoder) warm, so the next
/// request just resumes it — the same idea as Jellyfin's transcoding throttler.
fn park_run_shared(shared: &Arc<Mutex<Shared>>, token: &str, requested_k: u64) {
    let mut g = shared.lock().unwrap();
    let Some(session) = g.hls_sessions.get_mut(token) else {
        return;
    };
    let plan_len = session.plan.segment_count() as u64;
    let front = front_segment(
        scan_segments(&session.cache_dir).0,
        session.anchor_k,
        plan_len,
    );
    let Some(run) = session.run.as_mut() else {
        return;
    };
    let ahead = front.saturating_sub(requested_k);
    if ahead > AHEAD_SEGMENTS && !run.job.is_paused() {
        info!("HLS run throttled at segment {front} (requested {requested_k}, {ahead} ahead)");
        run.job.pause();
        // A paused job produces nothing, so it must not keep the single
        // transcode slot: otherwise the subtitle probe (a background job) is
        // rejected with `transcoding busy` for as long as the viewer stays
        // inside the buffered window. The permit is re-acquired on resume.
        run.permit = None;
    }
}

/// Drop a session's run so the next one can take the gate slot.
fn drop_previous_run(shared: &Arc<Mutex<Shared>>, token: &str) {
    let Ok(mut g) = shared.lock() else {
        return;
    };
    let Some(session) = g.hls_sessions.get_mut(token) else {
        return;
    };
    let Some(run) = session.run.take() else {
        return;
    };
    // Cancel first so the muxer stops appending, then drop the unfinished tail it
    // leaves behind. Run replacement happens on every seek, so a stale truncated
    // segment would otherwise be served for the rest of the session.
    drop(run);
    let purged = TranscodeCache::purge_partial_segments(&session.cache_dir, session.anchor_k);
    wdmm_ffmpeg::diag::log(format!(
        "[proxy] hls run replaced (session {token}, dropped {purged} unfinished segment(s))"
    ));
}

/// Release the global gate permit of every job that already finished.
///
/// A finished [`HlsRun`] stays in its session so the UI keeps seeing the last
/// encode telemetry, but it must not keep the single gate slot: the next cast
/// would otherwise fail with `transcoding busy` even though nothing is encoding.
fn release_finished_permits(shared: &Arc<Mutex<Shared>>) {
    let Ok(mut g) = shared.lock() else {
        return;
    };
    for session in g.hls_sessions.values_mut() {
        let Some(run) = session.run.as_mut() else {
            continue;
        };
        if run.job.is_running() {
            continue;
        }
        run.ended = true;
        if let Some(Err(e)) = run.job.take_result() {
            session.job_error = Some(e.to_string());
        }
        run.permit = None;
    }
}

/// Wait until a session's segment plan exists (calibration done / index read).
/// Parse `seg<k>.ts` into `k`.
fn parse_segment_index(seg: &str) -> Option<u64> {
    seg.strip_prefix("seg")
        .and_then(|rest| rest.strip_suffix(".ts"))
        .and_then(|digits| digits.parse::<u64>().ok())
}

// ── Shared server state ───────────────────────────────────────────────────────

struct Shared {
    entries: HashMap<String, Entry>,
    /// Security metadata for registered tokens (expiration, peer whitelist).
    token_meta: HashMap<String, TokenMeta>,
    /// Active HLS transcode sessions keyed by their proxy token.
    hls_sessions: HashMap<String, HlsSession>,
    /// Track active device+media pairs to enforce one-transcode-per-combo.
    /// Maps (device_id, media_path) -> token. When a new request arrives for the
    /// same combo, the old session is killed and replaced.
    device_media_map: HashMap<(String, String), String>,
    addr: Option<SocketAddr>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    /// Drop the old sender to cancel the previous idle task; assign new one.
    idle_cancel_tx: Option<oneshot::Sender<()>>,
    idle_timeout: Duration,
    /// Global transcode admission gate (also reached from the router).
    gate: Arc<JobGate>,
}

// ── Registration ─────────────────────────────────────────────────────────────

/// Result of a successful `register_*` call.
///
/// Callers compose the playback URL as:
/// ```text
/// http://{bind_host}:{port}{path_template}
/// ```
/// The proxy never constructs a full URL (no scheme/host/port embedded).
#[derive(Debug, Clone)]
pub struct Registration {
    /// 32-hex, 128-bit random token.
    pub token: String,
    /// URL path component, e.g. `"/s/3f7a…"`.
    pub path_template: String,
}

// ── ProxyHandle ───────────────────────────────────────────────────────────────

/// Unified local HTTP range proxy handle.
///
/// The server starts lazily on the first `register_*` and shuts down after
/// `idle_timeout` of inactivity. Dropping *the last* handle stops the server;
/// clones share one listener, so dropping a clone is harmless.
#[derive(Clone)]
pub struct ProxyHandle {
    bind_mode: BindMode,
    port: u16,
    /// Stored so `ensure_server` can pass it to `arm_idle_timer` on first start.
    #[allow(dead_code)]
    idle_timeout: Duration,
    shared: Arc<Mutex<Shared>>,
    /// Segment cache shared by every HLS session of this process.
    cache: Arc<TranscodeCache>,
    /// Global transcode admission gate (also used by FFI subtitle extraction).
    gate: Arc<JobGate>,
    /// Reference marker for handle clones.
    ///
    /// `Arc::strong_count(&self.shared)` cannot answer "is this the last
    /// handle": the axum task and its router hold `Arc`s of the same state for
    /// as long as the server runs. This marker is cloned only by `Clone`, so
    /// `strong_count == 1` really means "the last handle is going away".
    handles: Arc<()>,
}

/// Sanitise a caller-supplied filename for use as a URL path segment.
///
/// Renderers only read `A-Za-z0-9`, `.`, `_` and `-` reliably out of a filename,
/// so everything else (spaces, CJK, brackets) collapses to `_` and the result is
/// capped at 120 bytes.
fn url_safe_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(120));
    for ch in raw.chars() {
        if out.len() >= 120 {
            break;
        }
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    out
}

impl ProxyHandle {
    /// Create a handle with the given bind mode and port, using the default 10-minute idle timeout.
    ///
    /// Use `port = 0` to let the OS assign a free port.
    pub fn new(bind_mode: BindMode, port: u16) -> Self {
        Self::with_idle_timeout(bind_mode, port, Duration::from_secs(600))
    }

    /// Create a handle with a custom idle timeout.
    pub fn with_idle_timeout(bind_mode: BindMode, port: u16, idle_timeout: Duration) -> Self {
        let cache = Arc::new(TranscodeCache::default());
        let gate = JobGate::global();
        let shared = Arc::new(Mutex::new(Shared {
            entries: HashMap::new(),
            token_meta: HashMap::new(),
            hls_sessions: HashMap::new(),
            device_media_map: HashMap::new(),
            addr: None,
            shutdown_tx: None,
            idle_cancel_tx: None,
            idle_timeout,
            gate: Arc::clone(&gate),
        }));
        Self {
            bind_mode,
            port,
            idle_timeout,
            shared,
            cache,
            gate,
            handles: Arc::new(()),
        }
    }

    /// Segment cache backing every HLS session of this handle.
    pub fn transcode_cache(&self) -> Arc<TranscodeCache> {
        Arc::clone(&self.cache)
    }

    /// Global transcode admission gate (shared with FFI subtitle extraction).
    pub fn job_gate(&self) -> Arc<JobGate> {
        Arc::clone(&self.gate)
    }

    /// Return the bound `SocketAddr`, or `None` when the server is not running.
    pub fn addr(&self) -> Option<SocketAddr> {
        self.shared.lock().unwrap().addr
    }

    // ── Register ──────────────────────────────────────────────────────────────

    /// Register a local file. Returns a `Registration`.
    ///
    /// `name` is used only for Content-Type guessing (e.g. `"song.mp3"`).
    ///
    /// `ttl_secs` — optional time-to-live in seconds; `None` = no expiration.
    ///
    /// `allowed_peers` — optional IP whitelist; `None` = any peer allowed.
    pub async fn register_file(
        &self,
        path: impl Into<PathBuf>,
        name: Option<String>,
        ttl_secs: Option<u64>,
        allowed_peers: Option<Vec<IpAddr>>,
    ) -> ProxyResult<Registration> {
        let path = path.into();
        let size = tokio::fs::metadata(&path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        self.insert_entry(
            Entry {
                name,
                size,
                source: BackSource::LocalFile(path),
            },
            None,
            ttl_secs,
            allowed_peers,
        )
        .await
    }

    /// Register a driver Range callback back-source.
    ///
    /// `size` — known file size in bytes; pass `0` if unknown.
    /// `ranges_fn(start, end)` — `start` and `end` are both inclusive byte
    /// offsets. `end = -1` means read to EOF (size unknown).
    ///
    /// `ttl_secs` — optional time-to-live in seconds; `None` = no expiration.
    ///
    /// `allowed_peers` — optional IP whitelist; `None` = any peer allowed.
    pub async fn register_driver<F>(
        &self,
        size: u64,
        name: Option<String>,
        ttl_secs: Option<u64>,
        allowed_peers: Option<Vec<IpAddr>>,
        ranges_fn: F,
    ) -> ProxyResult<Registration>
    where
        F: Fn(i64, i64) -> BoxStream + Send + Sync + 'static,
    {
        self.insert_entry(
            Entry {
                name,
                size,
                source: BackSource::DriverCallback(Arc::new(ranges_fn)),
            },
            None,
            ttl_secs,
            allowed_peers,
        )
        .await
    }

    /// Register a driver Range callback whose URL carries a readable filename.
    ///
    /// `url_name` ends up in the URL path (`/s/{token}/{url_name}`), which is how
    /// a DLNA renderer learns a subtitle's language and label: Kodi parses the
    /// URL filename, not the DIDL. The proxy only sanitises it to URL-safe
    /// characters; the token stays the lookup key.
    pub async fn register_driver_named<F>(
        &self,
        size: u64,
        name: Option<String>,
        url_name: Option<String>,
        ttl_secs: Option<u64>,
        allowed_peers: Option<Vec<IpAddr>>,
        ranges_fn: F,
    ) -> ProxyResult<Registration>
    where
        F: Fn(i64, i64) -> BoxStream + Send + Sync + 'static,
    {
        self.insert_entry(
            Entry {
                name,
                size,
                source: BackSource::DriverCallback(Arc::new(ranges_fn)),
            },
            url_name,
            ttl_secs,
            allowed_peers,
        )
        .await
    }

    /// Revoke a token. The entry is removed immediately; in-flight requests
    /// will receive a 404 on the next sub-request after removal.
    pub fn unregister(&self, token: &str) {
        let mut g = self.shared.lock().unwrap();
        g.entries.remove(token);
        g.token_meta.remove(token);
    }

    /// Revoke a token immediately. Alias for `unregister` (semantic clarity for
    /// security-focused use cases).
    pub fn revoke_token(&self, token: &str) {
        self.unregister(token);
    }

    /// Renew the driver callback for an existing token (e.g. after re-auth).
    /// No-op if the token is unknown or its source is a local file.
    pub fn renew_callback<F>(&self, token: &str, ranges_fn: F)
    where
        F: Fn(i64, i64) -> BoxStream + Send + Sync + 'static,
    {
        let mut g = self.shared.lock().unwrap();
        if let Some(entry) = g.entries.get_mut(token) {
            entry.source = BackSource::DriverCallback(Arc::new(ranges_fn));
        }
    }

    // ── HLS register / seek / unregister ─────────────────────────────────────

    /// Register an HLS transcode session.
    ///
    /// Calls [`wdmm_ffmpeg::open_job`] which starts an in-process libav
    /// transcode/remux job writing `index.m3u8` + `.ts` segments into a
    /// temp directory. The [`wdmm_ffmpeg::TranscodePreset`] drives codec
    /// selection (`remux_first`, `video_codec`, `audio_codec`, …). For
    /// 「单文件直通」 byte-Range playback use [`register_file`](Self::register_file).
    ///
    /// **Enforcement:** One transcode per `(device_id, input_url)` pair. If a
    /// session already exists for this combo, the old session is killed and
    /// replaced by the new one. Pass a unique `device_id` per player to avoid
    /// killing other devices' transcodes.
    ///
    /// `input_url` accepts a local file path or an `http://127.0.0.1:{port}/…`
    /// loopback proxy stream URL (for encrypted source media).
    ///
    /// `ttl_secs` — optional time-to-live in seconds; `None` = no expiration.
    ///
    /// `allowed_peers` — optional IP whitelist; `None` = any peer allowed.
    ///
    /// Returns an [`HlsRegistration`] with a `playlist_template` ready to serve.
    pub async fn register_hls(&self, opts: HlsSessionOpts) -> ProxyResult<HlsRegistration> {
        self.start_hls_session(opts, true).await
    }

    /// Start an HLS VOD session **without** waiting for the plan.
    ///
    /// This is the warm-up path of contract §3.4: the FFI returns a `warming`
    /// phase to the UI and only points the renderer at `index.m3u8` once
    /// [`hls_progress`](Self::hls_progress) reports `playlist_ready`.
    /// [`register_hls`](Self::register_hls) keeps blocking until the plan (and
    /// thus the playlist) can be served.
    pub async fn register_hls_nowait(&self, opts: HlsSessionOpts) -> ProxyResult<HlsRegistration> {
        self.start_hls_session(opts, false).await
    }

    /// Shared implementation of [`register_hls`](Self::register_hls) and
    /// [`register_hls_nowait`](Self::register_hls_nowait).
    async fn start_hls_session(
        &self,
        opts: HlsSessionOpts,
        _wait_plan: bool,
    ) -> ProxyResult<HlsRegistration> {
        self.ensure_server().await?;
        let HlsSessionOpts {
            device_id,
            input_url,
            mut preset,
            ttl_secs,
            allowed_peers,
            cache,
        } = opts;
        self.cache.configure(cache.root.clone(), cache.budget_bytes);
        let mut cache_key = cache.cache_key();
        if cache_key.is_ephemeral() {
            // No etag/mtime identity ⇒ never reuse another session's segments.
            cache_key.ident = new_token();
        }

        // One session per device+media pair (the old run is replaced).
        let old_token = {
            let g = self.shared.lock().unwrap();
            g.device_media_map
                .get(&(device_id.clone(), input_url.clone()))
                .cloned()
        };
        if let Some(token) = old_token {
            info!("replacing existing HLS session for device={device_id}, input={input_url}");
            self.unregister_hls(&token);
        }

        let cache_dir = self.cache.prepare(&cache_key)?;
        let seg_secs = preset.hls_segment_secs.max(1) as f64;

        // 1. A plan persisted by an earlier session is reused only when its
        //    on-disk version still matches: `StoredPlan::into_plan` hard-rejects
        //    anything else, and `TranscodeCache::load_plan` drops the directory,
        //    so a plan written by older code can never steer a cast again.
        let mut plan = self
            .cache
            .load_plan::<StoredPlan>(&cache_key, &cache_dir)
            .and_then(StoredPlan::into_plan);
        let mut plan_error: Option<String> = None;
        if plan.is_some() {
            self.cache.note_hit();
        } else {
            self.cache.note_miss();
            // A plan we could not reuse may describe a different grid than the
            // segments still sitting in this directory (it was written by older
            // code, or a previous first cast died mid-grid). Opening the session
            // on top of them would serve old boundaries as the new plan's
            // content, so the media goes; the plan is rewritten below.
            let dropped = TranscodeCache::purge_media(&cache_dir);
            if dropped > 0 {
                wdmm_ffmpeg::diag::log(format!(
                    "[proxy] hls cache reset: dropped {dropped} file(s) planned by another grid"
                ));
            }
            // 2. No usable cached plan: ask libav once which grid this cast will
            //    really produce. Copying the video keeps the source keyframes as
            //    segment boundaries (cheap, and exact because the muxer cuts on
            //    every keyframe). When the container offers no usable keyframe
            //    index, the cast is re-encoded instead: the encoder writes exactly
            //    one IDR per segment, so `exact_plan` describes the output too.
            //    There is deliberately no nominal middle ground — a grid the
            //    muxer does not honour is what made seeking land on the wrong
            //    content.
            plan = match wdmm_ffmpeg::source_plan(&input_url, &preset) {
                Ok(Some(source)) => {
                    info!(
                        "source plan: alignment={} segments={} keyframes={} video_copied={} gop_ms={:?}",
                        alignment_label(source.plan.alignment),
                        source.plan.segment_count(),
                        source.keyframe_count,
                        source.video_copied,
                        source.plan.gop_ms,
                    );
                    Some(source.plan)
                }
                Ok(None) => None,
                Err(error) => {
                    warn!("source plan (copy video) failed ({error}); re-encoding the cast");
                    plan_error = Some(error.to_string());
                    None
                }
            };
            if plan.is_none() {
                info!("source plan unavailable; re-encoding the cast onto an exact grid");
                preset.video_copy = false;
                plan = match wdmm_ffmpeg::source_plan(&input_url, &preset) {
                    Ok(Some(source)) => {
                        info!(
                            "source plan: alignment={} segments={} re-encoded step={:.1}ms",
                            alignment_label(source.plan.alignment),
                            source.plan.segment_count(),
                            source.plan.seg_secs * 1000.0,
                        );
                        Some(source.plan)
                    }
                    Ok(None) => None,
                    Err(error) => {
                        warn!("source plan (re-encode) failed ({error})");
                        plan_error = Some(error.to_string());
                        None
                    }
                };
            }
        }
        // A cast without a grid cannot seek, so it is refused instead of served
        // with a nominal playlist that the muxer will not follow.
        let Some(plan) = plan else {
            let detail = plan_error
                .unwrap_or_else(|| "the source reports no usable duration/frame rate".to_string());
            let message = format!("cannot plan HLS segments for {input_url}: {detail}");
            wdmm_ffmpeg::diag::log(format!("[proxy] hls plan refused: {message}"));
            return Err(if wdmm_ffmpeg::is_available() {
                ProxyError::Cache(message)
            } else {
                // Without the ffmpeg feature there is nothing to plan with; keep
                // the typed error the FFI layer turns into a user-facing message.
                ProxyError::Ffmpeg(message)
            });
        };

        // The stored preset must describe the same path the plan was built for:
        // a keyframe grid only holds while the video is copied, an exact grid only
        // while it is re-encoded.
        preset.video_copy = plan.alignment == PlanAlignment::Keyframe;
        wdmm_ffmpeg::diag::log(format!(
            "[proxy] hls plan: alignment={} segments={} seg_secs={:.2} duration_ms={} \
             gop_ms={:?} video_copy={} cache={}",
            alignment_label(plan.alignment),
            plan.segment_count(),
            plan.seg_secs,
            plan.duration_ms,
            plan.gop_ms,
            preset.video_copy,
            cache_dir.display(),
        ));
        self.cache
            .save_plan(&cache_key, &cache_dir, &StoredPlan::from_plan(&plan))?;

        let token = new_token();
        let playlist_template = format!("/h/{token}/index.m3u8");
        // Both alignments ask the muxer to cut on *every* keyframe (1 ms
        // threshold): the copy path inherits the source keyframes, the re-encode
        // path writes one IDR per segment. Either way the cut sequence does not
        // depend on where a run is anchored, so a seek-restart started at any
        // planned boundary reproduces the same grid.
        let hls_time_override_ms = Some(KEYFRAME_CUT_HLS_TIME_MS);
        {
            let mut g = self.shared.lock().unwrap();
            g.hls_sessions.insert(
                token.clone(),
                HlsSession {
                    plan: Arc::new(plan),
                    cache: Arc::clone(&self.cache),
                    cache_key,
                    cache_dir,
                    preset,
                    input_url: input_url.clone(),
                    seg_secs,
                    run: None,
                    job_error: None,
                    max_requested_k: 0,
                    hits: 0,
                    last_reopen: None,
                    ended: false,
                    anchor_k: 0,
                    hls_time_override_ms,
                },
            );
            g.device_media_map
                .insert((device_id, input_url), token.clone());
            let now = SystemTime::now();
            let expires_at = ttl_secs.map(|ttl| now + Duration::from_secs(ttl));
            g.token_meta.insert(
                token.clone(),
                TokenMeta {
                    created_at: now,
                    expires_at,
                    allowed_peers,
                },
            );
            arm_idle_timer(&mut g, Arc::clone(&self.shared));
        }

        // Pre-buffer the head of the stream: the playlist is served immediately
        // (it is derived from the plan, not from the muxer), so the renderer's
        // first segment request should not pay for a cold container open.
        if let Err(error) = self.spawn_run(&token, 0).await {
            let mut g = self.shared.lock().unwrap();
            if let Some(session) = g.hls_sessions.get_mut(&token) {
                session.job_error = Some(error.to_string());
            }
        }
        Ok(HlsRegistration {
            token,
            playlist_template,
        })
    }

    /// Drive the calibration warm-up until a plan exists (or the timeout hits).
    /// Start (or restart) the session's transcode run so it produces from
    /// `first_k` on.
    ///
    /// `planned` = the plan exists and `first_k`'s planned start offset is used;
    /// otherwise the run is a calibration warm-up starting at the media head.
    async fn spawn_run(&self, token: &str, first_k: u64) -> ProxyResult<()> {
        spawn_run_shared(&self.shared, token, first_k).await
    }

    /// Revoke an HLS session token across the registry.
    ///
    /// The transcode job is cancelled; the cached segments stay on disk
    /// (contract §3.5: `unregister_hls` does not touch the cache).
    pub fn unregister_hls(&self, token: &str) {
        let mut g = self.shared.lock().unwrap();
        g.device_media_map.retain(|_, t| t != token);
        g.hls_sessions.remove(token);
        g.token_meta.remove(token);
    }

    /// Live progress of an HLS session (§2 / §7.3), or `None` for unknown
    /// tokens.
    ///
    /// Sync: reads the cache directory + the job's shared telemetry, and caches
    /// the job result once the thread has finished (calling `take_result` twice
    /// would lose the error text).
    pub fn hls_progress(&self, token: &str) -> Option<HlsProgress> {
        let mut g = self.shared.lock().unwrap();
        let session = g.hls_sessions.get_mut(token)?;
        if let Some(run) = session.run.as_mut() {
            if !run.job.is_running() {
                run.ended = true;
                // The encoder is gone: free the gate so the next cast can start.
                run.permit = None;
                if let Some(Err(e)) = run.job.take_result() {
                    session.job_error = Some(e.to_string());
                }
            }
        }
        Some(derive_hls_progress(session))
    }

    /// Gracefully stop the proxy. The handle is still usable; the server will
    /// restart automatically on the next `register_*`.
    pub fn stop(&self) {
        let mut g = self.shared.lock().unwrap();
        // Cancel idle timer.
        g.idle_cancel_tx = None;
        // Signal axum shutdown.
        if let Some(tx) = g.shutdown_tx.take() {
            let _ = tx.send(());
        }
        g.addr = None;
    }

    // ── Internal ──────────────────────────────────────────────────────────────

    async fn insert_entry(
        &self,
        entry: Entry,
        url_name: Option<String>,
        ttl_secs: Option<u64>,
        allowed_peers: Option<Vec<IpAddr>>,
    ) -> ProxyResult<Registration> {
        self.ensure_server().await?;
        let token = new_token();
        // A subtitle keeps the token as the lookup key and appends a readable
        // filename: DLNA renderers derive the subtitle language and label from the
        // URL *filename*, never from DIDL attributes (Kodi:
        // `CUtil::GetExternalStreamDetailsFromFilename`).
        let path_template = match url_name.as_deref() {
            Some(name) if !name.is_empty() => format!("/s/{token}/{}", url_safe_name(name)),
            _ => format!("/s/{token}"),
        };
        {
            let mut g = self.shared.lock().unwrap();
            g.entries.insert(token.clone(), entry);
            let now = SystemTime::now();
            let expires_at = ttl_secs.map(|t| now + Duration::from_secs(t));
            g.token_meta.insert(
                token.clone(),
                TokenMeta {
                    created_at: now,
                    expires_at,
                    allowed_peers,
                },
            );
            arm_idle_timer(&mut g, Arc::clone(&self.shared));
        }
        Ok(Registration {
            token,
            path_template,
        })
    }

    async fn ensure_server(&self) -> ProxyResult<()> {
        if self.shared.lock().unwrap().addr.is_some() {
            return Ok(());
        }

        let bind_addr = self.bind_mode.to_bind_addr(self.port);
        let listener = TcpListener::bind(&bind_addr).await?;
        let addr = listener.local_addr()?;
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let shared_for_router = Arc::clone(&self.shared);
        let app = Router::new()
            .route("/s/:token", get(serve))
            .route("/s/:token/:name", get(serve_named))
            .route("/h/:token/*seg", get(serve_hls))
            .with_state(shared_for_router);

        let shared_for_cleanup = Arc::clone(&self.shared);
        tokio::spawn(async move {
            if let Err(e) = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            {
                error!("wdmm-proxy: server error: {e}");
            }
            let mut g = shared_for_cleanup.lock().unwrap();
            g.addr = None;
            g.shutdown_tx = None;
        });

        info!("wdmm-proxy: listening on {addr}");
        let mut g = self.shared.lock().unwrap();
        g.addr = Some(addr);
        g.shutdown_tx = Some(shutdown_tx);
        // Start idle timer now so a server that never gets a request still stops.
        arm_idle_timer(&mut g, Arc::clone(&self.shared));
        Ok(())
    }
}

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        // Only the last handle may stop the server. Every clone is a live user
        // of the same listener: FFI DLNA sessions keep one for the session's
        // lifetime, and temporary clones (e.g. `entry.proxy.clone()` in a
        // subtitle switch) must not take the listener down while a renderer is
        // streaming — doing so closes the port, and the renderer's next HTTP
        // request gets `connection refused` mid-playback.
        if Arc::strong_count(&self.handles) == 1 {
            self.stop();
        }
    }
}

// ── Idle timer ────────────────────────────────────────────────────────────────

fn arm_idle_timer(g: &mut Shared, shared: Arc<Mutex<Shared>>) {
    // Drop the old cancel sender — previous timer task will receive Err and exit.
    let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
    g.idle_cancel_tx = Some(cancel_tx);
    let timeout = g.idle_timeout;

    tokio::spawn(async move {
        tokio::select! {
            _ = tokio::time::sleep(timeout) => {
                let mut g = shared.lock().unwrap();
                g.idle_cancel_tx = None;
                if let Some(tx) = g.shutdown_tx.take() {
                    let _ = tx.send(());
                }
                g.addr = None;
                info!("wdmm-proxy: idle timeout, server stopped");
            }
            _ = cancel_rx => {
                // Cancelled by a new touch or explicit stop().
            }
        }
    });
}

// ── Token ─────────────────────────────────────────────────────────────────────

fn new_token() -> String {
    format!("{:032x}", rand::random::<u128>())
}

// ── Content-Type ─────────────────────────────────────────────────────────────

/// Guess MIME type from filename extension.
/// Table matches Dart `LocalStreamBridge._contentTypeFor`.
pub fn content_type_for(name: Option<&str>) -> &'static str {
    let ext = name
        .and_then(|n| n.rfind('.').map(|i| &n[i + 1..]))
        .unwrap_or("");
    match ext.to_ascii_lowercase().as_str() {
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "mov" => "video/quicktime",
        "ts" => "video/mp2t",
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "wav" => "audio/wav",
        "ogg" | "opus" => "audio/ogg",
        // Subtitle sidecars and generated WebVTT, served to the DLNA renderer.
        // Kodi matches the MIME *subtype* against its subtitle formats.
        "srt" => "text/srt",
        "ass" => "text/ass",
        "ssa" => "text/ssa",
        "vtt" => "text/vtt",
        "sub" => "text/sub",
        _ => "application/octet-stream",
    }
}

// ── Range parsing ─────────────────────────────────────────────────────────────

/// Parse a single-range `Range` header against `size` bytes.
///
/// Returns `None` when the range is unsatisfiable (→ 416). Supports
/// `a-b`, `a-` (open-ended), and `-n` (suffix). Multi-range → unsatisfiable.
pub fn resolve_range(header: &str, size: u64) -> Option<(u64, u64)> {
    let spec = header.trim().strip_prefix("bytes=")?;
    if spec.contains(',') || size == 0 {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (start, end) = if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        if n == 0 {
            return None;
        }
        (size.saturating_sub(n), size - 1)
    } else {
        let start: u64 = a.parse().ok()?;
        let end = if b.is_empty() {
            size - 1
        } else {
            b.parse::<u64>().ok()?.min(size - 1)
        };
        (start, end)
    };
    (start <= end && start < size).then_some((start, end))
}

// ── axum handler ─────────────────────────────────────────────────────────────

async fn serve(
    State(shared): State<Arc<Mutex<Shared>>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    AxumPath(token): AxumPath<String>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    serve_token(token, shared, peer, headers).await
}

/// `/s/:token/:name` — the same resource with a renderer-readable filename.
///
/// DLNA renderers derive a subtitle's language and label from the URL filename
/// (Kodi: `CUtil::GetExternalStreamDetailsFromFilename`), so subtitles are
/// registered under a path that ends with `<stem>[.<lang>][.<title>].<ext>`.
/// The name is decoration only: the token stays the lookup key.
async fn serve_named(
    State(shared): State<Arc<Mutex<Shared>>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    AxumPath((token, _name)): AxumPath<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    serve_token(token, shared, peer, headers).await
}

async fn serve_token(
    token: String,
    shared: Arc<Mutex<Shared>>,
    peer: SocketAddr,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    // Extract what we need and reset the idle timer — all under one lock.
    enum Src {
        File(PathBuf),
        Cb(Arc<dyn Fn(i64, i64) -> BoxStream + Send + Sync>),
    }
    let (src, size, content_type) = {
        let mut g = shared.lock().unwrap();

        // Validate token expiration
        if let Some(meta) = g.token_meta.get(&token) {
            if let Some(expires_at) = meta.expires_at {
                if SystemTime::now() > expires_at {
                    // Token expired — cleanup and return 401
                    g.entries.remove(&token);
                    g.token_meta.remove(&token);
                    return Err(StatusCode::UNAUTHORIZED);
                }
            }
            if let Some(ref whitelist) = meta.allowed_peers {
                if !whitelist.contains(&peer.ip()) {
                    return Err(StatusCode::FORBIDDEN);
                }
            }
        }

        let entry = g.entries.get(&token).ok_or(StatusCode::NOT_FOUND)?;
        let src = match &entry.source {
            BackSource::LocalFile(p) => Src::File(p.clone()),
            BackSource::DriverCallback(f) => Src::Cb(Arc::clone(f)),
        };
        let ct = content_type_for(entry.name.as_deref());
        let sz = entry.size;
        arm_idle_timer(&mut g, Arc::clone(&shared));
        (src, sz, ct)
    };

    let range_hdr = headers
        .get(header::RANGE)
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_owned());

    let builder = Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ACCEPT_RANGES, "bytes");

    match src {
        Src::File(path) => serve_file(builder, path, size, range_hdr.as_deref()).await,
        Src::Cb(cb) => serve_callback(builder, cb, size, range_hdr.as_deref()),
    }
}

async fn serve_file(
    builder: axum::http::response::Builder,
    path: PathBuf,
    registered_size: u64,
    range_hdr: Option<&str>,
) -> Result<Response, StatusCode> {
    let mut file = tokio::fs::File::open(&path)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let file_size = tokio::fs::metadata(&path)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .len();
    let size = if registered_size > 0 {
        registered_size
    } else {
        file_size
    };

    match range_hdr.map(|h| resolve_range(h, size)) {
        None => builder
            .status(StatusCode::OK)
            .header(header::CONTENT_LENGTH, size)
            .body(Body::from_stream(ReaderStream::new(file)))
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR),
        Some(None) => builder
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(header::CONTENT_RANGE, format!("bytes */{size}"))
            .body(Body::empty())
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR),
        Some(Some((start, end))) => {
            file.seek(std::io::SeekFrom::Start(start))
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            let len = end - start + 1;
            builder
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_LENGTH, len)
                .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"))
                .body(Body::from_stream(ReaderStream::new(file.take(len))))
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

fn serve_callback(
    builder: axum::http::response::Builder,
    cb: Arc<dyn Fn(i64, i64) -> BoxStream + Send + Sync>,
    size: u64,
    range_hdr: Option<&str>,
) -> Result<Response, StatusCode> {
    let known = size > 0;

    if let Some(range_str) = range_hdr {
        if known {
            return match resolve_range(range_str, size) {
                None => builder
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header(header::CONTENT_RANGE, format!("bytes */{size}"))
                    .body(Body::empty())
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR),
                Some((start, end)) => {
                    let len = end - start + 1;
                    builder
                        .status(StatusCode::PARTIAL_CONTENT)
                        .header(header::CONTENT_LENGTH, len)
                        .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"))
                        .body(Body::from_stream(cb(start as i64, end as i64)))
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
                }
            };
        }
    }

    // No range (or unknown size) — stream everything from 0.
    let end_arg: i64 = if known { size as i64 - 1 } else { -1 };
    let mut resp = builder.status(StatusCode::OK);
    if known {
        resp = resp.header(header::CONTENT_LENGTH, size);
    }
    resp.body(Body::from_stream(cb(0, end_arg)))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

// ── HLS handler ───────────────────────────────────────────────────────────────

async fn serve_hls(
    State(shared): State<Arc<Mutex<Shared>>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    AxumPath((token, seg)): AxumPath<(String, String)>,
) -> Result<Response, (StatusCode, String)> {
    // Reject path traversal attempts — HLS segment names are always flat.
    if seg.contains('/') || seg.contains("..") {
        return Err((StatusCode::BAD_REQUEST, "invalid HLS path".to_string()));
    }

    {
        let mut g = shared.lock().unwrap();
        if !g.hls_sessions.contains_key(&token) {
            return Err((StatusCode::NOT_FOUND, "unknown HLS session".to_string()));
        }
        if let Some(meta) = g.token_meta.get(&token) {
            if let Some(expires_at) = meta.expires_at {
                if SystemTime::now() > expires_at {
                    g.hls_sessions.remove(&token);
                    g.token_meta.remove(&token);
                    return Err((StatusCode::UNAUTHORIZED, "HLS token expired".to_string()));
                }
            }
            if let Some(ref whitelist) = meta.allowed_peers {
                if !whitelist.contains(&peer.ip()) {
                    return Err((StatusCode::FORBIDDEN, "peer not allowed".to_string()));
                }
            }
        }
        arm_idle_timer(&mut g, Arc::clone(&shared));
    }

    // The playlist is synthesized from the segment plan — never read from disk
    // (contract §3.3: exactly one implementation).
    if seg.ends_with(".m3u8") {
        let plan = {
            let g = shared.lock().unwrap();
            match g.hls_sessions.get(&token) {
                Some(session) => Arc::clone(&session.plan),
                None => return Err((StatusCode::NOT_FOUND, "unknown HLS session".to_string())),
            }
        };
        let playlist = render_playlist(&playlist_segments(&plan));
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, PLAYLIST_CONTENT_TYPE)
            .header(header::CONTENT_LENGTH, playlist.len())
            .body(Body::from(playlist))
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
    }

    let Some(k) = parse_segment_index(&seg) else {
        return Err((StatusCode::NOT_FOUND, format!("unknown HLS path {seg}")));
    };

    let path = ensure_segment(&shared, &token, k).await?;
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, format!("segment read failed: {e}")))?;
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, SEGMENT_CONTENT_TYPE)
        .header(header::CONTENT_LENGTH, bytes.len())
        .body(Body::from(bytes))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// Wait for (or trigger) segment `k`, returning its path once it is on disk.
async fn ensure_segment(
    shared: &Arc<Mutex<Shared>>,
    token: &str,
    k: u64,
) -> Result<PathBuf, (StatusCode, String)> {
    let deadline = tokio::time::Instant::now() + HLS_SEGMENT_WAIT;
    loop {
        enum Step {
            Serve(PathBuf),
            Wait,
            Resume,
            /// The request jumped outside the running job's coverage (a seek):
            /// cancel that job and transcode from the requested point.
            Cancel,
            Start(u64),
            Sleep(Duration),
            Ended,
        }

        let step = {
            let mut g = shared.lock().unwrap();
            let Some(session) = g.hls_sessions.get_mut(token) else {
                return Err((StatusCode::NOT_FOUND, "unknown HLS session".to_string()));
            };
            session.max_requested_k = session.max_requested_k.max(k);
            let plan_len = session.plan.segment_count() as u64;
            let path = TranscodeCache::segment_path(&session.cache_dir, k);
            match TranscodeCache::segment_ready(&session.cache_dir, k) {
                SegmentLookup::Ready => {
                    session.hits = session.hits.saturating_add(1);
                    Step::Serve(path)
                }
                _ if k >= plan_len => Step::Ended,
                _ => {
                    // The directory scan covers the whole cache, but the running
                    // job is anchored at `anchor_k`: the previous run's segments
                    // are still on disk and sit *behind* it. Clamp the front to
                    // the anchor (exactly like `derive_hls_progress` does) —
                    // without this every request for the fresh anchor was judged
                    // "outside the run" and cancelled the job that had just been
                    // started for it, so a seek could never produce a segment.
                    let front = front_segment(
                        scan_segments(&session.cache_dir).0,
                        session.anchor_k,
                        plan_len,
                    );
                    // The running job covers everything from its anchor up to the
                    // ready front plus the throttle window, so a request in there
                    // must wait for it instead of restarting the job.
                    let in_window = session.run.is_some()
                        && k >= session.anchor_k
                        && k <= front.saturating_add(AHEAD_SEGMENTS);
                    if in_window {
                        let paused_without_slot = session
                            .run
                            .as_ref()
                            .is_some_and(|run| run.job.is_paused() && run.permit.is_none());
                        if paused_without_slot {
                            // The throttle gave the slot away (see
                            // `park_run_shared`); take it back before resuming.
                            Step::Resume
                        } else {
                            if let Some(run) = session.run.as_ref() {
                                if run.job.is_paused() {
                                    run.job.resume();
                                }
                            }
                            Step::Wait
                        }
                    } else if session.ended && k >= session.anchor_k && k <= front {
                        Step::Ended
                    } else if session.run.is_some() {
                        // A seek landed outside the coverage of the current job.
                        // Do not park it "for later" and do not keep it paused:
                        // its decoder, its input and its gate slot are all
                        // useless now, and the renderer is waiting on the new
                        // position. Interrupting is also far cheaper than
                        // waiting for a paused job to be resumed and to reach
                        // the target segment from the old anchor.
                        Step::Cancel
                    } else {
                        match session.last_reopen {
                            Some(last) if last.elapsed() < HLS_REOPEN_INTERVAL => {
                                Step::Sleep(HLS_REOPEN_INTERVAL - last.elapsed())
                            }
                            _ => Step::Start(k),
                        }
                    }
                }
            }
        };

        match step {
            Step::Serve(path) => {
                park_run_shared(shared, token, k);
                return Ok(path);
            }
            Step::Wait => {
                if tokio::time::Instant::now() >= deadline {
                    let (state, error) = {
                        let g = shared.lock().unwrap();
                        match g.hls_sessions.get(token) {
                            Some(session) => (
                                if session.job_error.is_some() {
                                    "error"
                                } else {
                                    "active"
                                },
                                session
                                    .job_error
                                    .clone()
                                    .unwrap_or_else(|| "segment not ready".to_string()),
                            ),
                            None => ("error", "session vanished".to_string()),
                        }
                    };
                    let status = if state == "error" {
                        StatusCode::INTERNAL_SERVER_ERROR
                    } else {
                        StatusCode::GATEWAY_TIMEOUT
                    };
                    return Err((
                        status,
                        format!(
                            "segment {k} not ready after {}s: {error}",
                            HLS_SEGMENT_WAIT.as_secs()
                        ),
                    ));
                }
                tokio::time::sleep(HLS_POLL_INTERVAL).await;
            }
            Step::Resume => {
                let gate = { Arc::clone(&shared.lock().unwrap().gate) };
                let permit = gate
                    .acquire_async(JOB_ACQUIRE_TIMEOUT)
                    .await
                    .map_err(|error| {
                        (
                            StatusCode::GATEWAY_TIMEOUT,
                            format!("segment {k} not ready: {error}"),
                        )
                    })?;
                let mut g = shared.lock().unwrap();
                let Some(session) = g.hls_sessions.get_mut(token) else {
                    return Err((StatusCode::NOT_FOUND, "unknown HLS session".to_string()));
                };
                if let Some(run) = session.run.as_mut() {
                    run.permit = Some(permit);
                    run.job.resume();
                    info!("HLS run resumed at segment {k}");
                }
            }
            Step::Cancel => {
                drop_previous_run(shared, token);
                info!(
                    token,
                    k, "HLS run interrupted by a seek outside its coverage"
                );
                wdmm_ffmpeg::diag::log(format!(
                    "[proxy] hls run interrupted: seek to segment {k} is outside the run"
                ));
                // Loop again: the session now has no run, so the next pass either
                // waits out the reopen throttle or starts a fresh job at `k`.
            }
            Step::Start(from_k) => {
                // Near-neighbour merge: one reopen covers `from_k..=from_k+2`.
                let from_k = *merge_range_start(from_k).start();
                spawn_run_shared(shared, token, from_k)
                    .await
                    .map_err(|error| match error {
                        ProxyError::Busy(message) => (
                            StatusCode::GATEWAY_TIMEOUT,
                            format!("segment {k} not ready: {message}"),
                        ),
                        other => (StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
                    })?;
            }
            Step::Sleep(wait) => tokio::time::sleep(wait).await,
            Step::Ended => {
                return Err((
                    StatusCode::NOT_FOUND,
                    format!("segment {k} is past the end of the media"),
                ));
            }
        }
    }
}

/// First index of a merged reopen range: asking for k also warms k+1/k+2.
///
/// A reopen costs a full container open, so one run covers the neighbour the
/// renderer asks for next; the job is throttled by [AHEAD_SEGMENTS] rather than
/// being cancelled per request.
fn merge_range_start(k: u64) -> std::ops::RangeInclusive<u64> {
    k..=k.saturating_add(2)
}

// ── Tests ─────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    // ── resolve_range ─────────────────────────────────────────────────────────

    #[test]
    fn range_exact() {
        assert_eq!(resolve_range("bytes=0-9", 100), Some((0, 9)));
    }
    #[test]
    fn range_open_end() {
        assert_eq!(resolve_range("bytes=90-", 100), Some((90, 99)));
    }
    #[test]
    fn range_suffix() {
        assert_eq!(resolve_range("bytes=-10", 100), Some((90, 99)));
    }
    #[test]
    fn range_clamped() {
        assert_eq!(resolve_range("bytes=50-500", 100), Some((50, 99)));
    }
    #[test]
    fn range_out_of_bounds() {
        assert_eq!(resolve_range("bytes=100-", 100), None);
    }
    #[test]
    fn range_inverted() {
        assert_eq!(resolve_range("bytes=9-0", 100), None);
    }
    #[test]
    fn range_multirange_rejected() {
        assert_eq!(resolve_range("bytes=0-1,5-6", 100), None);
    }
    #[test]
    fn range_junk() {
        assert_eq!(resolve_range("bytes=abc", 100), None);
    }
    #[test]
    fn range_zero_size() {
        assert_eq!(resolve_range("bytes=0-0", 0), None);
    }

    // ── content_type_for ──────────────────────────────────────────────────────

    #[test]
    fn content_type_video() {
        assert_eq!(content_type_for(Some("movie.mp4")), "video/mp4");
        assert_eq!(content_type_for(Some("clip.MKV")), "video/x-matroska");
    }
    #[test]
    fn content_type_audio() {
        assert_eq!(content_type_for(Some("track.mp3")), "audio/mpeg");
        assert_eq!(content_type_for(Some("lossless.flac")), "audio/flac");
    }
    #[test]
    fn content_type_subtitles() {
        // The subtitle endpoint must serve the real text MIME: Kodi's UPnP
        // renderer matches the subtype against its supported subtitle formats.
        assert_eq!(content_type_for(Some("movie.zh.srt")), "text/srt");
        assert_eq!(content_type_for(Some("movie.ass")), "text/ass");
        assert_eq!(content_type_for(Some("movie.SSA")), "text/ssa");
        assert_eq!(content_type_for(Some("movie.vtt")), "text/vtt");
        assert_eq!(content_type_for(Some("movie.sub")), "text/sub");
    }
    #[test]
    fn content_type_fallback() {
        assert_eq!(
            content_type_for(Some("data.bin")),
            "application/octet-stream"
        );
        assert_eq!(content_type_for(None), "application/octet-stream");
    }

    // ── full integration: local file ──────────────────────────────────────────

    #[tokio::test]
    async fn proxy_file_full_and_range() {
        let path = std::env::temp_dir().join(format!("wdmm-proxy-test-{}.bin", std::process::id()));
        let data: Vec<u8> = (0..=255u8).collect();
        std::fs::write(&path, &data).unwrap();

        let proxy = ProxyHandle::new(BindMode::Loopback, 0);
        let reg = proxy
            .register_file(&path, Some("test.bin".into()), None, None)
            .await
            .unwrap();
        let addr = proxy.addr().unwrap();
        let url = format!("http://{addr}{}", reg.path_template);
        let client = reqwest::Client::new();

        // Full GET
        let r = client.get(&url).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.bytes().await.unwrap().as_ref(), data.as_slice());

        // Range GET
        let r = client
            .get(&url)
            .header("Range", "bytes=10-19")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 206);
        assert_eq!(r.headers()["content-range"], "bytes 10-19/256");
        assert_eq!(r.bytes().await.unwrap().as_ref(), &data[10..20]);

        // Unsatisfiable range → 416
        let r = client
            .get(&url)
            .header("Range", "bytes=300-")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 416);

        // Unknown token → 404
        let r = client
            .get(format!("http://{addr}/s/deadbeef00deadbeef00deadbeef00aa"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404);

        // Unregister → 404
        proxy.unregister(&reg.token);
        assert_eq!(client.get(&url).send().await.unwrap().status(), 404);

        proxy.stop();
        let _ = std::fs::remove_file(&path);
    }

    /// Regression: a DLNA subtitle switch clones the session's handle and lets
    /// the clone go out of scope. That must not stop the shared listener, or the
    /// renderer's next request is refused mid-playback (the cast appears to
    /// freeze and then times out).
    #[tokio::test]
    async fn dropping_a_clone_keeps_the_server_listening() {
        let path = std::env::temp_dir().join(format!(
            "wdmm-proxy-clone-test-{}.bin",
            std::process::id()
        ));
        std::fs::write(&path, b"hello clone").unwrap();

        let proxy = ProxyHandle::new(BindMode::Loopback, 0);
        let reg = proxy
            .register_file(&path, Some("clone.bin".into()), None, None)
            .await
            .unwrap();
        let addr = proxy.addr().unwrap();
        let url = format!("http://{addr}{}", reg.path_template);

        let clone = proxy.clone();
        drop(clone);
        assert!(
            proxy.addr().is_some(),
            "dropping a clone must not stop the shared server"
        );

        let client = reqwest::Client::new();
        let r = client.get(&url).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.bytes().await.unwrap().as_ref(), b"hello clone");

        proxy.stop();
        let _ = std::fs::remove_file(&path);
    }

    // ── subtitle lane (named registration) ────────────────────────────────────

    #[test]
    fn url_safe_name_keeps_language_tokens() {
        assert_eq!(
            url_safe_name("ep01.jpn.Full subtitles.ass"),
            "ep01.jpn.Full_subtitles.ass"
        );
        assert_eq!(url_safe_name("中文 标题.ass"), "_____.ass");
        assert_eq!(url_safe_name(&"a".repeat(400)).len(), 120);
    }

    #[tokio::test]
    async fn named_registration_exposes_the_filename_in_the_url() {
        use bytes::Bytes;
        use std::io;

        let data: Arc<Vec<u8>> = Arc::new(b"[Script Info]\n".to_vec());
        let proxy = ProxyHandle::new(BindMode::Loopback, 0);
        let d = Arc::clone(&data);
        let reg = proxy
            .register_driver_named(
                data.len() as u64,
                Some("ep01.jpn.ass".into()),
                Some("ep01.jpn.ass".into()),
                None,
                None,
                move |start, end| {
                    let chunk = if end < 0 {
                        d[start as usize..].to_vec()
                    } else {
                        d[start as usize..=(end as usize).min(d.len() - 1)].to_vec()
                    };
                    let stream = futures::stream::once(async move {
                        Ok::<Bytes, io::Error>(Bytes::from(chunk))
                    });
                    Box::pin(stream) as BoxStream
                },
            )
            .await
            .unwrap();

        assert!(
            reg.path_template.starts_with("/s/") && reg.path_template.ends_with("/ep01.jpn.ass"),
            "unexpected path template {}",
            reg.path_template
        );

        let addr = proxy.addr().unwrap();
        let url = format!("http://{addr}{}", reg.path_template);
        let r = reqwest::Client::new().get(&url).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(
            r.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/ass"),
            "the renderer picks the subtitle demuxer from the extension"
        );
        assert_eq!(r.bytes().await.unwrap().as_ref(), data.as_slice());
    }

    // ── integration: driver callback ──────────────────────────────────────────

    #[tokio::test]
    async fn proxy_driver_callback_range() {
        use bytes::Bytes;
        use std::io;

        let data: Arc<Vec<u8>> = Arc::new((0u8..=255).collect());

        let proxy = ProxyHandle::new(BindMode::Loopback, 0);
        let d = Arc::clone(&data);
        let reg = proxy
            .register_driver(
                256,
                Some("audio.mp3".into()),
                None,
                None,
                move |start, end| {
                    let chunk = if end < 0 {
                        d[start as usize..].to_vec()
                    } else {
                        d[start as usize..=(end as usize).min(d.len() - 1)].to_vec()
                    };
                    // Build a single-item stream: emit the chunk then end.
                    let stream =
                        futures::stream::once(
                            async move { Ok::<Bytes, io::Error>(Bytes::from(chunk)) },
                        );
                    Box::pin(stream) as BoxStream
                },
            )
            .await
            .unwrap();

        let addr = proxy.addr().unwrap();
        let url = format!("http://{addr}{}", reg.path_template);
        let client = reqwest::Client::new();

        // Full
        let r = client.get(&url).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.bytes().await.unwrap().as_ref(), data.as_slice());

        // Range
        let r = client
            .get(&url)
            .header("Range", "bytes=0-3")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 206);
        assert_eq!(r.bytes().await.unwrap().as_ref(), &[0u8, 1, 2, 3]);

        proxy.stop();
    }

    // ── Registration token is not a URL ───────────────────────────────────────

    #[tokio::test]
    async fn registration_path_only_no_url() {
        let proxy = ProxyHandle::new(BindMode::Loopback, 0);
        let path = std::env::temp_dir().join(format!("wdmm-path-test-{}.bin", std::process::id()));
        std::fs::write(&path, b"hello").unwrap();
        let reg = proxy.register_file(&path, None, None, None).await.unwrap();
        // path_template must start with "/" and not contain "http"
        assert!(
            reg.path_template.starts_with('/'),
            "path_template must start with /"
        );
        assert!(
            !reg.path_template.contains("http"),
            "path_template must not embed scheme"
        );
        proxy.stop();
        let _ = std::fs::remove_file(&path);
    }

    // ── HLS skeleton ─────────────────────────────────────────────────────────
    //
    // These drive a real in-process libav transcode. Set `WDMM_HLS_TEST_MEDIA`
    // to a small media file and build with the `wdmm-ffmpeg/ffmpeg` feature to
    // exercise them; without either prerequisite they skip instead of failing.

    /// Media file for the HLS integration tests.
    fn hls_test_media() -> Option<PathBuf> {
        let path = PathBuf::from(std::env::var_os("WDMM_HLS_TEST_MEDIA")?);
        path.is_file().then_some(path)
    }

    // ── hls_progress derived state (§2) ──────────────────────────────────────
    //
    // Exercises the filesystem + telemetry derivation directly: no media and no
    // libav needed, so these run on every host (including Windows).

    fn unique_seg_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wdmm-hls-progress-{}-{}-{}",
            std::process::id(),
            name,
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
    // ── HLS progress derivation (§3.5/§4) ────────────────────────────────────
    //
    // Pure struct + filesystem state: no media and no libav needed, so these run
    // on every host (including Windows).

    /// A keyframe plan of `count` segments of `seg_secs` seconds each.
    fn test_plan(seg_secs: f64, count: usize, duration_ms: i64) -> Arc<SegmentPlan> {
        let seg_ms = (seg_secs * 1000.0) as i64;
        let entries = (0..count)
            .map(|i| wdmm_ffmpeg::SegEntry {
                start_ms: i as i64 * seg_ms,
                duration_ms: seg_ms,
            })
            .collect();
        Arc::new(SegmentPlan {
            duration_ms,
            seg_secs,
            alignment: PlanAlignment::Keyframe,
            gop_ms: Some(2_000),
            entries,
        })
    }

    fn test_session(dir: PathBuf, plan: Arc<SegmentPlan>) -> HlsSession {
        HlsSession {
            plan,
            cache: Arc::new(TranscodeCache::new(std::env::temp_dir(), 1 << 30)),
            cache_key: CacheKey::new("account", "/remote/movie.mkv", "balanced", 10, "etag-1"),
            cache_dir: dir,
            preset: wdmm_ffmpeg::preset::preset_h264(),
            input_url: "/nonexistent/movie.mkv".to_string(),
            seg_secs: 4.0,
            run: None,
            job_error: None,
            max_requested_k: 0,
            hits: 0,
            last_reopen: None,
            ended: false,
            anchor_k: 0,
            hls_time_override_ms: Some(KEYFRAME_CUT_HLS_TIME_MS),
        }
    }

    #[test]
    fn front_segment_never_reports_behind_the_anchor() {
        // Scan result from an earlier run that is still on disk.
        assert_eq!(front_segment(Some(20), 900, 1000), 900);
        // Nothing on disk yet: the anchor is the front.
        assert_eq!(front_segment(None, 5, 10), 5);
        // Normal forward progress is reported as-is.
        assert_eq!(front_segment(Some(9), 0, 10), 9);
        // Never past the last plan entry.
        assert_eq!(front_segment(Some(12), 0, 10), 9);
    }

    #[test]
    fn hls_progress_reports_plan_and_ready_front() {
        let dir = unique_seg_dir("with-plan");
        std::fs::write(dir.join("seg0.ts"), b"x").unwrap();
        std::fs::write(dir.join("seg1.ts"), b"x").unwrap();
        std::fs::write(dir.join("seg2.ts.part"), b"x").unwrap();
        // seg2 has a file but the muxer has not closed it yet, so it must not
        // count towards the ready front.
        std::fs::write(dir.join("seg2.ts"), b"x").unwrap();
        std::fs::write(
            dir.join("index.m3u8"),
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:4.000,\nseg0.ts\n#EXTINF:4.000,\nseg1.ts\n",
        )
        .unwrap();

        let mut session = test_session(dir.clone(), test_plan(4.0, 10, 40_000));
        session.hits = 3;
        session.max_requested_k = 1;

        let progress = derive_hls_progress(&session);
        assert!(progress.playlist_ready);
        assert_eq!(progress.segments, 2, "only non-empty seg<k>.ts count");
        assert_eq!(progress.ready_k, Some(1));
        assert_eq!(progress.segment_count, 10);
        assert_eq!(progress.seg_secs, 4.0);
        assert_eq!(progress.plan_duration_ms, Some(40_000));
        assert_eq!(progress.alignment, Some("keyframe"));
        assert_eq!(progress.max_requested_k, Some(1));
        assert_eq!(progress.cache_hits, 3);
        assert_eq!(
            progress.content_ms, 8_000,
            "planned media time to the front"
        );
        assert_eq!(progress.run_state, "idle");
        assert!(!progress.ended);
        assert!(
            progress.drift_ms.is_none(),
            "no running job ⇒ no drift measurement"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hls_progress_reports_error_and_ended() {
        let dir = unique_seg_dir("error");
        let mut session = test_session(dir.clone(), test_plan(4.0, 2, 8_000));
        session.job_error = Some("avcodec_open2(video encoder libx264) failed".to_string());
        session.ended = true;

        let progress = derive_hls_progress(&session);
        assert_eq!(progress.run_state, "error");
        assert!(progress.ended);
        assert_eq!(
            progress.error.as_deref(),
            Some("avcodec_open2(video encoder libx264) failed")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn hls_progress_unknown_token_is_none() {
        let proxy = ProxyHandle::new(BindMode::Loopback, 0);
        assert!(proxy
            .hls_progress("deadbeef00deadbeef00deadbeef00aa")
            .is_none());
    }

    // ── HLS session integration (real libav) ─────────────────────────────────

    fn hls_test_opts(path: &Path, device_id: &str) -> HlsSessionOpts {
        HlsSessionOpts {
            device_id: device_id.to_string(),
            input_url: path.to_string_lossy().into_owned(),
            preset: wdmm_ffmpeg::preset::preset_h264(),
            ttl_secs: None,
            allowed_peers: None,
            cache: HlsCacheOptions {
                root: Some(unique_seg_dir("cache")),
                budget_bytes: Some(256 * 1024 * 1024),
                account_id: "test-account".to_string(),
                remote_path: path.to_string_lossy().into_owned(),
                preset_id: "balanced".to_string(),
                size: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
                ident: format!("test-{}", std::process::id()),
            },
        }
    }

    #[tokio::test]
    async fn proxy_hls_register_reports_missing_ffmpeg() {
        // Contract for a build without the `ffmpeg` feature: registration fails
        // with a typed error the FFI layer can surface, never a panic.
        if wdmm_ffmpeg::is_available() {
            return;
        }
        let proxy = ProxyHandle::new(BindMode::Loopback, 0);
        let err = proxy
            .register_hls(hls_test_opts(
                Path::new("/nonexistent/media.mp4"),
                "device-1",
            ))
            .await
            .unwrap_err();
        assert!(matches!(err, ProxyError::Ffmpeg(_)), "got {err:?}");
        proxy.stop();
    }

    #[tokio::test]
    async fn proxy_hls_register_serves_vod_playlist_and_segment() {
        let Some(path) = hls_test_media() else { return };

        let proxy = ProxyHandle::new(BindMode::Loopback, 0);
        let reg = proxy
            .register_hls(hls_test_opts(&path, "device-1"))
            .await
            .unwrap();
        assert!(!reg.token.is_empty());
        assert!(reg.playlist_template.starts_with("/h/"));
        assert!(reg.playlist_template.ends_with("/index.m3u8"));

        // `register_hls` returns only once a playable plan exists, so the
        // renderer's immediate playlist fetch must succeed (this is what Kodi broke on).
        let addr = proxy.addr().unwrap();
        let client = reqwest::Client::new();
        let url = format!("http://{addr}{}", reg.playlist_template);
        let r = client.get(&url).send().await.unwrap();
        assert_eq!(r.status(), 200, "playlist must be servable at once");
        assert_eq!(
            r.headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some(PLAYLIST_CONTENT_TYPE)
        );
        let playlist = r.text().await.unwrap();
        assert!(playlist.starts_with("#EXTM3U"));
        assert!(playlist.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
        assert!(playlist.contains("#EXT-X-TARGETDURATION:"));
        assert!(playlist.contains("#EXT-X-MEDIA-SEQUENCE:0"));
        assert!(playlist.contains("seg0.ts"));
        assert!(playlist.trim_end().ends_with("#EXT-X-ENDLIST"));

        // Pulling the first segment must produce it on demand.
        let seg_url = url.replace("index.m3u8", "seg0.ts");
        let seg = client.get(&seg_url).send().await.unwrap();
        assert_eq!(seg.status(), 200, "seg0.ts must be produced on demand");
        assert_eq!(
            seg.headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some(SEGMENT_CONTENT_TYPE)
        );
        assert!(!seg.bytes().await.unwrap().is_empty());

        proxy.unregister_hls(&reg.token);
        let gone = client.get(&url).send().await.unwrap();
        assert_eq!(gone.status(), 404, "unregistered token must 404");
        proxy.stop();
    }

    #[tokio::test]
    async fn proxy_hls_pull_reopens_for_later_segments() {
        let Some(path) = hls_test_media() else { return };

        let proxy = ProxyHandle::new(BindMode::Loopback, 0);
        let reg = proxy
            .register_hls(hls_test_opts(&path, "device-1"))
            .await
            .unwrap();
        let addr = proxy.addr().unwrap();
        let client = reqwest::Client::new();
        let url = format!("http://{addr}{}", reg.playlist_template);
        let playlist = client.get(&url).send().await.unwrap().text().await.unwrap();
        let segments: Vec<&str> = playlist
            .lines()
            .filter(|line| line.ends_with(".ts"))
            .collect();
        assert!(
            segments.len() > 3,
            "test media must yield several segments, got {segments:?}"
        );

        // Walking forward forces the scheduler to keep the job running and, once
        // the front is outrun, to reopen at the requested index.
        let target = segments[3];
        let seg_url = format!("http://{addr}/h/{}/{target}", reg.token);
        let seg = client.get(&seg_url).send().await.unwrap();
        assert_eq!(seg.status(), 200, "{target} must be produced on demand");
        assert!(!seg.bytes().await.unwrap().is_empty());

        let progress = proxy.hls_progress(&reg.token).unwrap();
        assert!(
            progress.ready_k.unwrap_or(0) >= 3,
            "front must have reached segment 3, got {:?}",
            progress.ready_k
        );
        proxy.unregister_hls(&reg.token);
        proxy.stop();
    }

    #[tokio::test]
    async fn proxy_hls_one_transcode_per_device_media() {
        let Some(path) = hls_test_media() else { return };

        let proxy = ProxyHandle::new(BindMode::Loopback, 0);

        // First registration
        let reg1 = proxy
            .register_hls(hls_test_opts(&path, "device-1"))
            .await
            .unwrap();
        assert_eq!(proxy.shared.lock().unwrap().hls_sessions.len(), 1);

        // Second registration for same device+media kills the first
        let reg2 = proxy
            .register_hls(hls_test_opts(&path, "device-1"))
            .await
            .unwrap();
        assert_eq!(proxy.shared.lock().unwrap().hls_sessions.len(), 1);
        assert_ne!(reg1.token, reg2.token, "new token must replace old");

        // Different device can transcode the same file. Only one transcode may
        // run at a time (the global job gate), so the second session is created
        // with `register_hls_nowait`: session setup must not depend on a free
        // encoder slot, otherwise a second cast fails with `transcoding busy`
        // instead of queueing behind the first one.
        let _reg3 = proxy
            .register_hls_nowait(hls_test_opts(&path, "device-2"))
            .await
            .unwrap();
        assert_eq!(proxy.shared.lock().unwrap().hls_sessions.len(), 2);

        proxy.stop();
    }

    /// The user-visible regression: HLS cast, then drag the progress bar to the
    /// end of the file. Every segment index in the synthesized playlist must be
    /// fetchable, including ones far past the produced front, and the playlist
    /// itself must not change afterwards (VOD grids are immutable once read).
    #[tokio::test]
    async fn proxy_hls_seek_to_late_segment_is_served() {
        let Some(path) = hls_test_media() else { return };

        let proxy = ProxyHandle::new(BindMode::Loopback, 0);
        let reg = proxy
            .register_hls(hls_test_opts(&path, "device-1"))
            .await
            .unwrap();
        let addr = proxy.addr().unwrap();
        let client = reqwest::Client::new();
        let url = format!("http://{addr}{}", reg.playlist_template);
        let playlist = client.get(&url).send().await.unwrap().text().await.unwrap();

        // 1. The plan covers the whole file instead of collapsing to one entry
        //    (the keyframe-index bug produced a single 24-minute segment).
        let progress = proxy.hls_progress(&reg.token).unwrap();
        assert!(
            progress.segment_count >= 3,
            "plan must be split into segments, got {}",
            progress.segment_count
        );
        // A source with a usable keyframe index is planned up front from the
        // container (`keyframe`); only an unusable index falls back to measured
        // warm-up (`calibrated`/`approx`). All three must cover the whole file.
        let alignment = progress.alignment.unwrap_or("none");
        assert!(
            alignment == "keyframe" || alignment == "calibrated" || alignment == "approx",
            "unexpected alignment {alignment}"
        );

        // 2. The declared EXTINF total matches the probe'd duration, so the
        //    renderer's progress bar spans the real length of the file.
        let total_ms: f64 = playlist
            .lines()
            .filter_map(|line| line.strip_prefix("#EXTINF:"))
            .filter_map(|value| value.trim_end_matches(',').parse::<f64>().ok())
            .map(|secs| secs * 1000.0)
            .sum();
        let plan_duration = progress.plan_duration_ms.unwrap_or(0) as f64;
        assert!(plan_duration > 0.0, "plan must report a duration");
        assert!(
            (total_ms - plan_duration).abs() <= plan_duration * 0.02,
            "playlist declares {total_ms}ms but the media is {plan_duration}ms"
        );

        // 3. Seeking to the tail must produce that segment on demand.
        let indices: Vec<u64> = playlist
            .lines()
            .filter_map(|line| line.strip_prefix("seg"))
            .filter_map(|name| name.strip_suffix(".ts"))
            .filter_map(|index| index.parse::<u64>().ok())
            .collect();
        let last = *indices.last().expect("playlist must list segments");
        assert!(last >= 2, "expected several segments, got {indices:?}");
        let seg_url = format!("http://{addr}/h/{}/seg{last}.ts", reg.token);
        let seg = client.get(&seg_url).send().await.unwrap();
        assert_eq!(seg.status(), 200, "seg{last}.ts must be produced on demand");
        assert!(!seg.bytes().await.unwrap().is_empty());

        // 4. Serving a far segment must not rewrite the playlist (no drift).
        let after = client.get(&url).send().await.unwrap().text().await.unwrap();
        assert_eq!(
            playlist, after,
            "a VOD playlist must not change while playing"
        );

        proxy.unregister_hls(&reg.token);
        proxy.stop();
    }

    /// Regression: dragging the progress bar left the cast stuck on "idle".
    ///
    /// The window check compared the request with a directory scan that also
    /// sees the *previous* run's segments, so right after a seek the ready front
    /// sat far below the new anchor. Every request then looked "outside the run"
    /// and cancelled the job that had just been started for it: the on-device
    /// diag log showed `hls run start at segment N` / `hls run interrupted: seek
    /// to segment N` repeating once per second, each job dying with `packets=0`,
    /// until the renderer timed out and dropped the cast. A renderer asks for the
    /// same segment several times around a seek, so repeated requests must share
    /// one run instead of restarting it.
    #[tokio::test]
    async fn proxy_hls_repeated_seek_request_keeps_the_run() {
        let Some(path) = hls_test_media() else { return };

        let proxy = ProxyHandle::new(BindMode::Loopback, 0);
        let reg = proxy
            .register_hls(hls_test_opts(&path, "device-1"))
            .await
            .unwrap();
        let addr = proxy.addr().unwrap();
        let client = reqwest::Client::new();
        let playlist = client
            .get(format!("http://{addr}{}", reg.playlist_template))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        // Warm the head first: that is what leaves the previous run's segments on
        // disk, which the old window check then measured the seek against.
        let head = client
            .get(format!("http://{addr}/h/{}/seg0.ts", reg.token))
            .send()
            .await
            .unwrap();
        assert_eq!(head.status(), 200, "seg0.ts must be served");

        let last = playlist
            .lines()
            .filter_map(|line| line.strip_prefix("seg"))
            .filter_map(|name| name.strip_suffix(".ts"))
            .filter_map(|index| index.parse::<u64>().ok())
            .next_back()
            .expect("playlist must list segments");
        let target = last.saturating_sub(1);
        let seg_url = format!("http://{addr}/h/{}/seg{target}.ts", reg.token);

        // Two requests for the same seek target, exactly like a renderer probing
        // a new position: neither may cancel the other's run.
        let (first, second) =
            tokio::join!(client.get(&seg_url).send(), client.get(&seg_url).send());
        let first = first.unwrap();
        let second = second.unwrap();
        assert_eq!(first.status(), 200, "first seek request must be served");
        assert_eq!(
            second.status(),
            200,
            "a repeat request for the same segment must be served"
        );
        assert!(!first.bytes().await.unwrap().is_empty());
        assert!(!second.bytes().await.unwrap().is_empty());

        // The seek must be anchored at the target rather than at the head: the
        // ready front has to be at least the requested segment. A short tail is
        // copied fast enough that the run may already be done (that is fine),
        // so only a *failed* job or an unanchored front is a regression.
        let progress = proxy.hls_progress(&reg.token).unwrap();
        assert!(
            progress.error.is_none(),
            "the seek run must not fail: {:?}",
            progress.error
        );
        assert!(
            progress.ready_k.unwrap_or(0) >= target,
            "the run must report the seek target as produced, got {:?} (state {:?})",
            progress.ready_k,
            progress.run_state
        );

        proxy.unregister_hls(&reg.token);
        proxy.stop();
    }
}
