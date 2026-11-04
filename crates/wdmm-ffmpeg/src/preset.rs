//! Preset constants and helpers for `wdmm-ffmpeg` (B3.5).
//!
//! The two built-in `TranscodePreset` values mirror the seeds written to the
//! `transcode_presets` DB table in `wdmm-storage` schema v16. The DB is the
//! primary source of truth; these constants are used when the DB rows are not
//! available (unit tests, probe-only contexts) and by the
//! `wdmm_transcode_preset_reset` FFI call to restore the factory values.
//!
//! `is_ts_legal_video` / `is_ts_legal_audio` are the **single** MPEG-TS
//! legality check shared by the `on` and `strict` remux modes (contract
//! `remux-modes.md` §2) — there is deliberately no second copy-compatibility
//! list.
//!
//! Built-ins stay fully editable; only deletion is refused (see
//! `delete_transcode_preset`).

use wdmm_core::models::TranscodePreset;

/// Build the `h264` factory preset: copy-first, MediaCodec H.264 with an
/// `libx264` software fallback, 1080p60 / 4 Mbps envelope, AAC 128 kbps.
///
/// Value table: contract `preset-rate-control-and-codec-probe.md` §13.2.
pub fn preset_h264() -> TranscodePreset {
    TranscodePreset {
        id: "h264".into(),
        label: "H.264（兼容优先）".into(),
        video_remux: "on".into(),
        audio_remux: "on".into(),
        hw_video_encoder: "h264_mediacodec".into(),
        sw_video_encoder: "libx264".into(),
        sw_video_preset: "veryfast".into(),
        video_max_w: Some(1920),
        video_max_h: Some(1080),
        video_max_bitrate: Some(4_000_000),
        video_fps_cap: Some(60),
        video_rate_control: "vbr".into(),
        video_quality: None,
        audio_codec: "aac".into(),
        audio_bitrate: 128_000,
        audio_channels: 2,
        audio_sample_rate: Some(44100),
        hls_segment_secs: 4,
        sort_order: 0,
        is_builtin: true,
        updated_at: 0,
    }
}

/// Build the `hevc` factory preset: copy-first, MediaCodec HEVC with an
/// `libx264` software fallback (software HEVC is not shipped, so the fallback
/// downgrades to H.264), 1080p60 / 3 Mbps envelope, AAC 128 kbps.
///
/// Value table: contract `preset-rate-control-and-codec-probe.md` §13.2.
pub fn preset_hevc() -> TranscodePreset {
    TranscodePreset {
        id: "hevc".into(),
        label: "H.265（更高压缩）".into(),
        video_remux: "on".into(),
        audio_remux: "on".into(),
        hw_video_encoder: "hevc_mediacodec".into(),
        sw_video_encoder: "libx264".into(),
        sw_video_preset: "veryfast".into(),
        video_max_w: Some(1920),
        video_max_h: Some(1080),
        video_max_bitrate: Some(3_000_000),
        video_fps_cap: Some(60),
        video_rate_control: "vbr".into(),
        video_quality: None,
        audio_codec: "aac".into(),
        audio_bitrate: 128_000,
        audio_channels: 2,
        audio_sample_rate: Some(44100),
        hls_segment_secs: 4,
        sort_order: 1,
        is_builtin: true,
        updated_at: 0,
    }
}

/// All built-in presets in sort-order (`h264`, `hevc`).
pub fn all_builtin_presets() -> [TranscodePreset; 2] {
    [preset_h264(), preset_hevc()]
}

/// Return the factory preset with `id`, if `id` is a built-in.
pub fn builtin_preset(id: &str) -> Option<TranscodePreset> {
    all_builtin_presets().into_iter().find(|p| p.id == id)
}

