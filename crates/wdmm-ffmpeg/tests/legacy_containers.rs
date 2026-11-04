//! Regression tests for legacy containers (AVI, WMV, FLV, MPEG-PS, 3GP).
//!
//! Two failure modes are covered:
//!
//! 1. `swr_convert_frame` rejecting every frame with `AVERROR_INPUT_CHANGED`
//!    because the container stores no channel layout (`AV_CHANNEL_ORDER_UNSPEC`)
//!    while libswresample normalises the configured layout to `NATIVE`.
//! 2. `av_interleaved_write_frame` failing with `EINVAL` because a container
//!    without per-packet timestamps (AVI keeps only a frame index) leaves the
//!    video frame PTS at `AV_NOPTS_VALUE`, which the MPEG-TS muxer rejects.
//!
//! Both were fixed by normalising the decoded channel layout before handing
//! frames to libswresample, and by synthesising a monotonic timeline for streams
//! that do not carry one.
//!
//! Gate: `WDMM_LEGACY_TEST_MEDIA` = one or more local paths separated by `;`.
//! Run with:
//! ```text
//! cargo test -p wdmm-ffmpeg --features ffmpeg --test legacy_containers -- --ignored --nocapture --test-threads=1
//! ```

#![cfg(feature = "ffmpeg")]

use std::ffi::{c_char, c_int, c_void, CString};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use wdmm_ffmpeg::diag;

/// `AVMEDIA_TYPE_VIDEO` / `AVMEDIA_TYPE_AUDIO`.
const AV_MEDIA_TYPE_VIDEO: c_int = 0;
const AV_MEDIA_TYPE_AUDIO: c_int = 1;
/// `AVPacket`: pts @+8 (i64), stream_index @+36 (c_int) - see `sys.rs`.
const PKT_PTS: usize = 8;
const PKT_STREAM_INDEX: usize = 36;
/// MPEG-TS stamps both streams on a 1/90000 time base.
const TS_TICKS_PER_SEC: f64 = 90_000.0;

extern "C" {
    fn avformat_open_input(
        ps: *mut *mut c_void,
        url: *const c_char,
        fmt: *mut c_void,
        options: *mut *mut c_void,
    ) -> c_int;
    fn avformat_find_stream_info(ctx: *mut c_void, options: *mut *mut c_void) -> c_int;
    fn avformat_close_input(ps: *mut *mut c_void);
    fn av_find_best_stream(
        ctx: *mut c_void,
        media_type: c_int,
        wanted_stream_nb: c_int,
        related_stream: c_int,
        decoder: *mut *const c_void,
        flags: c_int,
    ) -> c_int;
    fn av_read_frame(ctx: *mut c_void, pkt: *mut u8) -> c_int;
    fn av_packet_alloc() -> *mut u8;
    fn av_packet_free(pkt: *mut *mut u8);
    fn av_packet_unref(pkt: *mut u8);
}

/// First pts (seconds) of the first video and the first audio packet.
fn first_pts(path: &Path) -> Option<(f64, f64)> {
    let url = CString::new(path.to_string_lossy().as_bytes()).ok()?;
    unsafe {
        let mut ctx: *mut c_void = std::ptr::null_mut();
        if avformat_open_input(
            &mut ctx,
            url.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ) != 0
        {
            return None;
        }
        let opened = avformat_find_stream_info(ctx, std::ptr::null_mut()) >= 0;
        let video = av_find_best_stream(ctx, AV_MEDIA_TYPE_VIDEO, -1, -1, std::ptr::null_mut(), 0);
        let audio = av_find_best_stream(ctx, AV_MEDIA_TYPE_AUDIO, -1, -1, std::ptr::null_mut(), 0);
        let mut video_pts: Option<f64> = None;
        let mut audio_pts: Option<f64> = None;
        if opened && video >= 0 && audio >= 0 {
            let mut pkt = av_packet_alloc();
            if !pkt.is_null() {
                for _ in 0..4_000 {
                    if video_pts.is_some() && audio_pts.is_some() {
                        break;
                    }
                    if av_read_frame(ctx, pkt) < 0 {
                        break;
                    }
                    let index = *(pkt.add(PKT_STREAM_INDEX) as *const c_int);
                    let pts = *(pkt.add(PKT_PTS) as *const i64);
                    if pts != i64::MIN {
                        let seconds = pts as f64 / TS_TICKS_PER_SEC;
                        if index == video && video_pts.is_none() {
                            video_pts = Some(seconds);
                        } else if index == audio && audio_pts.is_none() {
                            audio_pts = Some(seconds);
                        }
                    }
                    av_packet_unref(pkt);
                }
                av_packet_free(&mut pkt as *mut *mut u8);
            }
        }
        avformat_close_input(&mut ctx);
        Some((video_pts?, audio_pts?))
    }
}

