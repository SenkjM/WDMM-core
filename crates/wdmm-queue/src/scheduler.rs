//! The scheduler: start runnable tasks, run them through a [`TaskRunner`], and
//! apply the outcome (completion, retry via [`crate::policy`], cancellation).
//!
//! The pump loop itself lives on [`QueueHandle::pump`]; this module owns the
//! per-task execution and the state transitions that follow it.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::events::EventKind;
use crate::followups;
use crate::handle::{
    ProgressSink, QueueHandle, RunCtx, RunOutcome, RunningTask, FGS_COOLDOWN_MS,
    PROGRESS_PERSIST_INTERVAL,
};
use crate::model::{CancelToken, QueueTask, TaskStatus};
use crate::policy::{decide, RetryAction, RetryInput};
use crate::queue::MSG_CANCELLED;
use crate::{now_ms, QueueResult};

/// Mark a pending task active and hand it to the runner.
pub(crate) fn start_task(handle: QueueHandle, task_id: String) {
    let now = now_ms();
    let task = {
        let Ok(mut state) = handle.inner.state.lock() else {
            return;
        };
        state.mark_active(&task_id, now)
    };
    let Some(task) = task else {
        return;
    };
    handle.persist_task(&task);
    if !handle.batch_is_silent(&task.batch_id) {
        handle.emit(EventKind::TaskStarted { task: task.clone() });
    }

    let token = CancelToken::new();
    if let Ok(mut running) = handle.inner.running.lock() {
        running.insert(
            task_id.clone(),
            RunningTask {
                token: token.clone(),
                cancel_to_pending: false,
            },
        );
    }

    tokio::spawn(run_task(handle, task, token));
}

async fn run_task(handle: QueueHandle, task: QueueTask, token: CancelToken) {
    let reporter = Arc::new(TaskProgressSink::new(handle.clone(), task.id.clone()));
    let ctx = RunCtx {
        task: task.clone(),
        cancel: token,
        progress: reporter,
        queue: handle.clone(),
    };
    let outcome = handle.inner.runner.run(ctx).await;
    finish(handle, task.id, outcome);
}

/// Apply a runner outcome: terminal state, retry, or the FGS pause.
pub(crate) fn finish(handle: QueueHandle, task_id: String, outcome: RunOutcome) {
    let now = now_ms();
    let cancel_to_pending = handle
        .inner
        .running
        .lock()
        .ok()
        .and_then(|mut map| map.remove(&task_id))
        .map(|slot| slot.cancel_to_pending)
        .unwrap_or(false);

    let Some(task) = handle.task(&task_id) else {
        handle.kick();
        return;
    };

    // `fgs_timeout` asked the job to unwind: return to pending, not failed, and
    // keep the attempt counter untouched.
    if cancel_to_pending {
        let until = {
            let Ok(state) = handle.inner.state.lock() else {
                return;
            };
            state.suspended_until()
        }
        .unwrap_or(now + FGS_COOLDOWN_MS);
        let paused = {
            let Ok(mut state) = handle.inner.state.lock() else {
                return;
            };
            state.pause_until(&task_id, until, now)
        };
        if let Some(paused) = paused {
            handle.persist_task(&paused);
        }
        handle.settle();
        handle.kick();
        return;
    }

    match outcome {
        RunOutcome::Completed => {
            finalize(&handle, task_id, TaskStatus::Completed, None, None);
        }
        RunOutcome::Partial { result_json } => {
            finalize(
                &handle,
                task_id,
                TaskStatus::Partial,
                None,
                Some(result_json),
            );
        }
        RunOutcome::Cancelled => {
            finalize(
                &handle,
                task_id,
                TaskStatus::Cancelled,
                Some(MSG_CANCELLED.to_string()),
                None,
            );
        }
        RunOutcome::Failed { error, category } => {
            let decision = decide(&RetryInput {
                attempts: task.attempts,
                in_background: handle
                    .inner
                    .state
                    .lock()
                    .map(|state| state.background())
                    .unwrap_or(false),
                category,
                message: error.clone(),
                cancelled: false,
                jitter_ms: rand::random::<u64>() % 1_000,
            });
            match decision.action {
                RetryAction::Retry => {
                    let scheduled = {
                        let Ok(mut state) = handle.inner.state.lock() else {
                            return;
                        };
                        state.schedule_retry(
                            &task_id,
                            decision.attempts,
                            decision.cap,
                            decision.delay_ms,
                            error,
                            now,
                        )
                    };
                    if let Some(scheduled) = scheduled {
                        handle.persist_task(&scheduled);
                        if !handle.batch_is_silent(&scheduled.batch_id) {
                            handle.emit(EventKind::TaskAdded { task: scheduled });
                        }
                    }
                    handle.settle();
                    handle.kick();
                }
                RetryAction::Cancel | RetryAction::Fail => {
                    finalize(&handle, task_id, TaskStatus::Failed, Some(error), None);
                }
            }
        }
    }
}

