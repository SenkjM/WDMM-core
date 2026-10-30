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

#![allow(clippy::too_many_arguments)]

use std::ffi::{c_char, CStr, CString};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::sys::*;
use crate::{AudioStream, FfmpegError, FfmpegResult, ProbeResult, SubtitleStream, VideoStream};

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

        let mut video  = Vec::new();
        let mut audio  = Vec::new();
        let mut subs   = Vec::new();
        let mut fmt_name = String::new();

        // Read the iformat name via pointer arithmetic (iformat @ +32 in AVFormatContext).
        // iformat is an *const AVInputFormat; its first field is `const char *name`.
        let iformat_ptr = *(fmt_ctx as *const u8).add(32).cast::<*const *const c_char>();
        if !iformat_ptr.is_null() {
            let name_ptr = *iformat_ptr; // first field of AVInputFormat = name
            if !name_ptr.is_null() {
                fmt_name = CStr::from_ptr(name_ptr).to_string_lossy().into_owned();
            }
        }

        for i in 0..nb {
            let st = *streams_ptr.add(i);
            if st.is_null() { continue; }
            let cp = (*st).codecpar;
            if cp.is_null() { continue; }

            let codec_type = (*cp).codec_type;
            let codec_id   = (*cp).codec_id;
            let tb         = (*st).time_base;

            // Resolve codec name via avcodec_find_decoder (first codec matching ID).
            let codec_name = {
                let dec = avcodec_find_decoder(codec_id);
                if dec.is_null() {
                    format!("{}", codec_id)
                } else {
                    // AVCodec.name is the second pointer field (after AVClass* at +0):
                    // actually AVCodec.name is at +8 on LP64.
                    let name_ptr = *(dec as *const u8).add(8).cast::<*const c_char>();
                    if name_ptr.is_null() {
                        format!("{}", codec_id)
                    } else {
                        CStr::from_ptr(name_ptr).to_string_lossy().into_owned()
                    }
                }
            };

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
                        fps: if fps_raw > 0.0 && fps_raw < 1000.0 { fps_raw } else { 0.0 },
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
                3 => { // AVMEDIA_TYPE_SUBTITLE = 3
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
        Ok(ProbeResult { duration_secs, video, audio, subtitles: subs, format_name: fmt_name })
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
) -> FfmpegResult<()> {
    // Passthrough preset — no HLS wrapping, direct stream. Callers should not
    // call run_hls_job for passthrough; return immediately.
    if preset.container == "original" {
        return Ok(());
    }

    // Decide: remux or transcode?
    if preset.remux_first {
        match run_remux(input_url, preset, start_offset_secs, segment_dir, cancel.clone()) {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::warn!("remux failed ({}); falling back to transcode", e);
                if cancel.load(Ordering::Relaxed) { return Ok(()); }
            }
        }
    }

    run_transcode(input_url, preset, start_offset_secs, segment_dir, cancel)
}

// ── Remux path ────────────────────────────────────────────────────────────────

