//! Upload executor: materialise a SAF `content://` source through the platform
//! bridge, then write the payload to the account's `WdmmVfs`.
//!
//! Flow (Dart `TaskQueueService._runUpload`):
//!
//! 1. `local_uri` present → `PlatformAction::Materialize`; Dart copies the
//!    content URI into `ctx.paths.payload_dir` and replies `{ok, path}`.
//! 2. Resolve the local file (`local_uri` temp wins over `local_path`, matching
//!    the Dart `task.localPath = jitCached` assignment).
//! 3. Reject missing/empty files with `err.cloud.notFound|<path>` and files
//!    above [`MAX_UPLOAD_SIZE`] with `err.uploadTooLarge` (contract §6.2).
//! 4. Read the file in [`READ_CHUNK`] segments, reporting byte progress and
//!    checking cancellation between segments, then `vfs.put_content`.
//! 5. On success delete the materialised temp file and any payload file inside
//!    `ctx.paths.payload_dir`; on failure keep them so a retry can resume from
//!    the same source. `_followUpPackedUpload` (manga pack) is T5.
//!
//! ## Differences from Dart / known gaps
//!
//! * `WdmmVfs::put_content` takes a single `&[u8]`, so the whole payload is
//!   still buffered before the call. The 512 MiB cap bounds that buffer, and
//!   the segmented read keeps progress/cancellation responsive, but a true
//!   streaming upload needs a driver-level `put_file`/stream API (reported to
//!   the Lead as an interface gap).
//! * Dart deletes `task.localPath` even when it points at a user file. We only
//!   delete files under `ctx.paths.payload_dir` (the queue's own scratch area)
//!   so a user-selected file is not silently removed.
//! * Dart registers the packed-CBZ library row before deleting the payload;
//!   that follow-up is T5 and is skipped here.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::Value;
use tokio::fs;
use tokio::io::AsyncReadExt;

use wdmm_queue::model::TaskKind;
use wdmm_queue::policy::FailureCategory;

use crate::{Job, JobCtx, JobOutcome, PlatformAction};

/// Contract §6.2 hard cap for a single upload.
pub const MAX_UPLOAD_SIZE: u64 = 512 * 1024 * 1024;

/// Local read segment. Also the cancellation/progress granularity.
const READ_CHUNK: usize = 1024 * 1024;

/// Executor for `TaskKind::Upload`.
pub struct UploadJob;

#[async_trait]
impl Job for UploadJob {
    fn kind(&self) -> TaskKind {
        TaskKind::Upload
    }

    async fn run(&self, ctx: JobCtx) -> JobOutcome {
        run_upload(&ctx).await
    }
}

async fn run_upload(ctx: &JobCtx) -> JobOutcome {
    if ctx.task.account_id.trim().is_empty() || ctx.task.remote_path.trim().is_empty() {
        return JobOutcome::Failed {
            error: format!("err.sourceUnbound|{}", ctx.task.source_name),
            category: FailureCategory::State,
        };
    }
    if ctx.cancelled() {
        return JobOutcome::Cancelled;
    }

    let materialized = match materialize(ctx).await {
        Ok(path) => path,
        Err(outcome) => return outcome,
    };
    let local = match resolve_local(ctx, materialized.as_deref()) {
        Ok(path) => path,
        Err(outcome) => return outcome,
    };

    let total = match fs::metadata(&local).await {
        Ok(meta) if meta.is_file() && meta.len() > 0 => meta.len(),
        Ok(_) => return missing_file(&local),
        Err(_) => return missing_file(&local),
    };
    if is_too_large(total) {
        return JobOutcome::Failed {
            error: format!("err.uploadTooLarge|{total}"),
            category: FailureCategory::State,
        };
    }
    ctx.progress.bytes(0, Some(total));

    let data = match read_payload(ctx, &local, total).await {
        Ok(data) => data,
        Err(outcome) => return outcome,
    };
    if ctx.cancelled() {
        return JobOutcome::Cancelled;
    }

    // Best effort: object-store drivers may reject mkdir but accept the PUT.
    if let Some(parent) = remote_parent(&ctx.task.remote_path) {
        if let Err(error) = ctx.vfs.ensure_dir(parent).await {
            ctx.progress
                .note(&format!("ensure_dir({parent}) skipped: {error}"));
        }
    }

    if let Err(error) = ctx
        .vfs
        .put_content(&ctx.task.remote_path, &data)
        .await
    {
        return JobOutcome::Failed {
            error: format!("{error}"),
            category: FailureCategory::Dio,
        };
    }

    cleanup_after_success(ctx, materialized.as_deref(), &local).await;
    ctx.progress.bytes(total, Some(total));
    ctx.progress.note(&format!(
        "uploaded to {} ({total} bytes)",
        ctx.task.remote_path
    ));
    JobOutcome::Completed
}

