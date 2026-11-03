//! Pure remux / hybrid / transcode decision logic.
//!
//! These helpers involve no libav calls, so they compile (and are unit-tested)
//! on a host build without the `ffmpeg` feature — the Windows host has no
//! libav, which is exactly where the decision rules must be provable.

/// What the cheap remux stage should do for the selected best streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemuxPlan {
    /// Video and audio can both be copied verbatim (`-c copy`).
    Remux,
    /// Video is copied verbatim, audio is decoded/re-encoded (e.g. HEVC+Opus).
    Hybrid,
    /// Video cannot be copied; the caller falls back to the full transcode path.
    Transcode,
}

/// Decide the remux-stage plan from the best video/audio codec ids.
///
/// Audio-only sources (music, no video stream) keep the cheap path too: the
/// audio track is copied when MPEG-TS can carry it, and otherwise only the
/// audio is re-encoded — the hybrid machinery already does exactly that and
/// simply has no video stream to copy alongside.
pub fn remux_plan(video_codec_id: Option<u32>, audio_codec_id: Option<u32>) -> RemuxPlan {
    let Some(video_id) = video_codec_id else {
        return match audio_codec_id {
            Some(audio_id) if crate::preset::is_mpegts_audio_copy_compatible(audio_id) => {
                RemuxPlan::Remux
            }
            Some(_) => RemuxPlan::Hybrid,
            None => RemuxPlan::Transcode,
        };
    };
    if !crate::preset::is_mpegts_video_copy_compatible(video_id) {
        return RemuxPlan::Transcode;
    }
    match audio_codec_id {
        None => RemuxPlan::Remux,
        Some(audio_id) if crate::preset::is_mpegts_audio_copy_compatible(audio_id) => {
            RemuxPlan::Remux
        }
        Some(_) => RemuxPlan::Hybrid,
    }
}

/// Streams kept in the HLS output: the best video stream and the best audio
/// stream (`-1` = not found). Everything else is dropped.
pub fn remux_stream_selection(best_video: i32, best_audio: i32) -> (Option<usize>, Option<usize>) {
    fn idx(i: i32) -> Option<usize> {
        (i >= 0).then_some(i as usize)
    }
    (idx(best_video), idx(best_audio))
}

/// Input indices that will NOT be muxed, so the diag log can explain drops.
pub fn remux_dropped_streams(best_video: i32, best_audio: i32, nb_streams: usize) -> Vec<usize> {
    let (keep_v, keep_a) = remux_stream_selection(best_video, best_audio);
    (0..nb_streams)
        .filter(|i| Some(*i) != keep_v && Some(*i) != keep_a)
        .collect()
}

/// Audio codec used when the hybrid path has to re-encode the audio track.
/// The preset's `"copy"` choice is impossible in hybrid mode, so it maps to AAC.
pub fn hybrid_audio_codec_id(audio_codec: &str) -> u32 {
    if audio_codec == "mp3" {
        crate::sys::AV_CODEC_ID_MP3
    } else {
        crate::sys::AV_CODEC_ID_AAC
    }
}

// ── Video encoder selection (hardware MediaCodec vs. libx264) ────────────────

/// MediaCodec H.264 encoder name.
pub const HW_H264_ENCODER: &str = "h264_mediacodec";
/// MediaCodec H.265/HEVC encoder name.
pub const HW_HEVC_ENCODER: &str = "hevc_mediacodec";
/// Software H.264 encoder name (the universal fallback).
pub const SW_H264_ENCODER: &str = "libx264";

/// Hardware encoder to request for the preset's video codec.
///
/// Only an explicit HEVC request selects `hevc_mediacodec`; everything else
/// (including `None`) keeps the H.264 path — the codec is never switched
/// silently.
pub fn hw_video_encoder_name(video_codec: Option<&str>) -> &'static str {
    match video_codec {
        Some(codec) if codec.eq_ignore_ascii_case("hevc") || codec.eq_ignore_ascii_case("h265") => {
            HW_HEVC_ENCODER
        }
        _ => HW_H264_ENCODER,
    }
}

/// Why the transcode path ended up on the software encoder (if it did).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoftwareReason {
    /// The preset did not ask for hardware encoding.
    NotPreferred,
    /// The requested MediaCodec encoder is not compiled into libavcodec.
    HardwareUnavailable,
    /// The encoder exists but `avcodec_open2` failed (device codec missing or
    /// configuration rejected).
    HardwareOpenFailed,
}