fn finalize(
    handle: &QueueHandle,
    task_id: String,
    status: TaskStatus,
    error: Option<String>,
    result_json: Option<String>,
) {
    let now = now_ms();
    let task = {
        let Ok(mut state) = handle.inner.state.lock() else {
            return;
        };
        state.set_terminal(&task_id, status, error, result_json, now)
    };
    if let Some(task) = &task {
        handle.persist_task(task);
        // The scan/build sidecar is only kept for partial results (the UI may
        // still show them); every other terminal state drops it.
        if status != TaskStatus::Partial && task.payload_path.is_some() {
            let _ = handle.inner.store.delete_payload(&task_id);
        }
        handle.emit_terminal(task);
        // Follow-up chains must see the batch still open so the task they
        // enqueue joins it instead of landing in a just-closed batch.
        if matches!(status, TaskStatus::Completed | TaskStatus::Partial) {
            followups::after_terminal(handle, task);
        }
    }
    handle.settle();
    handle.kick();
}

/// `ProgressSink` bound to one task: updates counters, throttles events to
/// 10 Hz and persists at most twice per second.
pub(crate) struct TaskProgressSink {
    handle: QueueHandle,
    task_id: String,
    last_persist: Mutex<Instant>,
}

impl TaskProgressSink {
    fn new(handle: QueueHandle, task_id: String) -> Self {
        Self {
            handle,
            task_id,
            last_persist: Mutex::new(Instant::now() - PROGRESS_PERSIST_INTERVAL),
        }
    }

    fn update(&self, apply: impl FnOnce(&mut QueueTask)) {
        let task = {
            let Ok(mut state) = self.handle.inner.state.lock() else {
                return;
            };
            let Some(task) = state.task_mut(&self.task_id) else {
                return;
            };
            if !matches!(
                task.status,
                TaskStatus::Active | TaskStatus::AwaitingPlatform
            ) {
                return;
            }
            apply(task);
            task.updated_at = now_ms();
            task.clone()
        };

        if !self.handle.batch_is_silent(&task.batch_id) {
            self.handle.emit_progress(
                &self.task_id,
                EventKind::TaskProgress {
                    task_id: task.id.clone(),
                    status: task.status,
                    progress: task.display_progress(),
                    progress_mode: task.progress_mode,
                    bytes_done: task.bytes_done,
                    bytes_total: task.bytes_total,
                    units_done: task.units_done,
                    units_total: task.units_total,
                },
            );
        }

        let persist = self
            .last_persist
            .lock()
            .map(|mut at| {
                if at.elapsed() >= PROGRESS_PERSIST_INTERVAL {
                    *at = Instant::now();
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false);
        if persist {
            self.handle.silent_persist(&task);
        }
    }
}

impl ProgressSink for TaskProgressSink {
    fn bytes(&self, done: u64, total: Option<u64>) {
        self.update(|task| task.set_bytes(done, total));
    }

    fn units(&self, done: u32, total: Option<u32>) {
        self.update(|task| task.set_units(done, total));
    }

    fn note(&self, _message: &str) {
        // The current UI has no per-task note slot; progress + terminal events
        // carry everything it renders.
    }

    fn local_path(&self, path: &str) {
        let task = {
            let Ok(mut state) = self.handle.inner.state.lock() else {
                return;
            };
            state.note_local_path(&self.task_id, path, now_ms())
        };
        // Persist immediately: the caller records this right before a platform
        // hand-off, and a kill in between must not lose the file location.
        if let Some(task) = task {
            self.handle.persist_task(&task);
        }
    }

    fn result(&self, json: &str) {
        let task = {
            let Ok(mut state) = self.handle.inner.state.lock() else {
                return;
            };
            state.note_result(&self.task_id, json, now_ms())
        };
        if let Some(task) = task {
            self.handle.persist_task(&task);
        }
    }
}

/// Helper used by tests to drive the queue until it drains.
impl QueueHandle {
    pub async fn drain_for_tests(&self, timeout: Duration) -> QueueResult<()> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.pump().await;
            if self.snapshot().session.drained {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Err(crate::QueueError::Storage(
            "queue did not drain before the test timeout".into(),
        ))
    }
}
