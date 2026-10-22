//! `wdmm-queue` — task-queue state machine (FIFO + resumable operations).
//!
//! Core essentials ported from `task_queue_service.dart`:
//! - State transitions: Pending → Active → {Completed, Failed, Cancelled}
//! - Progress events
//! - Cancel tokens
//! - Retry logic with exponential backoff
//! - FIFO ordering by creation time

#![forbid(unsafe_code)]

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{broadcast, RwLock};
use wdmm_core::models::{QueueTask, TaskStatus};

#[derive(Debug, Error)]
pub enum QueueError {
    #[error("task not found: {0}")]
    NotFound(String),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("task already active")]
    AlreadyActive,

    #[error("task cancelled")]
    Cancelled,

    #[error("other: {0}")]
    Other(String),
}

pub type QueueResult<T> = std::result::Result<T, QueueError>;

/// Progress event emitted during task execution.
#[derive(Debug, Clone)]
pub struct ProgressEvent {
    pub task_id: String,
    pub transferred: u64,
    pub total: u64,
    pub progress: f32, // 0.0..=1.0
}

/// Queue event (progress, completion, failure).
#[derive(Debug, Clone)]
pub enum QueueEvent {
    Progress(ProgressEvent),
    Completed { task_id: String },
    Failed { task_id: String, error: String },
    Cancelled { task_id: String },
}

/// Task executor trait — each `TaskKind` has an executor.
#[async_trait]
pub trait TaskExecutor: Send + Sync {
    async fn execute(
        &self,
        task: &QueueTask,
        progress_tx: broadcast::Sender<ProgressEvent>,
    ) -> QueueResult<()>;
}

