//! `wdmm-ffmpeg` — Vendored FFmpeg bindings.
//!
//! Provides:
//! - [`probe`] — read container/codec metadata via `ffprobe`.
//! - [`open_job`] — start an HLS transcode job writing `index.m3u8` + segments
//!   into a token-keyed directory; returns a [`JobHandle`] whose [`Drop`]
//!   kills the process and cleans the segment directory.
//!
//! ## Round 11 skeleton
//! All functions are stubs returning placeholder results. The vendored
//! prebuilt library paths are wired in `build.rs` but not yet downloaded — see
//! the TODO there. The proxy exercises the session lifecycle (register / seek
//! / unregister) without a real FFmpeg process; `open_job` only creates the
//! segment directory so the proxy skeleton can serve from it.
//!
//! ## Mode model (brief §3)
//! - [`HlsMode::DirectCopy`] — remux into HLS segments with `-c copy`. Default
//!   for codec-compatible media.
//! - [`HlsMode::Compat`] — transcode to a compat codec profile (H.264 + AAC
//!   by default). Seek is implemented by killing the job and restarting with
//!   `-ss`; Range is never applied to transcode stdout.
//!
//! Subtitles: embedded streams and same-basename sidecars; burn-in off by
//! default (see [`SubtitleMode`]). No HDR tone mapping.
//!
//! ## License
//! This crate links FFmpeg, licensed under LGPL v2.1+ (optionally GPL v2+ when
//! certain components are enabled). See `docs/FFMPEG-ATTRIBUTION.md` for the
//! full third-party attribution note. WDMM is distributed under AGPL-3.0.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use thiserror::Error;

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

// ── Probe ─────────────────────────────────────────────────────────────────────

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
    /// Duration in seconds (`ffprobe format.duration`).
    pub duration_secs: f64,
    pub video: Vec<VideoStream>,
    pub audio: Vec<AudioStream>,
    pub subtitles: Vec<SubtitleStream>,
    /// Detected container format name (e.g. `"matroska"`, `"mov,mp4…"`).
    pub format_name: String,
}

/// Probe a local file via `ffprobe` (JSON format).
///
/// # Stub
/// Always returns `Err(FfmpegError::Unavailable)` until vendored binaries are
/// linked. See `build.rs` TODO.
pub fn probe(_path: &Path) -> FfmpegResult<ProbeResult> {
    // TODO(round-12+): invoke vendored ffprobe, parse JSON output.
    Err(FfmpegError::Unavailable(
        "vendored ffprobe not yet linked — see build.rs TODO".into(),
    ))
}

// ── HLS transcode job ─────────────────────────────────────────────────────────

/// How the media should be delivered as HLS.
#[derive(Debug, Clone)]
pub enum HlsMode {
    /// Remux into HLS segments without re-encoding (`-c copy`). Default.
    /// Only works when the renderer is compatible with the source codec.
    DirectCopy,
    /// Transcode to a compat codec profile (H.264 + AAC by default).
    /// Seek is implemented by killing the job and restarting with `-ss`.
    Compat {
        /// Override video codec (default `"libx264"`).
        video_codec: Option<String>,
        /// Override audio codec (default `"aac"`).
        audio_codec: Option<String>,
    },
}

/// Subtitle handling options. Burn-in is **off by default**.
#[derive(Debug, Clone)]
pub enum SubtitleMode {
    /// Ignore subtitles (default). No HDR tone mapping is applied.
    Off,
    /// Burn-in a specific embedded stream index (0-based).
    /// Only meaningful in [`HlsMode::Compat`]; silently ignored for DirectCopy.
    BurnIn { stream_index: u32 },
    /// Burn-in from a sidecar file (e.g. `.srt`).
    /// Silently ignored for DirectCopy.
    SidecarBurnIn { path: PathBuf },
}

/// Options for opening a new HLS transcode job.
#[derive(Debug)]
pub struct JobOpts {
    /// Input file path.
    pub input: PathBuf,
    /// Transcode mode.
    pub mode: HlsMode,
    /// Subtitle mode (default: [`SubtitleMode::Off`]).
    pub subtitles: SubtitleMode,
    /// Start offset in seconds (for seek). `0.0` = from the beginning.
    pub start_offset_secs: f64,
    /// Target HLS segment duration in seconds (brief: ~6).
    pub segment_duration_secs: u32,
}

