//! Pure remux / copy / encode decision logic for the three-layer preset model.
//!
//! These helpers involve no libav calls, so they compile (and are unit-tested)
//! on a host build without the `ffmpeg` feature — the Windows host has no
//! libav, which is exactly where the decision rules must be provable.
//!
//! The model is (see `preset-encoder-tiers` contract §1–§3):
//!
//! 1. copy the source stream when MPEG-TS can carry it and the preset allows it;
//! 2. otherwise encode with the preset's MediaCodec (hardware) encoder;
//! 3. otherwise encode with the preset's software encoder.
//!
//! [`decide_video_action`] / [`decide_audio_action`] implement that order for
//! one stream. The caller decides each stream once per session and caches the
//! result — reopen/seek restarts must reuse the same decision.

use wdmm_core::models::TranscodePreset;

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

// ── Three-layer stream decisions (contract §2) ───────────────────────────────

/// Decision for one video stream, fixed once per session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamAction<'a> {
    /// Pass the source bitstream through unchanged (MPEG-TS compatible codec).
    Copy,
    /// Re-encode; `encoder` is a name accepted by
    /// `avcodec_find_encoder_by_name`, `tier` is the preset's
    /// `fast`/`balanced`/`quality` knob string.
    Encode {
        /// `true` for a MediaCodec (hardware) encoder, `false` for software.
        hardware: bool,
        /// Encoder name from the preset (`""` never appears here).
        encoder: &'a str,
        /// Tier string from the preset.
        tier: &'a str,
    },
}

impl<'a> StreamAction<'a> {
    /// `true` for [`StreamAction::Copy`].
    pub fn is_copy(&self) -> bool {
        matches!(self, StreamAction::Copy)
    }

    /// Encoder name when encoding, `None` for copy.
    pub fn encoder_name(&self) -> Option<&'a str> {
        match self {
            StreamAction::Copy => None,
            StreamAction::Encode { encoder, .. } => Some(encoder),
        }
    }

    /// Tier string when encoding, `None` for copy.
    pub fn tier(&self) -> Option<&'a str> {
        match self {
            StreamAction::Copy => None,
            StreamAction::Encode { tier, .. } => Some(tier),
        }
    }
}

/// Decision for one audio stream, fixed once per session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioAction<'a> {
    /// Pass the source audio through unchanged (MPEG-TS compatible codec).
    Copy,
    /// Re-encode with the preset's `audio_codec` (`aac`/`ac3`/`eac3`/`mp2`).
    Encode { encoder: &'a str },
}

impl<'a> AudioAction<'a> {
    /// `true` for [`AudioAction::Copy`].
    pub fn is_copy(&self) -> bool {
        matches!(self, AudioAction::Copy)
    }

    /// Encoder name when encoding, `None` for copy.
    pub fn encoder_name(&self) -> Option<&'a str> {
        match self {
            AudioAction::Copy => None,
            AudioAction::Encode { encoder } => Some(encoder),
        }
    }
}

/// Decide the video action for a source stream (contract §2).
///
/// Order: copy (when allowed and MPEG-TS compatible) → hardware encode (when
/// configured *and* the device encoder is available) → software encode →
/// error. `hw_encoder_available` is the caller's `avcodec_find_encoder_by_name`
/// probe plus a successful `avcodec_open2` attempt; presence alone is not proof
/// that the device codec works.
pub fn decide_video_action<'a>(
    preset: &'a TranscodePreset,
    source_codec_id: u32,
    hw_encoder_available: bool,
) -> Result<StreamAction<'a>, String> {
    if preset.video_copy && crate::preset::is_mpegts_video_copy_compatible(source_codec_id) {
        return Ok(StreamAction::Copy);
    }
    if !preset.hw_video_encoder.is_empty() && hw_encoder_available {
        return Ok(StreamAction::Encode {
            hardware: true,
            encoder: &preset.hw_video_encoder,
            tier: &preset.hw_video_tier,
        });
    }
    if !preset.sw_video_encoder.is_empty() {
        return Ok(StreamAction::Encode {
            hardware: false,
            encoder: &preset.sw_video_encoder,
            tier: &preset.sw_video_tier,
        });
    }
    Err(format!(
        "no video encoder available for preset {}",
        preset.id
    ))
}

