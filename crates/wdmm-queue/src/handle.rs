//! `QueueHandle`: the process-wide queue singleton and its command surface.
//!
//! Construction takes a [`TaskStore`] and a [`TaskRunner`]; the FFI layer
//! installs one handle through [`QueueHandle::install_global`] and every later
//! call reaches it through [`QueueHandle::global`]. Dart holds no state: it
//! issues commands, reads [`Snapshot`]/[`QueueRow`]s and consumes
//! [`crate::events::Event`]s.
//!
//! Lock order is always `state` → `hub`/`running`/`store`; never the reverse.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Serialize;
use tokio::sync::{oneshot, Notify};

use crate::events::{EventHub, EventKind, EventsPage};
use crate::model::{CancelToken, QueueBatch, QueueTask, TaskKind, TaskStatus};
use crate::policy::FailureCategory;
use crate::queue::{CancelOutcome, EnqueueRequest, QueueState};
use crate::rows::{fold_rows, QueueRow};
use crate::scheduler;
use crate::store::TaskStore;
use crate::{new_id, now_ms, QueueError, QueueResult};

/// How long a job may wait for the Dart/Kotlin platform bridge before the task
/// fails with `err.platformTimeout`.
pub const PLATFORM_TIMEOUT: Duration = Duration::from_secs(300);

/// FGS recovery cooldown: a foreground-service timeout parks the session.
pub const FGS_COOLDOWN_MS: i64 = 30_000;

/// Progress throttle for persistence (contract §3): state writes are immediate,
/// progress is written at most twice per second.
pub const PROGRESS_PERSIST_INTERVAL: Duration = Duration::from_millis(500);

static GLOBAL: OnceLock<QueueHandle> = OnceLock::new();

/// Outcome reported by a [`TaskRunner`].
#[derive(Debug, Clone)]
pub enum RunOutcome {
    Completed,
    Partial {
        result_json: String,
    },
    Failed {
        error: String,
        category: FailureCategory,
    },
    Cancelled,
}

