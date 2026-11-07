//! The queue state machine: pure in-memory transitions over tasks and batches.
//!
//! [`QueueState`] owns no I/O and no locks. [`crate::handle::QueueHandle`] wraps
//! it in a `Mutex`, persists the resulting rows through
//! [`crate::store::TaskStore`] and emits the matching events.

use std::collections::{HashMap, HashSet};

use crate::model::{
    batch_kind_for, BatchOrigin, QueueBatch, QueueTask, TaskKind, TaskStatus, TaskTarget,
};
use crate::{new_id, now_ms, QueueError, QueueResult};

/// Everything needed to enqueue one task. Unknown/optional knobs default to the
/// Dart queue's behaviour (dedupe on, user batch, priority 0).
#[derive(Debug, Clone)]
pub struct EnqueueRequest {
    /// Caller-provided id (Dart compatibility); a random id is minted otherwise.
    pub id: Option<String>,
    pub kind: TaskKind,
    pub target: TaskTarget,
    pub source_name: String,
    pub account_id: String,
    pub remote_path: String,
    pub file_name: String,
    /// Existing batch to join. `None` creates a single-task batch.
    pub batch_id: Option<String>,
    /// Origin used only when this request creates its batch.
    pub batch_origin: BatchOrigin,
    pub batch_label: Option<String>,
    pub priority: i32,
    pub cache_group_id: Option<String>,
    /// JSON sidecar for scan/manga-build tasks.
    pub payload: Option<String>,
    /// Local source path, for tasks that operate on a file that already exists
    /// (uploads from a SAF copy, and `manga:<id>` markers).
    pub local_path: Option<String>,
    /// Local content URI for tasks the platform must materialise first.
    pub local_uri: Option<String>,
    pub remote_size: Option<u64>,
    pub remote_modified: Option<i64>,
    pub queued_while_foreground: bool,
    /// Reuse an in-flight task with the same account/remote path/target.
    pub dedupe: bool,
    pub expected_total: Option<u32>,
    /// Test hook; defaults to the wall clock.
    pub now: Option<i64>,
}

impl EnqueueRequest {
    pub fn new(
        kind: TaskKind,
        account_id: impl Into<String>,
        remote_path: impl Into<String>,
        file_name: impl Into<String>,
    ) -> Self {
        let target = match kind {
            TaskKind::DownloadPublic => TaskTarget::Public,
            _ => TaskTarget::Cache,
        };
        Self {
            id: None,
            kind,
            target,
            source_name: String::new(),
            account_id: account_id.into(),
            remote_path: remote_path.into(),
            file_name: file_name.into(),
            batch_id: None,
            batch_origin: BatchOrigin::User,
            batch_label: None,
            priority: 0,
            cache_group_id: None,
            payload: None,
            local_path: None,
            local_uri: None,
            remote_size: None,
            remote_modified: None,
            queued_while_foreground: false,
            dedupe: true,
            expected_total: None,
            now: None,
        }
    }

    fn timestamp(&self) -> i64 {
        self.now.unwrap_or_else(now_ms)
    }
}

/// Result of [`QueueState::enqueue`].
#[derive(Debug, Clone)]
pub struct Enqueued {
    pub task: QueueTask,
    pub deduped: bool,
    pub batch: QueueBatch,
    /// True when this enqueue created the batch (so the caller persists it).
    pub created_batch: bool,
}

/// What `cancel` decided for one task.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum CancelOutcome {
    /// Pending or awaiting-platform: already terminal, the caller persists it.
    Terminal(QueueTask),
    /// Active: the scheduler must flip the task's cancel token.
    Requested,
    /// Already completed/failed/cancelled.
    AlreadyTerminal,
}

/// Batches closed by [`QueueState::close_finished_batches`].
#[derive(Debug, Clone, Default)]
pub struct ClosedBatches {
    pub batches: Vec<QueueBatch>,
    /// User-visible success/failure counters, computed before pruning.
    pub summaries: Vec<BatchSummary>,
}

#[derive(Debug, Clone)]
pub struct BatchSummary {
    pub batch_id: String,
    pub label: Option<String>,
    pub completed: u32,
    pub failed: u32,
    pub cancelled: u32,
}

