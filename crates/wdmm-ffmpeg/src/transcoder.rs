//! In-process libav transcode engine (B3.5).
//!
//! Replaces the old `spawn.rs` / `probe.rs` subprocess approach. Both probe
//! and HLS transcode jobs run in this process via the FFmpeg shared libraries.
//!
//! # Safety
//!
//! All libav calls are unsafe. Each public function acquires and releases
//! libav resources in a balanced way; panics are not expected. Errors return
//! `FfmpegResult<T>`.
//!
//! # Failure model
//!
//! Every libav call that can fail is checked and turned into a contextual
//! [`FfmpegError`] (function name + codec + `av_err2str`). A job must never
//! end up "running but producing nothing" because a return value was ignored.
//!
//! # Diagnostics
//!
//! Both paths report to [`crate::diag`] with a `[hls:<job>]` prefix: input
//! probe results, encoder selection, every `avcodec_open2` /
//! `avformat_write_header` return code, the first segment written and roughly
//! every two seconds of progress.

#![allow(clippy::too_many_arguments)]

use std::ffi::{c_char, CStr, CString};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::sys::*;
use crate::{
    diag, AudioStream, EncoderInfo, FfmpegError, FfmpegResult, ProbeResult, ProgressState,
    SubtitleStream, VideoStream,
};

/// Time base used for millisecond content timestamps.
const MS_TB: AVRational = AVRational::new(1, 1000);

/// How often (~ms of wall time) to emit a progress diagnostic line.
const PROGRESS_LOG_EVERY_MS: u64 = 2_000;

// ── Probe ─────────────────────────────────────────────────────────────────────

/// Probe a media URL (file path or http URL) via in-process libav.
/// Returns codec/stream/duration metadata.
pub fn probe_url(url: &str) -> FfmpegResult<ProbeResult> {
    let url_c = CString::new(url).map_err(|e| FfmpegError::Probe(e.to_string()))?;
    unsafe {
        let mut fmt_ctx: *mut AVFormatContext = std::ptr::null_mut();
        let ret = avformat_open_input(
            &mut fmt_ctx,
            url_c.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
        );
        if ret < 0 {
            return Err(FfmpegError::Probe(format!(
                "avformat_open_input({}): {}",
                url,
                av_err2str(ret)
            )));
        }
        let ret = avformat_find_stream_info(fmt_ctx, std::ptr::null_mut());
        if ret < 0 {
            avformat_close_input(&mut fmt_ctx);
            return Err(FfmpegError::Probe(format!(
                "avformat_find_stream_info: {}",
                av_err2str(ret)
            )));
        }

        let nb = fmtctx_nb_streams(fmt_ctx) as usize;
        let streams_ptr = fmtctx_streams(fmt_ctx);
        let duration_raw = fmtctx_duration(fmt_ctx);
        let duration_secs = if duration_raw == AV_NOPTS_VALUE || duration_raw < 0 {
            0.0
        } else {
            duration_raw as f64 / 1_000_000.0 // AV_TIME_BASE = 1_000_000
        };

        let mut video = Vec::new();
        let mut audio = Vec::new();
        let mut subs = Vec::new();
        let mut fmt_name = String::new();

        // FFmpeg 7.1 LP64 AVFormatContext: av_class @0, iformat @8, oformat @16,
        // priv_data @24, pb @32. `iformat` is `*const AVInputFormat`; its first
        // field is `const char *name`.
        let iformat_ptr = *(fmt_ctx as *const u8).add(8).cast::<*const *const c_char>();
        if !iformat_ptr.is_null() {
            let name_ptr = *iformat_ptr; // first field of AVInputFormat = name
            if !name_ptr.is_null() {
                fmt_name = CStr::from_ptr(name_ptr).to_string_lossy().into_owned();
            }
        }

        for i in 0..nb {
            let st = *streams_ptr.add(i);
            if st.is_null() {
                continue;
            }
            let cp = (*st).codecpar;
            if cp.is_null() {
                continue;
            }

            let codec_type = (*cp).codec_type;
            let codec_id = (*cp).codec_id;
            let tb = (*st).time_base;

            // Resolve the codec name through libavcodec itself (avcodec_get_name),
            // which is stable across AVCodec struct layout changes.
            let codec_name = codec_display_name(codec_id);

            match codec_type {
                AVMEDIA_TYPE_VIDEO => {
                    let fps_raw = av_q2d(AVRational::new(
                        // framerate is not in codecpar; estimate from stream->r_frame_rate
                        // r_frame_rate is at +56 in AVStream (after duration@48, which is 8 bytes)
                        *(st as *const u8).add(56).cast::<i32>(),
                        *(st as *const u8).add(60).cast::<i32>(),
                    ));
                    video.push(VideoStream {
                        index: (*st).index as u32,
                        codec: codec_name,
                        width: (*cp).width as u32,
                        height: (*cp).height as u32,
                        fps: if fps_raw > 0.0 && fps_raw < 1000.0 {
                            fps_raw
                        } else {
                            0.0
                        },
                    });
                }
                AVMEDIA_TYPE_AUDIO => {
                    audio.push(AudioStream {
                        index: (*st).index as u32,
                        codec: codec_name,
                        channels: (*cp).ch_layout.nb_channels as u32,
                        sample_rate: (*cp).sample_rate as u32,
                        language: None, // TODO: read from stream metadata
                    });
                }
                3 => {
                    // AVMEDIA_TYPE_SUBTITLE = 3
                    subs.push(SubtitleStream {
                        index: (*st).index as u32,
                        codec: codec_name,
                        language: None,
                    });
                }
                _ => {}
            }
            let _ = tb; // suppress unused warning
        }

        avformat_close_input(&mut fmt_ctx);
        Ok(ProbeResult {
            duration_secs,
            video,
            audio,
            subtitles: subs,
            format_name: fmt_name,
        })
    }
}

// ── Diagnostics helpers ───────────────────────────────────────────────────────

/// Short `[hls:<job>]` prefix shared by every diagnostic line of one job.
fn job_tag(job_id: &str) -> String {
    let short: String = job_id.chars().take(6).collect();
    format!("[hls:{short}]")
}

/// Strip the token/query part of an input URL before logging it.
fn sanitize_url(url: &str) -> String {
    match url.split_once('?') {
        Some((head, _)) => format!("{head}?<redacted>"),
        None => url.to_string(),
    }
}

