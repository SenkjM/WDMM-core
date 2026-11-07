//! Post-terminal follow-up chains (contract T6a).
//!
//! Three chains are declared by the executors' own outcomes/results instead of
//! by the jobs crate (which cannot depend on the queue's command surface):
//!
//! * `MangaPack` → `Partial{result}` carrying the finished CBZ path and the
//!   upload coordinates → enqueue one `Upload` for that archive.
//! * `Upload` → `Completed` with a `wmm-pack-origin:<id>` marker → enqueue the
//!   `MangaScrape` for the just-uploaded CBZ, seeded from the manga row (or the
//!   file name when the row is gone).
//! * `DownloadCache`/`DownloadPublic` → `Completed` inside a CUE group → once
//!   every member of the group is terminal and at least one succeeded, enqueue
//!   the sheet `Scan`.
//!
//! Every enqueue goes through [`QueueHandle::enqueue`] so dedupe, batch
//! bookkeeping and events behave exactly like a user-initiated command.

use serde_json::{json, Value};

use crate::handle::QueueHandle;
use crate::model::{BatchOrigin, QueueTask, TaskKind, TaskStatus, TaskTarget};
use crate::now_ms;
use crate::queue::EnqueueRequest;
use crate::store::MangaDraft;

const PACK_ORIGIN_PREFIX: &str = "wmm-pack-origin:";
const PACK_DELETE_PREFIX: &str = "wmm-pack-del:";
const CUE_GROUP_PREFIX: &str = "cue";

/// Runs the chain attached to a task that just reached a terminal state.
pub(crate) fn after_terminal(handle: &QueueHandle, task: &QueueTask) {
    match (task.kind, task.status) {
        (TaskKind::MangaPack, TaskStatus::Partial) => follow_pack_upload(handle, task),
        (TaskKind::Upload, TaskStatus::Completed) => follow_pack_scrape(handle, task),
        (TaskKind::DownloadCache | TaskKind::DownloadPublic, TaskStatus::Completed) => {
            follow_cue_scan(handle, task)
        }
        _ => {}
    }
}

// --------------------------------------------------------------- pack→upload

fn follow_pack_upload(handle: &QueueHandle, pack: &QueueTask) {
    let Some(result) = pack.result_json.as_deref() else {
        return;
    };
    let Ok(value) = serde_json::from_str::<Value>(result) else {
        return;
    };
    let field = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };

    let (Some(archive), Some(remote_path), Some(source_name)) = (
        field("archive"),
        field("upload_remote_path"),
        field("upload_source_name"),
    ) else {
        return;
    };
    let display_name = field("display_name").unwrap_or_else(|| pack.file_name.clone());

    let account_id = match handle.inner.store.account_id_by_display_name(&source_name) {
        Ok(Some(account_id)) => account_id,
        Ok(None) => {
            handle.record_store_error(format!(
                "pack upload skipped: unknown account '{source_name}'"
            ));
            return;
        }
        Err(error) => {
            handle.record_store_error(format!("pack upload account lookup failed: {error}"));
            return;
        }
    };

    let origin = handle
        .batches()
        .into_iter()
        .find(|batch| batch.id == pack.batch_id)
        .map(|batch| batch.origin)
        .unwrap_or(BatchOrigin::User);

    let mut req = EnqueueRequest::new(TaskKind::Upload, account_id, remote_path, display_name);
    req.target = TaskTarget::Public;
    req.source_name = source_name;
    req.batch_id = Some(pack.batch_id.clone());
    req.batch_origin = origin;
    req.batch_label = None;
    req.priority = pack.priority;
    req.cache_group_id = field("cache_group_id");
    req.queued_while_foreground = pack.queued_while_foreground;
    req.dedupe = true;

    match handle.enqueue(req) {
        Ok(enqueued) => note_archive(handle, &enqueued.task_id, &archive),
        Err(error) => handle.record_store_error(format!("pack upload enqueue failed: {error}")),
    }
}

/// The archive path is durable task data (the upload executor reads it and owns
/// its lifetime), never a payload sidecar.
fn note_archive(handle: &QueueHandle, task_id: &str, archive: &str) {
    let updated = {
        let Ok(mut state) = handle.inner.state.lock() else {
            return;
        };
        state.note_local_path(task_id, archive, now_ms())
    };
    if let Some(task) = updated {
        handle.persist_task(&task);
    }
}

// --------------------------------------------------------------- upload→scrape

