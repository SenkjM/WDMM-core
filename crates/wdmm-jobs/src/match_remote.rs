//! `MatchRemote`: compare library rows with what the remote account actually has
//! and set/clear the remote-missing marks.
//!
//! Ported from `TaskQueueService._runMatchRemote` / `_remoteExists` (Dart). The
//! executor *probes* known rows; it never walks a tree. Each probe targets the
//! audio file behind a row — for CUE slices that is `audio_remote_path`, else the
//! row's own `remote_path`.
//!
//! The iOS/Android library keeps the remote-missing flag on the track row; the
//! Rust `wdmm-storage` schema has the column only on `cue_slices` (tracks and
//! playlist entries have no such column yet). The flag writes therefore go
//! through [`RemoteMissingStore`]: the DB implementation can persist CUE slices
//! and reports the remaining gaps per call so a note is logged instead of
//! failing the job. Playlist references are UIs-over-library in Rust today, so
//! the DB implementation reports that half as unimplemented as well.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::task::JoinSet;

use wdmm_core::models::LibraryTrack;
use wdmm_drivers::{DriverError, WdmmVfs};
use wdmm_queue::model::{CancelToken, QueueTask, TaskKind};
use wdmm_queue::policy::FailureCategory;
use wdmm_storage::LibraryDatabase;

use crate::cache_paths::{normalize_source_name, CachePaths};
use crate::{Job, JobCtx, JobOutcome};

/// Maximum in-flight `WdmmVfs::get` probes. The Dart queue probed serially; four
/// keeps a large library from taking minutes without stampeding a WebDAV host.
pub const MAX_PROBE_CONCURRENCY: usize = 4;

/// Persisted "this row is gone remotely" flag.
///
/// Implemented over `LibraryDatabase` for production and with a recorder in
/// tests. Every method reports storage gaps as `err.storageUnimplemented|…` so
/// the executor can log a note and keep going.
pub trait RemoteMissingStore: Send + Sync {
    /// Dart `LibraryService.markRemoteMissing` / `clearRemoteMissing`.
    fn set_missing(
        &self,
        source_name: &str,
        remote_path: &str,
        missing: bool,
    ) -> Result<(), String>;

    /// Dart `PlaylistService.setRemoteMissingForPath` /
    /// `setRemoteMissingForIdentities`.
    fn set_playlist_missing(
        &self,
        source_name: &str,
        remote_path: &str,
        music_id: Option<&str>,
        missing: bool,
    ) -> Result<(), String>;
}

/// Production store: CUE slices are writable, everything else is a reported gap.
#[derive(Clone)]
pub struct DbRemoteMissingStore {
    db: Arc<LibraryDatabase>,
}

impl DbRemoteMissingStore {
    pub fn new(db: Arc<LibraryDatabase>) -> Self {
        Self { db }
    }
}

impl RemoteMissingStore for DbRemoteMissingStore {
    fn set_missing(
        &self,
        source_name: &str,
        remote_path: &str,
        missing: bool,
    ) -> Result<(), String> {
        let slice = self
            .db
            .get_cue_slice_by_source_path(source_name, remote_path)
            .map_err(|error| format!("err.storage|{error}"))?;
        let Some(mut slice) = slice else {
            return Err("err.storageUnimplemented|track.remote_missing".to_string());
        };
        let desired = i64::from(missing);
        if slice.remote_missing == desired {
            return Ok(());
        }
        slice.remote_missing = desired;
        slice.rev += 1;
        self.db
            .upsert_cue_slice(&slice)
            .map_err(|error| format!("err.storage|{error}"))
    }

    fn set_playlist_missing(
        &self,
        _source_name: &str,
        _remote_path: &str,
        _music_id: Option<&str>,
        _missing: bool,
    ) -> Result<(), String> {
        Err("err.storageUnimplemented|playlist.remote_missing".to_string())
    }
}

/// One library row reduced to what a probe needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingTarget {
    /// Identity the playlist layer keys on (`music_id`).
    pub music_id: String,
    pub source_name: String,
    /// Library row path (what the flag is stored against).
    pub remote_path: String,
    /// Path actually probed — `audio_remote_path` for a CUE slice.
    pub probe_path: String,
    pub is_cue: bool,
}

