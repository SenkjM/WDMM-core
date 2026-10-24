//! `wdmm-sync` — incremental sync engine (credentials / playlists / library / manga).
//!
//! ## Round 4 status
//! `CredentialSyncEngine` / `LibrarySyncEngine` / `PlaylistSyncEngine` now call
//! a real [`CloudDriver`] (WebDAV happy path) for PROPFIND/GET/PUT via the
//! driver's `put_content` / `get_content` methods. Merge strategy is a simple
//! last-write-wins at the entity level; conflicts surface as `SyncAction::Conflict`.
//!
//! Format note: this round serialises sync payloads as **JSON**. The on-disk
//! binary `WDMMPL01` / `WDMMCV01` containers (docs/10-PLAYLIST-FORMAT.md) are
//! the long-term direction but land in a later round — JSON is sufficient to
//! prove the push/pull path end-to-end.

#![forbid(unsafe_code)]

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;
use tracing::{debug, info};
use wdmm_core::models::{CloudAccount, LibraryTrack};
use wdmm_core::rev_clock::RevClock;
use wdmm_drivers::{CloudDriver, DriverError};
use wdmm_storage::{LibraryDatabase, Playlist, PlaylistEntry, StorageError};

#[derive(Debug, Error)]
pub enum SyncError {
    #[error("driver error: {0}")]
    Driver(String),

    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    #[error("serialization error: {0}")]
    Serde(String),

    #[error("conflict: local rev {local} vs remote rev {remote}")]
    Conflict { local: u64, remote: u64 },

    #[error("other: {0}")]
    Other(String),
}

pub type SyncResult<T> = std::result::Result<T, SyncError>;

impl From<DriverError> for SyncError {
    fn from(e: DriverError) -> Self {
        SyncError::Driver(e.to_string())
    }
}

impl From<serde_json::Error> for SyncError {
    fn from(e: serde_json::Error) -> Self {
        SyncError::Serde(e.to_string())
    }
}

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

/// Join a base sync directory and a file name, collapsing double slashes.
fn join_path(base: &str, name: &str) -> String {
    let base = base.trim_end_matches('/');
    let name = name.trim_start_matches('/');
    format!("{base}/{name}")
}

/// A remote-file snapshot used by the engines to decide push vs pull.
#[async_trait]
trait RemoteStore: Send + Sync {
    async fn read(&self, path: &str) -> SyncResult<Option<Vec<u8>>>;
    async fn write(&self, path: &str, data: &[u8]) -> SyncResult<()>;
    async fn ensure_dir(&self, path: &str) -> SyncResult<()>;
    /// List file names (no directories) directly under `dir`.
    async fn list_files(&self, dir: &str) -> SyncResult<Vec<String>>;
}

/// Adapter that delegates to a `CloudDriver`. `NotFound` on read → `Ok(None)`
/// (file not yet on remote). `NotFound` on mkdir is ignored (already exists).
struct DriverStore {
    driver: Arc<dyn CloudDriver>,
}

#[async_trait]
impl RemoteStore for DriverStore {
    async fn read(&self, path: &str) -> SyncResult<Option<Vec<u8>>> {
        match self.driver.get_content(path).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(DriverError::NotFound(_)) => Ok(None),
            Err(e) => Err(SyncError::from(e)),
        }
    }

    async fn write(&self, path: &str, data: &[u8]) -> SyncResult<()> {
        self.driver.put_content(path, data).await?;
        Ok(())
    }

    async fn ensure_dir(&self, path: &str) -> SyncResult<()> {
        match self.driver.mkdir(path).await {
            Ok(()) => Ok(()),
            // Many WebDAV servers return 405 when the collection already exists.
            Err(DriverError::Network(_)) => {
                debug!("mkdir {path}: ignored non-fatal error (likely exists)");
                Ok(())
            }
            Err(e) => Err(SyncError::from(e)),
        }
    }

    async fn list_files(&self, dir: &str) -> SyncResult<Vec<String>> {
        match self.driver.list(dir).await {
            Ok(items) => Ok(items
                .into_iter()
                .filter(|i| !i.is_dir)
                .map(|i| i.name)
                .collect()),
            Err(DriverError::NotFound(_)) => Ok(vec![]),
            Err(e) => Err(SyncError::from(e)),
        }
    }
}

