#![cfg(feature = "ffmpeg")]

//! Host throughput baseline for the HLS transcode pipeline.
//!
//! This is a measurement harness, not a correctness test: every scenario drives
//! the real libav pipeline through `wdmm_ffmpeg::open_job`, waits until a fixed
//! amount of *content* has been produced, cancels, and reports the wall clock
//! it took. The result is printed as a Markdown table (visible with
//! `--nocapture`), so a before/after build can be compared on the same host.
//!
//! Scenarios:
//!
//! - `(a) H.264 re-encode` — 8-bit H.264 source, `remux_first=false`.
//! - `(b) HEVC re-encode` — HEVC source (`WDMM_TP_MEDIA`), `remux_first=false`.
//! - `(c) copy` — the primary source through the `remux_first` copy path.
//!
//! Env vars:
//!
//! - `WDMM_TP_MEDIA` — primary source (fallback: `E:\test\video\1.m4v`).
//! - `WDMM_TP_MEDIA2` — optional alternate source used for scenario (a) when
//!   `WDMM_TP_MEDIA_H264` is unset.
//! - `WDMM_TP_MEDIA_H264` — 8-bit H.264 source for scenario (a); if neither it
//!   nor `WDMM_TP_MEDIA2` names an existing file, scenario (a) is skipped.
//! - `WDMM_TP_CONTENT_SECS` — content seconds to produce per scenario
//!   (default 40, clamped below the source duration).
//! - `WDMM_TP_REPEAT` — runs per scenario (default 3, clamp 1..=10); the
//!   fastest run is reported and the spread is noted, so a single noisy run
//!   does not become the baseline.
//! - `WDMM_TP_PRESET_JSON` — partial JSON object merged over the `balanced`
//!   preset, e.g. `{"video_max_bitrate": 2000000, "hls_segment_secs": 2}`.
//!
//! Run it with a host FFmpeg 7.1 shared build:
//!
//! ```text
//! Set-Location 'E:\Webdav_Media_Manager\rust\wdmm-core'
//! $env:FF='E:\test\video\_env\ffmpeg-n7.1.5-12-g1fdbca85aa-win64-gpl-shared-7.1'
//! $env:WDMM_FFMPEG_LIB_DIR="$env:FF\lib"
//! $env:RUSTFLAGS="-L native=$env:FF\lib"
//! $env:PATH="$env:FF\bin;"+$env:PATH
//! $env:WDMM_TP_MEDIA='E:\test\video\1.m4v'
//! $env:WDMM_TP_MEDIA_H264='E:\test\video\2.mp4'
//! cargo test -p wdmm-ffmpeg --features ffmpeg --test transcode_throughput -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Missing media is reported as a `skipped` line, never a panic: the test only
//! fails when a source that *is* present cannot be measured.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use wdmm_core::models::TranscodePreset;
use wdmm_ffmpeg::preset::preset_h264;
use wdmm_ffmpeg::JobOpts;

/// Media used when `WDMM_TP_MEDIA` is unset.
const DEFAULT_MEDIA: &str = r"E:\test\video\1.m4v";
/// Default amount of produced content per scenario.
const DEFAULT_CONTENT_SECS: f64 = 40.0;
/// Default number of runs per scenario (the fastest run is reported).
const DEFAULT_REPEATS: usize = 3;
const MAX_REPEATS: usize = 10;
const POLL_INTERVAL: Duration = Duration::from_millis(20);
/// A scenario that produces no target content in this long is a failure.
const SCENARIO_TIMEOUT: Duration = Duration::from_secs(600);
/// Cancellation flushes the encoders and writes the trailer; allow for it.
const CANCEL_TIMEOUT: Duration = Duration::from_secs(120);

