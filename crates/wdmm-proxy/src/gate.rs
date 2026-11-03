//! Global concurrency gate for expensive media jobs (contract §3.6).
//!
//! HLS transcoding and the FFI embedded-subtitle extractor both pull libav
//! pipelines; running them concurrently on a phone starves the CPU and makes
//! playback stutter. A single process-wide [`JobGate`] serialises them.
//!
//! # Priority: a cast's transcode always wins
//!
//! Both callers used to compete for the same slot on equal terms, which caused a
//! priority inversion on the device: the embedded-subtitle probe runs right after
//! an HLS cast starts and, on a source with dozens of subtitle tracks, it holds
//! the only slot long enough that the cast's warm-up / seek re-open waited out
//! [`JOB_ACQUIRE_TIMEOUT`] and surfaced as `transcoding busy`.
//!
//! The gate therefore has two priority classes:
//!
//! * **Cast jobs** — [`JobGate::acquire_async`], the HLS transcode scheduler
//!   (warm-up runs and renderer-driven re-opens). They are what the user is
//!   looking at, so they are never queued behind background work: a cast acquire
//!   *preempts* a background holder by revoking its slot and takes it at once.
//! * **Background jobs** — [`JobGate::acquire`], the FFI subtitle probe and
//!   subtitle extraction. They only start when no cast job is running or waiting,
//!   so they can never take the slot a cast is about to need. A background waiter
//!   that cannot get the slot before its timeout fails with
//!   [`CAST_PRIORITY_ERROR`], an actionable reason instead of a generic busy.
//!
//! Preemption is a *priority* release, not a kill: the proxy cannot abort a libav
//! call already running on an FFI thread, so a revoked background job keeps
//! executing while the cast proceeds. Two properties bound the overlap:
//!
//! * A revoked job still counts against [`MAX_CONCURRENT_BACKGROUND_JOBS`], so at
//!   most one background decode is ever in flight and no *new* background job
//!   starts until it exits.
//! * Only cast activity can revoke a background holder, and only one cast job
//!   runs at a time ([`MAX_CONCURRENT_HLS_JOBS`]) — so the overlap is exactly
//!   "the cast the user asked for" + "the probe that was already in flight".
//!
//! [`JOB_ACQUIRE_TIMEOUT`] still bounds a cast's wait for the *previous cast* to
//! release the slot.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

/// Cast (HLS transcode) jobs allowed to run at the same time, process-wide.
pub const MAX_CONCURRENT_HLS_JOBS: usize = 1;
/// Background (subtitle probe / extractor) jobs allowed at the same time.
///
/// Keeping this at one means a preempted probe cannot be silently doubled up by
/// the next request: the revoked job still holds this budget until it exits.
pub const MAX_CONCURRENT_BACKGROUND_JOBS: usize = 1;
/// How long a caller waits for a permit before failing with a busy error.
pub const JOB_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(15);
/// Poll interval of the blocking (background) acquire loop.
const ACQUIRE_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Error text surfaced when another cast job holds the gate for too long.
pub const BUSY_ERROR: &str = "transcoding busy";
/// Error text surfaced when a background job loses the slot to a cast transcode.
pub const CAST_PRIORITY_ERROR: &str = "transcoding busy: cast transcode has priority";

/// Counting, priority-aware gate shared by every libav-hungry subsystem.
pub struct JobGate {
    inner: Arc<Inner>,
}

struct Inner {
    /// Cast jobs allowed concurrently.
    capacity: usize,
    state: Mutex<State>,
    /// Blocked callers (both classes) — telemetry only.
    waiters: AtomicU64,
    /// Wakes async (cast) waiters. `notify_one` stores a permit when nobody is
    /// waiting, so a release that lands between the state check and
    /// `notified().await` is never lost.
    wake: Notify,
}

#[derive(Default)]
struct State {
    /// Cast jobs currently holding a slot.
    high_active: usize,
    /// Cast callers blocked waiting for a slot.
    high_waiting: usize,
    /// Background jobs currently executing (including revoked ones).
    background_active: usize,
    /// Revocation handle of the background job that owns the slot.
    background: Option<Arc<BackgroundHandle>>,
    /// Background waits that timed out because a cast had priority.
    background_rejections: u64,
    /// Background jobs that lost their slot to a cast job.
    preemptions: u64,
}

/// Revocation flag handed to a background job through its permit.
#[derive(Debug, Default)]
struct BackgroundHandle {
    revoked: AtomicBool,
}