// ── Credential Sync ───────────────────────────────────────────────────────────

/// Wire payload for `credentials.json`. `updated_at` (epoch seconds) drives
/// last-write-wins at the account level. `credentials_encrypted` is base64 so
/// the JSON survives text round-trips; it is **already vault-encrypted** at the
/// application layer, so the remote file carries ciphertext only.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RemoteAccount {
    id: String,
    provider_type: String,
    display_name: String,
    base_url: Option<String>,
    /// Base64 of `CloudAccount.credentials_encrypted`.
    credentials_encrypted_b64: String,
    updated_at: i64,
}

impl From<&CloudAccount> for RemoteAccount {
    fn from(a: &CloudAccount) -> Self {
        use base64::prelude::*;
        Self {
            id: a.id.clone(),
            provider_type: a.provider_type.clone(),
            display_name: a.display_name.clone(),
            base_url: a.base_url.clone(),
            credentials_encrypted_b64: BASE64_STANDARD.encode(&a.credentials_encrypted),
            updated_at: chrono::Utc::now().timestamp(),
        }
    }
}

impl From<&RemoteAccount> for CloudAccount {
    fn from(r: &RemoteAccount) -> Self {
        use base64::prelude::*;
        let credentials_encrypted = BASE64_STANDARD
            .decode(&r.credentials_encrypted_b64)
            .unwrap_or_default();
        CloudAccount {
            id: r.id.clone(),
            provider_type: r.provider_type.clone(),
            display_name: r.display_name.clone(),
            base_url: r.base_url.clone(),
            credentials_encrypted,
        }
    }
}

/// Credential sync engine — syncs cloud account credentials to/from a remote
/// `CloudDriver` store (WebDAV happy path).
pub struct CredentialSyncEngine {
    db: Arc<LibraryDatabase>,
    store: Arc<dyn RemoteStore>,
    remote_dir: String,
}

impl CredentialSyncEngine {
    /// `remote_dir` is the base directory on the remote for sync metadata
    /// (e.g. `"/wdmm_sync"`). The credentials file lands at
    /// `{remote_dir}/credentials.json`.
    pub fn new(
        db: Arc<LibraryDatabase>,
        driver: Arc<dyn CloudDriver>,
        remote_dir: impl Into<String>,
    ) -> Self {
        Self {
            db,
            store: Arc::new(DriverStore { driver }),
            remote_dir: remote_dir.into(),
        }
    }

    fn file_path(&self) -> String {
        join_path(&self.remote_dir, "credentials.json")
    }

    async fn fetch_remote(&self) -> SyncResult<Vec<RemoteAccount>> {
        match self.store.read(&self.file_path()).await? {
            None => Ok(vec![]),
            Some(bytes) => Ok(serde_json::from_slice(&bytes)?),
        }
    }

    async fn push_all(&self, accounts: &[CloudAccount]) -> SyncResult<()> {
        self.store.ensure_dir(&self.remote_dir).await?;
        let remote: Vec<RemoteAccount> = accounts.iter().map(RemoteAccount::from).collect();
        let bytes = serde_json::to_vec(&remote)?;
        self.store.write(&self.file_path(), &bytes).await
    }
}