/// Cancel token for stopping a running task.
#[derive(Clone)]
pub struct CancelToken {
    cancelled: Arc<RwLock<bool>>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(RwLock::new(false)),
        }
    }

    pub async fn cancel(&self) {
        *self.cancelled.write().await = true;
    }

    pub async fn is_cancelled(&self) -> bool {
        *self.cancelled.read().await
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

/// Task queue with state machine, events, and cancellation.
pub struct TaskQueue {
    tasks: Arc<RwLock<Vec<QueueTask>>>,
    cancel_tokens: Arc<RwLock<HashMap<String, CancelToken>>>,
    event_tx: broadcast::Sender<QueueEvent>,
}

impl TaskQueue {
    pub fn new() -> Self {
        let (event_tx, _) = broadcast::channel(100);
        Self {
            tasks: Arc::new(RwLock::new(Vec::new())),
            cancel_tokens: Arc::new(RwLock::new(HashMap::new())),
            event_tx,
        }
    }

    /// Subscribe to queue events (progress, completion, errors).
    pub fn subscribe(&self) -> broadcast::Receiver<QueueEvent> {
        self.event_tx.subscribe()
    }

    /// Enqueue a new task (FIFO by creation time).
    pub async fn enqueue(&self, mut task: QueueTask) -> QueueResult<()> {
        task.status = TaskStatus::Pending;
        task.attempts = 0;
        task.progress = 0.0;
        task.transferred_bytes = 0;

        let mut tasks = self.tasks.write().await;
        tasks.push(task);
        // Maintain FIFO order by creation time
        tasks.sort_by(|a, b| a.created.cmp(&b.created));
        Ok(())
    }

    /// Get all tasks.
    pub async fn list(&self) -> Vec<QueueTask> {
        self.tasks.read().await.clone()
    }

    /// Get tasks by status.
    pub async fn list_by_status(&self, status: TaskStatus) -> Vec<QueueTask> {
        self.tasks
            .read()
            .await
            .iter()
            .filter(|t| t.status == status)
            .cloned()
            .collect()
    }

    /// Get next pending task (FIFO).
    pub async fn next_pending(&self) -> Option<QueueTask> {
        self.tasks
            .read()
            .await
            .iter()
            .find(|t| t.status == TaskStatus::Pending)
            .cloned()
    }

    /// Update task status.
    pub async fn update_status(&self, id: &str, status: TaskStatus) -> QueueResult<()> {
        let mut tasks = self.tasks.write().await;
        if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
            t.status = status;
            t.updated = chrono::Utc::now();
            Ok(())
        } else {
            Err(QueueError::NotFound(id.to_string()))
        }
    }

    /// Update task progress.
    pub async fn update_progress(
        &self,
        id: &str,
        transferred: u64,
        total: u64,
    ) -> QueueResult<()> {
        let progress = if total > 0 {
            (transferred as f32 / total as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };

        let mut tasks = self.tasks.write().await;
        if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
            t.transferred_bytes = transferred;
            t.total_bytes = total;
            t.progress = progress;
            t.updated = chrono::Utc::now();

            // Emit progress event
            let _ = self.event_tx.send(QueueEvent::Progress(ProgressEvent {
                task_id: id.to_string(),
                transferred,
                total,
                progress,
            }));

            Ok(())
        } else {
            Err(QueueError::NotFound(id.to_string()))
        }
    }

    /// Mark task as failed and record error.
    pub async fn mark_failed(&self, id: &str, error: String) -> QueueResult<()> {
        let mut tasks = self.tasks.write().await;
        if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
            t.status = TaskStatus::Failed;
            t.error = Some(error.clone());
            t.updated = chrono::Utc::now();

            let _ = self.event_tx.send(QueueEvent::Failed {
                task_id: id.to_string(),
                error,
            });

            Ok(())
        } else {
            Err(QueueError::NotFound(id.to_string()))
        }
    }

    /// Mark task as completed.
    pub async fn mark_completed(&self, id: &str) -> QueueResult<()> {
        let mut tasks = self.tasks.write().await;
        if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
            t.status = TaskStatus::Completed;
            t.progress = 1.0;
            t.updated = chrono::Utc::now();

            let _ = self.event_tx.send(QueueEvent::Completed {
                task_id: id.to_string(),
            });

            Ok(())
        } else {
            Err(QueueError::NotFound(id.to_string()))
        }
    }

    /// Cancel a task (if active) or remove it (if pending).
    pub async fn cancel(&self, id: &str) -> QueueResult<()> {
        // Signal cancellation to running task
        let tokens = self.cancel_tokens.read().await;
        if let Some(token) = tokens.get(id) {
            token.cancel().await;
        }
        drop(tokens);

        // Update status
        let mut tasks = self.tasks.write().await;
        if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
            t.status = TaskStatus::Cancelled;
            t.updated = chrono::Utc::now();

            let _ = self.event_tx.send(QueueEvent::Cancelled {
                task_id: id.to_string(),
            });

            Ok(())
        } else {
            Err(QueueError::NotFound(id.to_string()))
        }
    }

    /// Remove a task completely.
    pub async fn remove(&self, id: &str) -> QueueResult<()> {
        let mut tasks = self.tasks.write().await;
        if let Some(pos) = tasks.iter().position(|t| t.id == id) {
            tasks.remove(pos);
            Ok(())
        } else {
            Err(QueueError::NotFound(id.to_string()))
        }
    }

    /// Start executing a task (transition Pending → Active).
    pub async fn start_task(&self, id: &str) -> QueueResult<CancelToken> {
        let mut tasks = self.tasks.write().await;
        if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
            if t.status != TaskStatus::Pending {
                return Err(QueueError::AlreadyActive);
            }
            t.status = TaskStatus::Active;
            t.attempts += 1;
            t.updated = chrono::Utc::now();

            // Create cancel token
            let token = CancelToken::new();
            let mut tokens = self.cancel_tokens.write().await;
            tokens.insert(id.to_string(), token.clone());

            Ok(token)
        } else {
            Err(QueueError::NotFound(id.to_string()))
        }
    }

    /// Retry a failed task (reset to Pending).
    pub async fn retry(&self, id: &str) -> QueueResult<()> {
        let mut tasks = self.tasks.write().await;
        if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
            if t.status != TaskStatus::Failed && t.status != TaskStatus::Cancelled {
                return Ok(()); // Already in progress or completed
            }
            t.status = TaskStatus::Pending;
            t.progress = 0.0;
            t.transferred_bytes = 0;
            t.error = None;
            t.updated = chrono::Utc::now();
            Ok(())
        } else {
            Err(QueueError::NotFound(id.to_string()))
        }
    }

    /// Get retry delay based on attempt count (exponential backoff with jitter).
    pub fn retry_delay(attempts: u32) -> std::time::Duration {
        let base_seconds = 1u64 << attempts.min(5); // 2, 4, 8, 16, 32
        let jitter_ms = rand::random::<u64>() % 1000;
        std::time::Duration::from_millis(base_seconds * 1000 + jitter_ms)
    }

    /// Check if task should be automatically retried.
    pub fn should_auto_retry(task: &QueueTask, max_retries: u32) -> bool {
        task.attempts < max_retries
            && task.status == TaskStatus::Failed
            && task.error.as_ref().map_or(true, |e| is_retryable_error(e))
    }
}

