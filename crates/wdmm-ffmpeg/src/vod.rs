//! VOD HLS segment planning (§3.1 of `hls-vod-transcode-cache`).
//!
//! The old cast path produced a *live* playlist whose `EXTINF` values were
//! whatever the muxer happened to cut. The VOD path instead synthesizes the
//! playlist up front from a [`SegmentPlan`], so the player can seek without
//! reloading the playlist (`#EXT-X-PLAYLIST-TYPE:VOD` + `ENDLIST`).
//!
//! The alignment strategies model how the segment boundaries relate to the
//! media timeline:
//!
//! - [`PlanAlignment::Exact`] — transcode grid: strict `k * seg_secs`. The job
//!   seeks the input and decodes up to the requested frame, so the output can be
//!   cut on the requested grid regardless of input keyframes (the encoder is
//!   forced to emit a keyframe on every grid point).
//! - [`PlanAlignment::Keyframe`] — copy/hybrid with a usable container keyframe
//!   index: boundaries are the keyframes the HLS muxer will actually cut on, so
//!   the synthesized playlist `EXTINF` values match the segment files exactly
//!   (no warm-up measurement needed). See [`keyframe_plan`].
//! - [`PlanAlignment::Calibrated`] — copy/hybrid without a keyframe index: the
//!   warm-up phase measures real segment durations ([`measure_gop_from_durations`]
//!   infers `gop_ms`) and the plan steps on that multiple of the GOP. The muxer's
//!   `hls_time` is aligned to the same step, so errors no longer accumulate.
//! - [`PlanAlignment::Approx`] — VFR/no-measurement fallback: nominal grid, drift
//!   is reported by the caller instead of being modelled.
//!
//! For copy/hybrid runs the job sets `hls_time` to a tiny value, so the HLS
//! muxer's cut test (`pkt->pts - start_pts >= number * hls_time`) is satisfied by
//! every video keyframe: the muxer cuts on **every keyframe**. That makes the cut
//! sequence independent of the anchor a run was started at, so a restart from any
//! planned segment boundary reproduces the plan exactly. [`keyframe_plan`]
//! therefore plans one segment per keyframe interval; entry starts stay on the
//! media timeline so restarted runs seek to the exact planned keyframe.

//! For encode runs the caller instead keeps `hls_time` tiny while forcing an IDR
//! every `segment_gop_size` frames, so the cuts land on the uniform grid of
//! [`exact_plan`].
//!
//! All logic here is pure and always compiled, so it is unit-testable on a host
//! without libav.
//!
//! [`SegmentPlan`]: crate::vod::SegmentPlan

/// One planned HLS segment: media timeline start and duration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegEntry {
    /// Segment start on the media timeline (milliseconds).
    pub start_ms: i64,
    /// Segment duration (milliseconds). The last segment is truncated to the
    /// media duration.
    pub duration_ms: i64,
}

/// How a [`SegmentPlan`]'s boundaries relate to the media timeline (§3.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlanAlignment {
    /// Transcode grid: strict `k * seg_secs`.
    Exact,

    /// Copy/hybrid with a usable container keyframe index: boundaries are the
    /// real keyframes the muxer cuts on.
    Keyframe,

    /// Copy/hybrid without an index: warm-up measured GOP grid.
    Calibrated,
    /// VFR/no-index fallback: nominal grid, drift reported by the caller.
    Approx,
}

/// The synthesized VOD segment plan for one media item.
#[derive(Clone, Debug, PartialEq)]
pub struct SegmentPlan {
    /// Media duration (milliseconds).
    pub duration_ms: i64,
    /// Nominal segment length requested for this item (seconds).
    pub seg_secs: f64,
    /// Alignment strategy that produced [`entries`](SegmentPlan::entries).
    pub alignment: PlanAlignment,
    /// Measured keyframe interval for [`PlanAlignment::Calibrated`] plans.
    pub gop_ms: Option<i64>,
    /// Planned segments in playback order.
    pub entries: Vec<SegEntry>,
}

impl SegmentPlan {
    /// Number of segments in the plan.
    pub fn segment_count(&self) -> usize {
        self.entries.len()
    }