/// Outcome of one `WdmmVfs::get` probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    Present,
    Missing,
    Error(String),
}

/// `MatchRemote`: stat every library track whose bytes are neither cached nor
/// already known to be gone, and keep the missing flags (and playlist
/// references) in step with the remote tree.
pub struct MatchRemoteJob;

#[async_trait]
impl Job for MatchRemoteJob {
    fn kind(&self) -> TaskKind {
        TaskKind::MatchRemote
    }

    async fn run(&self, ctx: JobCtx) -> JobOutcome {
        let store = DbRemoteMissingStore::new(Arc::clone(&ctx.db));
        run_match(&ctx, &store).await
    }
}

/// Full job body (also the test seam: the store is injectable).
pub async fn run_match(ctx: &JobCtx, store: &dyn RemoteMissingStore) -> JobOutcome {
    if ctx.cancelled() {
        return JobOutcome::Cancelled;
    }

    let targets = match collect_targets(&ctx.db, &ctx.task, &ctx.paths.cache_root) {
        Ok(targets) => targets,
        Err(error) => return failed(error, FailureCategory::State),
    };

    // Dart reports `unitsTotal = targets.length` only after the local-cache and
    // source filters, so the progress bar matches the rows actually probed.
    let total = targets.len() as u32;
    ctx.progress.units(0, Some(total));
    if targets.is_empty() {
        return JobOutcome::Completed;
    }

    let paths: Vec<String> = targets.iter().map(|t| t.probe_path.clone()).collect();
    let Some(probes) = probe_paths(&ctx.vfs, &paths, Some(&ctx.cancel)).await else {
        return JobOutcome::Cancelled;
    };

    let mut present = 0u32;
    let mut missing = 0u32;
    let mut errors = 0u32;

    for (index, (target, probe)) in targets.iter().zip(probes).enumerate() {
        if ctx.cancelled() {
            return JobOutcome::Cancelled;
        }
        match probe {
            Probe::Present => {
                present += 1;
                apply_missing(ctx, store, target, false);
            }
            Probe::Missing => {
                missing += 1;
                apply_missing(ctx, store, target, true);
            }
            Probe::Error(error) => {
                errors += 1;
                // Dart: per-item failures are logged (and set errorMessage); only
                // an all-error run is fatal here so a single flaky path cannot
                // stop a library-wide pass.
                ctx.progress.note(&format!(
                    "match_remote probe failed: {}: {error}",
                    target.probe_path
                ));
            }
        }
        ctx.progress.units(index as u32 + 1, Some(total));
    }

    if errors == total {
        return failed(
            format!("err.cloud.unreachable|all {errors} probes failed"),
            FailureCategory::VfsData,
        );
    }

    ctx.progress.note(&format!(
        "match_remote: {present} present, {missing} missing, {errors} errored"
    ));
    JobOutcome::Completed
}

fn apply_missing(
    ctx: &JobCtx,
    store: &dyn RemoteMissingStore,
    target: &MissingTarget,
    missing: bool,
) {
    if let Err(error) = store.set_missing(&target.source_name, &target.remote_path, missing) {
        ctx.progress
            .note(&format!("match_remote missing flag: {error}"));
    }
    if let Err(error) = store.set_playlist_missing(
        &target.source_name,
        &target.remote_path,
        Some(&target.music_id),
        missing,
    ) {
        ctx.progress
            .note(&format!("match_remote playlist flag: {error}"));
    }
}

