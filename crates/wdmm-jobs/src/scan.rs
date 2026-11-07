//! Library scans: music/video tag ingest plus CUE album ingestion.
//!
//! A scan task owns its own discovery: given a folder root it walks it (reusing
//! `wdmm-ffi::ext::walk`'s BFS logic once moved here), and when given a payload
//! envelope it processes the listed paths. `units_total` grows while discovering.
//!
//! Owner: T4. Stub for now.

use async_trait::async_trait;

use wdmm_queue::model::TaskKind;

use crate::{Job, JobCtx, JobOutcome};

pub struct ScanJob;

#[async_trait]
impl Job for ScanJob {
    fn kind(&self) -> TaskKind {
        TaskKind::Scan
    }

    async fn run(&self, _ctx: JobCtx) -> JobOutcome {
        JobOutcome::unimplemented("scan")
    }
}