/// Rows removed when a silent batch closes.
#[derive(Debug, Clone, Default)]
pub struct PrunedBatches {
    pub task_ids: Vec<String>,
    pub batch_ids: Vec<String>,
}

/// `err.*` message keys persisted on tasks; the Dart layer localises them.
pub const MSG_CANCELLED: &str = "err.cancelled";
pub const MSG_QUEUE_CLEARED: &str = "err.queueCleared";

/// The live queue: tasks, batches, session flags and scheduling knobs.
#[derive(Debug)]
pub struct QueueState {
    tasks: HashMap<String, QueueTask>,
    batches: HashMap<String, QueueBatch>,
    concurrency: usize,
    notifications: bool,
    keep_alive: bool,
    background: bool,
    /// Set by `fgs_timeout`; no task starts before this timestamp.
    suspended_until: Option<i64>,
    /// Last keep-alive intent reported to / acknowledged by the platform.
    keepalive_phase: String,
    keepalive_last_error: Option<String>,
    /// Whether the drained state was already announced for the current session.
    drained_announced: bool,
}

impl QueueState {
    pub fn new() -> Self {
        Self {
            tasks: HashMap::new(),
            batches: HashMap::new(),
            concurrency: 1,
            notifications: true,
            keep_alive: false,
            background: false,
            suspended_until: None,
            keepalive_phase: "idle".to_string(),
            keepalive_last_error: None,
            drained_announced: false,
        }
    }

    /// Rebuild state from disk and apply the restart rules: a task that was
    /// `active` or `awaiting_platform` becomes `pending`, keeping its counters
    /// (so downloads resume at `bytes_done`) and its retry timestamp.
    pub fn from_loaded(loaded: crate::store::LoadedQueue, now: i64) -> Self {
        let mut state = Self::new();
        for mut task in loaded.tasks {
            if matches!(
                task.status,
                TaskStatus::Active | TaskStatus::AwaitingPlatform
            ) {
                task.status = TaskStatus::Pending;
                task.updated_at = now;
            }
            state.tasks.insert(task.id.clone(), task);
        }
        for batch in loaded.batches {
            state.batches.insert(batch.id.clone(), batch);
        }
        state
    }

    // ---------------------------------------------------------------- reads

    pub fn task(&self, id: &str) -> Option<&QueueTask> {
        self.tasks.get(id)
    }

    pub fn task_mut(&mut self, id: &str) -> Option<&mut QueueTask> {
        self.tasks.get_mut(id)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.tasks.contains_key(id)
    }

    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub fn tasks_sorted(&self) -> Vec<QueueTask> {
        let mut tasks: Vec<QueueTask> = self.tasks.values().cloned().collect();
        tasks.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        tasks
    }

    pub fn by_remote(&self, account_id: &str, remote_path: &str) -> Vec<QueueTask> {
        let mut tasks: Vec<QueueTask> = self
            .tasks
            .values()
            .filter(|t| t.account_id == account_id && t.remote_path == remote_path)
            .cloned()
            .collect();
        tasks.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        tasks
    }

    pub fn batch(&self, id: &str) -> Option<&QueueBatch> {
        self.batches.get(id)
    }

    pub fn batches_sorted(&self) -> Vec<QueueBatch> {
        let mut batches: Vec<QueueBatch> = self.batches.values().cloned().collect();
        batches.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        batches
    }

    pub fn batch_members(&self, batch_id: &str) -> Vec<&QueueTask> {
        let mut members: Vec<&QueueTask> = self
            .tasks
            .values()
            .filter(|t| t.batch_id == batch_id)
            .collect();
        members.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        members
    }

    // ----------------------------------------------------------- scheduling

    pub fn concurrency(&self) -> usize {
        self.concurrency
    }

    pub fn set_concurrency(&mut self, concurrency: usize) {
        self.concurrency = concurrency.max(1);
    }

    pub fn notifications(&self) -> bool {
        self.notifications
    }

    pub fn set_notifications(&mut self, enabled: bool) {
        self.notifications = enabled;
    }

