// Without the feature there is no libav to link against, so the test binary
// would fail to build (the whole file is compiled out instead).
#![cfg(feature = "ffmpeg")]

//! Host-only regression test: a job that starts **mid-file** must carry the
//! container clock in *both* streams.
//!
//! The hybrid plan copies video packets (so they keep the container pts) while
//! re-encoding audio. When the audio pipeline used to restart its pts counter at
//! zero, a run anchored at 10 s produced segments whose audio sat at 0 s and
//! whose video sat at 10 s. Players see two unrelated clocks: they report the
//! mismatch as drift, resync the playhead backwards, or stall waiting for
//! matching audio/video.
//!
//! Needs `--features ffmpeg`, a linked FFmpeg 7.1 and a real file with a
//! copy-compatible video codec plus an audio codec that has to be re-encoded:
//!
//! ```text
//! ffmpeg -f lavfi -i "testsrc2=size=640x360:rate=24:duration=24" \
//!        -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=24" \
//!        -c:v libx264 -pix_fmt yuv420p -g 24 -c:a flac -ac 2 audio_clock.mkv
//! $env:WDMM_AUDIO_CLOCK_MEDIA='…\audio_clock.mkv'
//! cargo test -p wdmm-ffmpeg --features ffmpeg --test audio_clock -- --ignored
//! ```
//!
//! The demuxing half declares the few libav entry points it needs itself
//! (`sys` is crate-private) and reads the documented `AVPacket` offsets.

use std::ffi::{c_char, c_int, c_void, CString};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// `AVMEDIA_TYPE_VIDEO` / `AVMEDIA_TYPE_AUDIO`.
const AV_MEDIA_TYPE_VIDEO: c_int = 0;
const AV_MEDIA_TYPE_AUDIO: c_int = 1;
/// `AVPacket`: pts @+8 (i64), stream_index @+36 (c_int) — see `sys.rs`.
const PKT_PTS: usize = 8;
const PKT_STREAM_INDEX: usize = 36;
/// MPEG-TS always uses a 1/90000 time base for both streams.
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

/// First pts (seconds) seen for the first video and first audio packet.
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

fn wait_for_segment(dir: &Path, number: u64, timeout: Duration) -> bool {
    let segment = dir.join(format!("seg{number}.ts"));
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if std::fs::metadata(&segment)
            .map(|meta| meta.len())
            .unwrap_or(0)
            > 0
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
#[ignore = "needs WDMM_AUDIO_CLOCK_MEDIA (h264 + flac) and a linked FFmpeg 7.1"]
fn mid_file_hybrid_run_keeps_audio_on_the_container_clock() {
    let Some(media) = std::env::var_os("WDMM_AUDIO_CLOCK_MEDIA")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
    else {
        eprintln!("skip: WDMM_AUDIO_CLOCK_MEDIA not set");
        return;
    };

    let out_dir = std::env::temp_dir().join(format!("wdmm-audio-clock-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out_dir);
    std::fs::create_dir_all(&out_dir).expect("create out dir");

    const START_OFFSET: f64 = 10.0;
    const START_NUMBER: u64 = 3;
    let opts = wdmm_ffmpeg::JobOpts {
        input_url: media.to_string_lossy().into_owned(),
        preset: wdmm_ffmpeg::preset::preset_h264(),
        start_offset_secs: START_OFFSET,
        start_number: START_NUMBER,
        out_dir: Some(out_dir.clone()),
        hls_time_override_ms: Some(1),
        ..wdmm_ffmpeg::JobOpts::default()
    };
    let job = wdmm_ffmpeg::open_job(opts).expect("open_job");
    assert!(
        wait_for_segment(&out_dir, START_NUMBER, Duration::from_secs(60)),
        "seg{START_NUMBER}.ts never appeared in {}",
        out_dir.display()
    );
    drop(job);

    let (video_pts, audio_pts) =
        first_pts(&out_dir.join(format!("seg{START_NUMBER}.ts"))).expect("demux the first segment");
    eprintln!("mid-file run: video_pts={video_pts:.3}s audio_pts={audio_pts:.3}s");
    assert!(
        (video_pts - START_OFFSET).abs() < 1.0,
        "the copied video must start at the requested offset, got {video_pts:.3}s"
    );
    assert!(
        (audio_pts - video_pts).abs() < 0.3,
        "audio must follow the container clock, got audio={audio_pts:.3}s video={video_pts:.3}s"
    );
    let _ = std::fs::remove_dir_all(&out_dir);
}