const SEGMENT_SECS: u32 = 2;
const JOB_TIMEOUT: Duration = Duration::from_secs(180);

fn media_list() -> Vec<PathBuf> {
    let Some(raw) = std::env::var_os("WDMM_LEGACY_TEST_MEDIA") else {
        eprintln!("skip: WDMM_LEGACY_TEST_MEDIA not set");
        return Vec::new();
    };
    let raw = raw.to_string_lossy().into_owned();
    raw.split(';')
        .filter(|item| !item.is_empty())
        .map(PathBuf::from)
        .filter(|path| {
            if path.is_file() {
                true
            } else {
                eprintln!("skip missing {}", path.display());
                false
            }
        })
        .collect()
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wdmm-legacy-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Every produced segment file, sorted by number.
fn segments(dir: &Path) -> Vec<(u64, u64)> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(number) = name
            .strip_prefix("seg")
            .and_then(|rest| rest.strip_suffix(".ts"))
            .and_then(|digits| digits.parse::<u64>().ok())
        else {
            continue;
        };
        let len = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
        found.push((number, len));
    }
    found.sort_unstable();
    found
}

/// Sum of the `#EXTINF` durations in the muxer playlist, in milliseconds.
fn playlist_seconds(dir: &Path) -> Option<f64> {
    let text = std::fs::read_to_string(dir.join("index.m3u8")).ok()?;
    let total: f64 = text
        .lines()
        .filter_map(|line| line.strip_prefix("#EXTINF:"))
        .filter_map(|rest| rest.split(',').next())
        .filter_map(|value| value.trim().parse::<f64>().ok())
        .sum();
    Some(total)
}

/// Transcode one media file through the forced re-encode path and assert that it
/// finishes cleanly with a usable VOD playlist.
fn transcode_cleanly(media: &Path, tag: &str) -> Option<f64> {
    let out_dir = temp_dir(tag);
    let mut preset = wdmm_ffmpeg::preset::preset_h264();
    preset.hls_segment_secs = SEGMENT_SECS;
    preset.video_copy = false;
    preset.audio_copy = false;
    let opts = wdmm_ffmpeg::JobOpts {
        input_url: media.to_string_lossy().into_owned(),
        preset,
        out_dir: Some(out_dir.clone()),
        ..wdmm_ffmpeg::JobOpts::default()
    };
    let mut job = match wdmm_ffmpeg::open_job(opts) {
        Ok(job) => job,
        Err(error) => {
            let tail = diag::tail(40).join("\n");
            let _ = std::fs::remove_dir_all(&out_dir);
            panic!("open_job failed for {}: {error}\n{tail}", media.display());
        }
    };
    let deadline = Instant::now() + JOB_TIMEOUT;
    while job.is_running() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if job.is_running() {
        job.cancel();
    }
    let result = job.take_result();
    let tail = diag::tail(60).join("\n");
    let produced = segments(&out_dir);
    let playlist = playlist_seconds(&out_dir);
    drop(job);
    let _ = std::fs::remove_dir_all(&out_dir);

    match result {
        Some(Ok(())) => {}
        other => panic!(
            "{} did not transcode cleanly: {other:?}\n{tail}",
            media.display()
        ),
    }
    assert!(
        !produced.is_empty(),
        "{} produced no segments\n{tail}",
        media.display()
    );
    assert!(
        produced.iter().all(|(number, len)| {
            let _ = number;
            *len > 0
        }),
        "{} produced an empty segment\n{tail}",
        media.display()
    );
    let total = playlist.unwrap_or_else(|| {
        panic!(
            "{} produced no playlist with #EXTINF\n{tail}",
            media.display()
        )
    });
    assert!(
        total > 0.0,
        "{} playlist carries no duration\n{tail}",
        media.display()
    );
    Some(total)
}

/// A container whose audio track has no stored channel layout must still decode:
/// libswresample compares the whole layout, so the decoded layout has to be
/// normalised before the frame reaches `swr_convert_frame`.
#[test]
#[ignore = "needs WDMM_LEGACY_TEST_MEDIA"]
fn legacy_containers_transcode_without_layout_or_timestamp_errors() {
    for media in media_list() {
        let duration = wdmm_ffmpeg::probe(&media.to_string_lossy())
            .map(|probe| probe.duration_secs)
            .unwrap_or(0.0);
        let tag = format!(
            "transcode-{}",
            media
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_else(|| "media".to_string())
        );
        let playlist = transcode_cleanly(&media, &tag).expect("playlist duration");
        // The playlist covers the same content as the source; a synthetic
        // timeline must not stretch or squash it.
        if duration > 1.0 {
            let ratio = playlist / duration;
            assert!(
                (0.8..=1.25).contains(&ratio),
                "{} playlist covers {playlist:.2}s for a {duration:.2}s source (ratio {ratio:.2})",
                media.display()
            );
        }
    }
}