/// Ask Dart to copy the SAF URI into our payload directory.
async fn materialize(ctx: &JobCtx) -> Result<Option<PathBuf>, JobOutcome> {
    let Some(uri) = ctx
        .task
        .local_uri
        .as_deref()
        .filter(|uri| !uri.trim().is_empty())
    else {
        return Ok(None);
    };
    let action = PlatformAction::Materialize {
        uri: uri.to_string(),
    };
    match ctx.platform.request(action).await {
        Ok(reply) => {
            let ok = reply.get("ok").and_then(Value::as_bool).unwrap_or(true);
            if !ok {
                return Err(JobOutcome::Failed {
                    error: reply
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("err.unknown|failed to materialise SAF URI")
                        .to_string(),
                    category: FailureCategory::Other,
                });
            }
            let path = reply
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if path.trim().is_empty() {
                return Err(JobOutcome::Failed {
                    error: "err.unknown|failed to materialise SAF URI".to_string(),
                    category: FailureCategory::Other,
                });
            }
            Ok(Some(PathBuf::from(path)))
        }
        Err(error) => Err(JobOutcome::Failed {
            error: platform_error(&error),
            category: FailureCategory::Other,
        }),
    }
}

/// `Materialize` output wins over a stale `local_path`, mirroring Dart's
/// `task.localPath = jitCached`.
fn resolve_local(ctx: &JobCtx, materialized: Option<&Path>) -> Result<PathBuf, JobOutcome> {
    if let Some(path) = materialized {
        return Ok(path.to_path_buf());
    }
    match ctx
        .task
        .local_path
        .as_deref()
        .filter(|path| !path.trim().is_empty())
    {
        Some(path) => Ok(PathBuf::from(path)),
        None => Err(JobOutcome::Failed {
            error: "err.unknown|no local file".to_string(),
            category: FailureCategory::State,
        }),
    }
}

fn missing_file(path: &Path) -> JobOutcome {
    JobOutcome::Failed {
        error: format!("err.cloud.notFound|{}", path.display()),
        category: FailureCategory::State,
    }
}

fn is_too_large(size: u64) -> bool {
    size > MAX_UPLOAD_SIZE
}

/// Read the payload in [`READ_CHUNK`] segments so cancellation is checked and
/// progress is reported during a large copy.
async fn read_payload(ctx: &JobCtx, path: &Path, total: u64) -> Result<Vec<u8>, JobOutcome> {
    let mut file = match fs::File::open(path).await {
        Ok(file) => file,
        Err(_) => return Err(missing_file(path)),
    };
    let mut data = Vec::with_capacity(total as usize);
    loop {
        if ctx.cancelled() {
            return Err(JobOutcome::Cancelled);
        }
        let remaining = total.saturating_sub(data.len() as u64);
        if remaining == 0 {
            break;
        }
        let want = READ_CHUNK.min(remaining as usize);
        let before = data.len();
        data.resize(before + want, 0);
        match file.read(&mut data[before..]).await {
            Ok(0) => {
                data.truncate(before);
                return Err(JobOutcome::Failed {
                    error: format!("err.uploadShortRead|{}", path.display()),
                    category: FailureCategory::Other,
                });
            }
            Ok(read) => data.truncate(before + read),
            Err(error) => {
                return Err(JobOutcome::Failed {
                    error: format!("err.local|read payload: {error}"),
                    category: FailureCategory::Other,
                })
            }
        }
        ctx.progress.bytes(data.len() as u64, Some(total));
    }
    Ok(data)
}

