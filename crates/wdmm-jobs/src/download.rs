//! Downloads: cache targets write into the app cache, public targets go through
//! the platform bridge (SAF tree / MediaStore / Downloads).
//!
//! One executor serves `DownloadCache` and `DownloadPublic`; the kind decides
//! which path is taken (Dart branches on `QueueTask.target`, which the Rust
//! model no longer distinguishes beyond `Cache`/`Public`).
//!
//! Both paths stream the remote object in [`CHUNK_SIZE`] ranges through
//! [`WdmmVfs::get_content_range`] into a `<dest>.part` file, resuming from the
//! existing `.part` length, then rename it into place. Cancellation is checked
//! between chunks (never mid-chunk) exactly like `WebDavService.downloadToFile`.
//!
//! ## Differences from the Dart executor (documented for the migration)
//!
//! * `TaskTarget` has no gallery/downloads split. A non-empty `tree_uri` in the
//!   enqueue payload sidecar (`task.payload_path`, JSON `{"tree_uri": "…"}`)
//!   routes the file to the user-picked SAF directory via
//!   `PlatformAction::SaveToTree`; without it, video/image files go to the
//!   gallery and everything else to Downloads — the same mapping the Dart UI
//!   used when it called `enqueueGallery` / `enqueueToDownloads`. A malformed or
//!   missing payload is treated as "no custom directory", never as a failure.
//! * Cache index bookkeeping uses the schema-v17 API that landed in T1
//!   (`cache_put_entry` / `cache_bind_group`) rather than a single
//!   `cache_register`; `kind` is Dart's `CacheKind.music.name` = `"music"`.
//! * Public downloads land their scratch file in `<cache>/queue/<taskId>.part`
//!   and are renamed to `<cache>/queue/<taskId>` before the platform hand-off
//!   (contract §6.1) instead of Dart's OS temp `public_dl/` directory. The final
//!   path is reported through `progress.local_path` (persisted immediately)
//!   before the bridge request, and a restart whose final file already matches
//!   `remote_size` skips the transfer entirely.
//! * `AwaitingPlatform` is not persisted: the platform bridge is awaited inline
//!   because `JobCtx` has no queue-store handle. The task row shows `active`
//!   while the platform replies; callers see `err.platformTimeout` after the
//!   300 s bridge timeout.

use std::path::Path;

use async_trait::async_trait;
use serde_json::Value;
use tokio::fs;
use tokio::io::AsyncWriteExt;

use wdmm_queue::model::TaskKind;
use wdmm_queue::policy::FailureCategory;
use wdmm_storage::CacheEntryRow;

use crate::cache_paths::{part_path, CachePaths};
use crate::{ingest, Job, JobCtx, JobOutcome, PlatformAction};

/// Range size for a download chunk. Kept at 8 MiB to match contract §6.1.
pub const CHUNK_SIZE: u64 = 8 * 1024 * 1024;

/// Executor for `DownloadCache` + `DownloadPublic`.
pub struct DownloadJob;

#[async_trait]
impl Job for DownloadJob {
    fn kind(&self) -> TaskKind {
        TaskKind::DownloadCache
    }

    async fn run(&self, ctx: JobCtx) -> JobOutcome {
        match ctx.task.kind {
            TaskKind::DownloadCache => run_cache(&ctx).await,
            TaskKind::DownloadPublic => run_public(&ctx).await,
            other => JobOutcome::Failed {
                error: format!("err.jobKindMismatch|{}", other.as_str()),
                category: FailureCategory::State,
            },
        }
    }
}

async fn run_cache(ctx: &JobCtx) -> JobOutcome {
    if !validate_source(ctx) {
        return source_unbound(ctx);
    }
    let paths = CachePaths::new(&ctx.paths.cache_root);
    let dest = paths.music_file_for(&ctx.task.remote_path, Some(&ctx.task.source_name));
    let tmp = part_path(&dest);

    let total = match download_to_part(ctx, &tmp).await {
        Ok(total) => total,
        Err(outcome) => return outcome,
    };

    if let Err(error) = replace_file(&tmp, &dest).await {
        return JobOutcome::Failed {
            error,
            category: FailureCategory::Other,
        };
    }

    let cache_group = ctx.task.cache_group_id.clone();
    if let Err(error) = register_cache_entry(ctx, &dest, total).await {
        ctx.progress
            .note(&format!("cache index skipped: {error}"));
    }
    if cache_group.as_deref().is_some_and(|g| g.starts_with("cue")) {
        // CUE album ingest belongs to T5 (`cache_group_id` starts with "cue").
        ctx.progress
            .note("cue group ingest pending T5 for cache_group_id");
    }
    // INGEST is a T4 stub today: tolerate its `err.ingestUnimplemented` so a
    // finished download still counts as completed.
    if let Err(error) = ingest::ingest_downloaded(ctx).await {
        ctx.progress.note(&format!("ingest skipped: {error}"));
    }

    ctx.progress.bytes(total, Some(total));
    JobOutcome::Completed
}

