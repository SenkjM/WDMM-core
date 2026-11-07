//! Reconciliation pass shared by every sharded sync entity.
//!
//! The protocol layer says *where* the index, the immutable shards and the
//! tombstone packs live; the entity adapter says *what* a row is and how to
//! encode, decode and persist it. This module owns the one thing that must be
//! identical everywhere: the decision table from `sync-correctness.md` §3.1.
//!
//! A row is dirty when `rev > synced_rev`. A dirty row is data the remote has
//! not seen yet; a clean row that the remote no longer lists is a remote
//! deletion, even after its tombstone has been garbage collected. That rule is
//! what keeps a full sync from resurrecting deleted media.

use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use tracing::info;
use wdmm_core::rev_clock::RevClock;

use crate::library_shard::TombRecord;
use crate::protocol::SyncManifest;
use crate::{SyncAction, SyncOutcome, SyncResult};

/// One sharded, tombstoned collection (music library, manga library, ...).
#[async_trait]
pub(crate) trait SyncEntity: Send + Sync {
    /// Row payload, opaque to this module.
    type Row: Clone + Send + Sync;

    /// Rows per base shard when a full sync rebuilds the base.
    fn shard_size(&self) -> usize;
    /// Prefix for the human-readable log line.
    fn log_label(&self) -> &'static str;
    /// Entity id of the summary outcome a full sync emits.
    fn rebuild_outcome_id(&self) -> &'static str;

    /// Stable identity of a row across devices.
    fn key_of(&self, row: &Self::Row) -> String;
    fn rev_of(&self, row: &Self::Row) -> i64;
    fn id_of(&self, row: &Self::Row) -> String;
    /// Identity carried by a tombstone record.
    fn tomb_key(&self, tomb: &TombRecord) -> String;
    /// Entity id reported for a pushed tombstone.
    fn tomb_outcome_id(&self, tomb: &TombRecord) -> String {
        format!("tomb:{}|{}", tomb.source_name, tomb.remote_path)
    }

    async fn read_index(&self) -> SyncResult<SyncManifest>;
    fn cursor_state(&self) -> SyncResult<(i64, i64, HashMap<String, i64>)>;
    fn store_cursor(
        &self,
        manifest: &SyncManifest,
        cursor_parts: &HashMap<String, i64>,
        read_parts: &HashSet<String>,
        highest_local_rev: i64,
    ) -> SyncResult<()>;

    fn live_rows(&self) -> SyncResult<Vec<Self::Row>>;
    fn synced_revs(&self) -> SyncResult<HashMap<String, i64>>;
    fn set_synced_revs(&self, revs: &HashMap<String, i64>) -> SyncResult<()>;
    fn observe_rev(&self, rev: i64) -> SyncResult<()>;

    /// Read every shard the manifest lists (minus `skip`), plus any unlisted
    /// `seg-*`/`del-*` shard this device can see.
    async fn read_cloud(
        &self,
        manifest: &SyncManifest,
        skip: &HashSet<String>,
    ) -> SyncResult<(Vec<Self::Row>, HashMap<String, i64>, HashSet<String>)>;

    async fn append_delta(&self, rows: &[Self::Row]) -> SyncResult<SyncManifest>;
    async fn append_tombstones(&self, tombs: &[TombRecord]) -> SyncResult<SyncManifest>;

    fn adopt(&self, row: &Self::Row) -> SyncResult<()>;
    fn delete(&self, id: &str) -> SyncResult<()>;

    fn pending_tombs(&self) -> SyncResult<Vec<TombRecord>>;
    fn mark_tombs_pushed(&self, revs: &[i64]) -> SyncResult<()>;
    fn purge_tombs_through(&self, up_to: i64) -> SyncResult<()>;

    /// Rebuild the base shards from [rows]; the entity keeps its tombstones.
    async fn write_base(&self, rows: &[Self::Row]) -> SyncResult<SyncManifest>;

    /// Hook after the incremental cursor was stored (manga syncs progress).
    async fn after_incremental(&self) -> SyncResult<()> {
        Ok(())
    }

    /// Hook after a full rebuild stored its cursor (manga clears it again,
    /// because the shard names the cursor pointed at are gone).
    async fn after_rebuild(&self) -> SyncResult<()> {
        Ok(())
    }
}

fn row_map<E: SyncEntity>(entity: &E, rows: &[E::Row]) -> HashMap<String, E::Row> {
    rows.iter()
        .map(|row| (entity.key_of(row), row.clone()))
        .collect()
}