    /// Start of segment `k`, or `None` when `k` is out of range.
    pub fn start_ms(&self, k: u64) -> Option<i64> {
        self.entries
            .get(usize::try_from(k).ok()?)
            .map(|e| e.start_ms)
    }

    /// Duration of segment `k`, or `None` when `k` is out of range.
    pub fn duration_of(&self, k: u64) -> Option<i64> {
        self.entries
            .get(usize::try_from(k).ok()?)
            .map(|e| e.duration_ms)
    }
}

/// Minimum plausible GOP used when validating measurements (250 ms).
const MIN_GOP_MS: i64 = 250;
/// Maximum plausible GOP used when validating measurements (30 s).
const MAX_GOP_MS: i64 = 30_000;

/// Nominal segment length in milliseconds (at least 1 ms).
fn seg_ms(seg_secs: f64) -> i64 {
    let ms = (seg_secs * 1000.0).round();
    if !ms.is_finite() || ms < 1.0 {
        1
    } else {
        ms as i64
    }
}

/// Build a strict `k * step_ms` grid, truncating the final segment to
/// `duration_ms`.
fn grid_entries(duration_ms: i64, step_ms: i64) -> Vec<SegEntry> {
    if duration_ms <= 0 || step_ms <= 0 {
        return Vec::new();
    }
    let mut entries = Vec::new();
    let mut start = 0_i64;
    while start < duration_ms {
        let duration = step_ms.min(duration_ms - start);
        entries.push(SegEntry {
            start_ms: start,
            duration_ms: duration,
        });
        start += duration;
    }
    entries
}

fn plan_with(
    duration_ms: i64,
    seg_secs: f64,
    alignment: PlanAlignment,
    gop_ms: Option<i64>,
    step_ms: i64,
) -> SegmentPlan {
    SegmentPlan {
        duration_ms: duration_ms.max(0),
        seg_secs,
        alignment,
        gop_ms,
        entries: grid_entries(duration_ms, step_ms),
    }
}

/// Transcode grid: boundaries are exactly `k * seg_secs`, final segment
/// truncated. Never drifts because the transcode job seeks per segment.
pub fn exact_plan(duration_ms: i64, seg_secs: f64) -> SegmentPlan {
    plan_with(
        duration_ms,
        seg_secs,
        PlanAlignment::Exact,
        None,
        seg_ms(seg_secs),
    )
}

/// Nominal grid used for VFR/no-index fallback; the caller reports the drift.
pub fn approximate_plan(duration_ms: i64, seg_secs: f64) -> SegmentPlan {
    plan_with(
        duration_ms,
        seg_secs,
        PlanAlignment::Approx,
        None,
        seg_ms(seg_secs),
    )
}

/// Calibrated grid: step is the smallest multiple of the measured GOP that is
/// at least `seg_secs` (`ceil(seg_secs*1000 / gop_ms) * gop_ms`), final segment
/// truncated. `hls_time` must be aligned to the same step by the caller.
///
/// A non-positive or implausible `gop_ms` degrades to [`approximate_plan`] (the
/// measurement was not trustworthy, so do not claim to be calibrated).
pub fn calibration_plan(duration_ms: i64, seg_secs: f64, gop_ms: i64) -> SegmentPlan {
    if !(MIN_GOP_MS..=MAX_GOP_MS).contains(&gop_ms) {
        return approximate_plan(duration_ms, seg_secs);
    }
    let seg = seg_ms(seg_secs);
    let steps = (seg + gop_ms - 1) / gop_ms;
    let step = steps.saturating_mul(gop_ms).max(gop_ms);
    plan_with(
        duration_ms,
        seg_secs,
        PlanAlignment::Calibrated,
        Some(gop_ms),
        step,
    )
}

/// Minimum number of keyframes needed to plan a keyframe grid: one to start and
/// one to cut on (a two-entry index is still a usable "one segment per GOP" grid
/// for short clips).
pub const MIN_KEYFRAMES_FOR_PLAN: usize = 2;