async fn run_public(ctx: &JobCtx) -> JobOutcome {
    if !validate_source(ctx) {
        return source_unbound(ctx);
    }
    let paths = CachePaths::new(&ctx.paths.cache_root);
    let part = paths.public_temp_file(&ctx.task.id);
    let final_path = public_final_file(&part);

    // Idempotent restart: a transferred file of the expected size is already
    // the payload the platform needs, so a Pending row that comes back after a
    // process death must not pull the object again. Without a known remote size
    // there is no completion marker, so the transfer runs normally.
    let known_size = ctx.task.remote_size.filter(|size| *size > 0);
    let already_complete = match known_size {
        Some(size) if completed_size(&final_path).await == Some(size) => Some(size),
        _ => None,
    };
    let total = if let Some(size) = already_complete {
        ctx.progress.note(&format!(
            "public download already complete: {} ({size} bytes)",
            final_path.display()
        ));
        ctx.progress.bytes(size, Some(size));
        size
    } else {
        let total = match download_to_part(ctx, &part).await {
            Ok(total) => total,
            Err(outcome) => return outcome,
        };
        if let Err(error) = replace_file(&part, &final_path).await {
            return JobOutcome::Failed {
                error,
                category: FailureCategory::Other,
            };
        }
        total
    };

    // Persist the payload location before the platform hand-off: if the process
    // dies while the bridge waits, the retry finds a complete file above.
    ctx.progress.local_path(&final_path.to_string_lossy());

    let tree_uri = payload_tree_uri(ctx).await;
    let (action, action_name) =
        public_action(&ctx.task.file_name, &final_path, tree_uri.as_deref());
    ctx.progress
        .note(&format!("awaiting platform: {action_name}"));
    match ctx.platform.request(action).await {
        Ok(reply) => match public_reply_path(&reply) {
            Ok(local_path) => {
                // The platform owns the saved copy now; the scratch file is
                // done. Failures below keep it for a resumable retry.
                let _ = fs::remove_file(&final_path).await;
                ctx.progress.bytes(total, Some(total));
                ctx.progress.note(&format!(
                    "public download delivered: {local_path}"
                ));
                JobOutcome::Completed
            }
            Err(error) => JobOutcome::Failed {
                error,
                category: FailureCategory::Other,
            },
        },
        Err(error) => JobOutcome::Failed {
            error: platform_error(&error),
            category: FailureCategory::Other,
        },
    }
}

/// Final public-download payload: the scratch `.part` without its suffix, so
/// the same id keeps the same resume/skip target across restarts.
fn public_final_file(part: &Path) -> std::path::PathBuf {
    match part
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".part"))
    {
        Some(stem) => part.with_file_name(stem),
        None => part.to_path_buf(),
    }
}

async fn completed_size(path: &Path) -> Option<u64> {
    fs::metadata(path).await.ok().map(|meta| meta.len())
}

/// Read the optional `{"tree_uri": "content://…"}` enqueue payload sidecar.
///
/// The queue stores the enqueue `payload` string in a sidecar and leaves only
/// `task.payload_path` on the row. A missing path, unreadable file, malformed
/// JSON, non-string or blank value all mean "no custom directory", so the
/// caller falls back to the gallery/Downloads routing without failing the job.
async fn payload_tree_uri(ctx: &JobCtx) -> Option<String> {
    let path = ctx
        .task
        .payload_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())?;
    let body = fs::read_to_string(path).await.ok()?;
    let payload: Value = serde_json::from_str(&body).ok()?;
    let uri = payload.get("tree_uri")?.as_str()?.trim();
    if uri.is_empty() {
        None
    } else {
        Some(uri.to_string())
    }
}