/// Video encoder chosen for the transcode path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoEncoderChoice {
    /// A MediaCodec encoder that already opened successfully.
    Hardware { name: &'static str },
    /// libx264, with the reason hardware was not used.
    Software {
        name: &'static str,
        reason: SoftwareReason,
    },
}

/// Decide which video encoder to use after probing.
///
/// `hw_codec_present` is `avcodec_find_encoder_by_name(...) != NULL`;
/// `hw_open_ok` is a real `avcodec_open2` success. Hardware is only chosen when
/// both are true — presence alone is not proof that the device codec works.
pub fn choose_video_encoder(
    video_codec: Option<&str>,
    hw_preferred: bool,
    hw_codec_present: bool,
    hw_open_ok: bool,
) -> VideoEncoderChoice {
    if !hw_preferred {
        return VideoEncoderChoice::Software {
            name: SW_H264_ENCODER,
            reason: SoftwareReason::NotPreferred,
        };
    }
    if !hw_codec_present {
        return VideoEncoderChoice::Software {
            name: SW_H264_ENCODER,
            reason: SoftwareReason::HardwareUnavailable,
        };
    }
    if !hw_open_ok {
        return VideoEncoderChoice::Software {
            name: SW_H264_ENCODER,
            reason: SoftwareReason::HardwareOpenFailed,
        };
    }
    VideoEncoderChoice::Hardware {
        name: hw_video_encoder_name(video_codec),
    }
}

/// Never-silent fallback message for `EncoderInfo.warnings` / the diag log.
///
/// Returns `None` when no fallback happened (hardware was used, or the preset
/// never asked for hardware). The fallback reason is always spelled out.
pub fn fallback_warning(
    choice: VideoEncoderChoice,
    requested_hw: &str,
    open_error: Option<&str>,
) -> Option<String> {
    match choice {
        VideoEncoderChoice::Hardware { .. } => None,
        VideoEncoderChoice::Software {
            reason: SoftwareReason::NotPreferred,
            ..
        } => None,
        VideoEncoderChoice::Software {
            reason: SoftwareReason::HardwareUnavailable,
            ..
        } => Some(format!("{requested_hw} 不可用，已回退 {SW_H264_ENCODER}")),
        VideoEncoderChoice::Software {
            reason: SoftwareReason::HardwareOpenFailed,
            ..
        } => {
            let err = open_error.unwrap_or("unknown error");
            Some(format!(
                "{requested_hw} 打开失败（{err}），已回退 {SW_H264_ENCODER}"
            ))
        }
    }
}

/// Frame rate to configure on the video encoder.
///
/// Uses the source stream rate when it is sane and not above the preset cap;
/// otherwise falls back to the cap (or 30 fps). Returns `(num, den)`.
pub fn encoder_frame_rate(input_num: i32, input_den: i32, fps_cap: Option<u32>) -> (i32, i32) {
    let sane_source = (input_num > 0 && input_den > 0).then(|| input_num as f64 / input_den as f64);
    let cap = fps_cap.filter(|c| *c > 0);
    if let Some(fps) = sane_source {
        if (1.0..=240.0).contains(&fps) && cap.is_none_or(|c| fps <= c as f64) {
            return (input_num, input_den);
        }
    }
    let base = match sane_source {
        Some(fps) if (1.0..=240.0).contains(&fps) => fps,
        _ => 30.0,
    };
    let capped = cap.map_or(base, |c| base.min(c as f64)).max(1.0);
    (capped.round() as i32, 1)
}

