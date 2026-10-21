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