/// Stream `ctx.task.remote_path` into `dest` (a `.part` path), resuming from
/// the file's current length. Returns the final byte count.
///
/// The `.part` file is removed on cancellation (Dart does the same) and kept on
/// failure so the next attempt resumes from its length.
async fn download_to_part(ctx: &JobCtx, dest: &Path) -> Result<u64, JobOutcome> {
    if ctx.cancelled() {
        // Dart's cache executor removes `<dest>.part` when the run is
        // cancelled, including a `.part` left over from an earlier attempt.
        let _ = fs::remove_file(dest).await;
        return Err(JobOutcome::Cancelled);
    }
    if let Some(parent) = dest.parent() {
        if let Err(error) = fs::create_dir_all(parent).await {
            return Err(local_failure("err.local|create dir", error));
        }
    }

    let remote_path = ctx.task.remote_path.as_str();
    let total = match ctx.task.bytes_total.or(ctx.task.remote_size) {
        Some(total) if total > 0 => total,
        _ => match ctx.vfs.get(remote_path).await {
            Ok(item) => item.size,
            Err(error) => {
                return Err(JobOutcome::Failed {
                    error: format!("{error}"),
                    category: FailureCategory::Dio,
                })
            }
        },
    };

    let mut have = match fs::metadata(dest).await {
        Ok(meta) => meta.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => return Err(local_failure("err.local|stat part", error)),
    };
    // A stale, oversized `.part` (e.g. the remote shrank) would make the range
    // request invalid; start over rather than append garbage.
    if have > total {
        have = 0;
        if let Err(error) = fs::File::create(dest).await {
            return Err(local_failure("err.local|truncate part", error));
        }
    }

    let mut file = match fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dest)
        .await
    {
        Ok(file) => file,
        Err(error) => return Err(local_failure("err.local|open part", error)),
    };

    let mut written = have;
    while written < total {
        if ctx.cancelled() {
            let _ = fs::remove_file(dest).await;
            return Err(JobOutcome::Cancelled);
        }
        let expected =
            (written + CHUNK_SIZE).min(total) - written;
        let end = written + expected - 1;
        let bytes = match ctx.vfs.get_content_range(remote_path, written, Some(end)).await {
            Ok(bytes) => bytes,
            Err(error) => {
                return Err(JobOutcome::Failed {
                    error: format!("{error}"),
                    category: FailureCategory::Dio,
                })
            }
        };
        // Drivers that ignore the range would corrupt an append; accept a
        // full-body answer only when nothing has been written yet.
        let got = bytes.len() as u64;
        let whole_body = written == 0 && got == total;
        if got != expected && !whole_body {
            return Err(JobOutcome::Failed {
                error: format!(
                    "err.download.range|expected {expected} bytes at {written}, got {got}"
                ),
                category: FailureCategory::Dio,
            });
        }
        if let Err(error) = file.write_all(&bytes).await {
            return Err(local_failure("err.local|write part", error));
        }
        written += bytes.len() as u64;
        ctx.progress.bytes(written.min(total), Some(total));
    }

    if let Err(error) = file.flush().await {
        return Err(local_failure("err.local|flush part", error));
    }
    if let Err(error) = file.sync_all().await {
        return Err(local_failure("err.local|sync part", error));
    }
    drop(file);
    Ok(total)
}

/// `rename(tmp → dest)` with Windows-safe overwrite semantics.
async fn replace_file(tmp: &Path, dest: &Path) -> Result<(), String> {
    if fs::metadata(dest).await.is_ok() {
        if let Err(error) = fs::remove_file(dest).await {
            return Err(format!("err.local|remove old cache file: {error}"));
        }
    }
    if let Err(error) = fs::rename(tmp, dest).await {
        return Err(format!("err.local|rename part: {error}"));
    }
    Ok(())
}

/// Cache-index registration (schema v17, replacing Dart `cache_index.db`).
///
/// Mirrors `CacheService.registerCompleted`: upsert the `cache_entries` row with
/// the freshly written size, then bind the file into its `cache_groups` album.
/// Best effort: a bookkeeping failure must not fail a landed download.
async fn register_cache_entry(ctx: &JobCtx, local_path: &Path, size: u64) -> Result<(), String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default();
    let path = local_path.display().to_string();
    let entry = CacheEntryRow {
        path: path.clone(),
        kind: "music".to_string(),
        size: size as i64,
        accessed_at: now,
        created_at: now,
        account_id: Some(ctx.task.account_id.clone()),
        remote_path: Some(ctx.task.remote_path.clone()),
    };
    if let Err(error) = ctx.db.cache_put_entry(&entry) {
        return Err(format!("cache_put_entry: {error}"));
    }
    if let Some(group_id) = ctx
        .task
        .cache_group_id
        .as_deref()
        .filter(|group| !group.is_empty())
    {
        if let Err(error) = ctx.db.cache_bind_group(group_id, &[path]) {
            return Err(format!("cache_bind_group({group_id}): {error}"));
        }
    }
    Ok(())
}

fn validate_source(ctx: &JobCtx) -> bool {
    !ctx.task.account_id.trim().is_empty() && !ctx.task.remote_path.trim().is_empty()
}

fn source_unbound(ctx: &JobCtx) -> JobOutcome {
    JobOutcome::Failed {
        error: format!("err.sourceUnbound|{}", ctx.task.source_name),
        category: FailureCategory::State,
    }
}

fn local_failure(context: &str, error: std::io::Error) -> JobOutcome {
    JobOutcome::Failed {
        error: format!("{context}: {error}"),
        category: FailureCategory::Other,
    }
}

/// Choose the platform action for a finished public download.
///
/// A non-empty `tree_uri` (custom SAF directory from the enqueue payload) wins
/// over the media routing: everything is written into that tree. Otherwise
/// video/image files go to the gallery (MediaStore) and everything else to the
/// public Downloads collection.
fn public_action(
    file_name: &str,
    tmp: &Path,
    tree_uri: Option<&str>,
) -> (PlatformAction, &'static str) {
    let temp_path = tmp.display().to_string();
    let name = file_name.to_string();
    let mime = Some(gallery_mime_for(file_name));
    if let Some(tree_uri) = tree_uri.map(str::trim).filter(|uri| !uri.is_empty()) {
        return (
            PlatformAction::SaveToTree {
                tree_uri: tree_uri.to_string(),
                temp_path,
                name,
                mime,
            },
            "save_to_tree",
        );
    }
    if is_gallery_extension(file_name) {
        (
            PlatformAction::SaveToGallery {
                temp_path,
                name,
                mime,
            },
            "save_to_gallery",
        )
    } else {
        (
            PlatformAction::SaveToDownloads {
                temp_path,
                name,
                mime,
            },
            "save_to_downloads",
        )
    }
}