/// Plan a copy/hybrid HLS run from the container's real keyframe timestamps.
///
/// The caller runs the job with `hls_time` set to a tiny value (`JobOpts::
/// hls_time_override_ms = Some(1)`), which makes the FFmpeg HLS muxer cut on
/// every video keyframe (`hlsenc.c`, `hls_write_packet`). Each segment is then
/// exactly one keyframe interval: segment `k` starts at the `k`-th keyframe and
/// ends at the next one, so restarting a run at any segment boundary produces
/// the same cuts as a run from the origin.
///
/// The first keyframe anchors the plan: the playlist's segment 0 duration is the
/// first keyframe interval, and every entry's `start_ms` is the keyframe's media
/// timeline position, which is also the offset the proxy seeks a restarted run
/// to. The plan keeps the container `duration_ms`; the entries cover
/// `duration_ms - first_keyframe`.
///
/// Returns `None` — the caller then falls back to warm-up calibration — when the
/// index is too short, ends more than two GOPs before the media end (a truncated
/// Matroska `Cues` block, e.g. a live/no-Cues file after one seek), or the media
/// duration is unknown.
///
/// `keyframes` are media-timeline milliseconds; unsorted, duplicate and negative
/// entries are tolerated.
pub fn keyframe_plan(duration_ms: i64, seg_secs: f64, keyframes: &[i64]) -> Option<SegmentPlan> {
    let duration = duration_ms.max(0);
    if duration <= 0 {
        return None;
    }
    let mut kfs: Vec<i64> = keyframes
        .iter()
        .copied()
        .filter(|k| *k >= 0 && *k < duration)
        .collect();
    kfs.sort_unstable();
    kfs.dedup();
    if kfs.len() < MIN_KEYFRAMES_FOR_PLAN {
        return None;
    }
    let gop = median_gap(&kfs)?;
    let last = *kfs.last()?;
    // A truncated index (no-Cues Matroska parsed only up to the seek target)
    // stops well short of the end; a healthy one reaches into the final GOP.
    if last + 2 * gop < duration {
        return None;
    }

    // One segment per keyframe interval; `start_ms` stays on the media timeline
    // so a restarted run seeks to the exact keyframe the plan names.
    let mut entries = Vec::with_capacity(kfs.len());
    for (i, &kf) in kfs.iter().enumerate() {
        let end = kfs.get(i + 1).copied().unwrap_or(duration);
        if end <= kf {
            continue;
        }
        entries.push(SegEntry {
            start_ms: kf,
            duration_ms: end - kf,
        });
    }
    if entries.is_empty() {
        return None;
    }
    Some(SegmentPlan {
        duration_ms: duration,
        seg_secs,
        alignment: PlanAlignment::Keyframe,
        gop_ms: Some(gop),
        entries,
    })
}

/// Median keyframe interval of a sorted, deduplicated keyframe list.
fn median_gap(kfs: &[i64]) -> Option<i64> {
    let mut gaps: Vec<i64> = kfs
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|gap| *gap > 0)
        .collect();
    if gaps.is_empty() {
        return None;
    }
    gaps.sort_unstable();
    gaps.get(gaps.len() / 2).copied()
}

/// Infer one GOP unit from measured segment durations.
///
/// The muxer only cuts copy/encode streams on keyframes, so every real segment
/// duration is an integer multiple of the GOP. The greatest common divisor of
/// the measured durations is therefore a valid grid unit (usually the GOP
/// itself, sometimes a multiple when the sample is especially stable).
///
/// Returns `None` when the sample is too short, non-positive, or the inferred
/// unit is implausible (unstable/VFR output) — the caller then uses
/// [`approximate_plan`] instead of pretending to be calibrated.
pub fn measure_gop_from_durations(durations_ms: &[i64]) -> Option<i64> {
    // Two segments is the minimum that can reveal an interval: one duration
    // alone says nothing about the GOP.
    if durations_ms.len() < 2 {
        return None;
    }
    let mut g = 0_i64;
    for d in durations_ms {
        if *d <= 0 {
            return None;
        }
        g = gcd(g, *d);
    }
    if !(MIN_GOP_MS..=MAX_GOP_MS).contains(&g) {
        return None;
    }
    Some(g)
}

