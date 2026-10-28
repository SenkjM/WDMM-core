//! Music cloud library sync — Android shard+tombstone protocol.
//!
//! Remote layout: `<remote_dir>/library/music/{index.json,lib-*,seg-*,del-*}`.
//! Cursor key: `library:{remote_dir}`.
//!
//! Incremental sync never rebuilds.  The manifest base is re-read only when
//! `base.up_to` advances beyond the saved cursor or the cursor is empty.
//! Segment and tombstone shards (`seg-*`, `del-*`) never carry covers.
//! Full sync (`sync_full`) is the only path that rebuilds lib-* base shards;
//! it may embed covers when [`LibrarySyncEngine::cover_roots`] is `Some`.

use crate::cover::CoverRoots;
use crate::library_shard::{
    self, track_identity_key, TombRecord, WMP_KIND_BASE, WMP_KIND_SEG,
};
use crate::{
    join_path, SyncAction, SyncEngine, SyncError, SyncOutcome, SyncResult,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tracing::info;
use wdmm_core::models::LibraryTrack;
use wdmm_core::rev_clock::RevClock;
use wdmm_drivers::{CloudDriver, DriverError};
use wdmm_storage::{LibraryDatabase, SyncState};

const FORMAT: &str = "wmp_library_index";
const FORMAT_VERSION: i64 = 1;
const INDEX_NAME: &str = "index.json";
const SHARD_PREFIX: &str = "lib-";
const SEGMENT_PREFIX: &str = "seg-";
const TOMB_PREFIX: &str = "del-";
pub const DEFAULT_TRACKS_PER_SHARD: usize = 500;
const CURSOR_PREFIX: &str = "library:";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BaseShardEntry {
    pub file: String,
    pub count: i64,
    pub bytes: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IndexPartEntry {
    pub file: String,
    pub from: i64,
    pub to: i64,
    pub count: i64,
    pub bytes: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryManifest {
    pub format: String,
    #[serde(rename = "formatVersion")]
    pub format_version: i64,
    pub base: ManifestBase,
    #[serde(default)]
    pub segments: Vec<IndexPartEntry>,
    #[serde(default)]
    pub tombstones: Vec<IndexPartEntry>,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ManifestBase {
    #[serde(rename = "upTo")]
    pub up_to: i64,
    #[serde(default)]
    pub shards: Vec<BaseShardEntry>,
}

impl Default for LibraryManifest {
    fn default() -> Self {
        Self {
            format: FORMAT.into(),
            format_version: FORMAT_VERSION,
            base: ManifestBase::default(),
            segments: vec![],
            tombstones: vec![],
            updated_at: chrono::Utc::now().to_rfc3339(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct CloudLibrary {
    pub tracks: Vec<LibraryTrack>,
    /// key → highest tomb rev seen
    pub tombs: HashMap<String, i64>,
}

fn merge_cloud_parts(
    data_parts: &[Vec<LibraryTrack>],
    tomb_parts: &[Vec<TombRecord>],
) -> CloudLibrary {
    let mut by_key: HashMap<String, LibraryTrack> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for rows in data_parts {
        for t in rows {
            let key = track_identity_key(&t.account_id, &t.remote_path);
            match by_key.get(&key) {
                None => {
                    by_key.insert(key.clone(), t.clone());
                    order.push(key);
                }
                Some(kept) if t.rev > kept.rev => {
                    by_key.insert(key, t.clone());
                }
                _ => {}
            }
        }
    }
    let mut tombs: HashMap<String, i64> = HashMap::new();
    for rows in tomb_parts {
        for row in rows {
            let key = track_identity_key(&row.source_name, &row.remote_path);
            let prev = tombs.get(&key).copied().unwrap_or(0);
            if row.rev > prev {
                tombs.insert(key.clone(), row.rev);
            }
            if let Some(live) = by_key.get(&key) {
                if row.rev >= live.rev {
                    by_key.remove(&key);
                    order.retain(|k| k != &key);
                }
            } else {
                // still record tomb
            }
        }
    }
    CloudLibrary {
        tracks: order.into_iter().filter_map(|k| by_key.remove(&k)).collect(),
        tombs,
    }
}

struct DriverStore {
    driver: Arc<dyn CloudDriver>,
}

impl DriverStore {
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
            Err(DriverError::Network(_)) => Ok(()),
            Err(e) => Err(SyncError::from(e)),
        }
    }
    async fn remove(&self, path: &str) -> SyncResult<()> {
        match self.driver.remove(path).await {
            Ok(()) => Ok(()),
            Err(DriverError::NotFound(_)) => Ok(()),
            Err(e) => Err(SyncError::from(e)),
        }
    }
    async fn list_files(&self, dir: &str) -> SyncResult<Vec<String>> {
        match self.driver.list(dir).await {
            Ok(items) => Ok(items.into_iter().filter(|i| !i.is_dir).map(|i| i.name).collect()),
            Err(DriverError::NotFound(_)) => Ok(vec![]),
            Err(e) => Err(SyncError::from(e)),
        }
    }
}

fn music_dir(remote_dir: &str) -> String {
    join_path(remote_dir, "library/music")
}

fn part_name(prefix: &str, rev: i64) -> String {
    let stamp = if rev > 0 {
        rev
    } else {
        chrono::Utc::now().timestamp_millis()
    };
    let rand = (stamp.wrapping_mul(1103515245).wrapping_add(12345).unsigned_abs()) % 0xFFFFFF;
    format!("{prefix}{stamp}-{rand:06x}.wdmm")
}

fn device_id(db: &LibraryDatabase) -> String {
    // Persisted in KV (key `wdmm_device_id`); generated as a UUID v4 on first use.
    // Shared with the manga engine so the `deviceId` shard field is stable per device.
    wdmm_storage::device_id_or_generate(db)
}

/// Music library sync engine (replaces toy library.json).
pub struct LibrarySyncEngine {
    db: Arc<LibraryDatabase>,
    store: DriverStore,
    remote_dir: String,
    cover_roots: Option<CoverRoots>,
}

impl LibrarySyncEngine {
    pub fn new(
        db: Arc<LibraryDatabase>,
        driver: Arc<dyn CloudDriver>,
        remote_dir: impl Into<String>,
        cover_roots: Option<CoverRoots>,
    ) -> Self {
        Self {
            db,
            store: DriverStore { driver },
            remote_dir: remote_dir.into(),
            cover_roots,
        }
    }

    fn dir(&self) -> String {
        music_dir(&self.remote_dir)
    }

    fn index_path(&self) -> String {
        join_path(&self.dir(), INDEX_NAME)
    }

    fn cursor_key(&self) -> String {
        format!("{CURSOR_PREFIX}{}", self.remote_dir)
    }

    async fn read_manifest(&self) -> SyncResult<LibraryManifest> {
        match self.store.read(&self.index_path()).await? {
            None => Ok(LibraryManifest::default()),
            Some(bytes) => {
                let v: serde_json::Value = serde_json::from_slice(&bytes)?;
                if v.get("format").and_then(|x| x.as_str()) != Some(FORMAT) {
                    return Ok(LibraryManifest::default());
                }
                Ok(serde_json::from_value(v)?)
            }
        }
    }

    async fn write_manifest(&self, mut m: LibraryManifest) -> SyncResult<()> {
        m.updated_at = chrono::Utc::now().to_rfc3339();
        m.format = FORMAT.into();
        m.format_version = FORMAT_VERSION;
        self.store.ensure_dir(&self.dir()).await?;
        let bytes = serde_json::to_vec_pretty(&m)?;
        self.store.write(&self.index_path(), &bytes).await
    }

    async fn read_cloud(
        &self,
        manifest: &LibraryManifest,
        skip_files: &HashSet<String>,
    ) -> SyncResult<(CloudLibrary, HashSet<String>)> {
        let mut data_parts = Vec::new();
        let mut tomb_parts = Vec::new();
        let mut read_parts = HashSet::new();

        for shard in &manifest.base.shards {
            if skip_files.contains(&shard.file) {
                continue;
            }
            read_parts.insert(shard.file.clone());
            let path = join_path(&self.dir(), &shard.file);
            if let Some(bytes) = self.store.read(&path).await? {
                let decoded = library_shard::decode_library_shard(&bytes)?;
                if let Some(roots) = &self.cover_roots {
                    let cover_ids = library_shard::apply_music_cover_blobs(
                        roots,
                        &decoded.tracks,
                        &decoded.cover_indices,
                        &decoded.covers,
                    );
                    for id in &cover_ids {
                        let _ = self.db.set_track_cover_state(id, 2);
                    }
                }
                data_parts.push(decoded.tracks);
            }
        }
        for seg in &manifest.segments {
            if skip_files.contains(&seg.file) {
                continue;
            }
            read_parts.insert(seg.file.clone());
            let path = join_path(&self.dir(), &seg.file);
            if let Some(bytes) = self.store.read(&path).await? {
                let decoded = library_shard::decode_library_shard(&bytes)?;
                if let Some(roots) = &self.cover_roots {
                    let cover_ids = library_shard::apply_music_cover_blobs(
                        roots,
                        &decoded.tracks,
                        &decoded.cover_indices,
                        &decoded.covers,
                    );
                    for id in &cover_ids {
                        let _ = self.db.set_track_cover_state(id, 2);
                    }
                }
                data_parts.push(decoded.tracks);
            }
        }
        for part in &manifest.tombstones {
            if skip_files.contains(&part.file) {
                continue;
            }
            read_parts.insert(part.file.clone());
            let path = join_path(&self.dir(), &part.file);
            if let Some(bytes) = self.store.read(&path).await? {
                let decoded = library_shard::decode_library_shard(&bytes)?;
                tomb_parts.push(decoded.tombstones);
            } else {
                tomb_parts.push(vec![]);
            }
        }

        Ok((merge_cloud_parts(&data_parts, &tomb_parts), read_parts))
    }

    async fn append_delta(&self, changed: &[LibraryTrack]) -> SyncResult<LibraryManifest> {
        if changed.is_empty() {
            return self.read_manifest().await;
        }
        let revs: Vec<i64> = changed.iter().map(|t| t.rev).collect();
        let from = *revs.iter().min().unwrap_or(&0);
        let to = *revs.iter().max().unwrap_or(&0);
        let dev = device_id(&self.db);
        let bytes = library_shard::encode_track_shard(WMP_KIND_SEG, &dev, from, to, changed, None)?;
        let file = part_name(SEGMENT_PREFIX, from);
        self.store.ensure_dir(&self.dir()).await?;
        self.store
            .write(&join_path(&self.dir(), &file), &bytes)
            .await?;
        let mut latest = self.read_manifest().await?;
        latest.segments.push(IndexPartEntry {
            file,
            from,
            to,
            count: changed.len() as i64,
            bytes: bytes.len() as i64,
        });
        self.write_manifest(latest.clone()).await?;
        Ok(latest)
    }

    async fn append_tombstones(&self, tombs: &[TombRecord]) -> SyncResult<LibraryManifest> {
        if tombs.is_empty() {
            return self.read_manifest().await;
        }
        let revs: Vec<i64> = tombs.iter().map(|t| t.rev).collect();
        let from = *revs.iter().min().unwrap_or(&0);
        let to = *revs.iter().max().unwrap_or(&0);
        let dev = device_id(&self.db);
        let bytes = library_shard::encode_tomb_shard(&dev, from, to, tombs)?;
        let file = part_name(TOMB_PREFIX, from);
        self.store.ensure_dir(&self.dir()).await?;
        self.store
            .write(&join_path(&self.dir(), &file), &bytes)
            .await?;
        let mut latest = self.read_manifest().await?;
        latest.tombstones.push(IndexPartEntry {
            file,
            from,
            to,
            count: tombs.len() as i64,
            bytes: bytes.len() as i64,
        });
        self.write_manifest(latest.clone()).await?;
        Ok(latest)
    }

    async fn rebuild(&self, tracks: &[LibraryTrack], tracks_per_shard: usize) -> SyncResult<LibraryManifest> {
        let per = tracks_per_shard.clamp(50, 5000);
        let previous = self.read_manifest().await?;
        self.store.ensure_dir(&self.dir()).await?;
        let dev = device_id(&self.db);
        let mut shards = Vec::new();
        let mut max_rev = 0i64;
        let slices: Vec<&[LibraryTrack]> = if tracks.is_empty() {
            vec![]
        } else {
            tracks.chunks(per).collect()
        };
        for (i, slice) in slices.into_iter().enumerate() {
            for t in slice {
                if t.rev > max_rev {
                    max_rev = t.rev;
                }
            }
            let bytes = library_shard::encode_track_shard(WMP_KIND_BASE, &dev, 0, 0, slice, self.cover_roots.as_ref())?;
            let file = format!(
                "{SHARD_PREFIX}{:04}-{:06x}.wdmm",
                i + 1,
                (max_rev.wrapping_mul(31).wrapping_add(i as i64).unsigned_abs()) % 0xFFFFFF
            );
            self.store
                .write(&join_path(&self.dir(), &file), &bytes)
                .await?;
            shards.push(BaseShardEntry {
                file,
                count: slice.len() as i64,
                bytes: bytes.len() as i64,
            });
        }
        let manifest = LibraryManifest {
            format: FORMAT.into(),
            format_version: FORMAT_VERSION,
            base: ManifestBase {
                up_to: max_rev,
                shards: shards.clone(),
            },
            segments: vec![],
            tombstones: vec![],
            updated_at: chrono::Utc::now().to_rfc3339(),
        };
        self.write_manifest(manifest.clone()).await?;
        // best-effort delete old parts
        let keep: HashSet<String> = shards.iter().map(|s| s.file.clone()).collect();
        for old in previous
            .base
            .shards
            .iter()
            .map(|s| &s.file)
            .chain(previous.segments.iter().map(|s| &s.file))
            .chain(previous.tombstones.iter().map(|s| &s.file))
        {
            if keep.contains(old) {
                continue;
            }
            let _ = self.store.remove(&join_path(&self.dir(), old)).await;
        }
        Ok(manifest)
    }

    fn load_cursor(&self) -> SyncResult<(i64, i64, HashMap<String, i64>)> {
        match self.db.get_sync_state(&self.cursor_key())? {
            None => Ok((0, 0, HashMap::new())),
            Some(s) => {
                let parts: HashMap<String, i64> = serde_json::from_str(&s.parts).unwrap_or_default();
                Ok((s.last_seq, s.base_up_to, parts))
            }
        }
    }

    fn save_cursor(
        &self,
        manifest: &LibraryManifest,
        cursor_parts: &HashMap<String, i64>,
        read_parts: &HashSet<String>,
        highest_local_rev: i64,
    ) -> SyncResult<()> {
        let mut parts = cursor_parts.clone();
        for name in read_parts {
            let to = manifest
                .segments
                .iter()
                .find(|s| &s.file == name)
                .map(|s| s.to)
                .or_else(|| {
                    manifest
                        .tombstones
                        .iter()
                        .find(|t| &t.file == name)
                        .map(|t| t.to)
                })
                .unwrap_or(0);
            parts.insert(name.clone(), to);
        }
        for s in manifest.segments.iter().chain(manifest.tombstones.iter()) {
            if parts.contains_key(&s.file) {
                continue;
            }
            if read_parts.is_empty() && s.to <= highest_local_rev {
                continue;
            }
            if s.to <= highest_local_rev {
                parts.insert(s.file.clone(), s.to);
            }
        }
        let mut last_seq = highest_local_rev;
        for &rev in parts.values() {
            if rev > last_seq {
                last_seq = rev;
            }
        }
        if manifest.base.up_to > last_seq {
            last_seq = manifest.base.up_to;
        }
        self.db.save_sync_state(&SyncState {
            remote_key: self.cursor_key(),
            last_seq,
            base_up_to: manifest.base.up_to,
            parts: serde_json::to_string(&parts).unwrap_or_else(|_| "{}".into()),
            updated_at: chrono::Utc::now().to_rfc3339(),
        })?;
        Ok(())
    }

    fn local_by_key(&self) -> SyncResult<HashMap<String, LibraryTrack>> {
        let local = self.db.list_tracks()?;
        Ok(local
            .into_iter()
            .map(|t| (track_identity_key(&t.account_id, &t.remote_path), t))
            .collect())
    }

    /// Audit the remote music library.
    ///
    /// Reads the remote manifest and directory listing, then cross-references to
    /// find shards referenced in the manifest that are missing on the remote, and
    /// files present on the remote that are not referenced by the manifest.
    pub async fn audit(&self) -> SyncResult<crate::SyncAuditResult> {
        let manifest = self.read_manifest().await?;
        let remote_files: std::collections::HashSet<String> =
            self.store.list_files(&self.dir()).await?.into_iter().collect();

        // Collect every filename the manifest references (base shards + incremental parts).
        let mut referenced = std::collections::HashSet::new();
        for s in &manifest.base.shards {
            referenced.insert(s.file.clone());
        }
        for p in manifest.segments.iter().chain(manifest.tombstones.iter()) {
            referenced.insert(p.file.clone());
        }

        let mut missing: Vec<String> = referenced.iter()
            .filter(|f| !remote_files.contains(*f))
            .cloned()
            .collect();
        missing.sort();

        let index_name = INDEX_NAME.to_string();
        let mut orphans: Vec<String> = remote_files.iter()
            .filter(|f| *f != &index_name && !referenced.contains(*f))
            .cloned()
            .collect();
        orphans.sort();

        let healthy = missing.is_empty() && orphans.is_empty();
        let summary = if healthy {
            format!(
                "healthy — {} base shards, {} segments, {} tombstones",
                manifest.base.shards.len(),
                manifest.segments.len(),
                manifest.tombstones.len(),
            )
        } else {
            format!("{} missing, {} orphans", missing.len(), orphans.len())
        };

        Ok(crate::SyncAuditResult { missing, orphans, healthy, summary })
    }

    /// Estimate the cost of a full rebuild without doing any network I/O.
    ///
    /// `tracks_per_shard` is clamped to `[50, 5000]`; pass 0 for the default (500).
    pub async fn estimate(&self, tracks_per_shard: usize) -> SyncResult<crate::SyncEstimateResult> {
        let per = if tracks_per_shard == 0 { DEFAULT_TRACKS_PER_SHARD } else { tracks_per_shard.clamp(50, 5000) };
        let count = self.db.list_tracks()?.len();
        let shards = if count == 0 { 0 } else { (count + per - 1) / per };
        let estimated_bytes = (count as i64) * 250; // ~250 B per track compressed

        let manifest = self.read_manifest().await?;
        let fragment_count = manifest.segments.len() + manifest.tombstones.len();
        let fragment_bytes: i64 = manifest.segments.iter().chain(manifest.tombstones.iter())
            .map(|p| p.bytes)
            .sum();

        Ok(crate::SyncEstimateResult { shards, estimated_bytes, fragment_count, fragment_bytes })
    }

    /// Delete the given orphan filenames from the remote music library directory.
    ///
    /// Returns the number of files successfully deleted. Individual delete
    /// failures are silently ignored (best-effort).
    pub async fn tidy_orphans(&self, orphans: &[String]) -> SyncResult<usize> {
        let mut deleted = 0usize;
        for name in orphans {
            let path = join_path(&self.dir(), name);
            if self.store.remove(&path).await.is_ok() {
                deleted += 1;
            }
        }
        Ok(deleted)
    }
}

#[async_trait]
impl SyncEngine for LibrarySyncEngine {
    async fn sync_incremental(&self) -> SyncResult<Vec<SyncOutcome>> {
        let mut outcomes = Vec::new();
        let manifest = self.read_manifest().await?;
        let (last_seq, base_up_to, cursor_parts) = self.load_cursor()?;

        let skip: HashSet<String> = if manifest.base.up_to > base_up_to {
            HashSet::new()
        } else {
            cursor_parts.keys().cloned().collect()
        };
        let (cloud, read_parts) = self.read_cloud(&manifest, &skip).await?;
        let remote_by_key: HashMap<String, LibraryTrack> = cloud
            .tracks
            .iter()
            .map(|t| (track_identity_key(&t.account_id, &t.remote_path), t.clone()))
            .collect();

        let local_map = self.local_by_key()?;
        let mut to_upload = Vec::new();
        for (key, t) in &local_map {
            if let Some(r) = remote_by_key.get(key) {
                if t.rev > r.rev {
                    to_upload.push(t.clone());
                }
                continue;
            }
            if t.rev > last_seq && !( !cursor_parts.is_empty() && t.rev <= base_up_to ) {
                to_upload.push(t.clone());
            }
        }

        let mut to_adopt = Vec::new();
        for t in &cloud.tracks {
            let key = track_identity_key(&t.account_id, &t.remote_path);
            if cloud.tombs.contains_key(&key) {
                // still in tracks means tomb didn't win; ok
            }
            match local_map.get(&key) {
                None => to_adopt.push(t.clone()),
                Some(l) if t.rev > l.rev => to_adopt.push(t.clone()),
                _ => {}
            }
        }

        // Apply cloud tombs to local (peer deletion) — required for Core parity tests.
        for (key, &tomb_rev) in &cloud.tombs {
            if let Some(local) = local_map.get(key) {
                if tomb_rev >= local.rev {
                    let _ = self.db.delete_track(&local.id);
                    outcomes.push(SyncOutcome {
                        entity_id: local.id.clone(),
                        action: SyncAction::Pulled,
                        rev: RevClock::new(tomb_rev.max(1) as u64),
                    });
                }
            }
        }

        for t in &to_adopt {
            self.db.upsert_track(t)?;
            outcomes.push(SyncOutcome {
                entity_id: t.id.clone(),
                action: SyncAction::Pulled,
                rev: RevClock::new(t.rev.max(1) as u64),
            });
        }

        let pending = self.db.list_unpushed_deleted_tracks()?;
        let mut resurrected = HashSet::new();
        let mut to_publish = Vec::new();
        for row in &pending {
            let key = track_identity_key(&row.source_name, &row.remote_path);
            // Skip: cloud already has a tomb at >= our rev — republishing is redundant.
            if cloud.tombs.get(&key).copied().unwrap_or(0) >= row.rev && row.rev > 0 {
                resurrected.insert(row.rev); // mark pushed, skip re-publish
                continue;
            }
            if let Some(live) = local_map.get(&key) {
                if live.rev > row.rev {
                    resurrected.insert(row.rev);
                    continue;
                }
            }
            // also check just-adopted
            if let Some(live) = to_adopt.iter().find(|t| {
                track_identity_key(&t.account_id, &t.remote_path) == key
            }) {
                if live.rev > row.rev {
                    resurrected.insert(row.rev);
                    continue;
                }
            }
            to_publish.push(TombRecord {
                source_name: row.source_name.clone(),
                remote_path: row.remote_path.clone(),
                rev: row.rev,
                deleted_at: row.deleted_at.clone(),
                sync_key: None, // music never writes syncKey
            });
        }
        if !resurrected.is_empty() {
            let revs: Vec<i64> = resurrected.iter().copied().collect();
            // mark pushed so they don't reappear; Android clearTombstones
            let _ = self.db.mark_deleted_tracks_pushed(&revs);
        }

        if to_upload.is_empty() && to_publish.is_empty() {
            let highest = to_adopt.iter().map(|t| t.rev).fold(last_seq, i64::max);
            let latest = self.read_manifest().await?;
            self.save_cursor(&latest, &cursor_parts, &read_parts, highest)?;
            info!(
                "library shard sync: no upload (adopted={})",
                to_adopt.len()
            );
            return Ok(outcomes);
        }

        if !to_publish.is_empty() {
            self.append_tombstones(&to_publish).await?;
            let revs: Vec<i64> = to_publish.iter().map(|t| t.rev).collect();
            self.db.mark_deleted_tracks_pushed(&revs)?;
            for t in &to_publish {
                outcomes.push(SyncOutcome {
                    entity_id: format!("tomb:{}|{}", t.source_name, t.remote_path),
                    action: SyncAction::Pushed,
                    rev: RevClock::new(t.rev.max(1) as u64),
                });
            }
        }
        if !to_upload.is_empty() {
            self.append_delta(&to_upload).await?;
            for t in &to_upload {
                outcomes.push(SyncOutcome {
                    entity_id: t.id.clone(),
                    action: SyncAction::Pushed,
                    rev: RevClock::new(t.rev.max(1) as u64),
                });
            }
        }

        let highest = to_upload.iter().map(|t| t.rev).fold(last_seq, i64::max);
        let latest = self.read_manifest().await?;
        self.save_cursor(&latest, &cursor_parts, &read_parts, highest)?;
        info!(
            "library shard sync: adopted={} tombs={} uploaded={}",
            to_adopt.len(),
            to_publish.len(),
            to_upload.len()
        );
        Ok(outcomes)
    }

    async fn sync_full(&self) -> SyncResult<Vec<SyncOutcome>> {
        let mut outcomes = Vec::new();
        let manifest = self.read_manifest().await?;
        let (cloud, _) = self.read_cloud(&manifest, &HashSet::new()).await?;
        let local_map = self.local_by_key()?;

        // Hoist once — not per cloud track.
        let pending_tombs = self.db.list_unpushed_deleted_tracks()?;

        for t in &cloud.tracks {
            let key = track_identity_key(&t.account_id, &t.remote_path);
            if cloud.tombs.get(&key).copied().unwrap_or(0) >= t.rev && cloud.tombs.contains_key(&key) {
                // shouldn't be in tracks if tomb won; skip
                continue;
            }
            // Never resurrect a track that has a local unpushed tombstone.
            let locally_tombstoned = pending_tombs.iter().any(|d| {
                d.source_name == t.account_id && d.remote_path == t.remote_path && d.rev >= t.rev
            });
            if locally_tombstoned {
                continue;
            }
            match local_map.get(&key) {
                None => {
                    self.db.upsert_track(t)?;
                    outcomes.push(SyncOutcome {
                        entity_id: t.id.clone(),
                        action: SyncAction::Pulled,
                        rev: RevClock::new(t.rev.max(1) as u64),
                    });
                }
                Some(l) if t.rev > l.rev => {
                    self.db.upsert_track(t)?;
                    outcomes.push(SyncOutcome {
                        entity_id: t.id.clone(),
                        action: SyncAction::Pulled,
                        rev: RevClock::new(t.rev.max(1) as u64),
                    });
                }
                _ => {}
            }
        }

        // Refresh local after adopt
        let tracks = self.db.list_tracks()?;
        // Filter out tracks that have local unpushed tombs? Android uses _library.tracks after adopt
        // which already excludes deleted. Our list_tracks returns live rows only.
        let written = self.rebuild(&tracks, DEFAULT_TRACKS_PER_SHARD).await?;
        let _ = self.db.purge_deleted_tracks_up_to(written.base.up_to)?;
        let _ = self.db.clear_sync_state(Some(CURSOR_PREFIX))?;
        outcomes.push(SyncOutcome {
            entity_id: "library-rebuild".into(),
            action: SyncAction::Pushed,
            rev: RevClock::new(written.base.up_to.max(1) as u64),
        });
        Ok(outcomes)
    }
}
