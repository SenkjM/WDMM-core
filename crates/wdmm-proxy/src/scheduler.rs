//! Pull-driven transcode scheduling (contract §3.4 + §3.5 "按需运行").
//!
//! The renderer requests one segment at a time. This module decides, purely
//! from the plan shape and the state of the current run, whether that request:
//!
//! * is already satisfied (`Serve`),
//! * will be satisfied by the running job shortly (`Wait`),
//! * needs a (re)started job (`Start { from_k }`), or
//! * is past the end of the media (`Ended`).
//!
//! It also decides when the running job has enough runway and can be parked
//! (`should_stop`). No libav, no I/O, no async: every rule is unit-tested here.

/// Segments of runway the job keeps ahead of the newest request (≈60 s).
pub const LOOKAHEAD_SECS: f64 = 60.0;
/// Requests this close behind the ready front resume the parked job instead of
/// jumping: sequential playback then just continues (`resume_from`).
pub const RESUME_MARGIN_SEGMENTS: u64 = 3;

/// State of the current transcode run for one HLS session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunState {
    /// First segment index the job was started at.
    pub first_k: u64,
    /// Highest segment index known to be complete on disk.
    pub ready_k: u64,
    /// The job reached the end of the media.
    pub ended: bool,
}

impl RunState {
    pub fn new(first_k: u64) -> Self {
        Self {
            first_k,
            ready_k: first_k,
            ended: false,
        }
    }
}

/// What the proxy should do for a segment request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// The segment is expected on disk (the caller checks the file first).
    Serve,
    /// A running job is on its way; poll the file for a short while.
    Wait,
    /// (Re)start a transcode job beginning at `from_k`.
    Start { from_k: u64 },
    /// Past the end of the media.
    Ended,
}

/// Plan-derived scheduling policy for one session.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SchedulePolicy {
    /// Number of segments in the plan.
    pub plan_len: u64,
    /// Nominal segment duration.
    pub seg_secs: f64,
}

impl SchedulePolicy {
    pub fn new(plan_len: u64, seg_secs: f64) -> Self {
        Self {
            plan_len,
            seg_secs: if seg_secs > 0.0 { seg_secs } else { 6.0 },
        }
    }

    /// Segments the job keeps ahead of the newest request.
    pub fn lookahead(&self) -> u64 {
        lookahead_segments(self.seg_secs)
    }

    /// Decide what to do for a request of segment `k`.
    ///
    /// `run` is `None` when no job has ever run; `running` is whether the job
    /// thread is alive right now.
    pub fn decide(&self, run: Option<RunState>, running: bool, k: u64) -> Action {
        if k >= self.plan_len {
            return Action::Ended;
        }
        let Some(run) = run else {
            return Action::Start { from_k: k };
        };
        if run.ended && !running {
            // The media ended, but the renderer asked for a segment that is
            // missing (evicted from cache): re-produce just that one.
            return Action::Start { from_k: k };
        }
        if k <= run.ready_k {
            // The index says it is done; the caller polls the file first, so a
            // short wait is the right answer (the write may still be flushing).
            return Action::Wait;
        }
        let lookahead = self.lookahead();
        if running && k <= run.ready_k + lookahead {
            // The live job will walk over it.
            return Action::Wait;
        }
        if !running && k <= run.ready_k + lookahead {
            // Parked job, request close to the front: continue sequentially.
            return Action::Start {
                from_k: run.ready_k + 1,
            };
        }
        // Too far ahead of the parked/finished front: jump to the request.
        Action::Start { from_k: k }
    }

    /// Park the job once it is `lookahead` segments ahead of the newest
    /// request (or it reached the end of the media).
    pub fn should_stop(&self, run: RunState, requested_k: u64) -> bool {
        run.ended || run.ready_k >= requested_k.saturating_add(self.lookahead())
    }

    /// Where a resumed job should continue.
    pub fn resume_from(&self, run: RunState, _k: u64) -> u64 {
        run.ready_k.saturating_add(1).min(self.plan_len)
    }

    /// True when the request is within the resume margin of the ready front.
    pub fn within_resume_margin(&self, run: RunState, k: u64) -> bool {
        k.saturating_add(RESUME_MARGIN_SEGMENTS) > run.ready_k
    }
}

