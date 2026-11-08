//! `wdmm-sync` — incremental sync engine (credentials / playlists / library / manga).
//!
//! ## Round 4 status
//! `CredentialSyncEngine` / `LibrarySyncEngine` / `PlaylistSyncEngine` now call
//! a real [`WdmmVfs`] (WebDAV happy path) for PROPFIND/GET/PUT via the
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

mod vault_crypto;
pub use vault_crypto::*;

pub mod cover;
pub use cover::{
    build_cover_section, detect_image_kind, manga_cover_stem, music_cover_stem,
    normalize_remote_path_for_cover, normalize_source_name, parse_cover_section, CoverRoots,
    COVER_KIND_JPEG, COVER_KIND_NONE, COVER_KIND_PNG, COVER_KIND_WEBP,
};

pub mod wmp;
pub(crate) mod protocol;
mod engine;
mod entities;
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
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use thiserror::Error;
use tracing::{debug, info};
use wdmm_core::models::CloudAccount;
use wdmm_core::rev_clock::RevClock;
use wdmm_drivers::{WdmmVfs, DriverError};
use wdmm_storage::{LibraryDatabase, Playlist, PlaylistEntry, StorageError, SyncState};

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

    /// The remote index was written by a newer or different format. Silently
    /// treating it as empty (the old behaviour) made one device wipe the other
    /// device's library on the next full sync, so this fails loudly instead.
    #[error("remote index format {found:?} is not supported (expected {expected})")]
    RemoteFormatUnsupported {
        found: Option<String>,
        expected: String,
    },

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

impl SyncAction {
    /// Wire name used by FFI summaries and per-playlist publish replies.
    pub fn as_str(&self) -> &'static str {
        match self {
            SyncAction::NoChange => "no_change",
            SyncAction::Pushed => "pushed",
            SyncAction::Pulled => "pulled",
            SyncAction::Conflict => "conflict",
        }
    }
}

/// Sync engine trait — one impl per sync kind (credentials / playlists / library).
#[async_trait]
pub trait SyncEngine: Send + Sync {
    async fn sync_incremental(&self) -> SyncResult<Vec<SyncOutcome>>;
    async fn sync_full(&self) -> SyncResult<Vec<SyncOutcome>>;
}

/// Result of a remote-library audit (music or manga).
#[derive(Debug, Clone, Default)]
pub struct SyncAuditResult {
    /// Shard/segment/tombstone filenames referenced in the manifest but not
    /// found on the remote (downloads may have failed or been removed manually).
    pub missing: Vec<String>,
    /// Files present on the remote that are not referenced by any manifest
    /// entry (left-over from aborted writes or old format migrations).
    pub orphans: Vec<String>,
    /// True iff both `missing` and `orphans` are empty.
    pub healthy: bool,
    /// Human-readable one-line summary (English).
    pub summary: String,
}

/// Estimated cost of a full rebuild for one library kind (music or manga).
#[derive(Debug, Clone, Default)]
pub struct SyncEstimateResult {
    /// Number of base shards the rebuild would write.
    pub shards: usize,
    /// Rough estimated compressed bytes (≈ 200 B per row × shard item count).
    pub estimated_bytes: i64,
    /// Number of incremental segments + tombstones currently on the remote
    /// (i.e. how many accumulated since the last full rebuild).
    pub fragment_count: usize,
    /// Total bytes of all segments + tombstones in the current remote manifest.
    pub fragment_bytes: i64,
}

/// Join a base sync directory and a file name, collapsing double slashes.
fn join_path(base: &str, name: &str) -> String {
    let base = base.trim_end_matches('/');
    let name = name.trim_start_matches('/');
    format!("{base}/{name}")
}

// ── WdmmVfs helpers (used by the credential + playlist engines) ──────────
//
// The engines talk to a [`WdmmVfs`] directly — no parallel file-store trait.
// These free functions adapt driver results into the sync-shaped results the
// engines need: `NotFound` on read → `Ok(None)` (file not yet on remote); a network
// error on `mkdir` is treated as success (many WebDAV servers return 405 when the
// collection already exists); `NotFound` on `list` → empty (dir not yet created).

async fn driver_read(driver: &Arc<dyn WdmmVfs>, path: &str) -> SyncResult<Option<Vec<u8>>> {
    match driver.get_content(path).await {
        Ok(bytes) => Ok(Some(bytes)),
        Err(DriverError::NotFound(_)) => Ok(None),
        Err(e) => Err(SyncError::from(e)),
    }
}

async fn driver_write(driver: &Arc<dyn WdmmVfs>, path: &str, data: &[u8]) -> SyncResult<()> {
    driver.put_content(path, data).await?;
    Ok(())
}

/// Ensure `dir` exists, creating every missing ancestor first.
///
/// A plain `MKCOL` creates exactly one collection: pointing it at
/// `<root>/library/music` on a fresh remote answers `409 Conflict` because
/// `<root>/library` does not exist yet, and the following `PUT` then answers
/// `404`. Each ancestor is attempted best-effort — an already-present
/// collection answers 405/409, which drivers report as an error and which the
/// following write would surface anyway if the failure were real.
pub(crate) async fn driver_ensure_dir(driver: &Arc<dyn WdmmVfs>, dir: &str) -> SyncResult<()> {
    let bytes = dir.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'/' && index > 0 && index + 1 < bytes.len() {
            let _ = driver.mkdir(&dir[..index]).await;
        }
    }
    match driver.mkdir(dir).await {
        Ok(()) => Ok(()),
        Err(DriverError::Network(_)) => {
            debug!("mkdir {dir}: ignored non-fatal error (likely exists)");
            Ok(())
        }
        Err(e) => Err(SyncError::from(e)),
    }
}

async fn driver_list_files(driver: &Arc<dyn WdmmVfs>, dir: &str) -> SyncResult<Vec<String>> {
    match driver.list(dir).await {
        Ok(items) => Ok(items
            .into_iter()
            .filter(|i| !i.is_dir)
            .map(|i| i.name)
            .collect()),
        Err(DriverError::NotFound(_)) => Ok(vec![]),
        Err(e) => Err(SyncError::from(e)),
    }
}

// ── Credential Sync ───────────────────────────────────────────────────────────

/// Map a local cloud account onto its record inside the engine snapshot.
///
/// The snapshot's password tag carries the opaque
/// `CloudAccount.credentials_encrypted` blob as base64, so the document is
/// ciphertext-only on the wire. `engine_updated_at` (epoch seconds) drives
/// last-write-wins at the account level, so the caller passes the row's *real*
/// local change clock -- stamping `now()` while mapping would make every run
/// claim a fresh edit and overwrite whatever another device published.
fn account_to_record(account: &CloudAccount, engine_updated_at: i64) -> wmp::VaultRecordDoc {
    use base64::prelude::*;
    wmp::VaultRecordDoc {
        id: account.id.clone(),
        name: account.display_name.clone(),
        provider_type: account.provider_type.clone(),
        url: account.base_url.clone().unwrap_or_default(),
        username: String::new(),
        password: BASE64_STANDARD.encode(&account.credentials_encrypted),
        password_encrypted: false,
        remote_path: "/".to_string(),
        driver_config: None,
        engine_updated_at,
    }
}

fn account_from_record(record: &wmp::VaultRecordDoc) -> CloudAccount {
    use base64::prelude::*;
    CloudAccount {
        id: record.id.clone(),
        provider_type: record.provider_type.clone(),
        display_name: record.name.clone(),
        base_url: if record.url.is_empty() {
            None
        } else {
            Some(record.url.clone())
        },
        credentials_encrypted: BASE64_STANDARD
            .decode(&record.password)
            .unwrap_or_default(),
    }
}

/// Credential sync engine — syncs cloud account credentials to/from a remote
/// `WdmmVfs` store (WebDAV happy path).
///
/// The remote snapshot is a `WDMMCV01` document at
/// `{remote_dir}/accounts.wdmcv` (`wmp::ENGINE_ACCOUNTS_FILE_NAME`). It shares the
/// container format with the user credential vault (`credentials.wdmcv`) but is
/// **not** that file: the snapshot stores the opaque encrypted credential blob
/// and no username, the vault stores url/username plus a plaintext or
/// `AESGCMv1:` password. Different documents — never interchange them.
pub struct CredentialSyncEngine {
    db: Arc<LibraryDatabase>,
    driver: Arc<dyn WdmmVfs>,
    remote_dir: String,
}

impl CredentialSyncEngine {
    /// `remote_dir` is the base directory on the remote for sync metadata
    /// (e.g. `"/wdmm_sync"`). The snapshot lands at
    /// `{remote_dir}/accounts.wdmcv`.
    pub fn new(
        db: Arc<LibraryDatabase>,
        driver: Arc<dyn WdmmVfs>,
        remote_dir: impl Into<String>,
    ) -> Self {
        Self {
            db,
            driver,
            remote_dir: remote_dir.into(),
        }
    }

    fn file_path(&self) -> String {
        join_path(&self.remote_dir, wmp::ENGINE_ACCOUNTS_FILE_NAME)
    }

    async fn fetch_remote(
        &self,
    ) -> SyncResult<(Vec<wmp::VaultRecordDoc>, Vec<wmp::VaultDeletionDoc>)> {
        match driver_read(&self.driver, &self.file_path()).await? {
            None => Ok((vec![], vec![])),
            Some(bytes) => {
                let document = wmp::VaultDocument::decode(&bytes)?;
                Ok((document.records, document.deleted))
            }
        }
    }

    /// The epoch second the account last reached the remote snapshot (`0` = never).
    fn account_synced_at(&self, id: &str) -> SyncResult<i64> {
        let state = self.db.get_sync_state(&format!("account:{id}"))?;
        Ok(state.map(|s| s.last_seq).unwrap_or(0))
    }

    fn mark_account_synced(&self, id: &str, at: i64) -> SyncResult<()> {
        self.db.save_sync_state(&SyncState {
            remote_key: format!("account:{id}"),
            last_seq: at,
            base_up_to: 0,
            parts: "{}".to_string(),
            updated_at: wmp::now_iso8601(),
        })?;
        Ok(())
    }

    /// Adopt a remote record locally and remember the row's new clock, so the
    /// adoption does not look like a pending local edit on the next run.
    fn adopt_record(&self, record: &wmp::VaultRecordDoc) -> SyncResult<()> {
        let at = if record.engine_updated_at > 0 {
            record.engine_updated_at
        } else {
            chrono::Utc::now().timestamp()
        };
        self.db
            .insert_account_at(&account_from_record(record), at)?;
        self.mark_account_synced(&record.id, at)
    }

    /// Overwrite the snapshot with the merged union.
    ///
    /// The union matters: the snapshot is one document, so writing only "local"
    /// rows would delete accounts another device just published.
    async fn push_records(
        &self,
        records: Vec<wmp::VaultRecordDoc>,
        deleted: Vec<wmp::VaultDeletionDoc>,
    ) -> SyncResult<()> {
        driver_ensure_dir(&self.driver, &self.remote_dir).await?;
        let document = wmp::VaultDocument {
            format_version: 2,
            device_id: String::new(),
            created_at: wmp::now_iso8601(),
            records,
            deleted,
        };
        driver_write(&self.driver, &self.file_path(), &document.encode()).await
    }
}