#[test]
#[ignore = "needs WDMM_TP_MEDIA (or E:\\test\\video\\1.m4v) and a host FFmpeg 7.1 build"]
fn host_transcode_throughput_baseline() {
    if !wdmm_ffmpeg::is_available() {
        eprintln!("skip: wdmm-ffmpeg has no usable host libav (WDMM_FFMPEG_LIB_DIR)");
        return;
    }
    let Some(primary) = primary_media() else {
        eprintln!("skip: no input media; set WDMM_TP_MEDIA or provide {DEFAULT_MEDIA}");
        return;
    };

    let base = preset_from_env();
    let content_secs = content_target_secs();
    let repeats = repeat_count();

    let specs = [
        Spec {
            label: "(a) H.264 re-encode",
            tag: "h264",
            media_env: "WDMM_TP_MEDIA_H264 or WDMM_TP_MEDIA2",
            media: env_media("WDMM_TP_MEDIA_H264").or_else(|| env_media("WDMM_TP_MEDIA2")),
            expected: Some(ExpectedCodec::H264),
            copy_path: false,
        },
        Spec {
            label: "(b) HEVC re-encode",
            tag: "hevc",
            media_env: "WDMM_TP_MEDIA",
            media: Some(primary.clone()),
            expected: Some(ExpectedCodec::Hevc),
            copy_path: false,
        },
        Spec {
            label: "(c) copy path",
            tag: "copy",
            media_env: "WDMM_TP_MEDIA",
            media: Some(primary),
            expected: None,
            copy_path: true,
        },
    ];

    let mut samples: Vec<Sample> = Vec::new();
    let mut skips: Vec<(String, String)> = Vec::new();
    let mut failures: Vec<String> = Vec::new();

    for spec in &specs {
        let Some(media) = spec.media.as_deref() else {
            skips.push((
                spec.label.to_string(),
                format!("{} is not set (or the file does not exist)", spec.media_env),
            ));
            continue;
        };
        let source = match probe_source(media) {
            Ok(source) => source,
            Err(err) => {
                failures.push(format!("{}: {err}", spec.label));
                continue;
            }
        };
        if let Some(expected) = spec.expected {
            let wanted = expected.name();
            if source.video_codec != wanted {
                skips.push((
                    spec.label.to_string(),
                    format!(
                        "{} carries '{}', not {wanted}",
                        media.display(),
                        source.video_codec
                    ),
                ));
                continue;
            }
        }
        if spec.copy_path {
            if let Some(reason) = copy_path_refusal(&source) {
                skips.push((spec.label.to_string(), reason));
                continue;
            }
        }
        let target_ms = content_target_ms(&source, content_secs);
        if target_ms < 500 {
            skips.push((
                spec.label.to_string(),
                format!("source is too short to measure ({target_ms} ms)"),
            ));
            continue;
        }
        let preset = scenario_preset(&base, spec.copy_path);
        let mut runs: Vec<Sample> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        for run in 1..=repeats {
            match run_scenario(
                spec.label, spec.tag, run, media, &source, &preset, target_ms,
            ) {
                Ok(sample) => runs.push(sample),
                Err(err) => {
                    failures.push(format!("{} (run {run}): {err}", spec.label));
                    errors.push(err);
                }
            }
        }
        if runs.is_empty() {
            continue;
        }
        // Report the fastest run: a run that lost time to host load is not a
        // property of the pipeline.
        runs.sort_by_key(|sample| sample.wall_ms);
        let mut best = runs.remove(0);
        if spec.copy_path && best.mode != "remux" {
            failures.push(format!(
                "{}: expected a stream copy but the job ran mode '{}'\n{}",
                spec.label,
                best.mode,
                diagnostics()
            ));
        }
        if repeats > 1 {
            let mut walls: Vec<i64> = runs.iter().map(|sample| sample.wall_ms).collect();
            walls.push(best.wall_ms);
            walls.sort_unstable();
            let spread = if best.wall_ms > 0 {
                (walls[walls.len() - 1] - walls[0]) as f64 / best.wall_ms as f64 * 100.0
            } else {
                0.0
            };
            best.notes.push(format!(
                "best of {} runs; wall {} ms (spread {spread:.1}%)",
                walls.len(),
                walls
                    .iter()
                    .map(i64::to_string)
                    .collect::<Vec<_>>()
                    .join(" / ")
            ));
        }
        if !errors.is_empty() {
            best.notes.push(format!("{} run(s) failed", errors.len()));
        }
        samples.push(best);
    }

    print_report(&samples, &skips, &base, content_secs, repeats);

    assert!(
        failures.is_empty(),
        "throughput scenarios failed:\n{}",
        failures.join("\n\n")
    );
}

// ── Scenario definition ───────────────────────────────────────────────────────