fn all_keys<E: SyncEntity>(
    local: &HashMap<String, E::Row>,
    remote: &HashMap<String, E::Row>,
    tombs: &HashMap<String, i64>,
) -> HashSet<String> {
    let mut keys: HashSet<String> = local.keys().cloned().collect();
    keys.extend(remote.keys().cloned());
    for tomb in tombs.keys() {
        // Tombstones for rows this device never had still carry information,
        // but there is nothing local to compare them with.
        keys.insert(tomb.clone());
    }
    keys
}

/// Single incremental pass: pull what is newer, push what is dirty, honour the
/// tombstones, then advance the cursor and the HLC high-water mark.
pub(crate) async fn run_incremental<E: SyncEntity>(
    entity: &E,
) -> SyncResult<Vec<SyncOutcome>> {
    let mut outcomes = Vec::new();
    let manifest = entity.read_index().await?;
    let (last_seq, base_up_to, cursor_parts) = entity.cursor_state()?;

    let skip: HashSet<String> = if manifest.base.up_to > base_up_to {
        HashSet::new()
    } else {
        cursor_parts.keys().cloned().collect()
    };
    let (cloud_rows, cloud_tombs, read_parts) = entity.read_cloud(&manifest, &skip).await?;
    let remote_by_key = row_map(entity, &cloud_rows);

    // Parts this device already consumed are not re-read, so the remote view can
    // be incomplete. A clean row absent from that partial view is only proven
    // deleted when its revision lies above everything the skipped parts could
    // contain.
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

    let local_map = row_map(entity, &entity.live_rows()?);
    let synced_by_id = entity.synced_revs()?;

    let mut to_upload: Vec<E::Row> = Vec::new();
    let mut to_adopt: Vec<E::Row> = Vec::new();
    let mut to_delete: Vec<String> = Vec::new();
    let mut conflicts = 0usize;
    let mut conflict_ids: Vec<(String, i64)> = Vec::new();

    for key in all_keys::<E>(&local_map, &remote_by_key, &cloud_tombs) {
        let local = local_map.get(&key);
        let remote = remote_by_key.get(&key);
        let tomb = cloud_tombs.get(&key).copied();
        let Some(l) = local else {
            if let Some(r) = remote {
                to_adopt.push(r.clone());
            }
            continue;
        };
        let synced = synced_by_id
            .get(&entity.id_of(l))
            .copied()
            .unwrap_or(0);
        let dirty = entity.rev_of(l) > synced;
        match remote {
            Some(r) if entity.rev_of(r) > entity.rev_of(l) => {
                if dirty {
                    conflicts += 1;
                    conflict_ids.push((entity.id_of(l), entity.rev_of(l)));
                }
                to_adopt.push(r.clone());
            }
            Some(r) if entity.rev_of(r) < entity.rev_of(l) => {
                if dirty {
                    to_upload.push(l.clone());
                }
            }
            Some(_) => {}
            None => match tomb {
                Some(tomb_rev) if tomb_rev >= entity.rev_of(l) => {
                    if dirty {
                        conflicts += 1;
                        conflict_ids.push((entity.id_of(l), entity.rev_of(l)));
                    }
                    to_delete.push(entity.id_of(l));
                }
                Some(_) => {
                    if dirty {
                        to_upload.push(l.clone());
                    }
                }
                None if dirty => to_upload.push(l.clone()),
                None if synced > 0 => {
                    if entity.rev_of(l) > skipped_through {
                        to_delete.push(entity.id_of(l));
                    }
                }
                None => to_upload.push(l.clone()),
            },
        }
    }

    let mut confirmed: HashMap<String, i64> = HashMap::new();
    for row in &to_adopt {
        entity.adopt(row)?;
        confirmed.insert(entity.id_of(row), entity.rev_of(row));
        outcomes.push(SyncOutcome {
            entity_id: entity.id_of(row),
            action: SyncAction::Pulled,
            rev: RevClock::new(entity.rev_of(row).max(1) as u64),
        });
    }
    for id in &to_delete {
        entity.delete(id)?;
        outcomes.push(SyncOutcome {
            entity_id: id.clone(),
            action: SyncAction::Pulled,
            rev: RevClock::ZERO,
        });
    }

    let (superseded, to_publish) =
        split_tombstones(entity, &cloud_tombs, &local_map, &to_adopt, entity.pending_tombs()?);

    if !superseded.is_empty() {
        let _ = entity.mark_tombs_pushed(&superseded);
    }
    if !to_publish.is_empty() {
        entity.append_tombstones(&to_publish).await?;
        let revs: Vec<i64> = to_publish.iter().map(|t| t.rev).collect();
        entity.mark_tombs_pushed(&revs)?;
        for tomb in &to_publish {
            outcomes.push(SyncOutcome {
                entity_id: entity.tomb_outcome_id(tomb),
                action: SyncAction::Pushed,
                rev: RevClock::new(tomb.rev.max(1) as u64),
            });
        }
    }

    if !to_upload.is_empty() {
        entity.append_delta(&to_upload).await?;
        for row in &to_upload {
            // Confirmed only after the remote write succeeded.
            confirmed.insert(entity.id_of(row), entity.rev_of(row));
            outcomes.push(SyncOutcome {
                entity_id: entity.id_of(row),
                action: SyncAction::Pushed,
                rev: RevClock::new(entity.rev_of(row).max(1) as u64),
            });
        }
    }
    entity.set_synced_revs(&confirmed)?;

    let seen = to_adopt
        .iter()
        .chain(to_upload.iter())
        .map(|row| entity.rev_of(row))
        .fold(last_seq, i64::max);
    let _ = entity.observe_rev(seen);

    let latest = entity.read_index().await?;
    entity.store_cursor(&latest, &cursor_parts, &read_parts, seen)?;
    entity.after_incremental().await?;
    info!(
        "{} sync: adopted={} deleted_local={} tombs={} uploaded={} conflicts={}",
        entity.log_label(),
        to_adopt.len(),
        to_delete.len(),
        to_publish.len(),
        to_upload.len(),
        conflicts
    );
    for (id, rev) in conflict_ids {
        outcomes.push(SyncOutcome {
            entity_id: id,
            action: SyncAction::Conflict,
            rev: RevClock::new(rev.max(1) as u64),
        });
    }
    Ok(outcomes)
}