#[async_trait]
impl SyncEngine for CredentialSyncEngine {
    async fn sync_incremental(&self) -> SyncResult<Vec<SyncOutcome>> {
        /// Published tombstones older than this are dropped; a device that was
        /// offline for longer than this may republish a deleted account.
        const ACCOUNT_TOMBSTONE_TTL_SECS: i64 = 180 * 24 * 60 * 60;

        let local = self.db.list_accounts()?;
        let local_ids: HashSet<String> = local.iter().map(|a| a.id.clone()).collect();
        let local_clocks = self.db.list_account_updated_at()?;
        let local_tombs: HashMap<String, i64> =
            self.db.list_deleted_accounts()?.into_iter().collect();
        let unpushed: HashSet<String> = self
            .db
            .list_unpushed_deleted_accounts()?
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let (remote, remote_deleted) = self.fetch_remote().await?;
        let remote_by_id: HashMap<String, &wmp::VaultRecordDoc> =
            remote.iter().map(|r| (r.id.clone(), r)).collect();
        let remote_tombs: HashMap<String, i64> = remote_deleted
            .iter()
            .map(|d| (d.id.clone(), d.deleted_at))
            .collect();

        let mut ids: std::collections::BTreeSet<String> = local_ids.iter().cloned().collect();
        ids.extend(remote_by_id.keys().cloned());
        ids.extend(local_tombs.keys().cloned());
        ids.extend(remote_tombs.keys().cloned());

        let mut outcomes = Vec::new();
        let mut merged: BTreeMap<String, wmp::VaultRecordDoc> = remote
            .iter()
            .map(|r| (r.id.clone(), r.clone()))
            .collect();
        let mut merged_deleted: std::collections::BTreeMap<String, i64> =
            remote_tombs.iter().map(|(k, v)| (k.clone(), *v)).collect();
        let mut published: Vec<String> = Vec::new();

        for id in ids {
            let local_account = local.iter().find(|a| a.id == id);
            let remote_record = remote_by_id.get(&id).copied();
            let local_tomb = local_tombs.get(&id).copied();
            let remote_tomb = remote_tombs.get(&id).copied();
            let local_at = local_clocks.get(&id).copied().unwrap_or(0);
            let synced_at = self.account_synced_at(&id)?;

            match (local_account, remote_record) {
                (Some(account), Some(record)) => {
                    let remote_at = record.engine_updated_at;
                    // A tombstone newer than both the row and its record is
                    // the live authority: the document file still carries the
                    // record because tombstones and records are separate.
                    if let Some(tomb) = remote_tomb {
                        if tomb >= local_at && tomb >= remote_at {
                            self.db.delete_account_quietly(&id)?;
                            outcomes.push(SyncOutcome {
                                entity_id: id.clone(),
                                action: SyncAction::Pulled,
                                rev: RevClock::new(tomb.max(1) as u64),
                            });
                            continue;
                        }
                    }
                    if remote_at > local_at {
                        if local_at > synced_at {
                            info!(
                                "credential sync conflict: {} kept the remote revision ({} < {})",
                                id, local_at, remote_at
                            );
                            outcomes.push(SyncOutcome {
                                entity_id: id.clone(),
                                action: SyncAction::Conflict,
                                rev: RevClock::new(local_at.max(1) as u64),
                            });
                        }
                        self.adopt_record(record)?;
                        merged_deleted.remove(&id);
                        outcomes.push(SyncOutcome {
                            entity_id: id.clone(),
                            action: SyncAction::Pulled,
                            rev: RevClock::new(remote_at.max(1) as u64),
                        });
                        continue;
                    }
                    if local_at > 0 && local_at <= synced_at {
                        outcomes.push(SyncOutcome {
                            entity_id: id.clone(),
                            action: SyncAction::NoChange,
                            rev: RevClock::new(local_at.max(1) as u64),
                        });
                        continue;
                    }
                    let stamp = if local_at > 0 {
                        local_at
                    } else {
                        chrono::Utc::now().timestamp()
                    };
                    merged.insert(id.clone(), account_to_record(account, stamp));
                    merged_deleted.remove(&id);
                    self.mark_account_synced(&id, stamp)?;
                    outcomes.push(SyncOutcome {
                        entity_id: id.clone(),
                        action: SyncAction::Pushed,
                        rev: RevClock::new(stamp.max(1) as u64),
                    });
                }
                (Some(account), None) => {
                    // The row is live locally; a remote tombstone only wins
                    // when it is not older than the row.
                    if let Some(tomb) = remote_tomb {
                        if tomb >= local_at {
                            self.db.delete_account_quietly(&id)?;
                            outcomes.push(SyncOutcome {
                                entity_id: id.clone(),
                                action: SyncAction::Pulled,
                                rev: RevClock::new(tomb.max(1) as u64),
                            });
                            continue;
                        }
                    }
                    let stamp = if local_at > 0 {
                        local_at
                    } else {
                        chrono::Utc::now().timestamp()
                    };
                    merged.insert(id.clone(), account_to_record(account, stamp));
                    merged_deleted.remove(&id);
                    if local_tomb.is_some() {
                        published.push(id.clone());
                        merged_deleted.remove(&id);
                    }
                    self.mark_account_synced(&id, stamp)?;
                    outcomes.push(SyncOutcome {
                        entity_id: id.clone(),
                        action: SyncAction::Pushed,
                        rev: RevClock::new(stamp.max(1) as u64),
                    });
                }
                (None, Some(record)) => {
                    let remote_at = record.engine_updated_at;
                    // A deletion this device recorded wins unless the peer
                    // recreated the account after it.
                    if let Some(tomb) = local_tomb {
                        if tomb >= remote_at {
                            merged_deleted.insert(id.clone(), tomb);
                            published.push(id.clone());
                            outcomes.push(SyncOutcome {
                                entity_id: id.clone(),
                                action: SyncAction::Pushed,
                                rev: RevClock::new(tomb.max(1) as u64),
                            });
                            continue;
                        }
                    }
                    if let Some(tomb) = remote_tomb {
                        if tomb >= remote_at {
                            continue;
                        }
                    }
                    self.adopt_record(record)?;
                    merged_deleted.remove(&id);
                    outcomes.push(SyncOutcome {
                        entity_id: id.clone(),
                        action: SyncAction::Pulled,
                        rev: RevClock::new(remote_at.max(1) as u64),
                    });
                }
                (None, None) => {
                    if let Some(tomb) = local_tomb {
                        merged_deleted.insert(id.clone(), tomb);
                        published.push(id.clone());
                        if unpushed.contains(&id) || remote_tomb != Some(tomb) {
                            outcomes.push(SyncOutcome {
                                entity_id: id.clone(),
                                action: SyncAction::Pushed,
                                rev: RevClock::new(tomb.max(1) as u64),
                            });
                        }
                    }
                }
            }
        }

        // Publish records + tombstones together: the snapshot is a single
        // document, so dropping either half loses a peer's change.
        self.push_records(
            merged.into_values().collect(),
            merged_deleted
                .into_iter()
                .map(|(id, deleted_at)| wmp::VaultDeletionDoc { id, deleted_at })
                .collect(),
        )
        .await?;
        self.db.mark_deleted_accounts_pushed(&published)?;
        self.db.purge_deleted_accounts_before(
            chrono::Utc::now().timestamp() - ACCOUNT_TOMBSTONE_TTL_SECS,
        )?;

        info!(
            "credential sync: {} local, {} remote, {} outcomes",
            local.len(),
            remote.len(),
            outcomes.len()
        );
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

/// Per-entry deletion clocks: `identity -> (epoch seconds, RFC-3339)`.
type EntryTombstones = HashMap<String, (i64, String)>;

/// Merge two entry-tombstone maps, keeping the newest deletion per identity.
fn merge_entry_tombstones(local: &EntryTombstones, remote: &EntryTombstones) -> EntryTombstones {
    let mut out = local.clone();
    for (identity, (epoch, at)) in remote {
        match out.get(identity) {
            Some((known, _)) if known >= epoch => {}
            _ => {
                out.insert(identity.clone(), (*epoch, at.clone()));
            }
        }
    }
    out
}

/// Drop the entries a tombstone retires and stamp the playlist's edit clock.
///
/// A deletion is an edit: when anything disappears the document's `updated_at`
/// moves to the newest tombstone, otherwise the next last-write-wins round would
/// hand the playlist back to a peer whose document is merely "older but complete".
fn apply_entry_tombstones(
    mut playlist: Playlist,
    entries: Vec<PlaylistEntry>,
    tombstones: &EntryTombstones,
) -> (Playlist, Vec<PlaylistEntry>, bool) {
    if tombstones.is_empty() {
        return (playlist, entries, false);
    }
    let before = entries.len();
    let document_epoch = playlist.updated_at.timestamp();
    let mut newest_applied: Option<i64> = None;
    let kept: Vec<PlaylistEntry> = entries
        .into_iter()
        .filter(|entry| match tombstones.get(&entry.music_id) {
            // A deletion only trims a document it is at least as new as: a
            // document edited after the deletion re-adds the entry on purpose.
            Some((epoch, _)) if *epoch >= document_epoch => {
                newest_applied = Some(newest_applied.map_or(*epoch, |old| old.max(*epoch)));
                false
            }
            _ => true,
        })
        .collect();
    if kept.len() == before {
        return (playlist, kept, false);
    }
    if let Some(newest) = newest_applied {
        if newest > document_epoch {
            playlist.updated_at = chrono::DateTime::from_timestamp(newest, 0).unwrap_or(playlist.updated_at);
        }
    }
    (playlist, kept, true)
}

/// Cover stem for one playlist entry, mirroring Android's `MusicIdHelper`.
///
/// Cue-derived entries carry a virtual `<audio>#cue:<n>` path; the cover lives
/// under the backing audio file, exactly like the app's playlist codec.
fn entry_cover_stem(entry: &PlaylistEntry) -> String {
    let remote = match entry.remote_path.find("#cue:") {
        Some(index) => &entry.remote_path[..index],
        None => entry.remote_path.as_str(),
    };
    music_cover_stem(&entry.source_name, remote)
}

/// Map a local playlist + its entries onto a `WDMMPL01` document.
///
/// `cover_roots` (when the run was asked to carry covers) embeds the locally
/// cached playlist artwork: one blob per distinct image, `cover_index` pointing
/// into it. Cue numbers and the "remote missing" flag travel as tags 6 / 8.
fn playlist_to_document(
    pl: &Playlist,
    entries: &[PlaylistEntry],
    cover_roots: Option<&CoverRoots>,
) -> wmp::PlaylistDocument {
    let mut document = wmp::PlaylistDocument {
        id: pl.id.clone(),
        name: pl.name.clone(),
        created_at: wmp::iso8601_from_epoch(pl.created_at.timestamp()),
        updated_at: wmp::iso8601_from_epoch(pl.updated_at.timestamp()),
        device_id: String::new(),
        entries: entries
            .iter()
            .map(|entry| wmp::PlaylistEntryDoc {
                music_id: entry.music_id.clone(),
                source_name: entry.source_name.clone(),
                remote_path: entry.remote_path.clone(),
                title: entry.title.clone(),
                duration_ms: Some(entry.duration_ms),
                cue_track_index: entry.cue_track_index,
                cover_index: None,
                remote_missing: entry.remote_missing,
            })
            .collect(),
        covers: Vec::new(),
    };
    let Some(roots) = cover_roots else {
        return document;
    };
    let mut blobs: Vec<Vec<u8>> = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let stem = entry_cover_stem(entry);
        let Ok(bytes) = std::fs::read(roots.music_covers_dir.join(&stem)) else {
            continue;
        };
        if bytes.is_empty() {
            continue;
        }
        let cover_index = match blobs.iter().position(|blob| *blob == bytes) {
            Some(known) => known,
            None => {
                blobs.push(bytes);
                blobs.len() - 1
            }
        };
        if let Some(slot) = document.entries.get_mut(index) {
            slot.cover_index = Some(cover_index as i64);
        }
    }
    document.covers = blobs
        .into_iter()
        .map(|bytes| wmp::PlaylistCoverDoc {
            kind: detect_image_kind(&bytes),
            bytes,
        })
        .collect();
    document
}

/// Whether two documents say the same thing (ignoring file names and device ids).
///
/// Used to skip a PUT when the winner already matches what the remote holds;
/// the comparison mirrors the app's old planner: name, clock, then the identity
/// and display fields of every entry.
fn document_matches(local: &wmp::PlaylistDocument, remote: &wmp::PlaylistDocument) -> bool {
    local.name == remote.name
        && local.updated_at == remote.updated_at
        && local.entries.len() == remote.entries.len()
        && local
            .entries
            .iter()
            .zip(&remote.entries)
            .all(|(a, b)| {
                a.music_id == b.music_id
                    && a.title == b.title
                    && a.duration_ms == b.duration_ms
                    && a.cue_track_index == b.cue_track_index
                    && a.remote_missing == b.remote_missing
            })
}

/// Which parts of a snapshot an aggregate run walks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncScope {
    /// Credentials + sharded library + playlists.
    All,
    /// Playlists only (the sync screen's dedicated button, and every publish).
    Playlists,
}