/// Check if error is retryable (network errors are retryable, data errors are not).
fn is_retryable_error(error: &str) -> bool {
    let lower = error.to_lowercase();
    // Network errors are retryable
    if lower.contains("network") || lower.contains("timeout") || lower.contains("connection") {
        return true;
    }
    // Cancellation is not retryable
    if lower.contains("cancel") {
        return false;
    }
    // Data errors are not retryable
    if lower.contains("decrypt") || lower.contains("corrupt") {
        return false;
    }
    // Default to retryable for unknown errors
    true
}

impl Default for TaskQueue {
    fn default() -> Self {
        Self::new()
    }
}

// ── Serial pump (single-worker FIFO) ──────────────────────────────────────────
//
// Product note: the Dart `TaskQueueService._pump` processes exactly one task at
// a time. WDMM must match — no concurrent download workers. `SerialPump` drives
// the [`TaskQueue`] state machine with a [`TaskExecutor`], guaranteeing at most
// one Active task at any instant. Concurrency is fixed at 1 and cannot be
// raised through this API (there is no `concurrency` knob on purpose).

/// Maximum automatic retries before a task is left in the Failed state.
pub const DEFAULT_MAX_RETRIES: u32 = 3;

/// A single-worker FIFO pump over a [`TaskQueue`].
///
/// Created from a shared queue + an executor. [`SerialPump::pump_once`] picks
/// the next Pending task, runs it to completion (Completed/Failed/Cancelled),
/// then returns. Repeated calls drain the queue. Because the pump holds an
/// executor handle and processes one task at a time, there is never more than
/// one Active task.
pub struct SerialPump {
    queue: Arc<TaskQueue>,
    executor: Arc<dyn TaskExecutor>,
    max_retries: u32,
}

impl SerialPump {
    /// Construct a serial pump. Concurrency is implicitly 1.
    pub fn new(queue: Arc<TaskQueue>, executor: Arc<dyn TaskExecutor>) -> Self {
        Self {
            queue,
            executor,
            max_retries: DEFAULT_MAX_RETRIES,
        }
    }

