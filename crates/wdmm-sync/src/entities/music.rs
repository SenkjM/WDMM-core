//! Music library adapter for [`crate::engine`].
//!
//! The music engine owns a `LibraryDatabase` and a shard store; this file is the
//! only place that knows a row is a `LibraryTrack` and how it maps onto the
//! database calls.

use async_trait::async_trait;
use std::collections::{HashMap, HashSet};

use crate::engine::SyncEntity;
use crate::library_music::{LibrarySyncEngine, DEFAULT_TRACKS_PER_SHARD};
use crate::library_shard::{track_identity_key, TombRecord};
use crate::protocol::SyncManifest;
use crate::SyncResult;
use wdmm_core::models::LibraryTrack;

#[async_trait]
impl SyncEntity for LibrarySyncEngine {
    type Row = LibraryTrack;

    fn shard_size(&self) -> usize {
        DEFAULT_TRACKS_PER_SHARD
    }

    fn log_label(&self) -> &'static str {
        "library shard"
    }

    fn rebuild_outcome_id(&self) -> &'static str {
        "library-rebuild"
    }

    fn key_of(&self, row: &Self::Row) -> String {
        track_identity_key(&row.account_id, &row.remote_path)
    }

    fn rev_of(&self, row: &Self::Row) -> i64 {
        row.rev
    }

    fn id_of(&self, row: &Self::Row) -> String {
        row.id.clone()
    }

    fn tomb_key(&self, tomb: &TombRecord) -> String {
        track_identity_key(&tomb.source_name, &tomb.remote_path)
    }

    async fn read_index(&self) -> SyncResult<SyncManifest> {
        self.read_manifest().await
    }


    fn cursor_state(&self) -> SyncResult<(i64, i64, HashMap<String, i64>)> {
        self.load_cursor()
    }

    fn store_cursor(
        &self,
        manifest: &SyncManifest,
        cursor_parts: &HashMap<String, i64>,
        read_parts: &HashSet<String>,
        highest_local_rev: i64,
    ) -> SyncResult<()> {
        self.save_cursor(manifest, cursor_parts, read_parts, highest_local_rev)
    }

    fn live_rows(&self) -> SyncResult<Vec<Self::Row>> {
        Ok(self.db.list_tracks()?)
    }

    fn synced_revs(&self) -> SyncResult<HashMap<String, i64>> {
        Ok(self.db.list_track_synced_revs()?)
    }

    fn set_synced_revs(&self, revs: &HashMap<String, i64>) -> SyncResult<()> {
        Ok(self.db.set_track_synced_revs(revs)?)
    }

    fn observe_rev(&self, rev: i64) -> SyncResult<()> {
        Ok(self.db.observe_rev(rev)?)
    }

    async fn read_cloud(
        &self,
        manifest: &SyncManifest,
        skip: &HashSet<String>,
    ) -> SyncResult<(Vec<Self::Row>, HashMap<String, i64>, HashSet<String>)> {
        let (cloud, read_parts) = LibrarySyncEngine::read_cloud(self, manifest, skip).await?;
        Ok((cloud.tracks, cloud.tombs, read_parts))
    }

    async fn append_delta(&self, rows: &[Self::Row]) -> SyncResult<SyncManifest> {
        LibrarySyncEngine::append_delta(self, rows).await
    }

    async fn append_tombstones(&self, tombs: &[TombRecord]) -> SyncResult<SyncManifest> {
        LibrarySyncEngine::append_tombstones(self, tombs).await
    }

    fn adopt(&self, row: &Self::Row) -> SyncResult<()> {
        Ok(self.db.upsert_track(row)?)
    }

    fn delete(&self, id: &str) -> SyncResult<()> {
        Ok(self.db.delete_track(id)?)
    }

    fn pending_tombs(&self) -> SyncResult<Vec<TombRecord>> {
        let rows = self.db.list_unpushed_deleted_tracks()?;
        Ok(rows
            .into_iter()
            .map(|row| TombRecord {
                source_name: row.source_name,
                remote_path: row.remote_path,
                rev: row.rev,
                deleted_at: row.deleted_at,
                sync_key: None, // music never writes syncKey
            })
            .collect())
    }

    fn mark_tombs_pushed(&self, revs: &[i64]) -> SyncResult<()> {
        Ok(self.db.mark_deleted_tracks_pushed(revs)?)
    }

    fn purge_tombs_through(&self, up_to: i64) -> SyncResult<()> {
        let _ = self.db.purge_deleted_tracks_up_to(up_to)?;
        Ok(())
    }

    async fn write_base(&self, rows: &[Self::Row]) -> SyncResult<SyncManifest> {
        self.rebuild(rows, self.shard_size()).await
    }
}