fn follow_pack_scrape(handle: &QueueHandle, upload: &QueueTask) {
    let Some(marker) = upload.cache_group_id.as_deref() else {
        return;
    };
    let Some(rest) = marker.strip_prefix(PACK_ORIGIN_PREFIX) else {
        return;
    };
    let Some(origin_id) = parse_marker_id(rest) else {
        return;
    };

    let already_queued = handle.tasks().into_iter().any(|task| {
        task.remote_path == upload.remote_path
            && task.kind == TaskKind::MangaScrape
            && scrape_marker_id(&task).as_deref() == Some(origin_id.as_str())
            && (!task.status.is_terminal() || task.status == TaskStatus::Completed)
    });
    if already_queued {
        return;
    }

    let draft = handle
        .inner
        .store
        .manga_draft(&origin_id)
        .ok()
        .flatten()
        .unwrap_or_else(|| MangaDraft {
            title: strip_extension(&upload.file_name),
            ..MangaDraft::default()
        });

    let payload = json!({
        "draft": {
            "title": draft.title,
            "author": draft.author,
            "series": draft.series,
            "volume": draft.volume,
            "chapter": draft.chapter,
            "format": "cbz",
        }
    })
    .to_string();

    let mut req = EnqueueRequest::new(
        TaskKind::MangaScrape,
        upload.account_id.clone(),
        upload.remote_path.clone(),
        upload.file_name.clone(),
    );
    req.target = TaskTarget::Cache;
    req.source_name = upload.source_name.clone();
    req.batch_id = Some(upload.batch_id.clone());
    req.priority = upload.priority;
    req.cache_group_id = Some(format!(
        "{PACK_DELETE_PREFIX}{}",
        serde_json::to_string(&origin_id).unwrap_or_else(|_| format!("\"{origin_id}\""))
    ));
    req.payload = Some(payload);
    req.queued_while_foreground = upload.queued_while_foreground;
    req.dedupe = true;

    // The scrape must never inherit the archive path: `local_path` would make
    // the executor try to open a file the upload just consumed.
    if let Err(error) = handle.enqueue(req) {
        handle.record_store_error(format!("pack scrape enqueue failed: {error}"));
    }
}

fn scrape_marker_id(task: &QueueTask) -> Option<String> {
    let marker = task.cache_group_id.as_deref()?;
    parse_marker_id(marker.strip_prefix(PACK_DELETE_PREFIX)?)
}

/// Accepts both wire shapes seen in the wild: a JSON string (`"id"`, written by
/// the Dart helper) and a bare id (written by `wdmm-jobs`'s pack result).
fn parse_marker_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let id = serde_json::from_str::<String>(trimmed)
        .ok()
        .unwrap_or_else(|| trimmed.trim_matches('"').to_string());
    let id = id.trim().to_string();
    if id.is_empty() {
        None
    } else {
        Some(id)
    }
}

fn strip_extension(file_name: &str) -> String {
    match file_name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem.to_string(),
        _ => file_name.to_string(),
    }
}

// ------------------------------------------------------------------ cue→scan

fn follow_cue_scan(handle: &QueueHandle, download: &QueueTask) {
    let Some(group) = download.cache_group_id.as_deref() else {
        return;
    };
    // CUE groups are encoded as `cue\0<source>\0<normalized cue path>`.
    let Some(rest) = group.strip_prefix(CUE_GROUP_PREFIX) else {
        return;
    };
    if !rest.starts_with('\u{0}') {
        return;
    }

    let members: Vec<QueueTask> = handle
        .tasks()
        .into_iter()
        .filter(|task| task.cache_group_id.as_deref() == Some(group))
        .collect();
    if members.iter().any(|task| !task.status.is_terminal()) {
        return;
    }
    if !members.iter().any(|task| task.status.is_success()) {
        return;
    }

    let parts: Vec<&str> = group.split('\u{0}').collect();
    if parts.len() < 3 {
        return;
    }
    let source_name = parts[1];
    let cue_path = parts[2];
    if cue_path.is_empty() {
        return;
    }

    let already_queued = handle.tasks().into_iter().any(|task| {
        task.kind == TaskKind::Scan
            && task.remote_path == cue_path
            && (!task.status.is_terminal() || task.status == TaskStatus::Completed)
            && payload_lists_cue(handle, &task, cue_path)
    });
    if already_queued {
        return;
    }

    let file_name = cue_path
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(cue_path)
        .to_string();
    let payload = json!([{ "path": cue_path, "name": file_name, "cue": true }]).to_string();

    let batch_id = members
        .iter()
        .map(|task| task.batch_id.clone())
        .find(|batch_id| !batch_id.is_empty())
        .unwrap_or_else(|| download.batch_id.clone());

    let mut req = EnqueueRequest::new(
        TaskKind::Scan,
        download.account_id.clone(),
        cue_path,
        file_name,
    );
    req.target = TaskTarget::Cache;
    req.source_name = source_name.to_string();
    req.batch_id = Some(batch_id);
    req.priority = download.priority;
    req.cache_group_id = Some(group.to_string());
    req.payload = Some(payload);
    req.queued_while_foreground = download.queued_while_foreground;
    req.dedupe = true;

    if let Err(error) = handle.enqueue(req) {
        handle.record_store_error(format!("cue scan enqueue failed: {error}"));
    }
}

fn payload_lists_cue(handle: &QueueHandle, task: &QueueTask, cue_path: &str) -> bool {
    let Some(raw) = handle.inner.store.read_payload(&task.id).ok().flatten() else {
        return false;
    };
    let Ok(items) = serde_json::from_str::<Vec<Value>>(&raw) else {
        return false;
    };
    items.iter().any(|item| {
        item.get("path")
            .and_then(Value::as_str)
            .map(|path| path == cue_path)
            .unwrap_or(false)
    })
}