    /// Override the auto-retry limit (default [`DEFAULT_MAX_RETRIES`]).
    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// Process a single Pending task if one exists. Returns `Ok(true)` if a
    /// task was run, `Ok(false)` if the queue had no pending work.
    ///
    /// This blocks until the task finishes (Completed/Failed/Cancelled). The
    /// pump guarantees no other task is started concurrently — it is the only
    /// caller of [`TaskQueue::start_task`] for its queue.
    pub async fn pump_once(&self) -> QueueResult<bool> {
        let next = match self.queue.next_pending().await {
            Some(t) => t,
            None => return Ok(false),
        };

        let _token = self.queue.start_task(&next.id).await?;
        let (progress_tx, mut progress_rx) = broadcast::channel::<ProgressEvent>(32);

        // Spawn the executor and drive progress updates serially.
        let exec = Arc::clone(&self.executor);
        let task = next.clone();
        let handle = tokio::spawn(async move {
            exec.execute(&task, progress_tx).await
        });

        // Forward progress events to the queue while the task runs.
        while let Ok(ev) = progress_rx.recv().await {
            let _ = self
                .queue
                .update_progress(&ev.task_id, ev.transferred, ev.total)
                .await;
            if handle.is_finished() {
                break;
            }
        }

        let outcome = handle.await.map_err(|e| QueueError::Other(e.to_string()))?;
        match outcome {
            Ok(()) => {
                self.queue.mark_completed(&next.id).await?;
            }
            Err(QueueError::Cancelled) => {
                self.queue.cancel(&next.id).await?;
            }
            Err(e) => {
                let is_cancel = e.to_string().to_lowercase().contains("cancel");
                let task = self
                    .queue
                    .list()
                    .await
                    .into_iter()
                    .find(|t| t.id == next.id);
                let should_retry = !is_cancel
                    && task
                        .as_ref()
                        .map(|t| TaskQueue::should_auto_retry(t, self.max_retries))
                        .unwrap_or(false);
                self.queue
                    .mark_failed(&next.id, e.to_string())
                    .await?;
                if should_retry {
                    // Exponential backoff, then reset to Pending for the next pump.
                    let delay = TaskQueue::retry_delay(next.attempts);
                    tokio::time::sleep(delay).await;
                    self.queue.retry(&next.id).await?;
                }
            }
        }
        Ok(true)
    }

    /// Drain the queue serially: run `pump_once` until no pending work remains.
    pub async fn drain(&self) -> QueueResult<()> {
        while self.pump_once().await? {}
        Ok(())
    }

    /// The number of tasks currently in the Active state. For a serial pump
    /// driven correctly this is always 0 or 1 — exposed for assertions/tests.
    pub async fn active_count(&self) -> usize {
        self.queue.list_by_status(TaskStatus::Active).await.len()
    }
}

#[cfg(test)]
mod serial_pump_tests {
    use super::*;
    use chrono::Utc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use wdmm_core::models::TaskKind;

    /// Executor that records how many instances are running concurrently.
    struct CountingExecutor {
        in_flight: Arc<AtomicUsize>,
        max_concurrent: Arc<AtomicUsize>,
        duration_ms: u64,
    }

    #[async_trait]
    impl TaskExecutor for CountingExecutor {
        async fn execute(
            &self,
            task: &QueueTask,
            _progress_tx: broadcast::Sender<ProgressEvent>,
        ) -> QueueResult<()> {
            let cur = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_concurrent.fetch_max(cur, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(self.duration_ms)).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            let _ = task;
            Ok(())
        }
    }

    fn make_task(id: &str) -> QueueTask {
        QueueTask {
            id: id.into(),
            kind: TaskKind::DownloadTrack,
            status: TaskStatus::Pending,
            progress: 0.0,
            total_bytes: 1000,
            transferred_bytes: 0,
            created: Utc::now(),
            updated: Utc::now(),
            attempts: 0,
            error: None,
            remote_path: Some(format!("/m/{id}.mp3")),
            local_path: None,
        }
    }

