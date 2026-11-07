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

use serde::Deserialize;
use wdmm_queue::model::{QueueTask, TaskKind};
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

/// One entry of the batch payload written by the settings action:
/// `[{"path","name","source"}]`. `source` is the account display name the row
/// belongs to; it falls back to the task's own source when omitted.
#[derive(Debug, Clone, Deserialize)]
struct RetagEntry {
    path: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    source: String,
}

/// Full job body (test seam: the missing-flag store is injectable).
///
/// A task is either a single retag (`remote_path`/`file_name`) or a batch whose
/// payload sidecar lists the rows to re-read; both run through the same shared
/// ingest path, and a batch keeps going after a failed item like `scan` does.
pub async fn run_retag(ctx: &JobCtx, store: &dyn RemoteMissingStore) -> JobOutcome {
    if ctx.cancelled() {
        return JobOutcome::Cancelled;
    }

    let entries = match load_entries(ctx) {
        Ok(entries) => entries,
        Err(error) => return failed(error, FailureCategory::Other),
    };
    let total = u32::try_from(entries.len()).unwrap_or(u32::MAX);
    ctx.progress.units(0, Some(total));

    let mut failed_items: Vec<serde_json::Value> = Vec::new();
    let mut first_error: Option<(String, FailureCategory)> = None;
    let mut done = 0_u32;

    for entry in &entries {
        if ctx.cancelled() {
            return JobOutcome::Cancelled;
        }
        let item = item_ctx(ctx, entry);
        let source_name = normalize_source_name(&item.task.source_name);
        if source_name.is_empty() {
            let error = format!("err.sourceUnbound|{}", item.task.source_name);
            if first_error.is_none() {
                first_error = Some((error.clone(), FailureCategory::State));
            }
            failed_items.push(serde_json::json!({ "path": entry.path, "error": error }));
        } else {
            let result = ingest_retag(&item).await;
            if ctx.cancelled() {
                return JobOutcome::Cancelled;
            }
            match finish_retag(&item, store, &source_name, result) {
                JobOutcome::Completed => {}
                JobOutcome::Cancelled => return JobOutcome::Cancelled,
                JobOutcome::Failed { error, category } => {
                    ctx.progress
                        .note(&format!("retag failed: {}: {error}", entry.path));
                    if first_error.is_none() {
                        first_error = Some((error.clone(), category));
                    }
                    failed_items.push(serde_json::json!({ "path": entry.path, "error": error }));
                }
                JobOutcome::Partial { .. } => {
                    // One item cannot produce a partial outcome; treat it as a
                    // failure so the batch accounting stays honest.
                    let error = format!("err.retag.partial|{}", entry.path);
                    if first_error.is_none() {
                        first_error = Some((error.clone(), FailureCategory::Other));
                    }
                    failed_items.push(serde_json::json!({ "path": entry.path, "error": error }));
                }
            }
        }
        done = done.saturating_add(1);
        ctx.progress.units(done, Some(total));
    }

    let failed_count = u32::try_from(failed_items.len()).unwrap_or(u32::MAX);
    let ok = total.saturating_sub(failed_count);
    if failed_items.is_empty() {
        JobOutcome::Completed
    } else if ok == 0 {
        let (error, category) = first_error
            .unwrap_or_else(|| ("err.retag.failed".to_string(), FailureCategory::Other));
        JobOutcome::Failed { error, category }
    } else {
        JobOutcome::Partial {
            result: serde_json::json!({ "ok": ok, "failed": failed_items }),
        }
    }
}

fn load_entries(ctx: &JobCtx) -> Result<Vec<RetagEntry>, String> {
    if let Some(raw) = ctx
        .task
        .payload_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
    {
        let path = payload_file(ctx, raw);
        let bytes = std::fs::read(&path)
            .map_err(|error| format!("err.retag.payload|{}: {error}", path.display()))?;
        let entries: Vec<RetagEntry> = serde_json::from_slice(&bytes)
            .map_err(|error| format!("err.retag.payloadJson|{}: {error}", path.display()))?;
        let entries: Vec<RetagEntry> = entries
            .into_iter()
            .filter(|entry| !entry.path.trim().is_empty())
            .collect();
        if !entries.is_empty() {
            return Ok(entries);
        }
    }
    Ok(vec![RetagEntry {
        path: ctx.task.remote_path.clone(),
        name: ctx.task.file_name.clone(),
        source: String::new(),
    }])
}

/// `payload_path` is relative to `<app docs>/queue_payloads` unless it is an
/// absolute path that still exists (same rule as `scan.rs`).
fn payload_file(ctx: &JobCtx, raw: &str) -> std::path::PathBuf {
    let candidate = std::path::PathBuf::from(raw);
    if candidate.is_absolute() {
        return candidate;
    }
    let joined = ctx.paths.payload_dir.join(raw);
    if joined.exists() {
        joined
    } else if candidate.exists() {
        candidate
    } else {
        joined
    }
}

/// A per-item view of the same run: only the identity fields differ, so the
/// shared ingest path writes the row under the item's own account.
fn item_ctx(ctx: &JobCtx, entry: &RetagEntry) -> JobCtx {
    let source_name = if entry.source.trim().is_empty() {
        ctx.task.source_name.clone()
    } else {
        entry.source.clone()
    };
    JobCtx {
        task: QueueTask {
            source_name,
            remote_path: entry.path.clone(),
            file_name: entry.name.clone(),
            ..ctx.task.clone()
        },
        cancel: ctx.cancel.clone(),
        progress: Arc::clone(&ctx.progress),
        platform: Arc::clone(&ctx.platform),
        db: Arc::clone(&ctx.db),
        vfs: Arc::clone(&ctx.vfs),
        paths: ctx.paths.clone(),
    }
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