fn run_remux(
    input_url: &str,
    preset: &wdmm_core::models::TranscodePreset,
    start_offset_secs: f64,
    segment_dir: &Path,
    cancel: Arc<AtomicBool>,
) -> FfmpegResult<()> {
    let url_c     = CString::new(input_url).unwrap();
    let playlist  = segment_dir.join("index.m3u8");
    let seg_pat   = segment_dir.join("seg%d.ts");
    let playlist_c = CString::new(playlist.to_string_lossy().as_ref()).unwrap();
    let seg_pat_c  = CString::new(seg_pat.to_string_lossy().as_ref()).unwrap();
    let hls_c      = c"hls";

    unsafe {
        // ── Open input ───────────────────────────────────────────────────────
        let mut in_ctx: *mut AVFormatContext = std::ptr::null_mut();
        let ret = avformat_open_input(&mut in_ctx, url_c.as_ptr(), std::ptr::null(), std::ptr::null_mut());
        if ret < 0 {
            return Err(FfmpegError::Probe(format!("open_input: {}", av_err2str(ret))));
        }
        let ret = avformat_find_stream_info(in_ctx, std::ptr::null_mut());
        if ret < 0 {
            avformat_close_input(&mut in_ctx);
            return Err(FfmpegError::Probe(format!("find_stream_info: {}", av_err2str(ret))));
        }

        // Optional seek
        if start_offset_secs > 0.01 {
            let ts = (start_offset_secs * 1_000_000.0) as i64;
            let _ = av_seek_frame(in_ctx, -1, ts, AVSEEK_FLAG_BACKWARD);
        }

        // ── Validate remux compatibility ──────────────────────────────────
        let nb = fmtctx_nb_streams(in_ctx) as usize;
        let streams_ptr = fmtctx_streams(in_ctx);
        let mut has_incompatible = false;
        for i in 0..nb {
            let st = *streams_ptr.add(i);
            if st.is_null() { continue; }
            let cp = (*st).codecpar;
            if cp.is_null() { continue; }
            let ct = (*cp).codec_type;
            if ct == AVMEDIA_TYPE_VIDEO || ct == AVMEDIA_TYPE_AUDIO {
                if !crate::preset::is_mpegts_copy_compatible((*cp).codec_id) {
                    has_incompatible = true;
                    break;
                }
            }
        }
        if has_incompatible {
            avformat_close_input(&mut in_ctx);
            return Err(FfmpegError::Probe("codec not mpegts-copy-compatible".into()));
        }

        // ── Open HLS output ───────────────────────────────────────────────
        let mut out_ctx: *mut AVFormatContext = std::ptr::null_mut();
        let ret = avformat_alloc_output_context2(
            &mut out_ctx, std::ptr::null(), hls_c.as_ptr(), playlist_c.as_ptr(),
        );
        if ret < 0 || out_ctx.is_null() {
            avformat_close_input(&mut in_ctx);
            return Err(FfmpegError::Io(std::io::Error::new(
                std::io::ErrorKind::Other, format!("alloc_output: {}", av_err2str(ret))
            )));
        }

        // Copy streams
        for i in 0..nb {
            let in_st = *streams_ptr.add(i);
            if in_st.is_null() { continue; }
            let out_st = avformat_new_stream(out_ctx, std::ptr::null());
            if out_st.is_null() {
                avformat_close_input(&mut in_ctx);
                avformat_free_context(out_ctx);
                return Err(FfmpegError::Io(std::io::Error::new(std::io::ErrorKind::Other, "new_stream")));
            }
            avcodec_parameters_copy((*out_st).codecpar, (*in_st).codecpar);
            (*out_st).time_base = (*in_st).time_base;
        }

        // Set HLS muxer options via AVDictionary
        let mut dict: *mut AVDictionary = std::ptr::null_mut();
        let seg_secs = preset.hls_segment_secs.to_string();
        let seg_secs_c = CString::new(seg_secs.as_str()).unwrap();
        av_dict_set(&mut dict, c"hls_time".as_ptr(), seg_secs_c.as_ptr(), 0);
        av_dict_set(&mut dict, c"hls_list_size".as_ptr(), c"0".as_ptr(), 0);
        av_dict_set(&mut dict, c"hls_flags".as_ptr(), c"split_by_time".as_ptr(), 0);
        av_dict_set(&mut dict, c"hls_segment_filename".as_ptr(), seg_pat_c.as_ptr(), 0);

        // Open output I/O
        let ret = avio_open(&mut *(out_ctx as *mut u8).add(8).cast::<*mut AVIOContext>(),
            playlist_c.as_ptr(), AVIO_FLAG_WRITE);
        if ret < 0 {
            av_dict_free(&mut dict);
            avformat_close_input(&mut in_ctx);
            avformat_free_context(out_ctx);
            return Err(FfmpegError::Io(std::io::Error::new(
                std::io::ErrorKind::Other, format!("avio_open: {}", av_err2str(ret))
            )));
        }

        let ret = avformat_write_header(out_ctx, &mut dict);
        av_dict_free(&mut dict);
        if ret < 0 {
            avformat_close_input(&mut in_ctx);
            avformat_free_context(out_ctx);
            return Err(FfmpegError::Io(std::io::Error::new(
                std::io::ErrorKind::Other, format!("write_header: {}", av_err2str(ret))
            )));
        }

        // ── Read/write loop ───────────────────────────────────────────────
        let pkt = av_packet_alloc();
        if pkt.is_null() {
            avformat_close_input(&mut in_ctx);
            av_write_trailer(out_ctx);
            avformat_free_context(out_ctx);
            return Err(FfmpegError::Io(std::io::Error::new(std::io::ErrorKind::Other, "pkt alloc")));
        }

        loop {
            if cancel.load(Ordering::Relaxed) { break; }

            let ret = av_read_frame(in_ctx, pkt);
            if ret == AVERROR_EOF || ret < 0 { break; }

            let in_st_idx = (*pkt).stream_index as usize;
            if in_st_idx >= nb {
                av_packet_unref(pkt);
                continue;
            }
            let in_st  = *fmtctx_streams(in_ctx).add(in_st_idx);
            let out_st = *fmtctx_streams(out_ctx).add(in_st_idx);
            if in_st.is_null() || out_st.is_null() {
                av_packet_unref(pkt);
                continue;
            }

            av_packet_rescale_ts(pkt, (*in_st).time_base, (*out_st).time_base);
            (*pkt).pos = -1;

            let _ = av_interleaved_write_frame(out_ctx, pkt);
            av_packet_unref(pkt);
        }

        av_packet_free(&mut (pkt as *mut AVPacket));
        av_write_trailer(out_ctx);
        let mut pb = fmtctx_pb(out_ctx);
        avio_closep(&mut pb);
        avformat_free_context(out_ctx);
        avformat_close_input(&mut in_ctx);
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
) -> FfmpegResult<()> {
    let url_c    = CString::new(input_url).unwrap();
    let playlist = segment_dir.join("index.m3u8");
    let seg_pat  = segment_dir.join("seg%d.ts");
    let playlist_c = CString::new(playlist.to_string_lossy().as_ref()).unwrap();
    let seg_pat_c  = CString::new(seg_pat.to_string_lossy().as_ref()).unwrap();
    let hls_c    = c"hls";

    unsafe {
        // ── Open + seek input ─────────────────────────────────────────────
        let mut in_ctx: *mut AVFormatContext = std::ptr::null_mut();
        let ret = avformat_open_input(&mut in_ctx, url_c.as_ptr(), std::ptr::null(), std::ptr::null_mut());
        if ret < 0 {
            return Err(FfmpegError::Probe(format!("open_input: {}", av_err2str(ret))));
        }
        let ret = avformat_find_stream_info(in_ctx, std::ptr::null_mut());
        if ret < 0 {
            avformat_close_input(&mut in_ctx);
            return Err(FfmpegError::Probe(format!("find_stream_info: {}", av_err2str(ret))));
        }
        if start_offset_secs > 0.01 {
            let ts = (start_offset_secs * 1_000_000.0) as i64;
            let _ = av_seek_frame(in_ctx, -1, ts, AVSEEK_FLAG_BACKWARD);
        }

        // ── Find best streams ─────────────────────────────────────────────
        let want_video = preset.video_codec.is_some();
        let mut v_dec_codec: *const AVCodec = std::ptr::null();
        let mut a_dec_codec: *const AVCodec = std::ptr::null();
        let v_idx = if want_video {
            av_find_best_stream(in_ctx, AVMEDIA_TYPE_VIDEO, -1, -1, &mut v_dec_codec, 0)
        } else { -1 };
        let a_idx = av_find_best_stream(in_ctx, AVMEDIA_TYPE_AUDIO, -1, -1, &mut a_dec_codec, 0);

        let nb = fmtctx_nb_streams(in_ctx) as usize;
        let streams_ptr = fmtctx_streams(in_ctx);

        // ── Open decoders ─────────────────────────────────────────────────
        let mut v_dec_ctx: *mut AVCodecContext = std::ptr::null_mut();
        let mut a_dec_ctx: *mut AVCodecContext = std::ptr::null_mut();

        if v_idx >= 0 && !v_dec_codec.is_null() {
            v_dec_ctx = avcodec_alloc_context3(v_dec_codec);
            let in_st = *streams_ptr.add(v_idx as usize);
            avcodec_parameters_to_context(v_dec_ctx, (*in_st).codecpar);
            avcodec_open2(v_dec_ctx, v_dec_codec, std::ptr::null_mut());
        }
        if a_idx >= 0 && !a_dec_codec.is_null() {
            a_dec_ctx = avcodec_alloc_context3(a_dec_codec);
            let in_st = *streams_ptr.add(a_idx as usize);
            avcodec_parameters_to_context(a_dec_ctx, (*in_st).codecpar);
            avcodec_open2(a_dec_ctx, a_dec_codec, std::ptr::null_mut());
        }

        // ── Open encoders ─────────────────────────────────────────────────
        // Video encoder: try hw first, then sw
        let v_enc_codec: *const AVCodec = if want_video {
            let hw_name = CString::new("h264_mediacodec").unwrap();
            let sw_name = CString::new("libx264").unwrap();
            let c = if preset.video_hw_preferred {
                let hw = avcodec_find_encoder_by_name(hw_name.as_ptr());
                if hw.is_null() { avcodec_find_encoder_by_name(sw_name.as_ptr()) } else { hw }
            } else {
                avcodec_find_encoder_by_name(sw_name.as_ptr())
            };
            if c.is_null() { avcodec_find_encoder(AV_CODEC_ID_H264) } else { c }
        } else { std::ptr::null() };

        // Audio encoder
        let a_codec_id = match preset.audio_codec.as_str() {
            "mp3"  => AV_CODEC_ID_MP3,
            "copy" => 0u32,
            _      => AV_CODEC_ID_AAC,
        };
        let a_enc_codec: *const AVCodec = if a_codec_id > 0 {
            avcodec_find_encoder(a_codec_id)
        } else { std::ptr::null() };

        let mut v_enc_ctx: *mut AVCodecContext = std::ptr::null_mut();
        let mut a_enc_ctx: *mut AVCodecContext = std::ptr::null_mut();

        if want_video && !v_enc_codec.is_null() && !v_dec_ctx.is_null() {
            v_enc_ctx = avcodec_alloc_context3(v_enc_codec);
            // Compute output dimensions (scale down to preset max, keep aspect).
            let in_st = *streams_ptr.add(v_idx as usize);
            let src_w = (*(*in_st).codecpar).width;
            let src_h = (*(*in_st).codecpar).height;
            let (out_w, out_h) = fit_dimensions(
                src_w, src_h,
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
            avcodec_open2(v_enc_ctx, v_enc_codec, std::ptr::null_mut());
        }

        if !a_enc_codec.is_null() && !a_dec_ctx.is_null() {
            a_enc_ctx = avcodec_alloc_context3(a_enc_codec);
            codctx_set_sample_rate(a_enc_ctx, preset.audio_sample_rate.unwrap_or(48000) as i32);
            codctx_set_sample_fmt(a_enc_ctx, AV_SAMPLE_FMT_FLTP);
            let layout = if preset.audio_channels == 1 { AVChannelLayout::mono() } else { AVChannelLayout::stereo() };
            codctx_set_ch_layout(a_enc_ctx, layout);
            codctx_set_bit_rate(a_enc_ctx, preset.audio_bitrate as i64);
            avcodec_open2(a_enc_ctx, a_enc_codec, std::ptr::null_mut());
        }

        // ── Open HLS output ───────────────────────────────────────────────
        let mut out_ctx: *mut AVFormatContext = std::ptr::null_mut();
        avformat_alloc_output_context2(&mut out_ctx, std::ptr::null(), hls_c.as_ptr(), playlist_c.as_ptr());
        if out_ctx.is_null() {
            cleanup_transcode(in_ctx, out_ctx, v_dec_ctx, a_dec_ctx, v_enc_ctx, a_enc_ctx);
            return Err(FfmpegError::Io(std::io::Error::new(std::io::ErrorKind::Other, "alloc output")));
        }

        // Add output streams
        let mut out_v_idx: i32 = -1;
        let mut out_a_idx: i32 = -1;
        if !v_enc_ctx.is_null() {
            let out_st = avformat_new_stream(out_ctx, v_enc_codec);
            avcodec_parameters_from_context((*out_st).codecpar, v_enc_ctx);
            (*out_st).time_base = AVRational::new(1, 90000);
            out_v_idx = (*out_st).index;
        }
        if !a_enc_ctx.is_null() {
            let out_st = avformat_new_stream(out_ctx, a_enc_codec);
            avcodec_parameters_from_context((*out_st).codecpar, a_enc_ctx);
            (*out_st).time_base = AVRational::new(1, preset.audio_sample_rate.unwrap_or(48000) as i32);
            out_a_idx = (*out_st).index;
        }

        // Open output + write header
        let mut dict: *mut AVDictionary = std::ptr::null_mut();
        let seg_secs = preset.hls_segment_secs.to_string();
        let seg_secs_c = CString::new(seg_secs.as_str()).unwrap();
        av_dict_set(&mut dict, c"hls_time".as_ptr(), seg_secs_c.as_ptr(), 0);
        av_dict_set(&mut dict, c"hls_list_size".as_ptr(), c"0".as_ptr(), 0);
        av_dict_set(&mut dict, c"hls_segment_filename".as_ptr(), seg_pat_c.as_ptr(), 0);
        avio_open(&mut *(out_ctx as *mut u8).add(8).cast::<*mut AVIOContext>(),
            playlist_c.as_ptr(), AVIO_FLAG_WRITE);
        avformat_write_header(out_ctx, &mut dict);
        av_dict_free(&mut dict);

        // ── Decode/encode loop ────────────────────────────────────────────
        let in_pkt   = av_packet_alloc();
        let out_pkt  = av_packet_alloc();
        let dec_frame = av_frame_alloc();
        // Resize context for video scaling (allocated lazily).
        let mut sws: *mut SwsContext = std::ptr::null_mut();
        let mut swr: *mut SwrContext = std::ptr::null_mut();
        let enc_vframe = if !v_enc_ctx.is_null() {
            let f = av_frame_alloc();
            if !f.is_null() {
                let w = codctx_frame_size(v_enc_ctx); // repurpose: use 0 for video
                let (vw, vh) = {
                    let nb_stms = fmtctx_nb_streams(in_ctx) as usize;
                    let sps = fmtctx_streams(in_ctx);
                    if v_idx >= 0 && (v_idx as usize) < nb_stms {
                        let vst = *sps.add(v_idx as usize);
                        ((*(*vst).codecpar).width, (*(*vst).codecpar).height)
                    } else { (0, 0) }
                };
                let (ow, oh) = fit_dimensions(vw, vh,
                    preset.video_max_w.map(|v| v as i32),
                    preset.video_max_h.map(|v| v as i32));
                frame_set_width(f, ow);
                frame_set_height(f, oh);
                frame_set_format(f, AV_PIX_FMT_YUV420P);
                av_frame_get_buffer(f, 32);
                let _ = w;
            }
            f
        } else { std::ptr::null_mut() };

        loop {
            if cancel.load(Ordering::Relaxed) { break; }

            let ret = av_read_frame(in_ctx, in_pkt);
            if ret == AVERROR_EOF || ret < 0 { break; }

            let stream_idx = (*in_pkt).stream_index;

            if stream_idx == v_idx && !v_dec_ctx.is_null() {
                avcodec_send_packet(v_dec_ctx, in_pkt);
                loop {
                    let r = avcodec_receive_frame(v_dec_ctx, dec_frame);
                    if r == AVERROR_EAGAIN || r == AVERROR_EOF || r < 0 { break; }
                    // Scale
                    if !sws.is_null() || !enc_vframe.is_null() {
                        let dw = frame_width(enc_vframe);
                        let dh = frame_height(enc_vframe);
                        let sw = frame_width(dec_frame);
                        let sh = frame_height(dec_frame);
                        if sws.is_null() {
                            sws = sws_getContext(sw, sh, frame_format(dec_frame),
                                dw, dh, AV_PIX_FMT_YUV420P, 2,
                                std::ptr::null(), std::ptr::null(), std::ptr::null());
                        }
                        if !sws.is_null() {
                            let src_d = [frame_data(dec_frame, 0) as *const u8,
                                         frame_data(dec_frame, 1) as *const u8,
                                         frame_data(dec_frame, 2) as *const u8,
                                         std::ptr::null()];
                            let src_s = [frame_linesize(dec_frame, 0), frame_linesize(dec_frame, 1),
                                         frame_linesize(dec_frame, 2), 0];
                            let dst_d = [frame_data(enc_vframe, 0),
                                         frame_data(enc_vframe, 1),
                                         frame_data(enc_vframe, 2),
                                         std::ptr::null_mut()];
                            let dst_s = [frame_linesize(enc_vframe, 0), frame_linesize(enc_vframe, 1),
                                         frame_linesize(enc_vframe, 2), 0];
                            sws_scale(sws, src_d.as_ptr(), src_s.as_ptr(), 0, sh, dst_d.as_ptr(), dst_s.as_ptr());
                            frame_set_pts(enc_vframe, frame_pts(dec_frame));
                        }
                        encode_and_write(v_enc_ctx, enc_vframe, out_pkt, out_ctx, out_v_idx,
                            codctx_time_base(v_enc_ctx));
                    }
                    av_frame_unref(dec_frame);
                }
            } else if stream_idx == a_idx && !a_dec_ctx.is_null() {
                avcodec_send_packet(a_dec_ctx, in_pkt);
                loop {
                    let r = avcodec_receive_frame(a_dec_ctx, dec_frame);
                    if r == AVERROR_EAGAIN || r == AVERROR_EOF || r < 0 { break; }

                    if !a_enc_ctx.is_null() {
                        // Resample if needed
                        if swr.is_null() {
                            let src_layout = frame_ch_layout(dec_frame);
                            let dst_layout = AVChannelLayout::stereo();
                            let src_sr = frame_sample_rate(dec_frame);
                            let dst_sr = preset.audio_sample_rate.unwrap_or(48000) as i32;
                            swr_alloc_set_opts2(&mut swr,
                                &dst_layout, AV_SAMPLE_FMT_FLTP, dst_sr,
                                src_layout, frame_format(dec_frame) as AVSampleFormat, src_sr,
                                0, std::ptr::null_mut());
                            if !swr.is_null() { swr_init(swr); }
                        }
                        if !swr.is_null() {
                            // Allocate resampled frame
                            let resamp = av_frame_alloc();
                            if !resamp.is_null() {
                                swr_convert_frame(swr, resamp, dec_frame);
                                encode_and_write(a_enc_ctx, resamp, out_pkt, out_ctx, out_a_idx,
                                    codctx_time_base(a_enc_ctx));
                                av_frame_free(&mut (resamp as *mut AVFrame));
                            }
                        } else {
                            encode_and_write(a_enc_ctx, dec_frame, out_pkt, out_ctx, out_a_idx,
                                codctx_time_base(a_enc_ctx));
                        }
                    }
                    av_frame_unref(dec_frame);
                }
            }
            av_packet_unref(in_pkt);
        }

        // Flush encoders
        if !v_enc_ctx.is_null() {
            encode_and_write(v_enc_ctx, std::ptr::null(), out_pkt, out_ctx, out_v_idx,
                codctx_time_base(v_enc_ctx));
        }
        if !a_enc_ctx.is_null() {
            encode_and_write(a_enc_ctx, std::ptr::null(), out_pkt, out_ctx, out_a_idx,
                codctx_time_base(a_enc_ctx));
        }

        // Cleanup
        if !sws.is_null() { sws_freeContext(sws); }
        if !swr.is_null() { swr_free(&mut swr); }
        if !enc_vframe.is_null() { av_frame_free(&mut (enc_vframe as *mut AVFrame)); }
        av_frame_free(&mut (dec_frame as *mut AVFrame));
        av_packet_free(&mut (in_pkt as *mut AVPacket));
        av_packet_free(&mut (out_pkt as *mut AVPacket));

        av_write_trailer(out_ctx);
        let mut pb = fmtctx_pb(out_ctx);
        avio_closep(&mut pb);
        cleanup_transcode(in_ctx, out_ctx, v_dec_ctx, a_dec_ctx, v_enc_ctx, a_enc_ctx);
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
) {
    avcodec_send_frame(enc_ctx, frame);
    loop {
        let r = avcodec_receive_packet(enc_ctx, out_pkt);
        if r == AVERROR_EAGAIN || r == AVERROR_EOF || r < 0 { break; }
        (*out_pkt).stream_index = out_stream_idx;
        let nb = fmtctx_nb_streams(out_ctx) as usize;
        let sps = fmtctx_streams(out_ctx);
        if out_stream_idx >= 0 && (out_stream_idx as usize) < nb {
            let out_st = *sps.add(out_stream_idx as usize);
            av_packet_rescale_ts(out_pkt, enc_tb, (*out_st).time_base);
        }
        av_interleaved_write_frame(out_ctx, out_pkt);
        av_packet_unref(out_pkt);
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
    if !out_ctx.is_null() { avformat_free_context(out_ctx); }
    if !v_dec.is_null() { avcodec_free_context(&mut v_dec); }
    if !a_dec.is_null() { avcodec_free_context(&mut a_dec); }
    if !v_enc.is_null() { avcodec_free_context(&mut v_enc); }
    if !a_enc.is_null() { avcodec_free_context(&mut a_enc); }
}

/// Scale (src_w, src_h) down to fit within max_w x max_h, keeping aspect ratio.
/// If either max is None, no constraint on that axis.
pub fn fit_dimensions(
    src_w: i32, src_h: i32,
    max_w: Option<i32>, max_h: Option<i32>,
) -> (i32, i32) {
    if src_w <= 0 || src_h <= 0 { return (src_w, src_h); }
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

/// Read `AVFormatContext.pb` field — `*mut AVIOContext` at offset +8.
#[inline]
unsafe fn fmtctx_pb(ctx: *mut AVFormatContext) -> *mut AVIOContext {
    *(ctx as *const u8).add(8).cast()
}
