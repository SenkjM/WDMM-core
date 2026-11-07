//! Manga cloud library sync — Android shard+tombstone protocol.
//!
//! Remote layout: `<remote_dir>/library/manga/{index.json,lib-*,seg-*,del-*,progress.json}`.
//! Cursor key: `manga:{remote_dir}`.
//!
//! Incremental sync never rebuilds.  The manifest base is re-read only when
//! `base.up_to` advances beyond the saved cursor or the cursor is empty.
//! Segment and tombstone shards (`seg-*`, `del-*`) never carry covers.
//! Full sync (`sync_full`) is the only path that rebuilds lib-* base shards;
//! it may embed covers when [`MangaLibrarySyncEngine::cover_roots`] is `Some`.

use crate::cover::CoverRoots;
use crate::library_manga_shard::{
    apply_manga_cover_blobs, decode_manga_shard, encode_manga_shard, encode_manga_tomb_shard,
    MangaSyncRow,
};
use crate::library_music::{BaseShardEntry, IndexPartEntry, LibraryManifest, ManifestBase};
use crate::library_shard::{TombRecord, WMP_KIND_BASE, WMP_KIND_SEG};
use crate::{join_path, SyncAction, SyncEngine, SyncError, SyncOutcome, SyncResult};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::info;
use wdmm_core::rev_clock::RevClock;
use wdmm_drivers::{WdmmVfs, DriverError};
use wdmm_storage::{
    manga_id_for_remote, manga_sync_key, normalize_manga_remote_path, LibraryDatabase, MangaPage,
    MangaProgress, SyncState,
};

const FORMAT: &str = "wmp_manga_library_index";
const FORMAT_VERSION: i64 = 1;
const INDEX_NAME: &str = "index.json";
const SHARD_PREFIX: &str = "lib-";
const SEGMENT_PREFIX: &str = "seg-";
const TOMB_PREFIX: &str = "del-";
const CURSOR_PREFIX: &str = "manga:";

/// Keeps shard names unique inside one process when two writes share a stamp.
static PART_SEQ: AtomicU64 = AtomicU64::new(0);
const PROGRESS_NAME: &str = "progress.json";
const FORMAT_PROGRESS: &str = "wmp_manga_progress";
const FORMAT_VERSION_PROGRESS: i64 = 1;
pub const DEFAULT_MANGAS_PER_SHARD: usize = 500;

// ── Progress doc ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProgressEntry {
    #[serde(rename = "mangaId")]
    manga_id: String,
    page: i64,
    #[serde(rename = "updatedAt")]
    updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProgressDoc {
    format: String,
    #[serde(rename = "formatVersion")]
    format_version: i64,
    #[serde(rename = "deviceId")]
    device_id: String,
    #[serde(rename = "updatedAt")]
    updated_at: String,
    #[serde(default)]
    entries: Vec<ProgressEntry>,
}

