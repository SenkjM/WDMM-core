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
use crate::library_shard::{TombRecord, WMP_KIND_BASE, WMP_KIND_SEG};
use crate::protocol::{
    self, BaseShardEntry, IndexPartEntry, ManifestBase, ShardStore, SyncManifest as LibraryManifest,
};
use crate::{join_path, SyncEngine, SyncError, SyncOutcome, SyncResult};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use wdmm_drivers::WdmmVfs;
use wdmm_storage::{
    manga_id_for_remote, manga_sync_key, normalize_manga_remote_path, LibraryDatabase, MangaPage,
    MangaProgress,
};

pub(crate) const FORMAT: &str = "wmp_manga_library_index";
pub(crate) const FORMAT_VERSION: i64 = 1;
const SHARD_PREFIX: &str = "lib-";
pub(crate) const CURSOR_PREFIX: &str = "manga:";
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
pub(crate) struct CloudManga {
    pub(crate) rows: Vec<MangaSyncRow>,
    /// sync_key → highest tomb rev seen
    pub(crate) tombs: HashMap<String, i64>,
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

fn manga_dir(remote_dir: &str) -> String {
    join_path(remote_dir, "library/manga")
}

pub struct MangaLibrarySyncEngine {
    pub(crate) db: Arc<LibraryDatabase>,
    store: ShardStore,
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
        let device = protocol::device_id(&db);
        Self {
            db,
            store: ShardStore { driver },
            remote_dir: remote_dir.into(),
            cover_roots,
            device,
        }
    }

    fn dir(&self) -> String {
        manga_dir(&self.remote_dir)
    }

    fn index_path(&self) -> String {
        join_path(&self.dir(), protocol::INDEX_NAME)
    }

    fn cursor_key(&self) -> String {
        format!("{CURSOR_PREFIX}{}", self.remote_dir)
    }

    pub(crate) async fn read_manifest(&self) -> SyncResult<LibraryManifest> {
        match self.store.read(&self.index_path()).await? {
            None => Ok(LibraryManifest::empty(FORMAT, FORMAT_VERSION)),
            Some(bytes) => {
                let v: serde_json::Value = serde_json::from_slice(&bytes)?;
                let found = v.get("format").and_then(|x| x.as_str());
                if found != Some(FORMAT) {
                    return Err(SyncError::RemoteFormatUnsupported {
                        found: found.map(str::to_string),
                        expected: FORMAT.into(),
                    });
                }
                let version = v.get("formatVersion").and_then(|x| x.as_i64()).unwrap_or(0);
                if version > FORMAT_VERSION {
                    return Err(SyncError::RemoteFormatUnsupported {
                        found: Some(format!("{FORMAT} v{version}")),
                        expected: format!("{FORMAT} v{FORMAT_VERSION}"),
                    });
                }
                Ok(serde_json::from_value(v)?)
            }
        }
    }

    pub(crate) async fn write_manifest(&self, mut m: LibraryManifest) -> SyncResult<()> {
        m.updated_at = chrono::Utc::now().to_rfc3339();
        m.format = FORMAT.into();
        m.format_version = FORMAT_VERSION;
        self.store.ensure_dir(&self.dir()).await?;
        let bytes = serde_json::to_vec_pretty(&m)?;
        self.store.write(&self.index_path(), &bytes).await
    }

    pub(crate) async fn read_cloud(
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
            if !file.starts_with(protocol::SEGMENT_PREFIX) && !file.starts_with(protocol::TOMB_PREFIX) {
                continue;
            }
            let Some(bytes) = self.store.read(&join_path(&self.dir(), &file)).await? else {
                continue;
            };
            read_parts.insert(file.clone());
            let decoded = decode_manga_shard(&bytes)?;
            if file.starts_with(protocol::TOMB_PREFIX) {
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

    pub(crate) async fn append_delta(&self, rows: &[MangaSyncRow]) -> SyncResult<LibraryManifest> {
        if rows.is_empty() {
            return self.read_manifest().await;
        }
        let revs: Vec<i64> = rows.iter().map(|r| r.manga.rev).collect();
        let from = *revs.iter().min().unwrap_or(&0);
        let to = *revs.iter().max().unwrap_or(&0);
        let dev = self.device.clone();
        let bytes = encode_manga_shard(WMP_KIND_SEG, &dev, from, to, rows, None)?;
        let file = protocol::part_name(protocol::SEGMENT_PREFIX, from, &self.device);
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

    pub(crate) async fn append_tombstones(&self, tombs: &[TombRecord]) -> SyncResult<LibraryManifest> {
        if tombs.is_empty() {
            return self.read_manifest().await;
        }
        let revs: Vec<i64> = tombs.iter().map(|t| t.rev).collect();
        let from = *revs.iter().min().unwrap_or(&0);
        let to = *revs.iter().max().unwrap_or(&0);
        let dev = self.device.clone();
        let bytes = encode_manga_tomb_shard(&dev, from, to, tombs)?;
        let file = protocol::part_name(protocol::TOMB_PREFIX, from, &self.device);
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

    pub(crate) async fn rebuild(
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

    pub(crate) fn load_cursor(&self) -> SyncResult<(i64, i64, HashMap<String, i64>)> {
        protocol::load_cursor(&self.db, &self.cursor_key())
    }

    pub(crate) fn save_cursor(
        &self,
        manifest: &LibraryManifest,
        cursor_parts: &HashMap<String, i64>,
        read_parts: &HashSet<String>,
        highest_local_rev: i64,
    ) -> SyncResult<()> {
        protocol::save_cursor(
            &self.db,
            &self.cursor_key(),
            manifest,
            cursor_parts,
            read_parts,
            highest_local_rev,
        )
    }

    /// Forget the saved cursor (a full rebuild replaced every base shard).
    pub(crate) fn reset_cursor(&self) -> SyncResult<()> {
        self.db.clear_sync_state(Some(CURSOR_PREFIX))?;
        Ok(())
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

        let index_name = protocol::INDEX_NAME.to_string();
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

    pub(crate) fn row_with_pages(db: &LibraryDatabase, m: &wdmm_storage::Manga) -> SyncResult<MangaSyncRow> {
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
    pub(crate) fn adopt_row(db: &LibraryDatabase, row: &MangaSyncRow) -> SyncResult<bool> {
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
    pub(crate) async fn sync_progress(&self) -> SyncResult<()> {
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
                device_id: protocol::device_id(&self.db),
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
        crate::engine::run_incremental(self).await
    }

    async fn sync_full(&self) -> SyncResult<Vec<SyncOutcome>> {
        crate::engine::run_full(self).await
    }
}
