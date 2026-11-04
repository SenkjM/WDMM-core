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
//! $env:WDMM_HLS_TEST_MEDIA_MKV='E:\test\video\_env\out_copy.mkv'
//! $env:WDMM_HLS_TEST_MEDIA_NOCUES='E:\test\video\_env\out_nocues.mkv'
//! cargo test -p wdmm-ffmpeg --features ffmpeg --test hls_vod_host -- --ignored --test-threads=1
//! ```

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use wdmm_ffmpeg::diag;

/// Serialise the suite: the diagnostics ring buffer is process-global, so
/// concurrent tests would truncate each other's window.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    diag::tail(256).join("\n")
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
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(media) = env_media("WDMM_HLS_TEST_MEDIA_HEAD") else {
        eprintln!("skip: WDMM_HLS_TEST_MEDIA_HEAD not set");
        return;
    };
    let out_dir = temp_dir("vod-opts");
    let start_number = 7_u64;
    let opts = wdmm_ffmpeg::JobOpts {
        input_url: media.to_string_lossy().into_owned(),
        preset: wdmm_ffmpeg::preset::preset_h264(),
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
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(media) = env_media("WDMM_HLS_TEST_MEDIA_TRANSCODE") else {
        eprintln!("skip: WDMM_HLS_TEST_MEDIA_TRANSCODE not set");
        return;
    };
    let out_dir = temp_dir("transcode");
    // dlna-compat tries remux first; a VP9/Opus input can only be transcoded.
    let mut preset = wdmm_ffmpeg::preset::preset_h264();
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
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
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
    let mut preset = wdmm_ffmpeg::preset::preset_h264();
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
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(media) = env_media("WDMM_HLS_TEST_MEDIA_AUDIO_HYBRID") else {
        eprintln!("skip: WDMM_HLS_TEST_MEDIA_AUDIO_HYBRID not set");
        return;
    };
    let out_dir = temp_dir("audio-hybrid");
    let mut preset = wdmm_ffmpeg::preset::preset_h264();
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
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(media) = env_media("WDMM_HLS_TEST_MEDIA_AUDIO") else {
        eprintln!("skip: WDMM_HLS_TEST_MEDIA_AUDIO not set");
        return;
    };
    let out_dir = temp_dir("audio-transcode");
    let mut preset = wdmm_ffmpeg::preset::preset_h264();
    preset.video_copy = false; // force the transcode path
    preset.audio_copy = false;
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
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
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

#[test]
#[ignore = "needs WDMM_HLS_TEST_MEDIA_SUBTITLES + linked FFmpeg 7.1"]
fn extract_subtitle_tracks_decodes_every_text_track_in_one_pass() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(media) = env_media("WDMM_HLS_TEST_MEDIA_SUBTITLES") else {
        eprintln!("skip: WDMM_HLS_TEST_MEDIA_SUBTITLES not set");
        return;
    };
    let url = media.to_string_lossy().into_owned();
    let started = std::time::Instant::now();
    // 0 is the video stream and 99 does not exist: both must be skipped, not
    // fail the pass. 2/3 are the subrip tracks.
    let tracks =
        wdmm_ffmpeg::extract_subtitle_tracks(&url, &[0, 2, 3, 99]).expect("multi-track extraction");
    let elapsed = started.elapsed();
    assert_eq!(
        tracks.len(),
        2,
        "expected only the two text tracks, got {:?}\n{}",
        tracks.keys().collect::<Vec<_>>(),
        diagnostics()
    );
    for index in [2u32, 3] {
        let text = tracks.get(&index).unwrap_or_else(|| {
            panic!("stream {index} missing from multi-track result: {tracks:?}")
        });
        assert!(
            text.starts_with("WEBVTT"),
            "stream {index} is not WebVTT: {text:?}"
        );
        assert!(text.contains("-->"), "stream {index} has no cues: {text:?}");
    }
    assert!(
        !tracks.contains_key(&0),
        "non-subtitle stream was extracted"
    );
    assert!(!tracks.contains_key(&99), "unknown stream was extracted");
    assert_ne!(
        tracks.get(&2),
        tracks.get(&3),
        "the two tracks must decode independently"
    );
    // An empty request must not open the media at all.
    let none = wdmm_ffmpeg::extract_subtitle_tracks(&url, &[]).expect("empty request");
    assert!(none.is_empty());
    eprintln!(
        "multi-track extraction: {} track(s) in {:?}\n{}",
        tracks.len(),
        elapsed,
        diagnostics()
    );
}

