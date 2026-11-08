//! Manual library ingest through the Rust executors.
//!
//! The library screens used to read tags through a byte-range probe and then
//! rebuild the row, the thumbnail and the CUE slices in Dart
//! (`LibraryService.ingestFromTags` / `ingestDownloaded`) — a second ingest next
//! to `wdmm-jobs::ingest`. This module exposes that single executor to the FFI
//! so a manual ingest (redirect, retag-after-cache, or a playlist row going
//! remote) runs exactly the code a queue scan runs.

use std::sync::Arc;

use serde_json::{json, Value};

use wdmm_jobs::{ingest, JobCtx, NoProgress, PlatformClient};
use wdmm_queue::model::{CancelToken, QueueTask, TaskKind, TaskTarget};

use crate::queue_api::ctl_paths;
use crate::{create_account_driver, try_global_library_db};

/// Ingest one file and return `{ok, rows, title, cover}`.
///
/// `args` is `{"account_id", "remote_path", "file_name"?, "source_name"?,
/// "local_path"?}`. With a `local_path` the tags come from the downloaded file
/// (Dart `ingestDownloaded`); without it they are probed through the driver
/// (Dart `ingestFromTags`), including CUE sheets.
pub(crate) fn library_ingest(args: &str) -> Result<Value, String> {
    let value: Value =
        serde_json::from_str(args).map_err(|error| format!("badArgs|{error}"))?;
    let account_id = value
        .get("account_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if account_id.is_empty() {
        return Err("missing account_id".into());
    }
    let remote_path = value
        .get("remote_path")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if remote_path.is_empty() {
        return Err("missing remote_path".into());
    }
    let file_name = value
        .get("file_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let local_path = value
        .get("local_path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(str::to_string);

    let Some(db) = try_global_library_db().cloned() else {
        return Err("err.notInitialized|call wdmm_library_init first".into());
    };
    let Some(paths) = ctl_paths() else {
        return Err("err.notInitialized|call wdmm_queue_set_paths first".into());
    };
    let source_name = match value
        .get("source_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        Some(name) => name.to_string(),
        None => db
            .get_account(&account_id)
            .ok()
            .flatten()
            .map(|account| account.display_name)
            .unwrap_or_else(|| account_id.clone()),
    };
    let vfs = create_account_driver(&account_id)?;

    let now = chrono::Utc::now().timestamp();
    let kind = if local_path.is_some() {
        TaskKind::DownloadCache
    } else {
        TaskKind::Scan
    };
    let mut task = QueueTask::new(
        format!("ingest-{now}"),
        kind,
        source_name,
        account_id,
        remote_path,
        file_name,
        TaskTarget::Cache,
        String::new(),
        now,
    );
    task.local_path = local_path.clone();

    let outcome = crate::rt().block_on(async {
        // Built inside the runtime on purpose: `PlatformClient::disabled()`
        // spawns its drain task, so pressing it together on the calling thread
        // (a worker isolate here) panics with "there is no reactor running".
        let ctx = JobCtx {
            task,
            cancel: CancelToken::new(),
            progress: Arc::new(NoProgress),
            platform: Arc::new(PlatformClient::disabled()),
            db,
            vfs,
            paths,
        };
        match local_path {
            Some(_) => ingest::ingest_downloaded(&ctx).await,
            None => ingest::ingest_remote_file(&ctx).await,
        }
    })?;
    Ok(json!({
        "ok": true,
        "rows": outcome.rows,
        "title": outcome.title,
        "cover": outcome.cover,
    }))
}
