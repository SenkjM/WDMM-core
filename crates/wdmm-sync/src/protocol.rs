//! Shared remote protocol for the library sync engines.
//!
//! The music and the manga libraries publish the same shape on the remote:
//! one `index.json` manifest plus immutable `lib-*` base shards, append-only
//! `seg-*` deltas and `del-*` tombstone packs. Only the payload encoding, the
//! entity directory and the manifest format string differ, so the layout, the
//! shard naming and the cursor bookkeeping live here instead of being copied
//! into every engine.
//!
//! Every file except `index.json` is immutable: a shard name is unique per
//! write (device tag + timestamp + process sequence), so two devices can never
//! overwrite each other's deltas even when they publish the same revision.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use wdmm_drivers::{DriverError, WdmmVfs};
use wdmm_storage::{LibraryDatabase, SyncState};

use crate::{SyncError, SyncResult};

/// Manifest file name inside an entity directory.
pub(crate) const INDEX_NAME: &str = "index.json";
/// Prefix of the immutable delta shards.
pub(crate) const SEGMENT_PREFIX: &str = "seg-";
/// Prefix of the tombstone packs.
pub(crate) const TOMB_PREFIX: &str = "del-";

/// Keeps shard names unique inside one process when two writes share a stamp.
static PART_SEQ: AtomicU64 = AtomicU64::new(0);

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
pub struct SyncManifest {
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

impl SyncManifest {
    /// An empty manifest for [format] at [format_version].
    ///
    /// There is deliberately no `Default`: the empty value has to carry the
    /// entity's own format string, otherwise a manga engine that fell back to
    /// `default()` would publish (and later reject) the music format.
    pub fn empty(format: &str, format_version: i64) -> Self {
        Self {
            format: format.into(),
            format_version,
            base: ManifestBase::default(),
            segments: vec![],
            tombstones: vec![],
            updated_at: chrono::Utc::now().to_rfc3339(),
        }
    }
}

/// Thin driver wrapper with the not-found semantics the sync protocol needs.
pub(crate) struct ShardStore {
    pub(crate) driver: Arc<dyn WdmmVfs>,
}

impl ShardStore {
    pub(crate) async fn read(&self, path: &str) -> SyncResult<Option<Vec<u8>>> {
        match self.driver.get_content(path).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(DriverError::NotFound(_)) => Ok(None),
            Err(e) => Err(SyncError::from(e)),
        }
    }

    pub(crate) async fn write(&self, path: &str, data: &[u8]) -> SyncResult<()> {
        self.driver.put_content(path, data).await?;
        Ok(())
    }

    pub(crate) async fn ensure_dir(&self, path: &str) -> SyncResult<()> {
        match self.driver.mkdir(path).await {
            Ok(()) => Ok(()),
            // Some WebDAV servers answer an existing collection with an error;
            // the following write reports a real failure anyway.
            Err(DriverError::Network(_)) => Ok(()),
            Err(e) => Err(SyncError::from(e)),
        }
    }

    pub(crate) async fn remove(&self, path: &str) -> SyncResult<()> {
        match self.driver.remove(path).await {
            Ok(()) => Ok(()),
            Err(DriverError::NotFound(_)) => Ok(()),
            Err(e) => Err(SyncError::from(e)),
        }
    }

    pub(crate) async fn list_files(&self, dir: &str) -> SyncResult<Vec<String>> {
        match self.driver.list(dir).await {
            Ok(items) => Ok(items
                .into_iter()
                .filter(|item| !item.is_dir)
                .map(|item| item.name)
                .collect()),
            Err(DriverError::NotFound(_)) => Ok(vec![]),
            Err(e) => Err(SyncError::from(e)),
        }
    }
}

/// Unique, immutable name for a delta or tombstone pack.
pub(crate) fn part_name(prefix: &str, rev: i64, device: &str) -> String {
    let stamp = if rev > 0 {
        rev
    } else {
        chrono::Utc::now().timestamp_millis()
    };
    let tag: String = device
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect();
    let nanos = chrono::Utc::now().timestamp_subsec_nanos();
    let seq = PART_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}{stamp}-{tag}-{nanos:08x}{seq:04x}.wdmm")
}

/// Stable per-device tag written into shard metadata.
pub(crate) fn device_id(db: &LibraryDatabase) -> String {
    // Persisted in KV (key `wdmm_device_id`); generated as a UUID v4 on first
    // use. Shared by every entity so the `deviceId` field is stable per device.
    wdmm_storage::device_id_or_generate(db)
}

/// Load `(last_seq, base_up_to, consumed parts)` for [cursor_key].
pub(crate) fn load_cursor(
    db: &LibraryDatabase,
    cursor_key: &str,
) -> SyncResult<(i64, i64, HashMap<String, i64>)> {
    match db.get_sync_state(cursor_key)? {
        None => Ok((0, 0, HashMap::new())),
        Some(state) => {
            let parts: HashMap<String, i64> =
                serde_json::from_str(&state.parts).unwrap_or_default();
            Ok((state.last_seq, state.base_up_to, parts))
        }
    }
}

/// Persist the cursor after a pass.
///
/// [read_parts] are the shards this pass actually read; they are marked with
/// the `to` revision of their manifest entry (or 0 when unlisted, which the
/// next pass re-reads). Listed parts that were not read are only marked when
/// their contents are already covered by the highest local revision — the
/// caller's `skip` set means "consumed", and skipping must not be mistaken for
/// "the remote deleted this".
pub(crate) fn save_cursor(
    db: &LibraryDatabase,
    cursor_key: &str,
    manifest: &SyncManifest,
    cursor_parts: &HashMap<String, i64>,
    read_parts: &HashSet<String>,
    highest_local_rev: i64,
) -> SyncResult<()> {
    let mut parts = cursor_parts.clone();
    for name in read_parts {
        let to = manifest
            .segments
            .iter()
            .find(|segment| &segment.file == name)
            .map(|segment| segment.to)
            .or_else(|| {
                manifest
                    .tombstones
                    .iter()
                    .find(|tomb| &tomb.file == name)
                    .map(|tomb| tomb.to)
            })
            .unwrap_or(0);
        parts.insert(name.clone(), to);
    }
    for part in manifest
        .segments
        .iter()
        .chain(manifest.tombstones.iter())
    {
        if parts.contains_key(&part.file) {
            continue;
        }
        if read_parts.is_empty() && part.to <= highest_local_rev {
            continue;
        }
        if part.to <= highest_local_rev {
            parts.insert(part.file.clone(), part.to);
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
    db.save_sync_state(&SyncState {
        remote_key: cursor_key.to_string(),
        last_seq,
        base_up_to: manifest.base.up_to,
        parts: serde_json::to_string(&parts).unwrap_or_else(|_| "{}".into()),
        updated_at: chrono::Utc::now().to_rfc3339(),
    })?;
    Ok(())
}