// ── Source keyframe plan: plan grid == muxer cut grid ────────────────────────

/// Minimal libav access used only to check that a written segment starts on a
/// keyframe. The crate's own `sys` module is private, so the test declares the
/// two demux calls and the 7.1 layouts it needs (see `keyframe_index_probe.rs`).
#[cfg(feature = "ffmpeg")]
mod libav_video_key {
    use std::ffi::{c_char, c_int, c_void};
    use std::path::Path;

    /// `AVPacket` (sizeof 104, LP64): only `stream_index` and `flags` are read.
    #[repr(C)]
    struct Packet {
        buf: *mut c_void,
        pts: i64,
        dts: i64,
        data: *mut u8,
        size: c_int,
        stream_index: c_int,
        flags: c_int,
        side_data: *mut c_void,
        side_data_elems: c_int,
        duration: i64,
        pos: i64,
        opaque: *mut c_void,
        opaque_ref: *mut c_void,
        time_base: [c_int; 2],
    }

    #[link(name = "avformat")]
    extern "C" {
        fn avformat_open_input(
            ps: *mut *mut c_void,
            url: *const c_char,
            fmt: *const c_void,
            options: *mut *mut c_void,
        ) -> c_int;
        fn avformat_find_stream_info(ic: *mut c_void, opts: *mut *mut c_void) -> c_int;
        fn av_read_frame(s: *mut c_void, pkt: *mut Packet) -> c_int;
        fn avformat_close_input(ps: *mut *mut c_void);
    }

    #[link(name = "avcodec")]
    extern "C" {
        fn av_packet_unref(pkt: *mut Packet);
    }

    /// Whether the first video packet of `path` carries `AV_PKT_FLAG_KEY`.
    ///
    /// `None` when the file cannot be opened or carries no video packet.
    pub fn first_video_packet_is_keyframe(path: &Path) -> Option<bool> {
        let url = std::ffi::CString::new(path.to_string_lossy().as_bytes()).ok()?;
        unsafe {
            let mut fmt: *mut c_void = std::ptr::null_mut();
            if avformat_open_input(
                &mut fmt,
                url.as_ptr(),
                std::ptr::null(),
                std::ptr::null_mut(),
            ) < 0
            {
                return None;
            }
            if avformat_find_stream_info(fmt, std::ptr::null_mut()) < 0 {
                avformat_close_input(&mut fmt);
                return None;
            }
            // AVFormatContext (LP64): nb_streams @44, streams @48 (`AVStream **`).
            let nb_streams = *(fmt as *const c_int).add(11) as usize;
            let streams = *(fmt as *const *mut *mut u8).add(6);
            let mut found = None;
            let mut pkt: Packet = std::mem::zeroed();
            loop {
                if av_read_frame(fmt, &mut pkt) < 0 {
                    break;
                }
                let index = pkt.stream_index.max(0) as usize;
                if index < nb_streams {
                    let st = *streams.add(index);
                    if !st.is_null() {
                        // AVStream.codecpar @16; AVCodecParameters.codec_type @0.
                        let cp = *((st as *const u8).add(16) as *const *const u8);
                        if !cp.is_null() && *(cp as *const c_int) == 0 {
                            found = Some(pkt.flags & 0x0001 != 0);
                            av_packet_unref(&mut pkt);
                            break;
                        }
                    }
                }
                av_packet_unref(&mut pkt);
            }
            avformat_close_input(&mut fmt);
            found
        }
    }
}

