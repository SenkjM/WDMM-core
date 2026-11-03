//! Host-only integration tests for the VOD HLS job plumbing (task-15).
//!
//! Every test here needs a linked FFmpeg 7.1 (`--features ffmpeg` + MSVC/NDK
//! libs on the link search path) **and** a real media file supplied through an
//! environment variable. They are `#[ignore]`d by default and silently pass when
//! the inputs are absent, so a normal `cargo test` (no libav) stays green.
//!
//! Run them explicitly, for example:
//!
//! ```text
//! $env:FF='E:\test\video\_env\ffmpeg-n7.1.5-...-7.1'
//! $env:WDMM_FFMPEG_LIB_DIR="$env:FF\lib"; $env:RUSTFLAGS="-L native=$env:FF\lib"
//! $env:PATH="$env:FF\bin;$env:PATH"
//! $env:WDMM_HLS_TEST_MEDIA_HEAD='E:\test\video\1.m4v'
//! $env:WDMM_HLS_TEST_MEDIA_TAIL='E:\test\video\2.mp4'
//! $env:WDMM_HLS_TEST_MEDIA_TRANSCODE='E:\test\video\_env\transcode.mkv'
//! $env:WDMM_HLS_TEST_MEDIA_AUDIO='E:\test\video\_env\audio-only.m4a'
//! $env:WDMM_HLS_TEST_MEDIA_AUDIO_HYBRID='E:\test\video\_env\audio-only.flac'
//! $env:WDMM_HLS_TEST_MEDIA_SUBTITLES='E:\test\video\_env\subtitles.mkv'
//! cargo test -p wdmm-ffmpeg --features ffmpeg --test hls_vod_host -- --ignored --test-threads=1
//! ```

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use wdmm_ffmpeg::diag;

/// Master gate: no test runs unless this is set (mirrors the existing HLS tests).
fn gate_open() -> bool {
    std::env::var_os("WDMM_HLS_TEST_MEDIA").is_some()
}

fn env_media(name: &str) -> Option<PathBuf> {
    if !gate_open() {
        return None;
    }
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_file())
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wdmm-hls-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn diagnostics() -> String {
    diag::tail(80).join("\n")
}

fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    predicate()
}

/// Wait for the job to finish (or cancel it after `timeout`).
fn wait_finished(job: &mut wdmm_ffmpeg::JobHandle, timeout: Duration) -> Option<bool> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !job.is_running() {
            return Some(true);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    job.cancel();
    None
}

fn segment_count(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    name.starts_with("seg") && name.ends_with(".ts")
                })
                .count()
        })
        .unwrap_or(0)
}

fn playlist_len(dir: &Path) -> u64 {
    std::fs::metadata(dir.join("index.m3u8"))
        .map(|meta| meta.len())
        .unwrap_or(0)
}

#[test]
#[ignore = "needs WDMM_HLS_TEST_MEDIA_HEAD + linked FFmpeg 7.1"]
fn vod_job_honours_start_number_out_dir_and_hls_time() {
    let Some(media) = env_media("WDMM_HLS_TEST_MEDIA_HEAD") else {
        eprintln!("skip: WDMM_HLS_TEST_MEDIA_HEAD not set");
        return;
    };
    let out_dir = temp_dir("vod-opts");
    let start_number = 7_u64;
    let opts = wdmm_ffmpeg::JobOpts {
        input_url: media.to_string_lossy().into_owned(),
        preset: wdmm_ffmpeg::preset::preset_dlna_compat(),
        start_number,
        out_dir: Some(out_dir.clone()),
        hls_time_override_ms: Some(1_600),
        ..wdmm_ffmpeg::JobOpts::default()
    };

    let job = wdmm_ffmpeg::open_job(opts).expect("open_job");
    let segment = out_dir.join(format!("seg{start_number}.ts"));
    let ready = wait_until(Duration::from_secs(60), || {
        segment.is_file() && playlist_len(&out_dir) > 0
    });
    assert!(
        ready,
        "seg{start_number}.ts / playlist never appeared in {}\n{}",
        out_dir.display(),
        diagnostics()
    );
    assert!(
        job.ready_segments() >= 1,
        "ready_segments must see the muxer output"
    );
    let durations = job.segment_durations();
    assert!(
        !durations.is_empty(),
        "segment_durations must parse #EXTINF from index.m3u8\n{}",
        diagnostics()
    );
    assert!(
        durations.iter().all(|duration| *duration > 0),
        "measured durations must be positive: {durations:?}"
    );
    assert_eq!(
        job.out_dir(),
        out_dir.as_path(),
        "out_dir() must expose the persistent cache dir"
    );
    eprintln!(
        "vod opts: start_number={start_number}, ready={}, durations={durations:?}",
        job.ready_segments()
    );

    // Drop must cancel but NOT delete the persistent cache dir.
    drop(job);
    assert!(
        out_dir.join("index.m3u8").is_file(),
        "JobHandle::drop must not delete a JobOpts::out_dir directory"
    );
    let _ = std::fs::remove_dir_all(&out_dir);
}