/// Numeric `AVCodecID` constants used by the preset/remux logic.
///
/// `AVCodecID` values shift across libav major versions (FFmpeg 9.0 renumbered
/// HEVC 173→172, VP9 167→166, AV1 226→222), and the hand-written bindings in
/// [`crate::sys`] only expose the ones the transcode path itself needs. Per the
/// preset-encoder-tiers contract, **every new numeric codec id lives here** with
/// a comment, instead of being scattered across the ffmpeg crate. All values
/// below were checked against FFmpeg 9.0.2 `libavcodec/codec_id.h`;
/// **re-verify on every libav major bump**.
pub mod codec_ids {
    /// `AV_CODEC_ID_MPEG1VIDEO`.
    pub const MPEG1VIDEO: u32 = 1;
    /// `AV_CODEC_ID_H264`.
    pub const H264: u32 = 27;
    /// `AV_CODEC_ID_HEVC` (a.k.a. H.265; renumbered from 173 in FFmpeg 9.0).
    pub const HEVC: u32 = 172;
    /// `AV_CODEC_ID_MPEG2VIDEO`.
    pub const MPEG2VIDEO: u32 = 2;
    /// `AV_CODEC_ID_MPEG4` (MPEG-4 Part 2).
    pub const MPEG4: u32 = 12;
    /// `AV_CODEC_ID_AAC`.
    pub const AAC: u32 = 86018;
    /// `AV_CODEC_ID_MP2` (MPEG audio layer 2).
    pub const MP2: u32 = 86016;
    /// `AV_CODEC_ID_MP3`.
    pub const MP3: u32 = 86017;
    /// `AV_CODEC_ID_AC3`.
    pub const AC3: u32 = 86019;
    /// `AV_CODEC_ID_EAC3`.
    pub const EAC3: u32 = 86056;
}

/// Numeric codec id produced by a video encoder name (contract §2: the output
/// codec follows the encoder that actually ran, never the requested one).
///
/// Returns `None` for an empty/unknown encoder name.
pub fn video_encoder_output_codec_id(encoder: &str) -> Option<u32> {
    use codec_ids::*;
    match encoder {
        "h264_mediacodec" | "libx264" => Some(H264),
        "hevc_mediacodec" => Some(HEVC),
        "mpeg4_mediacodec" | "mpeg4" => Some(MPEG4),
        "mpeg1video" => Some(MPEG1VIDEO),
        "mpeg2video" => Some(MPEG2VIDEO),
        _ => None,
    }
}

/// `avcodec_find_encoder_by_name` name for a preset `audio_codec` value.
///
/// Unknown values fall back to AAC — the universal MPEG-TS-compatible codec.
pub fn audio_encoder_name(codec: &str) -> &'static str {
    match codec {
        "ac3" => "ac3",
        "eac3" => "eac3",
        "mp2" => "mp2",
        _ => "aac",
    }
}

/// Numeric codec id of the encoder selected by a preset `audio_codec` value.
pub fn audio_encoder_codec_id(codec: &str) -> u32 {
    use codec_ids::*;
    match codec {
        "ac3" => AC3,
        "eac3" => EAC3,
        "mp2" => MP2,
        _ => AAC,
    }
}

/// Return `true` when a **video** codec is legal in MPEG-TS *and* a realistic
/// renderer can decode it.
///
/// This is the single legality check behind the `on` and `strict` remux modes
/// (contract `remux-modes.md` §2). The set follows
/// `libavformat/mpegtsenc.c::get_dvb_stream_type` but is narrowed: VVC, CAVS,
/// AVS2/AVS3, Dirac and VC-1 are deliberately **excluded**. MPEG-TS can wrap
/// them, but we never encode to those codecs and no realistic renderer decodes
/// them, so remuxing would produce a stream that muxes fine yet cannot play.
pub fn is_ts_legal_video(codec_id: u32) -> bool {
    matches!(
        codec_id,
        v if v == codec_ids::MPEG1VIDEO
            || v == codec_ids::MPEG2VIDEO
            || v == codec_ids::MPEG4
            || v == codec_ids::H264
            || v == codec_ids::HEVC
    )
}