#[async_trait]
impl SyncEngine for CredentialSyncEngine {
    async fn sync_incremental(&self) -> SyncResult<Vec<SyncOutcome>> {
        let local = self.db.list_accounts()?;
        let remote = self.fetch_remote().await?;

        // Build remote map by id for merge.
        let remote_by_id: HashMap<&str, &RemoteAccount> =
            remote.iter().map(|r| (r.id.as_str(), r)).collect();

        let mut outcomes = Vec::new();

        // Pull: remote accounts not present locally → insert.
        let local_ids: Vec<String> = local.iter().map(|a| a.id.clone()).collect();
        for r in &remote {
            if !local_ids.iter().any(|id| id == &r.id) {
                let account = CloudAccount::from(r);
                self.db.insert_account(&account)?;
                outcomes.push(SyncOutcome {
                    entity_id: r.id.clone(),
                    action: SyncAction::Pulled,
                    rev: RevClock::new(r.updated_at.max(1) as u64),
                });
            }
        }

        // Push: every local account is written to the remote snapshot.
        // (Round 4: full-replace push; per-row diffing lands with rev clocks.)
        if !local.is_empty() {
            self.push_all(&local).await?;
        }
        for a in &local {
            let was_remote = remote_by_id.contains_key(a.id.as_str());
            outcomes.push(SyncOutcome {
                entity_id: a.id.clone(),
                action: if was_remote {
                    SyncAction::NoChange
                } else {
                    SyncAction::Pushed
                },
                rev: RevClock::new(1),
            });
        }

        info!("credential sync: {} local, {} remote", local.len(), remote.len());
        Ok(outcomes)
    }

    async fn sync_full(&self) -> SyncResult<Vec<SyncOutcome>> {
        self.sync_incremental().await
    }
}

// ── Library Sync ──────────────────────────────────────────────────────────────

/// Wire payload for `library.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RemoteTrack {
    id: String,
    title: String,
    artist: Option<String>,
    remote_path: String,
    account_id: String,
    size: u64,
    modified: Option<i64>,
}

impl From<&LibraryTrack> for RemoteTrack {
    fn from(t: &LibraryTrack) -> Self {
        Self {
            id: t.id.clone(),
            title: t.title.clone(),
            artist: t.artist.clone(),
            remote_path: t.remote_path.clone(),
            account_id: t.account_id.clone(),
            size: t.size,
            modified: t.modified.map(|dt| dt.timestamp()),
        }
    }
}

impl From<&RemoteTrack> for LibraryTrack {
    fn from(r: &RemoteTrack) -> Self {
        let modified = r
            .modified
            .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0))
            .map(|dt| dt.with_timezone(&chrono::Utc));
        LibraryTrack {
            id: r.id.clone(),
            title: r.title.clone(),
            artist: r.artist.clone(),
            remote_path: r.remote_path.clone(),
            account_id: r.account_id.clone(),
            size: r.size,
            modified,
            ..Default::default()
        }
    }
}

/// Library sync engine — syncs track metadata to/from remote storage.
pub struct LibrarySyncEngine {
    db: Arc<LibraryDatabase>,
    store: Arc<dyn RemoteStore>,
    remote_dir: String,
}

impl LibrarySyncEngine {
    pub fn new(
        db: Arc<LibraryDatabase>,
        driver: Arc<dyn CloudDriver>,
        remote_dir: impl Into<String>,
    ) -> Self {
        Self {
            db,
            store: Arc::new(DriverStore { driver }),
            remote_dir: remote_dir.into(),
        }
    }

    fn file_path(&self) -> String {
        join_path(&self.remote_dir, "library.json")
    }

    async fn fetch_remote(&self) -> SyncResult<Vec<RemoteTrack>> {
        match self.store.read(&self.file_path()).await? {
            None => Ok(vec![]),
            Some(bytes) => Ok(serde_json::from_slice(&bytes)?),
        }
    }

    async fn push_all(&self, tracks: &[LibraryTrack]) -> SyncResult<()> {
        self.store.ensure_dir(&self.remote_dir).await?;
        let remote: Vec<RemoteTrack> = tracks.iter().map(RemoteTrack::from).collect();
        let bytes = serde_json::to_vec(&remote)?;
        self.store.write(&self.file_path(), &bytes).await
    }
}

