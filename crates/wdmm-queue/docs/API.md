# wdmm-queue — Public API

In-memory task queue with FIFO ordering, state machine, progress events,
cancellation, and exponential-backoff retry. Concurrency is fixed at 1
(`SerialPump`) matching the Dart `TaskQueueService._pump` contract.

---

## Error types

### `enum QueueError`
`NotFound(String)`, `Storage(String)`, `AlreadyActive`, `Cancelled`, `Other(String)`.

### `type QueueResult<T> = std::result::Result<T, QueueError>`

---

## Events

### `struct ProgressEvent`
Fields: `task_id: String`, `transferred: u64`, `total: u64`, `progress: f32` (0.0–1.0).

### `enum QueueEvent`
`Progress(ProgressEvent)`, `Completed { task_id }`, `Failed { task_id, error }`,
`Cancelled { task_id }`.

---

## Cancellation

### `struct CancelToken`
Shared signal. Passed to the running executor so it can poll `is_cancelled()`.

### `fn CancelToken::new() -> Self`

### `async fn CancelToken::cancel(&self)`
Set the cancelled flag.

### `async fn CancelToken::is_cancelled(&self) -> bool`

---

## Executor trait

### `trait TaskExecutor: Send + Sync`

### `async fn execute(&self, task: &QueueTask, progress_tx: broadcast::Sender<ProgressEvent>) -> QueueResult<()>`
Implementations send progress events through `progress_tx`. Return
`Err(QueueError::Cancelled)` if the task is cancelled.

---

## Task queue

### `struct TaskQueue`
In-memory store. All operations are async and lock-protected.

### `fn TaskQueue::new() -> Self`

### `fn TaskQueue::subscribe(&self) -> broadcast::Receiver<QueueEvent>`
Subscribe to all queue events.

### `async fn TaskQueue::enqueue(&self, task: QueueTask) -> QueueResult<()>`
Append task (status forced to `Pending`), sorted by `created` (FIFO).

### `async fn TaskQueue::list(&self) -> Vec<QueueTask>`

### `async fn TaskQueue::list_by_status(&self, status: TaskStatus) -> Vec<QueueTask>`

### `async fn TaskQueue::next_pending(&self) -> Option<QueueTask>`
Peek at the first `Pending` task (FIFO).

### `async fn TaskQueue::update_status(&self, id: &str, status: TaskStatus) -> QueueResult<()>`
Errors: `NotFound`.

### `async fn TaskQueue::update_progress(&self, id: &str, transferred: u64, total: u64) -> QueueResult<()>`
Update `transferred_bytes`, `total_bytes`, `progress`; emits a `Progress` event.
Errors: `NotFound`.

### `async fn TaskQueue::mark_failed(&self, id: &str, error: String) -> QueueResult<()>`
Transition to `Failed`; emits `Failed` event.
Errors: `NotFound`.

### `async fn TaskQueue::mark_completed(&self, id: &str) -> QueueResult<()>`
Transition to `Completed`; emits `Completed` event.
Errors: `NotFound`.

### `async fn TaskQueue::cancel(&self, id: &str) -> QueueResult<()>`
Signal the cancel token (if present) and set status to `Cancelled`; emits `Cancelled`.
Errors: `NotFound`.

### `async fn TaskQueue::remove(&self, id: &str) -> QueueResult<()>`
Remove task entirely.
Errors: `NotFound`.

### `async fn TaskQueue::start_task(&self, id: &str) -> QueueResult<CancelToken>`
Transition `Pending → Active`, increment `attempts`, register a fresh `CancelToken`.
Errors: `NotFound`, `AlreadyActive` (task not `Pending`).

### `async fn TaskQueue::retry(&self, id: &str) -> QueueResult<()>`
Reset `Failed`/`Cancelled` task to `Pending`, clear progress and error.
No-op if status is not `Failed` or `Cancelled`.
Errors: `NotFound`.

### `fn TaskQueue::retry_delay(attempts: u32) -> Duration`
Exponential backoff: `2^min(attempts,5)` seconds + up to 1 s jitter.

### `fn TaskQueue::should_auto_retry(task: &QueueTask, max_retries: u32) -> bool`
`true` if `attempts < max_retries`, status is `Failed`, and the error string is
retryable (network/timeout/connection keywords). Cancel and data-corruption errors
are not retryable.

---

## Serial pump

### `const DEFAULT_MAX_RETRIES: u32 = 3`

### `struct SerialPump`
Single-worker FIFO pump. Never more than one `Active` task at any instant.

### `fn SerialPump::new(queue: Arc<TaskQueue>, executor: Arc<dyn TaskExecutor>) -> Self`

### `fn SerialPump::with_max_retries(self, max_retries: u32) -> Self`

### `async fn SerialPump::pump_once(&self) -> QueueResult<bool>`
Pick the next `Pending` task and run it to completion. Returns `Ok(true)` if a
task was executed, `Ok(false)` if the queue was empty. Blocks until the task
finishes (`Completed`/`Failed`/`Cancelled`).

### `async fn SerialPump::drain(&self) -> QueueResult<()>`
Call `pump_once` in a loop until no pending tasks remain.

### `async fn SerialPump::active_count(&self) -> usize`
Number of `Active` tasks (invariant: always 0 or 1 during a serial pump run).