/// `ceil(60 / seg_secs)`, never zero.
pub fn lookahead_segments(seg_secs: f64) -> u64 {
    let seg_secs = if seg_secs > 0.0 { seg_secs } else { 6.0 };
    ((LOOKAHEAD_SECS / seg_secs).ceil() as u64).max(1)
}

/// Segment index a cache-miss request may be merged with: requesting `k` also
/// warms `k+1`/`k+2` (contract §3.5 "近邻合并").
pub fn merge_range(k: u64) -> std::ops::RangeInclusive<u64> {
    k..=k.saturating_add(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> SchedulePolicy {
        SchedulePolicy::new(100, 6.0)
    }

    #[test]
    fn lookahead_is_sixty_seconds_rounded_up() {
        assert_eq!(lookahead_segments(6.0), 10);
        assert_eq!(lookahead_segments(2.0), 30);
        assert_eq!(lookahead_segments(10.0), 6);
        assert_eq!(lookahead_segments(1.0), 60);
        assert_eq!(lookahead_segments(0.0), 10, "invalid seg size falls back");
        assert_eq!(lookahead_segments(120.0), 1, "never zero");
    }

    #[test]
    fn first_request_starts_a_job_at_that_segment() {
        assert_eq!(policy().decide(None, false, 0), Action::Start { from_k: 0 });
        assert_eq!(policy().decide(None, false, 7), Action::Start { from_k: 7 });
    }

    #[test]
    fn running_job_is_waited_for_inside_the_lookahead() {
        let run = RunState {
            first_k: 0,
            ready_k: 5,
            ended: false,
        };
        assert_eq!(policy().decide(Some(run), true, 6), Action::Wait);
        assert_eq!(policy().decide(Some(run), true, 15), Action::Wait);
        // Beyond the lookahead the running job is abandoned for a jump.
        assert_eq!(
            policy().decide(Some(run), true, 40),
            Action::Start { from_k: 40 }
        );
    }

    #[test]
    fn parked_job_resumes_sequentially_near_the_front() {
        let run = RunState {
            first_k: 0,
            ready_k: 20,
            ended: false,
        };
        assert_eq!(
            policy().decide(Some(run), false, 21),
            Action::Start { from_k: 21 }
        );
        assert_eq!(
            policy().decide(Some(run), false, 25),
            Action::Start { from_k: 21 },
            "inside the lookahead: continue from the front"
        );
    }

    #[test]
    fn parked_job_jumps_when_the_request_is_far_ahead() {
        let run = RunState {
            first_k: 0,
            ready_k: 20,
            ended: false,
        };
        assert_eq!(
            policy().decide(Some(run), false, 60),
            Action::Start { from_k: 60 }
        );
    }

    #[test]
    fn finished_media_never_restarts_for_past_the_plan() {
        let run = RunState {
            first_k: 0,
            ready_k: 99,
            ended: true,
        };
        assert_eq!(policy().decide(Some(run), false, 100), Action::Ended);
        // An evicted segment inside the plan is still re-produced.
        assert_eq!(
            policy().decide(Some(run), false, 12),
            Action::Start { from_k: 12 }
        );
    }

    #[test]
    fn stops_once_the_run_has_enough_runway() {
        let p = policy();
        let run = |ready_k| RunState {
            first_k: 0,
            ready_k,
            ended: false,
        };
        // requested 0 → needs ready_k >= 10
        assert!(!p.should_stop(run(9), 0));
        assert!(p.should_stop(run(10), 0));
        // requested 5 → needs ready_k >= 15
        assert!(!p.should_stop(run(14), 5));
        assert!(p.should_stop(run(15), 5));
        // End of media always parks the job.
        assert!(p.should_stop(
            RunState {
                first_k: 0,
                ready_k: 42,
                ended: true
            },
            90
        ));
    }

    #[test]
    fn merge_range_warms_two_neighbours() {
        assert_eq!(merge_range(4).collect::<Vec<_>>(), vec![4, 5, 6]);
        assert_eq!(merge_range(u64::MAX).collect::<Vec<_>>(), vec![u64::MAX]);
    }

    #[test]
    fn resume_margin_marks_the_front() {
        let run = RunState {
            first_k: 0,
            ready_k: 20,
            ended: false,
        };
        assert!(policy().within_resume_margin(run, 18));
        assert!(policy().within_resume_margin(run, 21));
        assert!(!policy().within_resume_margin(run, 10));
    }
}
