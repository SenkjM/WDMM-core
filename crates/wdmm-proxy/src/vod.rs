//! Synthesized VOD playlist (contract §3.3).
//!
//! The transcoder no longer owns the playlist: segments are requested by the
//! renderer and produced on demand, so the playlist has to be rendered from the
//! segment plan. Rendering is pure text work — no libav, no disk, no async — so
//! it is unit-testable on any host.

/// Content type served for `/h/<token>/index.m3u8`.
pub const PLAYLIST_CONTENT_TYPE: &str = "application/vnd.apple.mpegurl";
/// Content type served for `/h/<token>/seg<k>.ts`.
pub const SEGMENT_CONTENT_TYPE: &str = "video/mp2t";

/// One playlist line: segment `k` starts at `start_ms` and lasts `duration_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaylistSegment {
    pub start_ms: i64,
    pub duration_ms: i64,
}

impl PlaylistSegment {
    pub fn new(start_ms: i64, duration_ms: i64) -> Self {
        Self {
            start_ms,
            duration_ms,
        }
    }
}

/// `#EXT-X-TARGETDURATION`: rounded up whole seconds, never zero.
///
/// The grid is predicted from the container index, but a segment written by the
/// muxer can outrun its planned duration by a frame or two (frame cadence,
/// encoder IDR placement). HLS requires every `EXTINF` to be at or below
/// `TARGETDURATION`, so a plan sitting just under a whole second gets one second
/// of slack instead of being rounded to a value the real segment can exceed.
pub fn target_duration_secs(segments: &[PlaylistSegment]) -> u64 {
    let max_ms = segments
        .iter()
        .map(|segment| segment.duration_ms.max(1) as u64)
        .max()
        .unwrap_or(1)
        .max(1);
    let secs = max_ms.div_ceil(1000);
    if max_ms % 1000 >= 900 {
        secs + 1
    } else {
        secs
    }
}

/// Render a complete, finite VOD playlist.
///
/// Segment URIs are relative (`seg<k>.ts`) so the same body works through the
/// proxy's `/h/<token>/` prefix and through any signed base URL.
pub fn render_playlist(segments: &[PlaylistSegment]) -> String {
    let mut out = String::with_capacity(64 + segments.len() * 32);
    out.push_str("#EXTM3U\n");
    out.push_str("#EXT-X-VERSION:3\n");
    out.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");
    out.push_str(&format!(
        "#EXT-X-TARGETDURATION:{}\n",
        target_duration_secs(segments)
    ));
    out.push_str("#EXT-X-MEDIA-SEQUENCE:0\n");
    for (k, segment) in segments.iter().enumerate() {
        out.push_str(&format!(
            "#EXTINF:{:.3},\nseg{k}.ts\n",
            segment.duration_ms as f64 / 1000.0
        ));
    }
    out.push_str("#EXT-X-ENDLIST\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> Vec<PlaylistSegment> {
        vec![
            PlaylistSegment::new(0, 6_000),
            PlaylistSegment::new(6_000, 6_000),
            PlaylistSegment::new(12_000, 4_500),
        ]
    }

    #[test]
    fn renders_a_finite_vod_playlist() {
        let text = render_playlist(&plan());
        assert!(text.starts_with("#EXTM3U\n"));
        assert!(text.contains("#EXT-X-VERSION:3\n"));
        assert!(text.contains("#EXT-X-PLAYLIST-TYPE:VOD\n"));
        assert!(text.contains("#EXT-X-TARGETDURATION:6\n"));
        assert!(text.contains("#EXT-X-MEDIA-SEQUENCE:0\n"));
        assert!(text.contains("#EXTINF:6.000,\nseg0.ts\n"));
        assert!(text.contains("#EXTINF:6.000,\nseg1.ts\n"));
        assert!(text.contains("#EXTINF:4.500,\nseg2.ts\n"));
        assert!(text.trim_end().ends_with("#EXT-X-ENDLIST"));
        // No live-style tags: nothing may tell the renderer to refresh.
        assert!(!text.contains("#EXT-X-ENDLIST\n#EXT-X-ENDLIST"));
        assert_eq!(text.matches("#EXTINF:").count(), 3);
    }

    #[test]
    fn target_duration_rounds_up_and_never_zero() {
        assert_eq!(target_duration_secs(&[PlaylistSegment::new(0, 1)]), 1);
        assert_eq!(target_duration_secs(&[PlaylistSegment::new(0, 1_400)]), 2);
        assert_eq!(target_duration_secs(&plan()), 6);
        assert_eq!(target_duration_secs(&[]), 1);
    }

    #[test]
    fn target_duration_leaves_slack_near_a_second_boundary() {
        // 1979 ms rounds up to 2 s, but the muxer can write 2001 ms for that
        // segment when the encoder places IDRs on its own cadence.
        assert_eq!(target_duration_secs(&[PlaylistSegment::new(0, 1_979)]), 3);
        assert_eq!(target_duration_secs(&[PlaylistSegment::new(0, 9_086)]), 10);
        assert_eq!(target_duration_secs(&[PlaylistSegment::new(0, 1_800)]), 2);
    }

    #[test]
    fn empty_plan_still_renders_a_valid_finite_playlist() {
        let text = render_playlist(&[]);
        assert!(text.starts_with("#EXTM3U\n"));
        assert!(text.contains("#EXT-X-TARGETDURATION:1\n"));
        assert!(text.trim_end().ends_with("#EXT-X-ENDLIST"));
        assert!(!text.contains("#EXTINF:"));
    }
}