impl SyncScope {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "all" => Some(Self::All),
            "playlists" => Some(Self::Playlists),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Playlists => "playlists",
        }
    }
}

/// Cloud layout under one sync root.
///
/// Every engine derives its directories from the same root, and Dart both shows
/// these paths and hands the backup subtree back to the backup FFI. Keeping the
/// names here only stops the two lists from drifting (Dart still advertised a
/// `credentials.json` after the engine moved to `accounts.wdmcv`).
pub fn layout(remote_root: &str) -> serde_json::Value {
    let root = remote_root.trim();
    serde_json::json!({
        "credentials": join_path(root, wmp::ENGINE_ACCOUNTS_FILE_NAME),
        "playlists": join_path(root, "playlists"),
        "library": join_path(root, "library"),
        "library_music": join_path(root, "library/music"),
        "library_manga": join_path(root, "library/manga"),
        "backup": join_path(root, "backup"),
    })
}

/// Rebuild a local playlist + entries from a `WDMMPL01` document, plus the
/// epoch seconds of the document's `updated_at` (the LWW clock).
fn playlist_from_document(
    document: wmp::PlaylistDocument,
) -> (Playlist, Vec<PlaylistEntry>, i64) {
    let created_seconds = wmp::epoch_from_iso8601(&document.created_at);
    let updated_seconds = wmp::epoch_from_iso8601(&document.updated_at)
        .unwrap_or_else(|| chrono::Utc::now().timestamp());
    let playlist_id = document.id.clone();
    let entries: Vec<PlaylistEntry> = document
        .entries
        .iter()
        .enumerate()
        .map(|(position, entry)| PlaylistEntry {
            playlist_id: playlist_id.clone(),
            position: position as i64,
            music_id: entry.music_id.clone(),
            source_name: entry.source_name.clone(),
            remote_path: entry.remote_path.clone(),
            title: entry.title.clone(),
            duration_ms: entry.duration_ms.unwrap_or(0),
            cue_track_index: entry.cue_track_index,
            remote_missing: entry.remote_missing,
        })
        .collect();
    let playlist = Playlist {
        id: document.id,
        name: document.name,
        created_at: created_seconds
            .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
            .unwrap_or_else(chrono::Utc::now),
        updated_at: chrono::DateTime::from_timestamp(updated_seconds, 0)
            .unwrap_or_else(chrono::Utc::now),
    };
    (playlist, entries, updated_seconds)
}

/// Playlist sync engine — syncs playlist definitions to/from remote storage.
///
/// The engine speaks the same `WDMMPL01` / `WDMMPD01` containers the app does:
/// one document per playlist at `{remote_dir}/playlists/{id}.wdmp` plus one
/// deletion pack per playlist while its tombstones are pending. Documents carry
/// covers, cue slice numbers and the missing flag; packs carry both the
/// whole-playlist tombstone (empty identity) and per-entry tombstones. Files are
/// named after playlist ids, so a renamed playlist never orphans its remote copy.
pub struct PlaylistSyncEngine {
    db: Arc<LibraryDatabase>,
    driver: Arc<dyn WdmmVfs>,
    remote_dir: String,
    cover_roots: Option<CoverRoots>,
}

impl PlaylistSyncEngine {
    pub fn new(
        db: Arc<LibraryDatabase>,
        driver: Arc<dyn WdmmVfs>,
        remote_dir: impl Into<String>,
    ) -> Self {
        Self {
            db,
            driver,
            remote_dir: remote_dir.into(),
            cover_roots: None,
        }
    }

    /// Embed locally cached playlist covers in documents this engine publishes.
    pub fn with_covers(mut self, cover_roots: Option<CoverRoots>) -> Self {
        self.cover_roots = cover_roots;
        self
    }

    fn playlists_dir(&self) -> String {
        join_path(&self.remote_dir, "playlists")
    }

    fn file_path(&self, id: &str) -> String {
        join_path(&self.playlists_dir(), &wmp::engine_playlist_file_name(id))
    }

    fn deletion_path(&self, id: &str) -> String {
        join_path(
            &self.playlists_dir(),
            &wmp::playlist_deletion_file_name(id),
        )
    }

    /// Adopt a remote document locally and return its revision.
    ///
    /// Quiet apply: adopting a peer's document is not a local edit, so it must
    /// not record entry tombstones or clear pending ones.
    async fn adopt(&self, document: &wmp::PlaylistDocument) -> SyncResult<i64> {
        let (pulled, entries, epoch) = playlist_from_document(document.clone());
        self.db.upsert_playlist(&pulled)?;
        self.db.replace_playlist_entries_quietly(&pulled.id, &entries)?;
        Ok(epoch)
    }

    /// Remote documents plus the file names each one was read from.
    ///
    /// A peer may have published under a non-canonical name (older app builds);
    /// keeping the names lets a push clean those stale files up.
    async fn remote_documents(
        &self,
        files: &[String],
    ) -> SyncResult<(
        HashMap<String, wmp::PlaylistDocument>,
        HashMap<String, Vec<String>>,
    )> {
        let mut documents: HashMap<String, wmp::PlaylistDocument> = HashMap::new();
        let mut names: HashMap<String, Vec<String>> = HashMap::new();
        for name in files {
            if !wmp::looks_like_playlist_document(name) {
                continue;
            }
            let path = join_path(&self.playlists_dir(), name);
            let Some(bytes) = driver_read(&self.driver, &path).await? else {
                continue;
            };
            match wmp::PlaylistDocument::decode(&bytes) {
                Ok(document) => {
                    names
                        .entry(document.id.clone())
                        .or_default()
                        .push(name.clone());
                    documents.insert(document.id.clone(), document);
                }
                Err(error) => {
                    tracing::debug!("skip unreadable playlist document {name}: {error}")
                }
            }
        }
        Ok((documents, names))
    }

    /// Remote deletion packs.
    ///
    /// Returns `(whole-playlist tombstones by id, per-entry tombstones by id)`.
    /// An empty `entry_identity` record deletes the playlist itself; the others
    /// delete one entry.
    async fn remote_deletions(
        &self,
        files: &[String],
    ) -> SyncResult<(HashMap<String, i64>, HashMap<String, EntryTombstones>)> {
        let mut whole: HashMap<String, i64> = HashMap::new();
        let mut entries: HashMap<String, EntryTombstones> = HashMap::new();
        for name in files {
            if !wmp::looks_like_playlist_deletion(name) {
                continue;
            }
            let path = join_path(&self.playlists_dir(), name);
            let Some(bytes) = driver_read(&self.driver, &path).await? else {
                continue;
            };
            let document = match wmp::PlaylistDeletionDocument::decode(&bytes) {
                Ok(document) => document,
                Err(error) => {
                    tracing::debug!("skip unreadable playlist deletion pack {name}: {error}");
                    continue;
                }
            };
            for record in document.records {
                let Some(epoch) = wmp::epoch_from_iso8601(&record.deleted_at) else {
                    continue;
                };
                if record.entry_identity.is_empty() {
                    let slot = whole
                        .entry(document.playlist_id.clone())
                        .or_insert(epoch);
                    if epoch > *slot {
                        *slot = epoch;
                    }
                } else {
                    let slot = entries
                        .entry(document.playlist_id.clone())
                        .or_default();
                    match slot.get(&record.entry_identity) {
                        Some((known, _)) if *known >= epoch => {}
                        _ => {
                            slot.insert(record.entry_identity.clone(), (epoch, record.deleted_at));
                        }
                    }
                }
            }
        }
        Ok((whole, entries))
    }

    async fn push_one(&self, pl: &Playlist, entries: &[PlaylistEntry]) -> SyncResult<()> {
        driver_ensure_dir(&self.driver, &self.playlists_dir()).await?;
        let document = playlist_to_document(pl, entries, self.cover_roots.as_ref());
        driver_write(&self.driver, &self.file_path(&pl.id), &document.encode()).await
    }

    /// Publish a `WDMMPD01` pack so deletions outlive the documents they touch.
    ///
    /// `records` carry entry identities; the whole-playlist record is added when
    /// `deleted_at` is also the playlist's own tombstone (`whole`).
    async fn push_deletion(
        &self,
        id: &str,
        deleted_at: &str,
        whole: bool,
        records: &[(String, String)],
    ) -> SyncResult<()> {
        driver_ensure_dir(&self.driver, &self.playlists_dir()).await?;
        let mut rows: Vec<wmp::PlaylistDeletionRecordDoc> = Vec::new();
        if whole {
            rows.push(wmp::PlaylistDeletionRecordDoc {
                entry_identity: String::new(),
                deleted_at: deleted_at.to_string(),
            });
        }
        for (identity, at) in records {
            rows.push(wmp::PlaylistDeletionRecordDoc {
                entry_identity: identity.clone(),
                deleted_at: at.clone(),
            });
        }
        let document = wmp::PlaylistDeletionDocument {
            playlist_id: id.to_string(),
            device_id: String::new(),
            created_at: deleted_at.to_string(),
            updated_at: deleted_at.to_string(),
            records: rows,
        };
        driver_write(&self.driver, &self.deletion_path(id), &document.encode()).await
    }