/// GOP size (frames) for a hardware encoder.
///
/// `mediacodecenc.c` derives the I-frame interval from
/// `round(gop_size / fps)` and warns when it rounds to 0, so keep at least two
/// seconds between keyframes (MediaCodec default behaviour would be one).
pub fn hw_gop_size(fps_num: i32, fps_den: i32) -> i32 {
    if fps_num <= 0 || fps_den <= 0 {
        return 60;
    }
    let fps = fps_num as f64 / fps_den as f64;
    ((2.0 * fps).round() as i32).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Codec ids used by the tests (AVCodecID values).
    const H264: u32 = 27;
    const VP9: u32 = 167;
    const HEVC: u32 = 173;
    const AV1: u32 = 226;
    const MPEG2: u32 = 2;
    const MP3: u32 = 86017;
    const AAC: u32 = 86018;
    const AC3: u32 = 86019;
    const EAC3: u32 = 86056;
    const FLAC: u32 = 86028;
    const OPUS: u32 = 86076;

    #[test]
    fn remux_plan_matrix_matches_stream_compatibility() {
        // Both copyable → pure remux.
        assert_eq!(remux_plan(Some(H264), Some(AAC)), RemuxPlan::Remux);
        assert_eq!(remux_plan(Some(HEVC), Some(AC3)), RemuxPlan::Remux);
        assert_eq!(remux_plan(Some(MPEG2), Some(EAC3)), RemuxPlan::Remux);
        assert_eq!(remux_plan(Some(H264), Some(MP3)), RemuxPlan::Remux);
        // Video copyable, audio not → hybrid (the Feibanyama HEVC+Opus case).
        assert_eq!(remux_plan(Some(H264), Some(OPUS)), RemuxPlan::Hybrid);
        assert_eq!(remux_plan(Some(HEVC), Some(FLAC)), RemuxPlan::Hybrid);
        assert_eq!(remux_plan(Some(MPEG2), Some(OPUS)), RemuxPlan::Hybrid);
        // Video not copyable → full transcode.
        assert_eq!(remux_plan(Some(VP9), Some(OPUS)), RemuxPlan::Transcode);
        assert_eq!(remux_plan(Some(AV1), Some(AAC)), RemuxPlan::Transcode);
        // No audio is fine: video-only remux.
        assert_eq!(remux_plan(Some(H264), None), RemuxPlan::Remux);
        assert_eq!(remux_plan(None, None), RemuxPlan::Transcode);
    }

    #[test]
    fn audio_only_sources_stay_on_the_cheap_path() {
        // Audio-only source (no video stream): copy the audio when MPEG-TS can
        // carry it, otherwise hybrid re-encodes only the audio.
        assert_eq!(remux_plan(None, Some(AAC)), RemuxPlan::Remux);
        assert_eq!(remux_plan(None, Some(MP3)), RemuxPlan::Remux);
        assert_eq!(remux_plan(None, Some(AC3)), RemuxPlan::Remux);
        assert_eq!(remux_plan(None, Some(EAC3)), RemuxPlan::Remux);
        assert_eq!(remux_plan(None, Some(OPUS)), RemuxPlan::Hybrid);
        assert_eq!(remux_plan(None, Some(FLAC)), RemuxPlan::Hybrid);
        // Nothing playable at all still falls back to the transcode path.
        assert_eq!(remux_plan(None, None), RemuxPlan::Transcode);
    }

    #[test]
    fn stream_selection_maps_best_streams() {
        assert_eq!(remux_stream_selection(0, 1), (Some(0), Some(1)));
        assert_eq!(remux_stream_selection(-1, 1), (None, Some(1)));
        assert_eq!(remux_stream_selection(2, -1), (Some(2), None));
        assert_eq!(remux_stream_selection(-1, -1), (None, None));
    }

    #[test]
    fn stream_selection_drops_everything_else() {
        assert_eq!(remux_dropped_streams(0, 1, 5), vec![2, 3, 4]);
        assert_eq!(remux_dropped_streams(-1, 1, 3), vec![0, 2]);
        assert_eq!(remux_dropped_streams(0, -1, 2), vec![1]);
        assert_eq!(remux_dropped_streams(-1, -1, 2), vec![0, 1]);
        assert!(remux_dropped_streams(0, 1, 2).is_empty());
    }

    #[test]
    fn hybrid_audio_codec_is_aac_or_mp3() {
        assert_eq!(hybrid_audio_codec_id("mp3"), MP3);
        assert_eq!(hybrid_audio_codec_id("aac"), AAC);
        // Any other preset audio codec falls back to AAC for the hybrid path.
        assert_eq!(hybrid_audio_codec_id("opus"), AAC);
        assert_eq!(hybrid_audio_codec_id("copy"), AAC);
    }

    #[test]
    fn hardware_encoder_name_follows_the_preset_codec() {
        assert_eq!(hw_video_encoder_name(Some("h264")), HW_H264_ENCODER);
        assert_eq!(hw_video_encoder_name(Some("H264")), HW_H264_ENCODER);
        assert_eq!(hw_video_encoder_name(Some("hevc")), HW_HEVC_ENCODER);
        assert_eq!(hw_video_encoder_name(Some("h265")), HW_HEVC_ENCODER);
        // No explicit codec request keeps H.264 — never silently switch codecs.
        assert_eq!(hw_video_encoder_name(None), HW_H264_ENCODER);
    }

    #[test]
    fn video_encoder_choice_prefers_opened_hardware() {
        // H.264 preset + hardware preferred + present + opened → hardware.
        assert_eq!(
            choose_video_encoder(Some("h264"), true, true, true),
            VideoEncoderChoice::Hardware {
                name: HW_H264_ENCODER
            }
        );
        // Explicit HEVC request selects the HEVC encoder.
        assert_eq!(
            choose_video_encoder(Some("hevc"), true, true, true),
            VideoEncoderChoice::Hardware {
                name: HW_HEVC_ENCODER
            }
        );
        // Not preferred → software, and the reason says so.
        assert_eq!(
            choose_video_encoder(Some("h264"), false, true, true),
            VideoEncoderChoice::Software {
                name: SW_H264_ENCODER,
                reason: SoftwareReason::NotPreferred
            }
        );
        // Missing from libavcodec → software.
        assert_eq!(
            choose_video_encoder(Some("h264"), true, false, false),
            VideoEncoderChoice::Software {
                name: SW_H264_ENCODER,
                reason: SoftwareReason::HardwareUnavailable
            }
        );
        // Present but the device codec could not be opened → software; presence
        // alone must never be treated as usable.
        assert_eq!(
            choose_video_encoder(Some("hevc"), true, true, false),
            VideoEncoderChoice::Software {
                name: SW_H264_ENCODER,
                reason: SoftwareReason::HardwareOpenFailed
            }
        );
    }

    #[test]
    fn fallback_warnings_are_never_silent() {
        let hw = choose_video_encoder(Some("h264"), true, true, true);
        assert_eq!(fallback_warning(hw, HW_H264_ENCODER, None), None);
        let not_preferred = choose_video_encoder(Some("h264"), false, true, true);
        assert_eq!(fallback_warning(not_preferred, HW_H264_ENCODER, None), None);
        let unavailable = choose_video_encoder(Some("h264"), true, false, false);
        let msg = fallback_warning(unavailable, HW_H264_ENCODER, None).unwrap();
        assert!(msg.contains(HW_H264_ENCODER), "{msg}");
        assert!(msg.contains(SW_H264_ENCODER), "{msg}");
        assert!(msg.contains("不可用"), "{msg}");
        let open_failed = choose_video_encoder(Some("hevc"), true, true, false);
        let msg = fallback_warning(open_failed, HW_HEVC_ENCODER, Some("rc=-38")).unwrap();
        assert!(msg.contains(HW_HEVC_ENCODER), "{msg}");
        assert!(msg.contains("rc=-38"), "{msg}");
        assert!(msg.contains("回退"), "{msg}");
    }

    #[test]
    fn encoder_frame_rate_keeps_sane_source_and_respects_cap() {
        // 29.97 fps source, no cap → exact source rational.
        assert_eq!(encoder_frame_rate(30000, 1001, None), (30000, 1001));
        // 60 fps source with a 30 fps cap → 30/1.
        assert_eq!(encoder_frame_rate(60, 1, Some(30)), (30, 1));
        // Unknown/missing source rate → 30 fps default, cap still honoured.
        assert_eq!(encoder_frame_rate(0, 1, None), (30, 1));
        assert_eq!(encoder_frame_rate(0, 0, Some(24)), (24, 1));
        // Nonsense source (e.g. 1000 fps) falls back to the default.
        assert_eq!(encoder_frame_rate(1000, 1, None), (30, 1));
    }

    #[test]
    fn hardware_gop_is_two_seconds() {
        assert_eq!(hw_gop_size(30, 1), 60);
        assert_eq!(hw_gop_size(30000, 1001), 60);
        assert_eq!(hw_gop_size(60, 1), 120);
        // Degenerate input rate keeps a usable 2 s GOP at 30 fps.
        assert_eq!(hw_gop_size(0, 0), 60);
    }
}