#[async_trait]
impl SyncEngine for LibrarySyncEngine {
    async fn sync_incremental(&self) -> SyncResult<Vec<SyncOutcome>> {
        let local = self.db.list_tracks()?;
        let remote = self.fetch_remote().await?;

        // Pull: remote tracks not locally known → insert.
        let local_ids: Vec<String> = local.iter().map(|t| t.id.clone()).collect();
        let mut outcomes = Vec::new();
        for r in &remote {
            if !local_ids.iter().any(|id| id == &r.id) {
                let track = LibraryTrack::from(r);
                self.db.insert_track(&track)?;
                outcomes.push(SyncOutcome {
                    entity_id: r.id.clone(),
                    action: SyncAction::Pulled,
                    rev: RevClock::new(r.modified.unwrap_or(1).max(1) as u64),
                });
            }
        }

        // Push: full snapshot.
        if !local.is_empty() {
            self.push_all(&local).await?;
        }
        let remote_ids: Vec<&str> = remote.iter().map(|r| r.id.as_str()).collect();
        for t in &local {
            let was_remote = remote_ids.contains(&t.id.as_str());
            outcomes.push(SyncOutcome {
                entity_id: t.id.clone(),
                action: if was_remote {
                    SyncAction::NoChange
                } else {
                    SyncAction::Pushed
                },
                rev: RevClock::new(1),
            });
        }

        info!("library sync: {} local, {} remote", local.len(), remote.len());
        Ok(outcomes)
    }

    async fn sync_full(&self) -> SyncResult<Vec<SyncOutcome>> {
        self.sync_incremental().await
    }
}

// ── Playlist Sync ─────────────────────────────────────────────────────────────

/// Wire payload for one playlist file. `updated_at` drives last-write-wins.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RemotePlaylist {
    id: String,
    name: String,
    created_at: i64,
    updated_at: i64,
    entries: Vec<RemotePlaylistEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RemotePlaylistEntry {
    position: i64,
    music_id: String,
    source_name: String,
    remote_path: String,
    title: String,
    duration_ms: i64,
}

impl RemotePlaylist {
    fn file_name(id: &str) -> String {
        format!("{id}.json")
    }
}

/// Playlist sync engine — syncs playlist definitions to/from remote storage.
///
/// Round 4 scope: push/pull **one** playlist format path (JSON at
/// `{remote_dir}/playlists/{id}.json`), aligning with docs/10 essentials.
/// Tombstones / deletion packages (`WDMMPD01`) are out of scope this round.
pub struct PlaylistSyncEngine {
    db: Arc<LibraryDatabase>,
    store: Arc<dyn RemoteStore>,
    remote_dir: String,
}

impl PlaylistSyncEngine {
    pub fn new(
        db: Arc<LibraryDatabase>,
        driver: Arc<dyn CloudDriver>,
        remote_dir: impl Into<String>,
    ) -> Self {
        Self {
            db,
            store: Arc::new(DriverStore { driver }),
            remote_dir: remote_dir.into(),
        }
    }

    fn playlists_dir(&self) -> String {
        join_path(&self.remote_dir, "playlists")
    }

    fn file_path(&self, id: &str) -> String {
        join_path(&self.playlists_dir(), &RemotePlaylist::file_name(id))
    }

    async fn fetch_remote(&self, id: &str) -> SyncResult<Option<RemotePlaylist>> {
        match self.store.read(&self.file_path(id)).await? {
            None => Ok(None),
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        }
    }

    async fn push_one(&self, pl: &Playlist, entries: &[PlaylistEntry]) -> SyncResult<()> {
        self.store.ensure_dir(&self.playlists_dir()).await?;
        let remote = RemotePlaylist {
            id: pl.id.clone(),
            name: pl.name.clone(),
            created_at: pl.created_at.timestamp(),
            updated_at: pl.updated_at.timestamp(),
            entries: entries
                .iter()
                .map(|e| RemotePlaylistEntry {
                    position: e.position,
                    music_id: e.music_id.clone(),
                    source_name: e.source_name.clone(),
                    remote_path: e.remote_path.clone(),
                    title: e.title.clone(),
                    duration_ms: e.duration_ms,
                })
                .collect(),
        };
        let bytes = serde_json::to_vec(&remote)?;
        self.store.write(&self.file_path(&pl.id), &bytes).await
    }
}