    pub fn keep_alive(&self) -> bool {
        self.keep_alive
    }

    pub fn set_keep_alive(&mut self, enabled: bool) {
        self.keep_alive = enabled;
        self.keepalive_phase = if enabled { "start" } else { "release" }.to_string();
    }

    pub fn keepalive_phase(&self) -> &str {
        &self.keepalive_phase
    }

    pub fn keepalive_last_error(&self) -> Option<&str> {
        self.keepalive_last_error.as_deref()
    }

    /// Record the platform's acknowledgement/error for the keep-alive request.
    pub fn note_keepalive(&mut self, phase: &str, error: Option<String>) {
        self.keepalive_phase = phase.to_string();
        self.keepalive_last_error = error;
    }

    pub fn background(&self) -> bool {
        self.background
    }

    pub fn set_background(&mut self, background: bool) {
        self.background = background;
    }

    pub fn suspended_until(&self) -> Option<i64> {
        self.suspended_until
    }

    pub fn suspend_until(&mut self, until: i64) {
        self.suspended_until = Some(until);
    }

    /// Park a task until `until` without counting an attempt (FGS timeout).
    pub fn pause_until(&mut self, id: &str, until: i64, now: i64) -> Option<QueueTask> {
        let task = self.tasks.get_mut(id)?;
        task.status = TaskStatus::Pending;
        task.next_retry_at = Some(until);
        task.updated_at = now;
        Some(task.clone())
    }

    pub fn drained_announced(&self) -> bool {
        self.drained_announced
    }

    pub fn mark_drained_announced(&mut self) {
        self.drained_announced = true;
    }

