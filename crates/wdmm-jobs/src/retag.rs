//! `RetagCloud`: re-read remote tags and overwrite the library row, clearing any
//! "remote missing" marks it revives.
//!
//! Ported from `TaskQueueService._runRetagCloud` (Dart): resolve the remote
//! source, extract tags, ingest the row, then clear the missing flag on the
//! library row and every playlist entry pointing at it.
//!
//! Tag extraction + row writing is the shared ingest path
//! ([`crate::ingest::ingest_retag`]); this module owns the progress/units, the
//! missing-flag bookkeeping and the failure classification.

use std::sync::Arc;

use async_trait::async_trait;

use wdmm_queue::model::TaskKind;
use wdmm_queue::policy::FailureCategory;

use crate::cache_paths::normalize_source_name;
use crate::ingest::{ingest_retag, IngestOutcome};
use crate::match_remote::{DbRemoteMissingStore, RemoteMissingStore};
use crate::{Job, JobCtx, JobOutcome};

pub struct RetagCloudJob;

#[async_trait]
impl Job for RetagCloudJob {
    fn kind(&self) -> TaskKind {
        TaskKind::RetagCloud
    }

    async fn run(&self, ctx: JobCtx) -> JobOutcome {
        let store = DbRemoteMissingStore::new(Arc::clone(&ctx.db));
        run_retag(&ctx, &store).await
    }
}

/// Full job body (test seam: the missing-flag store is injectable).
pub async fn run_retag(ctx: &JobCtx, store: &dyn RemoteMissingStore) -> JobOutcome {
    if ctx.cancelled() {
        return JobOutcome::Cancelled;
    }

    let source_name = normalize_source_name(&ctx.task.source_name);
    if source_name.is_empty() {
        return failed(
            format!("err.sourceUnbound|{}", ctx.task.source_name),
            FailureCategory::State,
        );
    }

    // Dart sets `unitsTotal = 1` before touching the network, so a retry bar
    // never jumps from 0 to full.
    ctx.progress.units(0, Some(1));

    let result = ingest_retag(ctx).await;
    if ctx.cancelled() {
        return JobOutcome::Cancelled;
    }
    finish_retag(ctx, store, &source_name, result)
}

/// Apply a finished ingest attempt: success revives the row (and its playlist
/// references); a confirmed remote miss flags it.
///
/// Dart's retag worker only clears on success and leaves the flag untouched on
/// failure; the explicit `err.cloud.notFound` case follows the same rule the
/// `MatchRemote` pass applies — a confirmed absence is a missing row.
pub fn finish_retag(
    ctx: &JobCtx,
    store: &dyn RemoteMissingStore,
    source_name: &str,
    result: Result<IngestOutcome, String>,
) -> JobOutcome {
    let remote_path = ctx.task.remote_path.trim();
    match result {
        Ok(_outcome) => {
            if let Err(error) = store.set_missing(source_name, remote_path, false) {
                ctx.progress
                    .note(&format!("retag clear missing: {error}"));
            }
            if let Err(error) = store.set_playlist_missing(source_name, remote_path, None, false) {
                ctx.progress
                    .note(&format!("retag clear playlist missing: {error}"));
            }
            ctx.progress.units(1, Some(1));
            JobOutcome::Completed
        }
        Err(error) => {
            if is_not_found(&error) {
                if let Err(mark) = store.set_missing(source_name, remote_path, true) {
                    ctx.progress
                        .note(&format!("retag mark missing: {mark}"));
                }
            }
            failed(error.clone(), category_for(&error))
        }
    }
}

/// `err.cloud.notFound` is the wire error both the VFS probe and the Dart
/// `resolveStreamSource` path produce for an absent object.
fn is_not_found(error: &str) -> bool {
    error.to_ascii_lowercase().contains("notfound")
}

fn category_for(error: &str) -> FailureCategory {
    if is_not_found(error) {
        FailureCategory::State
    } else if error.contains("err.cloud.") || error.contains("err.vfs") {
        FailureCategory::VfsData
    } else {
        FailureCategory::Other
    }
}