#[test]
#[ignore = "needs WDMM_HLS_TEST_MEDIA_TRANSCODE + linked FFmpeg 7.1"]
fn transcode_path_produces_a_full_vod_playlist() {
    let Some(media) = env_media("WDMM_HLS_TEST_MEDIA_TRANSCODE") else {
        eprintln!("skip: WDMM_HLS_TEST_MEDIA_TRANSCODE not set");
        return;
    };
    let out_dir = temp_dir("transcode");
    // dlna-compat tries remux first; a VP9/Opus input can only be transcoded.
    let mut preset = wdmm_ffmpeg::preset::preset_dlna_compat();
    // The default preset's 4 s segments leave only two segments for this 8 s
    // clip; use 2 s so the assertion actually exercises playlist rotation.
    preset.hls_segment_secs = 2;
    let opts = wdmm_ffmpeg::JobOpts {
        input_url: media.to_string_lossy().into_owned(),
        preset,
        out_dir: Some(out_dir.clone()),
        ..wdmm_ffmpeg::JobOpts::default()
    };

    let mut job = wdmm_ffmpeg::open_job(opts).expect("open_job");
    let finished = wait_finished(&mut job, Duration::from_secs(180));
    assert!(
        finished.is_some(),
        "transcode job did not finish in time\n{}",
        diagnostics()
    );
    let result = job
        .take_result()
        .expect("finished job must expose a result");
    let segments = segment_count(&out_dir);
    let durations = job.segment_durations();
    assert!(
        result.is_ok(),
        "transcode job failed: {:?}\nsegments={segments} playlist_bytes={}\n{}",
        result.err(),
        playlist_len(&out_dir),
        diagnostics()
    );
    assert!(
        playlist_len(&out_dir) > 0,
        "transcode produced no playlist\n{}",
        diagnostics()
    );
    assert!(
        segments >= 3,
        "transcode produced only {segments} segments\n{}",
        diagnostics()
    );
    assert!(
        !durations.is_empty(),
        "transcode playlist has no #EXTINF durations\n{}",
        diagnostics()
    );
    let total: i64 = durations.iter().sum();
    assert!(
        total > 1_000,
        "playlist covers only {total} ms of media\n{}",
        diagnostics()
    );
    eprintln!(
        "transcode: segments={segments} durations={durations:?} total_ms={total}\n{}",
        diagnostics()
    );
    drop(job);
    let _ = std::fs::remove_dir_all(&out_dir);
}

