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

mod backup;
pub use backup::*;

pub mod cover;
pub use cover::{
    build_cover_section, detect_image_kind, manga_cover_stem, music_cover_stem,
    normalize_remote_path_for_cover, normalize_source_name, parse_cover_section, CoverRoots,
    COVER_KIND_JPEG, COVER_KIND_NONE, COVER_KIND_PNG, COVER_KIND_WEBP,
};

pub mod wmp;
mod library_shard;
mod library_music;
pub use library_music::LibrarySyncEngine;
mod library_manga_shard;
pub use library_manga_shard::{
    decode_manga_shard, encode_manga_shard, encode_manga_tomb_shard, DecodedMangaShard, MangaSyncRow,
};
mod library_manga;
pub use library_manga::MangaLibrarySyncEngine;

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

// Library sync engine lives in `library_music` (shard+tombstone protocol).
// Toy `library.json` path removed.

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
    let manga = MangaLibrarySyncEngine::new(Arc::clone(&db), Arc::clone(&driver), remote_dir)
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
        "manga":       summarize(&manga),
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
        pub files: AsyncMutex<Map<String, Vec<u8>>>,
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
        use wdmm_core::models::LibraryTrack;
        let db = tmp_db();
        let driver = Arc::new(MockDriver::new());

        let track = LibraryTrack {
            id: wdmm_core::identity::track_identity("/music/song.mp3"),
            title: "Song".into(),
            artist: Some("Artist".into()),
            remote_path: "/music/song.mp3".into(),
            account_id: "acc1".into(),
            size: 12345,
            modified: None,
            rev: 100,
            ..Default::default()
        };
        db.insert_track(&track).unwrap();

        let engine = LibrarySyncEngine::new(Arc::clone(&db), driver.clone(), "/sync");
        let outcomes = engine.sync_incremental().await.unwrap();
        assert!(outcomes.iter().any(|o| o.action == SyncAction::Pushed));

        // Manifest + segment under library/music/
        let files = driver.files.lock().await;
        assert!(files.contains_key("/sync/library/music/index.json"));
        assert!(files.keys().any(|k| k.contains("/sync/library/music/seg-")));
        drop(files);

        // Second device pulls.
        let db2 = tmp_db();
        let engine2 = LibrarySyncEngine::new(Arc::clone(&db2), driver.clone(), "/sync");
        let outcomes2 = engine2.sync_incremental().await.unwrap();
        assert!(outcomes2.iter().any(|o| o.action == SyncAction::Pulled));
        let loaded = db2
            .get_track(&wdmm_core::identity::track_identity("/music/song.mp3"))
            .unwrap()
            .unwrap();
        assert_eq!(loaded.title, "Song");
        assert_eq!(loaded.rev, 100);
    }

    #[tokio::test]
    async fn library_sync_tombstone_peer_deletes() {
        use wdmm_core::models::LibraryTrack;
        use wdmm_storage::DeletedTrack;

        let driver = Arc::new(MockDriver::new());
        let path = "/music/gone.mp3";
        let id = wdmm_core::identity::track_identity(path);

        // Device A has the track, syncs it up.
        let db_a = tmp_db();
        db_a
            .insert_track(&LibraryTrack {
                id: id.clone(),
                title: "Gone".into(),
                remote_path: path.into(),
                account_id: "nas".into(),
                rev: 50,
                ..Default::default()
            })
            .unwrap();
        LibrarySyncEngine::new(Arc::clone(&db_a), driver.clone(), "/sync")
            .sync_incremental()
            .await
            .unwrap();

        // Device B pulls.
        let db_b = tmp_db();
        LibrarySyncEngine::new(Arc::clone(&db_b), driver.clone(), "/sync")
            .sync_incremental()
            .await
            .unwrap();
        assert!(db_b.get_track(&id).unwrap().is_some());

        // Device A deletes locally and publishes tomb.
        db_a.delete_track(&id).unwrap();
        db_a
            .upsert_deleted_track(&DeletedTrack {
                source_name: "nas".into(),
                remote_path: path.into(),
                rev: 60,
                deleted_at: "2026-10-26T00:00:00Z".into(),
                pushed: 0,
            })
            .unwrap();
        LibrarySyncEngine::new(Arc::clone(&db_a), driver.clone(), "/sync")
            .sync_incremental()
            .await
            .unwrap();
        let files = driver.files.lock().await;
        assert!(files.keys().any(|k| k.contains("/sync/library/music/del-")));
        drop(files);

        // Device B adopts deletion.
        LibrarySyncEngine::new(Arc::clone(&db_b), driver.clone(), "/sync")
            .sync_incremental()
            .await
            .unwrap();
        assert!(db_b.get_track(&id).unwrap().is_none());
    }

    #[tokio::test]
    async fn library_sync_full_rebuild_base_shards() {
        use wdmm_core::models::LibraryTrack;
        let db = tmp_db();
        let driver = Arc::new(MockDriver::new());
        for i in 0..3 {
            let path = format!("/music/t{i}.mp3");
            db.insert_track(&LibraryTrack {
                id: wdmm_core::identity::track_identity(&path),
                title: format!("T{i}"),
                remote_path: path,
                account_id: "a".into(),
                rev: 10 + i,
                ..Default::default()
            })
            .unwrap();
        }
        // Seed a segment first
        LibrarySyncEngine::new(Arc::clone(&db), driver.clone(), "/sync")
            .sync_incremental()
            .await
            .unwrap();
        // Full rebuild
        LibrarySyncEngine::new(Arc::clone(&db), driver.clone(), "/sync")
            .sync_full()
            .await
            .unwrap();
        let files = driver.files.lock().await;
        let index: serde_json::Value =
            serde_json::from_slice(files.get("/sync/library/music/index.json").unwrap()).unwrap();
        assert_eq!(index["format"], "wmp_library_index");
        assert!(index["segments"].as_array().unwrap().is_empty());
        assert!(index["tombstones"].as_array().unwrap().is_empty());
        assert!(!index["base"]["shards"].as_array().unwrap().is_empty());
        assert!(files.keys().any(|k| k.contains("/sync/library/music/lib-")));
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
            rev: 1,
            ..Default::default()
        })
        .unwrap();

        let summary = run_sync_all(Arc::clone(&db), driver, "/sync").await.unwrap();
        assert_eq!(summary["credentials"]["pushed"], 1);
        assert!(summary["library"]["pushed"].as_u64().unwrap() >= 1);
        assert!(summary["manga"].is_object(), "manga key missing from summary");
        assert_eq!(summary["playlists"]["total"], 0);
    }

    // ── Manga tests ───────────────────────────────────────────────────────────

    fn mk_manga(n: u32, source: &str, path: &str, fmt: &str, rev: i64) -> wdmm_storage::Manga {
        use wdmm_storage::manga_id_for_remote;
        wdmm_storage::Manga {
            id: manga_id_for_remote(source, path),
            source_name: source.into(),
            remote_path: path.into(),
            title: format!("Manga {n}"),
            author: None,
            series: None,
            volume: None,
            chapter_number: None,
            page_count: 10,
            cover_relpath: None,
            format: fmt.into(),
            remote_missing: 0,
            page_turn: None,
            rev,
            updated_at: "2026-10-26T00:00:00Z".into(),
        }
    }

    #[tokio::test]
    async fn manga_sync_pushes_and_pulls() {
        let driver = Arc::new(MockDriver::new());
        let db_a = tmp_db();

        let m = mk_manga(1, "nas", "/manga/vol1.cbz", "cbz", 10);
        db_a.upsert_manga(&m).unwrap();

        let engine_a = MangaLibrarySyncEngine::new(Arc::clone(&db_a), driver.clone(), "/sync");
        let out_a = engine_a.sync_incremental().await.unwrap();
        assert!(out_a.iter().any(|o| o.action == SyncAction::Pushed));

        // manifest + segment created under library/manga/
        let files = driver.files.lock().await;
        assert!(files.contains_key("/sync/library/manga/index.json"));
        assert!(files.keys().any(|k| k.contains("/sync/library/manga/seg-")));
        drop(files);

        // second device pulls
        let db_b = tmp_db();
        let engine_b = MangaLibrarySyncEngine::new(Arc::clone(&db_b), driver.clone(), "/sync");
        let out_b = engine_b.sync_incremental().await.unwrap();
        assert!(out_b.iter().any(|o| o.action == SyncAction::Pulled));

        let all = db_b.list_mangas().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].title, "Manga 1");
        assert_eq!(all[0].rev, 10);
    }

    #[tokio::test]
    async fn manga_sync_tombstone_removes_peer_row() {
        // Tests the "images id key" tombstone path: for format=="images", manga_sync_key = id.
        // Device B has the manga locally (not via sync, since legacy-loose-images are skipped
        // on adopt). When device A pushes a tombstone with sync_key=id, device B must delete it.
        let driver = Arc::new(MockDriver::new());

        let path = "/manga/loose";
        let source = "nas";
        let db_a = tmp_db();
        let m = mk_manga(1, source, path, "images", 5);
        db_a.upsert_manga(&m).unwrap();

        // Device A deletes (writes DeletedManga with sync_key = m.id) then syncs the tombstone.
        db_a.delete_manga(&m.id).unwrap();
        MangaLibrarySyncEngine::new(Arc::clone(&db_a), driver.clone(), "/sync")
            .sync_incremental()
            .await
            .unwrap();

        let files = driver.files.lock().await;
        assert!(
            files.keys().any(|k| k.contains("/sync/library/manga/del-")),
            "del-* shard should exist after A publishes tomb"
        );
        drop(files);

        // Device B: has the same manga locally (directly inserted, same id ⇒ same sync_key).
        let db_b = tmp_db();
        db_b.upsert_manga(&m).unwrap();
        assert_eq!(db_b.list_mangas().unwrap().len(), 1);

        // Device B adopts deletion via tombstone.
        MangaLibrarySyncEngine::new(Arc::clone(&db_b), driver.clone(), "/sync")
            .sync_incremental()
            .await
            .unwrap();
        assert_eq!(
            db_b.list_mangas().unwrap().len(),
            0,
            "peer delete via tomb (images id key) should remove row on db_b"
        );
    }

    #[tokio::test]
    async fn manga_sync_full_rebuild_produces_base_shards() {
        let driver = Arc::new(MockDriver::new());
        let db = tmp_db();

        for i in 1u32..=3 {
            db.upsert_manga(&mk_manga(i, "nas", &format!("/m/{i}.cbz"), "cbz", i as i64))
                .unwrap();
        }

        let engine = MangaLibrarySyncEngine::new(Arc::clone(&db), driver.clone(), "/sync");
        let out = engine.sync_full().await.unwrap();
        assert!(out.iter().any(|o| o.entity_id == "manga-rebuild"));

        let files = driver.files.lock().await;
        assert!(files.contains_key("/sync/library/manga/index.json"));
        // should have lib-* shards, no seg-* or del-* after fresh rebuild
        assert!(files.keys().any(|k| k.contains("/sync/library/manga/lib-")));
        assert!(!files.keys().any(|k| k.contains("/sync/library/manga/seg-")));
        assert!(!files.keys().any(|k| k.contains("/sync/library/manga/del-")));
        drop(files);

        // manifest segments + tombstones empty
        let manifest_bytes = driver
            .get_content("/sync/library/manga/index.json")
            .await
            .unwrap();
        let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes).unwrap();
        assert_eq!(manifest["format"], "wmp_manga_library_index");
        let segs = manifest["segments"].as_array().unwrap();
        let tombs = manifest["tombstones"].as_array().unwrap();
        assert!(segs.is_empty(), "no segments after rebuild");
        assert!(tombs.is_empty(), "no tombstones after rebuild");
    }

    #[tokio::test]
    async fn manga_progress_lww() {
        let driver = Arc::new(MockDriver::new());
        let db = tmp_db();

        // seed a manga so progress.json upload has something
        let m = mk_manga(1, "nas", "/m/a.cbz", "cbz", 1);
        db.upsert_manga(&m).unwrap();

        // local progress is newer
        db.upsert_progress(&wdmm_storage::MangaProgress {
            manga_id: m.id.clone(),
            page_index: 5,
            updated_at: "2026-10-26T10:00:00Z".into(),
        })
        .unwrap();

        let engine = MangaLibrarySyncEngine::new(Arc::clone(&db), driver.clone(), "/sync");
        engine.sync_incremental().await.unwrap();

        // progress.json written
        let files = driver.files.lock().await;
        assert!(files.contains_key("/sync/library/manga/progress.json"));
        drop(files);

        // second device: cloud has page=5, local has page=2 (older timestamp) → cloud wins
        let db2 = tmp_db();
        let m2 = mk_manga(1, "nas", "/m/a.cbz", "cbz", 1);
        db2.upsert_manga(&m2).unwrap();
        db2.upsert_progress(&wdmm_storage::MangaProgress {
            manga_id: m2.id.clone(),
            page_index: 2,
            updated_at: "2026-10-25T08:00:00Z".into(),
        })
        .unwrap();

        let engine2 = MangaLibrarySyncEngine::new(Arc::clone(&db2), driver.clone(), "/sync");
        engine2.sync_incremental().await.unwrap();

        let prog = db2.list_progress().unwrap();
        let p = prog.iter().find(|p| p.manga_id == m2.id).unwrap();
        assert_eq!(p.page_index, 5, "cloud progress (page 5) should win over stale local (page 2)");
    }

    #[tokio::test]
    async fn run_sync_all_includes_manga() {
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
        db.upsert_manga(&mk_manga(1, "nas", "/m/a.cbz", "cbz", 1)).unwrap();

        let driver_dyn: Arc<dyn CloudDriver> = driver;
        let summary = run_sync_all(Arc::clone(&db), driver_dyn, "/sync")
            .await
            .unwrap();

        assert!(summary["manga"].is_object());
        assert!(
            summary["manga"]["pushed"].as_u64().unwrap() >= 1,
            "manga pushed expected"
        );
    }

    // silence unused import warnings on platforms where the helper isn't needed
    #[allow(dead_code)]
    fn _keep_mutex(_m: &Mutex<()>) {}
}
