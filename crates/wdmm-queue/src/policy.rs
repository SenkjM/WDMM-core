//! Slice 10: task-queue retry policy (retry caps, backoff, offline detection).
//!
//! This is the single authority for "what happens after a task failed". It was
//! ported from `lib/services/task_queue_service.dart`: the retry caps, the
//! 2/4/8/16/32 s backoff, the "no network" message table and the
//! cancel/fail/retry verdict all live here now. Dart keeps the per-kind
//! execution and the persisted task state, classifies the exception into a
//! [`FailureCategory`], and asks for one decision per failure through
//! `wdmm_queue_retry_json` (see `crates/wdmm-ffi/src/ext/policy.rs`).

/// Automatic retries allowed for ordinary (network) failures.
pub const MAX_AUTO_RETRIES: u32 = 5;

/// Automatic retries allowed when the app is in the background *and* offline.
///
/// Foreground failures, and background failures that are not offline, still use
/// [`MAX_AUTO_RETRIES`].
pub const BACKGROUND_OFFLINE_MAX_AUTO_RETRIES: u32 = 3;

/// Wait after a failure while offline. Network events can wake the queue early;
/// the attempt still counts against the cap.
///
/// Do not grow this: a user waiting for the network to come back reads five
/// minutes as "stuck". Ten seconds is the agreed interval.
pub const OFFLINE_RETRY_DELAY_MS: u64 = 10_000;

/// Lowercase message fragments that mean "there is no network at all"
/// (DNS / unreachable), as opposed to a slow or flaky peer.
pub const OFFLINE_SNIPPETS: &[&str] = &[
    "network is unreachable",
    "network is down",
    "failed host lookup",
    "unable to resolve host",
    "no address associated",
    "nodename nor servname",
];

/// Backoff base: the first retry waits 2 s, then doubles up to 32 s.
const BACKOFF_BASE_MS: u64 = 1_000;

/// Upper bound of the backoff exponent (2^5 = 32 s).
const BACKOFF_MAX_EXP: u32 = 5;

/// Largest jitter Dart may ask for (0..=999 ms).
const MAX_JITTER_MS: u64 = 999;

/// What kind of failure the task runner saw. Dart classifies the exception into
/// one of these (type checks only, never message matching for the category);
/// Rust owns what each category means for retrying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureCategory {
    /// Transport/HTTP failure from the driver (including auth and 4xx/5xx).
    Dio,
    /// Content itself is broken (decrypt / corrupt data).
    VfsData,
    /// Local state error, e.g. a task that no longer exists.
    State,
    /// Anything the runner could not classify.
    Other,
}

impl FailureCategory {
    /// Parse the wire name. `None` when the caller sent an unknown category.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "dio" => Some(Self::Dio),
            "vfs_data" => Some(Self::VfsData),
            "state" => Some(Self::State),
            "other" => Some(Self::Other),
            _ => None,
        }
    }

    /// Wire name (`dio`, `vfs_data`, `state`, `other`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dio => "dio",
            Self::VfsData => "vfs_data",
            Self::State => "state",
            Self::Other => "other",
        }
    }
}

/// Verdict for one failed attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryAction {
    /// The user cancelled (or the task was already cancelled): stop quietly.
    Cancel,
    /// Schedule another attempt after [`RetryDecision::delay_ms`].
    Retry,
    /// Give up: the task is terminal (`failed`).
    Fail,
}

impl RetryAction {
    /// Wire name (`cancel`, `retry`, `fail`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cancel => "cancel",
            Self::Retry => "retry",
            Self::Fail => "fail",
        }
    }
}

/// Everything one failure decision depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryInput {
    /// Attempts already recorded on the task (0 before the first failure).
    pub attempts: u32,
    /// Whether the app is currently in the background.
    pub in_background: bool,
    /// Exception category, classified by the Dart runner.
    pub category: FailureCategory,
    /// Error text, used only for the offline/dNS check.
    pub message: String,
    /// The user cancelled the task (or it was already cancelled).
    pub cancelled: bool,
    /// Jitter requested by the caller, 0..=999 ms. Supplied by Dart so a single
    /// decision is deterministic; missing/oversized values are clamped.
    pub jitter_ms: u64,
}

/// The decision handed back to Dart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryDecision {
    pub action: RetryAction,
    /// Attempts to store on the task: the *new* count on retry (old + 1), the
    /// unchanged count on cancel/fail.
    pub attempts: u32,
    /// Wait before the next attempt; 0 when the action is not `retry`.
    pub delay_ms: u64,
    /// Whether the failure looked like "no network".
    pub offline: bool,
    /// Retry cap in force for this decision (3 background+offline, else 5);
    /// the UI renders `attempts/cap`.
    pub cap: u32,
}