impl Default for ProgressDoc {
    fn default() -> Self {
        Self {
            format: FORMAT_PROGRESS.into(),
            format_version: FORMAT_VERSION_PROGRESS,
            device_id: "core-device".into(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            entries: vec![],
        }
    }
}

// ── Cloud state ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
struct CloudManga {
    rows: Vec<MangaSyncRow>,
    /// sync_key → highest tomb rev seen
    tombs: HashMap<String, i64>,
}

/// Resolve the merge key for a tombstone: prefer stored `sync_key`, else container rule.
fn tomb_key(t: &TombRecord) -> String {
    t.sync_key.clone().unwrap_or_else(|| {
        format!("{}\x00{}", t.source_name, normalize_manga_remote_path(&t.remote_path))
    })
}

fn merge_manga_cloud_parts(
    data_parts: &[Vec<MangaSyncRow>],
    tomb_parts: &[Vec<TombRecord>],
) -> CloudManga {
    let mut by_key: HashMap<String, MangaSyncRow> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for rows in data_parts {
        for row in rows {
            let key = manga_sync_key(
                &row.manga.id,
                &row.manga.format,
                &row.manga.source_name,
                &row.manga.remote_path,
            );
            match by_key.get(&key) {
                None => {
                    by_key.insert(key.clone(), row.clone());
                    order.push(key);
                }
                Some(kept) if row.manga.rev > kept.manga.rev => {
                    by_key.insert(key, row.clone());
                }
                _ => {}
            }
        }
    }
    let mut tombs: HashMap<String, i64> = HashMap::new();
    for rows in tomb_parts {
        for t in rows {
            let key = tomb_key(t);
            let prev = tombs.get(&key).copied().unwrap_or(0);
            if t.rev > prev {
                tombs.insert(key.clone(), t.rev);
            }
            if let Some(live) = by_key.get(&key) {
                if t.rev >= live.manga.rev {
                    by_key.remove(&key);
                    order.retain(|k| k != &key);
                }
            }
        }
    }
    CloudManga {
        rows: order.into_iter().filter_map(|k| by_key.remove(&k)).collect(),
        tombs,
    }
}

// ── Remote store ──────────────────────────────────────────────────────────────

struct DriverStore {
    driver: Arc<dyn WdmmVfs>,
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

// ── Helpers ───────────────────────────────────────────────────────────────────

fn manga_dir(remote_dir: &str) -> String {
    join_path(remote_dir, "library/manga")
}

fn part_name(prefix: &str, rev: i64, device: &str) -> String {
    let stamp = if rev > 0 { rev } else { chrono::Utc::now().timestamp_millis() };
    // Shard files are immutable and must never share a name. The old name came
    // from `rev` alone, so two devices writing at the same rev picked the same
    // file and one delta was silently lost.
    let tag: String = device
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect();
    let nanos = chrono::Utc::now().timestamp_subsec_nanos();
    let seq = PART_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}{stamp}-{tag}-{nanos:08x}{seq:04x}.wdmm")
}

fn device_id(db: &LibraryDatabase) -> String {
    // Persisted in KV (key `wdmm_device_id`); generated as a UUID v4 on first use.
    // Shared with the music engine so the `deviceId` shard field is stable per device.
    wdmm_storage::device_id_or_generate(db)
}

// ── Engine ────────────────────────────────────────────────────────────────────

/// Manga library sync engine (mirrors `LibrarySyncEngine`).
pub struct MangaLibrarySyncEngine {
    db: Arc<LibraryDatabase>,
    store: DriverStore,
    remote_dir: String,
    cover_roots: Option<CoverRoots>,
    device: String,
}

impl MangaLibrarySyncEngine {
    pub fn new(
        db: Arc<LibraryDatabase>,
        driver: Arc<dyn WdmmVfs>,
        remote_dir: impl Into<String>,
        cover_roots: Option<CoverRoots>,
    ) -> Self {
        let device = device_id(&db);
        Self {
            db,
            store: DriverStore { driver },
            remote_dir: remote_dir.into(),
            cover_roots,
            device,
        }
    }

    fn dir(&self) -> String {
        manga_dir(&self.remote_dir)
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
    ) -> SyncResult<(CloudManga, HashSet<String>)> {
        let mut data_parts: Vec<Vec<MangaSyncRow>> = Vec::new();
        let mut tomb_parts: Vec<Vec<TombRecord>> = Vec::new();
        let mut read_parts: HashSet<String> = HashSet::new();

        for shard in &manifest.base.shards {
            if skip_files.contains(&shard.file) { continue; }
            read_parts.insert(shard.file.clone());
            let path = join_path(&self.dir(), &shard.file);
            if let Some(bytes) = self.store.read(&path).await? {
                let decoded = decode_manga_shard(&bytes)?;
                if let Some(roots) = &self.cover_roots {
                    let cover_ids = apply_manga_cover_blobs(roots, &decoded.rows, &decoded.cover_indices, &decoded.covers);
                    for id in &cover_ids {
                        let _ = self.db.set_manga_cover_state(id, 2);
                    }
                }
                data_parts.push(decoded.rows);
            }
        }
        for seg in &manifest.segments {
            if skip_files.contains(&seg.file) { continue; }
            read_parts.insert(seg.file.clone());
            let path = join_path(&self.dir(), &seg.file);
            if let Some(bytes) = self.store.read(&path).await? {
                let decoded = decode_manga_shard(&bytes)?;
                data_parts.push(decoded.rows);
            }
        }
        for part in &manifest.tombstones {
            if skip_files.contains(&part.file) { continue; }
            read_parts.insert(part.file.clone());
            let path = join_path(&self.dir(), &part.file);
            if let Some(bytes) = self.store.read(&path).await? {
                let decoded = decode_manga_shard(&bytes)?;
                tomb_parts.push(decoded.tombstones);
            } else {
                tomb_parts.push(vec![]);
            }
        }
        // A shard is written before the manifest that names it, so a peer can
        // legitimately see a fresh `seg-*`/`del-*` the index does not list yet.
        // Read those too: otherwise this device's cursor would treat the delta
        // as consumed and the two sides would drift apart. Base shards are left
        // for the next index read on purpose -- mixing two base generations is
        // worse than one delayed sync.
        let listed: HashSet<&str> = manifest
            .base
            .shards
            .iter()
            .map(|s| s.file.as_str())
            .chain(manifest.segments.iter().map(|s| s.file.as_str()))
            .chain(manifest.tombstones.iter().map(|s| s.file.as_str()))
            .collect();
        for file in self.store.list_files(&self.dir()).await? {
            if listed.contains(file.as_str()) || skip_files.contains(&file) {
                continue;
            }
            if !file.starts_with(SEGMENT_PREFIX) && !file.starts_with(TOMB_PREFIX) {
                continue;
            }
            let Some(bytes) = self.store.read(&join_path(&self.dir(), &file)).await? else {
                continue;
            };
            read_parts.insert(file.clone());
            let decoded = decode_manga_shard(&bytes)?;
            if file.starts_with(TOMB_PREFIX) {
                tomb_parts.push(decoded.tombstones);
            } else {
                if let Some(roots) = &self.cover_roots {
                    let cover_ids = apply_manga_cover_blobs(
                        roots,
                        &decoded.rows,
                        &decoded.cover_indices,
                        &decoded.covers,
                    );
                    for id in &cover_ids {
                        let _ = self.db.set_manga_cover_state(id, 2);
                    }
                }
                data_parts.push(decoded.rows);
            }
        }

        Ok((merge_manga_cloud_parts(&data_parts, &tomb_parts), read_parts))
    }