    /// Next task the scheduler may start: pending, retry delay elapsed, queue not
    /// suspended. Highest priority first, then FIFO.
    pub fn next_runnable(&self, now: i64) -> Option<String> {
        if let Some(until) = self.suspended_until {
            if now < until {
                return None;
            }
        }
        let mut candidates: Vec<&QueueTask> = self
            .tasks
            .values()
            .filter(|t| t.status == TaskStatus::Pending)
            .filter(|t| t.next_retry_at.is_none_or(|at| at <= now))
            .collect();
        candidates.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority)
                .then_with(|| a.created_at.cmp(&b.created_at))
                .then_with(|| a.id.cmp(&b.id))
        });
        candidates.first().map(|t| t.id.clone())
    }

    /// Earliest retry timestamp among pending tasks (for the pump's timer).
    pub fn earliest_retry_at(&self) -> Option<i64> {
        self.tasks
            .values()
            .filter(|t| t.status == TaskStatus::Pending)
            .filter_map(|t| t.next_retry_at)
            .min()
    }

    /// Number of tasks currently holding a scheduler slot.
    pub fn active_count(&self) -> usize {
        self.tasks
            .values()
            .filter(|t| matches!(t.status, TaskStatus::Active | TaskStatus::AwaitingPlatform))
            .count()
    }

    pub fn open_batches(&self) -> usize {
        self.batches.values().filter(|b| b.is_open()).count()
    }

    /// A session is drained when no batch is open (matches the contract).
    pub fn is_drained(&self) -> bool {
        self.open_batches() == 0
    }

    // ------------------------------------------------------------ mutations

    /// Insert a task and (if needed) its batch.
    ///
    /// Dedupe mirrors the Dart queue: an in-flight task for the same
    /// `(account, remote_path, target)` is returned instead of a duplicate.
    pub fn enqueue(&mut self, req: EnqueueRequest) -> Enqueued {
        let now = req.timestamp();

        if req.dedupe {
            let existing = self
                .tasks
                .values()
                .find(|t| {
                    !t.status.is_terminal()
                        && t.account_id == req.account_id
                        && t.remote_path == req.remote_path
                        && t.kind == req.kind
                        && t.target == req.target
                })
                .cloned();
            if let Some(task) = existing {
                let batch = self
                    .batches
                    .get(&task.batch_id)
                    .cloned()
                    .unwrap_or_else(|| self.ensure_batch(&task.batch_id, &req, now).0);
                return Enqueued {
                    task,
                    deduped: true,
                    batch,
                    created_batch: false,
                };
            }
        }

        // New work re-opens the session, so a later drain must announce again.
        self.drained_announced = false;
        let task_id = req.id.clone().unwrap_or_else(new_id);
        let mut task = QueueTask::new(
            task_id,
            req.kind,
            req.source_name.clone(),
            req.account_id.clone(),
            req.remote_path.clone(),
            req.file_name.clone(),
            req.target,
            String::new(),
            now,
        );
        task.priority = req.priority;
        task.cache_group_id = req.cache_group_id.clone();
        task.local_path = req.local_path.clone();
        task.local_uri = req.local_uri.clone();
        task.remote_size = req.remote_size;
        task.remote_modified = req.remote_modified;
        task.queued_while_foreground = req.queued_while_foreground;

        let (batch, created_batch) = match &req.batch_id {
            Some(id) => match self.batches.get(id).cloned() {
                Some(batch) => (batch, false),
                None => self.ensure_batch(id, &req, now),
            },
            None => self.create_batch(&req, now),
        };
        if let Some(total) = req.expected_total {
            if batch.expected_total.is_none() {
                if let Some(b) = self.batches.get_mut(&batch.id) {
                    b.expected_total = Some(total);
                }
            }
        }
        task.batch_id = batch.id.clone();

        self.tasks.insert(task.id.clone(), task.clone());
        Enqueued {
            task,
            deduped: false,
            batch,
            created_batch,
        }
    }

    /// Create a single-task batch for `req`.
    fn create_batch(&mut self, req: &EnqueueRequest, now: i64) -> (QueueBatch, bool) {
        let batch = QueueBatch {
            id: new_id(),
            kind: batch_kind_for(req.kind),
            origin: req.batch_origin,
            silent: req.batch_origin.is_silent(),
            expected_total: req.expected_total,
            created_at: now,
            closed_at: None,
            label: req.batch_label.clone(),
        };
        self.batches.insert(batch.id.clone(), batch.clone());
        (batch, true)
    }

    /// Create a batch with an explicit id (Dart-provided batch).
    fn ensure_batch(&mut self, id: &str, req: &EnqueueRequest, now: i64) -> (QueueBatch, bool) {
        if let Some(batch) = self.batches.get(id) {
            return (batch.clone(), false);
        }
        let batch = QueueBatch {
            id: id.to_string(),
            kind: batch_kind_for(req.kind),
            origin: req.batch_origin,
            silent: req.batch_origin.is_silent(),
            expected_total: req.expected_total,
            created_at: now,
            closed_at: None,
            label: req.batch_label.clone(),
        };
        self.batches.insert(batch.id.clone(), batch.clone());
        (batch, true)
    }

    /// Move a pending task to active. Returns the task so the caller can persist
    /// and emit `task_started`.
    pub fn mark_active(&mut self, id: &str, now: i64) -> Option<QueueTask> {
        let task = self.tasks.get_mut(id)?;
        if task.status != TaskStatus::Pending {
            return None;
        }
        task.status = TaskStatus::Active;
        task.updated_at = now;
        Some(task.clone())
    }

    /// Apply a terminal status.
    pub fn set_terminal(
        &mut self,
        id: &str,
        status: TaskStatus,
        error_message: Option<String>,
        result_json: Option<String>,
        now: i64,
    ) -> Option<QueueTask> {
        let task = self.tasks.get_mut(id)?;
        task.status = status;
        task.error_message = error_message;
        if result_json.is_some() {
            task.result_json = result_json;
        }
        if status.is_success() {
            task.sync_progress_from_counters();
            task.progress = 1.0;
        }
        task.next_retry_at = None;
        task.updated_at = now;
        Some(task.clone())
    }

    /// Record the local file a task produced. Kept on the row (and in the
    /// store) so a restart knows the transfer already happened — the platform
    /// hand-off in particular must survive the app going away.
    pub fn note_local_path(&mut self, id: &str, path: &str, now: i64) -> Option<QueueTask> {
        let task = self.tasks.get_mut(id)?;
        task.local_path = Some(path.to_string());
        task.updated_at = now;
        Some(task.clone())
    }

    /// Record a result payload before the task goes terminal, for jobs that
    /// report a follow-up target but still end `Completed`.
    pub fn note_result(&mut self, id: &str, result_json: &str, now: i64) -> Option<QueueTask> {
        let task = self.tasks.get_mut(id)?;
        task.result_json = Some(result_json.to_string());
        task.updated_at = now;
        Some(task.clone())
    }

    /// Schedule a retry: back to pending with the policy's attempt count and
    /// delay. The `err.*` message stays on the task for the UI.
    pub fn schedule_retry(
        &mut self,
        id: &str,
        attempts: u32,
        cap: u32,
        delay_ms: u64,
        error_message: String,
        now: i64,
    ) -> Option<QueueTask> {
        let task = self.tasks.get_mut(id)?;
        task.status = TaskStatus::Pending;
        task.attempts = attempts;
        task.attempts_cap = Some(cap);
        task.next_retry_at = Some(now.saturating_add(delay_ms as i64));
        task.error_message = Some(error_message);
        task.updated_at = now;
        Some(task.clone())
    }

    /// Cancel one task. Pending/awaiting tasks go terminal immediately; active
    /// ones only get flagged so the running job can unwind.
    pub fn cancel(&mut self, id: &str, now: i64) -> Option<CancelOutcome> {
        let task = self.tasks.get(id)?;
        match task.status {
            TaskStatus::Pending | TaskStatus::AwaitingPlatform => {
                let task = self.set_terminal(
                    id,
                    TaskStatus::Cancelled,
                    Some(MSG_CANCELLED.to_string()),
                    None,
                    now,
                )?;
                Some(CancelOutcome::Terminal(task))
            }
            TaskStatus::Active => Some(CancelOutcome::Requested),
            _ => Some(CancelOutcome::AlreadyTerminal),
        }
    }

    pub fn cancel_all(&mut self, now: i64) -> Vec<QueueTask> {
        let ids: Vec<String> = self
            .tasks
            .values()
            .filter(|t| matches!(t.status, TaskStatus::Pending | TaskStatus::AwaitingPlatform))
            .map(|t| t.id.clone())
            .collect();
        ids.iter()
            .filter_map(|id| {
                self.set_terminal(
                    id,
                    TaskStatus::Cancelled,
                    Some(MSG_CANCELLED.to_string()),
                    None,
                    now,
                )
            })
            .collect()
    }

    /// Reset a finished task for another attempt.
    pub fn retry(&mut self, id: &str, now: i64) -> QueueResult<QueueTask> {
        let task = self
            .tasks
            .get_mut(id)
            .ok_or_else(|| QueueError::NotFound(id.to_string()))?;
        if !matches!(
            task.status,
            TaskStatus::Failed | TaskStatus::Partial | TaskStatus::Cancelled
        ) {
            return Err(QueueError::Invalid(format!(
                "task {id} is not retryable in status {}",
                task.status.as_str()
            )));
        }
        task.status = TaskStatus::Pending;
        task.attempts = 0;
        task.attempts_cap = None;
        task.next_retry_at = None;
        task.error_message = None;
        task.result_json = None;
        task.progress = 0.0;
        task.bytes_done = 0;
        task.units_done = 0;
        task.updated_at = now;
        Ok(task.clone())
    }

    pub fn retry_all(&mut self, now: i64) -> Vec<QueueTask> {
        let ids: Vec<String> = self
            .tasks
            .values()
            .filter(|t| {
                matches!(
                    t.status,
                    TaskStatus::Failed | TaskStatus::Partial | TaskStatus::Cancelled
                )
            })
            .map(|t| t.id.clone())
            .collect();
        ids.iter()
            .filter_map(|id| self.retry(id, now).ok())
            .collect()
    }

    pub fn remove(&mut self, id: &str) -> Option<QueueTask> {
        let task = self.tasks.remove(id)?;
        self.close_empty_batch(&task.batch_id);
        Some(task)
    }

    pub fn clear_completed(&mut self) -> Vec<String> {
        let ids: Vec<String> = self
            .tasks
            .values()
            .filter(|t| t.status == TaskStatus::Completed)
            .map(|t| t.id.clone())
            .collect();
        for id in &ids {
            self.tasks.remove(id);
        }
        self.close_all_empty_batches();
        ids
    }

    /// Remove every task and batch, cancelling whatever was still in flight.
    pub fn clear_all(&mut self, now: i64) -> Vec<QueueTask> {
        let cancelled = self.cancel_all(now);
        self.tasks.clear();
        self.batches.clear();
        self.drained_announced = false;
        cancelled
    }

    fn close_empty_batch(&mut self, batch_id: &str) {
        if self.batch_members(batch_id).is_empty() {
            self.batches.remove(batch_id);
        }
    }

    fn close_all_empty_batches(&mut self) {
        let empty: Vec<String> = self
            .batches
            .keys()
            .filter(|id| self.batch_members(id).is_empty())
            .cloned()
            .collect();
        for id in empty {
            self.batches.remove(&id);
        }
    }

    /// Close every open batch whose members are all terminal. Returns the closed
    /// batches together with per-batch counters captured before pruning.
    pub fn close_finished_batches(&mut self, now: i64) -> ClosedBatches {
        let mut closed = ClosedBatches::default();
        let candidates: Vec<String> = self
            .batches
            .values()
            .filter(|b| b.is_open())
            .map(|b| b.id.clone())
            .collect();
        for batch_id in candidates {
            let members = self.batch_members(&batch_id);
            if members.is_empty() || !members.iter().all(|t| t.status.is_terminal()) {
                continue;
            }
            let mut completed = 0;
            let mut failed = 0;
            let mut cancelled = 0;
            for task in &members {
                match task.status {
                    TaskStatus::Completed | TaskStatus::Partial => completed += 1,
                    TaskStatus::Cancelled => cancelled += 1,
                    TaskStatus::Failed => failed += 1,
                    _ => {}
                }
            }
            if let Some(batch) = self.batches.get_mut(&batch_id) {
                batch.closed_at = Some(now);
                let summary = BatchSummary {
                    batch_id: batch.id.clone(),
                    label: batch.label.clone(),
                    completed,
                    failed,
                    cancelled,
                };
                closed.batches.push(batch.clone());
                closed.summaries.push(summary);
            }
        }
        closed
    }

    /// Delete the successful rows of closed *silent* batches (and their payload
    /// sidecars); empty silent batches are removed entirely.
    pub fn prune_silent_closed(&mut self) -> PrunedBatches {
        let mut pruned = PrunedBatches::default();
        let silent_closed: Vec<String> = self
            .batches
            .values()
            .filter(|b| b.silent && b.closed_at.is_some())
            .map(|b| b.id.clone())
            .collect();
        for batch_id in silent_closed {
            let removable: Vec<(String, Option<String>)> = self
                .batch_members(&batch_id)
                .into_iter()
                .filter(|t| t.status.is_success())
                .map(|t| (t.id.clone(), t.payload_path.clone()))
                .collect();
            for (id, _payload) in &removable {
                self.tasks.remove(id);
                pruned.task_ids.push(id.clone());
            }
            if self.batch_members(&batch_id).is_empty() {
                self.batches.remove(&batch_id);
                pruned.batch_ids.push(batch_id);
            }
        }
        pruned
    }

    /// Delete pending tasks whose retry timestamp is older than `now` by more
    /// than `stale_ms` — a safety net for tasks stuck behind a dead runner.
    pub fn stale_retry_ids(&self, now: i64, stale_ms: i64) -> Vec<String> {
        self.tasks
            .values()
            .filter(|t| t.status == TaskStatus::Pending)
            .filter(|t| {
                t.next_retry_at
                    .is_some_and(|at| now.saturating_sub(at) > stale_ms)
            })
            .map(|t| t.id.clone())
            .collect()
    }

    /// Ids of tasks in `Pending` (used by the pump after a resume).
    pub fn pending_ids(&self) -> HashSet<String> {
        self.tasks
            .values()
            .filter(|t| t.status == TaskStatus::Pending)
            .map(|t| t.id.clone())
            .collect()
    }
}

impl Default for QueueState {
    fn default() -> Self {
        Self::new()
    }
}
