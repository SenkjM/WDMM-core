//! Adapter from [`wdmm_jobs::JobRegistry`] to the queue's [`TaskRunner`] seam
//! (contract §A1).
//!
//! `wdmm-jobs` may depend on `wdmm-queue` (it implements the executors), never
//! the other way around. This module is the only place where the two meet:
//! [`QueueRunner::run`] materialises the account driver, opens the platform
//! bridge, builds a [`JobCtx`] and translates [`JobOutcome`] back into
//! [`RunOutcome`].

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use wdmm_drivers::{CloudFileItem, DriverError, Result as VfsResult, WdmmVfs};
use wdmm_jobs::{JobCtx, JobOutcome, JobPaths, JobRegistry, PlatformCall, PlatformClient};
use wdmm_queue::policy::FailureCategory;
use wdmm_queue::{
    PlatformError, ProgressSink as QueueProgressSink, RunCtx, RunOutcome, TaskKind, TaskRunner,
};
use wdmm_storage::LibraryDatabase;

use crate::create_account_driver;

/// Bridges the queue's executor seam onto the `wdmm-jobs` job registry.
pub struct QueueRunner {
    registry: Arc<JobRegistry>,
    db: Arc<LibraryDatabase>,
    paths: JobPaths,
}

impl QueueRunner {
    pub fn new(registry: JobRegistry, db: Arc<LibraryDatabase>, paths: JobPaths) -> Self {
        Self {
            registry: Arc::new(registry),
            db,
            paths,
        }
    }
}

#[async_trait]
impl TaskRunner for QueueRunner {
    fn supports(&self, kind: TaskKind) -> bool {
        self.registry.get(kind).is_some()
    }

    async fn run(&self, ctx: RunCtx) -> RunOutcome {
        let Some(job) = self.registry.get(ctx.task.kind) else {
            return RunOutcome::Failed {
                error: format!("err.noExecutor|{}", ctx.task.kind.as_str()),
                category: FailureCategory::Other,
            };
        };

        let vfs = match build_vfs(&ctx.task.account_id) {
            Ok(vfs) => vfs,
            Err(error) => {
                return RunOutcome::Failed {
                    error,
                    category: FailureCategory::Other,
                }
            }
        };

        let (client, mut calls) = PlatformClient::channel(8);
        let job_ctx = JobCtx {
            task: ctx.task.clone(),
            cancel: ctx.cancel.clone(),
            progress: Arc::new(ProgressAdapter(Arc::clone(&ctx.progress))),
            platform: Arc::new(client),
            db: Arc::clone(&self.db),
            vfs,
            paths: self.paths.clone(),
        };

        // The bridge and the job share one task: `RunCtx` is not `Clone` and the
        // queue keys pending platform replies by request id, so the requests are
        // answered from the same future instead of a detached `tokio::spawn`.
        let mut job_fut = Box::pin(job.run(job_ctx));
        let mut platform_open = true;
        let outcome = loop {
            tokio::select! {
                biased;
                outcome = &mut job_fut => break outcome,
                call = calls.recv(), if platform_open => match call {
                    Some(call) => answer_platform(&ctx, call).await,
                    None => platform_open = false,
                },
            }
        };

        match outcome {
            JobOutcome::Completed => RunOutcome::Completed,
            JobOutcome::Partial { result } => RunOutcome::Partial {
                result_json: result.to_string(),
            },
            JobOutcome::Failed { error, category } => RunOutcome::Failed { error, category },
            JobOutcome::Cancelled => RunOutcome::Cancelled,
        }
    }
}

/// Resolve the account driver once per task.
///
/// A task without an account (manga build / local packing) still needs a
/// `WdmmVfs` in [`JobCtx`]; [`NoAccountVfs`] fails loudly on every call instead
/// of silently falling back to a Dart-side path.
fn build_vfs(account_id: &str) -> Result<Arc<dyn WdmmVfs>, String> {
    if account_id.trim().is_empty() {
        return Ok(Arc::new(NoAccountVfs));
    }
    create_account_driver(account_id).map_err(|e| format!("err.driver|{e}"))
}

/// Turn one queued platform action into `PlatformRequest{action, params_json}`
/// (contract §A4) and hand the answer back to the waiting job.
async fn answer_platform(ctx: &RunCtx, call: PlatformCall) {
    let action = serde_json::to_value(&call.action).unwrap_or(Value::Null);
    let name = action
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let params_json = action.to_string();

    let reply: Result<Value, String> = match ctx.request_platform(&name, &params_json).await {
        Ok(result) if result.ok => Ok(result
            .result_json
            .as_deref()
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .unwrap_or(Value::Null)),
        Ok(result) => Err(result
            .error
            .unwrap_or_else(|| "err.platformRejected".into())),
        Err(PlatformError::Timeout) => Err("err.platformTimeout".into()),
        Err(PlatformError::Cancelled) => Err("err.cancelled".into()),
        Err(PlatformError::Shutdown) => Err("err.shutdown".into()),
    };
    let _ = call.reply.send(reply);
}

/// Adapter for the two `ProgressSink` traits (same three methods, one in
/// `wdmm-queue` and one in `wdmm-jobs`).
struct ProgressAdapter(Arc<dyn QueueProgressSink>);

impl wdmm_jobs::ProgressSink for ProgressAdapter {
    fn bytes(&self, done: u64, total: Option<u64>) {
        self.0.bytes(done, total);
    }

    fn units(&self, done: u32, total: Option<u32>) {
        self.0.units(done, total);
    }

    fn note(&self, message: &str) {
        self.0.note(message);
    }
}

/// Placeholder driver for tasks that do not target a cloud account.
struct NoAccountVfs;

fn no_account<T>() -> VfsResult<T> {
    Err(DriverError::Other(
        "err.noAccount|task has no cloud account".into(),
    ))
}

#[async_trait]
impl WdmmVfs for NoAccountVfs {
    async fn list(&self, _path: &str) -> VfsResult<Vec<CloudFileItem>> {
        no_account()
    }

    async fn get(&self, _path: &str) -> VfsResult<CloudFileItem> {
        no_account()
    }

    async fn mkdir(&self, _path: &str) -> VfsResult<()> {
        no_account()
    }

    async fn rename(&self, _old_path: &str, _new_path: &str) -> VfsResult<()> {
        no_account()
    }

    async fn remove(&self, _path: &str) -> VfsResult<()> {
        no_account()
    }
}

/// Convenience used by [`crate::queue_api`] to build the production runner.
pub fn production_runner(db: Arc<LibraryDatabase>, paths: JobPaths) -> QueueRunner {
    QueueRunner::new(JobRegistry::with_defaults(), db, paths)
}