/// Decide the audio action for a source stream (contract §2).
///
/// Audio has no hardware encoder: copy when allowed and MPEG-TS compatible,
/// otherwise encode with the preset's `audio_codec`. Audio-only sources (no
/// video stream) still go through this same decision.
pub fn decide_audio_action<'a>(
    preset: &'a TranscodePreset,
    source_codec_id: u32,
) -> AudioAction<'a> {
    if preset.audio_copy && crate::preset::is_mpegts_audio_copy_compatible(source_codec_id) {
        return AudioAction::Copy;
    }
    AudioAction::Encode {
        encoder: &preset.audio_codec,
    }
}

/// `true` when a software fallback downgrades the intended HEVC output to
/// H.264 (software HEVC encoding is not shipped, contract §2).
///
/// Callers must log `video: sw fallback downgrades hevc to h264` when this is
/// true and the software branch actually runs.
pub fn sw_fallback_downgrades_hevc(preset: &TranscodePreset) -> bool {
    preset.hw_video_encoder == "hevc_mediacodec" && preset.sw_video_encoder == "libx264"
}

// ── Tier mapping (contract §3) ───────────────────────────────────────────────

/// `x264` `preset` value for a tier (`fast|balanced|quality`).
///
/// Returns `None` for unknown tiers: the caller then keeps libx264's default
/// rather than guessing.
pub fn x264_preset_for_tier(tier: &str) -> Option<&'static str> {
    match tier {
        "fast" => Some("veryfast"),
        "balanced" => Some("medium"),
        "quality" => Some("slow"),
        _ => None,
    }
}

/// Bitrate multiplier for a tier: faster tiers spend less bitrate for the same
/// quality target. Unknown tiers fall back to `1.0` (no adjustment).
pub fn bitrate_factor_for_tier(tier: &str) -> f64 {
    match tier {
        "fast" => 0.85,
        "quality" => 1.35,
        _ => 1.0,
    }
}

/// Default video bitrate used when the preset does not set one:
/// `width * height * fps * 0.10`, floored at 800 kbit/s.
pub fn default_video_bitrate(width: u32, height: u32, fps: f64) -> u32 {
    let fps = if fps.is_finite() && fps > 0.0 {
        fps
    } else {
        30.0
    };
    let derived = (f64::from(width) * f64::from(height) * fps * 0.10).round();
    (derived as u64).max(800_000) as u32
}

/// Effective video bitrate for one tier: `(video_max_bitrate or the envelope
/// default) * bitrate_factor_for_tier(tier)`.
pub fn target_video_bitrate_for_tier(
    preset: &TranscodePreset,
    width: u32,
    height: u32,
    fps: f64,
    tier: &str,
) -> u32 {
    let base = preset
        .video_max_bitrate
        .unwrap_or_else(|| default_video_bitrate(width, height, fps));
    (f64::from(base) * bitrate_factor_for_tier(tier))
        .round()
        .max(1.0) as u32
}

