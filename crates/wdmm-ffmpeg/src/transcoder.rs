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

use std::collections::HashMap;
use std::ffi::{c_char, c_int, CStr, CString};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::sys::*;
use crate::{
    diag, hwdecode, AudioStream, EncoderInfo, FfmpegError, FfmpegResult, PauseControl, ProbeResult,
    ProgressState, SessionDecision, SourcePlan, StreamMode, SubtitleStream, TranscodePreset,
    VideoStream,
};

/// Time base used for millisecond content timestamps.
const MS_TB: AVRational = AVRational::new(1, 1000);

/// How often (~ms of wall time) to emit a progress diagnostic line.
const PROGRESS_LOG_EVERY_MS: i64 = 2_000;

/// How many times one decoded audio frame may be retried after the resampler
/// reports that the source description changed. A legacy AVI typically flips
/// once (unspecified → concrete channel layout); the cap only stops a source
/// that changes on every frame from spinning.
const MAX_AUDIO_RECONFIGURES: u32 = 8;

// ── Stream decision (pure logic lives in `crate::plan`/`crate::SessionDecision`) ─

use crate::plan::{
    decide_video_action, encoder_frame_rate, hw_gop_size, remux_dropped_streams, segment_gop_size,
    target_video_bitrate_for_tier, video_plan_diag, x264_preset_for_tier,
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
/// passed with up to three decimals. A 1 ms override makes the muxer cut on
/// every video keyframe (video-copy paths) or every forced IDR (encode path).
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

        // FFmpeg 9.0 LP64 AVFormatContext: av_class @0, iformat @8, oformat @16,
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
/// Loosest sane assumption about keyframe density: one keyframe per ten
/// seconds of media.
///
/// An index holding fewer entries than that cannot describe the media, so it is
/// treated as not-yet-parsed and one `seek(0)` is spent to finish it. Counting
/// against the duration matters: Matroska parses `Cues` lazily, but the linear
/// prefix of a long file can still hold a handful of entries, and the previous
/// "fewer than 3 entries" test waved exactly that through as complete — a
/// 24-minute MKV with 3 entries looked fine and the plan fell back to a full
/// re-encode.
const ASSUMED_MAX_GOP_MS: i64 = 10_000;

unsafe fn video_keyframe_index_ms(
    in_ctx: *mut AVFormatContext,
    streams_ptr: *const *mut AVStream,
    v_idx: i32,
    duration_ms: i64,
) -> Vec<i64> {
    let mut keyframes = collect_keyframes_ms(streams_ptr, v_idx);
    let sparse = keyframes.len() < 2
        || (keyframes.len() as i64).saturating_mul(ASSUMED_MAX_GOP_MS) < duration_ms;
    if sparse {
        let before = keyframes.len();
        let ret = av_seek_frame(in_ctx, -1, 0, AVSEEK_FLAG_BACKWARD);
        diag::log(format!(
            "source plan: keyframe index sparse entries_before={before} duration_ms={duration_ms} seek(0) rc={ret}"
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
            diag::log(format!(
                "source plan: {url} has no duration, deferring to warm-up"
            ));
            avformat_close_input(&mut in_ctx);
            return Ok(None);
        };

        let streams_ptr = fmtctx_streams(in_ctx);
        let v_idx =
            av_find_best_stream(in_ctx, AVMEDIA_TYPE_VIDEO, -1, -1, std::ptr::null_mut(), 0);
        let a_idx =
            av_find_best_stream(in_ctx, AVMEDIA_TYPE_AUDIO, -1, -1, std::ptr::null_mut(), 0);
        let video_codec = stream_codec_id(streams_ptr, v_idx);
        let audio_codec = stream_codec_id(streams_ptr, a_idx);
        let seg_secs = f64::from(preset.hls_segment_secs);

        // Decide the per-stream layout once; the plan below and every job run of
        // this session must use the same decision (contract §2).
        let needs_encode = video_codec.is_some_and(|id| {
            !(preset.video_copy && crate::preset::is_mpegts_video_copy_compatible(id))
        });
        let hw_available = if needs_encode {
            hw_video_encoder_available(preset, "source plan")
        } else {
            false
        };
        let decision = SessionDecision::decide(
            preset,
            video_codec.unwrap_or(0),
            audio_codec.unwrap_or(0),
            hw_available,
        )
        .map_err(FfmpegError::Probe)?;
        diag::log(format!(
            "source plan: {url} stream decision video={} audio={}",
            decision.video_action_str(),
            decision.audio_action_str()
        ));

        let plan = if video_codec.is_none() {
            // Audio-only HLS cuts on audio packet timing; there is no index that
            // predicts it, so the warm-up calibration stays in charge.
            None
        } else if decision.video_is_copy() {
            let mut keyframes = video_keyframe_index_ms(in_ctx, streams_ptr, v_idx, duration_ms);
            // MP4 indexes store DTS, and the B-frame delay makes the first
            // keyframe's DTS negative. Anchor the list on the first video
            // packet's PTS so both the proxy's seek offset and the muxer's
            // first cut land on the keyframe the plan names. Matroska indexes
            // already use PTS, so the shift is zero there.
            if let (Some(first_pts), Some(first_kf)) = (
                first_video_packet_pts_ms(in_ctx, streams_ptr, v_idx),
                keyframes.first().copied(),
            ) {
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
                        decision: decision.clone(),
                    })
                }
                None => {
                    let first = keyframes.first().copied().unwrap_or(-1);
                    let last = keyframes.last().copied().unwrap_or(-1);
                    let gop = keyframes
                        .windows(2)
                        .map(|w| w[1] - w[0])
                        .filter(|gap| *gap > 0)
                        .min()
                        .unwrap_or(-1);
                    diag::log(format!(
                        "source plan: {url} keyframe index unusable (entries={count} first_ms={first} last_ms={last} min_gap_ms={gop} duration_ms={duration_ms}), deferring to warm-up"
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
                decision,
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

/// One text subtitle stream decoded by [`extract_subtitle_tracks`].
struct SubtitleTrack {
    index: u32,
    codec: String,
    time_base: AVRational,
    decoder: *mut AVCodecContext,
    cues: Vec<crate::subtitle::Cue>,
    packets: u64,
    decode_errors: u32,
}

/// Validate a subtitle stream and open a text decoder for it.
unsafe fn open_subtitle_track(
    fmt_ctx: *mut AVFormatContext,
    stream_index: u32,
) -> FfmpegResult<SubtitleTrack> {
    use crate::subtitle::{self, TrackKind};

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

    Ok(SubtitleTrack {
        index: stream_index,
        codec,
        time_base: (*st).time_base,
        decoder: dec_ctx,
        cues: Vec::new(),
        packets: 0,
        decode_errors: 0,
    })
}

/// Decode one subtitle packet into `track.cues`.
unsafe fn decode_subtitle_packet(track: &mut SubtitleTrack, pkt: *mut AVPacket) {
    track.packets += 1;
    let fallback_pts_ms = if (*pkt).pts == AV_NOPTS_VALUE {
        0
    } else {
        av_rescale_q((*pkt).pts, track.time_base, MS_TB).max(0)
    };
    let mut sub: AVSubtitle = std::mem::zeroed();
    let mut got: c_int = 0;
    let ret = avcodec_decode_subtitle2(track.decoder, &mut sub, &mut got, pkt);
    if ret < 0 {
        track.decode_errors += 1;
        if track.decode_errors <= 3 {
            diag::log(format!(
                "[subtitle] decode error #{} in '{}' stream {}: {}",
                track.decode_errors,
                track.codec,
                track.index,
                av_err2str(ret)
            ));
        }
    }
    if got != 0 {
        let cue = cue_from_avsubtitle(&sub, fallback_pts_ms, &track.codec);
        avsubtitle_free(&mut sub);
        if let Some(cue) = cue {
            track.cues.push(cue);
        }
    }
}

/// Decode the subtitle stream at `stream_index` of an already-opened input.
unsafe fn extract_subtitle_inner(
    fmt_ctx: *mut AVFormatContext,
    stream_index: u32,
) -> FfmpegResult<String> {
    use crate::subtitle::{normalize_cues, to_webvtt};

    let mut track = open_subtitle_track(fmt_ctx, stream_index)?;
    let mut pkt = av_packet_alloc();
    if pkt.is_null() {
        avcodec_free_context(&mut track.decoder);
        return Err(FfmpegError::Probe("av_packet_alloc returned NULL".into()));
    }

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
            decode_subtitle_packet(&mut track, pkt);
        }
        av_packet_unref(pkt);
        if track.packets >= SUBTITLE_MAX_PACKETS || track.cues.len() >= crate::subtitle::MAX_CUES {
            diag::log(format!(
                "[subtitle] stopping extraction early after {} packets / {} cues",
                track.packets,
                track.cues.len()
            ));
            break;
        }
    }

    av_packet_free(&mut pkt);
    avcodec_free_context(&mut track.decoder);

    let cues = normalize_cues(track.cues);
    if cues.is_empty() {
        return Err(FfmpegError::Probe(format!(
            "no subtitle cues decoded from '{}' stream {stream_index} \
             ({} packets, {} decode error(s){}",
            track.codec,
            track.packets,
            track.decode_errors,
            read_error
                .map(|code| format!(", read ended with {}", av_err2str(code)))
                .unwrap_or_default()
        )));
    }
    diag::log(format!(
        "[subtitle] extracted {} cues from '{}' stream {stream_index} ({} packets, {} decode error(s))",
        cues.len(),
        track.codec,
        track.packets,
        track.decode_errors
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

/// Extract several embedded **text** subtitle streams in one demux pass.
///
/// Opens the source once, opens one decoder per requested text stream, and
/// fans the demuxed packets out to the right decoder. Bitmap tracks
/// (PGS/DVD/DVB) and unsupported codecs are skipped with a diagnostic and are
/// absent from the returned map, so one bad track never fails the others.
/// Streams that produce no cues are skipped the same way.
pub fn extract_subtitle_tracks(
    url: &str,
    stream_indexes: &[u32],
) -> FfmpegResult<HashMap<u32, String>> {
    use crate::subtitle::{normalize_cues, to_webvtt};
    use std::time::Instant;

    let mut result = HashMap::new();
    if stream_indexes.is_empty() {
        return Ok(result);
    }
    let started = Instant::now();
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
        if ret < 0 {
            avformat_close_input(&mut fmt_ctx);
            return Err(ffmpeg_err("avformat_find_stream_info(subtitle)", ret));
        }

        let mut tracks: Vec<SubtitleTrack> = Vec::new();
        let mut seen: Vec<u32> = Vec::new();
        for &index in stream_indexes {
            if seen.contains(&index) {
                continue;
            }
            seen.push(index);
            match open_subtitle_track(fmt_ctx, index) {
                Ok(track) => tracks.push(track),
                Err(error) => diag::log(format!("[subtitle] skipping stream {index}: {error}")),
            }
        }
        if tracks.is_empty() {
            avformat_close_input(&mut fmt_ctx);
            return Ok(result);
        }

        let mut pkt = av_packet_alloc();
        if pkt.is_null() {
            for track in &mut tracks {
                avcodec_free_context(&mut track.decoder);
            }
            avformat_close_input(&mut fmt_ctx);
            return Err(FfmpegError::Probe("av_packet_alloc returned NULL".into()));
        }

        let mut packets_read = 0u64;
        loop {
            let ret = av_read_frame(fmt_ctx, pkt);
            if ret < 0 {
                break;
            }
            packets_read += 1;
            let index = (*pkt).stream_index as u32;
            if let Some(track) = tracks.iter_mut().find(|t| t.index == index) {
                if track.cues.len() < crate::subtitle::MAX_CUES {
                    decode_subtitle_packet(track, pkt);
                }
            }
            av_packet_unref(pkt);
            let all_capped = tracks
                .iter()
                .all(|t| t.cues.len() >= crate::subtitle::MAX_CUES);
            if packets_read >= SUBTITLE_MAX_PACKETS || all_capped {
                diag::log(format!(
                    "[subtitle] multi-track pass stopped after {packets_read} packets"
                ));
                break;
            }
        }
        av_packet_free(&mut pkt);

        for track in &mut tracks {
            let index = track.index;
            let codec = track.codec.clone();
            let packets = track.packets;
            let errors = track.decode_errors;
            let cues = normalize_cues(std::mem::take(&mut track.cues));
            avcodec_free_context(&mut track.decoder);
            if cues.is_empty() {
                diag::log(format!(
                    "[subtitle] stream {index} '{codec}' produced no cues \
                     ({packets} packets, {errors} decode error(s))"
                ));
                continue;
            }
            diag::log(format!(
                "[subtitle] extracted {} cues from '{codec}' stream {index} \
                 ({packets} packets, {errors} decode error(s))",
                cues.len()
            ));
            result.insert(index, to_webvtt(&cues));
        }
        avformat_close_input(&mut fmt_ctx);
        diag::log(format!(
            "[subtitle] multi-track extraction: {} track(s) from {packets_read} packets in {} ms",
            result.len(),
            started.elapsed().as_millis()
        ));
        Ok(result)
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
fn encode_grid(source_rate: AVRational, configured_rate: AVRational, seg_secs: u32) -> (i32, f64) {
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
/// `bitrate` is always applied: `mediacodecenc.c` only forwards a `bitrate` key
/// to the platform `MediaFormat` when `avctx->bit_rate` is non-zero, so leaving
/// it unset does not mean "automatic" — it hands quality to the codec's default,
/// which is far below what a 1080p stream needs. The caller passes the value
/// from [`plan_video_encode`], so an unset `video_max_bitrate` still produces a
/// resolution-derived bitrate (and the preset's tier scales it).
///
/// Returns the opened context plus the pixel format, or a diagnostic string
/// describing the last failure (caller falls back to libx264 and reports it).
unsafe fn open_hw_video_encoder(
    tag: &str,
    hw_name: &str,
    codec: *const AVCodec,
    bitrate: i64,
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
        codctx_set_bit_rate(ctx, bitrate);
        codctx_set_flags(ctx, codctx_flags(ctx) | AV_CODEC_FLAG_GLOBAL_HEADER);
        let ret = avcodec_open2(ctx, codec, std::ptr::null_mut());
        diag::log(format!(
            "{tag} transcode avcodec_open2(hw video encoder {hw_name} pix_fmt={} gop={gop} bitrate={bitrate} max_b_frames=0) rc={ret}",
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

/// Length of `ticks` in microseconds, rounded up (0 for a non-positive input).
fn ticks_to_us_ceil(ticks: i64, tb: AVRational) -> i64 {
    if ticks <= 0 || tb.num <= 0 || tb.den <= 0 {
        return 0;
    }
    let num = (ticks as i128) * 1_000_000 * (tb.num as i128);
    let den = tb.den as i128;
    ((num + den - 1) / den).min(i64::MAX as i128) as i64
}

/// Length of `us` in output ticks, rounded up (0 for a non-positive input).
fn us_to_ticks_ceil(us: i64, tb: AVRational) -> i64 {
    if us <= 0 || tb.num <= 0 || tb.den <= 0 {
        return 0;
    }
    let num = (us as i128) * (tb.den as i128);
    let den = 1_000_000i128 * (tb.num as i128);
    ((num + den - 1) / den).min(i64::MAX as i128) as i64
}

/// Per-job diagnostics + progress bookkeeping shared by both paths.
struct JobCtx<'a> {
    tag: &'a str,
    telemetry: &'a ProgressState,
    segment_dir: &'a Path,
    tracker: ProgressTracker,
    first_segment_logged: bool,
    last_progress_log_ms: i64,
    /// Shift applied to every written timestamp, in microseconds. Zero unless a
    /// source (or an encoder's B-frame delay) produced a negative timestamp.
    offset_us: i64,
    offset_logged: bool,
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
            offset_us: 0,
            offset_logged: false,
        }
    }

    /// Keep every written timestamp non-negative.
    ///
    /// MPEG-TS cannot signal a negative timestamp (`AVFMT_TS_NEGATIVE` is not
    /// set), and a B-frame encoder delays the first dts below zero, so a legacy
    /// container whose clock starts before zero (FLV/WMV) made
    /// `av_interleaved_write_frame` answer EINVAL on the very first packet. The
    /// shift is measured once and then applied to every packet of the run in
    /// both streams, so the A/V relationship is untouched.
    unsafe fn align_packet(&mut self, pkt: *mut AVPacket, tb: AVRational) {
        let mut lowest = i64::MAX;
        for value in [(*pkt).pts, (*pkt).dts] {
            if value != AV_NOPTS_VALUE {
                lowest = lowest.min(value);
            }
        }
        if lowest == i64::MAX {
            return;
        }
        let mut delta = us_to_ticks_ceil(self.offset_us, tb);
        if lowest.saturating_add(delta) < 0 {
            self.offset_us = self
                .offset_us
                .saturating_add(ticks_to_us_ceil(-(lowest + delta), tb));
            delta = us_to_ticks_ceil(self.offset_us, tb);
            if !self.offset_logged {
                self.offset_logged = true;
                diag::log(format!(
                    "{} output timestamps shifted by {} us so the muxer never sees a negative one",
                    self.tag, self.offset_us
                ));
            }
        }
        if delta != 0 {
            if (*pkt).pts != AV_NOPTS_VALUE {
                (*pkt).pts = (*pkt).pts.saturating_add(delta);
            }
            if (*pkt).dts != AV_NOPTS_VALUE {
                (*pkt).dts = (*pkt).dts.saturating_add(delta);
            }
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
    pause: &PauseControl,
    job_id: &str,
    start_number: u64,
    hls_time_override_ms: Option<u32>,
    decision: Option<SessionDecision>,
) -> FfmpegResult<()> {
    telemetry.start();
    telemetry.set_start_ms((start_offset_secs * 1000.0).round() as i64);
    let tag = job_tag(job_id);
    diag::log(format!(
        "{tag} job start preset={} hw_video_encoder={} sw_video_encoder={} video_copy={} audio_codec={} audio_copy={} start_offset={start_offset_secs:.2} start_number={start_number} hls_time_override_ms={hls_time_override_ms:?} seg_dir={} input={}",
        preset.id,
        preset.hw_video_encoder,
        preset.sw_video_encoder,
        preset.video_copy,
        preset.audio_codec,
        preset.audio_copy,
        segment_dir.display(),
        sanitize_url(input_url),
    ));

    // Per-stream decision, decided once per session (contract §2): a reopen /
    // seek reuses the decision handed in by the caller so the produced stream
    // layout can never drift between runs.
    let decision = match decision {
        Some(decision) => decision,
        None => {
            let resolved = unsafe { decide_session(input_url, preset, telemetry, &tag) }?;
            diag::log(format!(
                "{tag} stream decision (fresh) video={} audio={}",
                resolved.video_action_str(),
                resolved.audio_action_str()
            ));
            resolved
        }
    };
    telemetry.set_actions(decision.video_action_str(), decision.audio_action_str());

    // Only a video stream that actually has to be re-encoded needs the transcode
    // path. Everything else — including an audio-only source, whose decision is
    // `video = None` — goes through the remux path, which copies the video stream
    // when there is one and copies or re-encodes the audio stream.
    let video_needs_encode = matches!(decision.video, StreamMode::Hardware | StreamMode::Software);

    let result = if !video_needs_encode {
        match run_remux(
            input_url,
            preset,
            &decision,
            start_offset_secs,
            segment_dir,
            cancel.clone(),
            telemetry,
            pause,
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
                    let fallback = SessionDecision {
                        // An audio-only source has no video stream to encode; keep
                        // that decision so the transcode path stays audio-only.
                        video: if decision.video == StreamMode::None {
                            StreamMode::None
                        } else {
                            StreamMode::Software
                        },
                        // The copy marker is not an encoder name: let the software
                        // path pick the preset's encoder and tier.
                        video_encoder: None,
                        video_tier: None,
                        audio: decision.audio,
                        ..decision.clone()
                    };
                    run_transcode(
                        input_url,
                        preset,
                        &fallback,
                        start_offset_secs,
                        segment_dir,
                        cancel,
                        telemetry,
                        pause,
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
            &decision,
            start_offset_secs,
            segment_dir,
            cancel,
            telemetry,
            pause,
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

/// Decide the per-stream layout by probing the source once.
///
/// Only used when the caller has no cached decision (a fresh session); every
/// reopen passes the session's decision down, so this probe runs once per cast.
unsafe fn decide_session(
    input_url: &str,
    preset: &wdmm_core::models::TranscodePreset,
    telemetry: &ProgressState,
    tag: &str,
) -> FfmpegResult<SessionDecision> {
    let c_url = CString::new(input_url)
        .map_err(|_| FfmpegError::Probe("input URL contains an interior NUL byte".to_string()))?;
    let mut opts = input_open_options();
    let mut in_ctx: *mut AVFormatContext = std::ptr::null_mut();
    let ret = avformat_open_input(&mut in_ctx, c_url.as_ptr(), std::ptr::null_mut(), &mut opts);
    av_dict_free(&mut opts);
    if ret < 0 {
        return Err(ffmpeg_err("avformat_open_input", ret));
    }
    let probe = (|| {
        let ret = avformat_find_stream_info(in_ctx, std::ptr::null_mut());
        if ret < 0 {
            return Err(ffmpeg_err("avformat_find_stream_info", ret));
        }
        let streams = fmtctx_streams(in_ctx);
        let nb = fmtctx_nb_streams(in_ctx) as usize;
        let v_idx =
            av_find_best_stream(in_ctx, AVMEDIA_TYPE_VIDEO, -1, -1, std::ptr::null_mut(), 0);
        let a_idx =
            av_find_best_stream(in_ctx, AVMEDIA_TYPE_AUDIO, -1, -1, std::ptr::null_mut(), 0);
        let mut video_codec = 0u32;
        let mut audio_codec = 0u32;
        let mut src_w = 0u32;
        let mut src_h = 0u32;
        let mut src_rate = AVRational::new(0, 1);
        if v_idx >= 0 {
            let st = *streams.add(v_idx as usize);
            let cp = (*st).codecpar;
            video_codec = (*cp).codec_id as u32;
            src_w = (*cp).width.max(0) as u32;
            src_h = (*cp).height.max(0) as u32;
            src_rate = stream_r_frame_rate(st);
        }
        if a_idx >= 0 {
            let st = *streams.add(a_idx as usize);
            audio_codec = (*(*st).codecpar).codec_id as u32;
        }
        let _ = nb;
        let needs_video_encode = video_codec != 0
            && !(preset.video_copy && crate::preset::is_mpegts_video_copy_compatible(video_codec));
        let hw_available = if needs_video_encode {
            hw_video_encoder_available(preset, tag)
        } else {
            false
        };
        let decision = SessionDecision::decide(preset, video_codec, audio_codec, hw_available)
            .map_err(FfmpegError::Probe)?;
        if video_codec != 0 {
            let action = decide_video_action(preset, video_codec, hw_available)
                .map_err(FfmpegError::Probe)?;
            let bitrate = match action.tier() {
                Some(tier) => {
                    let params = plan_video_encode(preset, src_w, src_h, src_rate, tier);
                    params.bitrate as u32
                }
                None => 0,
            };
            diag::log(format!("{tag} {}", video_plan_diag(action, bitrate)));
        }
        Ok(decision)
    })();
    avformat_close_input(&mut in_ctx);
    let decision = probe?;
    telemetry.set_actions(decision.video_action_str(), decision.audio_action_str());
    Ok(decision)
}

/// Whether the preset's hardware video encoder is really usable: found by name
/// *and* `avcodec_open2` succeeded (contract §2 — presence alone is not proof).
unsafe fn hw_video_encoder_available(
    preset: &wdmm_core::models::TranscodePreset,
    tag: &str,
) -> bool {
    if preset.hw_video_encoder.is_empty() {
        return false;
    }
    let Ok(name) = CString::new(preset.hw_video_encoder.clone()) else {
        return false;
    };
    let codec = avcodec_find_encoder_by_name(name.as_ptr());
    if codec.is_null() {
        diag::log(format!(
            "{tag} hw encoder {} not present in libavcodec",
            preset.hw_video_encoder
        ));
        return false;
    }
    // Probe with the same bitrate the real session would use, at 720p30: an
    // encoder that only accepts a sane bitrate range must not fail here and
    // then succeed (or vice versa) during the actual cast.
    let probe_bitrate = i64::from(target_video_bitrate_for_tier(
        preset,
        1280,
        720,
        30.0,
        &preset.hw_video_tier,
    ));
    match open_hw_video_encoder(
        tag,
        &preset.hw_video_encoder,
        codec,
        probe_bitrate,
        1280,
        720,
        AVRational::new(30, 1),
        hw_gop_size(30, 1),
    ) {
        Ok((mut ctx, _)) => {
            avcodec_free_context(&mut ctx);
            true
        }
        Err(err) => {
            diag::log(format!(
                "{tag} hw encoder {} unavailable: {err}",
                preset.hw_video_encoder
            ));
            false
        }
    }
}

/// Everything the video encoder needs, derived from the preset and the source.
pub(crate) struct VideoEncodeParams {
    pub width: c_int,
    pub height: c_int,
    pub framerate: AVRational,
    pub gop: c_int,
    pub bitrate: i64,
}

/// Derive the encoder geometry/bitrate for one preset tier.
pub(crate) fn plan_video_encode(
    preset: &wdmm_core::models::TranscodePreset,
    src_w: u32,
    src_h: u32,
    src_rate: AVRational,
    tier: &str,
) -> VideoEncodeParams {
    let (width, height) = fit_dimensions(
        src_w.max(2) as i32,
        src_h.max(2) as i32,
        preset.video_max_w.map(|v| v as i32),
        preset.video_max_h.map(|v| v as i32),
    );
    let configured = encoder_frame_rate(src_rate.num, src_rate.den, preset.video_fps_cap);
    let framerate = AVRational::new(configured.0, configured.1);
    let (gop, _step_secs) = encode_grid(src_rate, framerate, preset.hls_segment_secs);
    let fps = if framerate.num > 0 && framerate.den > 0 {
        f64::from(framerate.num) / f64::from(framerate.den)
    } else {
        30.0
    };
    let bitrate = i64::from(target_video_bitrate_for_tier(
        preset,
        width.max(0) as u32,
        height.max(0) as u32,
        fps,
        tier,
    ));
    VideoEncodeParams {
        width: width as c_int,
        height: height as c_int,
        framerate,
        gop,
        bitrate,
    }
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

/// Sample format an audio encoder wants for `codec_id`.
///
/// `AV_SAMPLE_FMT_S16` — packed 16-bit samples. `sys.rs` only mirrors the planar
/// formats this crate needed before MP2 support.
const AV_SAMPLE_FMT_S16: c_int = 1;

/// MPEG-1 Layer II (`mp2`) only accepts packed 16-bit integer samples — feeding
/// it the planar float format the resampler uses for AAC/AC3/EAC3 (or the planar
/// S16P variant) makes `avcodec_open2` fail with EINVAL. The resampler and the
/// fifo handle packed formats natively, so the pipeline just has to be told.
fn audio_sample_format(codec_id: AVCodecID) -> c_int {
    if codec_id == crate::preset::codec_ids::MP2 {
        AV_SAMPLE_FMT_S16
    } else {
        AV_SAMPLE_FMT_FLTP
    }
}

/// Snap a frame rate to one of the eight rates MPEG-2 can signal.
///
/// `mpeg12enc` derives its `frame_rate_code` from the frame rate and rejects
/// everything else — a 97/6 (~16.17 fps) anime source makes `avcodec_open2`
/// return EINVAL, even though the decoder read it fine. The nearest standard
/// rate keeps the cast working; the caller logs the substitution.
fn mpeg2_frame_rate(src: AVRational) -> AVRational {
    const RATES: [(i32, i32); 8] = [
        (24000, 1001),
        (24, 1),
        (25, 1),
        (30000, 1001),
        (30, 1),
        (50, 1),
        (60000, 1001),
        (60, 1),
    ];
    if src.num <= 0 || src.den <= 0 {
        return AVRational::new(24000, 1001);
    }
    let fps = src.num as f64 / src.den as f64;
    let mut best = RATES[0];
    let mut best_delta = f64::INFINITY;
    for (num, den) in RATES {
        let delta = (num as f64 / den as f64 - fps).abs();
        if delta < best_delta {
            best_delta = delta;
            best = (num, den);
        }
    }
    AVRational::new(best.0, best.1)
}

unsafe fn open_audio_encoder(
    preset: &wdmm_core::models::TranscodePreset,
    codec_id: AVCodecID,
    tag: &str,
) -> FfmpegResult<(*mut AVCodecContext, String, c_int)> {
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
    let sample_fmt = audio_sample_format(codec_id);
    codctx_set_sample_fmt(enc_ctx, sample_fmt);
    let layout = if preset.audio_channels == 1 {
        AVChannelLayout::mono()
    } else {
        AVChannelLayout::stereo()
    };
    codctx_set_ch_layout(enc_ctx, layout);
    codctx_set_bit_rate(enc_ctx, preset.audio_bitrate as i64);
    let ret = avcodec_open2(enc_ctx, enc_codec, std::ptr::null_mut());
    diag::log(format!(
        "{tag} avcodec_open2(audio encoder {enc_name} sample_fmt={sample_fmt} rate={audio_sample_rate} bitrate={}) rc={ret}",
        preset.audio_bitrate
    ));
    if ret < 0 {
        avcodec_free_context(&mut enc_ctx);
        return Err(ffmpeg_err(
            format!("avcodec_open2(audio encoder {enc_name} sample_fmt={sample_fmt})"),
            ret,
        ));
    }
    Ok((enc_ctx, enc_name, sample_fmt))
}

/// Repairs timestamps for sources that do not carry per-packet time.
///
/// AVI stores a frame index instead of a pts, so an H.264 stream in AVI reaches
/// the muxer with `AV_NOPTS_VALUE` on every packet and `av_interleaved_write_frame`
/// answers EINVAL (`Invalid argument`). Such a stream is given a synthetic clock
/// taken from the packet duration (copy) or the encoder frame rate (transcode);
/// real timestamps are always kept, and an encoder input is nudged forward when
/// the source repeats or rewinds its clock.
struct TimestampRepair {
    /// Nominal distance between two timestamps, in the output time base.
    step: i64,
    /// Where the synthetic clock starts: the point this run begins at, so a
    /// seeked run keeps the container timeline instead of restarting at zero.
    seed: i64,
    /// Shift applied to real timestamps so the first one lands on `seed`.
    offset: Option<i64>,
    last: Option<i64>,
}

impl TimestampRepair {
    const fn new() -> Self {
        Self {
            step: 0,
            seed: 0,
            offset: None,
            last: None,
        }
    }

    /// Synthetic clock that begins at `seed` output ticks.
    const fn seeded(seed: i64) -> Self {
        Self {
            step: 0,
            seed,
            offset: None,
            last: None,
        }
    }

    /// Remember the frame length so a gap can be filled with the right step.
    fn observe_step(&mut self, step: i64) {
        if step > 0 {
            self.step = step;
        }
    }

    /// Continue the synthetic clock from the last known timestamp.
    fn fill_missing(&mut self) -> i64 {
        let step = self.step.max(1);
        let next = self.last.map(|last| last + step).unwrap_or(self.seed);
        self.last = Some(next);
        next
    }

    /// How far a real timestamp has to move for the run to start at `seed`.
    ///
    /// Streams of one file disagree about where a seek left them: a container
    /// without per-packet time answers `0` for audio while the video packets
    /// still report the scan position. One anchor per run is what keeps the two
    /// tracks in step; a run that starts at zero keeps the container clock
    /// verbatim.
    fn align_offset(&mut self, raw: i64) -> i64 {
        if self.seed == 0 {
            return 0;
        }
        match self.offset {
            Some(offset) => offset,
            None => {
                let offset = self.seed - raw;
                self.offset = Some(offset);
                offset
            }
        }
    }

    /// Record a real timestamp so a later gap continues from it.
    fn note(&mut self, timestamp: i64) {
        if timestamp != AV_NOPTS_VALUE {
            self.last = Some(timestamp);
        }
    }

    /// Timestamp for an encoder input frame: fills gaps and makes the sequence
    /// strictly increasing (libx264 warns about non-monotonic pts and the muxer
    /// rejects it).
    fn repair_monotonic(&mut self, mut raw: i64) -> i64 {
        if raw != AV_NOPTS_VALUE {
            raw = raw.saturating_add(self.align_offset(raw));
        }
        let step = self.step.max(1);
        let next = match self.last {
            None if raw == AV_NOPTS_VALUE => self.seed,
            None => raw.max(self.seed),
            Some(last) if raw == AV_NOPTS_VALUE || raw <= last => last + step,
            Some(_) => raw,
        };
        self.last = Some(next);
        next
    }
}

/// Output ticks corresponding to the media position where this run starts.
///
/// A run that begins mid-file (a seek) has to stamp its frames on the container
/// timeline: a source without per-packet time would otherwise start at zero and
/// drift away from the audio track, which is seeded from the same offset.
unsafe fn run_start_ticks(
    out_ctx: *mut AVFormatContext,
    out_idx: i32,
    start_offset_secs: f64,
) -> i64 {
    if out_idx < 0 || out_ctx.is_null() {
        return 0;
    }
    let streams = fmtctx_streams(out_ctx);
    if streams.is_null() {
        return 0;
    }
    start_ticks_in(
        (*(*streams.add(out_idx as usize))).time_base,
        start_offset_secs,
    )
}

/// Same as [`run_start_ticks`], for callers that already know the time base
/// (the encoder's, before the muxer rescales).
unsafe fn start_ticks_in(time_base: AVRational, start_offset_secs: f64) -> i64 {
    if start_offset_secs <= 0.01 {
        return 0;
    }
    if time_base.num <= 0 || time_base.den <= 0 {
        return 0;
    }
    let micros = (start_offset_secs * 1_000_000.0).round() as i64;
    av_rescale_q(micros, AVRational::new(1, 1_000_000), time_base)
}

/// How far the first decoded frame may sit from the run's start position before
/// its timestamp is treated as a container restart rather than a real clock.
const AUDIO_ANCHOR_TOLERANCE_SECS: f64 = 3.0;

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
    /// Sample format the target encoder accepts (FLTP for AAC/AC3/EAC3, S16P
    /// for MPEG-1 Layer II). The resampler, the FIFO and every encoder-input
    /// frame are described with it, so a format the encoder refuses can never
    /// reach `avcodec_open2`/`avcodec_send_frame`.
    out_format: c_int,
    frame_size: c_int,
    next_pts: i64,
    /// Encoder-time-base pts of the first queued sample, seeded from the source
    /// timeline. Copy runs keep the container timestamps for video, so a run
    /// that starts mid-file must not restart audio at zero: the rendered file
    /// would then carry two unrelated clocks (audio near 0, video near the seek
    /// point) and the player reports the mismatch as drift or resyncs the
    /// playhead backwards.
    start_samples: i64,
    /// Decoder time_base of the audio stream, used to reseat `next_pts` on the
    /// first decoded frame. Zero means "unknown" and keeps `start_samples`.
    dec_tb: AVRational,
    ///
    /// Source description the resampler was last configured for; a mid-run
    /// change is logged when the resampler is rebuilt for it. `in_layout` is the
    /// normalised copy (see `normalized_layout`) that both the configuration and
    /// every converted frame carry: legacy containers report an `UNSPEC` layout
    /// that libswresample would otherwise reject on every single frame.
    in_format: c_int,
    in_rate: c_int,
    in_channels: c_int,
    in_layout: AVChannelLayout,
}

impl Default for AudioPipeline {
    fn default() -> Self {
        Self {
            swr: std::ptr::null_mut(),
            fifo: std::ptr::null_mut(),
            sample_rate: 0,
            layout: AVChannelLayout::stereo(),
            out_format: AV_SAMPLE_FMT_FLTP,
            frame_size: 0,
            next_pts: 0,
            start_samples: 0,
            dec_tb: AVRational::new(0, 1),
            in_format: AV_SAMPLE_FMT_NONE,
            in_rate: 0,
            in_channels: 0,
            in_layout: AVChannelLayout::default(),
        }
    }
}

impl AudioPipeline {
    /// `start_secs` is the media time this run starts at; it only seeds the
    /// fallback pts for sources whose first decoded frame carries no timestamp.
    unsafe fn new(
        audio_sample_rate: i32,
        audio_channels: u32,
        start_secs: f64,
        out_format: c_int,
    ) -> Self {
        let start_samples = if audio_sample_rate > 0 && start_secs > 0.0 {
            (start_secs * f64::from(audio_sample_rate)).round() as i64
        } else {
            0
        };
        Self {
            sample_rate: audio_sample_rate,
            layout: if audio_channels == 1 {
                AVChannelLayout::mono()
            } else {
                AVChannelLayout::stereo()
            },
            next_pts: start_samples,
            start_samples,
            out_format,
            ..Self::default()
        }
    }

    /// Tell the pipeline which clock the decoded frames use.
    unsafe fn set_source_time_base(&mut self, tb: AVRational) {
        self.dec_tb = tb;
    }

    /// Configure swresample for `dec_frame`'s source format and, on the first
    /// call, allocate the re-chunking FIFO.
    ///
    /// Called again when a frame no longer matches: legacy containers do not
    /// store a channel layout, so decoders may report "unspecified" first and a
    /// concrete layout later, and some streams switch sample format mid-file.
    unsafe fn configure(
        &mut self,
        a_enc_ctx: *mut AVCodecContext,
        dec_frame: *mut AVFrame,
    ) -> FfmpegResult<()> {
        if !self.swr.is_null() {
            swr_free(&mut self.swr);
        }
        let src_layout = normalized_layout(frame_ch_layout(dec_frame));
        self.in_layout = src_layout;
        let ret = swr_alloc_set_opts2(
            &mut self.swr,
            &self.layout,
            self.out_format,
            self.sample_rate,
            &src_layout,
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
        self.in_format = frame_format(dec_frame);
        self.in_rate = frame_sample_rate(dec_frame);
        self.in_channels = (*frame_ch_layout(dec_frame)).nb_channels;
        if self.fifo.is_null() {
            self.frame_size = codctx_frame_size(a_enc_ctx);
            if self.frame_size <= 0 {
                // Encoders without a fixed frame size never need re-chunking;
                // any positive value makes `drain` a pass-through of what the
                // FIFO has.
                self.frame_size = 1;
            }
            self.fifo = av_audio_fifo_alloc(
                self.out_format,
                self.layout.nb_channels,
                self.frame_size * 4,
            );
            if self.fifo.is_null() {
                return Err(FfmpegError::Probe(
                    "av_audio_fifo_alloc returned null".into(),
                ));
            }
            // Align the audio clock with the container timeline: the first frame
            // after a mid-file seek carries the source pts, and everything after
            // it is `frame_size` samples apart, so the whole run stays in step
            // with the copied video packets. A reconfigure must not redo this —
            // the FIFO already holds samples counted from that anchor.
            let first_pts = frame_pts(dec_frame);
            if first_pts != AV_NOPTS_VALUE && self.dec_tb.den > 0 && self.sample_rate > 0 {
                let from_source =
                    av_rescale_q(first_pts, self.dec_tb, AVRational::new(1, self.sample_rate));
                // Containers without per-packet time restart their clock at zero
                // after a seek (AVI reports 0 while the video packets still carry
                // the scan position). Only follow the source clock when it agrees
                // with the position this run was asked to start at; otherwise the
                // seeded anchor is the only reliable one, and following the reset
                // would put audio at zero against video at the seek point.
                let tolerance = (f64::from(self.sample_rate) * AUDIO_ANCHOR_TOLERANCE_SECS) as i64;
                if (from_source - self.start_samples).abs() <= tolerance {
                    self.next_pts = from_source;
                }
                diag::log(format!(
                    "audio clock: source={from_source} seed={} -> next_pts={}",
                    self.start_samples, self.next_pts
                ));
            }
        }
        Ok(())
    }

    /// Convert whatever the old resampler still holds into the FIFO before it is
    /// freed, so a reconfigure does not clip the tail of the previous format.
    unsafe fn flush_delay(&mut self) -> FfmpegResult<()> {
        if self.swr.is_null() || self.fifo.is_null() {
            return Ok(());
        }
        loop {
            let mut conv = av_frame_alloc();
            if conv.is_null() {
                return Ok(());
            }
            frame_set_format(conv, self.out_format);
            frame_set_sample_rate(conv, self.sample_rate);
            frame_set_ch_layout(conv, self.layout);
            let ret = swr_convert_frame(self.swr, conv, std::ptr::null());
            if ret < 0 {
                av_frame_free(&mut conv);
                return Ok(());
            }
            let samples = frame_nb_samples(conv);
            if samples <= 0 {
                av_frame_free(&mut conv);
                return Ok(());
            }
            let written = av_audio_fifo_write(self.fifo, frame_extended_data(conv).cast(), samples);
            av_frame_free(&mut conv);
            if written < samples {
                return Err(FfmpegError::Probe(format!(
                    "av_audio_fifo_write accepted {written} of {samples} samples while flushing the resampler"
                )));
            }
        }
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
        if self.swr.is_null() || self.fifo.is_null() {
            self.configure(a_enc_ctx, dec_frame)?;
        }
        let mut attempt = 0u32;
        loop {
            // libswresample compares the *whole* channel layout, not just the
            // channel count, so give it the normalised layout it was configured
            // with; an `UNSPEC` source would otherwise be rejected forever.
            frame_set_ch_layout(dec_frame, self.in_layout);
            let mut conv = av_frame_alloc();
            if conv.is_null() {
                return Err(FfmpegError::Probe(
                    "av_frame_alloc(resampled) returned null".into(),
                ));
            }
            // `swr_convert_frame()` re-validates the output frame against the
            // context it was initialised with (`config_changed()` compares
            // format, sample_rate and layout, returning AVERROR_OUTPUT_CHANGED
            // otherwise), and it re-validates the *input* the same way — a frame
            // whose source description moved on is rejected with
            // AVERROR_INPUT_CHANGED before anything is converted, which leaves the
            // frame untouched for a retry. A bare frame leaves
            // format=NONE/sample_rate=0/empty layout, so describe the target here;
            // with linesize[0] == 0 swresample allocates the buffer and fills in
            // nb_samples itself.
            frame_set_format(conv, self.out_format);
            frame_set_sample_rate(conv, self.sample_rate);
            frame_set_ch_layout(conv, self.layout);
            let ret = swr_convert_frame(self.swr, conv, dec_frame);
            if ret < 0 {
                av_frame_free(&mut conv);
                let source_changed = ret == AVERROR_INPUT_CHANGED || ret == AVERROR_OUTPUT_CHANGED;
                if source_changed && attempt < MAX_AUDIO_RECONFIGURES {
                    attempt += 1;
                    diag::log(format!(
                        "audio resampler reconfigured ({}) after a source change: fmt {}→{} rate {}→{} layout {}/{}→{}/{}",
                        av_err2str(ret),
                        self.in_format,
                        frame_format(dec_frame),
                        self.in_rate,
                        frame_sample_rate(dec_frame),
                        self.in_layout.order(),
                        self.in_layout.native_mask(),
                        (*frame_ch_layout(dec_frame)).order(),
                        (*frame_ch_layout(dec_frame)).native_mask()
                    ));
                    self.flush_delay()?;
                    self.configure(a_enc_ctx, dec_frame)?;
                    continue;
                }
                return Err(ffmpeg_err("swr_convert_frame", ret));
            }
            let samples = frame_nb_samples(conv);
            if samples > 0 {
                let written =
                    av_audio_fifo_write(self.fifo, frame_extended_data(conv).cast(), samples);
                if written < samples {
                    av_frame_free(&mut conv);
                    return Err(FfmpegError::Probe(format!(
                        "av_audio_fifo_write accepted {written} of {samples} samples"
                    )));
                }
            }
            av_frame_free(&mut conv);
            break;
        }
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
            frame_set_format(frame, self.out_format);
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
        self.in_format = AV_SAMPLE_FMT_NONE;
        self.in_rate = 0;
        self.in_channels = 0;
        self.in_layout = AVChannelLayout::default();
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
    stamps: &mut TimestampRepair,
) -> FfmpegResult<()> {
    let sps = fmtctx_streams(out_ctx);
    let out_st = *sps.add(out_idx as usize);
    if out_st.is_null() {
        return Err(FfmpegError::Probe(
            "output stream pointer is null (remux)".into(),
        ));
    }
    av_packet_rescale_ts(pkt, (*in_st).time_base, (*out_st).time_base);
    // The input stream index says nothing about the output layout (a FLV file
    // lists audio before video, so its audio packet would otherwise be muxed
    // into the video stream); route the packet by the output index we chose.
    (*pkt).stream_index = out_idx;
    (*pkt).pos = -1;

    // Legacy containers may leave the timestamps unset; a muxer cannot place such
    // a packet (`EINVAL`), so give it the next slot of a synthetic clock.
    stamps.observe_step((*pkt).duration);
    if (*pkt).pts == AV_NOPTS_VALUE {
        let pts = stamps.fill_missing();
        (*pkt).pts = pts;
        if (*pkt).dts == AV_NOPTS_VALUE {
            (*pkt).dts = pts;
        }
    } else {
        let offset = stamps.align_offset((*pkt).pts);
        if offset != 0 {
            (*pkt).pts = (*pkt).pts.saturating_add(offset);
            if (*pkt).dts != AV_NOPTS_VALUE {
                (*pkt).dts = (*pkt).dts.saturating_add(offset);
            }
        }
        stamps.note((*pkt).pts);
    }

    // Legacy containers may start their clock before zero; the MPEG-TS muxer
    // cannot express a negative timestamp, so the run is shifted as a whole.
    job.align_packet(pkt, (*out_st).time_base);

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
    decision: &SessionDecision,
    start_offset_secs: f64,
    segment_dir: &Path,
    cancel: Arc<AtomicBool>,
    telemetry: &ProgressState,
    pause: &PauseControl,
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
        // The copy layout was decided once for the whole session (contract §2):
        // this function copies the video stream and either copies or encodes the
        // audio stream, never re-decides.
        let audio_encode = decision.audio == StreamMode::Software;
        diag::log(format!(
            "{tag} remux layout: video_idx={v_idx} video_codec={} audio_idx={a_idx} audio_codec={} audio={}",
            video_codec_id
                .map(codec_display_name)
                .unwrap_or_else(|| "none".into()),
            audio_codec_id
                .map(codec_display_name)
                .unwrap_or_else(|| "none".into()),
            decision.audio_action_str(),
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
        // Video is either copied or absent (audio-only source); this path never
        // re-encodes video, so anything else was dispatched wrongly.
        if !matches!(decision.video, StreamMode::Copy | StreamMode::None) {
            avformat_close_input(&mut in_ctx);
            return Err(FfmpegError::Probe(
                "remux path requires a copyable video stream".into(),
            ));
        }
        if v_idx < 0 {
            diag::log(format!(
                "{tag} remux: source has no decodable video stream → audio-only output"
            ));
            telemetry.warn("源文件没有视频流，已转为纯音频输出");
        }

        // ── Hybrid mode: decode the incompatible audio, encode it with the
        // preset's audio codec ─────────────────────────────────────────────
        let audio_sample_rate = preset.audio_sample_rate.unwrap_or(48_000) as i32;
        let mut a_dec_ctx: *mut AVCodecContext = std::ptr::null_mut();
        let mut a_enc_ctx: *mut AVCodecContext = std::ptr::null_mut();
        let mut audio_enc_name: Option<String> = None;
        let mut a_enc_fmt: c_int = AV_SAMPLE_FMT_FLTP;
        if audio_encode {
            let audio_codec =
                crate::preset::audio_encoder_codec_id(&preset.audio_codec) as AVCodecID;
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
                Ok((ctx, name, fmt)) => {
                    a_enc_ctx = ctx;
                    audio_enc_name = Some(name);
                    a_enc_fmt = fmt;
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
        let audio_mode = if audio_encode {
            audio_enc_name.clone()
        } else if a_idx >= 0 {
            Some("copy".to_string())
        } else {
            None
        };
        telemetry.set_encoder(EncoderInfo {
            mode: if audio_encode {
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
            video_action: None,
            audio_action: None,
        });
        if audio_encode {
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
            if audio_encode {
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
            av_dict_set(
                &mut dict,
                c"hls_flags".as_ptr(),
                c"split_by_time".as_ptr(),
                0,
            );
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
        if audio_encode {
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
        let mut audio = AudioPipeline::new(
            audio_sample_rate,
            u32::from(preset.audio_channels),
            start_offset_secs,
            a_enc_fmt,
        );
        if a_idx >= 0 {
            audio.set_source_time_base((*(*streams_ptr.add(a_idx as usize))).time_base);
        }
        // Containers without per-packet time (AVI) need a synthetic clock, one per
        // stream so video and audio keep their own cadence; a seeked run starts
        // that clock at the seek point.
        let mut video_stamps =
            TimestampRepair::seeded(run_start_ticks(out_ctx, out_v_idx, start_offset_secs));
        let mut audio_stamps =
            TimestampRepair::seeded(run_start_ticks(out_ctx, out_a_idx, start_offset_secs));

        loop {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            // Throttling: block on the packet boundary while the producer is
            // paused; a cancel that arrives during the wait also stops the run.
            if pause.wait_if_paused(&cancel) {
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

            if audio_encode && in_st_idx == a_idx as usize {
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
            let stamps = if in_st_idx == v_idx as usize {
                &mut video_stamps
            } else {
                &mut audio_stamps
            };
            let res = copy_packet_to_muxer(pkt, in_st, out_ctx, out_idx, &mut job, stamps);
            av_packet_unref(pkt);
            if let Err(e) = res {
                write_error = Some(e);
                break;
            }
        }

        // Flush the hybrid audio encoder so the last frames reach the muxer.
        if audio_encode && write_error.is_none() && !a_enc_ctx.is_null() {
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

/// Open one video decoder context for `stream` with `codec`.
///
/// Allocates the context, copies the stream parameters, optionally attaches a
/// MediaCodec hardware device ([`hwdecode::attach`] **must** run before
/// `avcodec_open2`), and opens the decoder. On failure the context (and any
/// attached device) is freed before returning the error, so the caller can retry
/// with the stock decoder.
unsafe fn open_video_decoder(
    stream: *mut AVStream,
    codec: *const AVCodec,
    codec_id: u32,
    tag: &str,
    label: &str,
    try_hardware: bool,
) -> FfmpegResult<(*mut AVCodecContext, Option<hwdecode::HwDecode>)> {
    let mut ctx = avcodec_alloc_context3(codec);
    if ctx.is_null() {
        return Err(FfmpegError::Probe(format!(
            "avcodec_alloc_context3({label}) returned null"
        )));
    }
    let ret = avcodec_parameters_to_context(ctx, (*stream).codecpar);
    if ret < 0 {
        avcodec_free_context(&mut ctx);
        return Err(ffmpeg_err(
            format!("avcodec_parameters_to_context({label})"),
            ret,
        ));
    }
    // attach() must run before avcodec_open2: the decoder negotiates the
    // MediaCodec pixel format during open.
    let mut hw = if try_hardware {
        hwdecode::attach(ctx, codec_id as c_int)
    } else {
        None
    };
    let ret = avcodec_open2(ctx, codec, std::ptr::null_mut());
    diag::log(format!("{tag} transcode avcodec_open2({label}) rc={ret}"));
    if ret < 0 {
        if let Some(hw) = hw.take() {
            hw.drop();
        }
        avcodec_free_context(&mut ctx);
        return Err(ffmpeg_err(format!("avcodec_open2({label})"), ret));
    }
    Ok((ctx, hw))
}

/// Open the video decoder, preferring a MediaCodec hardware decoder.
///
/// The MediaCodec decoder is only used when `avcodec_find_decoder_by_name`
/// resolves it *and* a device attaches and `avcodec_open2` succeeds. Every
/// failure (host build without the decoder, Android device without a usable
/// codec, unsupported profile) logs one diagnostic line and falls back to the
/// stock software decoder — hardware decode must never fail the cast.
unsafe fn open_video_decoder_hw_or_sw(
    stream: *mut AVStream,
    codec_id: u32,
    stock_codec: *const AVCodec,
    tag: &str,
) -> FfmpegResult<(*mut AVCodecContext, Option<hwdecode::HwDecode>)> {
    let dec_name = codec_display_name(codec_id);
    if let Some(hw_name) = hwdecode::mediacodec_decoder_name(codec_id) {
        let hw_codec = avcodec_find_decoder_by_name(hw_name.as_ptr().cast::<c_char>());
        if !hw_codec.is_null() {
            let label = format!("video decoder {dec_name} (mediacodec)");
            match open_video_decoder(stream, hw_codec, codec_id, tag, &label, true) {
                Ok((ctx, Some(hw))) => return Ok((ctx, Some(hw))),
                Ok((ctx, None)) => {
                    // The decoder is compiled in but no hardware device attached;
                    // the stock decoder keeps the software path predictable.
                    let mut ctx = ctx;
                    avcodec_free_context(&mut ctx);
                    diag::log(format!(
                        "{tag} transcode: no MediaCodec device, using software decoder {dec_name}"
                    ));
                }
                Err(error) => {
                    // The decoder opened with a hardware context but could not
                    // open (unsupported profile, device busy, …): retry software.
                    diag::log(format!(
                        "{tag} transcode: hardware video decode unavailable ({error}); using software decoder"
                    ));
                }
            }
        }
    }
    let label = format!("video decoder {dec_name}");
    open_video_decoder(stream, stock_codec, codec_id, tag, &label, false)
}

fn run_transcode(
    input_url: &str,
    preset: &wdmm_core::models::TranscodePreset,
    decision: &SessionDecision,
    start_offset_secs: f64,
    segment_dir: &Path,
    cancel: Arc<AtomicBool>,
    telemetry: &ProgressState,
    pause: &PauseControl,
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
        let want_video = decision.video != StreamMode::None;
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

        if !has_video_stream {
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

        // ── Open the audio decoder ────────────────────────────────────────
        // The video decoder is opened right before the decode loop (once the
        // muxer and encoder are fully set up) so a MediaCodec hardware attach can
        // fail and fall back to software decode without dismantling the output.
        let mut v_dec_ctx: *mut AVCodecContext = std::ptr::null_mut();
        let mut a_dec_ctx: *mut AVCodecContext = std::ptr::null_mut();
        // MediaCodec hardware-decode guard; dropped before the decoder context
        // is freed (see the cleanup section below).
        let mut hw_decode: Option<hwdecode::HwDecode> = None;

        if a_idx >= 0 && !a_dec_codec.is_null() && decision.audio != StreamMode::Copy {
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

        let audio_stream_present = a_idx >= 0 && !a_dec_codec.is_null();
        // The session decision is authoritative: `copy` keeps the source codec
        // (the source was proven MPEG-TS compatible when the decision was made),
        // otherwise the preset's audio codec is encoded.
        let audio_copy = audio_stream_present && decision.audio == StreamMode::Copy;
        let a_codec_id: AVCodecID = if audio_copy || !audio_stream_present {
            0
        } else {
            crate::preset::audio_encoder_codec_id(&preset.audio_codec) as AVCodecID
        };
        let mut a_enc_codec: *const AVCodec = std::ptr::null();
        let mut a_enc_fmt: c_int = AV_SAMPLE_FMT_FLTP;
        if audio_copy {
            diag::log(format!(
                "{tag} transcode audio copy: keeping source codec {} untouched",
                stream_codec_id(streams_ptr, a_idx)
                    .map(codec_display_name)
                    .unwrap_or_else(|| "?".into())
            ));
        } else if audio_stream_present {
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
        }

        // ── Open video encoder ────────────────────────────────────────────
        // The session decision already chose hardware vs software and the exact
        // encoder, so this only opens it. A hardware open that fails after the
        // decision falls back to the preset's software encoder — never silently:
        // the warning names the downgrade and telemetry records it (contract §2/§4).
        if decode_video {
            let in_st = *streams_ptr.add(v_idx as usize);
            let src_w = (*(*in_st).codecpar).width;
            let src_h = (*(*in_st).codecpar).height;
            let src_rate = stream_r_frame_rate(in_st);
            let hardware = decision.video == StreamMode::Hardware;
            let tier = decision.video_tier.clone().unwrap_or_else(|| {
                if hardware {
                    preset.hw_video_tier.clone()
                } else {
                    preset.sw_video_tier.clone()
                }
            });
            let planned = plan_video_encode(
                preset,
                src_w.max(0) as u32,
                src_h.max(0) as u32,
                src_rate,
                &tier,
            );
            let (out_w, out_h) = (planned.width, planned.height);
            let framerate = planned.framerate;
            let video_gop = planned.gop;
            let encoder_label = decision.video_encoder.as_deref().unwrap_or("?");
            diag::log(format!(
                "{tag} transcode video plan: action={} encoder={encoder_label} tier={tier} x264_preset={} bitrate={} gop={video_gop} rate={}/{} → {out_w}x{out_h}",
                if hardware { "hw" } else { "sw" },
                x264_preset_for_tier(&tier).unwrap_or("-"),
                planned.bitrate,
                framerate.num,
                framerate.den,
            ));

            if hardware {
                let hw_name = decision
                    .video_encoder
                    .as_deref()
                    .unwrap_or(preset.hw_video_encoder.as_str());
                let hw_name_c = CString::new(hw_name).unwrap();
                let hw_codec = avcodec_find_encoder_by_name(hw_name_c.as_ptr());
                if hw_codec.is_null() {
                    diag::log(format!(
                        "{tag} transcode hardware encoder {hw_name} no longer resolves"
                    ));
                } else {
                    match open_hw_video_encoder(
                        tag,
                        hw_name,
                        hw_codec,
                        planned.bitrate,
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
                        Err(err) => {
                            diag::log(format!("{tag} transcode hardware open failed: {err}"));
                        }
                    }
                }
            }

            if !hw_available {
                let decision_encoder = decision
                    .video_encoder
                    .as_deref()
                    .filter(|name| !name.is_empty() && *name != "copy");
                let sw_name = if decision.video == StreamMode::Software {
                    decision_encoder.unwrap_or(preset.sw_video_encoder.as_str())
                } else {
                    preset.sw_video_encoder.as_str()
                };
                // An hevc preset with a libx264 software fallback really encodes
                // H.264; say so instead of letting the UI believe it got HEVC.
                if sw_name == "libx264" && crate::plan::sw_fallback_downgrades_hevc(preset) {
                    let warning = "video: sw fallback downgrades hevc to h264";
                    diag::log(format!("{tag} transcode {warning}"));
                    telemetry.warn(warning);
                }
                if decision.video == StreamMode::Hardware {
                    // The decision promised hardware; report why it did not happen.
                    let warning = format!(
                        "video: hardware encoder {} unavailable, fell back to {} ({tier})",
                        decision.video_encoder.as_deref().unwrap_or("?"),
                        sw_name
                    );
                    diag::log(format!("{tag} transcode {warning}"));
                    telemetry.warn(warning);
                }
                let sw_name_c = CString::new(sw_name).unwrap();
                v_enc_codec = avcodec_find_encoder_by_name(sw_name_c.as_ptr());
                if v_enc_codec.is_null() {
                    if let Some(fallback_id) = crate::preset::video_encoder_output_codec_id(sw_name)
                    {
                        v_enc_codec = avcodec_find_encoder(fallback_id);
                    }
                }
                if v_enc_codec.is_null() {
                    diag::log(format!(
                        "{tag} transcode failed: no {sw_name} encoder available in libavcodec"
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
                        "no {sw_name} encoder available"
                    )));
                }
                v_enc_name = Some(sw_name.to_string());
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
                // libx264 treats time_base purely as a tick scale; the MPEG-1/2/4
                // encoders instead read it as the frame rate and reject anything
                // else (`MPEG-1/2 does not support 90000/1 fps`), so they get the
                // inverse of the output frame rate. MPEG-2 additionally only
                // signals its eight standard rates, so a 97/6 source is snapped
                // (and the GOP re-derived from the snapped rate) instead of making
                // `avcodec_open2` fail with EINVAL.
                let encoder_framerate = if sw_name == "mpeg2video" {
                    let snapped = mpeg2_frame_rate(framerate);
                    if snapped.num != framerate.num || snapped.den != framerate.den {
                        let warning = format!(
                            "video: frame rate {}/{} is not an MPEG-2 frame rate, snapped to {}/{}",
                            framerate.num, framerate.den, snapped.num, snapped.den
                        );
                        diag::log(format!("{tag} transcode {warning}"));
                        telemetry.warn(warning);
                    }
                    snapped
                } else {
                    framerate
                };
                let encoder_gop = if encoder_framerate.num == framerate.num
                    && encoder_framerate.den == framerate.den
                {
                    video_gop
                } else {
                    segment_gop_size(
                        encoder_framerate.num,
                        encoder_framerate.den,
                        preset.hls_segment_secs,
                    )
                };
                let encoder_time_base = if sw_name == "libx264" {
                    AVRational::new(1, 90000)
                } else {
                    AVRational::new(encoder_framerate.den.max(1), encoder_framerate.num.max(1))
                };
                codctx_set_time_base(v_enc_ctx, encoder_time_base);
                codctx_set_gop_size(v_enc_ctx, encoder_gop);
                codctx_set_framerate(v_enc_ctx, encoder_framerate);
                let mut enc_opts: *mut AVDictionary = std::ptr::null_mut();
                // Every software encoder must start each segment with an IDR: the
                // MPEG-2/MPEG-4 encoders only honour that through the codec
                // context flag (the `-flags +cgop` CLI switch answers ENOSYS once
                // the encoder is driven through libavcodec's API).
                let mut flags = codctx_flags(v_enc_ctx) | AV_CODEC_FLAG_CLOSED_GOP;
                if sw_name == "libx264" {
                    // mpegtsenc only inserts h264_mp4toannexb when the stream
                    // carries AVCC extradata, which libx264 emits for global
                    // header mode.
                    flags |= AV_CODEC_FLAG_GLOBAL_HEADER;
                    codctx_set_max_b_frames(v_enc_ctx, 2);
                    // Force an IDR exactly every `video_gop` frames: `keyint` = one
                    // segment, `min-keyint` blocks early IDRs, `scenecut=0` stops
                    // scene changes from shifting the cut points off the plan grid.
                    let x264_params = CString::new(format!(
                        "keyint={video_gop}:min-keyint={video_gop}:scenecut=0"
                    ))
                    .unwrap();
                    av_dict_set(
                        &mut enc_opts,
                        c"x264-params".as_ptr(),
                        x264_params.as_ptr(),
                        0,
                    );
                    // The tier is the speed/quality knob, expressed as the
                    // libx264 `preset` option.
                    // Unknown tiers keep libx264's default (contract §3).
                    if let Some(tier_preset) = x264_preset_for_tier(&tier) {
                        let tier_preset = CString::new(tier_preset).unwrap();
                        av_dict_set(&mut enc_opts, c"preset".as_ptr(), tier_preset.as_ptr(), 0);
                    }
                } else {
                    codctx_set_max_b_frames(v_enc_ctx, 0);
                    // The MPEG-2/MPEG-4 encoders refuse AV_CODEC_FLAG_CLOSED_GOP
                    // while scene-change detection is on: libavcodec asks for the
                    // threshold to be effectively infinite
                    // ("closed gop with scene change detection are not supported
                    // yet, set threshold to 1000000000").
                    av_dict_set(
                        &mut enc_opts,
                        c"sc_threshold".as_ptr(),
                        c"1000000000".as_ptr(),
                        0,
                    );
                }
                codctx_set_flags(v_enc_ctx, flags);
                codctx_set_bit_rate(v_enc_ctx, planned.bitrate);
                let ret = avcodec_open2(v_enc_ctx, v_enc_codec, &mut enc_opts);
                av_dict_free(&mut enc_opts);
                diag::log(format!(
                    "{tag} transcode avcodec_open2(video encoder {sw_name}, tier={tier}) rc={ret}"
                ));
                if ret < 0 {
                    cleanup_transcode(
                        in_ctx,
                        std::ptr::null_mut(),
                        v_dec_ctx,
                        a_dec_ctx,
                        v_enc_ctx,
                        std::ptr::null_mut(),
                    );
                    return Err(ffmpeg_err(
                        format!("avcodec_open2(video encoder {sw_name})"),
                        ret,
                    ));
                }
            }
            diag::log(format!(
                "{tag} transcode encoder selected video={} mode={} tier={tier} pix_fmt={}",
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
                Ok((ctx, name, fmt)) => {
                    a_enc_ctx = ctx;
                    a_enc_name = Some(name);
                    a_enc_fmt = fmt;
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
        if v_enc_ctx.is_null() && a_enc_ctx.is_null() && !audio_copy {
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
            audio: if audio_copy {
                Some("copy".to_string())
            } else if audio_stream_present {
                Some(a_enc_name.unwrap_or_else(|| preset.audio_codec.clone()))
            } else {
                None
            },
            hw_requested: decision.video == StreamMode::Hardware,
            hw_available,
            video_action: None,
            audio_action: None,
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
        if audio_copy {
            // Audio copy: the output stream mirrors the source stream, exactly like
            // the remux path does for video.
            let in_st = *streams_ptr.add(a_idx as usize);
            let out_st = avformat_new_stream(out_ctx, std::ptr::null());
            if out_st.is_null() {
                cleanup_transcode(in_ctx, out_ctx, v_dec_ctx, a_dec_ctx, v_enc_ctx, a_enc_ctx);
                return Err(FfmpegError::Probe(
                    "avformat_new_stream(audio copy) returned null".into(),
                ));
            }
            let ret = avcodec_parameters_copy((*out_st).codecpar, (*in_st).codecpar);
            if ret < 0 {
                cleanup_transcode(in_ctx, out_ctx, v_dec_ctx, a_dec_ctx, v_enc_ctx, a_enc_ctx);
                return Err(ffmpeg_err("avcodec_parameters_copy(audio copy)", ret));
            }
            (*out_st).time_base = (*in_st).time_base;
            out_a_idx = (*out_st).index;
        } else if !a_enc_ctx.is_null() {
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
            av_dict_set(
                &mut dict,
                c"hls_flags".as_ptr(),
                c"split_by_time".as_ptr(),
                0,
            );
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
        let mut audio = AudioPipeline::new(
            audio_sample_rate,
            u32::from(preset.audio_channels),
            start_offset_secs,
            a_enc_fmt,
        );
        if a_idx >= 0 {
            audio.set_source_time_base((*(*streams_ptr.add(a_idx as usize))).time_base);
        }
        let mut enc_vframe: *mut AVFrame = std::ptr::null_mut();
        // System-memory frame a MediaCodec hardware frame is transferred into
        // before scaling/encoding; allocated lazily and reused per frame.
        let mut hw_sw_frame: *mut AVFrame = std::ptr::null_mut();
        // Format the cached sws context was created for; a mid-job fallback from
        // hardware frames (NV12/P010) to software frames (YUV420P) changes it.
        let mut sws_src_fmt: c_int = AV_PIX_FMT_NONE;
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
                    &mut hw_sw_frame,
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
                    &mut hw_sw_frame,
                    &mut sws,
                    &mut audio,
                );
                cleanup_transcode(in_ctx, out_ctx, v_dec_ctx, a_dec_ctx, v_enc_ctx, a_enc_ctx);
                return Err(ffmpeg_err("av_frame_get_buffer(encoder frame)", ret));
            }
        }

        // ── Open the video decoder (MediaCodec hardware decode when possible) ─
        // Everything that can fail before decoding is set up by now, so a failed
        // hardware attach/open falls back to the stock decoder without touching
        // the muxer, encoders or output streams of the job.
        if decode_video && !v_dec_codec.is_null() {
            let in_st = *streams_ptr.add(v_idx as usize);
            let codec_id = (*(*in_st).codecpar).codec_id;
            match open_video_decoder_hw_or_sw(in_st, codec_id, v_dec_codec, tag) {
                Ok((ctx, hw)) => {
                    v_dec_ctx = ctx;
                    hw_decode = hw;
                }
                Err(error) => {
                    diag::log(format!("{tag} transcode: {error}"));
                    free_loop_state(
                        &mut in_pkt,
                        &mut out_pkt,
                        &mut dec_frame,
                        &mut enc_vframe,
                        &mut hw_sw_frame,
                        &mut sws,
                        &mut audio,
                    );
                    cleanup_transcode(in_ctx, out_ctx, v_dec_ctx, a_dec_ctx, v_enc_ctx, a_enc_ctx);
                    return Err(error);
                }
            }
        }

        // ── Decode/encode loop ────────────────────────────────────────────
        let mut job = JobCtx::new(tag, telemetry, segment_dir);
        let mut fatal: Option<FfmpegError> = None;
        let mut decode_errors_logged = 0u32;
        // A source without timestamps (AVI) would hand the encoder
        // `AV_NOPTS_VALUE`; the encoder then propagates it and the muxer rejects
        // the packet. Give the encoder a monotonic clock instead.
        let mut video_stamps = TimestampRepair::seeded(if v_enc_ctx.is_null() {
            0
        } else {
            start_ticks_in(codctx_time_base(v_enc_ctx), start_offset_secs)
        });
        if !v_enc_ctx.is_null() {
            let fr = codctx_framerate(v_enc_ctx);
            if fr.num > 0 && fr.den > 0 {
                video_stamps.observe_step(av_rescale_q(
                    1,
                    AVRational::new(fr.den, fr.num),
                    codctx_time_base(v_enc_ctx),
                ));
            }
        }
        // Audio copy keeps the source clock; the repair only fills in timestamps a
        // legacy container leaves unset, anchored on the run's start position.
        let mut audio_stamps = TimestampRepair::seeded(if audio_copy {
            run_start_ticks(out_ctx, out_a_idx, start_offset_secs)
        } else {
            0
        });

        'decode: loop {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            // Throttling: block on the packet boundary while the producer is
            // paused; a cancel that arrives during the wait also stops the run.
            if pause.wait_if_paused(&cancel) {
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
                        // MediaCodec frames carry GPU surfaces; transfer them to
                        // system memory before sws_scale can read the pixels.
                        let mut frame = dec_frame;
                        if hw_decode.is_some() && hwdecode::is_hw_frame(dec_frame) {
                            if hw_sw_frame.is_null() {
                                hw_sw_frame = av_frame_alloc();
                                if hw_sw_frame.is_null() {
                                    fatal = Some(FfmpegError::Probe(
                                        "av_frame_alloc(hardware→software frame) returned null"
                                            .into(),
                                    ));
                                    av_frame_unref(dec_frame);
                                    break 'decode;
                                }
                            }
                            let transferred = match hw_decode.as_ref() {
                                Some(hw) => hw.to_software(dec_frame, hw_sw_frame),
                                None => false,
                            };
                            if transferred {
                                frame = hw_sw_frame;
                            } else {
                                // A device that refuses a transfer must not kill the
                                // cast: reopen the stock decoder and keep the job.
                                diag::log(format!(
                                    "{tag} transcode: hardware frame transfer failed; switching to software decode"
                                ));
                                if let Some(hw) = hw_decode.take() {
                                    hw.drop();
                                }
                                avcodec_free_context(&mut v_dec_ctx);
                                if !sws.is_null() {
                                    sws_freeContext(sws);
                                    sws = std::ptr::null_mut();
                                }
                                sws_src_fmt = AV_PIX_FMT_NONE;
                                let in_st = *streams_ptr.add(v_idx as usize);
                                let codec_id = (*(*in_st).codecpar).codec_id;
                                match open_video_decoder(
                                    in_st,
                                    v_dec_codec,
                                    codec_id,
                                    tag,
                                    "video decoder (software fallback)",
                                    false,
                                ) {
                                    Ok((ctx, _)) => v_dec_ctx = ctx,
                                    Err(error) => fatal = Some(error),
                                }
                                av_frame_unref(dec_frame);
                                av_frame_unref(hw_sw_frame);
                                if fatal.is_some() {
                                    break 'decode;
                                }
                                break;
                            }
                        }
                        let sw = frame_width(frame);
                        let sh = frame_height(frame);
                        let src_fmt = frame_format(frame);
                        let dw = video_out_dims.0;
                        let dh = video_out_dims.1;
                        if sws.is_null() || src_fmt != sws_src_fmt {
                            if !sws.is_null() {
                                sws_freeContext(sws);
                            }
                            sws = sws_getContext(
                                sw,
                                sh,
                                src_fmt,
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
                                if !hw_sw_frame.is_null() {
                                    av_frame_unref(hw_sw_frame);
                                }
                                break 'decode;
                            }
                            sws_src_fmt = src_fmt;
                        }
                        let src_d = [
                            frame_data(frame, 0) as *const u8,
                            frame_data(frame, 1) as *const u8,
                            frame_data(frame, 2) as *const u8,
                            std::ptr::null(),
                        ];
                        let src_s = [
                            frame_linesize(frame, 0),
                            frame_linesize(frame, 1),
                            frame_linesize(frame, 2),
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
                            if !hw_sw_frame.is_null() {
                                av_frame_unref(hw_sw_frame);
                            }
                            continue;
                        }
                        frame_set_pts(
                            enc_vframe,
                            video_stamps.repair_monotonic(if frame_pts(frame) == AV_NOPTS_VALUE {
                                AV_NOPTS_VALUE
                            } else {
                                av_rescale_q(
                                    frame_pts(frame),
                                    video_dec_tb,
                                    codctx_time_base(v_enc_ctx),
                                )
                            }),
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
                            if !hw_sw_frame.is_null() {
                                av_frame_unref(hw_sw_frame);
                            }
                            break 'decode;
                        }
                    }
                    av_frame_unref(dec_frame);
                    if !hw_sw_frame.is_null() {
                        av_frame_unref(hw_sw_frame);
                    }
                }
            } else if stream_idx == a_idx && audio_copy {
                // encode+copy quadrant: the audio packet goes to the muxer
                // verbatim — no decoder, no resampler, no FIFO (`contract §2`).
                if let Err(e) = copy_packet_to_muxer(
                    in_pkt,
                    *streams_ptr.add(a_idx as usize),
                    out_ctx,
                    out_a_idx,
                    &mut job,
                    &mut audio_stamps,
                ) {
                    fatal = Some(e);
                    break 'decode;
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
        // Release the hardware decoder binding before the decoder context: the
        // guard owns the AVBufferRef(hw_device_ctx) it attached to that context.
        if let Some(hw) = hw_decode.take() {
            hw.drop();
        }
        if !sws.is_null() {
            sws_freeContext(sws);
        }
        audio.free();
        if !enc_vframe.is_null() {
            av_frame_free(&mut enc_vframe);
        }
        av_frame_free(&mut dec_frame);
        av_frame_free(&mut hw_sw_frame);
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
        // A hardware encoder can emit packets without a decode timestamp; the
        // muxer needs one to interleave, so fall back to the presentation time.
        if (*out_pkt).dts == AV_NOPTS_VALUE {
            (*out_pkt).dts = (*out_pkt).pts;
        }
        // A B-frame encoder delays the first dts below zero, which the MPEG-TS
        // muxer cannot express; shift the whole run instead of failing the write.
        job.align_packet(out_pkt, (*out_st).time_base);

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
    hw_sw_frame: &mut *mut AVFrame,
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
    if !hw_sw_frame.is_null() {
        av_frame_free(hw_sw_frame);
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

#[cfg(test)]
mod tests {
    use super::{TimestampRepair, AV_NOPTS_VALUE};

    #[test]
    fn timestamp_repair_fills_a_missing_clock_from_the_packet_duration() {
        let mut stamps = TimestampRepair::new();
        stamps.observe_step(3_600);
        for expected in [0, 3_600, 7_200] {
            assert_eq!(stamps.fill_missing(), expected);
        }
    }

    #[test]
    fn timestamp_repair_continues_from_a_real_timestamp() {
        let mut stamps = TimestampRepair::new();
        stamps.observe_step(1_200);
        stamps.note(48_000);
        assert_eq!(stamps.fill_missing(), 49_200);
    }

    #[test]
    fn timestamp_repair_ignores_a_missing_timestamp_when_noting() {
        let mut stamps = TimestampRepair::new();
        stamps.observe_step(900);
        stamps.note(AV_NOPTS_VALUE);
        assert_eq!(stamps.fill_missing(), 0);
    }

    #[test]
    fn timestamp_repair_keeps_a_monotonic_encoder_clock() {
        let mut stamps = TimestampRepair::new();
        stamps.observe_step(3_000);
        assert_eq!(stamps.repair_monotonic(AV_NOPTS_VALUE), 0);
        assert_eq!(stamps.repair_monotonic(AV_NOPTS_VALUE), 3_000);
        // A real timestamp ahead of the synthetic clock is kept as-is.
        assert_eq!(stamps.repair_monotonic(9_000), 9_000);
        // A repeat or a rewind is nudged forward instead of being passed on.
        assert_eq!(stamps.repair_monotonic(9_000), 12_000);
        assert_eq!(stamps.repair_monotonic(500), 15_000);
    }

    #[test]
    fn timestamp_repair_aligns_a_seeked_run_on_one_anchor() {
        // Leftovers from a real file: video reports the scan position, audio
        // reports zero for the same seek.
        let mut video = TimestampRepair::seeded(360_000);
        let mut audio = TimestampRepair::seeded(360_000);
        assert_eq!(video.repair_monotonic(368_100), 360_000);
        assert_eq!(video.repair_monotonic(368_400), 360_300);
        assert_eq!(audio.fill_missing(), 360_000);
        assert_eq!(audio.fill_missing(), 360_000 + audio.step.max(1));
    }

    #[test]
    fn timestamp_repair_keeps_the_container_clock_when_the_run_starts_at_zero() {
        let mut stamps = TimestampRepair::seeded(0);
        assert_eq!(stamps.repair_monotonic(6_000), 6_000);
        assert_eq!(stamps.repair_monotonic(6_006), 6_006);
    }

    #[test]
    fn timestamp_repair_starts_a_positive_clock_at_the_first_real_timestamp() {
        let mut stamps = TimestampRepair::new();
        stamps.observe_step(1_000);
        assert_eq!(stamps.repair_monotonic(7_500), 7_500);
        assert_eq!(stamps.repair_monotonic(AV_NOPTS_VALUE), 8_500);
    }

    #[test]
    fn mpeg2_frame_rate_keeps_standard_rates_and_snaps_the_rest() {
        let rate = |num, den| {
            let snapped = super::mpeg2_frame_rate(super::AVRational::new(num, den));
            (snapped.num, snapped.den)
        };
        // The eight signalled rates pass through untouched.
        assert_eq!(rate(24000, 1001), (24000, 1001));
        assert_eq!(rate(25, 1), (25, 1));
        assert_eq!(rate(30000, 1001), (30000, 1001));
        assert_eq!(rate(60, 1), (60, 1));
        // 97/6 (~16.17 fps) and 120 fps are not signalled: snap to the nearest.
        assert_eq!(rate(97, 6), (24000, 1001));
        assert_eq!(rate(120, 1), (60, 1));
        assert_eq!(rate(0, 1), (24000, 1001));
    }
}