    /// Delete remote files by name inside the playlists directory.
    async fn remove_remote(&self, names: &[String]) -> SyncResult<()> {
        for name in names {
            let path = join_path(&self.playlists_dir(), name);
            match self.driver.remove(&path).await {
                Ok(()) | Err(DriverError::NotFound(_)) => {}
                Err(error) => return Err(SyncError::from(error)),
            }
        }
        Ok(())
    }

    /// Publish one playlist right after a local edit.
    ///
    /// The app edits a playlist and expects the change on the remote without a
    /// full sync round trip. A row that no longer exists publishes its
    /// whole-playlist tombstone instead, together with any pending entry
    /// tombstones it left behind.
    pub async fn publish(&self, playlist_id: &str) -> SyncResult<Option<SyncOutcome>> {
        let Some(playlist) = self.db.get_playlist(playlist_id)? else {
            let Some((_, deleted_at)) = self
                .db
                .list_unpushed_deleted_playlists()?
                .into_iter()
                .find(|(id, _)| id == playlist_id)
            else {
                return Ok(None);
            };
            let pending = self.pending_entry_tombstones(playlist_id)?;
            self.push_deletion(playlist_id, &deleted_at, true, &pending)
                .await?;
            self.mark_entry_tombstones_pushed(playlist_id, &pending)?;
            self.db
                .mark_deleted_playlists_pushed(&[playlist_id.to_string()])?;
            let epoch = wmp::epoch_from_iso8601(&deleted_at).unwrap_or(0);
            return Ok(Some(SyncOutcome {
                entity_id: playlist_id.to_string(),
                action: SyncAction::Pushed,
                rev: RevClock::new(epoch.max(1) as u64),
            }));
        };
        let entries = self.db.list_playlist_entries(playlist_id)?;
        self.push_one(&playlist, &entries).await?;
        let pending = self.pending_entry_tombstones(playlist_id)?;
        self.publish_pack(playlist_id, &pending).await?;
        Ok(Some(SyncOutcome {
            entity_id: playlist_id.to_string(),
            action: SyncAction::Pushed,
            rev: RevClock::new(playlist.updated_at.timestamp().max(1) as u64),
        }))
    }

    /// Unpublished entry tombstones of one playlist, as `(identity, deleted_at)`.
    fn pending_entry_tombstones(&self, playlist_id: &str) -> SyncResult<Vec<(String, String)>> {
        let mut pending: Vec<(String, String)> = self
            .db
            .list_unpushed_deleted_playlist_entries()?
            .into_iter()
            .filter(|(id, _, _)| id == playlist_id)
            .map(|(_, identity, at)| (identity, at))
            .collect();
        pending.sort();
        Ok(pending)
    }

    fn mark_entry_tombstones_pushed(
        &self,
        playlist_id: &str,
        pending: &[(String, String)],
    ) -> SyncResult<()> {
        if pending.is_empty() {
            return Ok(());
        }
        let keys: Vec<(String, String)> = pending
            .iter()
            .map(|(identity, _)| (playlist_id.to_string(), identity.clone()))
            .collect();
        self.db.mark_deleted_playlist_entries_pushed(&keys)?;
        Ok(())
    }

    /// Write the entry-tombstone pack of one playlist and mark it published.
    async fn publish_pack(
        &self,
        playlist_id: &str,
        pending: &[(String, String)],
    ) -> SyncResult<()> {
        if pending.is_empty() {
            return Ok(());
        }
        let deleted_at = pending
            .iter()
            .map(|(_, at)| at.clone())
            .max()
            .unwrap_or_else(wmp::now_iso8601);
        self.push_deletion(playlist_id, &deleted_at, false, pending)
            .await?;
        self.mark_entry_tombstones_pushed(playlist_id, pending)?;
        Ok(())
    }

    /// Remove every playlist deletion pack from the remote (compaction).
    pub async fn delete_deletion_packs(&self) -> SyncResult<usize> {
        let files = driver_list_files(&self.driver, &self.playlists_dir()).await?;
        let packs: Vec<String> = files
            .into_iter()
            .filter(|name| wmp::looks_like_playlist_deletion(name))
            .collect();
        self.remove_remote(&packs).await?;
        Ok(packs.len())
    }
}

/// How long a published playlist deletion keeps protecting against a stale
/// peer copy. Same window as the other sync tombstones.
const PLAYLIST_TOMBSTONE_TTL_SECS: i64 = 180 * 24 * 60 * 60;