/// Return `true` when an **audio** codec is legal in MPEG-TS *and* a realistic
/// renderer can decode it (MP2, MP3, AAC, AC-3, E-AC-3).
///
/// Same single source of truth as [`is_ts_legal_video`]. DTS, TrueHD and Opus
/// are deliberately **excluded**: MPEG-TS can carry them, but no DMR-class
/// renderer decodes them, so remuxing them would only produce unplayable
/// output.
pub fn is_ts_legal_audio(codec_id: u32) -> bool {
    matches!(
        codec_id,
        v if v == codec_ids::MP2
            || v == codec_ids::MP3
            || v == codec_ids::AAC
            || v == codec_ids::AC3
            || v == codec_ids::EAC3
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preset::codec_ids;

    // Codecs MPEG-TS *can* carry but which we deliberately never remux to: we
    // never encode to them and no realistic renderer decodes them. Values
    // checked against FFmpeg 9.0.2 `libavcodec/codec_id.h`; re-verify on every
    // libav bump.
    const AV_CODEC_ID_VVC: u32 = 195;
    const AV_CODEC_ID_CAVS: u32 = 87;
    const AV_CODEC_ID_AVS2: u32 = 191;
    const AV_CODEC_ID_AVS3: u32 = 193;
    const AV_CODEC_ID_DIRAC: u32 = 116;
    const AV_CODEC_ID_VC1: u32 = 70;
    const AV_CODEC_ID_VP9: u32 = 166;
    const AV_CODEC_ID_AV1: u32 = 222;
    const AV_CODEC_ID_OPUS: u32 = 86076;
    const AV_CODEC_ID_FLAC: u32 = 86028;

    #[test]
    fn ts_legal_video_matrix() {
        for id in [
            codec_ids::MPEG1VIDEO,
            codec_ids::MPEG2VIDEO,
            codec_ids::MPEG4,
            codec_ids::H264,
            codec_ids::HEVC,
        ] {
            assert!(is_ts_legal_video(id), "codec {id} must be remuxable");
        }
        // Muxable but intentionally excluded, plus an audio id and 0.
        for id in [
            AV_CODEC_ID_VP9,
            AV_CODEC_ID_AV1,
            AV_CODEC_ID_VVC,
            AV_CODEC_ID_CAVS,
            AV_CODEC_ID_AVS2,
            AV_CODEC_ID_AVS3,
            AV_CODEC_ID_DIRAC,
            AV_CODEC_ID_VC1,
            codec_ids::AAC,
            0,
        ] {
            assert!(!is_ts_legal_video(id), "codec {id} must be re-encoded");
        }
    }

    #[test]
    fn ts_legal_audio_matrix() {
        for id in [
            codec_ids::MP2,
            codec_ids::MP3,
            codec_ids::AAC,
            codec_ids::AC3,
            codec_ids::EAC3,
        ] {
            assert!(is_ts_legal_audio(id), "codec {id} must be remuxable");
        }
        // Muxable but intentionally excluded, plus a video id and 0.
        for id in [AV_CODEC_ID_OPUS, AV_CODEC_ID_FLAC, codec_ids::H264, 0] {
            assert!(!is_ts_legal_audio(id), "codec {id} must be re-encoded");
        }
    }

    #[test]
    fn two_builtins_are_editable_and_valid() {
        let presets = all_builtin_presets();
        assert_eq!(presets.len(), 2);
        assert_eq!(presets[0].id, "h264");
        assert_eq!(presets[1].id, "hevc");
        for preset in &presets {
            assert!(preset.is_builtin);
            // Both remux modes on = the old copy-first behaviour.
            assert_eq!(preset.video_remux, "on");
            assert_eq!(preset.audio_remux, "on");
            assert!(preset.has_usable_video_path());
            // v14 rate control: both built-ins ship plain VBR (no CQ value).
            assert_eq!(preset.video_rate_control, "vbr");
            assert_eq!(preset.video_quality, None);
            // Built-ins must always pass the same validation the storage layer
            // applies to user saves.
            preset.validate().expect("factory preset must be valid");
        }
        assert!(builtin_preset("h264").is_some());
        assert!(builtin_preset("hevc").is_some());
        assert!(builtin_preset("balanced").is_none());
        assert!(builtin_preset("passthrough").is_none());
    }

    #[test]
    fn factory_values_match_the_contract_table() {
        let h264 = preset_h264();
        assert_eq!(h264.label, "H.264（兼容优先）");
        assert_eq!(h264.video_remux, "on");
        assert_eq!(h264.audio_remux, "on");
        assert_eq!(h264.hw_video_encoder, "h264_mediacodec");
        assert_eq!(h264.sw_video_encoder, "libx264");
        assert_eq!(h264.sw_video_preset, "veryfast");
        assert_eq!(h264.video_rate_control, "vbr");
        assert_eq!(h264.video_quality, None);
        assert_eq!(
            (h264.video_max_w, h264.video_max_h),
            (Some(1920), Some(1080))
        );
        assert_eq!(h264.video_max_bitrate, Some(4_000_000));
        assert_eq!(h264.video_fps_cap, Some(60));
        assert_eq!(
            (h264.audio_codec.as_str(), h264.audio_bitrate),
            ("aac", 128_000)
        );
        assert_eq!(
            (h264.audio_channels, h264.audio_sample_rate),
            (2, Some(44100))
        );
        assert_eq!(h264.hls_segment_secs, 4);
        assert_eq!(h264.sort_order, 0);
        assert!(h264.is_builtin);

        let hevc = preset_hevc();
        assert_eq!(hevc.label, "H.265（更高压缩）");
        assert_eq!(hevc.video_remux, "on");
        assert_eq!(hevc.audio_remux, "on");
        assert_eq!(hevc.hw_video_encoder, "hevc_mediacodec");
        assert_eq!(hevc.sw_video_encoder, "libx264");
        assert_eq!(hevc.sw_video_preset, "veryfast");
        assert_eq!(hevc.video_rate_control, "vbr");
        assert_eq!(hevc.video_max_bitrate, Some(3_000_000));
        assert_eq!(hevc.sort_order, 1);
        assert!(hevc.is_builtin);
    }

    #[test]
    fn video_encoder_names_map_to_their_output_codec() {
        assert_eq!(
            video_encoder_output_codec_id("h264_mediacodec"),
            Some(codec_ids::H264)
        );
        assert_eq!(
            video_encoder_output_codec_id("libx264"),
            Some(codec_ids::H264)
        );
        assert_eq!(
            video_encoder_output_codec_id("hevc_mediacodec"),
            Some(codec_ids::HEVC)
        );
        assert_eq!(
            video_encoder_output_codec_id("mpeg4_mediacodec"),
            Some(codec_ids::MPEG4)
        );
        assert_eq!(
            video_encoder_output_codec_id("mpeg4"),
            Some(codec_ids::MPEG4)
        );
        assert_eq!(
            video_encoder_output_codec_id("mpeg1video"),
            Some(codec_ids::MPEG1VIDEO)
        );
        assert_eq!(
            video_encoder_output_codec_id("mpeg2video"),
            Some(codec_ids::MPEG2VIDEO)
        );
        // Disabled / unknown encoders have no output codec.
        assert_eq!(video_encoder_output_codec_id(""), None);
        assert_eq!(video_encoder_output_codec_id("libx265"), None);
    }

    #[test]
    fn audio_codec_names_map_to_encoders() {
        for (codec, name, id) in [
            ("aac", "aac", codec_ids::AAC),
            ("ac3", "ac3", codec_ids::AC3),
            ("eac3", "eac3", codec_ids::EAC3),
            ("mp2", "mp2", codec_ids::MP2),
        ] {
            assert_eq!(audio_encoder_name(codec), name);
            assert_eq!(audio_encoder_codec_id(codec), id);
        }
        // Unknown audio codecs fall back to AAC (the only universal MPEG-TS
        // audio codec the preset layer allows).
        assert_eq!(audio_encoder_name("mp3"), "aac");
        assert_eq!(audio_encoder_name("copy"), "aac");
        assert_eq!(audio_encoder_codec_id("flac"), codec_ids::AAC);
    }
}