    async fn append_delta(&self, rows: &[MangaSyncRow]) -> SyncResult<LibraryManifest> {
        if rows.is_empty() {
            return self.read_manifest().await;
        }
        let revs: Vec<i64> = rows.iter().map(|r| r.manga.rev).collect();
        let from = *revs.iter().min().unwrap_or(&0);
        let to = *revs.iter().max().unwrap_or(&0);
        let dev = self.device.clone();
        let bytes = encode_manga_shard(WMP_KIND_SEG, &dev, from, to, rows, None)?;
        let file = part_name(SEGMENT_PREFIX, from, &self.device);
        self.store.ensure_dir(&self.dir()).await?;
        self.store.write(&join_path(&self.dir(), &file), &bytes).await?;
        let mut latest = self.read_manifest().await?;
        latest.segments.push(IndexPartEntry {
            file,
            from,
            to,
            count: rows.len() as i64,
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
        let dev = self.device.clone();
        let bytes = encode_manga_tomb_shard(&dev, from, to, tombs)?;
        let file = part_name(TOMB_PREFIX, from, &self.device);
        self.store.ensure_dir(&self.dir()).await?;
        self.store.write(&join_path(&self.dir(), &file), &bytes).await?;
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

    async fn rebuild(
        &self,
        rows: &[MangaSyncRow],
        mangas_per_shard: usize,
    ) -> SyncResult<LibraryManifest> {
        let per = mangas_per_shard.clamp(50, 5000);
        let previous = self.read_manifest().await?;
        self.store.ensure_dir(&self.dir()).await?;
        let dev = self.device.clone();
        let mut shards: Vec<BaseShardEntry> = Vec::new();
        let mut max_rev = 0i64;
        let slices: Vec<&[MangaSyncRow]> = if rows.is_empty() { vec![] } else { rows.chunks(per).collect() };
        for (i, slice) in slices.into_iter().enumerate() {
            for r in slice {
                if r.manga.rev > max_rev { max_rev = r.manga.rev; }
            }
            let bytes = encode_manga_shard(WMP_KIND_BASE, &dev, 0, 0, slice, self.cover_roots.as_ref())?;
            let file = format!(
                "{SHARD_PREFIX}{:04}-{:06x}.wdmm",
                i + 1,
                (max_rev.wrapping_mul(31).wrapping_add(i as i64).unsigned_abs()) % 0xFFFFFF
            );
            self.store.write(&join_path(&self.dir(), &file), &bytes).await?;
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
                up_to: max_rev.max(
                    previous
                        .tombstones
                        .iter()
                        .map(|part| part.to)
                        .max()
                        .unwrap_or(0),
                ),
                shards: shards.clone(),
            },
            segments: vec![],
            // §3.2: a rebuild must never drop tombstones, or deleted mangas come
            // back on the next full sync.
            tombstones: previous.tombstones.clone(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        };
        self.write_manifest(manifest.clone()).await?;
        // best-effort remove old parts folded into the new base (tombstone files
        // are preserved)
        let keep: HashSet<String> = shards.iter().map(|s| s.file.clone()).collect();
        for old in previous.base.shards.iter().map(|s| &s.file)
            .chain(previous.segments.iter().map(|s| &s.file))
        {
            if !keep.contains(old) {
                let _ = self.store.remove(&join_path(&self.dir(), old)).await;
            }
        }
        Ok(manifest)
    }

    fn load_cursor(&self) -> SyncResult<(i64, i64, HashMap<String, i64>)> {
        match self.db.get_sync_state(&self.cursor_key())? {
            None => Ok((0, 0, HashMap::new())),
            Some(s) => {
                let parts: HashMap<String, i64> =
                    serde_json::from_str(&s.parts).unwrap_or_default();
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
            let to = manifest.segments.iter().find(|s| &s.file == name).map(|s| s.to)
                .or_else(|| manifest.tombstones.iter().find(|t| &t.file == name).map(|t| t.to))
                .unwrap_or(0);
            parts.insert(name.clone(), to);
        }
        for s in manifest.segments.iter().chain(manifest.tombstones.iter()) {
            if parts.contains_key(&s.file) { continue; }
            if read_parts.is_empty() && s.to <= highest_local_rev { continue; }
            if s.to <= highest_local_rev {
                parts.insert(s.file.clone(), s.to);
            }
        }
        let mut last_seq = highest_local_rev;
        for &rev in parts.values() {
            if rev > last_seq { last_seq = rev; }
        }
        if manifest.base.up_to > last_seq { last_seq = manifest.base.up_to; }
        self.db.save_sync_state(&SyncState {
            remote_key: self.cursor_key(),
            last_seq,
            base_up_to: manifest.base.up_to,
            parts: serde_json::to_string(&parts).unwrap_or_else(|_| "{}".into()),
            updated_at: chrono::Utc::now().to_rfc3339(),
        })?;
        Ok(())
    }

    fn local_by_key(&self) -> SyncResult<HashMap<String, wdmm_storage::Manga>> {
        let local = self.db.list_mangas()?;
        Ok(local
            .into_iter()
            .map(|m| {
                let key = manga_sync_key(&m.id, &m.format, &m.source_name, &m.remote_path);
                (key, m)
            })
            .collect())
    }

    /// Audit the remote manga library.
    ///
    /// Cross-references the remote manifest with the actual remote directory
    /// listing to find missing shards and unreferenced orphan files.
    pub async fn audit(&self) -> SyncResult<crate::SyncAuditResult> {
        let manifest = self.read_manifest().await?;
        let remote_files: std::collections::HashSet<String> =
            self.store.list_files(&self.dir()).await?.into_iter().collect();

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
        let progress_name = PROGRESS_NAME.to_string();
        let mut orphans: Vec<String> = remote_files.iter()
            .filter(|f| *f != &index_name && *f != &progress_name && !referenced.contains(*f))
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

    /// Estimate the cost of a full manga rebuild without network I/O.
    ///
    /// `mangas_per_shard` is clamped to `[50, 5000]`; pass 0 for the default (500).
    pub async fn estimate(&self, mangas_per_shard: usize) -> SyncResult<crate::SyncEstimateResult> {
        let per = if mangas_per_shard == 0 { DEFAULT_MANGAS_PER_SHARD } else { mangas_per_shard.clamp(50, 5000) };
        let count = self.db.list_mangas()?.len();
        let shards = if count == 0 { 0 } else { count.div_ceil(per) };
        let estimated_bytes = (count as i64) * 300; // ~300 B per manga row

        let manifest = self.read_manifest().await?;
        let fragment_count = manifest.segments.len() + manifest.tombstones.len();
        let fragment_bytes: i64 = manifest.segments.iter().chain(manifest.tombstones.iter())
            .map(|p| p.bytes)
            .sum();

        Ok(crate::SyncEstimateResult { shards, estimated_bytes, fragment_count, fragment_bytes })
    }

    /// Delete the given orphan filenames from the remote manga library directory.
    ///
    /// Returns the number of files successfully deleted. Individual failures
    /// are silently ignored (best-effort).
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

    fn row_with_pages(db: &LibraryDatabase, m: &wdmm_storage::Manga) -> SyncResult<MangaSyncRow> {
        let pages = if m.format == "images" {
            db.list_pages(&m.id)?
                .into_iter()
                .map(|p| p.rel_path)
                .collect()
        } else {
            vec![]
        };
        Ok(MangaSyncRow {
            manga: m.clone(),
            pages,
        })
    }

    /// Adopt one remote row locally.
    ///
    /// Returns `false` for legacy loose-image rows whose id is derived from the
    /// remote path: those are re-scanned rather than synced as entities.
    fn adopt_row(db: &LibraryDatabase, row: &MangaSyncRow) -> SyncResult<bool> {
        if row.manga.format == "images" {
            let expected = manga_id_for_remote(&row.manga.source_name, &row.manga.remote_path);
            if row.manga.id == expected {
                return Ok(false);
            }
        }
        db.upsert_manga(&row.manga)?;
        Self::adopt_pages(db, &row.manga.id, &row.manga.format, &row.pages)?;
        Ok(true)
    }

    fn adopt_pages(
        db: &LibraryDatabase,
        manga_id: &str,
        format: &str,
        pages: &[String],
    ) -> SyncResult<()> {
        if format == "images" && !pages.is_empty() {
            let manga_pages: Vec<MangaPage> = pages
                .iter()
                .enumerate()
                .map(|(i, p)| MangaPage {
                    manga_id: manga_id.to_string(),
                    page_index: i as i64,
                    rel_path: p.clone(),
                })
                .collect();
            db.replace_pages(manga_id, &manga_pages)?;
        }
        Ok(())
    }

    /// Merge reading progress against `progress.json` (LWW per mangaId).
    async fn sync_progress(&self) -> SyncResult<()> {
        let path = join_path(&self.dir(), PROGRESS_NAME);
        let remote_doc: ProgressDoc = match self.store.read(&path).await? {
            None => ProgressDoc::default(),
            Some(bytes) => {
                let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
                if v.get("format").and_then(|f| f.as_str()) != Some(FORMAT_PROGRESS) {
                    ProgressDoc::default()
                } else {
                    serde_json::from_value(v).unwrap_or_default()
                }
            }
        };
        let remote_map: HashMap<String, &ProgressEntry> =
            remote_doc.entries.iter().map(|e| (e.manga_id.clone(), e)).collect();

        let local_all = self.db.list_progress()?;
        let local_map: HashMap<String, &MangaProgress> =
            local_all.iter().map(|p| (p.manga_id.clone(), p)).collect();

        let mut applied = false;
        // Cloud newer than local → adopt locally.
        for re in &remote_doc.entries {
            match local_map.get(&re.manga_id) {
                None => {
                    self.db.upsert_progress(&MangaProgress {
                        manga_id: re.manga_id.clone(),
                        page_index: re.page,
                        updated_at: re.updated_at.clone(),
                    })?;
                    applied = true;
                }
                Some(lp) if re.updated_at > lp.updated_at => {
                    self.db.upsert_progress(&MangaProgress {
                        manga_id: re.manga_id.clone(),
                        page_index: re.page,
                        updated_at: re.updated_at.clone(),
                    })?;
                    applied = true;
                }
                _ => {}
            }
        }

        // Decide whether to upload: any local entry missing or newer than cloud.
        let local_refresh = if applied { self.db.list_progress()? } else { local_all };
        let mut need_upload = false;
        for lp in &local_refresh {
            match remote_map.get(&lp.manga_id) {
                None => { need_upload = true; break; }
                Some(re) if lp.updated_at > re.updated_at => { need_upload = true; break; }
                _ => {}
            }
        }

        if need_upload {
            let entries: Vec<ProgressEntry> = local_refresh
                .iter()
                .map(|lp| ProgressEntry {
                    manga_id: lp.manga_id.clone(),
                    page: lp.page_index,
                    updated_at: lp.updated_at.clone(),
                })
                .collect();
            let doc = ProgressDoc {
                format: FORMAT_PROGRESS.into(),
                format_version: FORMAT_VERSION_PROGRESS,
                device_id: device_id(&self.db),
                updated_at: chrono::Utc::now().to_rfc3339(),
                entries,
            };
            self.store.ensure_dir(&self.dir()).await?;
            let bytes = serde_json::to_vec_pretty(&doc)?;
            self.store.write(&path, &bytes).await?;
        }
        Ok(())
    }
}

// ── SyncEngine impl ───────────────────────────────────────────────────────────

#[async_trait]
impl SyncEngine for MangaLibrarySyncEngine {
    async fn sync_incremental(&self) -> SyncResult<Vec<SyncOutcome>> {
        let mut outcomes = Vec::new();
        let manifest = self.read_manifest().await?;
        let (last_seq, base_up_to, cursor_parts) = self.load_cursor()?;

        // If remote base advanced past our cursor, re-read everything.
        let skip: HashSet<String> = if manifest.base.up_to > base_up_to {
            HashSet::new()
        } else {
            cursor_parts.keys().cloned().collect()
        };
        let (cloud, read_parts) = self.read_cloud(&manifest, &skip).await?;

        // Consumed parts are not re-read, so the remote view may be incomplete.
        let mut skipped_through = 0i64;
        if !skip.is_empty() {
            for shard in &manifest.base.shards {
                if skip.contains(&shard.file) {
                    skipped_through = skipped_through.max(manifest.base.up_to);
                }
            }
            for segment in &manifest.segments {
                if skip.contains(&segment.file) {
                    skipped_through = skipped_through.max(segment.to);
                }
            }
        }

        let remote_by_key = manga_row_map(&cloud.rows);
        let local_map = self.local_by_key()?;
        let synced_by_id = self.db.list_manga_synced_revs()?;

        // §3.1 reconciliation: `synced_rev` says whether a row is clean (its rev
        // is what the remote already has) or dirty (local edits not yet pushed).
        let mut to_upload: Vec<wdmm_storage::Manga> = Vec::new();
        let mut to_adopt: Vec<MangaSyncRow> = Vec::new();
        let mut to_delete: Vec<String> = Vec::new();
        let mut conflicts = 0usize;

        let mut keys: HashSet<String> = local_map.keys().cloned().collect();
        keys.extend(remote_by_key.keys().cloned());
        keys.extend(cloud.tombs.keys().cloned());

        for key in keys {
            let local = local_map.get(&key);
            let remote = remote_by_key.get(&key);
            let tomb = cloud.tombs.get(&key).copied();
            let Some(m) = local else {
                if let Some(r) = remote {
                    to_adopt.push(r.clone());
                }
                continue;
            };
            let synced = synced_by_id.get(&m.id).copied().unwrap_or(0);
            let dirty = m.rev > synced;
            match remote {
                Some(r) if r.manga.rev > m.rev => {
                    if dirty {
                        conflicts += 1;
                    }
                    to_adopt.push(r.clone());
                }
                Some(r) if r.manga.rev < m.rev => {
                    if dirty {
                        to_upload.push(m.clone());
                    }
                }
                Some(_) => {}
                None => match tomb {
                    Some(tomb_rev) if tomb_rev >= m.rev => {
                        if dirty {
                            conflicts += 1;
                        }
                        to_delete.push(m.id.clone());
                    }
                    Some(_) => {
                        if dirty {
                            to_upload.push(m.clone());
                        }
                    }
                    None if dirty => to_upload.push(m.clone()),
                    None if synced > 0 => {
                        if m.rev > skipped_through {
                            to_delete.push(m.id.clone());
                        }
                    }
                    None => to_upload.push(m.clone()),
                },
            }
        }

        let mut confirmed: HashMap<String, i64> = HashMap::new();
        for row in &to_adopt {
            if !Self::adopt_row(&self.db, row)? {
                continue;
            }
            confirmed.insert(row.manga.id.clone(), row.manga.rev);
            outcomes.push(SyncOutcome {
                entity_id: row.manga.id.clone(),
                action: SyncAction::Pulled,
                rev: RevClock::new(row.manga.rev.max(1) as u64),
            });
        }
        for id in &to_delete {
            self.db.delete_manga(id)?;
            outcomes.push(SyncOutcome {
                entity_id: id.clone(),
                action: SyncAction::Pulled,
                rev: RevClock::ZERO,
            });
        }

        // Publish local unpushed deletions as tombstones.
        let pending = self.db.list_unpushed_deleted_mangas()?;
        let mut resurrected: Vec<i64> = Vec::new();
        let mut to_publish: Vec<TombRecord> = Vec::new();
        for row in &pending {
            let key = &row.sync_key;
            let tomb_rev = row.rev.unwrap_or(0);
            if cloud.tombs.get(key).copied().unwrap_or(0) >= tomb_rev && tomb_rev > 0 {
                resurrected.push(tomb_rev);
                continue;
            }
            // Resurrected: a live local row with higher rev.
            if let Some(live) = local_map.get(key) {
                if live.rev > tomb_rev {
                    if let Some(r) = row.rev {
                        resurrected.push(r);
                    }
                    continue;
                }
            }
            // Resurrected by an adopted row with a higher rev.
            if let Some(adopted) = to_adopt
                .iter()
                .find(|r| manga_key_for_row(r) == *key)
            {
                if adopted.manga.rev > tomb_rev {
                    if let Some(r) = row.rev {
                        resurrected.push(r);
                    }
                    continue;
                }
            }
            to_publish.push(TombRecord {
                source_name: row.source_name.clone(),
                remote_path: row.remote_path.clone(),
                rev: tomb_rev,
                deleted_at: row.deleted_at.clone(),
                sync_key: Some(row.sync_key.clone()),
            });
        }
        if !resurrected.is_empty() {
            let _ = self.db.mark_deleted_mangas_pushed(&resurrected);
        }

        if !to_publish.is_empty() {
            self.append_tombstones(&to_publish).await?;
            let revs: Vec<i64> = to_publish.iter().map(|t| t.rev).collect();
            self.db.mark_deleted_mangas_pushed(&revs)?;
            for t in &to_publish {
                outcomes.push(SyncOutcome {
                    entity_id: format!("tomb:{}|{}", t.source_name, t.remote_path),
                    action: SyncAction::Pushed,
                    rev: RevClock::new(t.rev.max(1) as u64),
                });
            }
        }
        if !to_upload.is_empty() {
            let mut rows: Vec<MangaSyncRow> = Vec::with_capacity(to_upload.len());
            for m in &to_upload {
                rows.push(Self::row_with_pages(&self.db, m)?);
            }
            self.append_delta(&rows).await?;
            for r in &rows {
                confirmed.insert(r.manga.id.clone(), r.manga.rev);
                outcomes.push(SyncOutcome {
                    entity_id: r.manga.id.clone(),
                    action: SyncAction::Pushed,
                    rev: RevClock::new(r.manga.rev.max(1) as u64),
                });
            }
        }

        // Only rows the remote confirmed may be marked clean.
        self.db.set_manga_synced_revs(&confirmed)?;

        let highest = to_upload
            .iter()
            .map(|m| m.rev)
            .chain(to_adopt.iter().map(|r| r.manga.rev))
            .fold(last_seq, i64::max);
        let _ = self.db.observe_rev(highest);
        let latest = self.read_manifest().await?;
        self.save_cursor(&latest, &cursor_parts, &read_parts, highest)?;
        self.sync_progress().await?;
        info!(
            "manga shard sync: adopted={} deleted_local={} tombs={} uploaded={} conflicts={}",
            to_adopt.len(),
            to_delete.len(),
            to_publish.len(),
            to_upload.len(),
            conflicts
        );
        Ok(outcomes)
    }

    async fn sync_full(&self) -> SyncResult<Vec<SyncOutcome>> {
        let mut outcomes = Vec::new();
        let manifest = self.read_manifest().await?;
        let (cloud, _) = self.read_cloud(&manifest, &HashSet::new()).await?;
        let remote_by_key = manga_row_map(&cloud.rows);
        let local_map = self.local_by_key()?;
        let synced_by_id = self.db.list_manga_synced_revs()?;
        let pending_local = self.db.list_unpushed_deleted_mangas()?;

        let locally_tombstoned = |key: &str, rev: i64| -> bool {
            pending_local
                .iter()
                .any(|d| d.sync_key == key && d.rev.unwrap_or(0) >= rev)
        };

        // §3.2: reconcile before rebuilding — the remote view is authoritative for
        // rows it knows, remote absence deletes clean rows, and local unsynced
        // edits survive as uploads contained in the rebuilt base.
        let mut to_adopt: Vec<MangaSyncRow> = Vec::new();
        let mut to_delete: Vec<String> = Vec::new();

        let mut keys: HashSet<String> = local_map.keys().cloned().collect();
        keys.extend(remote_by_key.keys().cloned());
        keys.extend(cloud.tombs.keys().cloned());

        for key in keys {
            let local = local_map.get(&key);
            let remote = remote_by_key.get(&key);
            let tomb = cloud.tombs.get(&key).copied();
            match (local, remote) {
                (Some(m), Some(r)) => {
                    if r.manga.rev > m.rev && !locally_tombstoned(&key, r.manga.rev) {
                        to_adopt.push(r.clone());
                    }
                }
                (Some(m), None) => {
                    if let Some(tomb_rev) = tomb {
                        if tomb_rev >= m.rev {
                            to_delete.push(m.id.clone());
                            continue;
                        }
                    }
                    let synced = synced_by_id.get(&m.id).copied().unwrap_or(0);
                    if synced > 0 && m.rev == synced {
                        to_delete.push(m.id.clone());
                    }
                }
                (None, Some(r)) => {
                    if !locally_tombstoned(&key, r.manga.rev) {
                        to_adopt.push(r.clone());
                    }
                }
                (None, None) => {}
            }
        }

        for row in &to_adopt {
            if !Self::adopt_row(&self.db, row)? {
                continue;
            }
            outcomes.push(SyncOutcome {
                entity_id: row.manga.id.clone(),
                action: SyncAction::Pulled,
                rev: RevClock::new(row.manga.rev.max(1) as u64),
            });
        }
        for id in &to_delete {
            self.db.delete_manga(id)?;
            outcomes.push(SyncOutcome {
                entity_id: id.clone(),
                action: SyncAction::Pulled,
                rev: RevClock::ZERO,
            });
        }

        // Rebuild from the merged live set; tombstones are preserved (§3.2).
        let all_mangas = self.db.list_mangas()?;
        let mut rows: Vec<MangaSyncRow> = Vec::with_capacity(all_mangas.len());
        for m in &all_mangas {
            rows.push(Self::row_with_pages(&self.db, m)?);
        }
        let written = self.rebuild(&rows, DEFAULT_MANGAS_PER_SHARD).await?;

        // A base shard is the authoritative view: every row it carries is clean.
        let mut confirmed: HashMap<String, i64> = HashMap::new();
        for r in &rows {
            confirmed.insert(r.manga.id.clone(), r.manga.rev);
        }
        self.db.set_manga_synced_revs(&confirmed)?;

        // Publish local deletions that the remote has not seen yet.
        let pending = self.db.list_unpushed_deleted_mangas()?;
        let local_now: HashMap<String, i64> = self
            .db
            .list_mangas()?
            .into_iter()
            .map(|m| {
                (
                    manga_sync_key(&m.id, &m.format, &m.source_name, &m.remote_path),
                    m.rev,
                )
            })
            .collect();
        let mut superseded: Vec<i64> = Vec::new();
        let mut to_publish: Vec<TombRecord> = Vec::new();
        for row in &pending {
            let tomb_rev = row.rev.unwrap_or(0);
            if cloud.tombs.get(&row.sync_key).copied().unwrap_or(0) >= tomb_rev && tomb_rev > 0 {
                superseded.push(tomb_rev);
                continue;
            }
            if local_now.get(&row.sync_key).copied().unwrap_or(0) > tomb_rev {
                if let Some(r) = row.rev {
                    superseded.push(r);
                }
                continue;
            }
            to_publish.push(TombRecord {
                source_name: row.source_name.clone(),
                remote_path: row.remote_path.clone(),
                rev: tomb_rev,
                deleted_at: row.deleted_at.clone(),
                sync_key: Some(row.sync_key.clone()),
            });
        }
        if !superseded.is_empty() {
            let _ = self.db.mark_deleted_mangas_pushed(&superseded);
        }
        if !to_publish.is_empty() {
            self.append_tombstones(&to_publish).await?;
            let revs: Vec<i64> = to_publish.iter().map(|t| t.rev).collect();
            self.db.mark_deleted_mangas_pushed(&revs)?;
            for t in &to_publish {
                outcomes.push(SyncOutcome {
                    entity_id: format!("tomb:{}|{}", t.source_name, t.remote_path),
                    action: SyncAction::Pushed,
                    rev: RevClock::new(t.rev.max(1) as u64),
                });
            }
        }

        let highest = rows.iter().map(|r| r.manga.rev).fold(0, i64::max);
        let _ = self.db.observe_rev(highest);
        let _ = self.db.clear_sync_state(Some(CURSOR_PREFIX));
        self.sync_progress().await?;
        outcomes.push(SyncOutcome {
            entity_id: "manga-rebuild".into(),
            action: SyncAction::Pushed,
            rev: RevClock::new(written.base.up_to.max(1) as u64),
        });
        Ok(outcomes)
    }
}

/// Sync key for a cloud row (`manga_sync_key` wrapped for readability).
fn manga_key_for_row(row: &MangaSyncRow) -> String {
    manga_sync_key(
        &row.manga.id,
        &row.manga.format,
        &row.manga.source_name,
        &row.manga.remote_path,
    )
}

fn manga_row_map(rows: &[MangaSyncRow]) -> HashMap<String, MangaSyncRow> {
    rows.iter()
        .map(|r| (manga_key_for_row(r), r.clone()))
        .collect()
}
