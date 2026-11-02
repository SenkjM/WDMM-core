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
pub fn remux_plan(video_codec_id: Option<u32>, audio_codec_id: Option<u32>) -> RemuxPlan {
    let Some(video_id) = video_codec_id else {
        // No usable video stream: remuxing an audio-only HLS cast is not a
        // supported shape here, so let the full transcode path handle it.
        return RemuxPlan::Transcode;
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
        assert_eq!(remux_plan(None, Some(AAC)), RemuxPlan::Transcode);
        // No audio is fine: video-only remux.
        assert_eq!(remux_plan(Some(H264), None), RemuxPlan::Remux);
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
}
