//! Manga library adapter for [`crate::engine`].
//!
//! The manga engine owns a `LibraryDatabase` and a shard store; this file is the
//! only place that knows a row is a `MangaSyncRow` (a manga plus its page list)
//! and how it maps onto the database calls.

use async_trait::async_trait;
use std::collections::{HashMap, HashSet};

use crate::engine::SyncEntity;
use crate::library_manga::{MangaLibrarySyncEngine, DEFAULT_MANGAS_PER_SHARD};
use crate::library_manga_shard::MangaSyncRow;
use crate::library_shard::TombRecord;
use crate::protocol::SyncManifest;
use crate::SyncResult;
use wdmm_storage::{manga_sync_key, normalize_manga_remote_path};

/// Identity of a tombstone, mirroring `library_manga::tomb_key`.
///
/// Tombstones written by this device carry the manga sync key; older ones fall
/// back to the container rule (source + normalized path) that the manga engine
/// used before `syncKey` existed.
fn tomb_identity(tomb: &TombRecord) -> String {
    tomb.sync_key.clone().unwrap_or_else(|| {
        format!(
            "{}\u{0}{}",
            tomb.source_name,
            normalize_manga_remote_path(&tomb.remote_path)
        )
    })
}

#[async_trait]
impl SyncEntity for MangaLibrarySyncEngine {
    type Row = MangaSyncRow;

    fn shard_size(&self) -> usize {
        DEFAULT_MANGAS_PER_SHARD
    }

    fn log_label(&self) -> &'static str {
        "manga shard"
    }

    fn rebuild_outcome_id(&self) -> &'static str {
        "manga-rebuild"
    }

    fn key_of(&self, row: &Self::Row) -> String {
        manga_sync_key(
            &row.manga.id,
            &row.manga.format,
            &row.manga.source_name,
            &row.manga.remote_path,
        )
    }

    fn rev_of(&self, row: &Self::Row) -> i64 {
        row.manga.rev
    }

    fn id_of(&self, row: &Self::Row) -> String {
        row.manga.id.clone()
    }

    fn tomb_key(&self, tomb: &TombRecord) -> String {
        tomb_identity(tomb)
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
        let mut rows = Vec::new();
        for manga in self.db.list_mangas()? {
            rows.push(MangaLibrarySyncEngine::row_with_pages(&self.db, &manga)?);
        }
        Ok(rows)
    }

    fn synced_revs(&self) -> SyncResult<HashMap<String, i64>> {
        Ok(self.db.list_manga_synced_revs()?)
    }

    fn set_synced_revs(&self, revs: &HashMap<String, i64>) -> SyncResult<()> {
        Ok(self.db.set_manga_synced_revs(revs)?)
    }

    fn observe_rev(&self, rev: i64) -> SyncResult<()> {
        Ok(self.db.observe_rev(rev)?)
    }

    async fn read_cloud(
        &self,
        manifest: &SyncManifest,
        skip: &HashSet<String>,
    ) -> SyncResult<(Vec<Self::Row>, HashMap<String, i64>, HashSet<String>)> {
        let (cloud, read_parts) = MangaLibrarySyncEngine::read_cloud(self, manifest, skip).await?;
        Ok((cloud.rows, cloud.tombs, read_parts))
    }

    async fn append_delta(&self, rows: &[Self::Row]) -> SyncResult<SyncManifest> {
        MangaLibrarySyncEngine::append_delta(self, rows).await
    }

    async fn append_tombstones(&self, tombs: &[TombRecord]) -> SyncResult<SyncManifest> {
        MangaLibrarySyncEngine::append_tombstones(self, tombs).await
    }

    fn adopt(&self, row: &Self::Row) -> SyncResult<()> {
        // Legacy loose-image rows (`images` format with a path-derived id) are
        // re-scanned instead of adopted; `adopt_row` reports that with `false`.
        let _ = MangaLibrarySyncEngine::adopt_row(&self.db, row)?;
        Ok(())
    }

    fn delete(&self, id: &str) -> SyncResult<()> {
        Ok(self.db.delete_manga(id)?)
    }

    fn pending_tombs(&self) -> SyncResult<Vec<TombRecord>> {
        let rows = self.db.list_unpushed_deleted_mangas()?;
        Ok(rows
            .into_iter()
            .map(|row| TombRecord {
                source_name: row.source_name,
                remote_path: row.remote_path,
                rev: row.rev.unwrap_or(0),
                deleted_at: row.deleted_at,
                // Manga tombstones always carry their sync key: the manga key
                // includes the format, which the container rule cannot rebuild.
                sync_key: Some(row.sync_key),
            })
            .collect())
    }

    fn mark_tombs_pushed(&self, revs: &[i64]) -> SyncResult<()> {
        Ok(self.db.mark_deleted_mangas_pushed(revs)?)
    }

    fn purge_tombs_through(&self, _up_to: i64) -> SyncResult<()> {
        // `deleted_mangas` has no purge-by-rev query (and never had one); the
        // tombstone shards are the remote garbage collection, not this table.
        Ok(())
    }

    async fn write_base(&self, rows: &[Self::Row]) -> SyncResult<SyncManifest> {
        self.rebuild(rows, self.shard_size()).await
    }

    async fn after_incremental(&self) -> SyncResult<()> {
        self.sync_progress().await
    }

    async fn after_rebuild(&self) -> SyncResult<()> {
        // A full rebuild rewrote the base shards, so the saved cursor (which
        // points at shard names that no longer exist) must not survive.
        self.reset_cursor()?;
        self.sync_progress().await
    }
}