/// True when `message` looks like "the device has no network".
pub fn is_offline(message: &str) -> bool {
    let lower = message.to_lowercase();
    OFFLINE_SNIPPETS
        .iter()
        .any(|snippet| lower.contains(snippet))
}

/// Retry cap in force for one failure.
pub fn retry_cap(in_background: bool, offline: bool) -> u32 {
    if in_background && offline {
        BACKGROUND_OFFLINE_MAX_AUTO_RETRIES
    } else {
        MAX_AUTO_RETRIES
    }
}

/// Whether a failure of `category` is worth another attempt at all.
///
/// Default is retryable: unknown errors still go through the backoff (product
/// requirement). Only explicitly meaningless retries are fatal.
pub fn is_retryable(category: FailureCategory) -> bool {
    !matches!(category, FailureCategory::VfsData | FailureCategory::State)
}

/// Full retryability verdict for one failure.
///
/// Contract (§Slice 10): `vfs_data`, `state`, and a cancelled `dio` request are
/// fatal. Dart classifies the exception into a category; a cancelled HTTP
/// request is the one fatal case that has to be read from the text, because it
/// is still a `dio` failure.
pub fn is_retryable_failure(category: FailureCategory, message: &str) -> bool {
    if !is_retryable(category) {
        return false;
    }
    if category == FailureCategory::Dio && message.to_lowercase().contains("cancel") {
        return false;
    }
    true
}

/// String-only retryability check used by the FIFO pumps, which keep only the
/// error text (`TaskQueue::should_auto_retry`).
pub fn message_is_retryable(message: &str) -> bool {
    let lower = message.to_lowercase();
    if lower.contains("cancel") {
        return false;
    }
    !(lower.contains("decrypt") || lower.contains("corrupt"))
}

/// Backoff for the `attempts`-th retry: 2/4/8/16/32 s plus `jitter_ms`.
///
/// `jitter_ms` is folded into 0..=999 ms; `attempts` is clamped into the table,
/// so a task past the cap does not keep growing the wait.
pub fn backoff_delay_ms(attempts: u32, jitter_ms: u64) -> u64 {
    let exponent = attempts.clamp(1, BACKOFF_MAX_EXP);
    BACKOFF_BASE_MS * (1u64 << exponent) + jitter_ms.min(MAX_JITTER_MS)
}