/// One-line diag for the decided video layer (contract §3): emitted once per
/// session.
pub fn video_plan_diag(action: StreamAction<'_>, bitrate: u32) -> String {
    match action {
        StreamAction::Copy => format!(
            "video plan: action=copy encoder=none tier=none x264_preset=none bitrate={bitrate}"
        ),
        StreamAction::Encode {
            hardware,
            encoder,
            tier,
        } => {
            let action = if hardware { "hw" } else { "sw" };
            let x264 = x264_preset_for_tier(tier).unwrap_or("none");
            format!(
                "video plan: action={action} encoder={encoder} tier={tier} x264_preset={x264} bitrate={bitrate}"
            )
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

/// Encoder GOP size (frames) for a segment boundary: `round(seg_secs * fps)`.
///
/// The exact transcode grid assumes a keyframe on every `k * seg_secs` boundary,
/// and the HLS muxer only cuts encoded streams on keyframes. Setting the
/// encoder's GOP to exactly one segment worth of frames — with B-frames off and
/// scene-cut keyframes disabled — makes the produced `EXTINF` values match
/// [`exact_plan`](crate::vod::exact_plan).
///
/// Returns at least one frame; a non-positive frame rate falls back to 30 fps and
/// pathological inputs are capped at one hour of frames (no 32-bit overflow).
pub fn segment_gop_size(fps_num: i32, fps_den: i32, seg_secs: u32) -> i32 {
    let fps = if fps_num > 0 && fps_den > 0 {
        fps_num as f64 / fps_den as f64
    } else {
        30.0
    };
    let frames = (f64::from(seg_secs.max(1)) * fps).round();
    (frames as i64).clamp(1, 60 * 60) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preset::{codec_ids, preset_h264, preset_hevc};

    #[test]
    fn remux_plan_matrix_matches_stream_compatibility() {
        use codec_ids::*;
        // Both copyable → pure remux.
        assert_eq!(remux_plan(Some(H264), Some(AAC)), RemuxPlan::Remux);
        assert_eq!(remux_plan(Some(HEVC), Some(AC3)), RemuxPlan::Remux);
        assert_eq!(remux_plan(Some(MPEG2VIDEO), Some(EAC3)), RemuxPlan::Remux);
        assert_eq!(remux_plan(Some(H264), Some(MP3)), RemuxPlan::Remux);
        // Video copyable, audio not → hybrid (the HEVC+Opus case).
        assert_eq!(remux_plan(Some(H264), Some(OPUS_ID)), RemuxPlan::Hybrid);
        assert_eq!(remux_plan(Some(HEVC), Some(FLAC_ID)), RemuxPlan::Hybrid);
        // Video not copyable → full transcode.
        assert_eq!(
            remux_plan(Some(VP9_ID), Some(OPUS_ID)),
            RemuxPlan::Transcode
        );
        assert_eq!(remux_plan(Some(AV1_ID), Some(AAC)), RemuxPlan::Transcode);
        // No audio is fine: video-only remux.
        assert_eq!(remux_plan(Some(H264), None), RemuxPlan::Remux);
        assert_eq!(remux_plan(None, None), RemuxPlan::Transcode);
    }

    // Codecs not modelled by the preset layer; values are only used by these
    // tests (FFmpeg 9.0 numbering, see `preset::codec_ids`).
    const VP9_ID: u32 = 166;
    const AV1_ID: u32 = 222;
    const OPUS_ID: u32 = 86076;
    const FLAC_ID: u32 = 86028;

    #[test]
    fn audio_only_sources_stay_on_the_cheap_path() {
        use codec_ids::*;
        // Audio-only source (no video stream): copy the audio when MPEG-TS can
        // carry it, otherwise hybrid re-encodes only the audio.
        assert_eq!(remux_plan(None, Some(AAC)), RemuxPlan::Remux);
        assert_eq!(remux_plan(None, Some(MP3)), RemuxPlan::Remux);
        assert_eq!(remux_plan(None, Some(AC3)), RemuxPlan::Remux);
        assert_eq!(remux_plan(None, Some(EAC3)), RemuxPlan::Remux);
        assert_eq!(remux_plan(None, Some(OPUS_ID)), RemuxPlan::Hybrid);
        assert_eq!(remux_plan(None, Some(FLAC_ID)), RemuxPlan::Hybrid);
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
    fn four_quadrants_are_decidable() {
        // The built-ins allow copy, so a compatible source copies both streams.
        let preset = preset_h264();
        assert_eq!(
            decide_video_action(&preset, codec_ids::H264, true),
            Ok(StreamAction::Copy)
        );
        assert_eq!(
            decide_audio_action(&preset, codec_ids::AAC),
            AudioAction::Copy
        );

        // copy + encode: video copies, Opus audio must be re-encoded.
        assert_eq!(
            decide_video_action(&preset, codec_ids::H264, true),
            Ok(StreamAction::Copy)
        );
        assert_eq!(
            decide_audio_action(&preset, OPUS_ID),
            AudioAction::Encode { encoder: "aac" }
        );

        // encode + copy: VP9 video cannot be copied, AAC audio can.
        assert_eq!(
            decide_video_action(&preset, VP9_ID, true),
            Ok(StreamAction::Encode {
                hardware: true,
                encoder: "h264_mediacodec",
                tier: "balanced",
            })
        );
        assert_eq!(
            decide_audio_action(&preset, codec_ids::AAC),
            AudioAction::Copy
        );

        // encode + encode: VP9 video + Opus audio both re-encoded.
        assert_eq!(
            decide_video_action(&preset, VP9_ID, false),
            Ok(StreamAction::Encode {
                hardware: false,
                encoder: "libx264",
                tier: "balanced",
            })
        );
        assert_eq!(
            decide_audio_action(&preset, OPUS_ID),
            AudioAction::Encode { encoder: "aac" }
        );
    }

    #[test]
    fn copy_off_and_hardware_unavailable_fall_through_to_software() {
        let mut preset = preset_h264();
        preset.video_copy = false;
        preset.audio_copy = false;
        assert_eq!(
            decide_video_action(&preset, codec_ids::H264, true),
            Ok(StreamAction::Encode {
                hardware: true,
                encoder: "h264_mediacodec",
                tier: "balanced",
            })
        );
        assert_eq!(
            decide_video_action(&preset, codec_ids::H264, false),
            Ok(StreamAction::Encode {
                hardware: false,
                encoder: "libx264",
                tier: "balanced",
            })
        );
        assert_eq!(
            decide_audio_action(&preset, codec_ids::AAC),
            AudioAction::Encode { encoder: "aac" }
        );
    }

    #[test]
    fn no_usable_layer_is_a_loud_error() {
        let mut preset = preset_h264();
        preset.video_copy = false;
        preset.hw_video_encoder = String::new();
        preset.sw_video_encoder = String::new();
        let err = decide_video_action(&preset, VP9_ID, true).unwrap_err();
        assert_eq!(err, "no video encoder available for preset h264");
    }

    #[test]
    fn sw_fallback_downgrades_hevc_only_for_the_hevc_builtin() {
        assert!(sw_fallback_downgrades_hevc(&preset_hevc()));
        assert!(!sw_fallback_downgrades_hevc(&preset_h264()));
    }

    #[test]
    fn tier_maps_to_x264_preset_and_bitrate_factor() {
        assert_eq!(x264_preset_for_tier("fast"), Some("veryfast"));
        assert_eq!(x264_preset_for_tier("balanced"), Some("medium"));
        assert_eq!(x264_preset_for_tier("quality"), Some("slow"));
        assert_eq!(x264_preset_for_tier("turbo"), None);
        assert_eq!(bitrate_factor_for_tier("fast"), 0.85);
        assert_eq!(bitrate_factor_for_tier("balanced"), 1.0);
        assert_eq!(bitrate_factor_for_tier("quality"), 1.35);
        // Unknown tier does not silently boost/shrink the bitrate.
        assert_eq!(bitrate_factor_for_tier("turbo"), 1.0);
    }

    #[test]
    fn target_bitrate_uses_envelope_or_derives_with_a_floor() {
        let preset = preset_h264();
        // Explicit envelope (4 Mbps) × balanced factor 1.0.
        assert_eq!(
            target_video_bitrate_for_tier(&preset, 1920, 1080, 30.0, "balanced"),
            4_000_000
        );
        // quality = 1.35 × 4 Mbps.
        assert_eq!(
            target_video_bitrate_for_tier(&preset, 1920, 1080, 30.0, "quality"),
            5_400_000
        );
        // No envelope: 320x180@15 derives 86_400 → floored to 800_000.
        let mut bare = preset.clone();
        bare.video_max_bitrate = None;
        assert_eq!(
            target_video_bitrate_for_tier(&bare, 320, 180, 15.0, "balanced"),
            800_000
        );
        // The envelope-less default is the same value used by the plain helper.
        assert_eq!(default_video_bitrate(320, 180, 15.0), 800_000);
        // 1920x1080@30 derives 6_220_800.
        assert_eq!(default_video_bitrate(1920, 1080, 30.0), 6_220_800);
    }

    #[test]
    fn video_plan_diag_carries_action_encoder_tier_and_bitrate() {
        let copy = video_plan_diag(StreamAction::Copy, 4_000_000);
        assert_eq!(
            copy,
            "video plan: action=copy encoder=none tier=none x264_preset=none bitrate=4000000"
        );
        let hw = video_plan_diag(
            StreamAction::Encode {
                hardware: true,
                encoder: "hevc_mediacodec",
                tier: "quality",
            },
            3_000_000,
        );
        assert_eq!(
            hw,
            "video plan: action=hw encoder=hevc_mediacodec tier=quality x264_preset=slow bitrate=3000000"
        );
        let sw = video_plan_diag(
            StreamAction::Encode {
                hardware: false,
                encoder: "libx264",
                tier: "fast",
            },
            1_000_000,
        );
        assert_eq!(
            sw,
            "video plan: action=sw encoder=libx264 tier=fast x264_preset=veryfast bitrate=1000000"
        );
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

    #[test]
    fn segment_gop_is_one_segment_worth_of_frames() {
        // 30 fps × 4 s segments → a keyframe every 120 frames.
        assert_eq!(segment_gop_size(30, 1, 4), 120);
        // 23.976 fps × 4 s → 96 frames (rounded).
        assert_eq!(segment_gop_size(24000, 1001, 4), 96);
        // 25 fps × 2 s → 50 frames.
        assert_eq!(segment_gop_size(25, 1, 2), 50);
        // Degenerate input rate defaults to 30 fps; zero seconds means one frame.
        assert_eq!(segment_gop_size(0, 0, 4), 120);
        assert_eq!(segment_gop_size(30, 1, 0), 30);
    }
}
