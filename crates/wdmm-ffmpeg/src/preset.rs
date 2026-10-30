//! Preset constants and helpers for `wdmm-ffmpeg` (B3.5).
//!
//! The five built-in `TranscodePreset` values mirror the seeds written to the
//! `transcode_presets` DB table in `wdmm-storage` v11. The DB is the primary
//! source of truth; these constants are used as fallbacks when the DB is not
//! available (e.g. in unit tests or probe-only contexts).

use wdmm_core::models::TranscodePreset;

/// Build the preset with `id = "passthrough"`.
///
/// Container `"original"` means pass-through without re-muxing — the input
/// stream is served directly. `remux_first = false` disables the remux attempt
/// so the proxy hands the raw stream straight to the renderer.
pub fn preset_passthrough() -> TranscodePreset {
    TranscodePreset {
        id:                 "passthrough".into(),
        label:              "Direct / Pass-through".into(),
        container:          "original".into(),
        video_codec:        None,
        video_hw_preferred: false,
        video_max_w:        None,
        video_max_h:        None,
        video_max_bitrate:  None,
        video_fps_cap:      None,
        audio_codec:        "copy".into(),
        audio_bitrate:      0,
        audio_channels:     0,
        audio_sample_rate:  None,
        hls_segment_secs:   4,
        remux_first:        false,
        sort_order:         0,
        is_builtin:         true,
    }
}

/// Build the preset with `id = "dlna-compat"`.
///
/// H.264 + AAC, 720p cap, 2 Mbps video — the widest-compatible DLNA profile.
pub fn preset_dlna_compat() -> TranscodePreset {
    TranscodePreset {
        id:                 "dlna-compat".into(),
        label:              "DLNA Compatible (default)".into(),
        container:          "mpegts".into(),
        video_codec:        Some("h264".into()),
        video_hw_preferred: true,
        video_max_w:        Some(1280),
        video_max_h:        Some(720),
        video_max_bitrate:  Some(2_000_000),
        video_fps_cap:      Some(30),
        audio_codec:        "aac".into(),
        audio_bitrate:      128_000,
        audio_channels:     2,
        audio_sample_rate:  Some(44100),
        hls_segment_secs:   4,
        remux_first:        true,
        sort_order:         1,
        is_builtin:         true,
    }
}

/// Build the preset with `id = "balanced"`.
///
/// H.264 + AAC, 1080p cap, 4 Mbps video. Good balance of quality and
/// compatibility for modern DLNA renderers.
pub fn preset_balanced() -> TranscodePreset {
    TranscodePreset {
        id:                 "balanced".into(),
        label:              "Balanced".into(),
        container:          "mpegts".into(),
        video_codec:        Some("h264".into()),
        video_hw_preferred: true,
        video_max_w:        Some(1920),
        video_max_h:        Some(1080),
        video_max_bitrate:  Some(4_000_000),
        video_fps_cap:      Some(60),
        audio_codec:        "aac".into(),
        audio_bitrate:      192_000,
        audio_channels:     2,
        audio_sample_rate:  Some(48000),
        hls_segment_secs:   4,
        remux_first:        true,
        sort_order:         2,
        is_builtin:         true,
    }
}

/// Build the preset with `id = "high"`.
///
/// H.264 + AAC, 1080p, 8 Mbps — maximum quality within the H.264 envelope.
pub fn preset_high() -> TranscodePreset {
    TranscodePreset {
        id:                 "high".into(),
        label:              "High Quality".into(),
        container:          "mpegts".into(),
        video_codec:        Some("h264".into()),
        video_hw_preferred: true,
        video_max_w:        Some(1920),
        video_max_h:        Some(1080),
        video_max_bitrate:  Some(8_000_000),
        video_fps_cap:      None,
        audio_codec:        "aac".into(),
        audio_bitrate:      256_000,
        audio_channels:     2,
        audio_sample_rate:  Some(48000),
        hls_segment_secs:   4,
        remux_first:        true,
        sort_order:         3,
        is_builtin:         true,
    }
}

/// Build the preset with `id = "audio-only"`.
///
/// Strip video entirely; encode audio to MP3 320 kbps. Useful for music
/// cast to a renderer that does not support video (or for audio-only sources).
pub fn preset_audio_only() -> TranscodePreset {
    TranscodePreset {
        id:                 "audio-only".into(),
        label:              "Audio Only".into(),
        container:          "mpegts".into(),
        video_codec:        None,
        video_hw_preferred: false,
        video_max_w:        None,
        video_max_h:        None,
        video_max_bitrate:  None,
        video_fps_cap:      None,
        audio_codec:        "mp3".into(),
        audio_bitrate:      320_000,
        audio_channels:     2,
        audio_sample_rate:  Some(44100),
        hls_segment_secs:   4,
        remux_first:        false,
        sort_order:         4,
        is_builtin:         true,
    }
}

/// All five built-in presets in sort-order.
pub fn all_builtin_presets() -> [TranscodePreset; 5] {
    [
        preset_passthrough(),
        preset_dlna_compat(),
        preset_balanced(),
        preset_high(),
        preset_audio_only(),
    ]
}

/// Return `true` when the source video codec is HLS/MPEG-TS compatible
/// for copy (no re-encode needed). Only H.264, H.265, and MPEG-2 are safe
/// to remux into MPEG-TS without transcoding.
///
/// `codec_name` should come from `AVCodecParameters.codec_id` resolved via
/// `avcodec_get_name()`, or use the constants below.
pub fn is_mpegts_copy_compatible(codec_id: u32) -> bool {
    use crate::sys::{AV_CODEC_ID_H264, AV_CODEC_ID_AAC, AV_CODEC_ID_MP3};
    const AV_CODEC_ID_HEVC:  u32 = 173;
    const AV_CODEC_ID_MPEG2: u32 = 2;
    const AV_CODEC_ID_AC3:   u32 = 86019;
    const AV_CODEC_ID_EAC3:  u32 = 86056;

    matches!(codec_id, v if v == AV_CODEC_ID_H264
        || v == AV_CODEC_ID_HEVC
        || v == AV_CODEC_ID_MPEG2
        || v == AV_CODEC_ID_AAC
        || v == AV_CODEC_ID_MP3
        || v == AV_CODEC_ID_AC3
        || v == AV_CODEC_ID_EAC3)
}