/// The same sources must also survive the hybrid/remux path, which copies video
/// packets straight through and therefore needs the timestamp repair even more.
#[test]
#[ignore = "needs WDMM_LEGACY_TEST_MEDIA"]
fn legacy_containers_remux_without_timestamp_errors() {
    for media in media_list() {
        let out_dir = temp_dir(&format!(
            "remux-{}",
            media
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_else(|| "media".to_string())
        ));
        let mut preset = wdmm_ffmpeg::preset::preset_h264();
        preset.hls_segment_secs = SEGMENT_SECS;
        preset.video_copy = true;
        preset.audio_copy = true;
        let opts = wdmm_ffmpeg::JobOpts {
            input_url: media.to_string_lossy().into_owned(),
            preset,
            out_dir: Some(out_dir.clone()),
            ..wdmm_ffmpeg::JobOpts::default()
        };
        let mut job = wdmm_ffmpeg::open_job(opts).expect("open_job");
        let deadline = Instant::now() + JOB_TIMEOUT;
        while job.is_running() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if job.is_running() {
            job.cancel();
        }
        let result = job.take_result();
        let tail = diag::tail(60).join("\n");
        let produced = segments(&out_dir);
        drop(job);
        let _ = std::fs::remove_dir_all(&out_dir);
        match result {
            Some(Ok(())) => {}
            other => panic!(
                "{} did not remux cleanly: {other:?}\n{tail}",
                media.display()
            ),
        }
        assert!(
            !produced.is_empty(),
            "{} produced no segments on the remux path\n{tail}",
            media.display()
        );
    }
}

/// A run that starts mid-file must stamp **both** streams on the container clock.
///
/// A source without per-packet time gets a synthetic clock; if that clock started
/// at zero for a seeked run, the video would sit at 0 s while the audio pipeline
/// (seeded from the same offset) sat at the seek point - the desync players
/// report as drift or as a stalled stream.
#[test]
#[ignore = "needs WDMM_LEGACY_TEST_MEDIA"]
fn legacy_containers_keep_the_container_clock_after_a_seek() {
    const START_OFFSET: f64 = 4.0;
    for media in media_list() {
        let out_dir = temp_dir(&format!(
            "seek-{}",
            media
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_else(|| "media".to_string())
        ));
        let mut preset = wdmm_ffmpeg::preset::preset_h264();
        preset.hls_segment_secs = SEGMENT_SECS;
        preset.video_copy = true;
        preset.audio_copy = true;
        let opts = wdmm_ffmpeg::JobOpts {
            input_url: media.to_string_lossy().into_owned(),
            preset,
            start_offset_secs: START_OFFSET,
            out_dir: Some(out_dir.clone()),
            // Cut on the first keyframe so the segment appears without waiting.
            hls_time_override_ms: Some(1),
            ..wdmm_ffmpeg::JobOpts::default()
        };
        let job = wdmm_ffmpeg::open_job(opts).expect("open_job");
        let segment = out_dir.join("seg0.ts");
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if std::fs::metadata(&segment)
                .map(|meta| meta.len())
                .unwrap_or(0)
                > 0
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        drop(job);
        let (video_pts, audio_pts) = first_pts(&segment).unwrap_or_else(|| {
            panic!(
                "{} produced no demuxable segment\n{}",
                media.display(),
                diag::tail(60).join("\n")
            )
        });
        let _ = std::fs::remove_dir_all(&out_dir);
        eprintln!(
            "{}: seek to {START_OFFSET:.1}s -> video {video_pts:.3}s audio {audio_pts:.3}s",
            media.display()
        );
        assert!(
            (START_OFFSET - 3.0..=START_OFFSET + 1.0).contains(&video_pts),
            "{}: video must stay on the container clock after a seek, got {video_pts:.3}s",
            media.display()
        );
        // The audio track may start a little earlier: its packets precede the
        // first video keyframe the seek landed on. What must not happen is the
        // whole audio clock sitting at zero (or at a different anchor).
        assert!(
            (-1.0..=0.5).contains(&(audio_pts - video_pts)),
            "{}: audio must follow the same clock, audio={audio_pts:.3}s video={video_pts:.3}s",
            media.display()
        );
    }
}