/// Assert the job produced a real audio-only HLS output (playlist + segments).
fn assert_audio_only_output(
    job: &mut wdmm_ffmpeg::JobHandle,
    out_dir: &Path,
    tag: &str,
    min_segments: usize,
) -> Vec<i64> {
    let finished = wait_finished(job, Duration::from_secs(120));
    assert!(
        finished.is_some(),
        "{tag} job did not finish in time\n{}",
        diagnostics()
    );
    let result = job
        .take_result()
        .expect("finished job must expose a result");
    let segments = segment_count(out_dir);
    assert!(
        result.is_ok(),
        "{tag} job failed: {:?}\nsegments={segments} playlist_bytes={}\n{}",
        result.err(),
        playlist_len(out_dir),
        diagnostics()
    );
    assert!(
        playlist_len(out_dir) > 0,
        "{tag} produced no playlist\n{}",
        diagnostics()
    );
    assert!(
        segments >= min_segments,
        "{tag} produced only {segments} segments (expected >= {min_segments})\n{}",
        diagnostics()
    );
    let durations = job.segment_durations();
    assert!(
        !durations.is_empty(),
        "{tag} playlist has no #EXTINF durations\n{}",
        diagnostics()
    );
    let encoder = job.encoder_info();
    assert!(
        encoder.video.is_none(),
        "{tag}: an audio-only source must not report a video encoder: {encoder:?}"
    );
    durations
}

#[test]
#[ignore = "needs WDMM_HLS_TEST_MEDIA_AUDIO + linked FFmpeg 7.1"]
fn audio_only_source_remuxes_to_audio_only_playlist() {
    let Some(media) = env_media("WDMM_HLS_TEST_MEDIA_AUDIO") else {
        eprintln!("skip: WDMM_HLS_TEST_MEDIA_AUDIO not set");
        return;
    };
    let url = media.to_string_lossy().into_owned();
    let probed = wdmm_ffmpeg::probe(&url).expect("probe audio-only source");
    assert!(
        probed.video.is_empty(),
        "audio-only fixture must report no video streams: {:?}",
        probed.video
    );
    assert!(
        !probed.audio.is_empty(),
        "audio-only fixture must report an audio stream"
    );

    let out_dir = temp_dir("audio-remux");
    // dlna-compat asks for video, but the source has none: the remux stage must
    // still copy the MPEG-TS-copyable AAC audio instead of re-encoding it.
    let mut preset = wdmm_ffmpeg::preset::preset_dlna_compat();
    preset.hls_segment_secs = 4;
    let opts = wdmm_ffmpeg::JobOpts {
        input_url: url,
        preset,
        out_dir: Some(out_dir.clone()),
        ..wdmm_ffmpeg::JobOpts::default()
    };
    let mut job = wdmm_ffmpeg::open_job(opts).expect("open_job");
    let durations = assert_audio_only_output(&mut job, &out_dir, "audio-only remux", 2);
    let encoder = job.encoder_info();
    assert_eq!(
        encoder.mode, "remux",
        "AAC in an M4A is MPEG-TS copyable, so the audio must be copied"
    );
    assert_eq!(encoder.audio.as_deref(), Some("copy"));
    let total: i64 = durations.iter().sum();
    assert!(
        total > 5_000,
        "audio-only playlist covers only {total} ms of media: {durations:?}"
    );
    eprintln!(
        "audio-only remux: durations={durations:?} total_ms={total} encoder={encoder:?}\n{}",
        diagnostics()
    );
    drop(job);
    let _ = std::fs::remove_dir_all(&out_dir);
}

#[test]
#[ignore = "needs WDMM_HLS_TEST_MEDIA_AUDIO_HYBRID + linked FFmpeg 7.1"]
fn audio_only_source_reencodes_unmuxable_audio() {
    let Some(media) = env_media("WDMM_HLS_TEST_MEDIA_AUDIO_HYBRID") else {
        eprintln!("skip: WDMM_HLS_TEST_MEDIA_AUDIO_HYBRID not set");
        return;
    };
    let out_dir = temp_dir("audio-hybrid");
    let mut preset = wdmm_ffmpeg::preset::preset_dlna_compat();
    preset.hls_segment_secs = 4;
    let opts = wdmm_ffmpeg::JobOpts {
        input_url: media.to_string_lossy().into_owned(),
        preset,
        out_dir: Some(out_dir.clone()),
        ..wdmm_ffmpeg::JobOpts::default()
    };
    let mut job = wdmm_ffmpeg::open_job(opts).expect("open_job");
    let durations = assert_audio_only_output(&mut job, &out_dir, "audio-only hybrid", 2);
    let encoder = job.encoder_info();
    assert_eq!(
        encoder.mode, "hybrid",
        "FLAC cannot be muxed into MPEG-TS, so only the audio is re-encoded"
    );
    assert_eq!(encoder.audio.as_deref(), Some("aac"));
    let log = diagnostics();
    assert!(
        log.contains("remux hybrid: audio-only"),
        "diag must state that the hybrid run is audio-only:\n{log}"
    );
    eprintln!("audio-only hybrid: durations={durations:?} encoder={encoder:?}\n{log}");
    drop(job);
    let _ = std::fs::remove_dir_all(&out_dir);
}