/// Result of one platform action, as answered by Dart.
#[derive(Debug, Clone, Serialize)]
pub struct PlatformResult {
    #[serde(default)]
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_json: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl PlatformResult {
    pub fn ok(result_json: Option<String>) -> Self {
        Self {
            ok: true,
            result_json,
            error: None,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            result_json: None,
            error: Some(message.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformError {
    /// Dart did not answer within [`PLATFORM_TIMEOUT`].
    Timeout,
    /// The task was cancelled while waiting.
    Cancelled,
    /// The queue is shutting down.
    Shutdown,
}

impl std::fmt::Display for PlatformError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlatformError::Timeout => f.write_str("platform request timed out"),
            PlatformError::Cancelled => f.write_str("platform request cancelled"),
            PlatformError::Shutdown => f.write_str("queue is shutting down"),
        }
    }
}

impl std::error::Error for PlatformError {}

/// Progress channel handed to a running job.
pub trait ProgressSink: Send + Sync {
    fn bytes(&self, done: u64, total: Option<u64>);
    fn units(&self, done: u32, total: Option<u32>);
    /// Free-form note; the current UI has no slot for it, so it is ignored.
    fn note(&self, message: &str);

    /// The job produced a local file worth remembering (`local_path` column).
    ///
    /// Jobs call this **before** any step that can outlive the process — the
    /// platform hand-off in particular — so a restart can tell the file is
    /// already there instead of fetching it again.
    fn local_path(&self, _path: &str) {}

    /// A result payload for a task that still ends `Completed` (follow-up
    /// target, pack archive, …).
    fn result(&self, _json: &str) {}
}

/// Everything a job needs from the queue while it runs.
pub struct RunCtx {
    pub task: QueueTask,
    pub cancel: CancelToken,
    pub progress: Arc<dyn ProgressSink>,
    pub queue: QueueHandle,
}

impl RunCtx {
    pub fn cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Ask Dart/Kotlin to perform a platform action (SAF save, materialise a
    /// content URI, …). The task shows as `awaiting_platform` until the reply
    /// arrives or [`PLATFORM_TIMEOUT`] expires.
    pub async fn request_platform(
        &self,
        action: &str,
        params_json: &str,
    ) -> Result<PlatformResult, PlatformError> {
        self.queue
            .request_platform(&self.task.id, action, params_json)
            .await
    }
}

/// The execution seam: `wdmm-jobs` is adapted to this trait by the FFI layer.
#[async_trait]
pub trait TaskRunner: Send + Sync {
    fn supports(&self, kind: TaskKind) -> bool;
    async fn run(&self, ctx: RunCtx) -> RunOutcome;
}

/// Result of `wait`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitOutcome {
    Terminal { status: TaskStatus },
    Timeout,
    NotFound,
}

impl WaitOutcome {
    pub fn is_terminal(self) -> bool {
        matches!(self, WaitOutcome::Terminal { .. })
    }
}

/// Authoritative queue snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub rev: u64,
    pub tasks: Vec<QueueTask>,
    pub batches: Vec<QueueBatch>,
    pub session: SessionSnapshot,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionSnapshot {
    pub drained: bool,
    pub scrape_only: bool,
    pub background: bool,
    pub keepalive: KeepaliveSnapshot,
    pub notifications: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct KeepaliveSnapshot {
    pub phase: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// Counters used by the queue badge.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct QueueStats {
    pub pending: u32,
    pub active: u32,
    pub awaiting_platform: u32,
    pub completed: u32,
    pub partial: u32,
    pub failed: u32,
    pub cancelled: u32,
    pub total: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnqueueResult {
    pub task_id: String,
    pub deduped: bool,
    pub batch_id: String,
}

#[derive(Debug)]
pub(crate) struct RunningTask {
    pub token: CancelToken,
    /// `fgs_timeout` asked the job to stop, but the task must return to
    /// `pending` instead of failing.
    pub cancel_to_pending: bool,
}

pub(crate) struct QueueInner {
    pub state: Mutex<QueueState>,
    pub hub: Mutex<EventHub>,
    pub running: Mutex<HashMap<String, RunningTask>>,
    pub platform: Mutex<HashMap<String, oneshot::Sender<PlatformResult>>>,
    /// task id → outstanding platform request id.
    pub platform_tasks: Mutex<HashMap<String, String>>,
    pub store: Arc<dyn TaskStore>,
    pub runner: Arc<dyn TaskRunner>,
    pub runtime: Option<tokio::runtime::Handle>,
    pub notify: Notify,
    pub pumping: AtomicBool,
    pub shutdown: AtomicBool,
    pub last_store_error: Mutex<Option<String>>,
}

/// Cheap, cloneable handle to the queue.
#[derive(Clone)]
pub struct QueueHandle {
    pub(crate) inner: Arc<QueueInner>,
}

impl std::fmt::Debug for QueueHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueueHandle")
            .field("rev", &self.rev())
            .finish()
    }
}

impl QueueHandle {
    /// Build the queue and resume whatever the database still held.
    ///
    /// A load failure yields an empty queue; use [`QueueHandle::try_new`] when
    /// the caller wants to surface it (the FFI does).
    pub fn new(store: Arc<dyn TaskStore>, runner: Arc<dyn TaskRunner>) -> Self {
        Self::try_new(Arc::clone(&store), Arc::clone(&runner))
            .unwrap_or_else(|_| Self::empty(store, runner))
    }

    pub fn try_new(store: Arc<dyn TaskStore>, runner: Arc<dyn TaskRunner>) -> QueueResult<Self> {
        let loaded = store
            .load()
            .map_err(|e| QueueError::Storage(e.to_string()))?;
        let now = now_ms();
        let mut state = QueueState::from_loaded(loaded, now);

        // Restart rules: active/awaiting tasks are now pending with their
        // counters intact, and stale open batches that are already done close.
        let tasks = state.tasks_sorted();
        let mut startup_error = None;
        for task in &tasks {
            if let Err(e) = store.save_task(task) {
                startup_error = Some(e.to_string());
            }
        }
        let closed = state.close_finished_batches(now);
        for batch in &closed.batches {
            if let Err(e) = store.save_batch(batch) {
                startup_error = Some(e.to_string());
            }
        }
        let pruned = state.prune_silent_closed();
        for id in &pruned.task_ids {
            let _ = store.delete_task(id);
            let _ = store.delete_payload(id);
        }
        for id in &pruned.batch_ids {
            let _ = store.delete_batch(id);
        }

        Ok(Self {
            inner: Arc::new(QueueInner {
                state: Mutex::new(state),
                hub: Mutex::new(EventHub::new()),
                running: Mutex::new(HashMap::new()),
                platform: Mutex::new(HashMap::new()),
                platform_tasks: Mutex::new(HashMap::new()),
                store,
                runner,
                runtime: tokio::runtime::Handle::try_current().ok(),
                notify: Notify::new(),
                pumping: AtomicBool::new(false),
                shutdown: AtomicBool::new(false),
                last_store_error: Mutex::new(startup_error),
            }),
        })
    }

    fn empty(store: Arc<dyn TaskStore>, runner: Arc<dyn TaskRunner>) -> Self {
        let runtime = tokio::runtime::Handle::try_current().ok();
        Self {
            inner: Arc::new(QueueInner {
                state: Mutex::new(QueueState::new()),
                hub: Mutex::new(EventHub::new()),
                running: Mutex::new(HashMap::new()),
                platform: Mutex::new(HashMap::new()),
                platform_tasks: Mutex::new(HashMap::new()),
                store,
                runner,
                runtime,
                notify: Notify::new(),
                pumping: AtomicBool::new(false),
                shutdown: AtomicBool::new(false),
                last_store_error: Mutex::new(None),
            }),
        }
    }

    /// Install the process-wide handle. Returns `false` when one already exists.
    pub fn install_global(handle: QueueHandle) -> bool {
        GLOBAL.set(handle).is_ok()
    }

    pub fn global() -> Option<QueueHandle> {
        GLOBAL.get().cloned()
    }

    /// Alias for [`QueueHandle::global`] used by FFI guards.
    pub fn try_global() -> Option<QueueHandle> {
        GLOBAL.get().cloned()
    }

    pub fn last_store_error(&self) -> Option<String> {
        self.inner.last_store_error.lock().ok()?.clone()
    }

    // ------------------------------------------------------------- reads

    pub fn rev(&self) -> u64 {
        self.inner.hub.lock().map(|hub| hub.rev()).unwrap_or(0)
    }

    pub fn task(&self, id: &str) -> Option<QueueTask> {
        self.inner.state.lock().ok()?.task(id).cloned()
    }

    pub fn tasks(&self) -> Vec<QueueTask> {
        self.inner
            .state
            .lock()
            .map(|state| state.tasks_sorted())
            .unwrap_or_default()
    }

    pub fn batches(&self) -> Vec<QueueBatch> {
        self.inner
            .state
            .lock()
            .map(|state| state.batches_sorted())
            .unwrap_or_default()
    }

    pub fn by_remote(&self, account_id: &str, remote_path: &str) -> Vec<QueueTask> {
        self.inner
            .state
            .lock()
            .map(|state| state.by_remote(account_id, remote_path))
            .unwrap_or_default()
    }

    pub fn rows(&self) -> Vec<QueueRow> {
        let Ok(state) = self.inner.state.lock() else {
            return Vec::new();
        };
        let tasks = state.tasks_sorted();
        let batches = state.batches_sorted();
        drop(state);
        fold_rows(&tasks, &batches)
    }

    pub fn snapshot(&self) -> Snapshot {
        let (tasks, batches, session) = {
            let state = match self.inner.state.lock() {
                Ok(state) => state,
                Err(_) => {
                    return Snapshot {
                        rev: self.rev(),
                        tasks: Vec::new(),
                        batches: Vec::new(),
                        session: SessionSnapshot {
                            drained: true,
                            scrape_only: false,
                            background: false,
                            keepalive: KeepaliveSnapshot {
                                phase: "idle".into(),
                                last_error: None,
                            },
                            notifications: true,
                        },
                    };
                }
            };
            let tasks = state.tasks_sorted();
            let scrape_only = !tasks.is_empty() && tasks.iter().all(|t| t.kind.is_scrape_like());
            let session = SessionSnapshot {
                drained: state.is_drained(),
                scrape_only,
                background: state.background(),
                keepalive: KeepaliveSnapshot {
                    phase: state.keepalive_phase().to_string(),
                    last_error: state.keepalive_last_error().map(str::to_string),
                },
                notifications: state.notifications(),
            };
            (tasks, state.batches_sorted(), session)
        };
        Snapshot {
            rev: self.rev(),
            tasks,
            batches,
            session,
        }
    }

    pub fn stats(&self) -> QueueStats {
        let Ok(state) = self.inner.state.lock() else {
            return QueueStats::default();
        };
        let mut stats = QueueStats::default();
        for task in state.tasks_sorted() {
            stats.total += 1;
            match task.status {
                TaskStatus::Pending => stats.pending += 1,
                TaskStatus::Active => stats.active += 1,
                TaskStatus::AwaitingPlatform => stats.awaiting_platform += 1,
                TaskStatus::Completed => stats.completed += 1,
                TaskStatus::Partial => stats.partial += 1,
                TaskStatus::Failed => stats.failed += 1,
                TaskStatus::Cancelled => stats.cancelled += 1,
            }
        }
        stats
    }

    pub fn events(&self, since_seq: u64) -> EventsPage {
        self.inner
            .hub
            .lock()
            .map(|hub| hub.events_since(since_seq))
            .unwrap_or(EventsPage {
                rev: 0,
                events: Vec::new(),
                next_seq: 1,
            })
    }

    pub fn concurrency(&self) -> usize {
        self.inner
            .state
            .lock()
            .map(|state| state.concurrency())
            .unwrap_or(1)
    }

    // ---------------------------------------------------------- commands

    pub fn enqueue(&self, req: EnqueueRequest) -> QueueResult<EnqueueResult> {
        if self.inner.shutdown.load(Ordering::SeqCst) {
            return Err(QueueError::ShuttingDown);
        }
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| QueueError::Storage("queue lock poisoned".into()))?;
        let enqueued = state.enqueue(req.clone());
        drop(state);

        let task_id = enqueued.task.id.clone();
        let batch_id = enqueued.batch.id.clone();
        if enqueued.deduped {
            return Ok(EnqueueResult {
                task_id,
                deduped: true,
                batch_id,
            });
        }

        let mut task = enqueued.task;
        if let Some(payload) = &req.payload {
            match self.inner.store.write_payload(&task.id, payload) {
                Ok(path) => {
                    // Jobs read `task.payload_path`, and the scheduler starts the
                    // row from the in-memory state, so the sidecar path must land
                    // in both the store and the running-row snapshot.
                    task.payload_path = Some(path.clone());
                    if let Ok(mut state) = self.inner.state.lock() {
                        if let Some(stored) = state.task_mut(&task.id) {
                            stored.payload_path = Some(path);
                        }
                    }
                }
                Err(e) => self.record_store_error(e.to_string()),
            }
        }
        self.persist_task(&task);
        if enqueued.created_batch {
            self.persist_batch(&enqueued.batch);
        }
        if !enqueued.batch.silent {
            if enqueued.created_batch {
                self.emit(EventKind::BatchUpdated {
                    batch: enqueued.batch.clone(),
                });
            }
            self.emit(EventKind::TaskAdded { task: task.clone() });
        }
        self.settle();
        self.kick();
        Ok(EnqueueResult {
            task_id: task.id,
            deduped: false,
            batch_id,
        })
    }

    /// Cancel one task. Pending/awaiting tasks are terminal immediately; active
    /// ones are asked to unwind. Returns `false` when the task was already
    /// finished.
    pub fn cancel(&self, task_id: &str) -> QueueResult<bool> {
        let now = now_ms();
        let outcome = {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| QueueError::Storage("queue lock poisoned".into()))?;
            state
                .cancel(task_id, now)
                .ok_or_else(|| QueueError::NotFound(task_id.to_string()))?
        };
        match outcome {
            CancelOutcome::Terminal(task) => {
                self.persist_task(&task);
                self.abort_platform(task_id);
                self.emit_terminal(&task);
                self.settle();
                Ok(true)
            }
            CancelOutcome::Requested => {
                self.flag_cancel(task_id, false);
                self.abort_platform(task_id);
                Ok(true)
            }
            CancelOutcome::AlreadyTerminal => Ok(false),
        }
    }