    /// The core invariant: even with many tasks, only one runs at a time.
    #[tokio::test]
    async fn serial_only_one_active_at_a_time() {
        let queue = Arc::new(TaskQueue::new());
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));
        let executor = Arc::new(CountingExecutor {
            in_flight: Arc::clone(&in_flight),
            max_concurrent: Arc::clone(&max_concurrent),
            duration_ms: 30,
        });

        for i in 0..5 {
            queue.enqueue(make_task(&format!("t{i}"))).await.unwrap();
        }

        let pump = SerialPump::new(Arc::clone(&queue), executor);
        pump.drain().await.unwrap();

        assert_eq!(max_concurrent.load(Ordering::SeqCst), 1, "never more than one");
        assert_eq!(in_flight.load(Ordering::SeqCst), 0, "all finished");
        let done = queue.list().await;
        assert_eq!(done.len(), 5);
        assert!(done.iter().all(|t| t.status == TaskStatus::Completed));
    }

    /// active_count never exceeds 1 while the pump runs.
    #[tokio::test]
    async fn active_count_never_exceeds_one() {
        let queue = Arc::new(TaskQueue::new());
        let executor = Arc::new(CountingExecutor {
            in_flight: Arc::new(AtomicUsize::new(0)),
            max_concurrent: Arc::new(AtomicUsize::new(0)),
            duration_ms: 40,
        });
        for i in 0..3 {
            queue.enqueue(make_task(&format!("a{i}"))).await.unwrap();
        }
        let pump = SerialPump::new(Arc::clone(&queue), executor);

        let observer = Arc::clone(&queue);
        let obs = tokio::spawn(async move {
            let mut max_active = 0usize;
            for _ in 0..50 {
                max_active = max_active.max(observer.list_by_status(TaskStatus::Active).await.len());
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            max_active
        });

        pump.drain().await.unwrap();
        let max_active = obs.await.unwrap();
        assert!(max_active <= 1, "active_count peaked at {max_active}");
    }

    /// pump_once returns false when idle.
    #[tokio::test]
    async fn pump_once_idle() {
        let queue = Arc::new(TaskQueue::new());
        let executor = Arc::new(CountingExecutor {
            in_flight: Arc::new(AtomicUsize::new(0)),
            max_concurrent: Arc::new(AtomicUsize::new(0)),
            duration_ms: 1,
        });
        let pump = SerialPump::new(queue, executor);
        assert!(!pump.pump_once().await.unwrap());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use wdmm_core::models::TaskKind;

    #[tokio::test]
    async fn enqueue_and_list() {
        let q = TaskQueue::new();
        let task = QueueTask {
            id: "t1".into(),
            kind: TaskKind::DownloadTrack,
            status: TaskStatus::Pending,
            progress: 0.0,
            total_bytes: 1000,
            transferred_bytes: 0,
            created: Utc::now(),
            updated: Utc::now(),
            attempts: 0,
            error: None,
            remote_path: Some("/music/track.mp3".into()),
            local_path: None,
        };
        q.enqueue(task).await.unwrap();
        let list = q.list().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].status, TaskStatus::Pending);
    }

    #[tokio::test]
    async fn state_transitions() {
        let q = TaskQueue::new();
        let task = QueueTask {
            id: "t2".into(),
            kind: TaskKind::DownloadTrack,
            status: TaskStatus::Pending,
            progress: 0.0,
            total_bytes: 1000,
            transferred_bytes: 0,
            created: Utc::now(),
            updated: Utc::now(),
            attempts: 0,
            error: None,
            remote_path: Some("/music/track.mp3".into()),
            local_path: None,
        };
        q.enqueue(task).await.unwrap();

        // Pending → Active
        let _token = q.start_task("t2").await.unwrap();
        let task = q.list().await.into_iter().find(|t| t.id == "t2").unwrap();
        assert_eq!(task.status, TaskStatus::Active);
        assert_eq!(task.attempts, 1);

        // Update progress
        q.update_progress("t2", 500, 1000).await.unwrap();
        let task = q.list().await.into_iter().find(|t| t.id == "t2").unwrap();
        assert_eq!(task.transferred_bytes, 500);
        assert_eq!(task.progress, 0.5);

        // Active → Completed
        q.mark_completed("t2").await.unwrap();
        let task = q.list().await.into_iter().find(|t| t.id == "t2").unwrap();
        assert_eq!(task.status, TaskStatus::Completed);
        assert_eq!(task.progress, 1.0);
    }

    #[tokio::test]
    async fn cancel_task() {
        let q = TaskQueue::new();
        let task = QueueTask {
            id: "t3".into(),
            kind: TaskKind::DownloadTrack,
            status: TaskStatus::Pending,
            progress: 0.0,
            total_bytes: 1000,
            transferred_bytes: 0,
            created: Utc::now(),
            updated: Utc::now(),
            attempts: 0,
            error: None,
            remote_path: Some("/music/track.mp3".into()),
            local_path: None,
        };
        q.enqueue(task).await.unwrap();

        let token = q.start_task("t3").await.unwrap();
        q.cancel("t3").await.unwrap();

        assert!(token.is_cancelled().await);
        let task = q.list().await.into_iter().find(|t| t.id == "t3").unwrap();
        assert_eq!(task.status, TaskStatus::Cancelled);
    }

    #[tokio::test]
    async fn retry_failed_task() {
        let q = TaskQueue::new();
        let task = QueueTask {
            id: "t4".into(),
            kind: TaskKind::DownloadTrack,
            status: TaskStatus::Pending,
            progress: 0.0,
            total_bytes: 1000,
            transferred_bytes: 0,
            created: Utc::now(),
            updated: Utc::now(),
            attempts: 0,
            error: None,
            remote_path: Some("/music/track.mp3".into()),
            local_path: None,
        };
        q.enqueue(task).await.unwrap();

        q.start_task("t4").await.unwrap();
        q.mark_failed("t4", "network timeout".into()).await.unwrap();

        let task = q.list().await.into_iter().find(|t| t.id == "t4").unwrap();
        assert_eq!(task.status, TaskStatus::Failed);
        assert!(task.error.is_some());

        // Retry
        q.retry("t4").await.unwrap();
        let task = q.list().await.into_iter().find(|t| t.id == "t4").unwrap();
        assert_eq!(task.status, TaskStatus::Pending);
        assert_eq!(task.progress, 0.0);
        assert!(task.error.is_none());
    }

    #[tokio::test]
    async fn fifo_ordering() {
        let q = TaskQueue::new();
        use std::time::Duration;
        use tokio::time::sleep;

        for i in 0..3 {
            let task = QueueTask {
                id: format!("t{}", i),
                kind: TaskKind::DownloadTrack,
                status: TaskStatus::Pending,
                progress: 0.0,
                total_bytes: 1000,
                transferred_bytes: 0,
                created: Utc::now(),
                updated: Utc::now(),
                attempts: 0,
                error: None,
                remote_path: Some(format!("/track{}.mp3", i)),
                local_path: None,
            };
            q.enqueue(task).await.unwrap();
            sleep(Duration::from_millis(10)).await;
        }

        let pending = q.list_by_status(TaskStatus::Pending).await;
        assert_eq!(pending.len(), 3);
        assert_eq!(pending[0].id, "t0");
        assert_eq!(pending[1].id, "t1");
        assert_eq!(pending[2].id, "t2");
    }

    #[tokio::test]
    async fn event_notifications() {
        let q = TaskQueue::new();
        let mut rx = q.subscribe();

        let task = QueueTask {
            id: "t5".into(),
            kind: TaskKind::DownloadTrack,
            status: TaskStatus::Pending,
            progress: 0.0,
            total_bytes: 1000,
            transferred_bytes: 0,
            created: Utc::now(),
            updated: Utc::now(),
            attempts: 0,
            error: None,
            remote_path: Some("/music/track.mp3".into()),
            local_path: None,
        };
        q.enqueue(task).await.unwrap();
        q.start_task("t5").await.unwrap();

        // Progress event
        q.update_progress("t5", 500, 1000).await.unwrap();
        if let Ok(QueueEvent::Progress(p)) = rx.try_recv() {
            assert_eq!(p.task_id, "t5");
            assert_eq!(p.transferred, 500);
            assert_eq!(p.progress, 0.5);
        } else {
            panic!("Expected progress event");
        }

        // Completion event
        q.mark_completed("t5").await.unwrap();
        if let Ok(QueueEvent::Completed { task_id }) = rx.try_recv() {
            assert_eq!(task_id, "t5");
        } else {
            panic!("Expected completed event");
        }
    }
}