#[test]
#[ignore = "needs WDMM_HLS_TEST_MEDIA_AUDIO + linked FFmpeg 7.1"]
fn audio_only_source_survives_the_transcode_path() {
    let Some(media) = env_media("WDMM_HLS_TEST_MEDIA_AUDIO") else {
        eprintln!("skip: WDMM_HLS_TEST_MEDIA_AUDIO not set");
        return;
    };
    let out_dir = temp_dir("audio-transcode");
    let mut preset = wdmm_ffmpeg::preset::preset_dlna_compat();
    preset.remux_first = false; // force the transcode path
    preset.hls_segment_secs = 4;
    let opts = wdmm_ffmpeg::JobOpts {
        input_url: media.to_string_lossy().into_owned(),
        preset,
        out_dir: Some(out_dir.clone()),
        ..wdmm_ffmpeg::JobOpts::default()
    };
    let mut job = wdmm_ffmpeg::open_job(opts).expect("open_job");
    let durations = assert_audio_only_output(&mut job, &out_dir, "audio-only transcode", 2);
    let encoder = job.encoder_info();
    let log = diagnostics();
    assert!(
        log.contains("source has no decodable video stream → audio-only output"),
        "diag must explain the audio-only degradation:\n{log}"
    );
    assert!(
        !encoder.hw_requested,
        "no video stream means nothing to encode with hardware"
    );
    eprintln!("audio-only transcode: durations={durations:?} encoder={encoder:?}\n{log}");
    drop(job);
    let _ = std::fs::remove_dir_all(&out_dir);
}

#[test]
#[ignore = "needs WDMM_HLS_TEST_MEDIA_SUBTITLES + linked FFmpeg 7.1"]
fn embedded_subtitle_tracks_expose_language_and_title() {
    let Some(media) = env_media("WDMM_HLS_TEST_MEDIA_SUBTITLES") else {
        eprintln!("skip: WDMM_HLS_TEST_MEDIA_SUBTITLES not set");
        return;
    };
    let url = media.to_string_lossy().into_owned();
    let tracks = wdmm_ffmpeg::embedded_subtitle_tracks(&url).expect("embedded subtitle tracks");
    assert_eq!(
        tracks.len(),
        2,
        "fixture carries two subtitle tracks, got {tracks:?}\n{}",
        diagnostics()
    );
    let full = tracks
        .iter()
        .find(|track| track.title.as_deref() == Some("Full subtitles"))
        .unwrap_or_else(|| panic!("track title from container metadata missing: {tracks:?}"));
    assert_eq!(full.language.as_deref(), Some("jpn"));
    assert_eq!(full.index, 2, "probe must preserve the real stream index");
    let signs = tracks
        .iter()
        .find(|track| track.title.as_deref() == Some("Signs only"))
        .unwrap_or_else(|| panic!("second track title missing: {tracks:?}"));
    assert_eq!(signs.language.as_deref(), Some("eng"));
    assert_eq!(signs.index, 3);

    // The full probe and the thin track-list API must agree (one scan shape).
    let probed = wdmm_ffmpeg::probe(&url).expect("probe subtitles fixture");
    assert_eq!(probed.subtitles.len(), tracks.len());
    assert!(!probed.format_name.is_empty());
    eprintln!("embedded subtitles: {tracks:?}");
}
