//! Uploads: materialise a SAF URI through the platform bridge, then push the
//! bytes through `WdmmVfs::put_content`.
//!
//! Owner: T3. Stub for now.

use async_trait::async_trait;

use wdmm_queue::model::TaskKind;

use crate::{Job, JobCtx, JobOutcome};

pub struct UploadJob;

#[async_trait]
impl Job for UploadJob {
    fn kind(&self) -> TaskKind {
        TaskKind::Upload
    }

    async fn run(&self, _ctx: JobCtx) -> JobOutcome {
        JobOutcome::unimplemented("upload")
    }
}