impl BackgroundHandle {
    fn revoke(&self) {
        self.revoked.store(true, Ordering::SeqCst);
    }

    fn is_revoked(&self) -> bool {
        self.revoked.load(Ordering::SeqCst)
    }
}

/// Decrements the shared waiter counter on drop.
struct WaiterCounter<'a> {
    waiters: &'a AtomicU64,
}

impl<'a> WaiterCounter<'a> {
    fn new(waiters: &'a AtomicU64) -> Self {
        waiters.fetch_add(1, Ordering::Relaxed);
        Self { waiters }
    }
}

impl Drop for WaiterCounter<'_> {
    fn drop(&mut self) {
        self.waiters.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A cast caller parked for a slot: holds both `high_waiting` and `waiters` until
/// it is admitted or gives up.
struct CastWaiter<'a> {
    gate: &'a JobGate,
    _counter: WaiterCounter<'a>,
}

impl<'a> CastWaiter<'a> {
    fn new(gate: &'a JobGate) -> Self {
        gate.inner.state.lock().unwrap().high_waiting += 1;
        Self {
            gate,
            _counter: WaiterCounter::new(&gate.inner.waiters),
        }
    }
}

impl Drop for CastWaiter<'_> {
    fn drop(&mut self) {
        let mut state = self.gate.inner.state.lock().unwrap();
        state.high_waiting = state.high_waiting.saturating_sub(1);
    }
}

impl JobGate {
    /// Build a gate allowing `max_concurrent` cast jobs.
    pub fn new(max_concurrent: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(Inner {
                capacity: max_concurrent.max(1),
                state: Mutex::new(State::default()),
                waiters: AtomicU64::new(0),
                wake: Notify::new(),
            }),
        })
    }

    /// The process-wide default gate (`MAX_CONCURRENT_HLS_JOBS`).
    pub fn global() -> Arc<Self> {
        Self::new(MAX_CONCURRENT_HLS_JOBS)
    }

    pub fn capacity(&self) -> usize {
        self.inner.capacity
    }

    /// Jobs (cast + background, revoked background included) currently holding a
    /// slot.
    pub fn in_use(&self) -> usize {
        let state = self.inner.state.lock().unwrap();
        state.high_active + state.background_active
    }

    /// Callers currently blocked waiting for a slot.
    pub fn waiters(&self) -> u64 {
        self.inner.waiters.load(Ordering::Relaxed)
    }

    /// Background jobs that were preempted by a cast job since process start.
    pub fn preemptions(&self) -> u64 {
        self.inner.state.lock().unwrap().preemptions
    }

    /// Background waits skipped because a cast job had priority since process start.
    pub fn background_rejections(&self) -> u64 {
        self.inner.state.lock().unwrap().background_rejections
    }

    /// Blocking **background** acquire, for `std::thread` callers (FFI subtitle
    /// probe / extractor).
    ///
    /// Succeeds only when no cast job is running or waiting. The slot is
    /// revocable: a cast acquire preempts this permit immediately and
    /// [`JobPermit::is_revoked`] turns true. Times out with
    /// [`CAST_PRIORITY_ERROR`] while a cast job is active or queued, and with
    /// [`BUSY_ERROR`] when another background job holds the slot.
    ///
    /// Never call this from a Tokio worker thread: it sleeps. Use
    /// [`acquire_async`](Self::acquire_async) there.
    pub fn acquire(&self, timeout: Duration) -> Result<JobPermit, String> {
        let deadline = std::time::Instant::now() + timeout;
        let mut _waiter: Option<WaiterCounter<'_>> = None;
        loop {
            let busy_with_cast;
            {
                let mut state = self.inner.state.lock().unwrap();
                if state.high_active == 0
                    && state.high_waiting == 0
                    && state.background_active < MAX_CONCURRENT_BACKGROUND_JOBS
                {
                    let handle = Arc::new(BackgroundHandle::default());
                    state.background = Some(Arc::clone(&handle));
                    state.background_active = 1;
                    drop(state);
                    return Ok(JobPermit {
                        inner: Arc::clone(&self.inner),
                        kind: PermitKind::Background(handle),
                        preempted: false,
                    });
                }
                busy_with_cast = state.high_active > 0 || state.high_waiting > 0;
            }
            if _waiter.is_none() {
                _waiter = Some(WaiterCounter::new(&self.inner.waiters));
            }
            if std::time::Instant::now() >= deadline {
                if busy_with_cast {
                    self.inner.state.lock().unwrap().background_rejections += 1;
                    return Err(CAST_PRIORITY_ERROR.to_string());
                }
                return Err(BUSY_ERROR.to_string());
            }
            std::thread::sleep(ACQUIRE_POLL_INTERVAL);
        }
    }

    /// Async **cast** acquire used by the transcode scheduler.
    ///
    /// A cast job never waits behind background work: if a background job holds
    /// the slot it is revoked and the cast takes it immediately (the returned
    /// permit reports [`JobPermit::preempted`]). Waiting only happens when
    /// another cast job holds the slot, bounded by `timeout`.
    pub async fn acquire_async(&self, timeout: Duration) -> Result<JobPermit, String> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut _waiter: Option<CastWaiter<'_>> = None;
        loop {
            // Register interest before inspecting the state so a release cannot
            // slip between the check and the await.
            let notified = self.inner.wake.notified();
            let mut preempted = false;
            {
                let mut state = self.inner.state.lock().unwrap();
                if state.high_active < self.inner.capacity {
                    state.high_active += 1;
                    if let Some(handle) = state.background.take() {
                        handle.revoke();
                        state.preemptions += 1;
                        preempted = true;
                    }
                    drop(state);
                    return Ok(JobPermit {
                        inner: Arc::clone(&self.inner),
                        kind: PermitKind::High,
                        preempted,
                    });
                }
            }
            if _waiter.is_none() {
                _waiter = Some(CastWaiter::new(self));
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err(BUSY_ERROR.to_string());
            }
            let _ = tokio::time::timeout(deadline - now, notified).await;
        }
    }
}

