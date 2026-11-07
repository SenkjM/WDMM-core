//! Downloads: cache targets write into the app cache, public targets go through
//! the platform bridge (SAF tree / MediaStore / Downloads).
//!
//! Owner: T3. Currently a stub so the scheduler can be wired end to end.

use async_trait::async_trait;

use wdmm_queue::model::TaskKind;

use crate::{Job, JobCtx, JobOutcome};

/// Serves `DownloadCache` (registered for that kind) and `DownloadPublic`
/// (the scheduler registers the same executor for both; behaviour branches on
/// `ctx.task.target`).
pub struct DownloadJob;

#[async_trait]
impl Job for DownloadJob {
    fn kind(&self) -> TaskKind {
        TaskKind::DownloadCache
    }

    async fn run(&self, _ctx: JobCtx) -> JobOutcome {
        JobOutcome::unimplemented("download")
    }
}