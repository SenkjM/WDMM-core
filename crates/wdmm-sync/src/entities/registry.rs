//! The sharded entities a library sync walks.
//!
//! Credentials and playlists are single-document engines with their own files;
//! everything that lives in the shard+tombstone protocol is listed here, so
//! both aggregate entry points ([`crate::run_sync_all`], [`crate::run_sync_full`])
//! walk the same set in the same order and report it under the same JSON keys.

use std::sync::Arc;

use wdmm_drivers::WdmmVfs;
use wdmm_storage::LibraryDatabase;

use crate::cover::CoverRoots;
use crate::library_manga::MangaLibrarySyncEngine;
use crate::library_music::LibrarySyncEngine;
use crate::SyncAction;
use crate::{SyncMode, SyncEngine, SyncOutcome, SyncResult};

/// One sharded entity's outcomes, under the key it occupies in the summary.
pub(crate) struct ShardedSummary {
    pub key: &'static str,
    pub outcomes: Vec<SyncOutcome>,
}

/// Run the sharded entities in their canonical order: music library, manga.
pub(crate) async fn run_sharded_entities(
    db: Arc<LibraryDatabase>,
    driver: Arc<dyn WdmmVfs>,
    remote_dir: &str,
    cover_roots: Option<CoverRoots>,
    mode: SyncMode,
) -> SyncResult<Vec<ShardedSummary>> {
    let music = LibrarySyncEngine::new(
        Arc::clone(&db),
        Arc::clone(&driver),
        remote_dir,
        cover_roots.clone(),
    );
    let manga = MangaLibrarySyncEngine::new(Arc::clone(&db), Arc::clone(&driver), remote_dir, cover_roots);

    let music_outcomes = match mode {
        SyncMode::Incremental => music.sync_incremental().await?,
        SyncMode::Full => music.sync_full().await?,
    };
    let manga_outcomes = match mode {
        SyncMode::Incremental => manga.sync_incremental().await?,
        SyncMode::Full => manga.sync_full().await?,
    };

    Ok(vec![
        ShardedSummary {
            key: "library",
            outcomes: music_outcomes,
        },
        ShardedSummary {
            key: "manga",
            outcomes: manga_outcomes,
        },
    ])
}

/// Count pushed/pulled/conflict outcomes for the aggregate summary.
pub(crate) fn summarize(outcomes: &[SyncOutcome]) -> serde_json::Value {
    let (pushed, pulled, conflicts) =
        outcomes
            .iter()
            .fold((0, 0, 0), |(p, l, c), outcome| match outcome.action {
                SyncAction::Pushed => (p + 1, l, c),
                SyncAction::Pulled => (p, l + 1, c),
                SyncAction::Conflict => (p, l, c + 1),
                SyncAction::NoChange => (p, l, c),
            });
    serde_json::json!({
        "total": outcomes.len(),
        "pushed": pushed,
        "pulled": pulled,
        "conflicts": conflicts,
    })
}
