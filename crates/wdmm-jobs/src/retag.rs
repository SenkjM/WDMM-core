//! `RetagCloud`: re-read remote tags and overwrite the library row, clearing any
//! "remote missing" marks it revives.
//!
//! Owner: T5. Stub for now.

use async_trait::async_trait;

use wdmm_queue::model::TaskKind;

use crate::{Job, JobCtx, JobOutcome};

pub struct RetagCloudJob;

#[async_trait]
impl Job for RetagCloudJob {
    fn kind(&self) -> TaskKind {
        TaskKind::RetagCloud
    }

    async fn run(&self, _ctx: JobCtx) -> JobOutcome {
        JobOutcome::unimplemented("retag_cloud")
    }
}