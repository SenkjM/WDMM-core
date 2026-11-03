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

use std::ffi::{c_char, c_int, CStr, CString};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::sys::*;
use crate::{
    diag, AudioStream, EncoderInfo, FfmpegError, FfmpegResult, ProbeResult, ProgressState,
    SourcePlan, SubtitleStream, TranscodePreset, VideoStream,
};

/// Time base used for millisecond content timestamps.
const MS_TB: AVRational = AVRational::new(1, 1000);

/// How often (~ms of wall time) to emit a progress diagnostic line.
const PROGRESS_LOG_EVERY_MS: u64 = 2_000;

// ── Remux decision (pure logic lives in `crate::plan`, host-testable) ──────

use crate::plan::{
    choose_video_encoder, encoder_frame_rate, fallback_warning, hw_video_encoder_name,
    hybrid_audio_codec_id, remux_dropped_streams, remux_plan, segment_gop_size, RemuxPlan,
};
/// Options for opening the loopback HTTP input: bound every blocking read so a
/// stalled upstream fails with a real error instead of hanging the job forever.
const INPUT_TIMEOUT_US: i64 = 15_000_000;

unsafe fn input_open_options() -> *mut AVDictionary {
    let timeout = CString::new(INPUT_TIMEOUT_US.to_string()).unwrap();
    let mut dict: *mut AVDictionary = std::ptr::null_mut();
    av_dict_set(&mut dict, c"timeout".as_ptr(), timeout.as_ptr(), 0);
    av_dict_set(&mut dict, c"probesize".as_ptr(), c"5000000".as_ptr(), 0);
    av_dict_set(
        &mut dict,
        c"analyzeduration".as_ptr(),
        c"3000000".as_ptr(),
        0,
    );
    dict
}

/// Codec id of a stream, when the index and stream are valid.
unsafe fn stream_codec_id(streams_ptr: *const *mut AVStream, idx: i32) -> Option<u32> {
    if idx < 0 {
        return None;
    }
    let st = *streams_ptr.add(idx as usize);
    if st.is_null() {
        return None;
    }
    let cp = (*st).codecpar;
    if cp.is_null() {
        return None;
    }
    Some((*cp).codec_id)
}