fn gcd(a: i64, b: i64) -> i64 {
    let (mut a, mut b) = (a.abs(), b.abs());
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn starts(plan: &SegmentPlan) -> Vec<i64> {
        plan.entries.iter().map(|e| e.start_ms).collect()
    }

    fn durations(plan: &SegmentPlan) -> Vec<i64> {
        plan.entries.iter().map(|e| e.duration_ms).collect()
    }

    #[test]
    fn exact_plan_is_a_strict_grid() {
        let plan = exact_plan(10_000, 4.0);
        assert_eq!(plan.alignment, PlanAlignment::Exact);
        assert_eq!(plan.segment_count(), 3);
        assert_eq!(starts(&plan), vec![0, 4_000, 8_000]);
        assert_eq!(durations(&plan), vec![4_000, 4_000, 2_000]);
        assert_eq!(plan.start_ms(2), Some(8_000));
        assert_eq!(plan.duration_of(2), Some(2_000));
        assert_eq!(plan.start_ms(3), None);
        assert_eq!(plan.duration_of(3), None);
    }

    #[test]
    fn exact_plan_handles_zero_and_short_media() {
        let empty = exact_plan(0, 4.0);
        assert_eq!(empty.segment_count(), 0);
        assert_eq!(empty.start_ms(0), None);

        let short = exact_plan(1_500, 4.0);
        assert_eq!(short.segment_count(), 1);
        assert_eq!(short.entries[0].duration_ms, 1_500);

        // Fractional segment lengths round to milliseconds.
        let fractional = exact_plan(1_000, 0.333);
        assert_eq!(fractional.entries[0].duration_ms, 333);
        assert_eq!(fractional.entries.last().unwrap().duration_ms, 1);
    }

    #[test]
    fn exact_plan_truncates_non_multiple_duration() {
        let plan = exact_plan(9_500, 4.0);
        assert_eq!(
            durations(&plan),
            vec![4_000, 4_000, 1_500],
            "final segment must be truncated, not padded"
        );
        let total: i64 = durations(&plan).iter().sum();
        assert_eq!(total, 9_500, "segments must tile the media exactly");
    }

    #[test]
    fn approximate_plan_uses_nominal_grid() {
        let plan = approximate_plan(10_000, 4.0);
        assert_eq!(plan.alignment, PlanAlignment::Approx);
        assert_eq!(plan.gop_ms, None);
        assert_eq!(starts(&plan), vec![0, 4_000, 8_000]);
        assert_eq!(durations(&plan), vec![4_000, 4_000, 2_000]);
    }

    #[test]
    fn calibration_plan_rounds_step_up_to_gop_multiple() {
        // 4 s requested with a 3 s GOP → 6 s step (2 GOPs), not 3 s.
        let plan = calibration_plan(20_000, 4.0, 3_000);
        assert_eq!(plan.alignment, PlanAlignment::Calibrated);
        assert_eq!(plan.gop_ms, Some(3_000));
        assert_eq!(starts(&plan), vec![0, 6_000, 12_000, 18_000]);
        assert_eq!(durations(&plan), vec![6_000, 6_000, 6_000, 2_000]);

        // GOP that divides the requested length keeps it unchanged.
        let plan = calibration_plan(10_000, 4.0, 2_000);
        assert_eq!(starts(&plan), vec![0, 4_000, 8_000]);
        assert_eq!(durations(&plan), vec![4_000, 4_000, 2_000]);

        // Requested 4 s, GOP 10 s → one GOP per segment (10 s).
        let plan = calibration_plan(25_000, 4.0, 10_000);
        assert_eq!(starts(&plan), vec![0, 10_000, 20_000]);
        assert_eq!(durations(&plan), vec![10_000, 10_000, 5_000]);
    }

    #[test]
    fn calibration_plan_rejects_implausible_gop() {
        for gop in [0, -1, 1, 100, MAX_GOP_MS + 1] {
            let plan = calibration_plan(10_000, 4.0, gop);
            assert_eq!(
                plan.alignment,
                PlanAlignment::Approx,
                "gop={gop} must not claim to be calibrated"
            );
            assert_eq!(plan.gop_ms, None);
        }
    }

    #[test]
    fn measure_gop_uses_the_common_divisor() {
        assert_eq!(measure_gop_from_durations(&[4_000, 4_000]), Some(4_000));
        assert_eq!(
            measure_gop_from_durations(&[4_000, 6_000, 4_000]),
            Some(2_000)
        );
        assert_eq!(measure_gop_from_durations(&[6_000, 9_000]), Some(3_000));

        // Too short / unstable / non-positive samples are rejected.
        assert_eq!(measure_gop_from_durations(&[]), None);
        assert_eq!(measure_gop_from_durations(&[4_000]), None);
        assert_eq!(measure_gop_from_durations(&[4_000, 4_100]), None); // g=100 → implausible
        assert_eq!(measure_gop_from_durations(&[4_000, -4_000]), None);
        assert_eq!(measure_gop_from_durations(&[0, 4_000]), None);
    }

    #[test]
    fn measured_gop_feeds_calibration_handoff() {
        let gop = measure_gop_from_durations(&[4_000, 6_000, 4_000]).unwrap();
        let plan = calibration_plan(14_000, 4.0, gop);
        assert_eq!(plan.alignment, PlanAlignment::Calibrated);
        assert_eq!(plan.gop_ms, Some(2_000));
        assert_eq!(durations(&plan), vec![4_000, 4_000, 4_000, 2_000]);
        let total: i64 = durations(&plan).iter().sum();
        assert_eq!(total, 14_000);
    }

    #[test]
    fn plans_tile_the_media_without_gaps_or_overlaps() {
        for (duration, seg) in [(0_i64, 4.0_f64), (1, 4.0), (9_500, 4.0), (60_000, 6.0)] {
            for plan in [
                exact_plan(duration, seg),
                approximate_plan(duration, seg),
                calibration_plan(duration, seg, 2_000),
            ] {
                let mut cursor = 0;
                for entry in &plan.entries {
                    assert_eq!(entry.start_ms, cursor, "no gap/overlap before {entry:?}");
                    assert!(entry.duration_ms > 0);
                    cursor += entry.duration_ms;
                }
                assert_eq!(cursor, duration, "plan must cover the full duration");
            }
        }
    }

    /// Keyframes of the 110.293 s test clip `E:\test\video\1.m4v`, measured with
    /// `ffprobe -show_entries packet=pts_time,flags` (27 keyframes, variable GOP).
    const REAL_KEYFRAMES_MS: [i64; 27] = [
        0, 1_012, 5_277, 11_396, 20_482, 22_274, 24_437, 26_477, 28_640, 30_186, 31_545, 40_631,
        45_700, 53_364, 58_309, 59_854, 63_439, 64_922, 68_631, 76_048, 81_611, 88_595, 90_882,
        92_736, 97_743, 103_923, 105_035,
    ];

    #[test]
    fn keyframe_plan_makes_one_segment_per_keyframe_interval() {
        // The muxer cuts on every keyframe when `hls_time` is tiny, so each
        // segment is exactly one keyframe interval regardless of `seg_secs`.
        let kfs: Vec<i64> = (0..=9).map(|i| i * 2_000).collect();
        let plan = keyframe_plan(20_000, 4.0, &kfs).unwrap();
        assert_eq!(plan.alignment, PlanAlignment::Keyframe);
        assert_eq!(plan.gop_ms, Some(2_000));
        assert_eq!(
            starts(&plan),
            vec![
                0, 2_000, 4_000, 6_000, 8_000, 10_000, 12_000, 14_000, 16_000, 18_000
            ]
        );
        assert_eq!(durations(&plan), vec![2_000; 10]);
    }

    #[test]
    fn keyframe_plan_ignores_the_requested_segment_length() {
        let kfs = [0, 1_000, 6_000, 7_000, 12_000];
        let short = keyframe_plan(13_000, 2.0, &kfs).unwrap();
        let long = keyframe_plan(13_000, 8.0, &kfs).unwrap();
        assert_eq!(starts(&short), starts(&long));
        assert_eq!(durations(&short), vec![1_000, 5_000, 1_000, 5_000, 1_000]);
    }

    #[test]
    fn keyframe_plan_accepts_two_keyframes() {
        let plan = keyframe_plan(9_000, 4.0, &[0, 5_000]).unwrap();
        assert_eq!(plan.alignment, PlanAlignment::Keyframe);
        assert_eq!(starts(&plan), vec![0, 5_000]);
        assert_eq!(durations(&plan), vec![5_000, 4_000]);
    }

    #[test]
    fn keyframe_plan_matches_a_real_variable_gop_source() {
        let plan = keyframe_plan(110_293, 4.0, &REAL_KEYFRAMES_MS).unwrap();
        assert_eq!(plan.alignment, PlanAlignment::Keyframe);
        assert_eq!(plan.gop_ms, Some(3_709), "median keyframe interval");
        let expected: Vec<i64> = REAL_KEYFRAMES_MS.to_vec();
        assert_eq!(starts(&plan), expected);
        let mut expected_durations: Vec<i64> = REAL_KEYFRAMES_MS
            .windows(2)
            .map(|w| w[1] - w[0])
            .collect();
        expected_durations.push(110_293 - REAL_KEYFRAMES_MS[REAL_KEYFRAMES_MS.len() - 1]);
        assert_eq!(durations(&plan), expected_durations);
        // Boundaries strictly increase and tile [start, duration].
        for pair in plan.entries.windows(2) {
            assert!(
                pair[1].start_ms > pair[0].start_ms,
                "boundaries must increase: {pair:?}"
            );
            assert_eq!(
                pair[0].start_ms + pair[0].duration_ms,
                pair[1].start_ms,
                "segments must be contiguous"
            );
        }
        let total: i64 = plan.entries.iter().map(|e| e.duration_ms).sum();
        assert_eq!(total, plan.duration_ms - plan.entries[0].start_ms);
    }

    #[test]
    fn keyframe_plan_keeps_keyframes_on_the_media_timeline() {
        // Matroska starts its media 21 ms after the first keyframe; entry starts
        // must stay absolute so a restarted run seeks to the real keyframe.
        let kfs = [21, 5_298, 11_417, 20_503];
        let plan = keyframe_plan(30_000, 4.0, &kfs).unwrap();
        assert_eq!(starts(&plan), vec![21, 5_298, 11_417, 20_503]);
        assert_eq!(durations(&plan), vec![5_277, 6_119, 9_086, 9_497]);
        let total: i64 = durations(&plan).iter().sum();
        assert_eq!(total, 30_000 - 21);
    }

    #[test]
    fn keyframe_plan_keeps_short_gops_as_short_segments() {
        // Keyframes every 100 ms: below the requested 4 s, but each keyframe is a
        // real cut, so the plan must keep them (reported to the caller, not hidden).
        let kfs: Vec<i64> = (0..=20).map(|i| i * 100).collect();
        let plan = keyframe_plan(2_100, 4.0, &kfs).unwrap();
        assert_eq!(plan.segment_count(), 21);
        assert!(plan.entries.iter().all(|e| e.duration_ms == 100));
        assert_eq!(plan.gop_ms, Some(100));
    }

    #[test]
    fn keyframe_plan_rejects_a_truncated_index() {
        // No-Cues Matroska: one seek parsed the index only up to the seek target,
        // so the last keyframe is two GOPs (or more) before the end.
        let truncated: Vec<i64> = (0..=10).map(|i| i * 1_000).collect();
        assert_eq!(
            keyframe_plan(110_293, 4.0, &truncated),
            None,
            "an index that stops 100 s early must not plan a playlist"
        );
    }

    #[test]
    fn keyframe_plan_rejects_unusable_indexes() {
        assert_eq!(keyframe_plan(10_000, 4.0, &[]), None);
        assert_eq!(keyframe_plan(10_000, 4.0, &[0]), None);
        assert_eq!(keyframe_plan(0, 4.0, &REAL_KEYFRAMES_MS), None);
        // Keyframes past the media end are discarded, leaving too few.
        assert_eq!(keyframe_plan(1_000, 4.0, &[1_500, 2_000, 3_000]), None);
    }

    #[test]
    fn keyframe_plan_sanitizes_unsorted_duplicate_and_negative_pts() {
        let plan = keyframe_plan(
            16_000,
            4.0,
            &[0, -500, 8_000, 4_000, 4_000, 12_000, 8_000, 16_000],
        )
        .unwrap();
        assert_eq!(starts(&plan), vec![0, 4_000, 8_000, 12_000]);
        assert_eq!(durations(&plan), vec![4_000, 4_000, 4_000, 4_000]);
        assert_eq!(plan.gop_ms, Some(4_000));
    }
}

