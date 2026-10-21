//! `wdmm-queue` — task-queue state machine (FIFO + resumable download stub).

#![forbid(unsafe_code)]

use async_trait::async_trait;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::RwLock;
use wdmm_core::models::{QueueTask, TaskStatus};

#[derive(Debug, Error)]
pub enum QueueError {
    #[error("task not found: {0}")]
    NotFound(String),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("other: {0}")]
    Other(String),
}

pub type QueueResult<T> = std::result::Result<T, QueueError>;

/// Task executor trait — each `TaskKind` has an executor.
#[async_trait]
pub trait TaskExecutor: Send + Sync {
    async fn execute(&self, task: &QueueTask) -> QueueResult<()>;
}

/// A simple in-memory queue (stub). Full impl will persist to `wdmm-storage`.
pub struct TaskQueue {
    tasks: Arc<RwLock<Vec<QueueTask>>>,
}

impl TaskQueue {
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub async fn enqueue(&self, task: QueueTask) -> QueueResult<()> {
        let mut tasks = self.tasks.write().await;
        tasks.push(task);
        Ok(())
    }

    pub async fn list(&self) -> Vec<QueueTask> {
        self.tasks.read().await.clone()
    }

    pub async fn update_status(&self, id: &str, status: TaskStatus) -> QueueResult<()> {
        let mut tasks = self.tasks.write().await;
        if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
            t.status = status;
            Ok(())
        } else {
            Err(QueueError::NotFound(id.to_string()))
        }
    }

    pub async fn remove(&self, id: &str) -> QueueResult<()> {
        let mut tasks = self.tasks.write().await;
        if let Some(pos) = tasks.iter().position(|t| t.id == id) {
            tasks.remove(pos);
            Ok(())
        } else {
            Err(QueueError::NotFound(id.to_string()))
        }
    }
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
    use wdmm_core::models::{TaskKind, TaskStatus};

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
        };
        q.enqueue(task).await.unwrap();
        let list = q.list().await;
        assert_eq!(list.len(), 1);
    }
}
