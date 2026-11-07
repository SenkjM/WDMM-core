//! `MatchRemote`: compare library rows with what the remote account actually has
//! and set/clear the remote-missing marks.
//!
//! Owner: T5. Stub for now.

use async_trait::async_trait;

use wdmm_queue::model::TaskKind;

use crate::{Job, JobCtx, JobOutcome};

pub struct MatchRemoteJob;

#[async_trait]
impl Job for MatchRemoteJob {
    fn kind(&self) -> TaskKind {
        TaskKind::MatchRemote
    }

    async fn run(&self, _ctx: JobCtx) -> JobOutcome {
        JobOutcome::unimplemented("match_remote")
    }
}