fn failed(error: String, category: FailureCategory) -> JobOutcome {
    JobOutcome::Failed { error, category }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Mutex;

    use super::*;
    use crate::match_remote::DbRemoteMissingStore;
    use crate::{JobPaths, ProgressSink};
    use wdmm_drivers::{CloudFileItem, DriverError, Result as VfsResult, WdmmVfs};
    use wdmm_queue::model::{CancelToken, QueueTask, TaskTarget};
    use wdmm_storage::{CueSlice, LibraryDatabase};

    struct NullVfs;

    #[async_trait]
    impl WdmmVfs for NullVfs {
        async fn list(&self, _path: &str) -> VfsResult<Vec<CloudFileItem>> {
            Ok(Vec::new())
        }

        async fn get(&self, path: &str) -> VfsResult<CloudFileItem> {
            Err(DriverError::NotFound(path.to_string()))
        }

        async fn mkdir(&self, _path: &str) -> VfsResult<()> {
            Ok(())
        }

        async fn rename(&self, _old_path: &str, _new_path: &str) -> VfsResult<()> {
            Ok(())
        }

        async fn remove(&self, _path: &str) -> VfsResult<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingStore {
        missing: Mutex<Vec<(String, String, bool)>>,
        playlist: Mutex<Vec<(String, String, bool)>>,
    }

    impl RemoteMissingStore for RecordingStore {
        fn set_missing(
            &self,
            source_name: &str,
            remote_path: &str,
            missing: bool,
        ) -> Result<(), String> {
            self.missing.lock().unwrap().push((
                source_name.to_string(),
                remote_path.to_string(),
                missing,
            ));
            Ok(())
        }

        fn set_playlist_missing(
            &self,
            source_name: &str,
            remote_path: &str,
            _music_id: Option<&str>,
            missing: bool,
        ) -> Result<(), String> {
            self.playlist.lock().unwrap().push((
                source_name.to_string(),
                remote_path.to_string(),
                missing,
            ));
            Ok(())
        }
    }

    #[derive(Default)]
    struct Recorder {
        notes: Mutex<Vec<String>>,
    }

    impl ProgressSink for Recorder {
        fn bytes(&self, _done: u64, _total: Option<u64>) {}
        fn units(&self, _done: u32, _total: Option<u32>) {}
        fn note(&self, message: &str) {
            self.notes.lock().unwrap().push(message.to_string());
        }
    }

    fn memory_db() -> Arc<LibraryDatabase> {
        Arc::new(LibraryDatabase::open(Path::new(":memory:")).unwrap())
    }

    fn ctx_for(dir: &Path, source: &str, remote: &str, db: &Arc<LibraryDatabase>) -> JobCtx {
        ctx_with_progress(dir, source, remote, db, Arc::new(Recorder::default()))
    }

    fn ctx_with_progress(
        dir: &Path,
        source: &str,
        remote: &str,
        db: &Arc<LibraryDatabase>,
        progress: Arc<Recorder>,
    ) -> JobCtx {
        let mut task = QueueTask::new(
            "t-retag".to_string(),
            TaskKind::RetagCloud,
            source.to_string(),
            "acc".to_string(),
            remote.to_string(),
            remote.rsplit('/').next().unwrap_or_default().to_string(),
            TaskTarget::Cache,
            "batch".to_string(),
            0,
        );
        task.local_path = None;
        JobCtx {
            task,
            cancel: CancelToken::new(),
            progress,
            platform: Arc::new(crate::PlatformClient::disabled()),
            db: Arc::clone(db),
            vfs: Arc::new(NullVfs),
            paths: JobPaths::new(dir.join("docs"), dir.join("cache")),
        }
    }

    fn cue_slice(missing: i64) -> CueSlice {
        CueSlice {
            music_id: "cue-1".to_string(),
            cue_id: "cue-album".to_string(),
            audio_music_id: "audio-1".to_string(),
            source_name: "nas".to_string(),
            remote_path: "/m/album.virt".to_string(),
            file_name: "album.flac".to_string(),
            audio_remote_path: Some("/m/album.flac".to_string()),
            remote_missing: missing,
            rev: 7,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn success_clears_missing_and_units() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        let store = RecordingStore::default();
        let ctx = ctx_for(dir.path(), "nas", "/m/a.mp3", &db);

        let outcome = finish_retag(&ctx, &store, "nas", Ok(IngestOutcome::default()));
        assert!(matches!(outcome, JobOutcome::Completed), "{outcome:?}");
        assert_eq!(
            store.missing.lock().unwrap().as_slice(),
            &[("nas".to_string(), "/m/a.mp3".to_string(), false)]
        );
        assert_eq!(
            store.playlist.lock().unwrap().as_slice(),
            &[("nas".to_string(), "/m/a.mp3".to_string(), false)]
        );
    }

    #[tokio::test]
    async fn not_found_failure_marks_missing() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        let store = RecordingStore::default();
        let ctx = ctx_for(dir.path(), "nas", "/m/gone.mp3", &db);

        let outcome = finish_retag(
            &ctx,
            &store,
            "nas",
            Err("err.cloud.notFound|/m/gone.mp3".to_string()),
        );
        match outcome {
            JobOutcome::Failed { error, category } => {
                assert_eq!(error, "err.cloud.notFound|/m/gone.mp3");
                assert!(matches!(category, FailureCategory::State));
            }
            other => panic!("expected failure, got {other:?}"),
        }
        assert_eq!(
            store.missing.lock().unwrap().as_slice(),
            &[("nas".to_string(), "/m/gone.mp3".to_string(), true)]
        );
    }

    #[tokio::test]
    async fn other_failures_keep_the_flag_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        let store = RecordingStore::default();
        let ctx = ctx_for(dir.path(), "nas", "/m/a.mp3", &db);

        let outcome = finish_retag(&ctx, &store, "nas", Err("err.unknown|no tags".to_string()));
        match outcome {
            JobOutcome::Failed { category, .. } => {
                assert!(matches!(category, FailureCategory::Other))
            }
            other => panic!("expected failure, got {other:?}"),
        }
        assert!(store.missing.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unbound_source_fails_before_ingest() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        let store = RecordingStore::default();
        let ctx = ctx_for(dir.path(), "  ", "/m/a.mp3", &db);

        match run_retag(&ctx, &store).await {
            JobOutcome::Failed { error, .. } => assert!(error.starts_with("err.sourceUnbound|")),
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn db_store_clears_a_cue_slice_flag() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        // `cue_slices.cue_id` is a foreign key into `cue_albums`.
        db.upsert_cue_album(&wdmm_storage::CueAlbum {
            cue_id: "cue-album".to_string(),
            source_name: "nas".to_string(),
            cue_remote_path: "/m/album.cue".to_string(),
            title: None,
            performer: None,
            cache_group_id: None,
            created_at: "2025-01-01T00:00:00Z".to_string(),
        })
        .unwrap();
        db.upsert_cue_slice(&cue_slice(1)).unwrap();
        let store = DbRemoteMissingStore::new(Arc::clone(&db));
        let ctx = ctx_for(dir.path(), "nas", "/m/album.virt", &db);

        let outcome = finish_retag(&ctx, &store, "nas", Ok(IngestOutcome::default()));
        assert!(matches!(outcome, JobOutcome::Completed), "{outcome:?}");

        let slice = db
            .get_cue_slice_by_source_path("nas", "/m/album.virt")
            .unwrap()
            .unwrap();
        assert_eq!(slice.remote_missing, 0);
        assert_eq!(slice.rev, 8, "flag change bumps rev exactly once");
    }

    #[tokio::test]
    async fn db_store_reports_unpersisted_flag_for_tracks() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        let store = DbRemoteMissingStore::new(Arc::clone(&db));
        let progress = Arc::new(Recorder::default());
        let ctx = ctx_with_progress(dir.path(), "nas", "/m/a.mp3", &db, Arc::clone(&progress));

        assert!(matches!(
            finish_retag(&ctx, &store, "nas", Ok(IngestOutcome::default())),
            JobOutcome::Completed
        ));
        // The executor does not fail when the flag cannot be written; the note
        // is the observable signal (storage gap, see the task report).
        let notes = progress.notes.lock().unwrap().clone();
        assert!(
            notes.iter().any(|n| n.contains("track.remote_missing")),
            "{notes:?}"
        );
    }
}