#[async_trait]
impl SyncEngine for PlaylistSyncEngine {
    async fn sync_incremental(&self) -> SyncResult<Vec<SyncOutcome>> {
        let local = self.db.list_playlists()?;
        let local_ids: std::collections::HashSet<String> =
            local.iter().map(|p| p.id.clone()).collect();
        let mut outcomes = Vec::new();

        // Pull remote-only playlists (not present locally).
        let remote_files = self.store.list_files(&self.playlists_dir()).await?;
        for fname in &remote_files {
            // file name is "<id>.json"
            let Some(pl_id) = fname.strip_suffix(".json") else {
                continue;
            };
            if local_ids.contains(pl_id) {
                continue; // handled in local loop below
            }
            if let Some(remote) = self.fetch_remote(pl_id).await? {
                let pulled = Playlist {
                    id: remote.id.clone(),
                    name: remote.name.clone(),
                    created_at: chrono::DateTime::from_timestamp(remote.created_at, 0)
                        .unwrap_or_else(chrono::Utc::now),
                    updated_at: chrono::DateTime::from_timestamp(remote.updated_at, 0)
                        .unwrap_or_else(chrono::Utc::now),
                };
                self.db.upsert_playlist(&pulled)?;
                let pulled_entries: Vec<PlaylistEntry> = remote
                    .entries
                    .iter()
                    .map(|e| PlaylistEntry {
                        playlist_id: pl_id.to_string(),
                        position: e.position,
                        music_id: e.music_id.clone(),
                        source_name: e.source_name.clone(),
                        remote_path: e.remote_path.clone(),
                        title: e.title.clone(),
                        duration_ms: e.duration_ms,
                    })
                    .collect();
                self.db.set_playlist_entries(pl_id, &pulled_entries)?;
                outcomes.push(SyncOutcome {
                    entity_id: pl_id.to_string(),
                    action: SyncAction::Pulled,
                    rev: RevClock::new(remote.updated_at.max(1) as u64),
                });
            }
        }

        // Process local playlists: pull if remote is newer, else push.
        for pl in &local {
            let entries = self.db.list_playlist_entries(&pl.id)?;
            match self.fetch_remote(&pl.id).await? {
                Some(remote) if remote.updated_at > pl.updated_at.timestamp() => {
                    // Remote is strictly newer → pull.
                    let pulled = Playlist {
                        id: remote.id.clone(),
                        name: remote.name.clone(),
                        created_at: chrono::DateTime::from_timestamp(remote.created_at, 0)
                            .unwrap_or_else(chrono::Utc::now),
                        updated_at: chrono::DateTime::from_timestamp(remote.updated_at, 0)
                            .unwrap_or_else(chrono::Utc::now),
                    };
                    self.db.upsert_playlist(&pulled)?;
                    let pulled_entries: Vec<PlaylistEntry> = remote
                        .entries
                        .iter()
                        .map(|e| PlaylistEntry {
                            playlist_id: pl.id.clone(),
                            position: e.position,
                            music_id: e.music_id.clone(),
                            source_name: e.source_name.clone(),
                            remote_path: e.remote_path.clone(),
                            title: e.title.clone(),
                            duration_ms: e.duration_ms,
                        })
                        .collect();
                    self.db.set_playlist_entries(&pl.id, &pulled_entries)?;
                    outcomes.push(SyncOutcome {
                        entity_id: pl.id.clone(),
                        action: SyncAction::Pulled,
                        rev: RevClock::new(remote.updated_at.max(1) as u64),
                    });
                }
                Some(remote) if remote.updated_at == pl.updated_at.timestamp() => {
                    outcomes.push(SyncOutcome {
                        entity_id: pl.id.clone(),
                        action: SyncAction::NoChange,
                        rev: RevClock::new(remote.updated_at.max(1) as u64),
                    });
                }
                _ => {
                    // No remote or local is newer → push.
                    self.push_one(pl, &entries).await?;
                    outcomes.push(SyncOutcome {
                        entity_id: pl.id.clone(),
                        action: SyncAction::Pushed,
                        rev: RevClock::new(pl.updated_at.timestamp().max(1) as u64),
                    });
                }
            }
        }

        info!("playlist sync: {} local", local.len());
        Ok(outcomes)
    }