#[async_trait]
impl SyncEngine for PlaylistSyncEngine {
    async fn sync_incremental(&self) -> SyncResult<Vec<SyncOutcome>> {
        let local_by_id: HashMap<String, Playlist> = self
            .db
            .list_playlists()?
            .into_iter()
            .map(|playlist| (playlist.id.clone(), playlist))
            .collect();
        let local_whole_at: HashMap<String, String> = self
            .db
            .list_deleted_playlists()?
            .into_iter()
            .collect();
        let local_whole_epoch: HashMap<String, i64> = local_whole_at
            .iter()
            .filter_map(|(id, at)| wmp::epoch_from_iso8601(at).map(|epoch| (id.clone(), epoch)))
            .collect();
        let unpushed_whole: HashSet<String> = self
            .db
            .list_unpushed_deleted_playlists()?
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let mut local_entry: HashMap<String, EntryTombstones> = HashMap::new();
        for (playlist_id, identity, deleted_at, _pushed) in
            self.db.list_deleted_playlist_entries()?
        {
            if let Some(epoch) = wmp::epoch_from_iso8601(&deleted_at) {
                local_entry
                    .entry(playlist_id)
                    .or_default()
                    .insert(identity, (epoch, deleted_at));
            }
        }
        let unpushed_entries: HashSet<(String, String)> = self
            .db
            .list_unpushed_deleted_playlist_entries()?
            .into_iter()
            .map(|(playlist_id, identity, _)| (playlist_id, identity))
            .collect();

        let files = driver_list_files(&self.driver, &self.playlists_dir()).await?;
        let (remote_docs, remote_names) = self.remote_documents(&files).await?;
        let (remote_whole, remote_entry) = self.remote_deletions(&files).await?;

        let mut ids: Vec<String> = local_by_id
            .keys()
            .cloned()
            .chain(remote_docs.keys().cloned())
            .chain(local_whole_epoch.keys().cloned())
            .chain(remote_whole.keys().cloned())
            .collect();
        ids.sort_unstable();
        ids.dedup();

        let mut outcomes = Vec::new();
        let mut mark_whole_pushed: Vec<String> = Vec::new();
        let mut mark_entry_pushed: Vec<(String, String)> = Vec::new();
        let mut remove_paths: Vec<String> = Vec::new();

        for id in ids {
            let local_row = local_by_id.get(&id).cloned();
            let local_epoch = local_row
                .as_ref()
                .map(|playlist| playlist.updated_at.timestamp())
                .unwrap_or(0);
            let local_tomb = local_whole_epoch.get(&id).copied();
            let remote_doc = remote_docs.get(&id).cloned();
            let remote_epoch = remote_doc
                .as_ref()
                .and_then(|doc| wmp::epoch_from_iso8601(&doc.updated_at))
                .unwrap_or(0);
            let remote_tomb = remote_whole.get(&id).copied();
            let names = remote_names.get(&id).cloned().unwrap_or_default();
            let canonical = wmp::engine_playlist_file_name(&id);
            let mut tombstones = local_entry.get(&id).cloned().unwrap_or_default();
            if let Some(remote_tombstones) = remote_entry.get(&id) {
                tombstones = merge_entry_tombstones(&tombstones, remote_tombstones);
            }

            // A live row supersedes its own leftover tombstone; mark it published
            // so it cannot delete the row again next run.
            if local_row.is_some() && local_tomb.is_some() {
                mark_whole_pushed.push(id.clone());
            }

            // 1) A whole-playlist tombstone at least as new as both live copies
            //    retires the playlist; no document may resurrect it.
            if let Some(tomb) = local_tomb.into_iter().chain(remote_tomb).max() {
                let live_epoch = local_epoch.max(remote_epoch);
                if (local_row.is_some() || remote_doc.is_some()) && tomb >= live_epoch {
                    if local_row.is_some() {
                        self.db.delete_playlist_quietly(&id)?;
                    }
                    remove_paths.extend(names.iter().cloned());
                    if unpushed_whole.contains(&id) {
                        let deleted_at = local_whole_at
                            .get(&id)
                            .cloned()
                            .unwrap_or_else(wmp::now_iso8601);
                        let mut records: Vec<(String, String)> = unpushed_entries
                            .iter()
                            .filter(|(playlist_id, _)| *playlist_id == id)
                            .map(|(_, identity)| (identity.clone(), deleted_at.clone()))
                            .collect();
                        records.sort();
                        self.push_deletion(&id, &deleted_at, true, &records).await?;
                        mark_whole_pushed.push(id.clone());
                        mark_entry_pushed.extend(
                            records
                                .iter()
                                .map(|(identity, _)| (id.clone(), identity.clone())),
                        );
                        outcomes.push(SyncOutcome {
                            entity_id: id,
                            action: SyncAction::Pushed,
                            rev: RevClock::new(tomb.max(1) as u64),
                        });
                    } else {
                        outcomes.push(SyncOutcome {
                            entity_id: id,
                            action: SyncAction::Pulled,
                            rev: RevClock::new(tomb.max(1) as u64),
                        });
                    }
                    continue;
                }
            }

            // 2) Both sides live: last write wins, entry tombstones trim the
            //    winner (a deletion beats an equally new, longer document).
            if let (Some(local_playlist), Some(remote_document)) =
                (local_row.clone(), remote_doc.clone())
            {
                let (winner, winner_is_local) = if local_epoch >= remote_epoch {
                    (local_playlist, true)
                } else {
                    (playlist_from_document(remote_document.clone()).0, false)
                };
                let entries = if winner_is_local {
                    self.db.list_playlist_entries(&id)?
                } else {
                    playlist_from_document(remote_document.clone()).1
                };
                let (winner, winner_entries, dropped) =
                    apply_entry_tombstones(winner, entries, &tombstones);
                let document =
                    playlist_to_document(&winner, &winner_entries, self.cover_roots.as_ref());
                if !winner_is_local && !dropped {
                    // The remote side is newer and no tombstone trimmed it:
                    // adopt it quietly so the pull is not recorded as an edit.
                    let rev = self.adopt(&remote_document).await?;
                    outcomes.push(SyncOutcome {
                        entity_id: id.clone(),
                        action: SyncAction::Pulled,
                        rev: RevClock::new(rev.max(1) as u64),
                    });
                } else {
                    if !winner_is_local || dropped {
                        self.db.upsert_playlist(&winner)?;
                        self.db
                            .replace_playlist_entries_quietly(&winner.id, &winner_entries)?;
                    }
                    if document_matches(&document, &remote_document) {
                        outcomes.push(SyncOutcome {
                            entity_id: id.clone(),
                            action: SyncAction::NoChange,
                            rev: RevClock::new(local_epoch.max(remote_epoch).max(1) as u64),
                        });
                    } else {
                        self.push_one(&winner, &winner_entries).await?;
                        outcomes.push(SyncOutcome {
                            entity_id: id.clone(),
                            action: SyncAction::Pushed,
                            rev: RevClock::new(local_epoch.max(remote_epoch).max(1) as u64),
                        });
                    }
                }
                remove_paths.extend(names.iter().filter(|name| **name != canonical).cloned());
                let pending: Vec<(String, String)> = unpushed_entries
                    .iter()
                    .filter(|(playlist_id, _)| *playlist_id == id)
                    .map(|(_, identity)| {
                        let at = tombstones
                            .get(identity)
                            .map(|(_, at)| at.clone())
                            .unwrap_or_else(wmp::now_iso8601);
                        (identity.clone(), at)
                    })
                    .collect();
                self.publish_pack(&id, &pending).await?;
                mark_entry_pushed.extend(
                    pending
                        .iter()
                        .map(|(identity, _)| (id.clone(), identity.clone())),
                );
                continue;
            }

            // 3) Only local: publish it, unless a remote tombstone outruns it.
            if let Some(local_playlist) = local_row.clone() {
                if let Some(tomb) = remote_tomb {
                    if tomb >= local_epoch {
                        self.db.delete_playlist_quietly(&id)?;
                        remove_paths.extend(names.iter().cloned());
                        outcomes.push(SyncOutcome {
                            entity_id: id,
                            action: SyncAction::Pulled,
                            rev: RevClock::new(tomb.max(1) as u64),
                        });
                        continue;
                    }
                }
                let entries = self.db.list_playlist_entries(&id)?;
                let (playlist, entries, _dropped) =
                    apply_entry_tombstones(local_playlist, entries, &tombstones);
                self.push_one(&playlist, &entries).await?;
                remove_paths.extend(names.iter().filter(|name| **name != canonical).cloned());
                outcomes.push(SyncOutcome {
                    entity_id: id.clone(),
                    action: SyncAction::Pushed,
                    rev: RevClock::new(local_epoch.max(1) as u64),
                });
                let pending: Vec<(String, String)> = unpushed_entries
                    .iter()
                    .filter(|(playlist_id, _)| *playlist_id == id)
                    .map(|(_, identity)| {
                        let at = tombstones
                            .get(identity)
                            .map(|(_, at)| at.clone())
                            .unwrap_or_else(wmp::now_iso8601);
                        (identity.clone(), at)
                    })
                    .collect();
                self.publish_pack(&id, &pending).await?;
                mark_entry_pushed.extend(
                    pending
                        .iter()
                        .map(|(identity, _)| (id.clone(), identity.clone())),
                );
                continue;
            }

            // 4) Only a local deletion: publish it unless a newer peer edit wins.
            if let Some(tomb) = local_tomb {
                if let Some(remote_document) = remote_doc.clone() {
                    let edited = wmp::epoch_from_iso8601(&remote_document.updated_at).unwrap_or(0);
                    if edited > tomb {
                        let (pulled, pulled_entries, _) =
                            playlist_from_document(remote_document);
                        let (playlist, entries, _dropped) =
                            apply_entry_tombstones(pulled, pulled_entries, &tombstones);
                        self.db.upsert_playlist(&playlist)?;
                        self.db
                            .replace_playlist_entries_quietly(&playlist.id, &entries)?;
                        mark_whole_pushed.push(id.clone());
                        outcomes.push(SyncOutcome {
                            entity_id: id,
                            action: SyncAction::Pulled,
                            rev: RevClock::new(edited.max(1) as u64),
                        });
                        continue;
                    }
                }
                if unpushed_whole.contains(&id) {
                    let deleted_at = local_whole_at
                        .get(&id)
                        .cloned()
                        .unwrap_or_else(wmp::now_iso8601);
                    let mut records: Vec<(String, String)> = unpushed_entries
                        .iter()
                        .filter(|(playlist_id, _)| *playlist_id == id)
                        .map(|(_, identity)| (identity.clone(), deleted_at.clone()))
                        .collect();
                    records.sort();
                    self.push_deletion(&id, &deleted_at, true, &records).await?;
                    remove_paths.extend(names.iter().cloned());
                    mark_whole_pushed.push(id.clone());
                    mark_entry_pushed
                        .extend(records.iter().map(|(identity, _)| (id.clone(), identity.clone())));
                    outcomes.push(SyncOutcome {
                        entity_id: id,
                        action: SyncAction::Pushed,
                        rev: RevClock::new(tomb.max(1) as u64),
                    });
                }
                continue;
            }

            // 5) Nothing local: adopt the remote document unless a tombstone that
            //    is at least as new says it was deleted.
            if let Some(remote_document) = remote_doc.clone() {
                if let Some(tomb) = remote_tomb {
                    if tomb >= remote_epoch {
                        remove_paths.extend(names.iter().cloned());
                        continue;
                    }
                }
                let (pulled, pulled_entries, _) = playlist_from_document(remote_document);
                let (playlist, entries, dropped) =
                    apply_entry_tombstones(pulled, pulled_entries, &tombstones);
                self.db.upsert_playlist(&playlist)?;
                self.db
                    .replace_playlist_entries_quietly(&playlist.id, &entries)?;
                if dropped {
                    // Re-publish the trimmed document so the deletion travels on.
                    self.push_one(&playlist, &entries).await?;
                }
                outcomes.push(SyncOutcome {
                    entity_id: id.clone(),
                    action: SyncAction::Pulled,
                    rev: RevClock::new(remote_epoch.max(1) as u64),
                });
                remove_paths.extend(names.iter().filter(|name| **name != canonical).cloned());
            }
        }

        if !mark_whole_pushed.is_empty() {
            self.db.mark_deleted_playlists_pushed(&mark_whole_pushed)?;
        }
        mark_entry_pushed.sort();
        mark_entry_pushed.dedup();
        if !mark_entry_pushed.is_empty() {
            self.db
                .mark_deleted_playlist_entries_pushed(&mark_entry_pushed)?;
        }
        self.remove_remote(&remove_paths).await?;
        let cutoff =
            wmp::iso8601_from_epoch(chrono::Utc::now().timestamp() - PLAYLIST_TOMBSTONE_TTL_SECS);
        let _ = self.db.purge_deleted_playlists_before(&cutoff)?;
        let _ = self.db.purge_deleted_playlist_entries_before(&cutoff)?;

        info!(
            "playlist sync: {} local, {} remote, {} outcomes",
            local_by_id.len(),
            remote_docs.len(),
            outcomes.len()
        );
        Ok(outcomes)
    }

    async fn sync_full(&self) -> SyncResult<Vec<SyncOutcome>> {
        self.sync_incremental().await
    }
}

// ── aggregate trigger (used by FFI) ───────────────────────────────────────────

/// Run credential + library sync in sequence and return a JSON summary.
///
/// Engine-level pass behind [`run_sync`], the single FFI entry point.
pub async fn run_sync_all(
    db: Arc<LibraryDatabase>,
    driver: Arc<dyn WdmmVfs>,
    remote_dir: &str,
) -> SyncResult<serde_json::Value> {
    run_sync_all_with_covers(db, driver, remote_dir, None).await
}

async fn run_sync_all_with_covers(
    db: Arc<LibraryDatabase>,
    driver: Arc<dyn WdmmVfs>,
    remote_dir: &str,
    cover_roots: Option<CoverRoots>,
) -> SyncResult<serde_json::Value> {
    let cred = CredentialSyncEngine::new(Arc::clone(&db), Arc::clone(&driver), remote_dir)
        .sync_incremental()
        .await?;
    let sharded = entities::registry::run_sharded_entities(
        Arc::clone(&db),
        Arc::clone(&driver),
        remote_dir,
        cover_roots.clone(),
        SyncMode::Incremental,
    )
    .await?;
    let pl = PlaylistSyncEngine::new(db, driver, remote_dir)
        .with_covers(cover_roots)
        .sync_incremental()
        .await?;
    Ok(aggregate_summary(&cred, sharded, &pl))
}

/// Build the aggregate JSON: the single-document engines plus every sharded one.
fn aggregate_summary(
    credentials: &[SyncOutcome],
    sharded: Vec<entities::registry::ShardedSummary>,
    playlists: &[SyncOutcome],
) -> serde_json::Value {
    let mut summary = serde_json::Map::new();
    summary.insert(
        "credentials".into(),
        entities::registry::summarize(credentials),
    );
    for entity in sharded {
        summary.insert(
            entity.key.into(),
            entities::registry::summarize(&entity.outcomes),
        );
    }
    summary.insert(
        "playlists".into(),
        entities::registry::summarize(playlists),
    );
    serde_json::Value::Object(summary)
}

/// Playlist-only summary (`scope=playlists` runs touch nothing else).
fn playlist_summary(playlists: &[SyncOutcome]) -> serde_json::Value {
    serde_json::json!({
        "playlists": entities::registry::summarize(playlists),
    })
}

/// Run a **full** sync (base-shard rebuild for music + manga) and return a JSON
/// summary. Full-rebuild pass behind [`run_sync`], the single FFI entry point.
///
/// Each engine's `sync_full` adopts every cloud row, then rewrites the whole
/// library as fresh base shards (compacting away old segments + tombstones) and
/// clears the local cursor. Use after a device reset or to compact a fragmented
/// remote. Credentials and playlists delegate to `sync_full` (which currently
/// mirrors their incremental pass).
///
/// `cover_roots`: when `Some`, base shards written during the rebuild carry
/// `SEC_COVERS` (3) with deduplicated cover blobs. Pass `None` (the default)
/// to skip embedding — covers can be added on a later rebuild.
pub async fn run_sync_full(
    db: Arc<LibraryDatabase>,
    driver: Arc<dyn WdmmVfs>,
    remote_dir: &str,
    cover_roots: Option<CoverRoots>,
) -> SyncResult<serde_json::Value> {
    let cred = CredentialSyncEngine::new(Arc::clone(&db), Arc::clone(&driver), remote_dir)
        .sync_full()
        .await?;
    let sharded = entities::registry::run_sharded_entities(
        Arc::clone(&db),
        Arc::clone(&driver),
        remote_dir,
        cover_roots.clone(),
        SyncMode::Full,
    )
    .await?;
    let pl = PlaylistSyncEngine::new(db, driver, remote_dir)
        .with_covers(cover_roots)
        .sync_full()
        .await?;
    Ok(aggregate_summary(&cred, sharded, &pl))
}

// ── single entry point ────────────────────────────────────────────────────────

