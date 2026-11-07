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
use crate::protocol::{self, BaseShardEntry, IndexPartEntry, ManifestBase, ShardStore};
use crate::{join_path, SyncEngine, SyncError, SyncOutcome, SyncResult};
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use wdmm_core::models::LibraryTrack;
use wdmm_drivers::WdmmVfs;
use wdmm_storage::LibraryDatabase;

pub use crate::protocol::SyncManifest as LibraryManifest;

pub(crate) const FORMAT: &str = "wmp_library_index";
pub(crate) const FORMAT_VERSION: i64 = 1;
const SHARD_PREFIX: &str = "lib-";
pub const DEFAULT_TRACKS_PER_SHARD: usize = 500;
const CURSOR_PREFIX: &str = "library:";

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

fn music_dir(remote_dir: &str) -> String {
    join_path(remote_dir, "library/music")
}

/// Music library sync engine (replaces toy library.json).
pub struct LibrarySyncEngine {
    pub(crate) db: Arc<LibraryDatabase>,
    store: ShardStore,
    remote_dir: String,
    cover_roots: Option<CoverRoots>,
    device: String,
}

impl LibrarySyncEngine {
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
        music_dir(&self.remote_dir)
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
            let decoded = library_shard::decode_library_shard(&bytes)?;
            if file.starts_with(protocol::TOMB_PREFIX) {
                tomb_parts.push(decoded.tombstones);
            } else {
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

        Ok((merge_cloud_parts(&data_parts, &tomb_parts), read_parts))
    }

    pub(crate) async fn append_delta(&self, changed: &[LibraryTrack]) -> SyncResult<LibraryManifest> {
        if changed.is_empty() {
            return self.read_manifest().await;
        }
        let revs: Vec<i64> = changed.iter().map(|t| t.rev).collect();
        let from = *revs.iter().min().unwrap_or(&0);
        let to = *revs.iter().max().unwrap_or(&0);
        let dev = self.device.clone();
        let bytes = library_shard::encode_track_shard(WMP_KIND_SEG, &dev, from, to, changed, None)?;
        let file = protocol::part_name(protocol::SEGMENT_PREFIX, from, &self.device);
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

    pub(crate) async fn append_tombstones(&self, tombs: &[TombRecord]) -> SyncResult<LibraryManifest> {
        if tombs.is_empty() {
            return self.read_manifest().await;
        }
        let revs: Vec<i64> = tombs.iter().map(|t| t.rev).collect();
        let from = *revs.iter().min().unwrap_or(&0);
        let to = *revs.iter().max().unwrap_or(&0);
        let dev = self.device.clone();
        let bytes = library_shard::encode_tomb_shard(&dev, from, to, tombs)?;
        let file = protocol::part_name(protocol::TOMB_PREFIX, from, &self.device);
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

    pub(crate) async fn rebuild(&self, tracks: &[LibraryTrack], tracks_per_shard: usize) -> SyncResult<LibraryManifest> {
        let per = tracks_per_shard.clamp(50, 5000);
        let previous = self.read_manifest().await?;
        self.store.ensure_dir(&self.dir()).await?;
        let dev = self.device.clone();
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
                // Tombstoned deletions count as consumed by the base too, so a
                // rebuild never lets the cursor go backwards past them.
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
            // §3.2: a rebuild must never drop tombstones — they are the only
            // evidence that a row was deleted on some device, and losing them
            // resurrects deleted tracks on the next full sync.
            tombstones: previous.tombstones.clone(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        };
        self.write_manifest(manifest.clone()).await?;
        // best-effort delete old parts (base shards + segments are folded into
        // the new base; tombstone files are preserved).
        let keep: HashSet<String> = shards.iter().map(|s| s.file.clone()).collect();
        for old in previous
            .base
            .shards
            .iter()
            .map(|s| &s.file)
            .chain(previous.segments.iter().map(|s| &s.file))
        {
            if keep.contains(old) {
                continue;
            }
            let _ = self.store.remove(&join_path(&self.dir(), old)).await;
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

        let index_name = protocol::INDEX_NAME.to_string();
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
        let shards = if count == 0 { 0 } else { count.div_ceil(per) };
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
        crate::engine::run_incremental(self).await
    }

    async fn sync_full(&self) -> SyncResult<Vec<SyncOutcome>> {
        crate::engine::run_full(self).await
    }
}

#[cfg(test)]
mod shard_name_tests {
    use crate::protocol::part_name;

    #[test]
    fn shard_names_are_unique_per_write() {
        // Two writes that happen at the same rev and the same millisecond must
        // still get different names: a shard file is never overwritten.
        let a = part_name("seg-", 100, "device-aaaa-bbbb");
        let b = part_name("seg-", 100, "device-aaaa-bbbb");
        assert_ne!(a, b);
        assert!(a.starts_with("seg-100-"), "{a}");
        assert!(a.ends_with(".wdmm"), "{a}");
    }

    #[test]
    fn shard_names_carry_the_device_tag() {
        let a = part_name("seg-", 100, "device-aaaa");
        let b = part_name("seg-", 100, "device-bbbb");
        assert_ne!(a, b);
        assert!(a.contains("-devicea"), "{a}");
        assert!(b.contains("-deviceb"), "{b}");
    }
}