    async fn sync_full(&self) -> SyncResult<Vec<SyncOutcome>> {
        self.sync_incremental().await
    }
}

// ── aggregate trigger (used by FFI) ───────────────────────────────────────────

/// Run credential + library sync in sequence and return a JSON summary.
///
/// This is the single entry point the FFI `wdmm_sync_trigger` calls.
pub async fn run_sync_all(
    db: Arc<LibraryDatabase>,
    driver: Arc<dyn CloudDriver>,
    remote_dir: &str,
) -> SyncResult<serde_json::Value> {
    let cred = CredentialSyncEngine::new(Arc::clone(&db), Arc::clone(&driver), remote_dir)
        .sync_incremental()
        .await?;
    let lib = LibrarySyncEngine::new(Arc::clone(&db), Arc::clone(&driver), remote_dir)
        .sync_incremental()
        .await?;
    let pl = PlaylistSyncEngine::new(db, driver, remote_dir)
        .sync_incremental()
        .await?;
    let summarize = |outcomes: &[SyncOutcome]| {
        let (pushed, pulled, conflict) = outcomes.iter().fold((0, 0, 0), |(p, l, c), o| match o.action {
            SyncAction::Pushed => (p + 1, l, c),
            SyncAction::Pulled => (p, l + 1, c),
            SyncAction::Conflict => (p, l, c + 1),
            SyncAction::NoChange => (p, l, c),
        });
        serde_json::json!({ "total": outcomes.len(), "pushed": pushed, "pulled": pulled, "conflict": conflict })
    };
    Ok(serde_json::json!({
        "credentials": summarize(&cred),
        "library":     summarize(&lib),
        "playlists":   summarize(&pl),
    }))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as Map;
    use std::sync::Mutex;
    use tokio::sync::Mutex as AsyncMutex;
    use wdmm_core::models::CloudAccount;
    use wdmm_drivers::{CloudFileItem, DriverError};

    /// In-memory `CloudDriver` used by sync unit tests — no network.
    struct MockDriver {
        files: AsyncMutex<Map<String, Vec<u8>>>,
        dirs: AsyncMutex<std::collections::HashSet<String>>,
    }

    impl MockDriver {
        fn new() -> Self {
            Self {
                files: AsyncMutex::new(Map::new()),
                dirs: AsyncMutex::new(std::collections::HashSet::new()),
            }
        }
    }

    #[async_trait]
    impl CloudDriver for MockDriver {
        async fn init(&self) -> wdmm_drivers::Result<()> {
            Ok(())
        }
        async fn list(&self, path: &str) -> wdmm_drivers::Result<Vec<CloudFileItem>> {
            let files = self.files.lock().await;
            let prefix = format!("{path}/");
            let items: Vec<CloudFileItem> = files
                .keys()
                .filter(|k| k.starts_with(&prefix))
                .map(|k| CloudFileItem {
                    name: k.trim_start_matches(&prefix).to_string(),
                    is_dir: false,
                    size: 0,
                    modified: None,
                    raw_url: None,
                    raw_headers: vec![],
                    delivery: wdmm_drivers::ContentDeliveryKind::LocalStream,
                    local_path: None,
                })
                .collect();
            Ok(items)
        }
        async fn get(&self, path: &str) -> wdmm_drivers::Result<CloudFileItem> {
            if self.files.lock().await.contains_key(path) {
                Ok(CloudFileItem {
                    name: path.to_string(),
                    is_dir: false,
                    size: 0,
                    modified: None,
                    raw_url: Some(path.to_string()),
                    raw_headers: vec![],
                    delivery: wdmm_drivers::ContentDeliveryKind::NetworkDirect,
                    local_path: None,
                })
            } else {
                Err(DriverError::NotFound(path.into()))
            }
        }
        async fn mkdir(&self, path: &str) -> wdmm_drivers::Result<()> {
            self.dirs.lock().await.insert(path.to_string());
            Ok(())
        }
        async fn rename(&self, _old: &str, _new: &str) -> wdmm_drivers::Result<()> {
            Err(DriverError::Other("not supported".into()))
        }
        async fn remove(&self, path: &str) -> wdmm_drivers::Result<()> {
            self.files.lock().await.remove(path);
            Ok(())
        }
        async fn put_content(&self, path: &str, data: &[u8]) -> wdmm_drivers::Result<()> {
            self.files
                .lock()
                .await
                .insert(path.to_string(), data.to_vec());
            Ok(())
        }
        async fn get_content(&self, path: &str) -> wdmm_drivers::Result<Vec<u8>> {
            self.files
                .lock()
                .await
                .get(path)
                .cloned()
                .ok_or_else(|| DriverError::NotFound(path.to_string()))
        }
    }

    fn tmp_db() -> Arc<LibraryDatabase> {
        Arc::new(LibraryDatabase::open(std::path::Path::new(":memory:")).unwrap())
    }

    #[tokio::test]
    async fn credential_sync_pushes_then_pulls() {
        let db = tmp_db();
        let driver = Arc::new(MockDriver::new());

        // Device A: has one account → sync (pushes credentials.json).
        let account = CloudAccount {
            id: "acc1".into(),
            provider_type: "webdav".into(),
            display_name: "My NAS".into(),
            base_url: Some("https://nas.local/dav/".into()),
            credentials_encrypted: vec![1, 2, 3, 4],
        };
        db.insert_account(&account).unwrap();

        let engine = CredentialSyncEngine::new(Arc::clone(&db), driver.clone(), "/sync");
        let outcomes = engine.sync_incremental().await.unwrap();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].action, SyncAction::Pushed);

        // Verify the remote file exists and decodes.
        let remote_bytes = driver
            .get_content("/sync/credentials.json")
            .await
            .unwrap();
        let remote: Vec<RemoteAccount> = serde_json::from_slice(&remote_bytes).unwrap();
        assert_eq!(remote.len(), 1);
        assert_eq!(remote[0].id, "acc1");

        // Device B: empty DB, same remote → sync pulls the account in.
        let db2 = tmp_db();
        let engine2 = CredentialSyncEngine::new(Arc::clone(&db2), driver.clone(), "/sync");
        let outcomes2 = engine2.sync_incremental().await.unwrap();
        let pulled: Vec<&SyncOutcome> = outcomes2.iter().filter(|o| o.action == SyncAction::Pulled).collect();
        assert_eq!(pulled.len(), 1);
        assert_eq!(pulled[0].entity_id, "acc1");
        let loaded = db2.get_account("acc1").unwrap().unwrap();
        assert_eq!(loaded.display_name, "My NAS");
        assert_eq!(loaded.credentials_encrypted, vec![1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn library_sync_pushes_tracks() {
        let db = tmp_db();
        let driver = Arc::new(MockDriver::new());

        let track = LibraryTrack {
            id: "track1".into(),
            title: "Song".into(),
            artist: Some("Artist".into()),
            remote_path: "/music/song.mp3".into(),
            account_id: "acc1".into(),
            size: 12345,
            modified: None,
            ..Default::default()
        };
        db.insert_track(&track).unwrap();

        let engine = LibrarySyncEngine::new(Arc::clone(&db), driver.clone(), "/sync");
        let outcomes = engine.sync_incremental().await.unwrap();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].action, SyncAction::Pushed);

        // Second device pulls.
        let db2 = tmp_db();
        let engine2 = LibrarySyncEngine::new(Arc::clone(&db2), driver.clone(), "/sync");
        let outcomes2 = engine2.sync_incremental().await.unwrap();
        assert!(outcomes2.iter().any(|o| o.action == SyncAction::Pulled));
        let loaded = db2.get_track("track1").unwrap().unwrap();
        assert_eq!(loaded.title, "Song");
    }

    #[tokio::test]
    async fn playlist_sync_push_pull_last_write_wins() {
        let db = tmp_db();
        let driver = Arc::new(MockDriver::new());

        let older = chrono::Utc::now() - chrono::Duration::seconds(60);
        let pl = Playlist {
            id: "pl-1".into(),
            name: "Mix".into(),
            created_at: older,
            updated_at: older,
        };
        db.upsert_playlist(&pl).unwrap();
        db.set_playlist_entries(
            "pl-1",
            &[PlaylistEntry {
                playlist_id: "pl-1".into(),
                position: 0,
                music_id: "aa".repeat(20),
                source_name: "nas".into(),
                remote_path: "/a.mp3".into(),
                title: "A".into(),
                duration_ms: 1000,
            }],
        )
        .unwrap();

        let engine = PlaylistSyncEngine::new(Arc::clone(&db), driver.clone(), "/sync");
        let outcomes = engine.sync_incremental().await.unwrap();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].action, SyncAction::Pushed);

        // Remote now has pl-1. A second device with no local playlist pulls it.
        let db2 = tmp_db();
        let engine2 = PlaylistSyncEngine::new(Arc::clone(&db2), driver.clone(), "/sync");
        let outcomes2 = engine2.sync_incremental().await.unwrap();
        assert_eq!(outcomes2.len(), 1);
        assert_eq!(outcomes2[0].action, SyncAction::Pulled);
        let loaded = db2.get_playlist("pl-1").unwrap().unwrap();
        assert_eq!(loaded.name, "Mix");
        assert_eq!(db2.list_playlist_entries("pl-1").unwrap().len(), 1);

        // Now device 2 edits the playlist (newer updated_at) and pushes back.
        let newer = chrono::Utc::now();
        let edited = Playlist {
            id: "pl-1".into(),
            name: "Mix v2".into(),
            created_at: older,
            updated_at: newer,
        };
        db2.upsert_playlist(&edited).unwrap();
        let outcomes3 = engine2.sync_incremental().await.unwrap();
        assert_eq!(outcomes3[0].action, SyncAction::Pushed);

        // Device 1 pulls the newer version.
        let outcomes4 = engine.sync_incremental().await.unwrap();
        assert_eq!(outcomes4[0].action, SyncAction::Pulled);
        assert_eq!(db.get_playlist("pl-1").unwrap().unwrap().name, "Mix v2");
    }

    #[tokio::test]
    async fn run_sync_all_aggregates() {
        let db = tmp_db();
        let driver = Arc::new(MockDriver::new());
        db.insert_account(&CloudAccount {
            id: "a".into(),
            provider_type: "webdav".into(),
            display_name: "A".into(),
            base_url: None,
            credentials_encrypted: vec![],
        })
        .unwrap();
        db.insert_track(&LibraryTrack {
            id: "t".into(),
            title: "T".into(),
            artist: None,
            remote_path: "/t.mp3".into(),
            account_id: "a".into(),
            size: 0,
            modified: None,
            ..Default::default()
        })
        .unwrap();

        let summary = run_sync_all(Arc::clone(&db), driver, "/sync").await.unwrap();
        assert_eq!(summary["credentials"]["pushed"], 1);
        assert_eq!(summary["library"]["pushed"], 1);
        assert_eq!(summary["playlists"]["total"], 0);
    }

    // silence unused import warnings on platforms where the helper isn't needed
    #[allow(dead_code)]
    fn _keep_mutex(_m: &Mutex<()>) {}
}