#[cfg(feature = "ffmpeg")]
#[test]
#[ignore = "needs WDMM_HLS_TEST_MEDIA_HEAD (MP4) / WDMM_HLS_TEST_MEDIA_MKV (Cues MKV) + linked FFmpeg 7.1"]
fn source_plan_reports_the_container_keyframe_grid() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut checked = 0;
    for (env, expected_keyframes, label) in [
        ("WDMM_HLS_TEST_MEDIA_HEAD", 27_usize, "MP4"),
        ("WDMM_HLS_TEST_MEDIA_MKV", 27_usize, "Cues MKV"),
    ] {
        let Some(media) = env_media(env) else {
            eprintln!("skip: {env} not set");
            continue;
        };
        let url = media.to_string_lossy().into_owned();
        let preset = wdmm_ffmpeg::preset::preset_h264();
        let source = wdmm_ffmpeg::source_plan(&url, &preset)
            .expect("source_plan must not fail for a normal container")
            .unwrap_or_else(|| {
                panic!(
                    "{label}: a container with a keyframe index must yield a plan\n{}",
                    diagnostics()
                )
            });
        assert_eq!(
            source.plan.alignment,
            wdmm_ffmpeg::PlanAlignment::Keyframe,
            "{label}: the index-backed path must be marked keyframe"
        );
        assert!(
            source.video_copied,
            "{label}: H.264/AAC sources are video-copied, so the plan is the index grid"
        );
        assert_eq!(
            source.keyframe_count,
            expected_keyframes,
            "{label}: keyframe count must equal the ffprobe ground truth\n{}",
            diagnostics()
        );
        assert_eq!(
            source.plan.segment_count(),
            source.plan.entries.len(),
            "{label}: plan entries drive the playlist"
        );
        let first = source.plan.start_ms(0).expect("first segment start");
        let mut previous = i64::MIN;
        for (k, entry) in source.plan.entries.iter().enumerate() {
            assert!(
                entry.duration_ms > 0,
                "{label}: segment {k} must have positive duration"
            );
            assert!(
                entry.start_ms > previous,
                "{label}: boundaries must strictly increase at {k}"
            );
            if k > 0 {
                // One segment per keyframe interval: each entry starts exactly
                // where the previous one ends (contiguous keyframe cuts).
                assert_eq!(
                    entry.start_ms,
                    previous + source.plan.entries[k - 1].duration_ms,
                    "{label}: segment {k} must start where segment {} ended",
                    k - 1
                );
            }
            previous = entry.start_ms;
        }
        let total: i64 = source.plan.entries.iter().map(|e| e.duration_ms).sum();
        assert_eq!(
            total,
            source.plan.duration_ms - first,
            "{label}: the last segment must run to the media end"
        );
        eprintln!(
            "{label}: plan entries={:?}",
            source
                .plan
                .entries
                .iter()
                .map(|e| (e.start_ms, e.duration_ms))
                .collect::<Vec<_>>()
        );
        assert!(
            source.plan.gop_ms.unwrap_or(0) > 0,
            "{label}: a keyframe plan reports its measured GOP"
        );
        eprintln!(
            "{label}: keyframes={} segments={} gop_ms={:?} first={} last_start={}",
            source.keyframe_count,
            source.plan.segment_count(),
            source.plan.gop_ms,
            first,
            source
                .plan
                .start_ms(source.plan.segment_count() as u64 - 1)
                .expect("last segment start"),
        );
        checked += 1;
    }
    if checked == 0 {
        eprintln!("skip: neither source media env var is set");
    }
}

