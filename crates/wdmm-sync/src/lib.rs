//! `wdmm-sync` — incremental sync engine (credentials / playlists / library / manga).
//!
//! Sync strategy:
//! - Credential sync: sync account credentials to/from remote storage
//! - Library sync: sync track metadata
//! - Playlist sync: sync playlist definitions

#![forbid(unsafe_code)]

use async_trait::async_trait;
use std::sync::Arc;
use thiserror::Error;
use wdmm_core::models::{CloudAccount, LibraryTrack};
use wdmm_core::rev_clock::RevClock;
use wdmm_storage::{LibraryDatabase, StorageError};

#[derive(Debug, Error)]
pub enum SyncError {
    #[error("driver error: {0}")]
    Driver(String),

    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

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

// ── Credential Sync ───────────────────────────────────────────────────────────

/// Credential sync engine — syncs cloud account credentials.
pub struct CredentialSyncEngine {
    db: Arc<LibraryDatabase>,
}

impl CredentialSyncEngine {
    pub fn new(db: Arc<LibraryDatabase>) -> Self {
        Self { db }
    }

    /// Simulate pulling credentials from remote storage.
    async fn fetch_remote_accounts(&self) -> SyncResult<Vec<CloudAccount>> {
        // Stub: in real impl, this would call a WebDAV/remote API
        Ok(vec![])
    }

    /// Simulate pushing credentials to remote storage.
    async fn push_account(&self, _account: &CloudAccount) -> SyncResult<()> {
        // Stub: in real impl, this would upload to WebDAV/remote
        Ok(())
    }
}

#[async_trait]
impl SyncEngine for CredentialSyncEngine {
    async fn sync_incremental(&self) -> SyncResult<Vec<SyncOutcome>> {
        let mut outcomes = Vec::new();

        // Get local accounts
        let local_accounts = self.db.list_accounts()?;

        // Get remote accounts (stub)
        let remote_accounts = self.fetch_remote_accounts().await?;

        // Simple merge: push all local accounts that aren't in remote
        for account in &local_accounts {
            let in_remote = remote_accounts.iter().any(|r| r.id == account.id);
            if !in_remote {
                self.push_account(account).await?;
                outcomes.push(SyncOutcome {
                    entity_id: account.id.clone(),
                    action: SyncAction::Pushed,
                    rev: RevClock::new(1),
                });
            } else {
                outcomes.push(SyncOutcome {
                    entity_id: account.id.clone(),
                    action: SyncAction::NoChange,
                    rev: RevClock::new(1),
                });
            }
        }

        Ok(outcomes)
    }

    async fn sync_full(&self) -> SyncResult<Vec<SyncOutcome>> {
        // Full sync is same as incremental for credentials
        self.sync_incremental().await
    }
}

// ── Library Sync ──────────────────────────────────────────────────────────────

/// Library sync engine — syncs track metadata.
pub struct LibrarySyncEngine {
    db: Arc<LibraryDatabase>,
}

impl LibrarySyncEngine {
    pub fn new(db: Arc<LibraryDatabase>) -> Self {
        Self { db }
    }

    /// Simulate fetching remote library tracks.
    async fn fetch_remote_tracks(&self) -> SyncResult<Vec<LibraryTrack>> {
        // Stub: in real impl, this would scan remote WebDAV directories
        Ok(vec![])
    }

    /// Simulate pushing a track to remote.
    async fn push_track(&self, _track: &LibraryTrack) -> SyncResult<()> {
        // Stub: in real impl, this would upload metadata to remote
        Ok(())
    }
}

#[async_trait]
impl SyncEngine for LibrarySyncEngine {
    async fn sync_incremental(&self) -> SyncResult<Vec<SyncOutcome>> {
        let mut outcomes = Vec::new();

        // Get local tracks
        let local_tracks = self.db.list_tracks()?;

        // Get remote tracks (stub)
        let remote_tracks = self.fetch_remote_tracks().await?;

        // Simple merge: push local tracks not in remote
        for track in &local_tracks {
            let in_remote = remote_tracks.iter().any(|r| r.id == track.id);
            if !in_remote {
                self.push_track(track).await?;
                outcomes.push(SyncOutcome {
                    entity_id: track.id.clone(),
                    action: SyncAction::Pushed,
                    rev: RevClock::new(1),
                });
            } else {
                outcomes.push(SyncOutcome {
                    entity_id: track.id.clone(),
                    action: SyncAction::NoChange,
                    rev: RevClock::new(1),
                });
            }
        }

        Ok(outcomes)
    }

    async fn sync_full(&self) -> SyncResult<Vec<SyncOutcome>> {
        // Full sync is same as incremental for library
        self.sync_incremental().await
    }
}

// ── Playlist Sync ─────────────────────────────────────────────────────────────

/// Playlist sync engine — syncs playlist definitions.
pub struct PlaylistSyncEngine {
    #[allow(dead_code)] // round 4: will read playlists table
    db: Arc<LibraryDatabase>,
}

impl PlaylistSyncEngine {
    pub fn new(db: Arc<LibraryDatabase>) -> Self {
        Self { db }
    }
}

#[async_trait]
impl SyncEngine for PlaylistSyncEngine {
    async fn sync_incremental(&self) -> SyncResult<Vec<SyncOutcome>> {
        // Stub: playlists table exists but no data yet
        Ok(vec![])
    }

    async fn sync_full(&self) -> SyncResult<Vec<SyncOutcome>> {
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use wdmm_core::models::CloudAccount;

    #[tokio::test]
    async fn credential_sync_happy_path() {
        // Create temp database
        let db = Arc::new(LibraryDatabase::open(Path::new(":memory:")).unwrap());

        // Add an account
        let account = CloudAccount {
            id: "acc1".into(),
            provider_type: "webdav".into(),
            display_name: "Test Account".into(),
            base_url: Some("https://example.com/dav/".into()),
            credentials_encrypted: vec![1, 2, 3, 4], // dummy encrypted data
        };
        db.insert_account(&account).unwrap();

        // Run credential sync
        let engine = CredentialSyncEngine::new(Arc::clone(&db));
        let outcomes = engine.sync_incremental().await.unwrap();

        // Should have one outcome
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].entity_id, "acc1");
        // Since remote is empty (stub), local should be pushed
        assert_eq!(outcomes[0].action, SyncAction::Pushed);
    }

    #[tokio::test]
    async fn library_sync_happy_path() {
        // Create temp database
        let db = Arc::new(LibraryDatabase::open(Path::new(":memory:")).unwrap());

        // Add a track
        let track = LibraryTrack {
            id: "track1".into(),
            title: "Test Track".into(),
            artist: Some("Artist".into()),
            remote_path: "/music/test.mp3".into(),
            account_id: "acc1".into(),
            size: 12345,
            modified: None,
        };
        db.insert_track(&track).unwrap();

        // Run library sync
        let engine = LibrarySyncEngine::new(Arc::clone(&db));
        let outcomes = engine.sync_incremental().await.unwrap();

        // Should have one outcome
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].entity_id, "track1");
        assert_eq!(outcomes[0].action, SyncAction::Pushed);
    }

    #[tokio::test]
    async fn playlist_sync_stub() {
        let db = Arc::new(LibraryDatabase::open(Path::new(":memory:")).unwrap());
        let engine = PlaylistSyncEngine::new(Arc::clone(&db));
        let outcomes = engine.sync_incremental().await.unwrap();
        assert!(outcomes.is_empty()); // No playlists yet
    }
}