enum PermitKind {
    High,
    Background(Arc<BackgroundHandle>),
}

/// Held by a running libav job; released on drop.
///
/// `Send + Sync`, so the FFI can move it into a background thread.
pub struct JobPermit {
    inner: Arc<Inner>,
    kind: PermitKind,
    preempted: bool,
}

impl std::fmt::Debug for JobPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobPermit")
            .field(
                "class",
                &match self.kind {
                    PermitKind::High => "cast",
                    PermitKind::Background(_) => "background",
                },
            )
            .field("preempted", &self.preempted)
            .field("revoked", &self.is_revoked())
            .finish()
    }
}

impl JobPermit {
    /// Explicitly release before the job handle is dropped.
    pub fn release(self) {
        drop(self);
    }

    /// This permit was issued to a cast job and took the slot from a background
    /// holder (contract §3.6 priority rule).
    pub fn preempted(&self) -> bool {
        self.preempted
    }

    /// A background permit that a cast job revoked: the job may still be running
    /// but no longer owns the slot.
    pub fn is_revoked(&self) -> bool {
        match &self.kind {
            PermitKind::High => false,
            PermitKind::Background(handle) => handle.is_revoked(),
        }
    }
}

impl Drop for JobPermit {
    fn drop(&mut self) {
        {
            let mut state = self.inner.state.lock().unwrap();
            match &self.kind {
                PermitKind::High => {
                    state.high_active = state.high_active.saturating_sub(1);
                }
                PermitKind::Background(handle) => {
                    state.background_active = state.background_active.saturating_sub(1);
                    let same = state
                        .background
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, handle));
                    if same {
                        state.background = None;
                    }
                }
            }
        }
        // Wake exactly one cast waiter; a stored permit covers a waiter that has
        // not polled yet.
        self.inner.wake.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cast(gate: &JobGate, timeout: Duration) -> Result<JobPermit, String> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(gate.acquire_async(timeout))
    }

    fn ms(started: std::time::Instant) -> u128 {
        started.elapsed().as_millis()
    }

    #[test]
    fn only_one_cast_holder_at_a_time() {
        let gate = JobGate::new(1);
        let first = cast(&gate, Duration::from_millis(50)).expect("first cast slot");
        assert_eq!(gate.in_use(), 1);
        let err = cast(&gate, Duration::from_millis(30)).unwrap_err();
        assert_eq!(err, BUSY_ERROR);
        assert_eq!(gate.waiters(), 0);
        drop(first);
        assert_eq!(gate.in_use(), 0);
        assert!(cast(&gate, Duration::from_millis(50)).is_ok());
    }

    #[test]
    fn cast_acquire_preempts_background_probe_immediately() {
        let gate = JobGate::new(1);
        let probe = gate.acquire(Duration::from_millis(50)).expect("probe slot");
        assert!(!probe.is_revoked());
        assert_eq!(gate.in_use(), 1);

        let started = std::time::Instant::now();
        let cast_permit = cast(&gate, Duration::from_millis(50)).expect("cast preempts probe");
        let waited = ms(started);
        assert!(
            waited < 50,
            "cast acquire waited {waited} ms behind a probe"
        );
        assert!(cast_permit.preempted(), "permit must report the preemption");
        assert!(probe.is_revoked(), "probe permit must be revoked");
        assert_eq!(gate.preemptions(), 1);
        // The revoked probe is still executing, so both jobs hold slots for now.
        assert_eq!(gate.in_use(), 2);
        drop(probe);
        assert_eq!(gate.in_use(), 1);
        drop(cast_permit);
        assert_eq!(gate.in_use(), 0);
    }

    #[test]
    fn background_cannot_start_while_cast_job_is_active() {
        let gate = JobGate::new(1);
        let holder = cast(&gate, Duration::from_millis(50)).expect("cast slot");
        let started = std::time::Instant::now();
        let err = gate.acquire(Duration::from_millis(60)).unwrap_err();
        let waited = ms(started);
        assert_eq!(err, CAST_PRIORITY_ERROR);
        assert_eq!(gate.background_rejections(), 1);
        assert!(
            waited >= 60,
            "background must honour its timeout ({waited} ms)"
        );
        assert_eq!(gate.preemptions(), 0, "no probe ever held the slot");
        assert_eq!(gate.waiters(), 0);
        drop(holder);
        // Once the cast parks, the probe is admitted.
        let probe = gate
            .acquire(Duration::from_millis(50))
            .expect("probe after park");
        assert!(!probe.is_revoked());
    }

    #[test]
    fn background_waits_for_a_parked_cast_then_runs() {
        let gate = JobGate::new(1);
        let holder = cast(&gate, Duration::from_millis(50)).expect("cast slot");
        let gate_ref = Arc::clone(&gate);
        let waiter = std::thread::spawn(move || gate_ref.acquire(Duration::from_millis(500)));
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(gate.waiters(), 1);
        drop(holder);
        let probe = waiter.join().unwrap().expect("probe admitted after park");
        assert!(!probe.is_revoked());
        assert_eq!(gate.waiters(), 0);
        assert_eq!(gate.background_rejections(), 0);
    }

    #[test]
    fn background_cannot_jump_a_queued_cast() {
        let gate = JobGate::new(1);
        let running = cast(&gate, Duration::from_millis(50)).expect("cast slot");
        let gate_ref = Arc::clone(&gate);
        let queued = std::thread::spawn(move || cast(&gate_ref, Duration::from_millis(500)));
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(gate.waiters(), 1, "the queued cast is a waiter");
        let err = gate.acquire(Duration::from_millis(40)).unwrap_err();
        assert_eq!(err, CAST_PRIORITY_ERROR, "a claim ahead of the probe wins");
        drop(running);
        let admitted = queued.join().unwrap().expect("queued cast wins the slot");
        assert!(!admitted.preempted());
        assert_eq!(gate.waiters(), 0);
    }

    #[test]
    fn only_one_background_job_runs() {
        let gate = JobGate::new(1);
        let first = gate.acquire(Duration::from_millis(50)).expect("probe slot");
        let started = std::time::Instant::now();
        let err = gate.acquire(Duration::from_millis(40)).unwrap_err();
        assert_eq!(err, BUSY_ERROR);
        assert!(ms(started) >= 40);
        drop(first);
        assert!(gate.acquire(Duration::from_millis(50)).is_ok());
    }

    #[test]
    fn revoked_background_still_blocks_new_background_jobs() {
        let gate = JobGate::new(1);
        let probe = gate.acquire(Duration::from_millis(50)).expect("probe slot");
        let cast_permit = cast(&gate, Duration::from_millis(50)).expect("cast preempts probe");
        assert!(cast_permit.preempted());
        drop(cast_permit);
        // The revoked probe is still executing on its FFI thread.
        let err = gate.acquire(Duration::from_millis(30)).unwrap_err();
        assert_eq!(err, BUSY_ERROR, "a revoked probe still owns its budget");
        drop(probe);
        assert!(gate.acquire(Duration::from_millis(50)).is_ok());
    }

    #[test]
    fn permit_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<JobGate>();
        assert_send_sync::<JobPermit>();
        assert_send_sync::<Arc<JobGate>>();
    }
}