#[cfg(feature = "ffmpeg")]
#[test]
#[ignore = "needs WDMM_HLS_TEST_MEDIA_HEAD + linked FFmpeg 7.1"]
fn keyframe_plan_matches_muxer_cuts_for_sampled_segments() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(media) = env_media("WDMM_HLS_TEST_MEDIA_HEAD") else {
        eprintln!("skip: WDMM_HLS_TEST_MEDIA_HEAD not set");
        return;
    };
    let url = media.to_string_lossy().into_owned();
    let preset = wdmm_ffmpeg::preset::preset_h264();
    let source = wdmm_ffmpeg::source_plan(&url, &preset)
        .expect("source_plan")
        .expect("keyframe plan");
    let plan = source.plan.clone();
    let last = plan.segment_count().saturating_sub(1) as u64;
    let samples = [0_u64, last / 2, last.saturating_sub(2)];
    let mut seen = std::collections::HashSet::new();
    for k in samples {
        if !seen.insert(k) {
            continue;
        }
        let out_dir = temp_dir(&format!("keyframe-plan-k{k}"));
        let planned = plan.duration_of(k).expect("planned duration for k");
        // The proxy seeks half a GOP into the interval for keyframe plans
        // (`spawn_run_shared`: container indexes store DTS on MP4, so an exact
        // start would land on the previous keyframe). Segment 0 still starts at
        // the origin, so it is never offset.
        let seek_ms = if k == 0 {
            0
        } else {
            plan.start_ms(k).expect("segment start for k") + plan.gop_ms.unwrap_or(0) / 2
        };
        let opts = wdmm_ffmpeg::JobOpts {
            input_url: url.clone(),
            preset: preset.clone(),
            start_offset_secs: seek_ms as f64 / 1000.0,
            start_number: k,
            out_dir: Some(out_dir.clone()),
            // The proxy passes a 1 ms `hls_time` for keyframe/transcode plans so
            // the muxer cuts on every keyframe (see `KEYFRAME_CUT_HLS_TIME_MS`).
            hls_time_override_ms: Some(1),
            ..wdmm_ffmpeg::JobOpts::default()
        };
        let job = wdmm_ffmpeg::open_job(opts).expect("open_job");
        let segment = out_dir.join(format!("seg{k}.ts"));
        assert!(
            wait_until(Duration::from_secs(120), || segment.is_file()),
            "seg{k}.ts never appeared\n{}",
            diagnostics()
        );
        assert!(
            wait_until(Duration::from_secs(180), || !job.is_running()),
            "job for k={k} did not finish\n{}",
            diagnostics()
        );
        let durations = job.segment_durations();
        assert!(
            !durations.is_empty(),
            "no #EXTINF for k={k}\n{}",
            diagnostics()
        );
        let measured = durations[0];
        assert!(
            (measured - planned).abs() <= 200,
            "k={k}: plan says {planned} ms but the muxer cut {measured} ms (durations={durations:?})\n{}",
            diagnostics()
        );
        assert_eq!(
            libav_video_key::first_video_packet_is_keyframe(&segment),
            Some(true),
            "k={k}: seg{k}.ts must start on a video keyframe\n{}",
            diagnostics()
        );
        eprintln!("k={k}: planned={planned} ms muxer={measured} ms durations={durations:?}");
        drop(job);
        let _ = std::fs::remove_dir_all(&out_dir);
    }
}