struct Spec {
    label: &'static str,
    tag: &'static str,
    media_env: &'static str,
    media: Option<PathBuf>,
    expected: Option<ExpectedCodec>,
    /// Scenario (c) must exercise the copy path, not the encoder.
    copy_path: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExpectedCodec {
    H264,
    Hevc,
}

impl ExpectedCodec {
    fn name(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Hevc => "hevc",
        }
    }
}

/// Stream facts needed to describe a sample and decide the copy path.
#[derive(Clone)]
struct SourceInfo {
    duration_ms: i64,
    video_codec: String,
    audio_codec: Option<String>,
    width: u32,
    height: u32,
    fps: f64,
}

/// One measured scenario.
struct Sample {
    label: String,
    source: SourceInfo,
    media_name: String,
    mode: String,
    encoder: Option<String>,
    content_ms: i64,
    wall_ms: i64,
    segments: usize,
    first_seg_ms: Option<i64>,
    notes: Vec<String>,
}

// ── Measurement ──────────────────────────────────────────────────────────────

fn run_scenario(
    label: &str,
    tag: &str,
    run: usize,
    media: &Path,
    source: &SourceInfo,
    preset: &TranscodePreset,
    target_ms: i64,
) -> Result<Sample, String> {
    let out_dir = scenario_dir(&format!("{tag}-{run}"));
    let url = media.to_string_lossy().into_owned();
    let mut job = wdmm_ffmpeg::open_job(JobOpts {
        input_url: url,
        preset: preset.clone(),
        out_dir: Some(out_dir.clone()),
        hls_time_override_ms: Some(1),
        ..JobOpts::default()
    })
    .map_err(|err| format!("open_job failed: {err}\n{}", diagnostics()))?;

    let started = Instant::now();
    let mut first_seg_ms = None;
    let mut target_hit = false;
    let mut timed_out = false;

    loop {
        if first_seg_ms.is_none() && job.ready_segments() > 0 {
            first_seg_ms = Some(elapsed_ms(started));
        }
        if job.progress().content_ms >= target_ms {
            target_hit = true;
            break;
        }
        if !job.is_running() {
            break;
        }
        if started.elapsed() >= SCENARIO_TIMEOUT {
            timed_out = true;
            break;
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    // Snapshot before cancelling: the numbers describe "time to produce
    // `target_ms` of content", not the trailing flush.
    let wall_ms = elapsed_ms(started);
    let content_ms = job.progress().content_ms;
    let segments = job.ready_segments();
    let info = job.encoder_info();

    job.cancel();
    if !wait_until(CANCEL_TIMEOUT, || !job.is_running()) {
        drop(job);
        let _ = std::fs::remove_dir_all(&out_dir);
        return Err(format!(
            "the job did not stop within {} s after cancel",
            CANCEL_TIMEOUT.as_secs()
        ));
    }
    let finished = job.take_result();
    drop(job);
    let final_segments = count_segments(&out_dir);
    let _ = std::fs::remove_dir_all(&out_dir);

    if timed_out {
        return Err(format!(
            "produced only {content_ms} ms of content in {} s (target {target_ms} ms)",
            SCENARIO_TIMEOUT.as_secs()
        ));
    }
    match finished {
        Some(Ok(())) => {}
        Some(Err(err)) => return Err(format!("job failed: {err}\n{}", diagnostics())),
        None => return Err(format!("job ended without a result\n{}", diagnostics())),
    }
    if content_ms <= 0 || segments == 0 {
        return Err(format!(
            "job produced no segments (content_ms={content_ms})\n{}",
            diagnostics()
        ));
    }

    let mut notes = Vec::new();
    if !target_hit {
        notes.push("source ended before the content target".to_string());
    }
    if content_ms > target_ms + 2_000 {
        notes.push(format!(
            "ran {} ms past the target before the poll cancelled it",
            content_ms - target_ms
        ));
    }
    if final_segments > segments {
        notes.push(format!(
            "cancel finalization added {} segment(s)",
            final_segments - segments
        ));
    }

    Ok(Sample {
        label: label.to_string(),
        source: source.clone(),
        media_name: media
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| media.display().to_string()),
        mode: info.mode,
        encoder: info.video,
        content_ms,
        wall_ms,
        segments,
        first_seg_ms,
        notes,
    })
}

fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if predicate() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn elapsed_ms(start: Instant) -> i64 {
    i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX)
}