/// Delete queue-owned scratch files once the remote write landed. Failures are
/// intentionally ignored: a leftover temp file must not fail a landed upload
/// (same rationale as the Dart `catch (_) {}` cleanup).
async fn cleanup_after_success(ctx: &JobCtx, materialized: Option<&Path>, local: &Path) {
    if let Some(path) = materialized {
        let _ = fs::remove_file(path).await;
    }
    if local.starts_with(&ctx.paths.payload_dir) {
        let _ = fs::remove_file(local).await;
    }
}

fn remote_parent(remote_path: &str) -> Option<&str> {
    let trimmed = remote_path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => None,
        Some(index) => Some(&trimmed[..index]),
    }
}

fn platform_error(error: &str) -> String {
    if error.contains("timeout") {
        "err.platformTimeout".to_string()
    } else if error.contains("unavailable") || error.contains("channel closed") {
        "err.safUnavailable".to_string()
    } else {
        format!("err.platform|{error}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use crate::{JobPaths, ProgressSink};
    use wdmm_drivers::{
        CloudFileItem, ContentDeliveryKind, DriverError, Result as VfsResult, WdmmVfs,
    };
    use wdmm_queue::model::{CancelToken, QueueTask, TaskTarget};
    use wdmm_storage::LibraryDatabase;

    /// VFS stand-in that records `put_content` and can fail it.
    struct FakeVfs {
        puts: Mutex<Vec<(String, Vec<u8>)>>,
        fail_put: bool,
    }

    impl FakeVfs {
        fn new() -> Self {
            Self {
                puts: Mutex::new(Vec::new()),
                fail_put: false,
            }
        }

        fn failing() -> Self {
            Self {
                puts: Mutex::new(Vec::new()),
                fail_put: true,
            }
        }

        fn puts(&self) -> Vec<(String, Vec<u8>)> {
            self.puts.lock().unwrap().clone()
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
                size: 0,
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

        async fn put_content(&self, path: &str, data: &[u8]) -> VfsResult<()> {
            if self.fail_put {
                return Err(DriverError::Network("put boom".into()));
            }
            self.puts
                .lock()
                .unwrap()
                .push((path.to_string(), data.to_vec()));
            Ok(())
        }

        async fn get_content(&self, _path: &str) -> VfsResult<Vec<u8>> {
            Ok(Vec::new())
        }
    }

    #[derive(Default)]
    struct Recorder {
        bytes: Mutex<Vec<(u64, Option<u64>)>>,
        notes: Mutex<Vec<String>>,
    }

    impl ProgressSink for Recorder {
        fn bytes(&self, done: u64, total: Option<u64>) {
            self.bytes.lock().unwrap().push((done, total));
        }

        fn units(&self, _done: u32, _total: Option<u32>) {}

        fn note(&self, message: &str) {
            self.notes.lock().unwrap().push(message.to_string());
        }
    }

    fn upload_task(id: &str, local_path: Option<&Path>, local_uri: Option<&str>) -> QueueTask {
        let mut task = QueueTask::new(
            id.to_string(),
            TaskKind::Upload,
            "nas".to_string(),
            "acc".to_string(),
            "/Music/Album/up.bin".to_string(),
            "up.bin".to_string(),
            TaskTarget::Public,
            "batch".to_string(),
            0,
        );
        task.local_path = local_path.map(|path| path.display().to_string());
        task.local_uri = local_uri.map(str::to_string);
        task
    }

    fn build_ctx(
        dir: &Path,
        task: QueueTask,
        vfs: Arc<dyn WdmmVfs>,
        platform: crate::PlatformClient,
        progress: Arc<Recorder>,
    ) -> JobCtx {
        let db = Arc::new(LibraryDatabase::open(Path::new(":memory:")).unwrap());
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

    #[tokio::test]
    async fn upload_reads_in_segments_and_puts_full_payload() {
        let dir = tempfile::tempdir().unwrap();
        let data = pattern_data(2 * READ_CHUNK + 7);
        let source = dir.path().join("payload.bin");
        std::fs::write(&source, &data).unwrap();

        let vfs = Arc::new(FakeVfs::new());
        let progress = Arc::new(Recorder::default());
        let ctx = build_ctx(
            dir.path(),
            upload_task("t-up", Some(&source), None),
            vfs.clone(),
            crate::PlatformClient::disabled(),
            progress.clone(),
        );
        let outcome = UploadJob.run(ctx).await;
        assert!(matches!(outcome, JobOutcome::Completed), "{outcome:?}");

        let puts = vfs.puts();
        assert_eq!(puts.len(), 1);
        assert_eq!(puts[0].0, "/Music/Album/up.bin");
        assert_eq!(puts[0].1, data);
        let updates = progress.bytes.lock().unwrap().len();
        assert!(updates >= 3, "expected segmented progress, got {updates}");
        assert_eq!(
            *progress.bytes.lock().unwrap().last().unwrap(),
            (data.len() as u64, Some(data.len() as u64))
        );
        assert!(source.exists(), "user file outside payload_dir must be kept");
    }

    #[tokio::test]
    async fn upload_deletes_payload_dir_scratch_files() {
        let dir = tempfile::tempdir().unwrap();
        let payload_dir = dir.path().join("docs").join("queue_payloads");
        std::fs::create_dir_all(&payload_dir).unwrap();
        let source = payload_dir.join("queued.bin");
        std::fs::write(&source, pattern_data(64)).unwrap();

        let vfs = Arc::new(FakeVfs::new());
        let ctx = build_ctx(
            dir.path(),
            upload_task("t-payload", Some(&source), None),
            vfs,
            crate::PlatformClient::disabled(),
            Arc::new(Recorder::default()),
        );
        assert!(matches!(UploadJob.run(ctx).await, JobOutcome::Completed));
        assert!(!source.exists(), "payload_dir scratch must be cleaned up");
    }

    #[tokio::test]
    async fn upload_materializes_saf_uri_and_cleans_temp() {
        let dir = tempfile::tempdir().unwrap();
        let materialized = dir.path().join("jit.bin");
        let data = pattern_data(4096);
        std::fs::write(&materialized, &data).unwrap();
        let reply_path = materialized.display().to_string();

        let (client, mut rx) = crate::PlatformClient::channel(4);
        let responder = tokio::spawn(async move {
            let call = rx.recv().await.expect("platform call");
            let action = call.action.clone();
            let _ = call
                .reply
                .send(Ok(serde_json::json!({"ok": true, "path": reply_path})));
            action
        });

        let vfs = Arc::new(FakeVfs::new());
        let ctx = build_ctx(
            dir.path(),
            upload_task("t-saf", None, Some("content://docs/1")),
            vfs.clone(),
            client,
            Arc::new(Recorder::default()),
        );
        let outcome = UploadJob.run(ctx).await;
        assert!(matches!(outcome, JobOutcome::Completed), "{outcome:?}");

        match responder.await.unwrap() {
            PlatformAction::Materialize { uri } => assert_eq!(uri, "content://docs/1"),
            other => panic!("expected Materialize, got {other:?}"),
        }
        assert_eq!(vfs.puts()[0].1, data);
        assert!(
            !materialized.exists(),
            "materialised temp must be deleted after success"
        );
    }

    #[tokio::test]
    async fn upload_materialize_failure_skips_put() {
        let dir = tempfile::tempdir().unwrap();
        let (client, mut rx) = crate::PlatformClient::channel(4);
        let responder = tokio::spawn(async move {
            let call = rx.recv().await.expect("platform call");
            let _ = call
                .reply
                .send(Ok(serde_json::json!({"ok": false, "error": "err.readFailed"})));
        });

        let vfs = Arc::new(FakeVfs::new());
        let ctx = build_ctx(
            dir.path(),
            upload_task("t-saf-fail", None, Some("content://docs/1")),
            vfs.clone(),
            client,
            Arc::new(Recorder::default()),
        );
        match UploadJob.run(ctx).await {
            JobOutcome::Failed { error, .. } => assert_eq!(error, "err.readFailed"),
            other => panic!("expected failure, got {other:?}"),
        }
        responder.await.unwrap();
        assert!(vfs.puts().is_empty());
    }

    #[tokio::test]
    async fn upload_missing_file_fails_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.bin");
        let ctx = build_ctx(
            dir.path(),
            upload_task("t-missing", Some(&missing), None),
            Arc::new(FakeVfs::new()),
            crate::PlatformClient::disabled(),
            Arc::new(Recorder::default()),
        );
        match UploadJob.run(ctx).await {
            JobOutcome::Failed { error, category } => {
                assert!(error.starts_with("err.cloud.notFound|"), "{error}");
                assert_eq!(category, FailureCategory::State);
            }
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn upload_empty_file_fails_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty.bin");
        std::fs::write(&empty, b"").unwrap();
        let ctx = build_ctx(
            dir.path(),
            upload_task("t-empty", Some(&empty), None),
            Arc::new(FakeVfs::new()),
            crate::PlatformClient::disabled(),
            Arc::new(Recorder::default()),
        );
        match UploadJob.run(ctx).await {
            JobOutcome::Failed { error, .. } => {
                assert!(error.starts_with("err.cloud.notFound|"))
            }
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn upload_cancel_skips_put_and_keeps_source() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("payload.bin");
        std::fs::write(&source, pattern_data(128)).unwrap();
        let vfs = Arc::new(FakeVfs::new());
        let ctx = build_ctx(
            dir.path(),
            upload_task("t-cancel", Some(&source), None),
            vfs.clone(),
            crate::PlatformClient::disabled(),
            Arc::new(Recorder::default()),
        );
        ctx.cancel.cancel();
        assert!(matches!(UploadJob.run(ctx).await, JobOutcome::Cancelled));
        assert!(vfs.puts().is_empty());
        assert!(source.exists());
    }

    #[tokio::test]
    async fn upload_put_failure_is_dio_and_keeps_source() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("payload.bin");
        std::fs::write(&source, pattern_data(128)).unwrap();
        let ctx = build_ctx(
            dir.path(),
            upload_task("t-put-fail", Some(&source), None),
            Arc::new(FakeVfs::failing()),
            crate::PlatformClient::disabled(),
            Arc::new(Recorder::default()),
        );
        match UploadJob.run(ctx).await {
            JobOutcome::Failed { error, category } => {
                assert!(error.contains("put boom"), "{error}");
                assert_eq!(category, FailureCategory::Dio);
            }
            other => panic!("expected failure, got {other:?}"),
        }
        assert!(source.exists(), "failed upload keeps the payload");
    }

    #[test]
    fn upload_size_cap_and_parent_helpers() {
        assert!(!is_too_large(MAX_UPLOAD_SIZE));
        assert!(is_too_large(MAX_UPLOAD_SIZE + 1));
        assert_eq!(remote_parent("/Music/Album/up.bin"), Some("/Music/Album"));
        assert_eq!(remote_parent("/up.bin"), None);
        assert_eq!(remote_parent("up.bin"), None);
    }
}