/// The single entry point: decide what to do with one failed attempt.
pub fn decide(input: &RetryInput) -> RetryDecision {
    let offline = is_offline(&input.message);
    let cap = retry_cap(input.in_background, offline);
    if input.cancelled {
        return RetryDecision {
            action: RetryAction::Cancel,
            attempts: input.attempts,
            delay_ms: 0,
            offline,
            cap,
        };
    }
    if is_retryable_failure(input.category, &input.message) && input.attempts < cap {
        let attempts = input.attempts + 1;
        let delay_ms = if offline {
            OFFLINE_RETRY_DELAY_MS
        } else {
            backoff_delay_ms(attempts, input.jitter_ms)
        };
        return RetryDecision {
            action: RetryAction::Retry,
            attempts,
            delay_ms,
            offline,
            cap,
        };
    }
    RetryDecision {
        action: RetryAction::Fail,
        attempts: input.attempts,
        delay_ms: 0,
        offline,
        cap,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(attempts: u32, category: FailureCategory, message: &str) -> RetryInput {
        RetryInput {
            attempts,
            in_background: false,
            category,
            message: message.to_string(),
            cancelled: false,
            jitter_ms: 0,
        }
    }

    #[test]
    fn caps_are_five_and_three_for_background_offline() {
        assert_eq!(retry_cap(false, false), 5);
        assert_eq!(retry_cap(false, true), 5);
        assert_eq!(retry_cap(true, false), 5);
        assert_eq!(retry_cap(true, true), 3);
    }

    #[test]
    fn every_offline_snippet_is_recognised_case_insensitively() {
        for snippet in OFFLINE_SNIPPETS {
            let upper = snippet.to_uppercase();
            assert!(is_offline(&format!("DioException: {upper}")), "{snippet}");
        }
        assert!(!is_offline("HTTP 503 Service Unavailable"));
    }

    #[test]
    fn backoff_is_two_four_eight_sixteen_thirty_two_seconds() {
        let expected = [2_000, 4_000, 8_000, 16_000, 32_000];
        for (index, want) in expected.iter().enumerate() {
            assert_eq!(backoff_delay_ms(index as u32 + 1, 0), *want);
        }
        // Past the cap the wait stops growing.
        assert_eq!(backoff_delay_ms(9, 0), 32_000);
        assert_eq!(backoff_delay_ms(0, 0), 2_000);
        // Jitter is added once and bounded.
        assert_eq!(backoff_delay_ms(1, 999), 2_999);
        assert_eq!(backoff_delay_ms(1, 5_000), 2_999);
    }

    #[test]
    fn a_retry_increments_the_attempt_count_and_uses_the_backoff() {
        let decision = decide(&input(0, FailureCategory::Dio, "timeout"));
        assert_eq!(decision.action, RetryAction::Retry);
        assert_eq!(decision.attempts, 1);
        assert_eq!(decision.delay_ms, 2_000);
        assert!(!decision.offline);
        assert_eq!(decision.cap, 5);
    }

    #[test]
    fn offline_waits_ten_seconds_and_keeps_counting() {
        let mut input = input(1, FailureCategory::Other, "Failed host lookup");
        input.in_background = true;
        let decision = decide(&input);
        assert_eq!(decision.action, RetryAction::Retry);
        assert_eq!(decision.attempts, 2);
        assert_eq!(decision.delay_ms, OFFLINE_RETRY_DELAY_MS);
        assert!(decision.offline);
        assert_eq!(decision.cap, 3);
    }

    #[test]
    fn background_offline_stops_at_three_others_at_five() {
        let mut bg = input(2, FailureCategory::Dio, "network is unreachable");
        bg.in_background = true;
        assert_eq!(decide(&bg).action, RetryAction::Retry);
        bg.attempts = 3;
        let exhausted = decide(&bg);
        assert_eq!(exhausted.action, RetryAction::Fail);
        assert_eq!(exhausted.attempts, 3);
        assert_eq!(exhausted.delay_ms, 0);

        let foreground = input(4, FailureCategory::Dio, "network is unreachable");
        assert_eq!(decide(&foreground).action, RetryAction::Retry);
        let at_cap = input(5, FailureCategory::Dio, "network is unreachable");
        assert_eq!(decide(&at_cap).action, RetryAction::Fail);
    }

    #[test]
    fn fatal_categories_never_retry() {
        for category in [FailureCategory::VfsData, FailureCategory::State] {
            let decision = decide(&input(0, category, "everything is fine"));
            assert_eq!(decision.action, RetryAction::Fail);
            assert_eq!(decision.attempts, 0);
        }
        assert!(is_retryable(FailureCategory::Dio));
        assert!(is_retryable(FailureCategory::Other));
    }

    #[test]
    fn cancellation_wins_over_every_other_rule() {
        let mut input = input(0, FailureCategory::Dio, "timeout");
        input.cancelled = true;
        let decision = decide(&input);
        assert_eq!(decision.action, RetryAction::Cancel);
        assert_eq!(decision.attempts, 0);
        assert_eq!(decision.delay_ms, 0);
    }

    #[test]
    fn categories_round_trip_through_their_wire_names() {
        for category in [
            FailureCategory::Dio,
            FailureCategory::VfsData,
            FailureCategory::State,
            FailureCategory::Other,
        ] {
            assert_eq!(FailureCategory::parse(category.as_str()), Some(category));
        }
        assert_eq!(FailureCategory::parse("weird"), None);
        assert_eq!(RetryAction::Cancel.as_str(), "cancel");
        assert_eq!(RetryAction::Retry.as_str(), "retry");
        assert_eq!(RetryAction::Fail.as_str(), "fail");
    }

    #[test]
    fn message_retryability_matches_the_original_heuristic() {
        assert!(message_is_retryable("connection reset by peer"));
        assert!(!message_is_retryable("operation cancelled"));
        assert!(!message_is_retryable("decrypt failed"));
        assert!(!message_is_retryable("corrupt block"));
    }

    #[test]
    fn a_cancelled_dio_request_is_fatal_but_other_dio_failures_retry() {
        // Dio reports cancellation as a `cancel`-type exception, which stays in
        // the `dio` category — the verdict is fatal, matching the old Dart rule.
        let cancelled = decide(&input(
            0,
            FailureCategory::Dio,
            "DioException [cancel]: The request was cancelled.",
        ));
        assert_eq!(cancelled.action, RetryAction::Fail);
        assert_eq!(cancelled.attempts, 0);
        assert_eq!(cancelled.delay_ms, 0);

        assert!(is_retryable_failure(FailureCategory::Dio, "timeout"));
        assert!(is_retryable_failure(FailureCategory::Other, "cancelled"));
        assert!(!is_retryable_failure(FailureCategory::VfsData, "timeout"));
        assert!(!is_retryable_failure(FailureCategory::State, "timeout"));
    }
}