fn scenario_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wdmm-throughput-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scenario directory");
    dir
}

fn count_segments(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".ts"))
                .count()
        })
        .unwrap_or(0)
}

fn diagnostics() -> String {
    wdmm_ffmpeg::diag::tail(80).join("\n")
}

// ── Source inspection ────────────────────────────────────────────────────────

fn probe_source(media: &Path) -> Result<SourceInfo, String> {
    let probed = wdmm_ffmpeg::probe(&media.to_string_lossy())
        .map_err(|err| format!("probe {}: {err}", media.display()))?;
    let video = probed
        .video
        .first()
        .ok_or_else(|| format!("{} has no video stream", media.display()))?;
    Ok(SourceInfo {
        duration_ms: (probed.duration_secs * 1000.0) as i64,
        video_codec: video.codec.clone(),
        audio_codec: probed.audio.first().map(|audio| audio.codec.clone()),
        width: video.width,
        height: video.height,
        fps: video.fps,
    })
}

/// Why the `remux_first` stage cannot stream-copy this source, if it cannot.
///
/// Mirrors `plan.rs::remux_plan`: MPEG-TS can carry H.264/HEVC/MPEG-2 video and
/// AAC/MP3/AC-3/E-AC-3 audio. Anything else falls back to a full re-encode.
fn copy_path_refusal(source: &SourceInfo) -> Option<String> {
    if !matches!(source.video_codec.as_str(), "h264" | "hevc" | "mpeg2video") {
        return Some(format!(
            "video codec '{}' cannot be copied into MPEG-TS",
            source.video_codec
        ));
    }
    match source.audio_codec.as_deref() {
        None | Some("aac" | "mp3" | "ac3" | "eac3") => None,
        Some(codec) => Some(format!(
            "audio codec '{codec}' cannot be copied into MPEG-TS (hybrid would re-encode it)"
        )),
    }
}

// ── Preset + media configuration ─────────────────────────────────────────────

fn primary_media() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("WDMM_TP_MEDIA").map(PathBuf::from) {
        return path.is_file().then_some(path);
    }
    let fallback = PathBuf::from(DEFAULT_MEDIA);
    fallback.is_file().then_some(fallback)
}

fn env_media(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_file())
}

fn content_target_secs() -> f64 {
    match std::env::var("WDMM_TP_CONTENT_SECS") {
        Ok(raw) => match raw.trim().parse::<f64>() {
            Ok(secs) if secs > 0.0 => secs,
            _ => panic!("WDMM_TP_CONTENT_SECS must be a positive number, got {raw:?}"),
        },
        Err(_) => DEFAULT_CONTENT_SECS,
    }
}

/// Runs per scenario; the fastest one is reported.
fn repeat_count() -> usize {
    match std::env::var("WDMM_TP_REPEAT") {
        Ok(raw) => match raw.trim().parse::<usize>() {
            Ok(count) if (1..=MAX_REPEATS).contains(&count) => count,
            _ => panic!("WDMM_TP_REPEAT must be an integer in 1..={MAX_REPEATS}, got {raw:?}"),
        },
        Err(_) => DEFAULT_REPEATS,
    }
}

/// The built-in h264 preset, optionally overridden by `WDMM_TP_PRESET_JSON`.
///
/// The JSON object is merged field-by-field over the balanced preset, so a run
/// can tune one knob (bitrate, segment length, fps cap) without restating all
/// sixteen fields.
fn preset_from_env() -> TranscodePreset {
    let preset = preset_h264();
    let Some(raw) = std::env::var_os("WDMM_TP_PRESET_JSON") else {
        return preset;
    };
    let raw = raw.to_string_lossy().into_owned();
    let overrides: serde_json::Value = serde_json::from_str(&raw)
        .unwrap_or_else(|err| panic!("WDMM_TP_PRESET_JSON is not valid JSON: {err}"));
    let mut merged = serde_json::to_value(&preset).expect("serialize TranscodePreset");
    let base = merged
        .as_object_mut()
        .expect("TranscodePreset serializes to an object");
    for (key, value) in overrides
        .as_object()
        .unwrap_or_else(|| panic!("WDMM_TP_PRESET_JSON must be a JSON object"))
    {
        base.insert(key.clone(), value.clone());
    }
    serde_json::from_value(merged)
        .unwrap_or_else(|err| panic!("WDMM_TP_PRESET_JSON does not fit TranscodePreset: {err}"))
}