/// Interpret the platform reply. `Ok(local_uri_or_path)` on success, the Dart
/// error string on `{"ok":false}`.
fn public_reply_path(reply: &Value) -> Result<String, String> {
    let ok = reply.get("ok").and_then(Value::as_bool).unwrap_or(true);
    if !ok {
        if reply.get("safInvalid").and_then(Value::as_bool).unwrap_or(false) {
            return Err("err.customDirInvalid".to_string());
        }
        return Err(reply
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("err.writePublicFailed")
            .to_string());
    }
    Ok(reply
        .get("uri")
        .and_then(Value::as_str)
        .or_else(|| reply.get("path").and_then(Value::as_str))
        .unwrap_or_default()
        .to_string())
}

/// Map a bridge failure to the Dart-facing error code.
fn platform_error(error: &str) -> String {
    if error.contains("timeout") {
        "err.platformTimeout".to_string()
    } else if error.contains("unavailable") || error.contains("channel closed") {
        "err.safUnavailable".to_string()
    } else {
        format!("err.platform|{error}")
    }
}

fn is_gallery_extension(file_name: &str) -> bool {
    let ext = extension(file_name);
    matches!(
        ext.as_str(),
        "mp4" | "m4v" | "mkv" | "webm" | "avi" | "mov" | "flv" | "ts" | "wmv" | "3gp"
            | "mpg" | "mpeg" | "jpg" | "jpeg" | "png" | "webp" | "gif"
    )
}

/// Dart `galleryMimeFor` — MIME handed to MediaStore / the share sheet.
pub(crate) fn gallery_mime_for(file_name: &str) -> String {
    match extension(file_name).as_str() {
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "mov" => "video/quicktime",
        "flv" => "video/x-flv",
        "ts" => "video/mp2t",
        "wmv" => "video/x-ms-wmv",
        "3gp" => "video/3gpp",
        "mpg" | "mpeg" => "video/mpeg",
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "m4a" | "aac" => "audio/mp4",
        "ogg" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        "wma" => "audio/x-ms-wma",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "zip" => "application/zip",
        "json" => "application/json",
        "m3u8" => "application/vnd.apple.mpegurl",
        _ => "application/octet-stream",
    }
    .to_string()
}