/// HLS `hls_time` in seconds, honouring a millisecond override.
///
/// `hlsenc` parses `hls_time` as a decimal float, so sub-second overrides are
/// passed with up to three decimals. This is what lets the proxy align the
/// muxer grid with a calibrated (non-integer) segment plan.
fn hls_time_seconds(preset_secs: u32, override_ms: Option<u32>) -> String {
    let ms = match override_ms {
        Some(ms) if ms > 0 => ms,
        _ => preset_secs.saturating_mul(1000),
    };
    if ms % 1000 == 0 {
        (ms / 1000).to_string()
    } else {
        format!("{:.3}", ms as f64 / 1000.0)
    }
}

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
                    // framerate is not in codecpar; read AVStream.r_frame_rate
                    // (@216/@220 on LP64 — see `stream_r_frame_rate`, `+56` is nb_frames).
                    // Feel free to compute num/den by hand: FFmpeg 7 made
                    // av_q2d() a static inline in rational.h, so the shared
                    // library no longer exports it and calling it would leave
                    // an undefined symbol that makes dlopen fail on device.
                    let fps_raw = q2d(stream_r_frame_rate(st));
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
                        language: stream_metadata_value(st, "language"),
                    });
                }
                3 => {
                    // AVMEDIA_TYPE_SUBTITLE = 3
                    subs.push(SubtitleStream {
                        index: (*st).index as u32,
                        codec: codec_name,
                        language: stream_metadata_value(st, "language"),
                        title: stream_metadata_value(st, "title"),
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

// ── Source plan (decided from one container open, before any segment) ────────

/// Collect the video stream's keyframe timestamps (milliseconds) from the
/// container index built so far in `streams_ptr`.
///
/// The timestamps are kept signed on purpose: the MP4 demuxer stores each
/// sample's DTS in the index, and with B-frames the first keyframe's DTS is
/// negative (e.g. `-146.556 ms` for a 23.976 fps clip). `source_plan_impl`
/// shifts the whole list onto the packet-PTS timeline before planning.
unsafe fn collect_keyframes_ms(streams_ptr: *const *mut AVStream, v_idx: i32) -> Vec<i64> {
    if v_idx < 0 {
        return Vec::new();
    }
    let st = *streams_ptr.add(v_idx as usize);
    if st.is_null() {
        return Vec::new();
    }
    let count = avformat_index_get_entries_count(st);
    if count <= 0 {
        return Vec::new();
    }
    let tb = (*st).time_base;
    if tb.den <= 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count {
        let entry = avformat_index_get_entry(st, i);
        if entry.is_null() {
            continue;
        }
        if (*entry).flags_and_size & AVINDEX_KEYFRAME == 0 {
            continue;
        }
        let pts = (*entry).timestamp;
        if pts == AV_NOPTS_VALUE {
            continue;
        }
        let ms = i128::from(pts) * 1000 * i128::from(tb.num) / i128::from(tb.den);
        out.push(ms.clamp(i64::MIN as i128, i64::MAX as i128) as i64);
    }
    out
}

/// Video keyframe timestamps (ms), asking the demuxer to finish parsing the
/// container index when it has not done so yet.
///
/// MP4 exposes its complete sample index right after
/// `avformat_find_stream_info`. Matroska defers `Cues` parsing until the first
/// seek, so one `av_seek_frame(.., 0, AVSEEK_FLAG_BACKWARD)` populates the full
/// index without demuxing media. A no-`Cues` file only gets the linear-scan
/// prefix; [`crate::vod::keyframe_plan`] rejects that truncated index.
/// Entry count below which the index is assumed to be lazily built (MKV cues are
/// only parsed on the first seek): do one `seek(0)` and re-read.
const MIN_INDEX_ENTRIES_BEFORE_SEEK: usize = 3;

unsafe fn video_keyframe_index_ms(
    in_ctx: *mut AVFormatContext,
    streams_ptr: *const *mut AVStream,
    v_idx: i32,
) -> Vec<i64> {
    let mut keyframes = collect_keyframes_ms(streams_ptr, v_idx);
    if keyframes.len() < MIN_INDEX_ENTRIES_BEFORE_SEEK {
        let before = keyframes.len();
        let ret = av_seek_frame(in_ctx, -1, 0, AVSEEK_FLAG_BACKWARD);
        diag::log(format!(
            "source plan: keyframe index entries_before={before} seek(0) rc={ret}"
        ));
        if ret >= 0 {
            keyframes = collect_keyframes_ms(streams_ptr, v_idx);
        }
    }
    keyframes.sort_unstable();
    keyframes.dedup();
    keyframes
}

/// PTS (in milliseconds, stream timeline) of the first video packet in the
/// current read position.
///
/// The index timestamps are DTS for MP4 but `av_seek_frame` (and every player)
/// works on PTS, so the plan needs the packet-PTS origin to shift the index onto
/// the seekable timeline. The walk stops at the first video packet, so it reads
/// at most a handful of packets even when audio comes first.
unsafe fn first_video_packet_pts_ms(
    in_ctx: *mut AVFormatContext,
    streams_ptr: *const *mut AVStream,
    v_idx: i32,
) -> Option<i64> {
    if v_idx < 0 {
        return None;
    }
    let st = *streams_ptr.add(v_idx as usize);
    if st.is_null() {
        return None;
    }
    let tb = (*st).time_base;
    if tb.den <= 0 {
        return None;
    }
    let mut pkt = av_packet_alloc();
    if pkt.is_null() {
        return None;
    }
    let mut out = None;
    // Bound the walk: the first video packet normally arrives immediately, but
    // a malformed file could pad the front with thousands of other packets.
    for _ in 0..4096 {
        let ret = av_read_frame(in_ctx, pkt);
        if ret < 0 {
            break;
        }
        let is_video = (*pkt).stream_index == v_idx;
        if is_video {
            let pts = (*pkt).pts;
            if pts != AV_NOPTS_VALUE {
                let ms = i128::from(pts) * 1000 * i128::from(tb.num) / i128::from(tb.den);
                out = Some(ms.clamp(i64::MIN as i128, i64::MAX as i128) as i64);
            }
        }
        av_packet_unref(pkt);
        if is_video {
            break;
        }
    }
    av_packet_free(&mut pkt);
    out
}

/// Implementation of [`crate::source_plan`]: one open, decide plan.
pub fn source_plan_impl(url: &str, preset: &TranscodePreset) -> FfmpegResult<Option<SourcePlan>> {
    if preset.container == "original" {
        // Pass-through casts have no HLS segmentation at all.
        return Ok(None);
    }
    let url_c = CString::new(url).map_err(|e| FfmpegError::Probe(e.to_string()))?;
    unsafe {
        let mut in_ctx: *mut AVFormatContext = std::ptr::null_mut();
        let mut open_opts = input_open_options();
        let ret = avformat_open_input(
            &mut in_ctx,
            url_c.as_ptr(),
            std::ptr::null(),
            &mut open_opts,
        );
        av_dict_free(&mut open_opts);
        if ret < 0 {
            return Err(FfmpegError::Probe(format!(
                "source_plan avformat_open_input({url}): {}",
                av_err2str(ret)
            )));
        }
        let ret = avformat_find_stream_info(in_ctx, std::ptr::null_mut());
        if ret < 0 {
            avformat_close_input(&mut in_ctx);
            return Err(FfmpegError::Probe(format!(
                "source_plan avformat_find_stream_info: {}",
                av_err2str(ret)
            )));
        }

        let duration_raw = fmtctx_duration(in_ctx);
        let duration_ms = if duration_raw == AV_NOPTS_VALUE || duration_raw <= 0 {
            None
        } else {
            Some(duration_raw / 1000)
        };
        let Some(duration_ms) = duration_ms else {
            diag::log(format!("source plan: {url} has no duration, deferring to warm-up"));
            avformat_close_input(&mut in_ctx);
            return Ok(None);
        };

        let streams_ptr = fmtctx_streams(in_ctx);
        let v_idx = av_find_best_stream(
            in_ctx,
            AVMEDIA_TYPE_VIDEO,
            -1,
            -1,
            std::ptr::null_mut(),
            0,
        );
        let a_idx = av_find_best_stream(
            in_ctx,
            AVMEDIA_TYPE_AUDIO,
            -1,
            -1,
            std::ptr::null_mut(),
            0,
        );
        let video_codec = stream_codec_id(streams_ptr, v_idx);
        let audio_codec = stream_codec_id(streams_ptr, a_idx);
        let seg_secs = f64::from(preset.hls_segment_secs);

        let plan = if video_codec.is_none() {
            // Audio-only HLS cuts on audio packet timing; there is no index that
            // predicts it, so the warm-up calibration stays in charge.
            None
        } else if preset.remux_first
            && remux_plan(video_codec, audio_codec) != RemuxPlan::Transcode
        {
            let mut keyframes = video_keyframe_index_ms(in_ctx, streams_ptr, v_idx);
            // MP4 indexes store DTS, and the B-frame delay makes the first
            // keyframe's DTS negative. Anchor the list on the first video
            // packet's PTS so both the proxy's seek offset and the muxer's
            // first cut land on the keyframe the plan names. Matroska indexes
            // already use PTS, so the shift is zero there.
            if let (Some(first_pts), Some(first_kf)) =
                (first_video_packet_pts_ms(in_ctx, streams_ptr, v_idx), keyframes.first().copied())
            {
                let shift = first_pts.saturating_sub(first_kf);
                if shift != 0 {
                    for kf in &mut keyframes {
                        *kf = kf.saturating_add(shift);
                    }
                    diag::log(format!(
                        "source plan: {url} keyframe timeline shifted by {shift}ms (index timestamps are dts)"
                    ));
                }
            }
            let count = keyframes.len();
            match crate::vod::keyframe_plan(duration_ms, seg_secs, &keyframes) {
                Some(plan) => {
                    diag::log(format!(
                        "source plan: {url} keyframe grid segments={} keyframes={count} gop_ms={:?}",
                        plan.segment_count(),
                        plan.gop_ms
                    ));
                    Some(SourcePlan {
                        plan,
                        video_copied: true,
                        keyframe_count: count,
                    })
                }
                None => {
                    diag::log(format!(
                        "source plan: {url} keyframe index unusable (entries={count}), deferring to warm-up"
                    ));
                    None
                }
            }
        } else {
            // Video is re-encoded: the encoder holds one IDR per segment (see
            // `encode_grid`), so the grid is uniform — but its step is the
            // encoder's frame-accurate GOP, not the nominal segment length: a
            // 16.1667 fps source can only cut at 32 frames (1.979 s) for a 2 s
            // target, and the plan must name what the muxer will really write.
            let v_st = *streams_ptr.add(v_idx as usize);
            let src_rate = stream_r_frame_rate(v_st);
            let (fps_num, fps_den) =
                encoder_frame_rate(src_rate.num, src_rate.den, preset.video_fps_cap);
            let configured = AVRational::new(fps_num, fps_den);
            let (gop, step_secs) = encode_grid(src_rate, configured, preset.hls_segment_secs);
            let plan = crate::vod::exact_plan(duration_ms, step_secs);
            diag::log(format!(
                "source plan: {url} re-encode exact grid segments={} gop={gop} step_ms={:.1}",
                plan.segment_count(),
                step_secs * 1000.0
            ));
            Some(SourcePlan {
                plan,
                video_copied: false,
                keyframe_count: 0,
            })
        };

        avformat_close_input(&mut in_ctx);
        Ok(plan)
    }
}

// ── Embedded text subtitle extraction ─────────────────────────────────────────

/// How many packets/cues an extraction walk may consume before it gives up.
/// A subtitle track is tiny; this only guards against pathological inputs.
const SUBTITLE_MAX_PACKETS: u64 = 2_000_000;

/// Read a libav-returned C string, tolerating NULL.
unsafe fn cstr_opt(p: *const c_char) -> Option<String> {
    if p.is_null() {
        None
    } else {
        Some(CStr::from_ptr(p).to_string_lossy().into_owned())
    }
}

/// Read one `AVStream` metadata value (`metadata` dictionary @ +80).
///
/// Containers store per-track `language` and `title` here (Matroska/MP4), which
/// is the only way to tell apart e.g. 33 Japanese subtitle tracks that all use
/// the same codec. Returns `None` for missing or blank values.
unsafe fn stream_metadata_value(st: *const AVStream, key: &str) -> Option<String> {
    let dict = stream_metadata(st);
    if dict.is_null() {
        return None;
    }
    let key_c = CString::new(key).ok()?;
    let entry = av_dict_get(
        dict,
        key_c.as_ptr(),
        std::ptr::null(),
        AV_DICT_IGNORE_SUFFIX,
    );
    if entry.is_null() {
        return None;
    }
    cstr_opt((*entry).value).filter(|value| !value.trim().is_empty())
}

/// Turn one decoded `AVSubtitle` into a cue (text + absolute ms window).
unsafe fn cue_from_avsubtitle(
    sub: *const AVSubtitle,
    fallback_pts_ms: i64,
    codec: &str,
) -> Option<crate::subtitle::Cue> {
    let sub_pts = subtitle_pts(sub);
    // AVSubtitle.pts is in AV_TIME_BASE (µs); fall back to the packet pts.
    let base_ms = if sub_pts == AV_NOPTS_VALUE || sub_pts < 0 {
        fallback_pts_ms
    } else {
        sub_pts / 1_000
    };
    let start_ms = base_ms + subtitle_start_display_time(sub) as i64;
    let end_ms = base_ms + subtitle_end_display_time(sub) as i64;

    let num = subtitle_num_rects(sub);
    let rects = subtitle_rects(sub);
    if rects.is_null() || num == 0 {
        return None;
    }
    let mut texts: Vec<String> = Vec::new();
    for i in 0..num {
        let rect = *rects.add(i as usize);
        if rect.is_null() {
            continue;
        }
        if rect_type(rect) == SUBTITLE_RECT_BITMAP {
            // Should have been rejected before decoding, but never render.
            continue;
        }
        let text = cstr_opt(rect_text(rect));
        let ass = cstr_opt(rect_ass(rect));
        if let Some(body) = crate::subtitle::cue_text_from_decoded(text.as_deref(), ass.as_deref())
        {
            texts.push(body);
        }
    }
    if texts.is_empty() {
        return None;
    }
    let text = texts.join("\n");
    let start_ms = start_ms.max(0) as u64;
    let end_ms = if end_ms <= start_ms as i64 {
        start_ms.saturating_add(1_000)
    } else {
        end_ms as u64
    };
    diag::log(format!(
        "[subtitle] cue {start_ms}..{end_ms}ms from {codec} ({} rect(s))",
        texts.len()
    ));
    Some(crate::subtitle::Cue {
        start_ms,
        end_ms,
        text,
    })
}

/// Decode the subtitle stream at `stream_index` of an already-opened input.
unsafe fn extract_subtitle_inner(
    fmt_ctx: *mut AVFormatContext,
    stream_index: u32,
) -> FfmpegResult<String> {
    use crate::subtitle::{self, normalize_cues, to_webvtt, Cue, TrackKind};

    let nb = fmtctx_nb_streams(fmt_ctx) as usize;
    if stream_index as usize >= nb {
        return Err(FfmpegError::Probe(format!(
            "subtitle stream {stream_index} not found ({nb} streams in input)"
        )));
    }
    let st = *fmtctx_streams(fmt_ctx).add(stream_index as usize);
    if st.is_null() || (*st).codecpar.is_null() {
        return Err(FfmpegError::Probe(format!(
            "subtitle stream {stream_index} has no codec parameters"
        )));
    }
    let cp = (*st).codecpar;
    if (*cp).codec_type != AVMEDIA_TYPE_SUBTITLE {
        return Err(FfmpegError::Probe(format!(
            "stream {stream_index} is not a subtitle stream (av media type {})",
            (*cp).codec_type
        )));
    }
    let codec = codec_display_name((*cp).codec_id);
    match subtitle::classify_codec(&codec) {
        TrackKind::Text => {}
        TrackKind::Bitmap => {
            return Err(FfmpegError::Probe(format!(
                "stream {stream_index} codec '{codec}': {}",
                subtitle::unsupported_reason(&codec).unwrap_or("bitmap subtitle")
            )));
        }
        TrackKind::Other => {
            return Err(FfmpegError::Probe(format!(
                "stream {stream_index} codec '{codec}': {}",
                subtitle::unsupported_reason(&codec).unwrap_or("unsupported subtitle")
            )));
        }
    }

    let decoder = avcodec_find_decoder((*cp).codec_id);
    if decoder.is_null() {
        return Err(FfmpegError::Probe(format!(
            "libavcodec has no decoder for subtitle codec '{codec}'"
        )));
    }
    let mut dec_ctx = avcodec_alloc_context3(decoder);
    if dec_ctx.is_null() {
        return Err(FfmpegError::Probe(
            "avcodec_alloc_context3(subtitle) returned NULL".into(),
        ));
    }
    let ret = avcodec_parameters_to_context(dec_ctx, cp);
    if ret < 0 {
        avcodec_free_context(&mut dec_ctx);
        return Err(ffmpeg_err(
            format!("avcodec_parameters_to_context({codec})"),
            ret,
        ));
    }
    let ret = avcodec_open2(dec_ctx, decoder, std::ptr::null_mut());
    if ret < 0 {
        avcodec_free_context(&mut dec_ctx);
        return Err(ffmpeg_err(format!("avcodec_open2({codec})"), ret));
    }

    let mut pkt = av_packet_alloc();
    if pkt.is_null() {
        avcodec_free_context(&mut dec_ctx);
        return Err(FfmpegError::Probe("av_packet_alloc returned NULL".into()));
    }

    let tb = (*st).time_base;
    let mut cues: Vec<Cue> = Vec::new();
    let mut packets = 0u64;
    let mut decode_errors = 0u32;
    let mut read_error: Option<i32> = None;

    loop {
        let ret = av_read_frame(fmt_ctx, pkt);
        if ret < 0 {
            // AVERROR_EOF is the normal end; anything else is reported when no
            // cue was produced at all.
            read_error = Some(ret);
            break;
        }
        if (*pkt).stream_index as u32 == stream_index {
            packets += 1;
            let fallback_pts_ms = if (*pkt).pts == AV_NOPTS_VALUE {
                0
            } else {
                av_rescale_q((*pkt).pts, tb, MS_TB).max(0)
            };
            let mut sub: AVSubtitle = std::mem::zeroed();
            let mut got: c_int = 0;
            let ret = avcodec_decode_subtitle2(dec_ctx, &mut sub, &mut got, pkt);
            if ret < 0 {
                decode_errors += 1;
                if decode_errors <= 3 {
                    diag::log(format!(
                        "[subtitle] decode error #{decode_errors} in '{codec}' stream {stream_index}: {}",
                        av_err2str(ret)
                    ));
                }
            }
            if got != 0 {
                let cue = cue_from_avsubtitle(&sub, fallback_pts_ms, &codec);
                avsubtitle_free(&mut sub);
                if let Some(cue) = cue {
                    cues.push(cue);
                }
            }
        }
        av_packet_unref(pkt);
        if packets >= SUBTITLE_MAX_PACKETS || cues.len() >= subtitle::MAX_CUES {
            diag::log(format!(
                "[subtitle] stopping extraction early after {packets} packets / {} cues",
                cues.len()
            ));
            break;
        }
    }

    av_packet_free(&mut pkt);
    avcodec_free_context(&mut dec_ctx);

    let cues = normalize_cues(cues);
    if cues.is_empty() {
        return Err(FfmpegError::Probe(format!(
            "no subtitle cues decoded from '{codec}' stream {stream_index} \
             ({packets} packets, {decode_errors} decode error(s){}",
            read_error
                .map(|code| format!(", read ended with {}", av_err2str(code)))
                .unwrap_or_default()
        )));
    }
    diag::log(format!(
        "[subtitle] extracted {} cues from '{codec}' stream {stream_index} ({packets} packets, {decode_errors} decode error(s))",
        cues.len()
    ));
    Ok(to_webvtt(&cues))
}

/// Extract one embedded text subtitle stream as WebVTT (public entry point).
pub fn extract_subtitle_track(url: &str, stream_index: u32) -> FfmpegResult<String> {
    let url_c = CString::new(url).map_err(|e| FfmpegError::Probe(e.to_string()))?;
    unsafe {
        let mut fmt_ctx: *mut AVFormatContext = std::ptr::null_mut();
        let mut opts = input_open_options();
        let ret = avformat_open_input(&mut fmt_ctx, url_c.as_ptr(), std::ptr::null(), &mut opts);
        av_dict_free(&mut opts);
        if ret < 0 || fmt_ctx.is_null() {
            return Err(ffmpeg_err(format!("open subtitle source {url}"), ret));
        }
        let ret = avformat_find_stream_info(fmt_ctx, std::ptr::null_mut());
        let result = if ret < 0 {
            Err(ffmpeg_err("avformat_find_stream_info(subtitle)", ret))
        } else {
            extract_subtitle_inner(fmt_ctx, stream_index)
        };
        avformat_close_input(&mut fmt_ctx);
        result
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

/// Human-readable name for the pixel formats the hardware path can feed.
fn pix_fmt_name(fmt: c_int) -> &'static str {
    if fmt == AV_PIX_FMT_NV12 {
        "nv12"
    } else if fmt == AV_PIX_FMT_YUV420P {
        "yuv420p"
    } else {
        "unknown"
    }
}

/// Frame rate the encoder actually receives.
///
/// Decoded frames are forwarded with their original timestamps (the transcode
/// path never resamples or drops frames), so the real frame cadence is the
/// source stream's rate. A preset fps cap only changes the encoder *hint* and
/// must not be used to size the keyframe interval, or the IDRs land off the
/// planned grid (e.g. a 16.1667 fps source hinted as 15 fps would produce
/// 1.855 s segments for a 2 s grid).
fn encoder_grid_rate(source: AVRational, configured: AVRational) -> AVRational {
    let fps = f64::from(source.num) / f64::from(source.den.max(1));
    if source.num > 0 && source.den > 0 && (1.0..=240.0).contains(&fps) {
        source
    } else {
        configured
    }
}

/// Encoder GOP in frames plus its real duration in seconds.
///
/// The frame count is chosen so that an IDR lands on every `seg_secs` boundary;
/// the returned `step_secs` is that GOP expressed in media time, which is the
/// grid the produced `EXTINF` values follow when the source rate does not divide
/// `seg_secs` evenly (16.1667 fps: 2 s = 32.33 frames, so the encoder can only
/// cut at 32 frames = 1.979 s).
fn encode_grid(
    source_rate: AVRational,
    configured_rate: AVRational,
    seg_secs: u32,
) -> (i32, f64) {
    let rate = encoder_grid_rate(source_rate, configured_rate);
    let gop = segment_gop_size(rate.num, rate.den, seg_secs);
    let step_secs = if rate.num > 0 && rate.den > 0 {
        f64::from(gop) * f64::from(rate.den) / f64::from(rate.num)
    } else {
        f64::from(seg_secs)
    };
    (gop, step_secs)
}

/// Configure and open a MediaCodec hardware video encoder.
///
/// `mediacodecenc.c` accepts software frames via an input ByteBuffer in
/// `AV_PIX_FMT_NV12` or `AV_PIX_FMT_YUV420P` (no EGL/Surface); NV12 is tried
/// first because that is the semi-planar format the Android encoders prefer.
/// Opening is the real probe: `avcodec_find_encoder_by_name` only proves the
/// wrapper is compiled into libavcodec, while the device codec may be missing
/// or reject the configuration. `max_b_frames = 0` is mandatory for this
/// encoder, and `AV_CODEC_FLAG_GLOBAL_HEADER` makes it emit AVCC extradata in
/// `codecpar` so the MPEG-TS muxer can insert `h264_mp4toannexb`.
///
/// Returns the opened context plus the pixel format, or a diagnostic string
/// describing the last failure (caller falls back to libx264 and reports it).
unsafe fn open_hw_video_encoder(
    tag: &str,
    hw_name: &str,
    codec: *const AVCodec,
    preset: &wdmm_core::models::TranscodePreset,
    width: c_int,
    height: c_int,
    framerate: AVRational,
    gop: c_int,
) -> Result<(*mut AVCodecContext, c_int), String> {
    let mut last_err = String::from("unknown error");
    for pix_fmt in [AV_PIX_FMT_NV12, AV_PIX_FMT_YUV420P] {
        let mut ctx = avcodec_alloc_context3(codec);
        if ctx.is_null() {
            last_err = "avcodec_alloc_context3 returned null".to_string();
            continue;
        }
        codctx_set_width(ctx, width);
        codctx_set_height(ctx, height);
        codctx_set_pix_fmt(ctx, pix_fmt);
        codctx_set_time_base(ctx, AVRational::new(1, 90_000));
        codctx_set_framerate(ctx, framerate);
        codctx_set_gop_size(ctx, gop);
        codctx_set_max_b_frames(ctx, 0);
        if let Some(br) = preset.video_max_bitrate {
            codctx_set_bit_rate(ctx, br as i64);
        }
        codctx_set_flags(ctx, codctx_flags(ctx) | AV_CODEC_FLAG_GLOBAL_HEADER);
        let ret = avcodec_open2(ctx, codec, std::ptr::null_mut());
        diag::log(format!(
            "{tag} transcode avcodec_open2(hw video encoder {hw_name} pix_fmt={} gop={gop} max_b_frames=0) rc={ret}",
            pix_fmt_name(pix_fmt)
        ));
        if ret >= 0 {
            return Ok((ctx, pix_fmt));
        }
        last_err = format!(
            "pix_fmt={} rc={ret} ({})",
            pix_fmt_name(pix_fmt),
            av_err2str(ret)
        );
        avcodec_free_context(&mut ctx);
    }
    Err(last_err)
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
                    (*cp).codec_id,
                    (*cp).width,
                    (*cp).height
                )
            }
            AVMEDIA_TYPE_AUDIO => {
                audio = format!(
                    "{name} (id={}) {}Hz {}ch",
                    (*cp).codec_id,
                    (*cp).sample_rate,
                    (*cp).ch_layout.nb_channels
                )
            }
            _ => {}
        }
        diag::log(format!(
            "{tag} {mode} input stream {i}: type={} codec={name} (id={})",
            (*cp).codec_type,
            (*cp).codec_id
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
                entries
                    .flatten()
                    .any(|e| e.file_name().to_string_lossy().ends_with(".ts"))
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
    start_number: u64,
    hls_time_override_ms: Option<u32>,
) -> FfmpegResult<()> {
    telemetry.start();
    let tag = job_tag(job_id);
    diag::log(format!(
        "{tag} job start preset={} container={} remux_first={} video_codec={:?} audio_codec={} start_offset={start_offset_secs:.2} start_number={start_number} hls_time_override_ms={hls_time_override_ms:?} seg_dir={} input={}",
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
            start_number,
            hls_time_override_ms,
        ) {
            Ok(()) => Ok(()),
            Err(e) => {
                diag::log(format!(
                    "{tag} remux failed: {e}; falling back to transcode"
                ));
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
                        start_number,
                        hls_time_override_ms,
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
            start_number,
            hls_time_override_ms,
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

// ── Shared audio decode → resample → encode pipeline ────────────────────────
// Used by both the full transcode path and the hybrid remux path (copy video,
// re-encode only the audio track). Keeping one implementation means the hybrid
// mode cannot silently drift away from the transcode path.

unsafe fn open_audio_decoder(
    streams_ptr: *const *mut AVStream,
    a_idx: i32,
    a_dec_codec: *const AVCodec,
    tag: &str,
) -> FfmpegResult<*mut AVCodecContext> {
    let in_st = *streams_ptr.add(a_idx as usize);
    let dec_name = codec_display_name((*(*in_st).codecpar).codec_id);
    let mut dec_ctx = avcodec_alloc_context3(a_dec_codec);
    if dec_ctx.is_null() {
        return Err(FfmpegError::Probe(
            "avcodec_alloc_context3(audio decoder) returned null".into(),
        ));
    }
    let ret = avcodec_parameters_to_context(dec_ctx, (*in_st).codecpar);
    if ret < 0 {
        avcodec_free_context(&mut dec_ctx);
        return Err(ffmpeg_err(
            "avcodec_parameters_to_context(audio decoder)",
            ret,
        ));
    }
    let ret = avcodec_open2(dec_ctx, a_dec_codec, std::ptr::null_mut());
    diag::log(format!(
        "{tag} avcodec_open2(audio decoder {dec_name}) rc={ret}"
    ));
    if ret < 0 {
        avcodec_free_context(&mut dec_ctx);
        return Err(ffmpeg_err(
            format!("avcodec_open2(audio decoder {dec_name})"),
            ret,
        ));
    }
    Ok(dec_ctx)
}

unsafe fn open_audio_encoder(
    preset: &wdmm_core::models::TranscodePreset,
    codec_id: AVCodecID,
    tag: &str,
) -> FfmpegResult<(*mut AVCodecContext, String)> {
    let enc_name = codec_display_name(codec_id);
    let enc_codec = avcodec_find_encoder(codec_id);
    if enc_codec.is_null() {
        diag::log(format!(
            "{tag} failed: no encoder for audio codec {enc_name} (id={codec_id})"
        ));
        return Err(FfmpegError::Probe(format!(
            "no encoder for audio codec {enc_name} (id={codec_id})"
        )));
    }
    let mut enc_ctx = avcodec_alloc_context3(enc_codec);
    if enc_ctx.is_null() {
        return Err(FfmpegError::Probe(
            "avcodec_alloc_context3(audio encoder) returned null".into(),
        ));
    }
    let audio_sample_rate = preset.audio_sample_rate.unwrap_or(48_000) as i32;
    codctx_set_sample_rate(enc_ctx, audio_sample_rate);
    // Set the time base *before* opening: encoders derive packet timestamps
    // from it, and the value must match the output stream's.
    codctx_set_time_base(enc_ctx, AVRational::new(1, audio_sample_rate));
    codctx_set_sample_fmt(enc_ctx, AV_SAMPLE_FMT_FLTP);
    let layout = if preset.audio_channels == 1 {
        AVChannelLayout::mono()
    } else {
        AVChannelLayout::stereo()
    };
    codctx_set_ch_layout(enc_ctx, layout);
    codctx_set_bit_rate(enc_ctx, preset.audio_bitrate as i64);
    let ret = avcodec_open2(enc_ctx, enc_codec, std::ptr::null_mut());
    diag::log(format!(
        "{tag} avcodec_open2(audio encoder {}) rc={ret}",
        codec_display_name(codec_id)
    ));
    if ret < 0 {
        avcodec_free_context(&mut enc_ctx);
        return Err(ffmpeg_err(
            format!(
                "avcodec_open2(audio encoder {})",
                codec_display_name(codec_id)
            ),
            ret,
        ));
    }
    Ok((enc_ctx, codec_display_name(codec_id)))
}

/// Audio transcode pipeline: swresample context plus a sample FIFO.
///
/// swresample hands back whatever sample count the decoder produced (FLAC emits
/// 4096-sample frames), while the AAC/MP3 encoders require exactly
/// `AVCodecContext.frame_size` samples per frame — `libavcodec/encode.c` rejects
/// `nb_samples > frame_size` with EINVAL. The FIFO re-chunks the resampled audio
/// into encoder-sized frames and keeps an independent sample counter for
/// timestamps.
struct AudioPipeline {
    swr: *mut SwrContext,
    fifo: *mut AVAudioFifo,
    sample_rate: c_int,
    layout: AVChannelLayout,
    frame_size: c_int,
    next_pts: i64,
    /// First decoded frame described the source format; later frames must match.
    source_configured: bool,
}

impl Default for AudioPipeline {
    fn default() -> Self {
        Self {
            swr: std::ptr::null_mut(),
            fifo: std::ptr::null_mut(),
            sample_rate: 0,
            layout: AVChannelLayout::stereo(),
            frame_size: 0,
            next_pts: 0,
            source_configured: false,
        }
    }
}

impl AudioPipeline {
    unsafe fn new(audio_sample_rate: i32, audio_channels: u32) -> Self {
        Self {
            sample_rate: audio_sample_rate,
            layout: if audio_channels == 1 {
                AVChannelLayout::mono()
            } else {
                AVChannelLayout::stereo()
            },
            ..Self::default()
        }
    }

    /// Lazily configure swresample/FIFO from the first decoded frame.
    unsafe fn ensure(
        &mut self,
        a_enc_ctx: *mut AVCodecContext,
        dec_frame: *mut AVFrame,
    ) -> FfmpegResult<()> {
        if self.source_configured {
            return Ok(());
        }
        let ret = swr_alloc_set_opts2(
            &mut self.swr,
            &self.layout,
            AV_SAMPLE_FMT_FLTP,
            self.sample_rate,
            frame_ch_layout(dec_frame),
            frame_format(dec_frame) as AVSampleFormat,
            frame_sample_rate(dec_frame),
            0,
            std::ptr::null_mut(),
        );
        if ret < 0 || self.swr.is_null() {
            return Err(ffmpeg_err("swr_alloc_set_opts2", ret));
        }
        let ret = swr_init(self.swr);
        if ret < 0 {
            return Err(ffmpeg_err("swr_init", ret));
        }
        self.frame_size = codctx_frame_size(a_enc_ctx);
        if self.frame_size <= 0 {
            // Encoders without a fixed frame size never need re-chunking; any
            // positive value makes `drain` a pass-through of what the FIFO has.
            self.frame_size = 1;
        }
        self.fifo = av_audio_fifo_alloc(
            AV_SAMPLE_FMT_FLTP,
            self.layout.nb_channels,
            self.frame_size * 4,
        );
        if self.fifo.is_null() {
            return Err(FfmpegError::Probe(
                "av_audio_fifo_alloc returned null".into(),
            ));
        }
        self.source_configured = true;
        Ok(())
    }

    /// Resample one decoded frame, queue it, and mux every complete output frame.
    unsafe fn push(
        &mut self,
        a_enc_ctx: *mut AVCodecContext,
        dec_frame: *mut AVFrame,
        out_pkt: *mut AVPacket,
        out_ctx: *mut AVFormatContext,
        out_a_idx: i32,
        job: &mut JobCtx<'_>,
    ) -> FfmpegResult<()> {
        self.ensure(a_enc_ctx, dec_frame)?;
        let mut conv = av_frame_alloc();
        if conv.is_null() {
            return Err(FfmpegError::Probe(
                "av_frame_alloc(resampled) returned null".into(),
            ));
        }
        // `swr_convert_frame()` re-validates the output frame against the context
        // it was initialised with (`config_changed()` compares format,
        // sample_rate and layout, returning AVERROR_OUTPUT_CHANGED otherwise). A
        // bare frame leaves format=NONE/sample_rate=0/empty layout, so describe
        // the target here; with linesize[0] == 0 swresample allocates the buffer
        // and fills in nb_samples itself.
        frame_set_format(conv, AV_SAMPLE_FMT_FLTP);
        frame_set_sample_rate(conv, self.sample_rate);
        frame_set_ch_layout(conv, self.layout);
        let ret = swr_convert_frame(self.swr, conv, dec_frame);
        if ret < 0 {
            av_frame_free(&mut conv);
            return Err(ffmpeg_err("swr_convert_frame", ret));
        }
        let samples = frame_nb_samples(conv);
        if samples > 0 {
            let written = av_audio_fifo_write(self.fifo, frame_extended_data(conv).cast(), samples);
            if written < samples {
                av_frame_free(&mut conv);
                return Err(FfmpegError::Probe(format!(
                    "av_audio_fifo_write accepted {written} of {samples} samples"
                )));
            }
        }
        av_frame_free(&mut conv);
        self.drain(a_enc_ctx, out_pkt, out_ctx, out_a_idx, job, false)
    }

    /// Mux the next `frame_size` samples. With `flush` the remaining tail (which
    /// may be shorter than a full frame) is emitted too.
    unsafe fn drain(
        &mut self,
        a_enc_ctx: *mut AVCodecContext,
        out_pkt: *mut AVPacket,
        out_ctx: *mut AVFormatContext,
        out_a_idx: i32,
        job: &mut JobCtx<'_>,
        flush: bool,
    ) -> FfmpegResult<()> {
        if self.fifo.is_null() {
            return Ok(());
        }
        loop {
            let available = av_audio_fifo_size(self.fifo);
            let want = if flush {
                available.min(self.frame_size)
            } else {
                self.frame_size
            };
            if want <= 0 || available < want {
                return Ok(());
            }
            let mut frame = av_frame_alloc();
            if frame.is_null() {
                return Err(FfmpegError::Probe(
                    "av_frame_alloc(audio encoder input) returned null".into(),
                ));
            }
            frame_set_nb_samples(frame, want);
            frame_set_format(frame, AV_SAMPLE_FMT_FLTP);
            frame_set_sample_rate(frame, self.sample_rate);
            frame_set_ch_layout(frame, self.layout);
            let ret = av_frame_get_buffer(frame, 0);
            if ret < 0 {
                av_frame_free(&mut frame);
                return Err(ffmpeg_err("av_frame_get_buffer(audio)", ret));
            }
            let got = av_audio_fifo_read(self.fifo, frame_extended_data(frame).cast(), want);
            if got != want {
                av_frame_free(&mut frame);
                return Err(FfmpegError::Probe(format!(
                    "av_audio_fifo_read returned {got} samples, expected {want}"
                )));
            }
            frame_set_pts(frame, self.next_pts);
            self.next_pts += i64::from(want);
            let result = encode_and_write(
                a_enc_ctx,
                frame,
                out_pkt,
                out_ctx,
                out_a_idx,
                codctx_time_base(a_enc_ctx),
                job,
            );
            av_frame_free(&mut frame);
            result?;
        }
    }

    unsafe fn free(&mut self) {
        if !self.fifo.is_null() {
            av_audio_fifo_free(self.fifo);
            self.fifo = std::ptr::null_mut();
        }
        if !self.swr.is_null() {
            swr_free(&mut self.swr);
        }
        self.source_configured = false;
    }
}

/// Drain all decoded audio frames after one packet was sent to the decoder.
/// Non-fatal decode errors are logged (max 3) and skipped.
unsafe fn drain_audio_frames(
    a_dec_ctx: *mut AVCodecContext,
    a_enc_ctx: *mut AVCodecContext,
    pkt: *mut AVPacket,
    dec_frame: *mut AVFrame,
    audio: &mut AudioPipeline,
    out_pkt: *mut AVPacket,
    out_ctx: *mut AVFormatContext,
    out_a_idx: i32,
    job: &mut JobCtx<'_>,
    tag: &str,
    decode_errors_logged: &mut u32,
) -> FfmpegResult<()> {
    let sret = avcodec_send_packet(a_dec_ctx, pkt);
    if sret < 0 {
        if *decode_errors_logged < 3 {
            *decode_errors_logged += 1;
            diag::log(format!(
                "{tag} avcodec_send_packet(audio) rc={sret}: {}",
                av_err2str(sret)
            ));
        }
        return Ok(());
    }
    loop {
        let r = avcodec_receive_frame(a_dec_ctx, dec_frame);
        if r == AVERROR_EAGAIN || r == AVERROR_EOF {
            return Ok(());
        }
        if r < 0 {
            if *decode_errors_logged < 3 {
                *decode_errors_logged += 1;
                diag::log(format!(
                    "{tag} avcodec_receive_frame(audio) rc={r}: {}",
                    av_err2str(r)
                ));
            }
            return Ok(());
        }
        let result = audio.push(a_enc_ctx, dec_frame, out_pkt, out_ctx, out_a_idx, job);
        av_frame_unref(dec_frame);
        result?;
    }
}

/// Copy one input packet verbatim into the muxer (remux video/audio streams).
unsafe fn copy_packet_to_muxer(
    pkt: *mut AVPacket,
    in_st: *mut AVStream,
    out_ctx: *mut AVFormatContext,
    out_idx: i32,
    job: &mut JobCtx<'_>,
) -> FfmpegResult<()> {
    let sps = fmtctx_streams(out_ctx);
    let out_st = *sps.add(out_idx as usize);
    if out_st.is_null() {
        return Err(FfmpegError::Probe(
            "output stream pointer is null (remux)".into(),
        ));
    }
    av_packet_rescale_ts(pkt, (*in_st).time_base, (*out_st).time_base);
    (*pkt).pos = -1;

    // Capture before the write consumes the packet.
    let pts = (*pkt).pts;
    let tb = (*out_st).time_base;
    let ret = av_interleaved_write_frame(out_ctx, pkt);
    if ret < 0 {
        return Err(ffmpeg_err("av_interleaved_write_frame (remux)", ret));
    }
    job.record(pts, tb);
    Ok(())
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
    start_number: u64,
    hls_time_override_ms: Option<u32>,
) -> FfmpegResult<()> {
    let url_c = CString::new(input_url).unwrap();
    let playlist = segment_dir.join("index.m3u8");
    let seg_pat = segment_dir.join("seg%d.ts");
    let playlist_c = CString::new(playlist.to_string_lossy().as_ref()).unwrap();
    let seg_pat_c = CString::new(seg_pat.to_string_lossy().as_ref()).unwrap();
    let hls_c = c"hls";

    unsafe {
        // ── Open input ───────────────────────────────────────────────────────
        let mut in_opts = input_open_options();
        let mut in_ctx: *mut AVFormatContext = std::ptr::null_mut();
        let ret = avformat_open_input(&mut in_ctx, url_c.as_ptr(), std::ptr::null(), &mut in_opts);
        av_dict_free(&mut in_opts);
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

        // ── Pick best streams, then decide remux / hybrid / transcode ─────
        let nb = fmtctx_nb_streams(in_ctx) as usize;
        let streams_ptr = fmtctx_streams(in_ctx);
        log_streams(tag, "remux", streams_ptr, nb);

        let mut v_dec_codec: *const AVCodec = std::ptr::null();
        let mut a_dec_codec: *const AVCodec = std::ptr::null();
        let v_idx = av_find_best_stream(in_ctx, AVMEDIA_TYPE_VIDEO, -1, -1, &mut v_dec_codec, 0);
        let a_idx = av_find_best_stream(in_ctx, AVMEDIA_TYPE_AUDIO, -1, -1, &mut a_dec_codec, 0);
        let video_codec_id = stream_codec_id(streams_ptr, v_idx);
        let audio_codec_id = stream_codec_id(streams_ptr, a_idx);
        let plan = remux_plan(video_codec_id, audio_codec_id);
        diag::log(format!(
            "{tag} remux plan: video_idx={v_idx} video_codec={} audio_idx={a_idx} audio_codec={} → {plan:?}",
            video_codec_id
                .map(codec_display_name)
                .unwrap_or_else(|| "none".into()),
            audio_codec_id
                .map(codec_display_name)
                .unwrap_or_else(|| "none".into()),
        ));
        for dropped in remux_dropped_streams(v_idx, a_idx, nb) {
            let st = *streams_ptr.add(dropped);
            if st.is_null() || (*st).codecpar.is_null() {
                diag::log(format!("{tag} remux dropping stream {dropped} (unknown)"));
                continue;
            }
            let cp = (*st).codecpar;
            diag::log(format!(
                "{tag} remux dropping stream {dropped} codec={} (id={})",
                codec_display_name((*cp).codec_id),
                (*cp).codec_id
            ));
        }
        if plan == RemuxPlan::Transcode {
            diag::log(format!(
                "{tag} remux decision: no MPEG-TS copyable A/V stream → full transcode fallback"
            ));
            avformat_close_input(&mut in_ctx);
            return Err(FfmpegError::Probe(
                "no mpegts-copy-compatible audio/video stream".into(),
            ));
        }

        // ── Hybrid mode: decode the incompatible audio, encode AAC/MP3 ──────
        let audio_sample_rate = preset.audio_sample_rate.unwrap_or(48_000) as i32;
        let mut a_dec_ctx: *mut AVCodecContext = std::ptr::null_mut();
        let mut a_enc_ctx: *mut AVCodecContext = std::ptr::null_mut();
        let mut audio_enc_name: Option<String> = None;
        if plan == RemuxPlan::Hybrid {
            let audio_codec = hybrid_audio_codec_id(&preset.audio_codec);
            diag::log(format!(
                "{tag} remux hybrid: {}re-encoding audio {} → {}",
                if v_idx >= 0 {
                    "copying video, "
                } else {
                    "audio-only, "
                },
                audio_codec_id
                    .map(codec_display_name)
                    .unwrap_or_else(|| "?".into()),
                codec_display_name(audio_codec),
            ));
            a_dec_ctx = match open_audio_decoder(streams_ptr, a_idx, a_dec_codec, tag) {
                Ok(ctx) => ctx,
                Err(e) => {
                    avformat_close_input(&mut in_ctx);
                    return Err(e);
                }
            };
            match open_audio_encoder(preset, audio_codec, tag) {
                Ok((ctx, name)) => {
                    a_enc_ctx = ctx;
                    audio_enc_name = Some(name);
                }
                Err(e) => {
                    cleanup_transcode(
                        in_ctx,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        a_dec_ctx,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    );
                    return Err(e);
                }
            }
        }

        // ── Telemetry ───────────────────────────────────────────────────────
        // Both copy paths leave the video stream untouched, so no encoder was
        // probed or opened — the hardware fields are false by definition.
        let audio_mode = if plan == RemuxPlan::Hybrid {
            audio_enc_name.clone()
        } else if a_idx >= 0 {
            Some("copy".to_string())
        } else {
            None
        };
        telemetry.set_encoder(EncoderInfo {
            mode: if plan == RemuxPlan::Hybrid {
                "hybrid".into()
            } else {
                "remux".into()
            },
            // Audio-only sources copy nothing from a video stream.
            video: if v_idx >= 0 {
                Some("copy".into())
            } else {
                None
            },
            audio: audio_mode,
            hw_requested: false,
            hw_available: false,
        });
        if plan == RemuxPlan::Hybrid {
            let src_name = audio_codec_id
                .map(codec_display_name)
                .unwrap_or_else(|| "?".into());
            let enc_name = audio_enc_name.clone().unwrap_or_else(|| "aac".into());
            telemetry.warn(format!("音频 {src_name} 不可直拷，已转码为 {enc_name}"));
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
            cleanup_transcode(
                in_ctx,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                a_dec_ctx,
                std::ptr::null_mut(),
                a_enc_ctx,
            );
            return Err(ffmpeg_err("avformat_alloc_output_context2 (remux)", ret));
        }

        macro_rules! fail_remux {
            ($err:expr) => {{
                cleanup_transcode(
                    in_ctx,
                    out_ctx,
                    std::ptr::null_mut(),
                    a_dec_ctx,
                    std::ptr::null_mut(),
                    a_enc_ctx,
                );
                return Err($err);
            }};
        }

        // Only the best video and best audio streams reach the muxer; every
        // other stream (subtitles, attachments, extra audio tracks) is dropped
        // because MPEG-TS cannot carry them and they would fail the remux.
        let mut out_v_idx: i32 = -1;
        let mut out_a_idx: i32 = -1;
        if v_idx >= 0 {
            let in_st = *streams_ptr.add(v_idx as usize);
            let out_st = avformat_new_stream(out_ctx, std::ptr::null());
            if out_st.is_null() {
                fail_remux!(FfmpegError::Probe(
                    "avformat_new_stream(video) returned null (remux)".into()
                ));
            }
            let ret = avcodec_parameters_copy((*out_st).codecpar, (*in_st).codecpar);
            if ret < 0 {
                fail_remux!(ffmpeg_err("avcodec_parameters_copy(video, remux)", ret));
            }
            (*out_st).time_base = (*in_st).time_base;
            out_v_idx = (*out_st).index;
        }
        if a_idx >= 0 {
            let out_st = avformat_new_stream(out_ctx, std::ptr::null());
            if out_st.is_null() {
                fail_remux!(FfmpegError::Probe(
                    "avformat_new_stream(audio) returned null (remux)".into()
                ));
            }
            if plan == RemuxPlan::Hybrid {
                if a_enc_ctx.is_null() {
                    fail_remux!(FfmpegError::Probe(
                        "hybrid audio encoder is null (remux)".into()
                    ));
                }
                let ret = avcodec_parameters_from_context((*out_st).codecpar, a_enc_ctx);
                if ret < 0 {
                    fail_remux!(ffmpeg_err(
                        "avcodec_parameters_from_context(audio, hybrid)",
                        ret
                    ));
                }
                (*out_st).time_base = codctx_time_base(a_enc_ctx);
            } else {
                let in_st = *streams_ptr.add(a_idx as usize);
                let ret = avcodec_parameters_copy((*out_st).codecpar, (*in_st).codecpar);
                if ret < 0 {
                    fail_remux!(ffmpeg_err("avcodec_parameters_copy(audio, remux)", ret));
                }
                (*out_st).time_base = (*in_st).time_base;
            }
            out_a_idx = (*out_st).index;
        }

        // HLS muxer options. The HLS muxer is AVFMT_NOFILE: libav opens the
        // playlist itself from the output context's URL, so no `avio_open` and
        // no manual `AVFormatContext.pb` assignment is needed here.
        //
        // With a video stream the caller passes a tiny `hls_time` (1 ms) so the
        // muxer cuts on *every* keyframe: that makes the cut sequence independent
        // of the anchor a run was started at, which is what the keyframe plan
        // relies on. Without a video stream there are no keyframes to cut on, so
        // the audio-only path keeps a normal `hls_time` and cuts on time.
        let audio_only = v_idx < 0;
        let mut dict: *mut AVDictionary = std::ptr::null_mut();
        let hls_time = if audio_only {
            hls_time_seconds(preset.hls_segment_secs, None)
        } else {
            hls_time_seconds(preset.hls_segment_secs, hls_time_override_ms)
        };
        let hls_time_c = CString::new(hls_time.as_str()).unwrap();
        let start_number_c = CString::new(start_number.to_string()).unwrap();
        // No `hls_init_time`: the VOD playlist's `EXTINF` values must match the
        // real cuts, and a shorter first segment would contradict the plan.
        av_dict_set(&mut dict, c"hls_time".as_ptr(), hls_time_c.as_ptr(), 0);
        av_dict_set(&mut dict, c"hls_list_size".as_ptr(), c"0".as_ptr(), 0);
        av_dict_set(
            &mut dict,
            c"start_number".as_ptr(),
            start_number_c.as_ptr(),
            0,
        );
        if audio_only {
            // No video stream: the muxer has no keyframes, so cut on the audio
            // packet timestamps instead (`hls_time` above stays the preset value).
            av_dict_set(&mut dict, c"hls_flags".as_ptr(), c"split_by_time".as_ptr(), 0);
        }
        // With video both copy paths (pure remux and hybrid audio re-encode) cut
        // on video keyframes only; `split_by_time` is deliberately NOT set there
        // because it would let the muxer cut mid-GOP, producing segments that
        // cannot be decoded independently and contradicting the keyframe plan.
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
            fail_remux!(ffmpeg_err("avformat_write_header (remux)", ret));
        }

        // ── Read/write loop ───────────────────────────────────────────────
        let mut pkt = av_packet_alloc();
        if pkt.is_null() {
            fail_remux!(FfmpegError::Probe(
                "av_packet_alloc returned null (remux)".into()
            ));
        }
        let mut dec_frame: *mut AVFrame = std::ptr::null_mut();
        let mut out_pkt: *mut AVPacket = std::ptr::null_mut();
        if plan == RemuxPlan::Hybrid {
            dec_frame = av_frame_alloc();
            out_pkt = av_packet_alloc();
            if dec_frame.is_null() || out_pkt.is_null() {
                av_frame_free(&mut dec_frame);
                av_packet_free(&mut out_pkt);
                av_packet_free(&mut pkt);
                fail_remux!(FfmpegError::Probe(
                    "av_frame_alloc/av_packet_alloc returned null (hybrid remux)".into()
                ));
            }
        }

        let mut job = JobCtx::new(tag, telemetry, segment_dir);
        let mut write_error: Option<FfmpegError> = None;
        let mut decode_errors_logged: u32 = 0;
        let mut audio = AudioPipeline::new(audio_sample_rate, u32::from(preset.audio_channels));

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

            if plan == RemuxPlan::Hybrid && in_st_idx == a_idx as usize {
                // Audio: decode → resample → AAC/MP3, then write the encoded
                // packets into the same muxer as the copied video.
                let res = drain_audio_frames(
                    a_dec_ctx,
                    a_enc_ctx,
                    pkt,
                    dec_frame,
                    &mut audio,
                    out_pkt,
                    out_ctx,
                    out_a_idx,
                    &mut job,
                    tag,
                    &mut decode_errors_logged,
                );
                av_packet_unref(pkt);
                if let Err(e) = res {
                    write_error = Some(e);
                    break;
                }
                continue;
            }

            if in_st_idx != v_idx as usize && in_st_idx != a_idx as usize {
                // Dropped stream (subtitle/attachment/extra audio track).
                av_packet_unref(pkt);
                continue;
            }

            let in_st = *streams_ptr.add(in_st_idx);
            let out_idx = if in_st_idx == v_idx as usize {
                out_v_idx
            } else {
                out_a_idx
            };
            if in_st.is_null() || out_idx < 0 {
                av_packet_unref(pkt);
                continue;
            }
            let res = copy_packet_to_muxer(pkt, in_st, out_ctx, out_idx, &mut job);
            av_packet_unref(pkt);
            if let Err(e) = res {
                write_error = Some(e);
                break;
            }
        }

        // Flush the hybrid audio encoder so the last frames reach the muxer.
        if plan == RemuxPlan::Hybrid && write_error.is_none() && !a_enc_ctx.is_null() {
            if let Err(e) = audio.drain(a_enc_ctx, out_pkt, out_ctx, out_a_idx, &mut job, true) {
                write_error = Some(e);
            }
            if write_error.is_none() {
                if let Err(e) = encode_and_write(
                    a_enc_ctx,
                    std::ptr::null(),
                    out_pkt,
                    out_ctx,
                    out_a_idx,
                    codctx_time_base(a_enc_ctx),
                    &mut job,
                ) {
                    write_error = Some(e);
                }
            }
        }

        av_packet_free(&mut pkt);
        av_packet_free(&mut out_pkt);
        av_frame_free(&mut dec_frame);
        audio.free();
        let ret = av_write_trailer(out_ctx);
        if ret < 0 {
            diag::log(format!("{tag} remux av_write_trailer rc={ret}"));
        }
        cleanup_transcode(
            in_ctx,
            out_ctx,
            std::ptr::null_mut(),
            a_dec_ctx,
            std::ptr::null_mut(),
            a_enc_ctx,
        );

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
    start_number: u64,
    hls_time_override_ms: Option<u32>,
) -> FfmpegResult<()> {
    let url_c = CString::new(input_url).unwrap();
    let playlist = segment_dir.join("index.m3u8");
    let seg_pat = segment_dir.join("seg%d.ts");
    let playlist_c = CString::new(playlist.to_string_lossy().as_ref()).unwrap();
    let seg_pat_c = CString::new(seg_pat.to_string_lossy().as_ref()).unwrap();
    let hls_c = c"hls";

    unsafe {
        // ── Open + seek input ─────────────────────────────────────────────
        let mut in_opts = input_open_options();
        let mut in_ctx: *mut AVFormatContext = std::ptr::null_mut();
        let ret = avformat_open_input(&mut in_ctx, url_c.as_ptr(), std::ptr::null(), &mut in_opts);
        av_dict_free(&mut in_opts);
        diag::log(format!("{tag} transcode avformat_open_input rc={ret}"));
        if ret < 0 {
            return Err(ffmpeg_err("avformat_open_input (transcode)", ret));
        }
        let ret = avformat_find_stream_info(in_ctx, std::ptr::null_mut());
        diag::log(format!(
            "{tag} transcode avformat_find_stream_info rc={ret}"
        ));
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
        // Always look for a video stream: audio-only sources (music) must work
        // even when the preset asks for video, and `decode_video` is what the
        // pipeline actually does.
        let v_idx = av_find_best_stream(in_ctx, AVMEDIA_TYPE_VIDEO, -1, -1, &mut v_dec_codec, 0);
        let a_idx = av_find_best_stream(in_ctx, AVMEDIA_TYPE_AUDIO, -1, -1, &mut a_dec_codec, 0);
        let has_video_stream = v_idx >= 0 && !v_dec_codec.is_null();
        let decode_video = want_video && has_video_stream;

        let nb = fmtctx_nb_streams(in_ctx) as usize;
        let streams_ptr = fmtctx_streams(in_ctx);
        diag::log(format!(
            "{tag} transcode stream selection: video_idx={v_idx} audio_idx={a_idx} want_video={want_video} decode_video={decode_video}"
        ));
        log_streams(tag, "transcode", streams_ptr, nb);

        if want_video && !has_video_stream {
            diag::log(format!(
                "{tag} transcode: source has no decodable video stream → audio-only output"
            ));
            telemetry.warn("源文件没有视频流，已转为纯音频输出");
        }
        if !decode_video && a_idx < 0 {
            diag::log(format!(
                "{tag} transcode failed: neither a decodable audio nor video stream was found"
            ));
            avformat_close_input(&mut in_ctx);
            return Err(FfmpegError::Probe(
                "no decodable audio or video stream for transcode".into(),
            ));
        }

        // ── Open decoders ─────────────────────────────────────────────────
        let mut v_dec_ctx: *mut AVCodecContext = std::ptr::null_mut();
        let mut a_dec_ctx: *mut AVCodecContext = std::ptr::null_mut();

        if decode_video && !v_dec_codec.is_null() {
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
                cleanup_transcode(
                    in_ctx,
                    std::ptr::null_mut(),
                    v_dec_ctx,
                    a_dec_ctx,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                );
                return Err(ffmpeg_err(
                    "avcodec_parameters_to_context(video decoder)",
                    ret,
                ));
            }
            let ret = avcodec_open2(v_dec_ctx, v_dec_codec, std::ptr::null_mut());
            diag::log(format!(
                "{tag} transcode avcodec_open2(video decoder {dec_name}) rc={ret}"
            ));
            if ret < 0 {
                cleanup_transcode(
                    in_ctx,
                    std::ptr::null_mut(),
                    v_dec_ctx,
                    a_dec_ctx,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                );
                return Err(ffmpeg_err(
                    format!("avcodec_open2(video decoder {dec_name})"),
                    ret,
                ));
            }
        }
        if a_idx >= 0 && !a_dec_codec.is_null() {
            match open_audio_decoder(streams_ptr, a_idx, a_dec_codec, tag) {
                Ok(ctx) => a_dec_ctx = ctx,
                Err(e) => {
                    cleanup_transcode(
                        in_ctx,
                        std::ptr::null_mut(),
                        v_dec_ctx,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    );
                    return Err(e);
                }
            }
        }

        // ── Video encoder selection + open ────────────────────────────────
        // Selection and opening happen in one pass: hardware usability must be
        // proven with a real `avcodec_open2` because a compiled-in
        // `h264_mediacodec` can still fail on a device without that codec. The
        // fallback to libx264 is never silent — it goes to `EncoderInfo.warnings`
        // and the diag log with the failure reason.
        let mut v_enc_ctx: *mut AVCodecContext = std::ptr::null_mut();
        let mut v_enc_codec: *const AVCodec = std::ptr::null();
        let mut v_enc_name: Option<String> = None;
        let mut v_enc_pix_fmt: c_int = AV_PIX_FMT_YUV420P;
        let mut hw_available = false;

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
                cleanup_transcode(
                    in_ctx,
                    std::ptr::null_mut(),
                    v_dec_ctx,
                    a_dec_ctx,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                );
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

        // ── Open video encoder (hardware probe, then software fallback) ────
        if decode_video {
            let in_st = *streams_ptr.add(v_idx as usize);
            let src_w = (*(*in_st).codecpar).width;
            let src_h = (*(*in_st).codecpar).height;
            let (out_w, out_h) = fit_dimensions(
                src_w,
                src_h,
                preset.video_max_w.map(|v| v as i32),
                preset.video_max_h.map(|v| v as i32),
            );
            let src_rate = stream_r_frame_rate(in_st);
            let (fps_num, fps_den) =
                encoder_frame_rate(src_rate.num, src_rate.den, preset.video_fps_cap);
            let framerate = AVRational::new(fps_num, fps_den);
            // GOP frames are sized for the frames the encoder actually sees, not
            // for the capped hint (see `encoder_grid_rate`).
            let (video_gop, _) = encode_grid(src_rate, framerate, preset.hls_segment_secs);
            let hw_name = hw_video_encoder_name(preset.video_codec.as_deref());
            let hw_name_c = CString::new(hw_name).unwrap();
            let hw_codec = if preset.video_hw_preferred {
                avcodec_find_encoder_by_name(hw_name_c.as_ptr())
            } else {
                std::ptr::null()
            };
            let hw_present = !hw_codec.is_null();
            let mut hw_open_error: Option<String> = None;
            if hw_present {
                match open_hw_video_encoder(
                    tag,
                    hw_name,
                    hw_codec,
                    preset,
                    out_w,
                    out_h,
                    framerate,
                    video_gop,
                ) {
                    Ok((ctx, pix_fmt)) => {
                        v_enc_ctx = ctx;
                        v_enc_codec = hw_codec;
                        v_enc_pix_fmt = pix_fmt;
                        v_enc_name = Some(hw_name.to_string());
                        hw_available = true;
                    }
                    Err(err) => hw_open_error = Some(err),
                }
            }
            // Pure decision after the probe. Hardware is selected only when it
            // actually opened, so the warning names the real fallback reason.
            let choice = choose_video_encoder(
                preset.video_codec.as_deref(),
                preset.video_hw_preferred,
                hw_present,
                hw_available,
            );
            diag::log(format!(
                "{tag} transcode encoder choice: requested_hw={hw_name} present={hw_present} opened={hw_available} fps={}/{} source_fps={}/{} gop={}",
                framerate.num,
                framerate.den,
                src_rate.num,
                src_rate.den,
                video_gop
            ));
            if let Some(warning) = fallback_warning(choice, hw_name, hw_open_error.as_deref()) {
                diag::log(format!("{tag} transcode hardware fallback: {warning}"));
                telemetry.warn(warning);
            }
            if !hw_available {
                // Software path: libx264 first, then any H.264 encoder. The key
                // difference to a generic encode is that the GOP is pinned to
                // one segment so the exact plan grid is what the muxer cuts on.
                v_enc_codec = avcodec_find_encoder_by_name(c"libx264".as_ptr());
                if !v_enc_codec.is_null() {
                    v_enc_name = Some("libx264".into());
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
                    cleanup_transcode(
                        in_ctx,
                        std::ptr::null_mut(),
                        v_dec_ctx,
                        a_dec_ctx,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    );
                    return Err(FfmpegError::Probe(
                        "no H.264 encoder available (libx264 missing)".into(),
                    ));
                }
                v_enc_pix_fmt = AV_PIX_FMT_YUV420P;
                v_enc_ctx = avcodec_alloc_context3(v_enc_codec);
                if v_enc_ctx.is_null() {
                    cleanup_transcode(
                        in_ctx,
                        std::ptr::null_mut(),
                        v_dec_ctx,
                        a_dec_ctx,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    );
                    return Err(FfmpegError::Probe(
                        "avcodec_alloc_context3(video encoder) returned null".into(),
                    ));
                }
                codctx_set_width(v_enc_ctx, out_w);
                codctx_set_height(v_enc_ctx, out_h);
                codctx_set_pix_fmt(v_enc_ctx, v_enc_pix_fmt);
                let sw_gop = video_gop;
                codctx_set_time_base(v_enc_ctx, AVRational::new(1, 90000));
                codctx_set_gop_size(v_enc_ctx, sw_gop);
                codctx_set_max_b_frames(v_enc_ctx, 2);
                codctx_set_framerate(v_enc_ctx, framerate);
                // Force an IDR exactly every `sw_gop` frames: `keyint` = one
                // segment, `min-keyint` blocks early IDRs, `scenecut=0` stops
                // scene changes from shifting the cut points off the plan grid.
                let mut enc_opts: *mut AVDictionary = std::ptr::null_mut();
                if v_enc_name.as_deref() == Some("libx264") {
                    let params =
                        CString::new(format!("keyint={sw_gop}:min-keyint={sw_gop}:scenecut=0"))
                            .unwrap();
                    av_dict_set(&mut enc_opts, c"x264-params".as_ptr(), params.as_ptr(), 0);
                }
                // mpegtsenc only inserts h264_mp4toannexb when the stream
                // carries AVCC extradata, which libx264 emits for global header
                // mode. Without this the muxer has no SPS/PPS to start from.
                // CLOSED_GOP keeps every segment independently decodable.
                codctx_set_flags(
                    v_enc_ctx,
                    codctx_flags(v_enc_ctx) | AV_CODEC_FLAG_GLOBAL_HEADER | AV_CODEC_FLAG_CLOSED_GOP,
                );
                if let Some(br) = preset.video_max_bitrate {
                    codctx_set_bit_rate(v_enc_ctx, br as i64);
                }
                let ret = avcodec_open2(v_enc_ctx, v_enc_codec, &mut enc_opts);
                av_dict_free(&mut enc_opts);
                diag::log(format!(
                    "{tag} transcode avcodec_open2(video encoder {}) rc={ret}",
                    v_enc_name.as_deref().unwrap_or("?")
                ));
                if ret < 0 {
                    let name = v_enc_name.clone().unwrap_or_else(|| "h264".into());
                    cleanup_transcode(
                        in_ctx,
                        std::ptr::null_mut(),
                        v_dec_ctx,
                        a_dec_ctx,
                        v_enc_ctx,
                        std::ptr::null_mut(),
                    );
                    return Err(ffmpeg_err(
                        format!("avcodec_open2(video encoder {name})"),
                        ret,
                    ));
                }
            }
            diag::log(format!(
                "{tag} transcode encoder selected video={} mode={} pix_fmt={}",
                v_enc_name.as_deref().unwrap_or("?"),
                if hw_available { "hw" } else { "sw" },
                pix_fmt_name(v_enc_pix_fmt)
            ));
        }

        let mut a_enc_ctx: *mut AVCodecContext = std::ptr::null_mut();
        let mut a_enc_name: Option<String> = None;
        let audio_sample_rate = preset.audio_sample_rate.unwrap_or(48_000) as i32;
        if !a_enc_codec.is_null() && !a_dec_ctx.is_null() {
            match open_audio_encoder(preset, a_codec_id, tag) {
                Ok((ctx, name)) => {
                    a_enc_ctx = ctx;
                    a_enc_name = Some(name);
                }
                Err(e) => {
                    cleanup_transcode(
                        in_ctx,
                        std::ptr::null_mut(),
                        v_dec_ctx,
                        a_dec_ctx,
                        v_enc_ctx,
                        std::ptr::null_mut(),
                    );
                    return Err(e);
                }
            }
        }

        // A job with no encoder cannot produce any output: fail before the
        // muxer writes a header instead of reporting an empty success.
        if v_enc_ctx.is_null() && a_enc_ctx.is_null() {
            diag::log(format!(
                "{tag} transcode failed: no usable video or audio encoder"
            ));
            cleanup_transcode(
                in_ctx,
                std::ptr::null_mut(),
                v_dec_ctx,
                a_dec_ctx,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            return Err(FfmpegError::Probe(
                "transcode produced no output stream (no encoder opened)".into(),
            ));
        }

        telemetry.set_encoder(EncoderInfo {
            mode: if hw_available {
                "hw".into()
            } else {
                "sw".into()
            },
            video: v_enc_name.clone(),
            audio: if audio_stream_present {
                Some(if a_codec_id == 0 {
                    "copy".to_string()
                } else {
                    a_enc_name.unwrap_or_else(|| preset.audio_codec.clone())
                })
            } else {
                None
            },
            hw_requested: preset.video_hw_preferred && decode_video,
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
            cleanup_transcode(
                in_ctx,
                std::ptr::null_mut(),
                v_dec_ctx,
                a_dec_ctx,
                v_enc_ctx,
                a_enc_ctx,
            );
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
        //
        // With a video encoder the caller passes a tiny `hls_time` (1 ms) and the
        // encoder is forced to emit an IDR every `segment_gop_size` frames, so the
        // muxer cuts exactly on the transcode plan's uniform grid — independent of
        // the anchor a run was started at. Pure audio keeps a normal `hls_time`.
        let audio_only = v_enc_ctx.is_null();
        let mut dict: *mut AVDictionary = std::ptr::null_mut();
        let hls_time = if audio_only {
            hls_time_seconds(preset.hls_segment_secs, None)
        } else {
            hls_time_seconds(preset.hls_segment_secs, hls_time_override_ms)
        };
        let hls_time_c = CString::new(hls_time.as_str()).unwrap();
        let start_number_c = CString::new(start_number.to_string()).unwrap();
        // No `hls_init_time`: the VOD playlist's `EXTINF` values must match the
        // real cuts, and a shorter first segment would contradict the plan.
        av_dict_set(&mut dict, c"hls_time".as_ptr(), hls_time_c.as_ptr(), 0);
        av_dict_set(&mut dict, c"hls_list_size".as_ptr(), c"0".as_ptr(), 0);
        av_dict_set(
            &mut dict,
            c"start_number".as_ptr(),
            start_number_c.as_ptr(),
            0,
        );
        if audio_only {
            // No video stream: the muxer has no keyframes, so cut on the audio
            // packet timestamps instead (`hls_time` above stays the preset value).
            av_dict_set(&mut dict, c"hls_flags".as_ptr(), c"split_by_time".as_ptr(), 0);
        }
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
        let mut audio = AudioPipeline::new(audio_sample_rate, u32::from(preset.audio_channels));
        let mut enc_vframe: *mut AVFrame = std::ptr::null_mut();
        let mut video_out_dims = (0i32, 0i32);
        // Decoded video frames carry pts in the **input stream** time_base.
        // libav does not set `AVCodecContext.time_base` for video decoders (it
        // stays 0/1), so reading it silently scales every frame to pts 0; that
        // was the mpegts "non monotonically increasing dts" EINVAL.
        let mut video_dec_tb = AVRational::new(0, 1);
        if !v_enc_ctx.is_null() {
            let in_st = *streams_ptr.add(v_idx as usize);
            video_dec_tb = (*in_st).time_base;
            let cp = (*in_st).codecpar;
            video_out_dims = fit_dimensions(
                (*cp).width,
                (*cp).height,
                preset.video_max_w.map(|v| v as i32),
                preset.video_max_h.map(|v| v as i32),
            );
            enc_vframe = av_frame_alloc();
            if enc_vframe.is_null() {
                free_loop_state(
                    &mut in_pkt,
                    &mut out_pkt,
                    &mut dec_frame,
                    &mut enc_vframe,
                    &mut sws,
                    &mut audio,
                );
                cleanup_transcode(in_ctx, out_ctx, v_dec_ctx, a_dec_ctx, v_enc_ctx, a_enc_ctx);
                return Err(FfmpegError::Probe(
                    "av_frame_alloc(encoder frame) returned null".into(),
                ));
            }
            frame_set_width(enc_vframe, video_out_dims.0);
            frame_set_height(enc_vframe, video_out_dims.1);
            frame_set_format(enc_vframe, v_enc_pix_fmt);
            let ret = av_frame_get_buffer(enc_vframe, 32);
            if ret < 0 {
                free_loop_state(
                    &mut in_pkt,
                    &mut out_pkt,
                    &mut dec_frame,
                    &mut enc_vframe,
                    &mut sws,
                    &mut audio,
                );
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
                                v_enc_pix_fmt,
                                2,
                                std::ptr::null(),
                                std::ptr::null(),
                                std::ptr::null(),
                            );
                            if sws.is_null() {
                                fatal =
                                    Some(FfmpegError::Probe("sws_getContext returned null".into()));
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
                                video_dec_tb,
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
            } else if stream_idx == a_idx && !a_dec_ctx.is_null() && !a_enc_ctx.is_null() {
                if let Err(e) = drain_audio_frames(
                    a_dec_ctx,
                    a_enc_ctx,
                    in_pkt,
                    dec_frame,
                    &mut audio,
                    out_pkt,
                    out_ctx,
                    out_a_idx,
                    &mut job,
                    tag,
                    &mut decode_errors_logged,
                ) {
                    fatal = Some(e);
                    break 'decode;
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
            if let Err(e) = audio.drain(a_enc_ctx, out_pkt, out_ctx, out_a_idx, &mut job, true) {
                fatal = Some(e);
            }
            if fatal.is_none() {
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
        }

        // ── Cleanup ───────────────────────────────────────────────────────
        if !sws.is_null() {
            sws_freeContext(sws);
        }
        audio.free();
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
        let dts = (*out_pkt).dts;
        let duration = (*out_pkt).duration;
        let size = (*out_pkt).size;
        let tb = (*out_st).time_base;
        let ret = av_interleaved_write_frame(out_ctx, out_pkt);
        if ret < 0 {
            diag::log(format!(
                "{} write FAILED stream={} pts={pts} dts={dts} dur={duration} size={size} tb={}/{} rc={ret}",
                job.tag, out_stream_idx, tb.num, tb.den
            ));
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
    audio: &mut AudioPipeline,
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
    audio.free();
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