/// Mode of a sharded-entity sync pass ([`run_sync_all`] / [`run_sync_full`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    /// One pass over the deltas (cursor driven).
    Incremental,
    /// Reconcile everything, then rebuild the base shards.
    Full,
}

impl SyncMode {
    /// Parse the wire value (`"incremental"` / `"full"`).
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "incremental" => Some(Self::Incremental),
            "full" => Some(Self::Full),
            _ => None,
        }
    }

    /// Wire value used by the FFI and the summary.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Incremental => "incremental",
            Self::Full => "full",
        }
    }
}

/// What one aggregate run should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncCommand {
    /// Walk the delta (or rebuild the shards) for the requested scope.
    Sync(SyncMode),
    /// Publish local playlist tombstones, then drop every playlist deletion pack
    /// on the remote and forget the tombstones locally.
    CompactPlaylists,
}

impl SyncCommand {
    /// Parse the wire value (`"incremental"` / `"full"` / `"compact_playlists"`).
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "compact_playlists" => Some(Self::CompactPlaylists),
            other => SyncMode::parse(other).map(Self::Sync),
        }
    }

    /// Wire value used by the FFI and the summary.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sync(mode) => mode.as_str(),
            Self::CompactPlaylists => "compact_playlists",
        }
    }
}

/// Run an aggregate sync and return the contract summary
/// `{"ok":true,"mode":"…","scope":"…","conflicts":N,"entities":{…}}`.
///
/// This is the only aggregate entry point: the FFI exposes it as
/// `wdmm_sync_run(json)` and the app never calls the per-mode passes directly.
pub async fn run_sync(
    db: Arc<LibraryDatabase>,
    driver: Arc<dyn WdmmVfs>,
    remote_dir: &str,
    command: SyncCommand,
    scope: SyncScope,
    cover_roots: Option<CoverRoots>,
) -> SyncResult<serde_json::Value> {
    let entities = match command {
        SyncCommand::CompactPlaylists => {
            let engine = PlaylistSyncEngine::new(Arc::clone(&db), Arc::clone(&driver), remote_dir)
                .with_covers(cover_roots);
            let outcomes = engine.sync_incremental().await?;
            let packs_removed = engine.delete_deletion_packs().await?;
            let playlists_cleared = db.clear_deleted_playlists()?;
            let entries_cleared = db.clear_deleted_playlist_entries()?;
            serde_json::json!({
                "playlists": entities::registry::summarize(&outcomes),
                "packs_removed": packs_removed,
                "tombstones_cleared": playlists_cleared + entries_cleared,
            })
        }
        SyncCommand::Sync(SyncMode::Incremental) => match scope {
            SyncScope::All => {
                run_sync_all_with_covers(
                    Arc::clone(&db),
                    Arc::clone(&driver),
                    remote_dir,
                    cover_roots,
                )
                .await?
            }
            SyncScope::Playlists => {
                let outcomes = PlaylistSyncEngine::new(db, driver, remote_dir)
                    .with_covers(cover_roots)
                    .sync_incremental()
                    .await?;
                playlist_summary(&outcomes)
            }
        },
        SyncCommand::Sync(SyncMode::Full) => match scope {
            SyncScope::All => {
                run_sync_full(Arc::clone(&db), Arc::clone(&driver), remote_dir, cover_roots).await?
            }
            SyncScope::Playlists => {
                let outcomes = PlaylistSyncEngine::new(db, driver, remote_dir)
                    .with_covers(cover_roots)
                    .sync_full()
                    .await?;
                playlist_summary(&outcomes)
            }
        },
    };
    let conflicts = entities
        .as_object()
        .map(|by_entity| {
            by_entity
                .values()
                .filter_map(|entity| entity.get("conflicts").and_then(|value| value.as_u64()))
                .sum::<u64>()
        })
        .unwrap_or(0);
    Ok(serde_json::json!({
        "ok": true,
        "mode": command.as_str(),
        "scope": scope.as_str(),
        "conflicts": conflicts,
        "entities": entities,
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

    /// In-memory `WdmmVfs` used by sync unit tests — no network.
    struct MockDriver {
        pub files: AsyncMutex<Map<String, Vec<u8>>>,
        dirs: AsyncMutex<std::collections::HashSet<String>>,
        /// Paths passed to `get_content` — used by timing tests.
        pub read_paths: Mutex<Vec<String>>,
        /// Enforce real WebDAV semantics: `MKCOL` on a missing parent answers
        /// 409 and `PUT` into a missing collection answers 404.
        strict: bool,
    }

    impl MockDriver {
        fn new() -> Self {
            Self {
                files: AsyncMutex::new(Map::new()),
                dirs: AsyncMutex::new(std::collections::HashSet::new()),
                read_paths: Mutex::new(vec![]),
                strict: false,
            }
        }

        fn strict() -> Self {
            Self {
                strict: true,
                ..Self::new()
            }
        }

        async fn parent_exists(&self, path: &str) -> bool {
            let parent = path.rsplit_once('/').map(|(head, _)| head).unwrap_or("");
            parent.is_empty() || self.dirs.lock().await.contains(parent)
        }
    }

    #[async_trait]
    impl WdmmVfs for MockDriver {
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
            if self.strict {
                if self.dirs.lock().await.contains(path) {
                    return Err(DriverError::Network("MKCOL failed: 405".into()));
                }
                if !self.parent_exists(path).await {
                    return Err(DriverError::Network("MKCOL failed: 409".into()));
                }
            }
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
            if self.strict && !self.parent_exists(path).await {
                return Err(DriverError::Network("PUT failed: 404".into()));
            }
            self.files
                .lock()
                .await
                .insert(path.to_string(), data.to_vec());
            Ok(())
        }
        async fn get_content(&self, path: &str) -> wdmm_drivers::Result<Vec<u8>> {
            self.read_paths.lock().unwrap().push(path.to_string());
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

    /// Regression: a brand-new remote root has no `library` collection yet.
    /// WebDAV `MKCOL` refuses to create `<root>/library/music` while its parent
    /// is missing (409), and the following `PUT` then answers 404 — so the sync
    /// must create the ancestor chain instead of only the leaf directory.
    #[tokio::test]
    async fn fresh_root_sync_creates_missing_ancestor_dirs() {
        use wdmm_core::models::LibraryTrack;
        let db = tmp_db();
        db.insert_track(&LibraryTrack {
            id: wdmm_core::identity::track_identity("/music/fresh.mp3"),
            title: "Fresh".into(),
            remote_path: "/music/fresh.mp3".into(),
            account_id: "acc1".into(),
            size: 1,
            rev: 1,
            ..Default::default()
        })
        .unwrap();
        let driver = Arc::new(MockDriver::strict());
        run_sync_all(Arc::clone(&db), driver.clone(), "/fresh")
            .await
            .unwrap();

        let dirs = driver.dirs.lock().await;
        for dir in ["/fresh", "/fresh/library", "/fresh/library/music"] {
            assert!(dirs.contains(dir), "missing {dir} after sync: {dirs:?}");
        }
    }

    #[tokio::test]
    async fn credential_sync_pushes_then_pulls() {
        let db = tmp_db();
        let driver = Arc::new(MockDriver::new());

        // Device A: has one account → sync (pushes the WDMMCV01 snapshot).
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

        // Verify the remote snapshot is a WDMMCV01 document at accounts.wdmcv,
        // and that it is not the user vault file name.
        let remote_bytes = driver
            .get_content("/sync/accounts.wdmcv")
            .await
            .unwrap();
        assert_eq!(
            wmp::container_file_kind(&remote_bytes),
            Some(*wmp::FILE_KIND_VAULT)
        );
        let remote = wmp::VaultDocument::decode(&remote_bytes).unwrap();
        assert_eq!(remote.records.len(), 1);
        assert_eq!(remote.records[0].id, "acc1");
        assert_eq!(remote.records[0].name, "My NAS");
        assert!(
            !driver
                .files
                .lock()
                .await
                .contains_key("/sync/credentials.wdmcv"),
            "the engine must never write the user vault file"
        );

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
    async fn credential_sync_publishes_the_union_and_keeps_clean_rows_quiet() {
        let db = tmp_db();
        let driver = Arc::new(MockDriver::new());
        for (id, name) in [("acc1", "One"), ("acc2", "Two")] {
            db.insert_account(&CloudAccount {
                id: id.into(),
                provider_type: "webdav".into(),
                display_name: name.into(),
                base_url: Some(format!("https://{id}.local/dav/")),
                credentials_encrypted: vec![1, 2, 3],
            })
            .unwrap();
        }
        let engine = CredentialSyncEngine::new(Arc::clone(&db), driver.clone(), "/sync");
        engine.sync_incremental().await.unwrap();

        // Device B adopts both accounts, then adds a third one.
        let db2 = tmp_db();
        let engine2 = CredentialSyncEngine::new(Arc::clone(&db2), driver.clone(), "/sync");
        engine2.sync_incremental().await.unwrap();
        assert!(db2.get_account("acc1").unwrap().is_some());
        db2.insert_account(&CloudAccount {
            id: "acc3".into(),
            provider_type: "webdav".into(),
            display_name: "Three".into(),
            base_url: None,
            credentials_encrypted: vec![9],
        })
        .unwrap();
        let outcomes = engine2.sync_incremental().await.unwrap();
        assert!(outcomes
            .iter()
            .any(|o| o.entity_id == "acc3" && o.action == SyncAction::Pushed));

        // The snapshot must still carry acc1/acc2: pushing only local rows used
        // to delete the accounts another device had just published.
        let snapshot = wmp::VaultDocument::decode(
            &driver.get_content("/sync/accounts.wdmcv").await.unwrap(),
        )
        .unwrap();
        let ids: Vec<&str> = snapshot.records.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids.len(), 3, "snapshot must keep the union: {ids:?}");
        assert!(ids.contains(&"acc1") && ids.contains(&"acc2") && ids.contains(&"acc3"));

        // Adoption stamps the row as synced, so an unchanged account never
        // claims a fresh revision again.
        let second = engine2.sync_incremental().await.unwrap();
        assert!(
            second.iter().all(|o| o.action != SyncAction::Pushed),
            "clean rows must not republish: {second:?}"
        );
    }

    fn account(id: &str, name: &str) -> CloudAccount {
        CloudAccount {
            id: id.into(),
            provider_type: "webdav".into(),
            display_name: name.into(),
            base_url: Some(format!("https://{id}.local/dav/")),
            credentials_encrypted: vec![1, 2, 3],
        }
    }

    #[tokio::test]
    async fn credential_deletion_propagates_and_stays_deleted() {
        let db = tmp_db();
        let driver = Arc::new(MockDriver::new());
        db.insert_account(&account("acc1", "One")).unwrap();
        db.insert_account(&account("acc2", "Two")).unwrap();
        let engine = CredentialSyncEngine::new(Arc::clone(&db), driver.clone(), "/sync");
        engine.sync_incremental().await.unwrap();

        let db2 = tmp_db();
        let engine2 = CredentialSyncEngine::new(Arc::clone(&db2), driver.clone(), "/sync");
        engine2.sync_incremental().await.unwrap();
        assert!(db2.get_account("acc2").unwrap().is_some());

        // Device A deletes acc2: the row is gone but a tombstone remains.
        db.delete_account("acc2").unwrap();
        assert_eq!(db.list_unpushed_deleted_accounts().unwrap().len(), 1);
        let outcomes = engine.sync_incremental().await.unwrap();
        assert!(outcomes
            .iter()
            .any(|o| o.entity_id == "acc2" && o.action == SyncAction::Pushed));
        assert!(db.list_unpushed_deleted_accounts().unwrap().is_empty());

        let snapshot = wmp::VaultDocument::decode(
            &driver.get_content("/sync/accounts.wdmcv").await.unwrap(),
        )
        .unwrap();
        assert_eq!(snapshot.deleted.len(), 1);
        assert_eq!(snapshot.deleted[0].id, "acc2");

        // Device B applies the tombstone instead of republishing its row.
        engine2.sync_incremental().await.unwrap();
        assert!(db2.get_account("acc2").unwrap().is_none());
        let snapshot = wmp::VaultDocument::decode(
            &driver.get_content("/sync/accounts.wdmcv").await.unwrap(),
        )
        .unwrap();
        assert_eq!(
            snapshot.deleted.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
            vec!["acc2"],
            "the tombstone must survive a peer sync"
        );
        engine2.sync_incremental().await.unwrap();
        assert!(
            db2.get_account("acc2").unwrap().is_none(),
            "a deleted account must not come back on the next run"
        );

        let purged = db
            .purge_deleted_accounts_before(chrono::Utc::now().timestamp() + 86_400)
            .unwrap();
        assert_eq!(purged, 1);
        assert!(db.list_deleted_accounts().unwrap().is_empty());
    }

    #[tokio::test]
    async fn credential_recreated_after_deletion_wins() {
        let db = tmp_db();
        let driver = Arc::new(MockDriver::new());
        db.insert_account(&account("acc1", "One")).unwrap();
        db.insert_account(&account("acc2", "Two")).unwrap();
        let engine = CredentialSyncEngine::new(Arc::clone(&db), driver.clone(), "/sync");
        engine.sync_incremental().await.unwrap();

        let db2 = tmp_db();
        let engine2 = CredentialSyncEngine::new(Arc::clone(&db2), driver.clone(), "/sync");
        engine2.sync_incremental().await.unwrap();

        db.delete_account("acc2").unwrap();
        engine.sync_incremental().await.unwrap();
        engine2.sync_incremental().await.unwrap();
        assert!(db2.get_account("acc2").unwrap().is_none());

        // Device B recreates the account *after* the deletion clock.
        let later = chrono::Utc::now().timestamp() + 5;
        db2.insert_account_at(&account("acc2", "Two again"), later)
            .unwrap();
        engine2.sync_incremental().await.unwrap();
        let snapshot = wmp::VaultDocument::decode(
            &driver.get_content("/sync/accounts.wdmcv").await.unwrap(),
        )
        .unwrap();
        assert!(
            snapshot.deleted.iter().all(|d| d.id != "acc2"),
            "a newer live record supersedes the tombstone"
        );

        engine.sync_incremental().await.unwrap();
        let restored = db.get_account("acc2").unwrap().expect("account must return");
        assert_eq!(restored.display_name, "Two again");
        assert!(
            db.list_deleted_accounts().unwrap().is_empty(),
            "adopting the record must clear the local tombstone"
        );
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

        let engine = LibrarySyncEngine::new(Arc::clone(&db), driver.clone(), "/sync", None);
        let outcomes = engine.sync_incremental().await.unwrap();
        assert!(outcomes.iter().any(|o| o.action == SyncAction::Pushed));

        // Manifest + segment under library/music/
        let files = driver.files.lock().await;
        assert!(files.contains_key("/sync/library/music/index.json"));
        assert!(files.keys().any(|k| k.contains("/sync/library/music/seg-")));
        drop(files);

        // Second device pulls.
        let db2 = tmp_db();
        let engine2 = LibrarySyncEngine::new(Arc::clone(&db2), driver.clone(), "/sync", None);
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
        LibrarySyncEngine::new(Arc::clone(&db_a), driver.clone(), "/sync", None)
            .sync_incremental()
            .await
            .unwrap();

        // Device B pulls.
        let db_b = tmp_db();
        LibrarySyncEngine::new(Arc::clone(&db_b), driver.clone(), "/sync", None)
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
        LibrarySyncEngine::new(Arc::clone(&db_a), driver.clone(), "/sync", None)
            .sync_incremental()
            .await
            .unwrap();
        let files = driver.files.lock().await;
        assert!(files.keys().any(|k| k.contains("/sync/library/music/del-")));
        drop(files);

        // Device B adopts deletion.
        LibrarySyncEngine::new(Arc::clone(&db_b), driver.clone(), "/sync", None)
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
        LibrarySyncEngine::new(Arc::clone(&db), driver.clone(), "/sync", None)
            .sync_incremental()
            .await
            .unwrap();
        // Full rebuild
        LibrarySyncEngine::new(Arc::clone(&db), driver.clone(), "/sync", None)
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
                cue_track_index: None,
                remote_missing: false,
            }],
        )
        .unwrap();

        let engine = PlaylistSyncEngine::new(Arc::clone(&db), driver.clone(), "/sync");
        let outcomes = engine.sync_incremental().await.unwrap();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].action, SyncAction::Pushed);

        // The mirror is a WDMMPL01 document, not JSON.
        let remote_bytes = driver
            .get_content("/sync/playlists/pl-1.wdmp")
            .await
            .unwrap();
        assert_eq!(
            wmp::container_file_kind(&remote_bytes),
            Some(*wmp::FILE_KIND_PLAYLIST)
        );
        let remote_document = wmp::PlaylistDocument::decode(&remote_bytes).unwrap();
        assert_eq!(remote_document.name, "Mix");
        assert_eq!(remote_document.entries.len(), 1);
        assert_eq!(remote_document.entries[0].title, "A");

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

    fn seed_playlist(db: &LibraryDatabase) {
        let older = chrono::Utc::now() - chrono::Duration::seconds(60);
        db.upsert_playlist(&Playlist {
            id: "pl-1".into(),
            name: "Mix".into(),
            created_at: older,
            updated_at: older,
        })
        .unwrap();
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
                cue_track_index: None,
                remote_missing: false,
            }],
        )
        .unwrap();
    }

    #[tokio::test]
    async fn playlist_deletion_propagates_to_the_other_device() {
        let db_a = tmp_db();
        let driver = Arc::new(MockDriver::new());
        seed_playlist(&db_a);
        let engine_a = PlaylistSyncEngine::new(Arc::clone(&db_a), driver.clone(), "/sync");
        engine_a.sync_incremental().await.unwrap();

        let db_b = tmp_db();
        let engine_b = PlaylistSyncEngine::new(Arc::clone(&db_b), driver.clone(), "/sync");
        engine_b.sync_incremental().await.unwrap();
        assert!(db_b.get_playlist("pl-1").unwrap().is_some());

        // Device A deletes. The deletion must outlive the document it deletes:
        // a WLMMPD01 pack is the only record that survives once the row is gone.
        db_a.delete_playlist("pl-1").unwrap();
        assert_eq!(db_a.list_unpushed_deleted_playlists().unwrap().len(), 1);
        let outcomes = engine_a.sync_incremental().await.unwrap();
        assert!(outcomes
            .iter()
            .any(|outcome| outcome.action == SyncAction::Pushed));
        assert!(db_a.list_unpushed_deleted_playlists().unwrap().is_empty());
        {
            let files = driver.files.lock().await;
            let name = wmp::playlist_deletion_file_name("pl-1");
            assert!(
                files.keys().any(|key| key.ends_with(&name)),
                "deletion pack missing: {:?}",
                files.keys().collect::<Vec<_>>()
            );
        }

        // Device B adopts the deletion instead of pushing its stale copy back.
        let outcomes = engine_b.sync_incremental().await.unwrap();
        assert!(outcomes
            .iter()
            .all(|outcome| outcome.action != SyncAction::Pushed));
        assert!(db_b.get_playlist("pl-1").unwrap().is_none());

        // Published tombstones expire once they are old enough to be safe.
        let future = wmp::iso8601_from_epoch(chrono::Utc::now().timestamp() + 24 * 60 * 60);
        assert_eq!(db_a.purge_deleted_playlists_before(&future).unwrap(), 1);
        assert!(db_a.list_deleted_playlists().unwrap().is_empty());
    }

    #[tokio::test]
    async fn playlist_edit_after_deletion_brings_the_playlist_back() {
        let db_a = tmp_db();
        let driver = Arc::new(MockDriver::new());
        seed_playlist(&db_a);
        let engine_a = PlaylistSyncEngine::new(Arc::clone(&db_a), driver.clone(), "/sync");
        engine_a.sync_incremental().await.unwrap();

        let db_b = tmp_db();
        let engine_b = PlaylistSyncEngine::new(Arc::clone(&db_b), driver.clone(), "/sync");
        engine_b.sync_incremental().await.unwrap();

        // A deletes the playlist; B never hears about it.
        db_a.delete_playlist("pl-1").unwrap();
        engine_a.sync_incremental().await.unwrap();
        assert!(db_a.get_playlist("pl-1").unwrap().is_none());

        // B edits strictly after the deletion, so its copy wins.
        let edited = Playlist {
            id: "pl-1".into(),
            name: "Mix v2".into(),
            created_at: db_b.get_playlist("pl-1").unwrap().unwrap().created_at,
            updated_at: chrono::Utc::now() + chrono::Duration::seconds(1),
        };
        db_b.upsert_playlist(&edited).unwrap();
        let outcomes = engine_b.sync_incremental().await.unwrap();
        assert!(outcomes
            .iter()
            .any(|outcome| outcome.action == SyncAction::Pushed));

        // A resurrects the playlist and drops its now-superseded tombstone.
        let outcomes = engine_a.sync_incremental().await.unwrap();
        assert!(outcomes
            .iter()
            .any(|outcome| outcome.action == SyncAction::Pulled));
        let revived = db_a.get_playlist("pl-1").unwrap().expect("revived");
        assert_eq!(revived.name, "Mix v2");
        assert!(db_a.list_unpushed_deleted_playlists().unwrap().is_empty());
    }

    #[tokio::test]
    async fn run_sync_all_aggregates() {
        use wdmm_core::models::LibraryTrack;
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
            cover_state: 0,
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

        let engine_a = MangaLibrarySyncEngine::new(Arc::clone(&db_a), driver.clone(), "/sync", None);
        let out_a = engine_a.sync_incremental().await.unwrap();
        assert!(out_a.iter().any(|o| o.action == SyncAction::Pushed));

        // manifest + segment created under library/manga/
        let files = driver.files.lock().await;
        assert!(files.contains_key("/sync/library/manga/index.json"));
        assert!(files.keys().any(|k| k.contains("/sync/library/manga/seg-")));
        drop(files);

        // second device pulls
        let db_b = tmp_db();
        let engine_b = MangaLibrarySyncEngine::new(Arc::clone(&db_b), driver.clone(), "/sync", None);
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
        MangaLibrarySyncEngine::new(Arc::clone(&db_a), driver.clone(), "/sync", None)
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
        MangaLibrarySyncEngine::new(Arc::clone(&db_b), driver.clone(), "/sync", None)
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

        let engine = MangaLibrarySyncEngine::new(Arc::clone(&db), driver.clone(), "/sync", None);
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

        let engine = MangaLibrarySyncEngine::new(Arc::clone(&db), driver.clone(), "/sync", None);
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

        let engine2 = MangaLibrarySyncEngine::new(Arc::clone(&db2), driver.clone(), "/sync", None);
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

        let driver_dyn: Arc<dyn WdmmVfs> = driver;
        let summary = run_sync_all(Arc::clone(&db), driver_dyn, "/sync")
            .await
            .unwrap();

        assert!(summary["manga"].is_object());
        assert!(
            summary["manga"]["pushed"].as_u64().unwrap() >= 1,
            "manga pushed expected"
        );
    }

    // Parity test suite (Chunk B): must-add scenarios from wdmm-sync-parity-spec.md.
    // Music: 2 (rev win), 3B (tomb loses), 4 (resurrect), 5A/B (cursor), 6 (zero-byte), 8 (rev gap).
    // Manga: 10A/B (images pages + tomb id), 11 (container tomb), 12B/C (progress), 13 (legacy skip), 14 (rebuild clear).
    include!("parity_tests.rs");

    /// After one incremental that reads lib-* base shards, a second incremental
    /// with the same `base.up_to` must NOT re-read those lib-* paths — the cursor
    /// records them as consumed and they are skipped.
    #[tokio::test]
    async fn incremental_stable_base_skips_lib_reads() {
        use wdmm_core::models::LibraryTrack;
        let driver = Arc::new(MockDriver::new());
        let db = tmp_db();

        // Seed a track so device A produces a lib-* base shard via full rebuild.
        let path = "/music/stable.mp3";
        db.insert_track(&LibraryTrack {
            id: wdmm_core::identity::track_identity(path),
            title: "Stable".into(),
            remote_path: path.into(),
            account_id: "nas".into(),
            rev: 10,
            ..Default::default()
        })
        .unwrap();
        LibrarySyncEngine::new(Arc::clone(&db), driver.clone(), "/sync", None)
            .sync_full()
            .await
            .unwrap();

        // A second device: first incremental pulls and reads the lib-* shard.
        let db2 = tmp_db();
        LibrarySyncEngine::new(Arc::clone(&db2), driver.clone(), "/sync", None)
            .sync_incremental()
            .await
            .unwrap();

        // Snapshot which lib-* paths were read so far.
        let reads_after_first: Vec<String> = driver
            .read_paths
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.contains("/library/music/lib-"))
            .cloned()
            .collect();
        assert!(!reads_after_first.is_empty(), "first incremental must read lib-* shards");

        // Second incremental — base.up_to unchanged, cursor is populated.
        let reads_before = driver.read_paths.lock().unwrap().len();
        LibrarySyncEngine::new(Arc::clone(&db2), driver.clone(), "/sync", None)
            .sync_incremental()
            .await
            .unwrap();
        let reads_after = driver.read_paths.lock().unwrap().len();

        let new_lib_reads: Vec<String> = driver
            .read_paths
            .lock()
            .unwrap()
            .iter()
            .skip(reads_before)
            .filter(|p| p.contains("/library/music/lib-"))
            .cloned()
            .collect();
        assert!(
            new_lib_reads.is_empty(),
            "second incremental with stable base.up_to must not re-read lib-* shards; \
             unexpected reads: {new_lib_reads:?} (total new reads: {})",
            reads_after - reads_before
        );
    }

    // silence unused import warnings on platforms where the helper isn't needed
    #[allow(dead_code)]
    fn _keep_mutex(_m: &Mutex<()>) {}

    /// Build one playlist entry for the tests below.
    fn entry(music_id: &str, position: i64, remote: &str, title: &str) -> PlaylistEntry {
        PlaylistEntry {
            playlist_id: "pl-1".into(),
            position,
            music_id: music_id.to_string(),
            source_name: "nas".into(),
            remote_path: remote.to_string(),
            title: title.to_string(),
            duration_ms: 1000,
            cue_track_index: None,
            remote_missing: false,
        }
    }

    #[tokio::test]
    async fn playlist_documents_carry_covers_and_cue_indexes() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("wdmm-pl-covers-{stamp}"));
        let covers = root.join("covers");
        std::fs::create_dir_all(&covers).unwrap();
        let stem = crate::cover::music_cover_stem("nas", "/a.mp3");
        std::fs::write(covers.join(&stem), b"\x89PNG\r\n\x1a\n").unwrap();

        let driver = Arc::new(MockDriver::new());
        let db = tmp_db();
        let older = chrono::Utc::now() - chrono::Duration::seconds(60);
        db.upsert_playlist(&Playlist {
            id: "pl-1".into(),
            name: "Mix".into(),
            created_at: older,
            updated_at: older,
        })
        .unwrap();
        let mut first = entry("aa".repeat(20).as_str(), 0, "/a.mp3", "A");
        first.cue_track_index = Some(2);
        db.set_playlist_entries("pl-1", &[first]).unwrap();

        let engine = PlaylistSyncEngine::new(Arc::clone(&db), driver.clone(), "/sync").with_covers(
            Some(CoverRoots {
                music_covers_dir: covers,
                manga_covers_dir: root.join("manga"),
            }),
        );
        engine.sync_incremental().await.unwrap();

        let bytes = driver
            .get_content("/sync/playlists/pl-1.wdmp")
            .await
            .unwrap();
        let document = wmp::PlaylistDocument::decode(&bytes).unwrap();
        assert_eq!(document.entries.len(), 1);
        assert_eq!(document.entries[0].cue_track_index, Some(2));
        assert_eq!(document.entries[0].cover_index, Some(0));
        assert_eq!(document.covers.len(), 1);
        assert_eq!(
            crate::cover::detect_image_kind(&document.covers[0].bytes),
            crate::cover::COVER_KIND_PNG
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn playlist_entry_tombstones_trim_a_newer_peer_edit() {
        let driver = Arc::new(MockDriver::new());
        let db_a = tmp_db();
        let older = chrono::Utc::now() - chrono::Duration::seconds(60);
        db_a.upsert_playlist(&Playlist {
            id: "pl-1".into(),
            name: "Mix".into(),
            created_at: older,
            updated_at: older,
        })
        .unwrap();
        db_a.set_playlist_entries(
            "pl-1",
            &[
                entry("aa".repeat(20).as_str(), 0, "/a.mp3", "A"),
                entry("bb".repeat(20).as_str(), 1, "/b.mp3", "B"),
            ],
        )
        .unwrap();
        let engine_a = PlaylistSyncEngine::new(Arc::clone(&db_a), driver.clone(), "/sync");
        engine_a.sync_incremental().await.unwrap();

        // Device B pulls both entries.
        let db_b = tmp_db();
        let engine_b = PlaylistSyncEngine::new(Arc::clone(&db_b), driver.clone(), "/sync");
        engine_b.sync_incremental().await.unwrap();
        assert_eq!(db_b.list_playlist_entries("pl-1").unwrap().len(), 2);

        // Device A drops "bb": the row diff records an entry tombstone, and the
        // next sync ships a deletion pack next to the trimmed document.
        db_a.set_playlist_entries(
            "pl-1",
            &[entry("aa".repeat(20).as_str(), 0, "/b.mp3", "A")],
        )
        .unwrap();
        engine_a.sync_incremental().await.unwrap();
        let pack_path = format!(
            "/sync/playlists/{}",
            wmp::playlist_deletion_file_name("pl-1")
        );
        assert!(driver.files.lock().await.contains_key(&pack_path));

        // Device B edits after the deletion but is still older than the
        // tombstone, so the entry deletion still trims B's document.
        db_b.upsert_playlist(&Playlist {
            id: "pl-1".into(),
            name: "Mix".into(),
            created_at: older,
            updated_at: older + chrono::Duration::seconds(10),
        })
        .unwrap();
        db_b.set_playlist_entries(
            "pl-1",
            &[
                entry("aa".repeat(20).as_str(), 0, "/a.mp3", "A"),
                entry("bb".repeat(20).as_str(), 1, "/b.mp3", "B"),
                entry("cc".repeat(20).as_str(), 2, "/c.mp3", "C"),
            ],
        )
        .unwrap();
        let b_outcomes = engine_b.sync_incremental().await.unwrap();
        assert_eq!(b_outcomes[0].action, SyncAction::Pushed);

        let ids: Vec<String> = db_b
            .list_playlist_entries("pl-1")
            .unwrap()
            .into_iter()
            .map(|row| row.music_id)
            .collect();
        assert_eq!(ids, vec!["aa".repeat(20), "cc".repeat(20)]);

        let pushed = wmp::PlaylistDocument::decode(
            &driver
                .get_content("/sync/playlists/pl-1.wdmp")
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(pushed.entries.len(), 2);
        assert!(!pushed.entries.iter().any(|row| row.music_id == "bb".repeat(20)));
    }

    #[tokio::test]
    async fn compact_playlists_removes_deletion_packs_and_tombstones() {
        let driver = Arc::new(MockDriver::new());
        let db = tmp_db();
        let older = chrono::Utc::now() - chrono::Duration::seconds(60);
        db.upsert_playlist(&Playlist {
            id: "pl-1".into(),
            name: "Mix".into(),
            created_at: older,
            updated_at: older,
        })
        .unwrap();
        db.set_playlist_entries("pl-1", &[entry("aa".repeat(20).as_str(), 0, "/a.mp3", "A")])
            .unwrap();
        let engine = PlaylistSyncEngine::new(Arc::clone(&db), driver.clone(), "/sync");
        engine.sync_incremental().await.unwrap();

        db.delete_playlist("pl-1").unwrap();
        engine.sync_incremental().await.unwrap();
        let pack_path = format!(
            "/sync/playlists/{}",
            wmp::playlist_deletion_file_name("pl-1")
        );
        assert!(driver.files.lock().await.contains_key(&pack_path));
        assert!(!db.list_deleted_playlists().unwrap().is_empty());

        let summary = run_sync(
            Arc::clone(&db),
            driver.clone(),
            "/sync",
            SyncCommand::CompactPlaylists,
            SyncScope::Playlists,
            None,
        )
        .await
        .unwrap();
        assert_eq!(summary["ok"], true);
        assert_eq!(summary["mode"], "compact_playlists");
        assert!(summary["entities"]["packs_removed"].as_u64().unwrap() >= 1);
        assert!(!driver.files.lock().await.contains_key(&pack_path));
        assert!(db.list_deleted_playlists().unwrap().is_empty());
        assert!(db.list_deleted_playlist_entries().unwrap().is_empty());
    }
}