/// Scenario presets differ only in whether the cheap copy stage runs first.
fn scenario_preset(base: &TranscodePreset, copy_first: bool) -> TranscodePreset {
    let mut preset = base.clone();
    preset.video_copy = copy_first;
    preset.audio_copy = copy_first;
    preset
}

fn content_target_ms(source: &SourceInfo, content_secs: f64) -> i64 {
    let requested = (content_secs * 1000.0).round() as i64;
    if source.duration_ms > 3000 {
        requested.min(source.duration_ms - 500)
    } else {
        requested
    }
}

// ── Report ───────────────────────────────────────────────────────────────────

fn print_report(
    samples: &[Sample],
    skips: &[(String, String)],
    preset: &TranscodePreset,
    content_secs: f64,
    repeats: usize,
) {
    let cpus = std::thread::available_parallelism().map_or(0, |n| n.get());
    println!();
    println!("## Host HLS transcode throughput (real libav)");
    println!();
    println!(
        "host: {} {} / {cpus} logical CPUs",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!("preset: {}", preset_summary(preset));
    println!("content target: {content_secs} s per scenario (or EOF); best of {repeats} run(s)");
    println!();
    println!(
        "| scenario | source | mode (encoder) | content s | wall s | speed | segments | first seg ms | avg seg ms |"
    );
    println!("|---|---|---|---:|---:|---:|---:|---:|---:|");
    for sample in samples {
        let content_s = sample.content_ms as f64 / 1000.0;
        let wall_s = sample.wall_ms as f64 / 1000.0;
        let speed = if sample.wall_ms > 0 {
            sample.content_ms as f64 / sample.wall_ms as f64
        } else {
            0.0
        };
        let avg_seg_ms = if sample.segments > 0 {
            sample.wall_ms as f64 / sample.segments as f64
        } else {
            0.0
        };
        let first_seg = sample
            .first_seg_ms
            .map_or_else(|| "-".to_string(), |ms| ms.to_string());
        let encoder = sample.encoder.as_deref().unwrap_or("?");
        println!(
            "| {} | {} | {} ({encoder}) | {content_s:.2} | {wall_s:.2} | {speed:.2}x | {} | {first_seg} | {avg_seg_ms:.0} |",
            sample.label, sample.media_name, sample.mode, sample.segments
        );
    }
    println!();
    for sample in samples {
        println!(
            "- {}: {}x{} {:.2} fps, {} -> {} on `{}`",
            sample.label,
            sample.source.width,
            sample.source.height,
            sample.source.fps,
            sample.source.video_codec,
            sample.mode,
            sample.media_name
        );
        for note in &sample.notes {
            println!("  - note: {note}");
        }
    }
    if !skips.is_empty() {
        println!();
        println!("Skipped scenarios:");
        for (label, reason) in skips {
            println!("- {label}: {reason}");
        }
    }
}

fn preset_summary(preset: &TranscodePreset) -> String {
    let dims = match (preset.video_max_w, preset.video_max_h) {
        (Some(width), Some(height)) => format!("{width}x{height}"),
        _ => "unbounded".to_string(),
    };
    let bitrate = preset
        .video_max_bitrate
        .map_or_else(|| "unset".to_string(), |bps| format!("{}k", bps / 1000));
    let fps_cap = preset
        .video_fps_cap
        .map_or_else(|| "none".to_string(), |fps| fps.to_string());
    let video_encoder = if preset.hw_video_encoder.is_empty() {
        preset.sw_video_encoder.clone()
    } else {
        format!("{}/{}", preset.hw_video_encoder, preset.sw_video_encoder)
    };
    format!(
        "{video_encoder} {dims} {bitrate} fps_cap={fps_cap} + {} {}k {}ch {}Hz / seg {}s / video_copy={} audio_copy={}",
        preset.audio_codec,
        preset.audio_bitrate / 1000,
        preset.audio_channels,
        preset.audio_sample_rate.unwrap_or(0),
        preset.hls_segment_secs,
        preset.video_copy,
        preset.audio_copy
    )
}
