//! `wdmm-sync` — incremental sync engine (credentials / playlists / library / manga).

#![forbid(unsafe_code)]

use async_trait::async_trait;
use thiserror::Error;
use wdmm_core::rev_clock::RevClock;

#[derive(Debug, Error)]
pub enum SyncError {
    #[error("driver error: {0}")]
    Driver(String),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("conflict: local rev {local} vs remote rev {remote}")]
    Conflict { local: u64, remote: u64 },

    #[error("other: {0}")]
    Other(String),
}

pub type SyncResult<T> = std::result::Result<T, SyncError>;

/// Sync outcome for one entity.
#[derive(Debug, Clone)]
pub struct SyncOutcome {
    pub entity_id: String,
    pub action: SyncAction,
    pub rev: RevClock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncAction {
    NoChange,
    Pushed,
    Pulled,
    Conflict,
}

/// Sync engine trait — one impl per sync kind (credentials / playlists / library).
#[async_trait]
pub trait SyncEngine: Send + Sync {
    async fn sync_incremental(&self) -> SyncResult<Vec<SyncOutcome>>;
    async fn sync_full(&self) -> SyncResult<Vec<SyncOutcome>>;
}

/// Stub credential sync.
pub struct CredentialSyncEngine;

#[async_trait]
impl SyncEngine for CredentialSyncEngine {
    async fn sync_incremental(&self) -> SyncResult<Vec<SyncOutcome>> {
        // TODO: read vault, compare rev, push/pull
        Ok(vec![])
    }

    async fn sync_full(&self) -> SyncResult<Vec<SyncOutcome>> {
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stub_sync() {
        let engine = CredentialSyncEngine;
        let outcomes = engine.sync_incremental().await.unwrap();
        assert!(outcomes.is_empty());
    }
}