    pub fn cancel_all(&self) -> QueueResult<usize> {
        let now = now_ms();
        let (cancelled, active_ids): (Vec<QueueTask>, Vec<String>) = {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| QueueError::Storage("queue lock poisoned".into()))?;
            let active_ids = state
                .tasks_sorted()
                .into_iter()
                .filter(|t| matches!(t.status, TaskStatus::Active))
                .map(|t| t.id)
                .collect();
            (state.cancel_all(now), active_ids)
        };
        for id in &active_ids {
            self.flag_cancel(id, false);
        }
        for task in &cancelled {
            self.persist_task(task);
            self.abort_platform(&task.id);
            self.emit_terminal(task);
        }
        self.settle();
        Ok(cancelled.len() + active_ids.len())
    }

    pub fn retry(&self, task_id: &str) -> QueueResult<QueueTask> {
        let now = now_ms();
        let task = {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| QueueError::Storage("queue lock poisoned".into()))?;
            state.retry(task_id, now)?
        };
        self.persist_task(&task);
        self.emit_task_added(&task);
        self.kick();
        Ok(task)
    }

    pub fn retry_all(&self) -> QueueResult<usize> {
        let now = now_ms();
        let tasks = {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| QueueError::Storage("queue lock poisoned".into()))?;
            state.retry_all(now)
        };
        for task in &tasks {
            self.persist_task(task);
            self.emit_task_added(task);
        }
        self.kick();
        Ok(tasks.len())
    }

    pub fn remove(&self, task_id: &str) -> QueueResult<bool> {
        let was_active = self.flag_cancel(task_id, false);
        let removed = {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| QueueError::Storage("queue lock poisoned".into()))?;
            state.remove(task_id)
        };
        let Some(task) = removed else {
            return Ok(false);
        };
        self.abort_platform(task_id);
        if was_active {
            // Let the runner notice before its row disappears.
            self.flag_cancel(task_id, false);
        }
        let _ = self.inner.store.delete_task(&task.id);
        let _ = self.inner.store.delete_payload(&task.id);
        self.settle();
        self.kick();
        Ok(true)
    }

    pub fn clear_completed(&self) -> QueueResult<usize> {
        let ids = {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| QueueError::Storage("queue lock poisoned".into()))?;
            state.clear_completed()
        };
        for id in &ids {
            let _ = self.inner.store.delete_task(id);
            let _ = self.inner.store.delete_payload(id);
        }
        self.settle();
        Ok(ids.len())
    }

    pub fn clear_all(&self) -> QueueResult<usize> {
        let now = now_ms();
        let before = self.tasks();
        let ids: Vec<String> = before.iter().map(|t| t.id.clone()).collect();
        let active_ids: Vec<String> = before
            .iter()
            .filter(|t| matches!(t.status, TaskStatus::Active))
            .map(|t| t.id.clone())
            .collect();
        for id in &active_ids {
            self.flag_cancel(id, false);
        }
        {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| QueueError::Storage("queue lock poisoned".into()))?;
            state.clear_all(now);
        }
        let _ = self.inner.store.delete_all_tasks();
        let _ = self.inner.store.delete_all_batches();
        for id in &ids {
            let _ = self.inner.store.delete_payload(id);
        }
        self.settle();
        self.kick();
        Ok(ids.len())
    }

    pub fn set_concurrency(&self, concurrency: usize) -> QueueResult<()> {
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| QueueError::Storage("queue lock poisoned".into()))?;
        state.set_concurrency(concurrency);
        drop(state);
        self.kick();
        Ok(())
    }

    pub fn set_notifications(&self, enabled: bool) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.set_notifications(enabled);
        }
    }

    pub fn set_keep_alive(&self, enabled: bool) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.set_keep_alive(enabled);
        }
        self.emit(EventKind::KeepaliveHint {
            phase: if enabled { "start" } else { "release" }.to_string(),
        });
    }

    pub fn note_keepalive(&self, phase: &str, error: Option<String>) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.note_keepalive(phase, error);
        }
    }

    pub fn set_background(&self, background: bool) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.set_background(background);
        }
    }

    /// Foreground service died: park the session for [`FGS_COOLDOWN_MS`] and ask
    /// running jobs to unwind into `pending` (not `failed`).
    pub fn fgs_timeout(&self) -> QueueResult<()> {
        let now = now_ms();
        let until = now + FGS_COOLDOWN_MS;
        {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| QueueError::Storage("queue lock poisoned".into()))?;
            state.suspend_until(until);
            let ids = state
                .tasks_sorted()
                .into_iter()
                .filter(|t| matches!(t.status, TaskStatus::Active))
                .map(|t| t.id)
                .collect::<Vec<_>>();
            for id in ids {
                if let Ok(mut running) = self.inner.running.lock() {
                    if let Some(slot) = running.get_mut(&id) {
                        slot.cancel_to_pending = true;
                        slot.token.cancel();
                    }
                }
            }
        }
        self.emit(EventKind::FgsTimeoutHint);
        Ok(())
    }

    pub fn platform_reply(&self, req_id: &str, result: PlatformResult) -> bool {
        let sender = self
            .inner
            .platform
            .lock()
            .ok()
            .and_then(|mut map| map.remove(req_id));
        let Some(sender) = sender else {
            return false;
        };
        self.inner
            .platform_tasks
            .lock()
            .map(|mut map| map.retain(|_, req| req != req_id))
            .ok();
        sender.send(result).is_ok()
    }

    async fn request_platform(
        &self,
        task_id: &str,
        action: &str,
        params_json: &str,
    ) -> Result<PlatformResult, PlatformError> {
        if self.inner.shutdown.load(Ordering::SeqCst) {
            return Err(PlatformError::Shutdown);
        }
        let req_id = new_id();
        let (tx, rx) = oneshot::channel();
        self.inner
            .platform
            .lock()
            .map_err(|_| PlatformError::Cancelled)?
            .insert(req_id.clone(), tx);
        self.inner
            .platform_tasks
            .lock()
            .map_err(|_| PlatformError::Cancelled)?
            .insert(task_id.to_string(), req_id.clone());

        if let Ok(mut state) = self.inner.state.lock() {
            if let Some(task) = state.task_mut(task_id) {
                task.status = TaskStatus::AwaitingPlatform;
                task.updated_at = now_ms();
                let task = task.clone();
                drop(state);
                self.persist_task(&task);
                if !self.batch_is_silent(&task.batch_id) {
                    self.emit(EventKind::PlatformRequest {
                        req_id: req_id.clone(),
                        action: action.to_string(),
                        params_json: params_json.to_string(),
                    });
                }
            }
        }

        match tokio::time::timeout(PLATFORM_TIMEOUT, rx).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(_)) => Err(PlatformError::Cancelled),
            Err(_) => {
                if let Ok(mut map) = self.inner.platform.lock() {
                    map.remove(&req_id);
                }
                if let Ok(mut map) = self.inner.platform_tasks.lock() {
                    map.remove(task_id);
                }
                Err(PlatformError::Timeout)
            }
        }
    }

    // --------------------------------------------------------- scheduler

    /// The pump: start runnable tasks until the concurrency slots are full, then
    /// wait for a completion or the next retry deadline.
    pub async fn pump(&self) {
        loop {
            if self.inner.shutdown.load(Ordering::SeqCst) {
                return;
            }
            let now = now_ms();
            let (active, concurrency, next, retry_at) = {
                let Ok(state) = self.inner.state.lock() else {
                    return;
                };
                (
                    state.active_count(),
                    state.concurrency(),
                    state.next_runnable(now),
                    state.earliest_retry_at(),
                )
            };

            if active < concurrency {
                if let Some(id) = next {
                    scheduler::start_task(self.clone(), id);
                    continue;
                }
            }

            if active == 0 && retry_at.is_none() {
                // Idle: a later `kick` restarts the pump.
                return;
            }

            let wait_ms = retry_at
                .map(|at| ((at - now).max(0) as u64).min(1_000))
                .unwrap_or(1_000);
            tokio::select! {
                _ = self.inner.notify.notified() => {}
                _ = tokio::time::sleep(Duration::from_millis(wait_ms)) => {}
            }
        }
    }

    /// Spawn one pump turn when a runtime is available.
    pub fn kick(&self) {
        if self.inner.runtime.is_none() || self.inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        if self.inner.pumping.swap(true, Ordering::SeqCst) {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            this.pump().await;
            this.inner.pumping.store(false, Ordering::SeqCst);
        });
    }

    pub fn shutdown(&self) {
        self.inner.shutdown.store(true, Ordering::SeqCst);
        self.inner.notify.notify_waiters();
        if let Ok(mut map) = self.inner.platform.lock() {
            map.clear();
        }
    }

    /// Wait until a task reaches a terminal status.
    pub async fn wait(&self, task_id: &str, timeout_ms: u64) -> WaitOutcome {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            match self.task(task_id) {
                None => return WaitOutcome::NotFound,
                Some(task) if task.status.is_terminal() => {
                    return WaitOutcome::Terminal {
                        status: task.status,
                    };
                }
                Some(_) => {}
            }
            if Instant::now() >= deadline {
                return WaitOutcome::Timeout;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Blocking variant for synchronous FFI calls.
    ///
    /// Never parks the runtime: the caller may be the only worker thread (the
    /// FFI entry point often is), so this polls instead of `block_on`.
    pub fn wait_blocking(&self, task_id: &str, timeout_ms: u64) -> WaitOutcome {
        self.wait_polling(task_id, timeout_ms)
    }

    fn wait_polling(&self, task_id: &str, timeout_ms: u64) -> WaitOutcome {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            match self.task(task_id) {
                None => return WaitOutcome::NotFound,
                Some(task) if task.status.is_terminal() => {
                    return WaitOutcome::Terminal {
                        status: task.status,
                    };
                }
                Some(_) => {}
            }
            if Instant::now() >= deadline {
                return WaitOutcome::Timeout;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    // ------------------------------------------------------------ internals

    pub(crate) fn persist_task(&self, task: &QueueTask) {
        if let Err(e) = self.inner.store.save_task(task) {
            self.record_store_error(e.to_string());
        }
    }

    pub(crate) fn persist_batch(&self, batch: &QueueBatch) {
        if let Err(e) = self.inner.store.save_batch(batch) {
            self.record_store_error(e.to_string());
        }
    }

    pub(crate) fn emit(&self, kind: EventKind) {
        if let Ok(mut hub) = self.inner.hub.lock() {
            hub.push(kind);
        }
        self.inner.notify.notify_one();
    }

    /// Emit `task_added` unless the task belongs to a silent (auto/system)
    /// batch, whose members never surface individual events.
    pub(crate) fn emit_task_added(&self, task: &QueueTask) {
        if !self.batch_is_silent(&task.batch_id) {
            self.emit(EventKind::TaskAdded { task: task.clone() });
        }
    }

    pub(crate) fn emit_progress(&self, task_id: &str, kind: EventKind) {
        if let Ok(mut hub) = self.inner.hub.lock() {
            hub.push_progress(task_id, kind);
        }
        self.inner.notify.notify_one();
    }

    pub(crate) fn emit_terminal(&self, task: &QueueTask) {
        if !self.batch_is_silent(&task.batch_id) {
            self.emit(EventKind::TaskTerminal {
                task_id: task.id.clone(),
                status: task.status,
                error_message: task.error_message.clone(),
                local_path: task.local_path.clone(),
                local_uri: task.local_uri.clone(),
                result_json: task.result_json.clone(),
                progress: task.progress,
            });
        }
        if let Ok(mut hub) = self.inner.hub.lock() {
            hub.clear_progress(&task.id);
        }
    }

    pub(crate) fn batch_is_silent(&self, batch_id: &str) -> bool {
        self.inner
            .state
            .lock()
            .ok()
            .and_then(|state| state.batch(batch_id).map(|b| b.silent))
            .unwrap_or(false)
    }

    /// Persist counters without emitting (progress writes).
    pub(crate) fn silent_persist(&self, task: &QueueTask) {
        self.persist_task(task);
    }

    pub(crate) fn record_store_error(&self, message: String) {
        if let Ok(mut slot) = self.inner.last_store_error.lock() {
            *slot = Some(message);
        }
    }

    /// Cancel a running task's token. Returns whether it was running.
    pub(crate) fn flag_cancel(&self, task_id: &str, cancel_to_pending: bool) -> bool {
        let Ok(mut running) = self.inner.running.lock() else {
            return false;
        };
        let Some(slot) = running.get_mut(task_id) else {
            return false;
        };
        slot.cancel_to_pending |= cancel_to_pending;
        slot.token.cancel();
        true
    }

    /// Drop the platform waiter for a task (cancellation/timeout path).
    pub(crate) fn abort_platform(&self, task_id: &str) {
        let req_id = self
            .inner
            .platform_tasks
            .lock()
            .ok()
            .and_then(|mut map| map.remove(task_id));
        if let Some(req_id) = req_id {
            if let Ok(mut map) = self.inner.platform.lock() {
                map.remove(&req_id);
            }
        }
    }

    /// After every state change: close finished batches, prune silent ones and
    /// announce `session_drained` once.
    pub(crate) fn settle(&self) {
        let now = now_ms();
        let closed = {
            let Ok(mut state) = self.inner.state.lock() else {
                return;
            };
            state.close_finished_batches(now)
        };
        for batch in &closed.batches {
            self.persist_batch(batch);
            if batch.silent {
                continue;
            }
            self.emit(EventKind::BatchUpdated {
                batch: batch.clone(),
            });
        }
        for summary in &closed.summaries {
            let silent = self.batch_is_silent(&summary.batch_id);
            // Silent batches only surface a summary when something failed.
            if silent && summary.failed > 0 {
                self.emit(EventKind::Summary {
                    batch_id: summary.batch_id.clone(),
                    label: summary.label.clone(),
                    completed: summary.completed,
                    failed: summary.failed,
                    cancelled: summary.cancelled,
                });
            }
        }

        let pruned = {
            let Ok(mut state) = self.inner.state.lock() else {
                return;
            };
            state.prune_silent_closed()
        };
        for id in &pruned.task_ids {
            let _ = self.inner.store.delete_task(id);
            let _ = self.inner.store.delete_payload(id);
        }
        for id in &pruned.batch_ids {
            let _ = self.inner.store.delete_batch(id);
        }

        let drained = {
            let Ok(mut state) = self.inner.state.lock() else {
                return;
            };
            if state.is_drained() && !state.drained_announced() {
                state.mark_drained_announced();
                true
            } else {
                false
            }
        };
        if drained {
            self.emit(EventKind::SessionDrained);
        }
    }
}
