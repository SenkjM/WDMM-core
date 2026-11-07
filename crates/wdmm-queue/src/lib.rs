//! `wdmm-queue` — the single authority for WDMM's durable task queue.
//!
//! The Flutter layer no longer owns queue state: this crate keeps the model,
//! the scheduler, retry policy, batch lifecycle, event ring and row folding.
//! Jobs (the code that actually moves bytes or touches the library) live in
//! `wdmm-jobs` and are plugged in through [`handle::TaskRunner`]; persistence
//! lives in `wdmm-storage` schema v17 behind [`store::TaskStore`].
//!
//! # Public API
//!
//! Singleton access and commands ([`handle::QueueHandle`]):
//!
//! ```text
//! QueueHandle::new(store: Arc<dyn TaskStore>, runner: Arc<dyn TaskRunner>) -> QueueHandle
//! QueueHandle::install_global(handle: QueueHandle) -> bool
//! QueueHandle::global() -> Option<QueueHandle>
//! QueueHandle::try_global() -> Option<QueueHandle>          // same, for FFI guards
//! handle.rev() -> u64
//! handle.snapshot() -> Snapshot
//! handle.tasks() -> Vec<QueueTask>
//! handle.batches() -> Vec<QueueBatch>
//! handle.task(id: &str) -> Option<QueueTask>
//! handle.by_remote(account_id: &str, remote_path: &str) -> Vec<QueueTask>
//! handle.rows() -> Vec<QueueRow>
//! handle.stats() -> QueueStats
//! handle.events(since_seq: u64) -> EventsPage
//! handle.enqueue(req: EnqueueRequest) -> QueueResult<EnqueueResult>
//! handle.cancel(task_id: &str) -> QueueResult<bool>
//! handle.cancel_all() -> QueueResult<usize>
//! handle.retry(task_id: &str) -> QueueResult<QueueTask>
//! handle.retry_all() -> QueueResult<usize>
//! handle.remove(task_id: &str) -> QueueResult<bool>
//! handle.clear_completed() -> QueueResult<usize>
//! handle.clear_all() -> QueueResult<usize>
//! handle.set_concurrency(n: usize) -> QueueResult<()>
//! handle.concurrency() -> usize
//! handle.set_notifications(enabled: bool)
//! handle.set_keep_alive(enabled: bool)
//! handle.set_background(background: bool)
//! handle.fgs_timeout() -> QueueResult<()>
//! handle.platform_reply(req_id: &str, result: PlatformResult) -> bool
//! handle.pump() -> impl Future<Output = ()>                // scheduler loop
//! handle.kick()                                           // spawn a pump turn
//! handle.shutdown()                                       // cancel timers, stop pumping
//! handle.wait(task_id: &str, timeout_ms: u64) -> impl Future<Output = WaitOutcome>
//! handle.wait_blocking(task_id: &str, timeout_ms: u64) -> WaitOutcome
//! ```
//!
//! Execution seam ([`handle::TaskRunner`], implemented by the FFI adapter around
//! `wdmm_jobs::JobRegistry`):
//!
//! ```text
//! trait TaskRunner: Send + Sync {
//!     fn supports(&self, kind: TaskKind) -> bool;
//!     async fn run(&self, ctx: RunCtx) -> RunOutcome;
//! }
//! struct RunCtx { task: QueueTask, cancel: CancelToken,
//!                 progress: Arc<dyn ProgressSink>, queue: QueueHandle }
//! impl RunCtx { fn cancelled(&self) -> bool;
//!                async fn request_platform(&self, action: &str, params_json: &str)
//!                    -> Result<PlatformResult, PlatformError> }
//! enum RunOutcome { Completed, Partial { result_json: String },
//!                   Failed { error: String, category: FailureCategory }, Cancelled }
//! trait ProgressSink: Send + Sync {
//!     fn bytes(&self, done: u64, total: Option<u64>);
//!     fn units(&self, done: u32, total: Option<u32>);
//!     fn note(&self, message: &str);
//! }
//! ```
//!
//! Persistence ([`store`]):
//!
//! ```text
//! trait TaskStore: Send + Sync {
//!     fn load(&self) -> Result<LoadedQueue, StoreError>;
//!     fn save_task(&self, task: &QueueTask) -> Result<(), StoreError>;
//!     fn delete_task(&self, id: &str) -> Result<(), StoreError>;
//!     fn delete_tasks(&self, ids: &[String]) -> Result<(), StoreError>;
//!     fn delete_all_tasks(&self) -> Result<(), StoreError>;
//!     fn save_batch(&self, batch: &QueueBatch) -> Result<(), StoreError>;
//!     fn delete_batch(&self, id: &str) -> Result<(), StoreError>;
//!     fn delete_all_batches(&self) -> Result<(), StoreError>;
//!     fn write_payload(&self, task_id: &str, contents: &str) -> Result<String, StoreError>;
//!     fn read_payload(&self, task_id: &str) -> Result<Option<String>, StoreError>;
//!     fn delete_payload(&self, task_id: &str) -> Result<(), StoreError>;
//! }
//! DbStore::new(db: Arc<LibraryDatabase>, payload_dir: impl Into<PathBuf>) -> DbStore
//! MemStore::new() -> MemStore                                  // tests / headless handles
//! ```

#![forbid(unsafe_code)]

pub mod events;
pub mod handle;
pub mod model;
pub mod policy;
pub mod queue;
pub mod rows;
pub mod scheduler;
pub mod store;

mod followups;

#[cfg(test)]
mod tests;

pub use events::{Event, EventKind, EventsPage};
pub use handle::{
    EnqueueResult, PlatformError, PlatformResult, ProgressSink, QueueHandle, QueueStats, RunCtx,
    RunOutcome, SessionSnapshot, Snapshot, TaskRunner, WaitOutcome,
};
pub use model::{
    batch_kind_for, BatchKind, BatchOrigin, CancelToken, ProgressMode, QueueBatch, QueueTask,
    TaskKind, TaskStatus, TaskTarget,
};
pub use queue::{EnqueueRequest, QueueState};
pub use rows::QueueRow;
pub use store::{DbStore, LoadedQueue, MangaDraft, MemStore, StoreError, TaskStore};

use thiserror::Error;

/// Errors returned by queue commands.
#[derive(Debug, Error)]
pub enum QueueError {
    #[error("task not found: {0}")]
    NotFound(String),

    #[error("queue storage error: {0}")]
    Storage(String),

    #[error("invalid request: {0}")]
    Invalid(String),

    #[error("queue is shutting down")]
    ShuttingDown,
}

pub type QueueResult<T> = std::result::Result<T, QueueError>;

/// Current wall clock in epoch milliseconds (the unit used by every queue row).
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Random, opaque id (32 hex chars). Task/batch ids are only ever compared as
/// strings, so a UUID-shaped random value is enough and avoids a `uuid` dep.
pub fn new_id() -> String {
    format!("{:032x}", rand::random::<u128>())
}