/// Partition pending tombstones into "already covered" and "publish now".
fn split_tombstones<E: SyncEntity>(
    entity: &E,
    cloud_tombs: &HashMap<String, i64>,
    local_map: &HashMap<String, E::Row>,
    to_adopt: &[E::Row],
    pending: Vec<TombRecord>,
) -> (Vec<i64>, Vec<TombRecord>) {
    let mut superseded = Vec::new();
    let mut to_publish = Vec::new();
    for tomb in pending {
        let key = entity.tomb_key(&tomb);
        if cloud_tombs.get(&key).copied().unwrap_or(0) >= tomb.rev && tomb.rev > 0 {
            superseded.push(tomb.rev);
            continue;
        }
        if let Some(live) = local_map.get(&key) {
            if entity.rev_of(live) > tomb.rev {
                superseded.push(tomb.rev);
                continue;
            }
        }
        if let Some(live) = to_adopt
            .iter()
            .find(|row| entity.key_of(row) == key)
        {
            if entity.rev_of(live) > tomb.rev {
                superseded.push(tomb.rev);
                continue;
            }
        }
        to_publish.push(tomb);
    }
    (superseded, to_publish)
}

/// Full pass: reconcile first, then rebuild the base from the merged live set.
///
/// It is *not* "the remote mirrors this device": a remote deletion is applied
/// locally and the tombstones survive the rebuild, so a device that was offline
/// while another one deleted a row cannot bring it back.
pub(crate) async fn run_full<E: SyncEntity>(entity: &E) -> SyncResult<Vec<SyncOutcome>> {
    let mut outcomes = Vec::new();
    let manifest = entity.read_index().await?;
    let (cloud_rows, cloud_tombs, read_parts) =
        entity.read_cloud(&manifest, &HashSet::new()).await?;
    let remote_by_key = row_map(entity, &cloud_rows);
    let local_map = row_map(entity, &entity.live_rows()?);
    let synced_by_id = entity.synced_revs()?;
    let pending_tombs = entity.pending_tombs()?;

    let mut merged: HashMap<String, E::Row> = local_map.clone();
    let mut to_delete: Vec<String> = Vec::new();
    let mut conflicts = 0usize;
    let mut conflict_ids: Vec<(String, i64)> = Vec::new();

    for key in all_keys::<E>(&local_map, &remote_by_key, &cloud_tombs) {
        let local = local_map.get(&key);
        let remote = remote_by_key.get(&key);
        let tomb = cloud_tombs.get(&key).copied();
        match (local, remote) {
            (Some(l), Some(r)) if entity.rev_of(r) > entity.rev_of(l) => {
                let synced = synced_by_id.get(&entity.id_of(l)).copied().unwrap_or(0);
                if entity.rev_of(l) > synced {
                    conflicts += 1;
                    conflict_ids.push((entity.id_of(l), entity.rev_of(l)));
                }
                merged.insert(key.clone(), r.clone());
            }
            (Some(_), Some(_)) => {}
            (Some(l), None) => match tomb {
                Some(tomb_rev) if tomb_rev >= entity.rev_of(l) => {
                    let synced = synced_by_id.get(&entity.id_of(l)).copied().unwrap_or(0);
                    if entity.rev_of(l) > synced {
                        conflicts += 1;
                        conflict_ids.push((entity.id_of(l), entity.rev_of(l)));
                    }
                    merged.remove(&key);
                    to_delete.push(entity.id_of(l));
                }
                Some(_) => {}
                None => {
                    let synced = synced_by_id.get(&entity.id_of(l)).copied().unwrap_or(0);
                    if entity.rev_of(l) == synced && synced > 0 {
                        merged.remove(&key);
                        to_delete.push(entity.id_of(l));
                    }
                }
            },
            (None, Some(r)) => {
                let locally_tombstoned = pending_tombs.iter().any(|tomb| {
                    entity.tomb_key(tomb) == key && tomb.rev >= entity.rev_of(r)
                });
                if !locally_tombstoned {
                    merged.insert(key.clone(), r.clone());
                }
            }
            (None, None) => {}
        }
    }

    for id in &to_delete {
        entity.delete(id)?;
        outcomes.push(SyncOutcome {
            entity_id: id.clone(),
            action: SyncAction::Pulled,
            rev: RevClock::ZERO,
        });
    }

    let mut rows: Vec<E::Row> = merged.into_values().collect();
    rows.sort_by_key(|row| entity.id_of(row));
    let written = entity.write_base(&rows).await?;
    // The rebuilt base is the authoritative copy of every kept row.
    let confirmed: HashMap<String, i64> = rows
        .iter()
        .map(|row| (entity.id_of(row), entity.rev_of(row)))
        .collect();
    entity.set_synced_revs(&confirmed)?;

    let (superseded, to_publish) =
        split_tombstones(entity, &cloud_tombs, &local_map, &rows, pending_tombs);
    if !superseded.is_empty() {
        let _ = entity.mark_tombs_pushed(&superseded);
    }
    if !to_publish.is_empty() {
        entity.append_tombstones(&to_publish).await?;
        let revs: Vec<i64> = to_publish.iter().map(|t| t.rev).collect();
        entity.mark_tombs_pushed(&revs)?;
        for tomb in &to_publish {
            outcomes.push(SyncOutcome {
                entity_id: entity.tomb_outcome_id(tomb),
                action: SyncAction::Pushed,
                rev: RevClock::new(tomb.rev.max(1) as u64),
            });
        }
    }

    entity.purge_tombs_through(written.base.up_to)?;
    let highest = rows
        .iter()
        .map(|row| entity.rev_of(row))
        .chain(to_publish.iter().map(|t| t.rev))
        .fold(written.base.up_to, i64::max);
    let _ = entity.observe_rev(highest);
    let latest = entity.read_index().await?;
    let mut tomb_parts: HashMap<String, i64> = HashMap::new();
    for part in &latest.tombstones {
        tomb_parts.insert(part.file.clone(), part.to);
    }
    entity.store_cursor(&latest, &tomb_parts, &read_parts, highest)?;
    entity.after_rebuild().await?;
    outcomes.push(SyncOutcome {
        entity_id: entity.rebuild_outcome_id().to_string(),
        action: SyncAction::Pushed,
        rev: RevClock::new(written.base.up_to.max(1) as u64),
    });
    info!(
        "{} full sync: rebuilt={} deleted_local={} tombs_published={} conflicts={}",
        entity.log_label(),
        rows.len(),
        to_delete.len(),
        to_publish.len(),
        conflicts
    );
    for (id, rev) in conflict_ids {
        outcomes.push(SyncOutcome {
            entity_id: id,
            action: SyncAction::Conflict,
            rev: RevClock::new(rev.max(1) as u64),
        });
    }
    Ok(outcomes)
}