/// Library rows for `task`, minus rows that already have a local cache copy.
///
/// The Dart version filters on `Track.sourceName`; `LibraryTrack` here keys the
/// account through `account_id`, so a specific source resolves through
/// `task.account_id`. When the task names a source but carries no account id the
/// filter cannot be applied and the pass covers every account.
pub fn collect_targets(
    db: &LibraryDatabase,
    task: &QueueTask,
    cache_root: &Path,
) -> Result<Vec<MissingTarget>, String> {
    let filter = normalize_source_name(&task.source_name);
    let filter = if filter == "*" { String::new() } else { filter };
    let account = task.account_id.trim();
    let cache = CachePaths::new(cache_root);

    let mut targets: Vec<MissingTarget> = Vec::new();

    let tracks: Vec<LibraryTrack> = db.list_tracks().map_err(|e| format!("err.storage|{e}"))?;
    for track in tracks {
        if !account.is_empty() && track.account_id != account {
            continue;
        }
        let probe_path = track.remote_path.trim();
        if probe_path.is_empty() {
            continue;
        }
        let source_name = if filter.is_empty() {
            normalize_source_name(&track.account_id)
        } else {
            filter.clone()
        };
        if has_local_cache(&cache, probe_path, &source_name) {
            continue;
        }
        targets.push(MissingTarget {
            music_id: track.id,
            source_name,
            remote_path: track.remote_path.clone(),
            probe_path: probe_path.to_string(),
            is_cue: false,
        });
    }

    let slices = db
        .list_cue_slices()
        .map_err(|e| format!("err.storage|{e}"))?;
    for slice in slices {
        let source_name = normalize_source_name(&slice.source_name);
        if !filter.is_empty() && source_name != filter {
            continue;
        }
        let probe_path = slice
            .audio_remote_path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| slice.remote_path.trim().to_string());
        if probe_path.is_empty() {
            continue;
        }
        if has_local_cache(&cache, &probe_path, &source_name) {
            continue;
        }
        targets.push(MissingTarget {
            music_id: slice.music_id,
            source_name,
            remote_path: slice.remote_path,
            probe_path,
            is_cue: true,
        });
    }

    Ok(targets)
}

fn has_local_cache(cache: &CachePaths, probe_path: &str, source_name: &str) -> bool {
    let source = if source_name.is_empty() {
        None
    } else {
        Some(source_name)
    };
    cache.music_file_for(probe_path, source).exists()
}

/// Probe `paths` with at most [`MAX_PROBE_CONCURRENCY`] in-flight `get` calls.
///
/// Returns `None` when the cancel token fired between batches. The result order
/// matches `paths`.
pub async fn probe_paths(
    vfs: &Arc<dyn WdmmVfs>,
    paths: &[String],
    cancel: Option<&CancelToken>,
) -> Option<Vec<Probe>> {
    let mut out = Vec::with_capacity(paths.len());
    for chunk in paths.chunks(MAX_PROBE_CONCURRENCY) {
        if let Some(token) = cancel {
            if token.is_cancelled() {
                return None;
            }
        }
        let mut set: JoinSet<(usize, Probe)> = JoinSet::new();
        for (offset, path) in chunk.iter().enumerate() {
            let vfs = Arc::clone(vfs);
            let path = path.clone();
            set.spawn(async move { (offset, probe_one(vfs.as_ref(), &path).await) });
        }
        let mut batch = vec![Probe::Error("probe task dropped".to_string()); chunk.len()];
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok((offset, probe)) => batch[offset] = probe,
                Err(error) => {
                    // A panicking probe must not poison the batch; it shows up as
                    // an error and is counted like any other failed `get`.
                    batch.push(Probe::Error(error.to_string()));
                }
            }
        }
        out.extend(batch);
    }
    Some(out)
}

async fn probe_one(vfs: &dyn WdmmVfs, path: &str) -> Probe {
    match vfs.get(path).await {
        Ok(_) => Probe::Present,
        Err(DriverError::NotFound(_)) => Probe::Missing,
        Err(error) => Probe::Error(error.to_string()),
    }
}

