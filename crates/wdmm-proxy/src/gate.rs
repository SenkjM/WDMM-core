//! Global concurrency gate for expensive media jobs (contract §3.6).
//!
//! HLS transcoding and the FFI embedded-subtitle extractor both pull full
//! libav decode pipelines; running them concurrently on a phone starves the
//! CPU and makes playback stutter. A single process-wide [`JobGate`] serialises
//! them: the transcode scheduler and the subtitle extractor must acquire a
//! [`JobPermit`] before starting, and drop it when the job finishes.
//!
//! [`JobGate::acquire`] is the blocking flavour used from FFI background
//! threads; [`JobGate::acquire_async`] is used inside the proxy runtime.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// FFmpeg jobs allowed to run at the same time, process-wide.
pub const MAX_CONCURRENT_HLS_JOBS: usize = 1;
/// How long a caller waits for a permit before failing with "transcoding busy".
pub const JOB_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(15);
/// Poll interval of the blocking acquire loop.
const ACQUIRE_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Error text surfaced when another job holds the gate for too long.
pub const BUSY_ERROR: &str = "transcoding busy";

/// Counting gate shared by every libav-hungry subsystem.
pub struct JobGate {
    permits: Arc<Semaphore>,
    capacity: usize,
    waiters: AtomicU64,
}

impl JobGate {
    /// Build a gate allowing `max_concurrent` concurrent jobs.
    pub fn new(max_concurrent: usize) -> Arc<Self> {
        let capacity = max_concurrent.max(1);
        Arc::new(Self {
            permits: Arc::new(Semaphore::new(capacity)),
            capacity,
            waiters: AtomicU64::new(0),
        })
    }

    /// The process-wide default gate (`MAX_CONCURRENT_HLS_JOBS`).
    pub fn global() -> Arc<Self> {
        Self::new(MAX_CONCURRENT_HLS_JOBS)
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Permits currently held.
    pub fn in_use(&self) -> usize {
        self.capacity - self.permits.available_permits()
    }

    /// Callers currently waiting for a permit.
    pub fn waiters(&self) -> u64 {
        self.waiters.load(Ordering::Relaxed)
    }

    /// Blocking acquire, for `std::thread` callers (FFI subtitle extraction).
    ///
    /// Never call this from inside a Tokio worker: it sleeps the thread. Use
    /// [`acquire_async`](Self::acquire_async) there.
    pub fn acquire(&self, timeout: Duration) -> Result<JobPermit, String> {
        // Fast path: no contention, so the call is never counted as a waiter.
        if let Ok(permit) = self.permits.clone().try_acquire_owned() {
            return Ok(JobPermit { _permit: permit });
        }
        let deadline = Instant::now() + timeout;
        // Counted only while the permit is really contended; the guard
        // decrements on every exit path (success or timeout).
        let _waiter = WaiterGuard::new(self);
        loop {
            if let Ok(permit) = self.permits.clone().try_acquire_owned() {
                return Ok(JobPermit { _permit: permit });
            }
            if Instant::now() >= deadline {
                return Err(BUSY_ERROR.to_string());
            }
            std::thread::sleep(ACQUIRE_POLL_INTERVAL);
        }
    }

    /// Async acquire used by the transcode scheduler.
    pub async fn acquire_async(&self, timeout: Duration) -> Result<JobPermit, String> {
        self.waiters.fetch_add(1, Ordering::Relaxed);
        let acquired = tokio::time::timeout(timeout, self.permits.clone().acquire_owned()).await;
        self.waiters.fetch_sub(1, Ordering::Relaxed);
        match acquired {
            Ok(Ok(permit)) => Ok(JobPermit { _permit: permit }),
            Ok(Err(_)) => Err("job gate closed".to_string()),
            Err(_) => Err(BUSY_ERROR.to_string()),
        }
    }
}

/// Counts one blocked caller and decrements on drop.
struct WaiterGuard<'a> {
    gate: &'a JobGate,
}

impl<'a> WaiterGuard<'a> {
    fn new(gate: &'a JobGate) -> Self {
        gate.waiters.fetch_add(1, Ordering::Relaxed);
        Self { gate }
    }
}

impl Drop for WaiterGuard<'_> {
    fn drop(&mut self) {
        self.gate.waiters.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Held by a running libav job; released on drop.
///
/// Send + Sync, so the FFI can move it into a background thread.
#[derive(Debug)]
pub struct JobPermit {
    _permit: OwnedSemaphorePermit,
}

impl JobPermit {
    /// Explicitly release before the job handle is dropped.
    pub fn release(self) {
        drop(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_holder_at_a_time() {
        let gate = JobGate::new(1);
        let first = gate.acquire(Duration::from_millis(50)).unwrap();
        assert_eq!(gate.in_use(), 1);
        let err = gate.acquire(Duration::from_millis(30)).unwrap_err();
        assert_eq!(err, BUSY_ERROR);
        drop(first);
        assert_eq!(gate.in_use(), 0);
        let _second = gate.acquire(Duration::from_millis(50)).unwrap();
        assert_eq!(gate.in_use(), 1);
    }

    #[test]
    fn waiters_are_tracked_while_blocked() {
        let gate = JobGate::new(1);
        let held = gate.acquire(Duration::from_millis(10)).unwrap();
        let gate_ref = Arc::clone(&gate);
        let waiter = std::thread::spawn(move || {
            gate_ref
                .acquire(Duration::from_millis(500))
                .expect("permit after release")
        });
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(gate.waiters(), 1);
        drop(held);
        let waiter_permit = waiter.join().unwrap();
        assert_eq!(gate.waiters(), 0);
        drop(waiter_permit);
    }

    #[tokio::test]
    async fn async_acquire_and_timeout() {
        let gate = JobGate::new(1);
        let permit = gate.acquire_async(Duration::from_millis(50)).await.unwrap();
        let busy = gate
            .acquire_async(Duration::from_millis(20))
            .await
            .unwrap_err();
        assert_eq!(busy, BUSY_ERROR);
        permit.release();
        let again = gate.acquire_async(Duration::from_millis(50)).await.unwrap();
        drop(again);
    }

    #[test]
    fn permit_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<JobGate>();
        assert_send_sync::<JobPermit>();
        assert_send_sync::<Arc<JobGate>>();
    }
}