#[cfg(feature = "ffmpeg")]
#[test]
#[ignore = "needs WDMM_HLS_TEST_MEDIA_HEAD + linked FFmpeg 7.1"]
fn encode_path_cuts_on_the_exact_grid() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(media) = env_media("WDMM_HLS_TEST_MEDIA_HEAD") else {
        eprintln!("skip: WDMM_HLS_TEST_MEDIA_HEAD not set");
        return;
    };
    let url = media.to_string_lossy().into_owned();
    // Force the real re-encode path at a cheap resolution, with a 2 s grid.
    let mut preset = wdmm_ffmpeg::preset::preset_h264();
    preset.video_copy = false;
    preset.audio_copy = false;
    preset.hls_segment_secs = 2;
    preset.video_max_w = Some(320);
    preset.video_max_h = Some(180);
    preset.video_fps_cap = Some(15);
    // An empty hardware encoder name forces the software encoder.
    preset.hw_video_encoder = String::new();

    let source = wdmm_ffmpeg::source_plan(&url, &preset)
        .expect("source_plan")
        .expect("exact plan for the encode path");
    assert_eq!(
        source.plan.alignment,
        wdmm_ffmpeg::PlanAlignment::Exact,
        "the re-encode path plans the exact uniform grid"
    );
    assert!(!source.video_copied);
    assert_eq!(
        source.keyframe_count, 0,
        "no keyframe index is needed when the encoder controls the cuts"
    );
    let plan = source.plan.clone();
    let out_dir = temp_dir("encode-grid");
    let mut job = wdmm_ffmpeg::open_job(wdmm_ffmpeg::JobOpts {
        input_url: url.clone(),
        preset: preset.clone(),
        start_offset_secs: 0.0,
        start_number: 0,
        out_dir: Some(out_dir.clone()),
        // The proxy passes a 1 ms `hls_time` for exact/keyframe plans; the
        // encoder's closed GOP pins a keyframe to every grid boundary.
        hls_time_override_ms: Some(1),
        ..wdmm_ffmpeg::JobOpts::default()
    })
    .expect("open_job");
    assert!(
        wait_until(Duration::from_secs(300), || !job.is_running()),
        "encode job did not finish\n{}",
        diagnostics()
    );
    let result = job.take_result().expect("encode job result");
    result.expect("encode job must succeed");
    let durations = job.segment_durations();
    assert!(
        durations.len() >= 3,
        "expected several encoded segments, got {durations:?}\n{}",
        diagnostics()
    );
    let mut cumulative = 0_i64;
    let last = durations.len().saturating_sub(1);
    for (k, &measured) in durations.iter().enumerate() {
        let planned = plan.duration_of(k as u64).expect("planned duration");
        cumulative += measured;
        if k == last {
            // The final EXTINF comes from the muxer trailer (real media tail),
            // so it is not a full grid step; the sampled-segment contract
            // covers every other entry.
            eprintln!(
                "encode-path last segment: plan={planned} ms muxer={measured} ms (cumulative end {cumulative})"
            );
            break;
        }
        assert!(
            (measured - planned).abs() <= 200,
            "encoded segment {k}: plan says {planned} ms, muxer wrote {measured} ms (durations={durations:?})\n{}",
            diagnostics()
        );
        let start = plan.start_ms(k as u64).expect("planned start");
        assert!(
            (cumulative - measured - start).abs() <= 500,
            "encoded segment {k}: cumulative start {} != planned start {start}",
            cumulative - measured
        );
        if k < 3 {
            assert_eq!(
                libav_video_key::first_video_packet_is_keyframe(
                    &out_dir.join(format!("seg{k}.ts"))
                ),
                Some(true),
                "encoded seg{k}.ts must start on an IDR frame"
            );
        }
    }
    eprintln!(
        "encode-path sample: planned={:?} muxer={:?}",
        (0..durations.len().min(4))
            .map(|k| plan.duration_of(k as u64).unwrap_or(0))
            .collect::<Vec<_>>(),
        &durations[..durations.len().min(4)]
    );
    drop(job);
    let _ = std::fs::remove_dir_all(&out_dir);
}

#[cfg(feature = "ffmpeg")]
#[test]
#[ignore = "needs WDMM_HLS_TEST_MEDIA_NOCUES + linked FFmpeg 7.1"]
fn source_plan_refuses_a_truncated_keyframe_index() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(media) = env_media("WDMM_HLS_TEST_MEDIA_NOCUES") else {
        eprintln!("skip: WDMM_HLS_TEST_MEDIA_NOCUES not set");
        return;
    };
    let url = media.to_string_lossy().into_owned();
    let preset = wdmm_ffmpeg::preset::preset_h264();
    let plan = wdmm_ffmpeg::source_plan(&url, &preset)
        .expect("source_plan must degrade, not fail")
        .map(|source| source.plan.alignment);
    assert_eq!(
        plan,
        None,
        "a no-Cues MKV has no usable keyframe index and must fall back to the re-encode plan\n{}",
        diagnostics()
    );
    assert!(
        diagnostics().contains("source plan:"),
        "the fallback must be explained in the diagnostics\n{}",
        diagnostics()
    );
}