fn failed(error: String, category: FailureCategory) -> JobOutcome {
    JobOutcome::Failed { error, category }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    use super::*;
    use crate::cbz::entry_basename;
    use crate::{JobPaths, ProgressSink};
    use wdmm_drivers::{CloudFileItem, ContentDeliveryKind, Result as VfsResult};
    use wdmm_queue::model::TaskTarget;

    #[derive(Default)]
    struct FakeVfs {
        files: HashMap<String, Vec<u8>>,
        fail: bool,
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
    }

    impl FakeVfs {
        fn with_files(paths: &[&str]) -> Self {
            let mut vfs = Self::default();
            for path in paths {
                vfs.files.insert((*path).to_string(), b"x".to_vec());
            }
            vfs
        }

        fn failing() -> Self {
            Self {
                fail: true,
                ..Self::default()
            }
        }
    }

    #[async_trait]
    impl WdmmVfs for FakeVfs {
        async fn list(&self, _path: &str) -> VfsResult<Vec<CloudFileItem>> {
            Ok(Vec::new())
        }

        async fn get(&self, path: &str) -> VfsResult<CloudFileItem> {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(5)).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);

            if self.fail {
                return Err(DriverError::Network("boom".to_string()));
            }
            match self.files.get(path) {
                Some(data) => Ok(CloudFileItem {
                    name: entry_basename(path).to_string(),
                    is_dir: false,
                    size: data.len() as u64,
                    modified: None,
                    raw_url: None,
                    raw_headers: Vec::new(),
                    delivery: ContentDeliveryKind::NetworkDirect,
                    local_path: None,
                }),
                None => Err(DriverError::NotFound(path.to_string())),
            }
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

        async fn get_content(&self, path: &str) -> VfsResult<Vec<u8>> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| DriverError::NotFound(path.to_string()))
        }
    }

    /// One recorded playlist flag write: `(source, path, music id, missing)`.
    type PlaylistMark = (String, String, Option<String>, bool);

    /// Records the flag writes the DB cannot yet persist.
    #[derive(Default)]
    struct RecordingStore {
        missing: Mutex<Vec<(String, String, bool)>>,
        playlist: Mutex<Vec<PlaylistMark>>,
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
            music_id: Option<&str>,
            missing: bool,
        ) -> Result<(), String> {
            self.playlist.lock().unwrap().push((
                source_name.to_string(),
                remote_path.to_string(),
                music_id.map(str::to_string),
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

    fn task(source: &str, account: &str) -> QueueTask {
        QueueTask::new(
            "t-match".to_string(),
            TaskKind::MatchRemote,
            source.to_string(),
            account.to_string(),
            String::new(),
            String::new(),
            TaskTarget::Cache,
            "batch".to_string(),
            0,
        )
    }

    fn ctx_for(
        dir: &Path,
        task: QueueTask,
        vfs: Arc<dyn WdmmVfs>,
        progress: Arc<Recorder>,
        db: &Arc<LibraryDatabase>,
    ) -> JobCtx {
        JobCtx {
            task,
            cancel: CancelToken::new(),
            progress,
            platform: Arc::new(crate::PlatformClient::disabled()),
            db: Arc::clone(db),
            vfs,
            paths: JobPaths::new(dir.join("docs"), dir.join("cache")),
        }
    }

    fn seed_track(db: &LibraryDatabase, id: &str, account: &str, remote: &str) {
        db.upsert_track(&LibraryTrack {
            id: id.to_string(),
            title: id.to_string(),
            remote_path: remote.to_string(),
            account_id: account.to_string(),
            ..Default::default()
        })
        .unwrap();
    }

    #[tokio::test]
    async fn present_rows_clear_the_missing_flag() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        seed_track(&db, "t1", "acc", "/m/a.mp3");
        let store = RecordingStore::default();
        let ctx = ctx_for(
            dir.path(),
            task("nas", "acc"),
            Arc::new(FakeVfs::with_files(&["/m/a.mp3"])),
            Arc::new(Recorder::default()),
            &db,
        );

        let outcome = run_match(&ctx, &store).await;
        assert!(matches!(outcome, JobOutcome::Completed), "{outcome:?}");
        assert_eq!(
            store.missing.lock().unwrap().as_slice(),
            &[("nas".to_string(), "/m/a.mp3".to_string(), false)]
        );
        assert_eq!(
            store.playlist.lock().unwrap().as_slice(),
            &[(
                "nas".to_string(),
                "/m/a.mp3".to_string(),
                Some("t1".to_string()),
                false
            )]
        );
    }

    #[tokio::test]
    async fn absent_rows_are_marked_missing() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        seed_track(&db, "t1", "acc", "/m/gone.mp3");
        let store = RecordingStore::default();
        let ctx = ctx_for(
            dir.path(),
            task("nas", "acc"),
            Arc::new(FakeVfs::default()),
            Arc::new(Recorder::default()),
            &db,
        );

        let outcome = run_match(&ctx, &store).await;
        assert!(matches!(outcome, JobOutcome::Completed), "{outcome:?}");
        assert_eq!(
            store.missing.lock().unwrap().as_slice(),
            &[("nas".to_string(), "/m/gone.mp3".to_string(), true)]
        );
        assert!(store.playlist.lock().unwrap()[0].3);
    }

    #[tokio::test]
    async fn all_probes_failing_fails_the_task() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        seed_track(&db, "t1", "acc", "/m/a.mp3");
        seed_track(&db, "t2", "acc", "/m/b.mp3");
        let store = RecordingStore::default();
        let ctx = ctx_for(
            dir.path(),
            task("*", "acc"),
            Arc::new(FakeVfs::failing()),
            Arc::new(Recorder::default()),
            &db,
        );

        match run_match(&ctx, &store).await {
            JobOutcome::Failed { error, category } => {
                assert!(error.starts_with("err.cloud.unreachable"), "{error}");
                assert!(matches!(category, FailureCategory::VfsData));
            }
            other => panic!("expected failure, got {other:?}"),
        }
        assert!(store.missing.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn locally_cached_rows_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        seed_track(&db, "t1", "acc", "/m/cached.mp3");
        seed_track(&db, "t2", "acc", "/m/remote.mp3");

        let cache = CachePaths::new(dir.path().join("cache"));
        let cached = cache.music_file_for("/m/cached.mp3", Some("nas"));
        std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
        std::fs::write(&cached, b"cached").unwrap();

        let store = RecordingStore::default();
        let ctx = ctx_for(
            dir.path(),
            task("nas", "acc"),
            Arc::new(FakeVfs::default()),
            Arc::new(Recorder::default()),
            &db,
        );

        assert!(matches!(
            run_match(&ctx, &store).await,
            JobOutcome::Completed
        ));
        let calls = store.missing.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, "/m/remote.mp3");
    }

    #[tokio::test]
    async fn account_filter_limits_the_scan() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        seed_track(&db, "t1", "acc1", "/m/a.mp3");
        seed_track(&db, "t2", "acc2", "/m/b.mp3");
        let targets = collect_targets(&db, &task("*", "acc1"), &dir.path().join("cache")).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].music_id, "t1");

        let all = collect_targets(&db, &task("*", ""), &dir.path().join("cache")).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn cue_slices_probe_the_audio_file() {
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
        db.upsert_cue_slice(&wdmm_storage::CueSlice {
            music_id: "cue-1".to_string(),
            cue_id: "cue-album".to_string(),
            audio_music_id: "audio-1".to_string(),
            source_name: "nas".to_string(),
            remote_path: "/m/album.virt".to_string(),
            file_name: "album.flac".to_string(),
            audio_remote_path: Some("/m/album.flac".to_string()),
            ..Default::default()
        })
        .unwrap();

        let store = RecordingStore::default();
        let ctx = ctx_for(
            dir.path(),
            task("nas", ""),
            Arc::new(FakeVfs::with_files(&["/m/album.flac"])),
            Arc::new(Recorder::default()),
            &db,
        );

        assert!(matches!(
            run_match(&ctx, &store).await,
            JobOutcome::Completed
        ));
        assert_eq!(
            store.missing.lock().unwrap().as_slice(),
            &[("nas".to_string(), "/m/album.virt".to_string(), false)]
        );
        assert_eq!(store.playlist.lock().unwrap()[0].2.as_deref(), Some("cue-1"));
    }

    #[tokio::test]
    async fn probes_respect_the_concurrency_cap() {
        let names: Vec<String> = (0..9).map(|i| format!("/m/{i}.mp3")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let vfs = Arc::new(FakeVfs::with_files(&refs));
        let handle: Arc<dyn WdmmVfs> = vfs.clone();
        let probes = probe_paths(&handle, &names, Some(&CancelToken::new()))
            .await
            .unwrap();
        assert_eq!(probes.len(), 9);
        assert!(probes.iter().all(|p| matches!(p, Probe::Present)));
        let peak = vfs.max_in_flight.load(Ordering::SeqCst);
        assert!(
            peak <= MAX_PROBE_CONCURRENCY,
            "peak concurrency {peak} exceeded the cap"
        );
        assert!(peak > 1, "probes should overlap");
    }
}