impl Default for JobOpts {
    fn default() -> Self {
        Self {
            input: PathBuf::new(),
            mode: HlsMode::DirectCopy,
            subtitles: SubtitleMode::Off,
            start_offset_secs: 0.0,
            segment_duration_secs: 6,
        }
    }
}

/// Handle to an active (or completed) HLS transcode job.
///
/// The job writes an `index.m3u8` playlist and numbered segment files
/// (`0.ts`, `1.ts`, …) into [`segment_dir`](JobHandle::segment_dir). The proxy
/// reads from that directory and deletes it when the session is dropped.
#[derive(Debug)]
pub struct JobHandle {
    /// Unique session id (internal; the proxy assigns its own route token).
    pub segment_dir: PathBuf,
    // OS process handle — `None` in stub mode.
    _proc: Option<tokio::process::Child>,
}

impl JobHandle {
    /// Directory where the job writes `index.m3u8` + segments.
    pub fn segment_dir(&self) -> &Path {
        &self.segment_dir
    }

    /// Kill the transcoding process. No-op if already exited (or in stub mode).
    pub fn kill(&mut self) {
        // TODO(round-12+): kill vendored ffmpeg process.
        if let Some(ref mut child) = self._proc {
            let _ = child.start_kill();
        }
    }
}

impl Drop for JobHandle {
    fn drop(&mut self) {
        self.kill();
        // Best-effort cleanup of the segment directory.
        if self.segment_dir.exists() {
            let _ = std::fs::remove_dir_all(&self.segment_dir);
        }
    }
}

/// Open a new HLS transcode job.
///
/// Creates a temporary directory for segments, then (in a real build) spawns
/// the vendored `ffmpeg` process writing to that directory.
///
/// # Stub
/// Currently only creates the segment directory; no actual process is spawned.
/// Returns `Ok(JobHandle)` so the proxy skeleton can exercise the session path
/// without FFmpeg.
pub fn open_job(opts: JobOpts) -> FfmpegResult<JobHandle> {
    let id = format!("{:016x}", rand::random::<u64>());
    let segment_dir = std::env::temp_dir().join(format!("wdmm-hls-{id}"));
    std::fs::create_dir_all(&segment_dir)?;

    // TODO(round-12+): spawn vendored ffmpeg with:
    //   -ss <start_offset_secs> -i <input> [codec flags per mode]
    //   -hls_time <segment_duration_secs> -hls_list_size 0 -f hls
    //   <segment_dir>/index.m3u8
    // Store the process handle in JobHandle._proc.

    tracing::debug!(
        "wdmm-ffmpeg: stub open_job mode={:?} start={:?}s seg_dir={}",
        opts.mode,
        opts.start_offset_secs,
        segment_dir.display()
    );

    Ok(JobHandle {
        segment_dir,
        _proc: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_stub_unavailable() {
        let r = probe(Path::new("/dev/null"));
        assert!(matches!(r, Err(FfmpegError::Unavailable(_))));
    }

    #[test]
    fn open_job_creates_segment_dir() {
        let h = open_job(JobOpts {
            input: PathBuf::from("/tmp/x.mp4"),
            mode: HlsMode::DirectCopy,
            subtitles: SubtitleMode::Off,
            start_offset_secs: 0.0,
            segment_duration_secs: 6,
        })
        .unwrap();
        assert!(h.segment_dir().exists(), "segment dir should exist");
        assert!(h.segment_dir().to_string_lossy().contains("wdmm-hls-"));
        // Drop cleans it up.
        let dir = h.segment_dir().to_path_buf();
        drop(h);
        assert!(!dir.exists(), "segment dir should be removed on drop");
    }

    #[test]
    fn jobopts_default_is_direct_copy_off() {
        let o = JobOpts::default();
        assert!(matches!(o.mode, HlsMode::DirectCopy));
        assert!(matches!(o.subtitles, SubtitleMode::Off));
        assert_eq!(o.segment_duration_secs, 6);
    }

    #[test]
    fn hls_mode_clone() {
        // Seek restarts clone the mode from the session — ensure Clone works.
        let m = HlsMode::Compat {
            video_codec: Some("libx264".into()),
            audio_codec: None,
        };
        let _m2 = m.clone();
    }
}