/// Encoder preset that runs long enough (hundreds of segments) to observe a
/// pause; a stream-copy job can finish in a fraction of a second.
fn long_running_preset() -> wdmm_core::models::TranscodePreset {
    let mut preset = wdmm_ffmpeg::preset::preset_h264();
    preset.video_copy = false;
    preset.audio_copy = false;
    preset.hls_segment_secs = 2;
    preset.video_max_w = Some(320);
    preset.video_max_h = Some(180);
    preset.video_fps_cap = Some(15);
    // An empty hardware encoder name forces the software encoder.
    preset.hw_video_encoder = String::new();
    preset
}

#[test]
#[ignore = "needs WDMM_HLS_TEST_MEDIA_HEAD + linked FFmpeg 7.1"]
fn paused_job_holds_progress_and_resume_finishes() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(media) = env_media("WDMM_HLS_TEST_MEDIA_HEAD") else {
        eprintln!("skip: WDMM_HLS_TEST_MEDIA_HEAD not set");
        return;
    };
    let out_dir = temp_dir("pause-resume");
    let mut job = wdmm_ffmpeg::open_job(wdmm_ffmpeg::JobOpts {
        input_url: media.to_string_lossy().into_owned(),
        preset: long_running_preset(),
        out_dir: Some(out_dir.clone()),
        hls_time_override_ms: Some(1),
        ..wdmm_ffmpeg::JobOpts::default()
    })
    .expect("open_job");

    assert!(
        wait_until(Duration::from_secs(120), || job.progress().content_ms > 0),
        "job never produced progress\n{}",
        diagnostics()
    );
    job.pause();
    assert!(job.is_paused(), "the handle reports the pause");
    // Give the read loop time to reach a packet boundary and block.
    std::thread::sleep(Duration::from_millis(300));
    assert!(job.is_running(), "a paused job still counts as running");
    let frozen = job.progress().content_ms;
    std::thread::sleep(Duration::from_millis(500));
    let later = job.progress().content_ms;
    assert_eq!(
        frozen,
        later,
        "progress must not advance while paused (frozen={frozen}, later={later})\n{}",
        diagnostics()
    );

    job.resume();
    assert!(!job.is_paused(), "the handle reports the resume");
    assert!(
        wait_until(Duration::from_secs(300), || !job.is_running()),
        "resumed job did not finish\n{}",
        diagnostics()
    );
    job.take_result()
        .expect("job result")
        .expect("a resumed job must succeed");
    assert!(
        segment_count(&out_dir) > 0,
        "a resumed job keeps writing segments\n{}",
        diagnostics()
    );
    drop(job);
    let _ = std::fs::remove_dir_all(&out_dir);
}

#[test]
#[ignore = "needs WDMM_HLS_TEST_MEDIA_HEAD + linked FFmpeg 7.1"]
fn cancelling_a_paused_job_exits_without_deadlock() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(media) = env_media("WDMM_HLS_TEST_MEDIA_HEAD") else {
        eprintln!("skip: WDMM_HLS_TEST_MEDIA_HEAD not set");
        return;
    };
    let out_dir = temp_dir("pause-cancel");
    let mut job = wdmm_ffmpeg::open_job(wdmm_ffmpeg::JobOpts {
        input_url: media.to_string_lossy().into_owned(),
        preset: long_running_preset(),
        out_dir: Some(out_dir.clone()),
        hls_time_override_ms: Some(1),
        ..wdmm_ffmpeg::JobOpts::default()
    })
    .expect("open_job");

    assert!(
        wait_until(Duration::from_secs(120), || job.progress().content_ms > 0),
        "job never produced progress\n{}",
        diagnostics()
    );
    job.pause();
    std::thread::sleep(Duration::from_millis(200));
    assert!(job.is_running(), "job is paused, not finished");
    job.cancel();
    assert!(
        wait_until(Duration::from_secs(60), || !job.is_running()),
        "a cancel must wake a paused job (no deadlock)\n{}",
        diagnostics()
    );
    assert!(job.is_paused(), "cancel leaves the pause latch untouched");
    match job.take_result() {
        Some(Err(_)) => {} // cancelled jobs report an error
        Some(Ok(())) => {} // or the run finished at the boundary
        None => panic!("job ended without a result\n{}", diagnostics()),
    }
    drop(job);
    let _ = std::fs::remove_dir_all(&out_dir);
}