fn extension(file_name: &str) -> String {
    match file_name.rfind('.') {
        Some(index) => file_name[index + 1..].to_ascii_lowercase(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use crate::{JobPaths, ProgressSink};
    use wdmm_drivers::{CloudFileItem, ContentDeliveryKind, DriverError, Result as VfsResult, WdmmVfs};
    use wdmm_queue::model::{CancelToken, QueueTask, TaskTarget};
    use wdmm_storage::LibraryDatabase;

    /// VFS stand-in: serves `data` by range and records every request.
    struct FakeVfs {
        data: Vec<u8>,
        ranges: Mutex<Vec<(u64, Option<u64>)>>,
        fail_from: Option<u64>,
        ignore_range: bool,
    }

    impl FakeVfs {
        fn new(data: Vec<u8>) -> Self {
            Self {
                data,
                ranges: Mutex::new(Vec::new()),
                fail_from: None,
                ignore_range: false,
            }
        }

        fn failing_from(mut self, start: u64) -> Self {
            self.fail_from = Some(start);
            self
        }

        fn ranges(&self) -> Vec<(u64, Option<u64>)> {
            self.ranges.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl WdmmVfs for FakeVfs {
        async fn list(&self, _path: &str) -> VfsResult<Vec<CloudFileItem>> {
            Ok(Vec::new())
        }

        async fn get(&self, path: &str) -> VfsResult<CloudFileItem> {
            Ok(CloudFileItem {
                name: path.rsplit('/').next().unwrap_or(path).to_string(),
                is_dir: false,
                size: self.data.len() as u64,
                modified: None,
                raw_url: None,
                raw_headers: Vec::new(),
                delivery: ContentDeliveryKind::LocalStream,
                local_path: None,
            })
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

        async fn put_content(&self, _path: &str, _data: &[u8]) -> VfsResult<()> {
            Ok(())
        }

        async fn get_content(&self, _path: &str) -> VfsResult<Vec<u8>> {
            Ok(self.data.clone())
        }

        async fn get_content_range(
            &self,
            _path: &str,
            start: u64,
            end: Option<u64>,
        ) -> VfsResult<Vec<u8>> {
            self.ranges.lock().unwrap().push((start, end));
            if let Some(fail_from) = self.fail_from {
                if start >= fail_from {
                    return Err(DriverError::Network("boom".into()));
                }
            }
            if self.ignore_range {
                return Ok(self.data.clone());
            }
            if start >= self.data.len() as u64 {
                return Ok(Vec::new());
            }
            let last = end
                .unwrap_or(self.data.len() as u64 - 1)
                .min(self.data.len() as u64 - 1);
            Ok(self.data[start as usize..=last as usize].to_vec())
        }
    }

    /// Progress sink that records byte updates, persisted local paths and diag
    /// notes.
    #[derive(Default)]
    struct Recorder {
        bytes: Mutex<Vec<(u64, Option<u64>)>>,
        notes: Mutex<Vec<String>>,
        local_paths: Mutex<Vec<String>>,
    }

    impl ProgressSink for Recorder {
        fn bytes(&self, done: u64, total: Option<u64>) {
            self.bytes.lock().unwrap().push((done, total));
        }

        fn units(&self, _done: u32, _total: Option<u32>) {}

        fn note(&self, message: &str) {
            self.notes.lock().unwrap().push(message.to_string());
        }

        fn local_path(&self, path: &str) {
            self.local_paths.lock().unwrap().push(path.to_string());
        }
    }

    fn task(kind: TaskKind, id: &str, file_name: &str) -> QueueTask {
        QueueTask::new(
            id.to_string(),
            kind,
            "nas".to_string(),
            "acc".to_string(),
            "/Music/Album/Track 01.mp3".to_string(),
            file_name.to_string(),
            match kind {
                TaskKind::DownloadCache => TaskTarget::Cache,
                _ => TaskTarget::Public,
            },
            "batch".to_string(),
            0,
        )
    }

    fn build_ctx(
        dir: &Path,
        task: QueueTask,
        vfs: Arc<dyn WdmmVfs>,
        platform: crate::PlatformClient,
        progress: Arc<Recorder>,
    ) -> JobCtx {
        let db = Arc::new(LibraryDatabase::open(Path::new(":memory:")).unwrap());
        build_ctx_with_db(dir, task, vfs, platform, progress, db)
    }

    fn build_ctx_with_db(
        dir: &Path,
        task: QueueTask,
        vfs: Arc<dyn WdmmVfs>,
        platform: crate::PlatformClient,
        progress: Arc<Recorder>,
        db: Arc<LibraryDatabase>,
    ) -> JobCtx {
        JobCtx {
            task,
            cancel: CancelToken::new(),
            progress,
            platform: Arc::new(platform),
            db,
            vfs,
            paths: JobPaths::new(dir.join("docs"), dir.join("cache")),
        }
    }

    fn pattern_data(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    fn cache_dest(dir: &Path) -> std::path::PathBuf {
        CachePaths::new(dir.join("cache")).music_file_for(
            "/Music/Album/Track 01.mp3",
            Some("nas"),
        )
    }

    #[tokio::test]
    async fn cache_download_resumes_and_uses_eight_mib_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let data = pattern_data(2 * CHUNK_SIZE as usize + 1234);
        let resume = 777_u64;
        let dest = cache_dest(dir.path());
        let part = part_path(&dest);
        std::fs::create_dir_all(part.parent().unwrap()).unwrap();
        std::fs::write(&part, &data[..resume as usize]).unwrap();

        let vfs = Arc::new(FakeVfs::new(data.clone()));
        let progress = Arc::new(Recorder::default());
        let ctx = build_ctx(
            dir.path(),
            task(TaskKind::DownloadCache, "t-cache", "Track 01.mp3"),
            vfs.clone(),
            crate::PlatformClient::disabled(),
            progress.clone(),
        );

        let outcome = DownloadJob.run(ctx).await;
        assert!(matches!(outcome, JobOutcome::Completed), "{outcome:?}");

        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert!(!part.exists(), ".part must be renamed away");
        assert_eq!(
            vfs.ranges(),
            vec![
                (resume, Some(resume + CHUNK_SIZE - 1)),
                (resume + CHUNK_SIZE, Some(resume + 2 * CHUNK_SIZE - 1)),
                (resume + 2 * CHUNK_SIZE, Some(data.len() as u64 - 1)),
            ]
        );
        let last = *progress.bytes.lock().unwrap().last().unwrap();
        assert_eq!(last, (data.len() as u64, Some(data.len() as u64)));
    }

    #[tokio::test]
    async fn cache_download_registers_index_entry_and_group() {
        let dir = tempfile::tempdir().unwrap();
        let data = pattern_data(2048);
        let db = Arc::new(LibraryDatabase::open(Path::new(":memory:")).unwrap());
        let mut cache_task = task(TaskKind::DownloadCache, "t-index", "Track 01.mp3");
        cache_task.cache_group_id = Some("cue-album".to_string());
        let progress = Arc::new(Recorder::default());
        let ctx = build_ctx_with_db(
            dir.path(),
            cache_task,
            Arc::new(FakeVfs::new(data.clone())),
            crate::PlatformClient::disabled(),
            progress.clone(),
            db.clone(),
        );
        assert!(matches!(DownloadJob.run(ctx).await, JobOutcome::Completed));

        let dest = cache_dest(dir.path());
        let path = dest.to_string_lossy().to_string();
        let entry = db.cache_get_entry(&path).unwrap().expect("cache entry");
        assert_eq!(entry.kind, "music");
        assert_eq!(entry.size, data.len() as i64);
        assert_eq!(entry.account_id.as_deref(), Some("acc"));
        assert_eq!(
            entry.remote_path.as_deref(),
            Some("/Music/Album/Track 01.mp3")
        );
        assert!(entry.accessed_at > 0);
        assert_eq!(db.cache_group_paths("cue-album").unwrap(), vec![path]);
        assert!(progress
            .notes
            .lock()
            .unwrap()
            .iter()
            .any(|note| note.contains("pending T5")));
    }

    #[tokio::test]
    async fn cache_download_cancel_removes_part_and_returns_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let dest = cache_dest(dir.path());
        let part = part_path(&dest);
        std::fs::create_dir_all(part.parent().unwrap()).unwrap();
        std::fs::write(&part, b"partial").unwrap();

        let vfs = Arc::new(FakeVfs::new(pattern_data(1024)));
        let ctx = build_ctx(
            dir.path(),
            task(TaskKind::DownloadCache, "t-cancel", "Track 01.mp3"),
            vfs,
            crate::PlatformClient::disabled(),
            Arc::new(Recorder::default()),
        );
        ctx.cancel.cancel();

        let outcome = DownloadJob.run(ctx).await;
        assert!(matches!(outcome, JobOutcome::Cancelled));
        assert!(!part.exists());
        assert!(!dest.exists());
    }

    #[tokio::test]
    async fn cache_download_failure_is_dio_and_keeps_part() {
        let dir = tempfile::tempdir().unwrap();
        let dest = cache_dest(dir.path());
        let part = part_path(&dest);
        std::fs::create_dir_all(part.parent().unwrap()).unwrap();
        std::fs::write(&part, b"keep-me").unwrap();

        let vfs = Arc::new(FakeVfs::new(pattern_data(512)).failing_from(0));
        let ctx = build_ctx(
            dir.path(),
            task(TaskKind::DownloadCache, "t-fail", "Track 01.mp3"),
            vfs,
            crate::PlatformClient::disabled(),
            Arc::new(Recorder::default()),
        );

        match DownloadJob.run(ctx).await {
            JobOutcome::Failed { category, error } => {
                assert_eq!(category, FailureCategory::Dio);
                assert!(error.contains("boom"), "{error}");
            }
            other => panic!("expected Dio failure, got {other:?}"),
        }
        assert_eq!(std::fs::read(&part).unwrap(), b"keep-me");
        assert!(!dest.exists());
    }

    #[tokio::test]
    async fn cache_download_empty_remote_creates_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let vfs = Arc::new(FakeVfs::new(Vec::new()));
        let ctx = build_ctx(
            dir.path(),
            task(TaskKind::DownloadCache, "t-empty", "Track 01.mp3"),
            vfs,
            crate::PlatformClient::disabled(),
            Arc::new(Recorder::default()),
        );
        let outcome = DownloadJob.run(ctx).await;
        assert!(matches!(outcome, JobOutcome::Completed), "{outcome:?}");
        assert_eq!(std::fs::read(cache_dest(dir.path())).unwrap(), Vec::<u8>::new());
    }

    /// Result of one public download run. Sizes are measured while the temp dir
    /// is still alive, so the dir is carried in the struct.
    struct PublicRun {
        outcome: JobOutcome,
        action: PlatformAction,
        final_path: std::path::PathBuf,
        final_size: Option<u64>,
        part_size: Option<u64>,
        local_paths: Vec<String>,
        /// Keeps the temp dir alive while the tests inspect its contents.
        #[allow(dead_code)]
        dir: tempfile::TempDir,
    }

    async fn public_run(file_name: &str, reply: Value, payload: Option<&str>) -> PublicRun {
        let dir = tempfile::tempdir().unwrap();
        let vfs = Arc::new(FakeVfs::new(pattern_data(4096)));
        let (client, mut rx) = crate::PlatformClient::channel(4);
        let responder = tokio::spawn(async move {
            let call = rx.recv().await.expect("platform call");
            let action = call.action.clone();
            let _ = call.reply.send(Ok(reply));
            action
        });
        let progress = Arc::new(Recorder::default());
        let mut public_task = task(TaskKind::DownloadPublic, "t-public", file_name);
        if let Some(body) = payload {
            let sidecar = dir.path().join("queue-payload.json");
            std::fs::write(&sidecar, body).unwrap();
            public_task.payload_path = Some(sidecar.display().to_string());
        }
        let ctx = build_ctx(
            dir.path(),
            public_task,
            vfs.clone(),
            client,
            progress.clone(),
        );
        let part = CachePaths::new(dir.path().join("cache")).public_temp_file("t-public");
        let final_path = public_final_file(&part);
        let outcome = DownloadJob.run(ctx).await;
        let action = responder.await.unwrap();
        let final_size = std::fs::metadata(&final_path).ok().map(|meta| meta.len());
        let part_size = std::fs::metadata(&part).ok().map(|meta| meta.len());
        let local_paths = progress.local_paths.lock().unwrap().clone();
        PublicRun {
            outcome,
            action,
            final_path,
            final_size,
            part_size,
            local_paths,
            dir,
        }
    }

    #[tokio::test]
    async fn public_video_goes_to_gallery_and_cleans_temp() {
        let run = public_run(
            "clip.MP4",
            serde_json::json!({"ok": true, "uri": "content://media/1"}),
            None,
        )
        .await;
        assert!(matches!(run.outcome, JobOutcome::Completed), "{:?}", run.outcome);
        match run.action {
            PlatformAction::SaveToGallery { name, mime, temp_path } => {
                assert_eq!(name, "clip.MP4");
                assert_eq!(mime.as_deref(), Some("video/mp4"));
                assert_eq!(
                    std::path::PathBuf::from(&temp_path),
                    run.final_path,
                    "platform must see the renamed final file"
                );
            }
            ref other => panic!("expected SaveToGallery, got {other:?}"),
        }
        assert_eq!(run.final_size, None, "payload file must be gone after success");
        assert_eq!(run.part_size, None);
        assert_eq!(
            run.local_paths,
            vec![run.final_path.to_string_lossy().to_string()]
        );
    }

    #[tokio::test]
    async fn public_unknown_extension_goes_to_downloads() {
        let run = public_run(
            "archive.bin",
            serde_json::json!({"ok": true, "path": "/storage/emulated/0/Download/a.bin"}),
            None,
        )
        .await;
        assert!(matches!(run.outcome, JobOutcome::Completed), "{:?}", run.outcome);
        assert!(matches!(run.action, PlatformAction::SaveToDownloads { .. }));
        assert_eq!(run.final_size, None);
    }

    #[tokio::test]
    async fn public_failure_keeps_file_for_retry() {
        let run = public_run(
            "clip.mp4",
            serde_json::json!({"ok": false, "error": "err.diskFull"}),
            None,
        )
        .await;
        match run.outcome {
            JobOutcome::Failed { error, category } => {
                assert_eq!(error, "err.diskFull");
                assert_eq!(category, FailureCategory::Other);
            }
            ref other => panic!("expected failure, got {other:?}"),
        }
        assert_eq!(run.final_size, Some(4096));
        assert_eq!(run.part_size, None, "part must be renamed away");
    }

    #[tokio::test]
    async fn public_saf_invalid_maps_to_custom_dir_error() {
        let run = public_run(
            "clip.mp4",
            serde_json::json!({"ok": false, "safInvalid": true}),
            None,
        )
        .await;
        match run.outcome {
            JobOutcome::Failed { error, .. } => assert_eq!(error, "err.customDirInvalid"),
            ref other => panic!("expected failure, got {other:?}"),
        }
        assert_eq!(run.final_size, Some(4096));
    }

    #[tokio::test]
    async fn public_tree_uri_routes_to_save_to_tree() {
        let tree_uri = "content://com.android.externalstorage.documents/tree/primary%3ADownload";
        let payload = format!(r#"{{"tree_uri":"{tree_uri}"}}"#);
        let run = public_run(
            "clip.mp4",
            serde_json::json!({"ok": true, "uri": "content://tree/document/1"}),
            Some(payload.as_str()),
        )
        .await;
        assert!(matches!(run.outcome, JobOutcome::Completed), "{:?}", run.outcome);
        match run.action {
            PlatformAction::SaveToTree {
                ref tree_uri,
                ref temp_path,
                ref name,
                ref mime,
            } => {
                assert_eq!(tree_uri, "content://com.android.externalstorage.documents/tree/primary%3ADownload");
                assert_eq!(name, "clip.mp4");
                assert_eq!(mime.as_deref(), Some("video/mp4"));
                assert_eq!(
                    std::path::PathBuf::from(temp_path),
                    run.final_path,
                    "tree save must use the renamed final file"
                );
            }
            ref other => panic!("expected SaveToTree, got {other:?}"),
        }
        assert_eq!(run.final_size, None, "tree hand-off owns the copy now");

        // The Dart bridge reads these exact params keys for `save_to_tree`.
        let params = serde_json::to_value(&run.action).unwrap();
        assert_eq!(params["action"], "save_to_tree");
        assert_eq!(params["tree_uri"], tree_uri);
        assert!(params.get("temp_path").is_some());
        assert!(params.get("name").is_some());
        assert!(params.get("mime").is_some());
    }

    #[tokio::test]
    async fn public_payload_without_tree_uri_keeps_media_routing() {
        let run = public_run(
            "clip.mp4",
            serde_json::json!({"ok": true, "uri": "content://media/2"}),
            Some(r#"{"batch":"nightly"}"#),
        )
        .await;
        assert!(matches!(run.outcome, JobOutcome::Completed), "{:?}", run.outcome);
        assert!(matches!(run.action, PlatformAction::SaveToGallery { .. }));
    }

    #[tokio::test]
    async fn public_malformed_payload_falls_back_without_failing() {
        for body in [
            "not json at all",
            r#"{"tree_uri":"   "}"#,
            r#"{"tree_uri":42}"#,
            r#"["tree_uri"]"#,
        ] {
            let run = public_run(
                "clip.mp4",
                serde_json::json!({"ok": true, "uri": "content://media/3"}),
                Some(body),
            )
            .await;
            assert!(
                matches!(run.outcome, JobOutcome::Completed),
                "{body}: {:?}",
                run.outcome
            );
            assert!(
                matches!(run.action, PlatformAction::SaveToGallery { .. }),
                "{body}: {:?}",
                run.action
            );
        }
    }

    #[tokio::test]
    async fn public_tree_saf_invalid_maps_to_custom_dir_error() {
        let payload = r#"{"tree_uri":"content://tree/primary%3ADownload"}"#;
        let run = public_run(
            "clip.mp4",
            serde_json::json!({"ok": false, "safInvalid": true}),
            Some(payload),
        )
        .await;
        match run.outcome {
            JobOutcome::Failed { error, .. } => assert_eq!(error, "err.customDirInvalid"),
            ref other => panic!("expected failure, got {other:?}"),
        }
        assert!(
            matches!(run.action, PlatformAction::SaveToTree { .. }),
            "the failing action must be the tree save"
        );
        assert_eq!(run.final_size, Some(4096), "failed tree save keeps the payload");
    }

    #[tokio::test]
    async fn public_restart_skips_transfer_when_final_file_matches_remote_size() {
        let dir = tempfile::tempdir().unwrap();
        let data = pattern_data(4096);
        let part = CachePaths::new(dir.path().join("cache")).public_temp_file("t-idem");
        let final_path = public_final_file(&part);
        std::fs::create_dir_all(final_path.parent().unwrap()).unwrap();
        std::fs::write(&final_path, &data).unwrap();

        let vfs = Arc::new(FakeVfs::new(data));
        let (client, mut rx) = crate::PlatformClient::channel(4);
        let responder = tokio::spawn(async move {
            let call = rx.recv().await.expect("platform call");
            let action = call.action.clone();
            let _ = call.reply.send(Ok(serde_json::json!({
                "ok": true,
                "uri": "content://media/9"
            })));
            action
        });
        let progress = Arc::new(Recorder::default());
        let mut public_task = task(TaskKind::DownloadPublic, "t-idem", "clip.mp4");
        public_task.remote_size = Some(4096);
        let ctx = build_ctx(
            dir.path(),
            public_task,
            vfs.clone(),
            client,
            progress.clone(),
        );
        let outcome = DownloadJob.run(ctx).await;
        let action = responder.await.unwrap();

        assert!(matches!(outcome, JobOutcome::Completed), "{outcome:?}");
        assert!(vfs.ranges().is_empty(), "completed payload must not be re-fetched");
        assert!(matches!(action, PlatformAction::SaveToGallery { .. }));
        assert_eq!(
            progress.local_paths.lock().unwrap().as_slice(),
            &[final_path.to_string_lossy().to_string()]
        );
        assert_eq!(
            *progress.bytes.lock().unwrap().last().unwrap(),
            (4096, Some(4096))
        );
        assert!(!final_path.exists(), "delivered payload is cleaned up");
    }

    #[tokio::test]
    async fn public_platform_unavailable_maps_to_saf_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let vfs = Arc::new(FakeVfs::new(pattern_data(256)));
        let ctx = build_ctx(
            dir.path(),
            task(TaskKind::DownloadPublic, "t-no-saf", "clip.mp4"),
            vfs,
            crate::PlatformClient::disabled(),
            Arc::new(Recorder::default()),
        );
        match DownloadJob.run(ctx).await {
            JobOutcome::Failed { error, .. } => assert_eq!(error, "err.safUnavailable"),
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[test]
    fn mime_mapping_matches_dart_table() {
        assert_eq!(gallery_mime_for("a.MP4"), "video/mp4");
        assert_eq!(gallery_mime_for("a.mkv"), "video/x-matroska");
        assert_eq!(gallery_mime_for("a.flac"), "audio/flac");
        assert_eq!(gallery_mime_for("a.unknown"), "application/octet-stream");
    }

    #[tokio::test]
    async fn target_validation_rejects_empty_source() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = task(TaskKind::DownloadCache, "t-validate", "Track 01.mp3");
        t.account_id = String::new();
        let ctx = build_ctx(
            dir.path(),
            t,
            Arc::new(FakeVfs::new(Vec::new())),
            crate::PlatformClient::disabled(),
            Arc::new(Recorder::default()),
        );
        assert!(!validate_source(&ctx));
    }
}