/// Codec name by id (`avcodec_get_name`), falling back to the numeric id.
fn codec_display_name(codec_id: AVCodecID) -> String {
    unsafe {
        let p = avcodec_get_name(codec_id);
        if p.is_null() {
            format!("id:{codec_id}")
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

/// Whether the linked libavcodec contains `h264_mediacodec`.
///
/// The packaged Android `libavcodec.so` only ships `libx264`, so this is
/// routinely `false` — the job falls back to software encoding and reports it.
fn h264_mediacodec_available() -> bool {
    unsafe { !avcodec_find_encoder_by_name(c"h264_mediacodec".as_ptr()).is_null() }
}

/// Build a contextual error for a failed libav call.
fn ffmpeg_err(what: impl std::fmt::Display, code: i32) -> FfmpegError {
    FfmpegError::Probe(format!("{what} failed: {} (rc={code})", av_err2str(code)))
}

unsafe fn log_streams(tag: &str, mode: &str, streams: *const *mut AVStream, nb: usize) {
    let mut video = String::new();
    let mut audio = String::new();
    for i in 0..nb {
        let st = *streams.add(i);
        if st.is_null() {
            continue;
        }
        let cp = (*st).codecpar;
        if cp.is_null() {
            continue;
        }
        let name = codec_display_name((*cp).codec_id);
        match (*cp).codec_type {
            AVMEDIA_TYPE_VIDEO => {
                video = format!(
                    "{name} (id={}) {}x{}",
                    (*cp).codec_id, (*cp).width, (*cp).height
                )
            }
            AVMEDIA_TYPE_AUDIO => {
                audio = format!(
                    "{name} (id={}) {}Hz {}ch",
                    (*cp).codec_id, (*cp).sample_rate, (*cp).ch_layout.nb_channels
                )
            }
            _ => {}
        }
        diag::log(format!(
            "{tag} {mode} input stream {i}: type={} codec={name} (id={})",
            (*cp).codec_type, (*cp).codec_id
        ));
    }
    diag::log(format!(
        "{tag} {mode} detected video={} audio={}",
        if video.is_empty() { "none" } else { &video },
        if audio.is_empty() { "none" } else { &audio },
    ));
}

/// Translates packet timestamps into monotonic millisecond progress.
#[derive(Default)]
struct ProgressTracker {
    /// First timestamp written, used as the zero point.
    origin_ms: Option<i64>,
}

impl ProgressTracker {
    fn record(&mut self, telemetry: &ProgressState, pts: i64, tb: AVRational) {
        if pts == AV_NOPTS_VALUE {
            return;
        }
        let ms = unsafe { av_rescale_q(pts, tb, MS_TB) };
        let origin = *self.origin_ms.get_or_insert(ms);
        telemetry.note_written(ms.saturating_sub(origin));
    }
}

/// Per-job diagnostics + progress bookkeeping shared by both paths.
struct JobCtx<'a> {
    tag: &'a str,
    telemetry: &'a ProgressState,
    segment_dir: &'a Path,
    tracker: ProgressTracker,
    first_segment_logged: bool,
    last_progress_log_ms: u64,
}

impl<'a> JobCtx<'a> {
    fn new(tag: &'a str, telemetry: &'a ProgressState, segment_dir: &'a Path) -> Self {
        Self {
            tag,
            telemetry,
            segment_dir,
            tracker: ProgressTracker::default(),
            first_segment_logged: false,
            last_progress_log_ms: 0,
        }
    }

    /// Record one muxed packet and emit throttled progress diagnostics.
    fn record(&mut self, pts: i64, tb: AVRational) {
        self.tracker.record(self.telemetry, pts, tb);
        self.log_progress();
        self.note_first_segment();
    }

    fn log_progress(&mut self) {
        let snap = self.telemetry.snapshot();
        if snap.wall_ms < self.last_progress_log_ms + PROGRESS_LOG_EVERY_MS {
            return;
        }
        self.last_progress_log_ms = snap.wall_ms;
        let speed = if snap.wall_ms == 0 {
            0.0
        } else {
            snap.content_ms as f64 / snap.wall_ms as f64
        };
        diag::log(format!(
            "{} progress content_ms={} wall_ms={} speed={speed:.2}x packets={}",
            self.tag, snap.content_ms, snap.wall_ms, snap.packets
        ));
    }

    fn note_first_segment(&mut self) {
        if self.first_segment_logged {
            return;
        }
        let found = std::fs::read_dir(self.segment_dir)
            .map(|entries| {
                entries.flatten().any(|e| {
                    e.file_name().to_string_lossy().ends_with(".ts")
                })
            })
            .unwrap_or(false);
        if found {
            self.first_segment_logged = true;
            diag::log(format!(
                "{} first segment written into {}",
                self.tag,
                self.segment_dir.display()
            ));
        }
    }
}

// ── HLS job ───────────────────────────────────────────────────────────────────

/// Run an HLS transcode/remux job, writing `index.m3u8` + numbered `.ts`
/// segments into `segment_dir`. Blocks the calling thread until EOF or
/// `cancel` is set to `true`.
///
/// `input_url` may be a local file path (`/path/to/file.mp4`) or an HTTP URL
/// (e.g. `http://127.0.0.1:{port}/s/{token}`).
pub fn run_hls_job(
    input_url: &str,
    preset: &wdmm_core::models::TranscodePreset,
    start_offset_secs: f64,
    segment_dir: &Path,
    cancel: Arc<AtomicBool>,
    telemetry: &ProgressState,
    job_id: &str,
) -> FfmpegResult<()> {
    telemetry.start();
    let tag = job_tag(job_id);
    diag::log(format!(
        "{tag} job start preset={} container={} remux_first={} video_codec={:?} audio_codec={} start_offset={start_offset_secs:.2} seg_dir={} input={}",
        preset.id,
        preset.container,
        preset.remux_first,
        preset.video_codec,
        preset.audio_codec,
        segment_dir.display(),
        sanitize_url(input_url),
    ));

    // Passthrough preset — no HLS wrapping, direct stream. Callers should not
    // call run_hls_job for passthrough; return immediately.
    if preset.container == "original" {
        diag::log(format!(
            "{tag} container=original → no HLS output (pass-through preset)"
        ));
        return Ok(());
    }

    // Decide: remux or transcode?
    let result = if preset.remux_first {
        match run_remux(
            input_url,
            preset,
            start_offset_secs,
            segment_dir,
            cancel.clone(),
            telemetry,
            &tag,
        ) {
            Ok(()) => Ok(()),
            Err(e) => {
                diag::log(format!("{tag} remux failed: {e}; falling back to transcode"));
                if cancel.load(Ordering::Relaxed) {
                    Ok(())
                } else {
                    run_transcode(
                        input_url,
                        preset,
                        start_offset_secs,
                        segment_dir,
                        cancel,
                        telemetry,
                        &tag,
                    )
                }
            }
        }
    } else {
        run_transcode(
            input_url,
            preset,
            start_offset_secs,
            segment_dir,
            cancel,
            telemetry,
            &tag,
        )
    };

    match &result {
        Ok(()) => {
            let snap = telemetry.snapshot();
            let speed = if snap.wall_ms == 0 {
                0.0
            } else {
                snap.content_ms as f64 / snap.wall_ms as f64
            };
            diag::log(format!(
                "{tag} job end ok content_ms={} wall_ms={} speed={speed:.2}x packets={}",
                snap.content_ms, snap.wall_ms, snap.packets
            ));
        }
        Err(e) => diag::log(format!("{tag} job end error: {e}")),
    }
    result
}

// ── Remux path ────────────────────────────────────────────────────────────────

fn run_remux(
    input_url: &str,
    preset: &wdmm_core::models::TranscodePreset,
    start_offset_secs: f64,
    segment_dir: &Path,
    cancel: Arc<AtomicBool>,
    telemetry: &ProgressState,
    tag: &str,
) -> FfmpegResult<()> {
    let url_c = CString::new(input_url).unwrap();
    let playlist = segment_dir.join("index.m3u8");
    let seg_pat = segment_dir.join("seg%d.ts");
    let playlist_c = CString::new(playlist.to_string_lossy().as_ref()).unwrap();
    let seg_pat_c = CString::new(seg_pat.to_string_lossy().as_ref()).unwrap();
    let hls_c = c"hls";
    let hw_available = h264_mediacodec_available();

    unsafe {
        // ── Open input ───────────────────────────────────────────────────────
        let mut in_ctx: *mut AVFormatContext = std::ptr::null_mut();
        let ret = avformat_open_input(
            &mut in_ctx,
            url_c.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
        );
        diag::log(format!("{tag} remux avformat_open_input rc={ret}"));
        if ret < 0 {
            return Err(ffmpeg_err("avformat_open_input (remux)", ret));
        }
        let ret = avformat_find_stream_info(in_ctx, std::ptr::null_mut());
        diag::log(format!("{tag} remux avformat_find_stream_info rc={ret}"));
        if ret < 0 {
            avformat_close_input(&mut in_ctx);
            return Err(ffmpeg_err("avformat_find_stream_info (remux)", ret));
        }

        // Optional seek
        if start_offset_secs > 0.01 {
            let ts = (start_offset_secs * 1_000_000.0) as i64;
            let ret = av_seek_frame(in_ctx, -1, ts, AVSEEK_FLAG_BACKWARD);
            diag::log(format!(
                "{tag} remux av_seek_frame({start_offset_secs:.2}s) rc={ret}"
            ));
        }

        // ── Validate remux compatibility ──────────────────────────────────
        let nb = fmtctx_nb_streams(in_ctx) as usize;
        let streams_ptr = fmtctx_streams(in_ctx);
        log_streams(tag, "remux", streams_ptr, nb);
        let mut has_incompatible = false;
        for i in 0..nb {
            let st = *streams_ptr.add(i);
            if st.is_null() {
                continue;
            }
            let cp = (*st).codecpar;
            if cp.is_null() {
                continue;
            }
            let ct = (*cp).codec_type;
            if (ct == AVMEDIA_TYPE_VIDEO || ct == AVMEDIA_TYPE_AUDIO)
                && !crate::preset::is_mpegts_copy_compatible((*cp).codec_id)
            {
                diag::log(format!(
                    "{tag} remux verdict: stream {i} codec={} (id={}) is not MPEG-TS copy compatible",
                    codec_display_name((*cp).codec_id),
                    (*cp).codec_id
                ));
                has_incompatible = true;
                break;
            }
        }
        if has_incompatible {
            avformat_close_input(&mut in_ctx);
            return Err(FfmpegError::Probe("codec not mpegts-copy-compatible".into()));
        }
        diag::log(format!("{tag} remux verdict: all streams MPEG-TS copy compatible"));

        telemetry.set_encoder(EncoderInfo {
            mode: "remux".into(),
            video: None,
            audio: Some("copy".into()),
            hw_requested: preset.video_hw_preferred,
            hw_available,
        });

        // ── Open HLS output ───────────────────────────────────────────────
        let mut out_ctx: *mut AVFormatContext = std::ptr::null_mut();
        let ret = avformat_alloc_output_context2(
            &mut out_ctx,
            std::ptr::null(),
            hls_c.as_ptr(),
            playlist_c.as_ptr(),
        );
        if ret < 0 || out_ctx.is_null() {
            avformat_close_input(&mut in_ctx);
            return Err(ffmpeg_err("avformat_alloc_output_context2 (remux)", ret));
        }

        // Copy streams
        for i in 0..nb {
            let in_st = *streams_ptr.add(i);
            if in_st.is_null() {
                continue;
            }
            let out_st = avformat_new_stream(out_ctx, std::ptr::null());
            if out_st.is_null() {
                avformat_close_input(&mut in_ctx);
                avformat_free_context(out_ctx);
                return Err(FfmpegError::Probe(
                    "avformat_new_stream returned null (remux)".into(),
                ));
            }
            let ret = avcodec_parameters_copy((*out_st).codecpar, (*in_st).codecpar);
            if ret < 0 {
                avformat_close_input(&mut in_ctx);
                avformat_free_context(out_ctx);
                return Err(ffmpeg_err("avcodec_parameters_copy (remux)", ret));
            }
            (*out_st).time_base = (*in_st).time_base;
        }

        // HLS muxer options. The HLS muxer is AVFMT_NOFILE: libav opens the
        // playlist itself from the output context's URL, so no `avio_open` and
        // no manual `AVFormatContext.pb` assignment is needed here.
        let mut dict: *mut AVDictionary = std::ptr::null_mut();
        let seg_secs = preset.hls_segment_secs.to_string();
        let seg_secs_c = CString::new(seg_secs.as_str()).unwrap();
        av_dict_set(&mut dict, c"hls_time".as_ptr(), seg_secs_c.as_ptr(), 0);
        av_dict_set(&mut dict, c"hls_list_size".as_ptr(), c"0".as_ptr(), 0);
        av_dict_set(&mut dict, c"hls_flags".as_ptr(), c"split_by_time".as_ptr(), 0);
        av_dict_set(
            &mut dict,
            c"hls_segment_filename".as_ptr(),
            seg_pat_c.as_ptr(),
            0,
        );

        let ret = avformat_write_header(out_ctx, &mut dict);
        diag::log(format!("{tag} remux avformat_write_header rc={ret}"));
        av_dict_free(&mut dict);
        if ret < 0 {
            avformat_close_input(&mut in_ctx);
            avformat_free_context(out_ctx);
            return Err(ffmpeg_err("avformat_write_header (remux)", ret));
        }

        // ── Read/write loop ───────────────────────────────────────────────
        let mut pkt = av_packet_alloc();
        if pkt.is_null() {
            avformat_close_input(&mut in_ctx);
            avformat_free_context(out_ctx);
            return Err(FfmpegError::Probe(
                "av_packet_alloc returned null (remux)".into(),
            ));
        }

        let mut job = JobCtx::new(tag, telemetry, segment_dir);
        let mut write_error: Option<FfmpegError> = None;

        loop {
            if cancel.load(Ordering::Relaxed) {
                break;
            }

            let ret = av_read_frame(in_ctx, pkt);
            if ret < 0 {
                if ret != AVERROR_EOF {
                    diag::log(format!(
                        "{tag} remux av_read_frame rc={ret}: {}",
                        av_err2str(ret)
                    ));
                }
                break;
            }

            let in_st_idx = (*pkt).stream_index as usize;
            if in_st_idx >= nb {
                av_packet_unref(pkt);
                continue;
            }
            let in_st = *streams_ptr.add(in_st_idx);
            let out_st = *fmtctx_streams(out_ctx).add(in_st_idx);
            if in_st.is_null() || out_st.is_null() {
                av_packet_unref(pkt);
                continue;
            }

            av_packet_rescale_ts(pkt, (*in_st).time_base, (*out_st).time_base);
            (*pkt).pos = -1;

            // Capture before the write consumes the packet.
            let pts = (*pkt).pts;
            let tb = (*out_st).time_base;
            let ret = av_interleaved_write_frame(out_ctx, pkt);
            if ret < 0 {
                write_error = Some(ffmpeg_err("av_interleaved_write_frame (remux)", ret));
                av_packet_unref(pkt);
                break;
            }
            job.record(pts, tb);
            av_packet_unref(pkt);
        }

        av_packet_free(&mut pkt);
        let ret = av_write_trailer(out_ctx);
        if ret < 0 {
            diag::log(format!("{tag} remux av_write_trailer rc={ret}"));
        }
        avformat_free_context(out_ctx);
        avformat_close_input(&mut in_ctx);

        if let Some(e) = write_error {
            return Err(e);
        }
    }
    Ok(())
}

// ── Transcode path ────────────────────────────────────────────────────────────

fn run_transcode(
    input_url: &str,
    preset: &wdmm_core::models::TranscodePreset,
    start_offset_secs: f64,
    segment_dir: &Path,
    cancel: Arc<AtomicBool>,
    telemetry: &ProgressState,
    tag: &str,
) -> FfmpegResult<()> {
    let url_c = CString::new(input_url).unwrap();
    let playlist = segment_dir.join("index.m3u8");
    let seg_pat = segment_dir.join("seg%d.ts");
    let playlist_c = CString::new(playlist.to_string_lossy().as_ref()).unwrap();
    let seg_pat_c = CString::new(seg_pat.to_string_lossy().as_ref()).unwrap();
    let hls_c = c"hls";
    let hw_available = h264_mediacodec_available();

    unsafe {
        // ── Open + seek input ─────────────────────────────────────────────
        let mut in_ctx: *mut AVFormatContext = std::ptr::null_mut();
        let ret = avformat_open_input(
            &mut in_ctx,
            url_c.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
        );
        diag::log(format!("{tag} transcode avformat_open_input rc={ret}"));
        if ret < 0 {
            return Err(ffmpeg_err("avformat_open_input (transcode)", ret));
        }
        let ret = avformat_find_stream_info(in_ctx, std::ptr::null_mut());
        diag::log(format!("{tag} transcode avformat_find_stream_info rc={ret}"));
        if ret < 0 {
            avformat_close_input(&mut in_ctx);
            return Err(ffmpeg_err("avformat_find_stream_info (transcode)", ret));
        }
        if start_offset_secs > 0.01 {
            let ts = (start_offset_secs * 1_000_000.0) as i64;
            let ret = av_seek_frame(in_ctx, -1, ts, AVSEEK_FLAG_BACKWARD);
            diag::log(format!(
                "{tag} transcode av_seek_frame({start_offset_secs:.2}s) rc={ret}"
            ));
        }

        // ── Find best streams ─────────────────────────────────────────────
        let want_video = preset.video_codec.is_some();
        let mut v_dec_codec: *const AVCodec = std::ptr::null();
        let mut a_dec_codec: *const AVCodec = std::ptr::null();
        let v_idx = if want_video {
            av_find_best_stream(in_ctx, AVMEDIA_TYPE_VIDEO, -1, -1, &mut v_dec_codec, 0)
        } else {
            -1
        };
        let a_idx = av_find_best_stream(in_ctx, AVMEDIA_TYPE_AUDIO, -1, -1, &mut a_dec_codec, 0);

        let nb = fmtctx_nb_streams(in_ctx) as usize;
        let streams_ptr = fmtctx_streams(in_ctx);
        diag::log(format!(
            "{tag} transcode stream selection: video_idx={v_idx} audio_idx={a_idx} want_video={want_video}"
        ));
        log_streams(tag, "transcode", streams_ptr, nb);

        if want_video && (v_idx < 0 || v_dec_codec.is_null()) {
            diag::log(format!(
                "{tag} transcode failed: preset requires video but no decodable video stream was found"
            ));
            avformat_close_input(&mut in_ctx);
            return Err(FfmpegError::Probe(
                "no decodable video stream for transcode".into(),
            ));
        }

        // ── Open decoders ─────────────────────────────────────────────────
        let mut v_dec_ctx: *mut AVCodecContext = std::ptr::null_mut();
        let mut a_dec_ctx: *mut AVCodecContext = std::ptr::null_mut();

        if v_idx >= 0 && !v_dec_codec.is_null() {
            let in_st = *streams_ptr.add(v_idx as usize);
            let dec_name = codec_display_name((*(*in_st).codecpar).codec_id);
            v_dec_ctx = avcodec_alloc_context3(v_dec_codec);
            if v_dec_ctx.is_null() {
                avformat_close_input(&mut in_ctx);
                return Err(FfmpegError::Probe(
                    "avcodec_alloc_context3(video decoder) returned null".into(),
                ));
            }
            let ret = avcodec_parameters_to_context(v_dec_ctx, (*in_st).codecpar);
            if ret < 0 {
                cleanup_transcode(in_ctx, std::ptr::null_mut(), v_dec_ctx, a_dec_ctx, std::ptr::null_mut(), std::ptr::null_mut());
                return Err(ffmpeg_err("avcodec_parameters_to_context(video decoder)", ret));
            }
            let ret = avcodec_open2(v_dec_ctx, v_dec_codec, std::ptr::null_mut());
            diag::log(format!(
                "{tag} transcode avcodec_open2(video decoder {dec_name}) rc={ret}"
            ));
            if ret < 0 {
                cleanup_transcode(in_ctx, std::ptr::null_mut(), v_dec_ctx, a_dec_ctx, std::ptr::null_mut(), std::ptr::null_mut());
                return Err(ffmpeg_err(
                    format!("avcodec_open2(video decoder {dec_name})"),
                    ret,
                ));
            }
        }
        if a_idx >= 0 && !a_dec_codec.is_null() {
            let in_st = *streams_ptr.add(a_idx as usize);
            let dec_name = codec_display_name((*(*in_st).codecpar).codec_id);
            a_dec_ctx = avcodec_alloc_context3(a_dec_codec);
            if a_dec_ctx.is_null() {
                cleanup_transcode(in_ctx, std::ptr::null_mut(), v_dec_ctx, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
                return Err(FfmpegError::Probe(
                    "avcodec_alloc_context3(audio decoder) returned null".into(),
                ));
            }
            let ret = avcodec_parameters_to_context(a_dec_ctx, (*in_st).codecpar);
            if ret < 0 {
                cleanup_transcode(in_ctx, std::ptr::null_mut(), v_dec_ctx, a_dec_ctx, std::ptr::null_mut(), std::ptr::null_mut());
                return Err(ffmpeg_err("avcodec_parameters_to_context(audio decoder)", ret));
            }
            let ret = avcodec_open2(a_dec_ctx, a_dec_codec, std::ptr::null_mut());
            diag::log(format!(
                "{tag} transcode avcodec_open2(audio decoder {dec_name}) rc={ret}"
            ));
            if ret < 0 {
                cleanup_transcode(in_ctx, std::ptr::null_mut(), v_dec_ctx, a_dec_ctx, std::ptr::null_mut(), std::ptr::null_mut());
                return Err(ffmpeg_err(
                    format!("avcodec_open2(audio decoder {dec_name})"),
                    ret,
                ));
            }
        }

        // ── Select encoders ───────────────────────────────────────────────
        let mut v_enc_codec: *const AVCodec = std::ptr::null();
        let mut v_enc_name: Option<String> = None;
        if want_video {
            if preset.video_hw_preferred {
                let hw = avcodec_find_encoder_by_name(c"h264_mediacodec".as_ptr());
                if hw.is_null() {
                    diag::log(format!(
                        "{tag} transcode encoder: requested h264_mediacodec not found (hw_available=false) → fallback libx264"
                    ));
                    telemetry.warn("h264_mediacodec 不可用，已回退 libx264");
                } else {
                    v_enc_codec = hw;
                    v_enc_name = Some("h264_mediacodec".into());
                }
            }
            if v_enc_codec.is_null() {
                v_enc_codec = avcodec_find_encoder_by_name(c"libx264".as_ptr());
                if !v_enc_codec.is_null() {
                    v_enc_name = Some("libx264".into());
                }
            }
            if v_enc_codec.is_null() {
                v_enc_codec = avcodec_find_encoder(AV_CODEC_ID_H264);
                if !v_enc_codec.is_null() {
                    v_enc_name = Some("h264".into());
                }
            }
            if v_enc_codec.is_null() {
                diag::log(format!(
                    "{tag} transcode failed: no H.264 encoder available in libavcodec"
                ));
                cleanup_transcode(in_ctx, std::ptr::null_mut(), v_dec_ctx, a_dec_ctx, std::ptr::null_mut(), std::ptr::null_mut());
                return Err(FfmpegError::Probe(
                    "no H.264 encoder available (libx264 missing)".into(),
                ));
            }
            diag::log(format!(
                "{tag} transcode encoder selected video={} hw_requested={} hw_available={hw_available}",
                v_enc_name.as_deref().unwrap_or("?"),
                preset.video_hw_preferred
            ));
        }

        let a_codec_id = match preset.audio_codec.as_str() {
            "mp3" => AV_CODEC_ID_MP3,
            "copy" => 0u32,
            _ => AV_CODEC_ID_AAC,
        };
        let audio_stream_present = a_idx >= 0 && !a_dec_codec.is_null();
        let mut a_enc_codec: *const AVCodec = std::ptr::null();
        if audio_stream_present && a_codec_id > 0 {
            a_enc_codec = avcodec_find_encoder(a_codec_id);
            if a_enc_codec.is_null() {
                let name = codec_display_name(a_codec_id);
                diag::log(format!(
                    "{tag} transcode failed: no encoder for audio codec {name} (id={a_codec_id})"
                ));
                cleanup_transcode(in_ctx, std::ptr::null_mut(), v_dec_ctx, a_dec_ctx, std::ptr::null_mut(), std::ptr::null_mut());
                return Err(FfmpegError::Probe(format!(
                    "no encoder for audio codec {name} (id={a_codec_id})"
                )));
            }
            diag::log(format!(
                "{tag} transcode encoder selected audio={}",
                preset.audio_codec
            ));
        } else if audio_stream_present && a_codec_id == 0 {
            diag::log(format!(
                "{tag} transcode WARNING: audio stream copy requested, but the transcode path only muxes re-encoded audio → audio dropped"
            ));
            telemetry.warn("转码路径暂不支持音频直拷，音频已丢弃");
        }

        telemetry.set_encoder(EncoderInfo {
            mode: "transcode".into(),
            video: v_enc_name.clone(),
            audio: if audio_stream_present {
                Some(if a_codec_id == 0 {
                    "copy".to_string()
                } else {
                    preset.audio_codec.clone()
                })
            } else {
                None
            },
            hw_requested: preset.video_hw_preferred,
            hw_available,
        });

        // ── Open encoders ─────────────────────────────────────────────────
        let mut v_enc_ctx: *mut AVCodecContext = std::ptr::null_mut();
        if want_video {
            v_enc_ctx = avcodec_alloc_context3(v_enc_codec);
            if v_enc_ctx.is_null() {
                cleanup_transcode(in_ctx, std::ptr::null_mut(), v_dec_ctx, a_dec_ctx, std::ptr::null_mut(), std::ptr::null_mut());
                return Err(FfmpegError::Probe(
                    "avcodec_alloc_context3(video encoder) returned null".into(),
                ));
            }
            let in_st = *streams_ptr.add(v_idx as usize);
            let src_w = (*(*in_st).codecpar).width;
            let src_h = (*(*in_st).codecpar).height;
            let (out_w, out_h) = fit_dimensions(
                src_w,
                src_h,
                preset.video_max_w.map(|v| v as i32),
                preset.video_max_h.map(|v| v as i32),
            );
            codctx_set_width(v_enc_ctx, out_w);
            codctx_set_height(v_enc_ctx, out_h);
            codctx_set_pix_fmt(v_enc_ctx, AV_PIX_FMT_YUV420P);
            codctx_set_time_base(v_enc_ctx, AVRational::new(1, 90000));
            codctx_set_gop_size(v_enc_ctx, 12);
            codctx_set_max_b_frames(v_enc_ctx, 2);
            if let Some(br) = preset.video_max_bitrate {
                codctx_set_bit_rate(v_enc_ctx, br as i64);
            }
            let ret = avcodec_open2(v_enc_ctx, v_enc_codec, std::ptr::null_mut());
            diag::log(format!(
                "{tag} transcode avcodec_open2(video encoder {}) rc={ret}",
                v_enc_name.as_deref().unwrap_or("?")
            ));
            if ret < 0 {
                let name = v_enc_name.unwrap_or_else(|| "h264".into());
                cleanup_transcode(in_ctx, std::ptr::null_mut(), v_dec_ctx, a_dec_ctx, v_enc_ctx, std::ptr::null_mut());
                return Err(ffmpeg_err(format!("avcodec_open2(video encoder {name})"), ret));
            }
        }

        let mut a_enc_ctx: *mut AVCodecContext = std::ptr::null_mut();
        let audio_sample_rate = preset.audio_sample_rate.unwrap_or(48_000) as i32;
        if !a_enc_codec.is_null() && !a_dec_ctx.is_null() {
            a_enc_ctx = avcodec_alloc_context3(a_enc_codec);
            if a_enc_ctx.is_null() {
                cleanup_transcode(in_ctx, std::ptr::null_mut(), v_dec_ctx, a_dec_ctx, v_enc_ctx, std::ptr::null_mut());
                return Err(FfmpegError::Probe(
                    "avcodec_alloc_context3(audio encoder) returned null".into(),
                ));
            }
            codctx_set_sample_rate(a_enc_ctx, audio_sample_rate);
            // Set the time base *before* opening: encoders derive packet
            // timestamps from it, and the value must match the output stream's.
            codctx_set_time_base(a_enc_ctx, AVRational::new(1, audio_sample_rate));
            codctx_set_sample_fmt(a_enc_ctx, AV_SAMPLE_FMT_FLTP);
            let layout = if preset.audio_channels == 1 {
                AVChannelLayout::mono()
            } else {
                AVChannelLayout::stereo()
            };
            codctx_set_ch_layout(a_enc_ctx, layout);
            codctx_set_bit_rate(a_enc_ctx, preset.audio_bitrate as i64);
            let ret = avcodec_open2(a_enc_ctx, a_enc_codec, std::ptr::null_mut());
            diag::log(format!(
                "{tag} transcode avcodec_open2(audio encoder {}) rc={ret}",
                preset.audio_codec
            ));
            if ret < 0 {
                cleanup_transcode(in_ctx, std::ptr::null_mut(), v_dec_ctx, a_dec_ctx, v_enc_ctx, a_enc_ctx);
                return Err(ffmpeg_err(
                    format!("avcodec_open2(audio encoder {})", preset.audio_codec),
                    ret,
                ));
            }
        }

        // ── Open HLS output ───────────────────────────────────────────────
        let mut out_ctx: *mut AVFormatContext = std::ptr::null_mut();
        let ret = avformat_alloc_output_context2(
            &mut out_ctx,
            std::ptr::null(),
            hls_c.as_ptr(),
            playlist_c.as_ptr(),
        );
        if ret < 0 || out_ctx.is_null() {
            cleanup_transcode(in_ctx, std::ptr::null_mut(), v_dec_ctx, a_dec_ctx, v_enc_ctx, a_enc_ctx);
            return Err(ffmpeg_err(
                "avformat_alloc_output_context2 (transcode)",
                ret,
            ));
        }

        // Add output streams
        let mut out_v_idx: i32 = -1;
        let mut out_a_idx: i32 = -1;
        if !v_enc_ctx.is_null() {
            let out_st = avformat_new_stream(out_ctx, v_enc_codec);
            if out_st.is_null() {
                cleanup_transcode(in_ctx, out_ctx, v_dec_ctx, a_dec_ctx, v_enc_ctx, a_enc_ctx);
                return Err(FfmpegError::Probe(
                    "avformat_new_stream(video) returned null".into(),
                ));
            }
            let ret = avcodec_parameters_from_context((*out_st).codecpar, v_enc_ctx);
            if ret < 0 {
                cleanup_transcode(in_ctx, out_ctx, v_dec_ctx, a_dec_ctx, v_enc_ctx, a_enc_ctx);
                return Err(ffmpeg_err("avcodec_parameters_from_context(video)", ret));
            }
            (*out_st).time_base = codctx_time_base(v_enc_ctx);
            out_v_idx = (*out_st).index;
        }
        if !a_enc_ctx.is_null() {
            let out_st = avformat_new_stream(out_ctx, a_enc_codec);
            if out_st.is_null() {
                cleanup_transcode(in_ctx, out_ctx, v_dec_ctx, a_dec_ctx, v_enc_ctx, a_enc_ctx);
                return Err(FfmpegError::Probe(
                    "avformat_new_stream(audio) returned null".into(),
                ));
            }
            let ret = avcodec_parameters_from_context((*out_st).codecpar, a_enc_ctx);
            if ret < 0 {
                cleanup_transcode(in_ctx, out_ctx, v_dec_ctx, a_dec_ctx, v_enc_ctx, a_enc_ctx);
                return Err(ffmpeg_err("avcodec_parameters_from_context(audio)", ret));
            }
            (*out_st).time_base = codctx_time_base(a_enc_ctx);
            out_a_idx = (*out_st).index;
        }

        // HLS muxer options. The HLS muxer is AVFMT_NOFILE: libav opens the
        // playlist itself from the output context's URL, so no `avio_open` and
        // no manual `AVFormatContext.pb` assignment is needed here.
        let mut dict: *mut AVDictionary = std::ptr::null_mut();
        let seg_secs = preset.hls_segment_secs.to_string();
        let seg_secs_c = CString::new(seg_secs.as_str()).unwrap();
        av_dict_set(&mut dict, c"hls_time".as_ptr(), seg_secs_c.as_ptr(), 0);
        av_dict_set(&mut dict, c"hls_list_size".as_ptr(), c"0".as_ptr(), 0);
        av_dict_set(
            &mut dict,
            c"hls_segment_filename".as_ptr(),
            seg_pat_c.as_ptr(),
            0,
        );

        let ret = avformat_write_header(out_ctx, &mut dict);
        diag::log(format!("{tag} transcode avformat_write_header rc={ret}"));
        av_dict_free(&mut dict);
        if ret < 0 {
            cleanup_transcode(in_ctx, out_ctx, v_dec_ctx, a_dec_ctx, v_enc_ctx, a_enc_ctx);
            return Err(ffmpeg_err("avformat_write_header (transcode)", ret));
        }

        // ── Allocate loop state ───────────────────────────────────────────
        let mut in_pkt = av_packet_alloc();
        let mut out_pkt = av_packet_alloc();
        let mut dec_frame = av_frame_alloc();
        if in_pkt.is_null() || out_pkt.is_null() || dec_frame.is_null() {
            if !in_pkt.is_null() {
                av_packet_free(&mut in_pkt);
            }
            if !out_pkt.is_null() {
                av_packet_free(&mut out_pkt);
            }
            if !dec_frame.is_null() {
                av_frame_free(&mut dec_frame);
            }
            cleanup_transcode(in_ctx, out_ctx, v_dec_ctx, a_dec_ctx, v_enc_ctx, a_enc_ctx);
            return Err(FfmpegError::Probe(
                "av_packet_alloc/av_frame_alloc returned null (transcode)".into(),
            ));
        }

        let mut sws: *mut SwsContext = std::ptr::null_mut();
        let mut swr: *mut SwrContext = std::ptr::null_mut();
        let mut enc_vframe: *mut AVFrame = std::ptr::null_mut();
        let mut video_out_dims = (0i32, 0i32);
        if !v_enc_ctx.is_null() {
            let in_st = *streams_ptr.add(v_idx as usize);
            let cp = (*in_st).codecpar;
            video_out_dims = fit_dimensions(
                (*cp).width,
                (*cp).height,
                preset.video_max_w.map(|v| v as i32),
                preset.video_max_h.map(|v| v as i32),
            );
            enc_vframe = av_frame_alloc();
            if enc_vframe.is_null() {
                free_loop_state(&mut in_pkt, &mut out_pkt, &mut dec_frame, &mut enc_vframe, &mut sws, &mut swr);
                cleanup_transcode(in_ctx, out_ctx, v_dec_ctx, a_dec_ctx, v_enc_ctx, a_enc_ctx);
                return Err(FfmpegError::Probe(
                    "av_frame_alloc(encoder frame) returned null".into(),
                ));
            }
            frame_set_width(enc_vframe, video_out_dims.0);
            frame_set_height(enc_vframe, video_out_dims.1);
            frame_set_format(enc_vframe, AV_PIX_FMT_YUV420P);
            let ret = av_frame_get_buffer(enc_vframe, 32);
            if ret < 0 {
                free_loop_state(&mut in_pkt, &mut out_pkt, &mut dec_frame, &mut enc_vframe, &mut sws, &mut swr);
                cleanup_transcode(in_ctx, out_ctx, v_dec_ctx, a_dec_ctx, v_enc_ctx, a_enc_ctx);
                return Err(ffmpeg_err("av_frame_get_buffer(encoder frame)", ret));
            }
        }

        // ── Decode/encode loop ────────────────────────────────────────────
        let mut job = JobCtx::new(tag, telemetry, segment_dir);
        let mut fatal: Option<FfmpegError> = None;
        let mut decode_errors_logged = 0u32;

        'decode: loop {
            if cancel.load(Ordering::Relaxed) {
                break;
            }

            let ret = av_read_frame(in_ctx, in_pkt);
            if ret < 0 {
                if ret != AVERROR_EOF {
                    diag::log(format!(
                        "{tag} transcode av_read_frame rc={ret}: {} (stopping)",
                        av_err2str(ret)
                    ));
                }
                break;
            }

            let stream_idx = (*in_pkt).stream_index;

            if stream_idx == v_idx && !v_dec_ctx.is_null() {
                let r = avcodec_send_packet(v_dec_ctx, in_pkt);
                if r < 0 && r != AVERROR_EAGAIN && decode_errors_logged < 3 {
                    decode_errors_logged += 1;
                    diag::log(format!(
                        "{tag} transcode avcodec_send_packet(video) rc={r}: {}",
                        av_err2str(r)
                    ));
                }
                loop {
                    let r = avcodec_receive_frame(v_dec_ctx, dec_frame);
                    if r == AVERROR_EAGAIN || r == AVERROR_EOF {
                        break;
                    }
                    if r < 0 {
                        if decode_errors_logged < 3 {
                            decode_errors_logged += 1;
                            diag::log(format!(
                                "{tag} transcode avcodec_receive_frame(video) rc={r}: {}",
                                av_err2str(r)
                            ));
                        }
                        break;
                    }
                    if !enc_vframe.is_null() && !v_enc_ctx.is_null() {
                        let sw = frame_width(dec_frame);
                        let sh = frame_height(dec_frame);
                        let dw = video_out_dims.0;
                        let dh = video_out_dims.1;
                        if sws.is_null() {
                            sws = sws_getContext(
                                sw,
                                sh,
                                frame_format(dec_frame),
                                dw,
                                dh,
                                AV_PIX_FMT_YUV420P,
                                2,
                                std::ptr::null(),
                                std::ptr::null(),
                                std::ptr::null(),
                            );
                            if sws.is_null() {
                                fatal = Some(FfmpegError::Probe(
                                    "sws_getContext returned null".into(),
                                ));
                                av_frame_unref(dec_frame);
                                break 'decode;
                            }
                        }
                        let src_d = [
                            frame_data(dec_frame, 0) as *const u8,
                            frame_data(dec_frame, 1) as *const u8,
                            frame_data(dec_frame, 2) as *const u8,
                            std::ptr::null(),
                        ];
                        let src_s = [
                            frame_linesize(dec_frame, 0),
                            frame_linesize(dec_frame, 1),
                            frame_linesize(dec_frame, 2),
                            0,
                        ];
                        let dst_d = [
                            frame_data(enc_vframe, 0),
                            frame_data(enc_vframe, 1),
                            frame_data(enc_vframe, 2),
                            std::ptr::null_mut(),
                        ];
                        let dst_s = [
                            frame_linesize(enc_vframe, 0),
                            frame_linesize(enc_vframe, 1),
                            frame_linesize(enc_vframe, 2),
                            0,
                        ];
                        let scaled = sws_scale(
                            sws,
                            src_d.as_ptr(),
                            src_s.as_ptr(),
                            0,
                            sh,
                            dst_d.as_ptr(),
                            dst_s.as_ptr(),
                        );
                        if scaled <= 0 {
                            if decode_errors_logged < 3 {
                                decode_errors_logged += 1;
                                diag::log(format!(
                                    "{tag} transcode sws_scale returned {scaled} for {sw}x{sh} → {dw}x{dh}"
                                ));
                            }
                            av_frame_unref(dec_frame);
                            continue;
                        }
                        frame_set_pts(
                            enc_vframe,
                            av_rescale_q(
                                frame_pts(dec_frame),
                                codctx_time_base(v_dec_ctx),
                                codctx_time_base(v_enc_ctx),
                            ),
                        );
                        if let Err(e) = encode_and_write(
                            v_enc_ctx,
                            enc_vframe,
                            out_pkt,
                            out_ctx,
                            out_v_idx,
                            codctx_time_base(v_enc_ctx),
                            &mut job,
                        ) {
                            fatal = Some(e);
                            av_frame_unref(dec_frame);
                            break 'decode;
                        }
                    }
                    av_frame_unref(dec_frame);
                }
            } else if stream_idx == a_idx && !a_dec_ctx.is_null() {
                let r = avcodec_send_packet(a_dec_ctx, in_pkt);
                if r < 0 && r != AVERROR_EAGAIN && decode_errors_logged < 3 {
                    decode_errors_logged += 1;
                    diag::log(format!(
                        "{tag} transcode avcodec_send_packet(audio) rc={r}: {}",
                        av_err2str(r)
                    ));
                }
                loop {
                    let r = avcodec_receive_frame(a_dec_ctx, dec_frame);
                    if r == AVERROR_EAGAIN || r == AVERROR_EOF {
                        break;
                    }
                    if r < 0 {
                        if decode_errors_logged < 3 {
                            decode_errors_logged += 1;
                            diag::log(format!(
                                "{tag} transcode avcodec_receive_frame(audio) rc={r}: {}",
                                av_err2str(r)
                            ));
                        }
                        break;
                    }

                    if !a_enc_ctx.is_null() {
                        // Resample if needed
                        if swr.is_null() {
                            let src_layout = frame_ch_layout(dec_frame);
                            let dst_layout = if preset.audio_channels == 1 {
                                AVChannelLayout::mono()
                            } else {
                                AVChannelLayout::stereo()
                            };
                            let src_sr = frame_sample_rate(dec_frame);
                            let dst_sr = audio_sample_rate;
                            let ret = swr_alloc_set_opts2(
                                &mut swr,
                                &dst_layout,
                                AV_SAMPLE_FMT_FLTP,
                                dst_sr,
                                src_layout,
                                frame_format(dec_frame) as AVSampleFormat,
                                src_sr,
                                0,
                                std::ptr::null_mut(),
                            );
                            if ret < 0 || swr.is_null() {
                                fatal = Some(ffmpeg_err("swr_alloc_set_opts2", ret));
                                av_frame_unref(dec_frame);
                                break 'decode;
                            }
                            let ret = swr_init(swr);
                            if ret < 0 {
                                fatal = Some(ffmpeg_err("swr_init", ret));
                                av_frame_unref(dec_frame);
                                break 'decode;
                            }
                        }
                        let mut resamp = av_frame_alloc();
                        if resamp.is_null() {
                            fatal = Some(FfmpegError::Probe(
                                "av_frame_alloc(resampled) returned null".into(),
                            ));
                            av_frame_unref(dec_frame);
                            break 'decode;
                        }
                        let ret = swr_convert_frame(swr, resamp, dec_frame);
                        if ret < 0 {
                            av_frame_free(&mut resamp);
                            fatal = Some(ffmpeg_err("swr_convert_frame", ret));
                            av_frame_unref(dec_frame);
                            break 'decode;
                        }
                        frame_set_pts(
                            resamp,
                            av_rescale_q(
                                frame_pts(resamp),
                                codctx_time_base(a_dec_ctx),
                                codctx_time_base(a_enc_ctx),
                            ),
                        );
                        if let Err(e) = encode_and_write(
                            a_enc_ctx,
                            resamp,
                            out_pkt,
                            out_ctx,
                            out_a_idx,
                            codctx_time_base(a_enc_ctx),
                            &mut job,
                        ) {
                            av_frame_free(&mut resamp);
                            fatal = Some(e);
                            av_frame_unref(dec_frame);
                            break 'decode;
                        }
                        av_frame_free(&mut resamp);
                    }
                    av_frame_unref(dec_frame);
                }
            }
            av_packet_unref(in_pkt);
        }

        // Flush encoders (only when the loop did not already fail).
        if fatal.is_none() && !v_enc_ctx.is_null() {
            if let Err(e) = encode_and_write(
                v_enc_ctx,
                std::ptr::null(),
                out_pkt,
                out_ctx,
                out_v_idx,
                codctx_time_base(v_enc_ctx),
                &mut job,
            ) {
                fatal = Some(e);
            }
        }
        if fatal.is_none() && !a_enc_ctx.is_null() {
            if let Err(e) = encode_and_write(
                a_enc_ctx,
                std::ptr::null(),
                out_pkt,
                out_ctx,
                out_a_idx,
                codctx_time_base(a_enc_ctx),
                &mut job,
            ) {
                fatal = Some(e);
            }
        }

        // ── Cleanup ───────────────────────────────────────────────────────
        if !sws.is_null() {
            sws_freeContext(sws);
        }
        if !swr.is_null() {
            swr_free(&mut swr);
        }
        if !enc_vframe.is_null() {
            av_frame_free(&mut enc_vframe);
        }
        av_frame_free(&mut dec_frame);
        av_packet_free(&mut in_pkt);
        av_packet_free(&mut out_pkt);

        let ret = av_write_trailer(out_ctx);
        if ret < 0 {
            diag::log(format!("{tag} transcode av_write_trailer rc={ret}"));
        }
        cleanup_transcode(in_ctx, out_ctx, v_dec_ctx, a_dec_ctx, v_enc_ctx, a_enc_ctx);

        if let Some(e) = fatal {
            return Err(e);
        }
    }
    Ok(())
}

/// Send one frame to an encoder and write any output packets to the muxer.
/// Pass `frame = null` to flush the encoder.
unsafe fn encode_and_write(
    enc_ctx: *mut AVCodecContext,
    frame: *const AVFrame,
    out_pkt: *mut AVPacket,
    out_ctx: *mut AVFormatContext,
    out_stream_idx: i32,
    enc_tb: AVRational,
    job: &mut JobCtx<'_>,
) -> FfmpegResult<()> {
    let ret = avcodec_send_frame(enc_ctx, frame);
    if ret < 0 {
        return Err(ffmpeg_err("avcodec_send_frame", ret));
    }
    loop {
        let r = avcodec_receive_packet(enc_ctx, out_pkt);
        if r == AVERROR_EAGAIN || r == AVERROR_EOF {
            break;
        }
        if r < 0 {
            return Err(ffmpeg_err("avcodec_receive_packet", r));
        }
        let nb = fmtctx_nb_streams(out_ctx) as usize;
        let sps = fmtctx_streams(out_ctx);
        if out_stream_idx < 0 || (out_stream_idx as usize) >= nb {
            av_packet_unref(out_pkt);
            return Err(FfmpegError::Probe(format!(
                "invalid output stream index {out_stream_idx} (muxer has {nb} streams)"
            )));
        }
        let out_st = *sps.add(out_stream_idx as usize);
        if out_st.is_null() {
            av_packet_unref(out_pkt);
            return Err(FfmpegError::Probe(
                "output stream pointer is null (transcode)".into(),
            ));
        }
        (*out_pkt).stream_index = out_stream_idx;
        av_packet_rescale_ts(out_pkt, enc_tb, (*out_st).time_base);

        // Capture before the write consumes the packet.
        let pts = (*out_pkt).pts;
        let tb = (*out_st).time_base;
        let ret = av_interleaved_write_frame(out_ctx, out_pkt);
        if ret < 0 {
            return Err(ffmpeg_err("av_interleaved_write_frame (transcode)", ret));
        }
        job.record(pts, tb);
        av_packet_unref(out_pkt);
    }
    Ok(())
}

/// Free the loop-owned libav objects (used on early allocation failures).
unsafe fn free_loop_state(
    in_pkt: &mut *mut AVPacket,
    out_pkt: &mut *mut AVPacket,
    dec_frame: &mut *mut AVFrame,
    enc_vframe: &mut *mut AVFrame,
    sws: &mut *mut SwsContext,
    swr: &mut *mut SwrContext,
) {
    if !in_pkt.is_null() {
        av_packet_free(in_pkt);
    }
    if !out_pkt.is_null() {
        av_packet_free(out_pkt);
    }
    if !dec_frame.is_null() {
        av_frame_free(dec_frame);
    }
    if !enc_vframe.is_null() {
        av_frame_free(enc_vframe);
    }
    if !sws.is_null() {
        sws_freeContext(*sws);
        *sws = std::ptr::null_mut();
    }
    if !swr.is_null() {
        swr_free(swr);
    }
}

unsafe fn cleanup_transcode(
    mut in_ctx: *mut AVFormatContext,
    out_ctx: *mut AVFormatContext,
    mut v_dec: *mut AVCodecContext,
    mut a_dec: *mut AVCodecContext,
    mut v_enc: *mut AVCodecContext,
    mut a_enc: *mut AVCodecContext,
) {
    avformat_close_input(&mut in_ctx);
    if !out_ctx.is_null() {
        avformat_free_context(out_ctx);
    }
    if !v_dec.is_null() {
        avcodec_free_context(&mut v_dec);
    }
    if !a_dec.is_null() {
        avcodec_free_context(&mut a_dec);
    }
    if !v_enc.is_null() {
        avcodec_free_context(&mut v_enc);
    }
    if !a_enc.is_null() {
        avcodec_free_context(&mut a_enc);
    }
}

/// Scale (src_w, src_h) down to fit within max_w x max_h, keeping aspect ratio.
/// If either max is None, no constraint on that axis.
pub fn fit_dimensions(
    src_w: i32,
    src_h: i32,
    max_w: Option<i32>,
    max_h: Option<i32>,
) -> (i32, i32) {
    if src_w <= 0 || src_h <= 0 {
        return (src_w, src_h);
    }
    let mut w = src_w as f64;
    let mut h = src_h as f64;
    if let Some(mw) = max_w.filter(|&v| v > 0) {
        if w > mw as f64 {
            let scale = mw as f64 / w;
            w *= scale;
            h *= scale;
        }
    }
    if let Some(mh) = max_h.filter(|&v| v > 0) {
        if h > mh as f64 {
            let scale = mh as f64 / h;
            w *= scale;
            h *= scale;
        }
    }
    // Round to even (required by most H.264 encoders)
    ((w as i32 + 1) & !1, (h as i32 + 1) & !1)
}
