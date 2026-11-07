//! FFI surface for the Rust-owned task queue (contract §5).
//!
//! Every symbol here is a thin JSON in / JSON out edge over [`QueueHandle`]:
//! Dart issues commands, reads the authoritative snapshot, and drains events by
//! sequence number. No queue state lives on the Dart side.
//!
//! * `wdmm_queue_set_paths` — one-time wiring (DB store + job adapter + pump).
//! * `wdmm_queue_cmd` — mutations (`enqueue*`, `cancel`, `retry`, `remove`,
//!   `clear_*`, settings, maintenance, `platform_reply`, `fgs_timeout`).
//! * `wdmm_queue_query` — reads (`snapshot`, `events`, `rows`, `task`,
//!   `by_remote`, `stats`).
//! * `wdmm_queue_wait` — blocking wait for a terminal task (called from
//!   `Isolate.run`).
//! * `wdmm_queue_platform_result` — answer to a `platform_request` event.

use std::os::raw::c_char;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use serde_json::{json, Map, Value};

use wdmm_jobs::JobPaths;
use wdmm_queue::{
    new_id, BatchOrigin, DbStore, EnqueueRequest, PlatformResult, QueueError, QueueHandle, TaskKind,
    TaskStatus, TaskStore, TaskTarget, WaitOutcome,
};

use crate::queue_runner::production_runner;

/// Process-wide context installed by [`wdmm_queue_set_paths`].
struct QueueCtl {
    paths: JobPaths,
}

static CTL: OnceLock<QueueCtl> = OnceLock::new();

// ── Helpers ───────────────────────────────────────────────────────────────────

fn handle() -> Result<QueueHandle, String> {
    QueueHandle::global()
        .ok_or_else(|| "err.notInitialized|call wdmm_queue_set_paths first".to_string())
}

fn fail(message: impl Into<String>) -> crate::FfiString {
    let message = message.into();
    crate::FfiString::from(json!({ "ok": false, "error": message }).to_string())
}

fn queue_err(error: QueueError) -> String {
    format!("err.queue|{error}")
}

fn req_str<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value[key]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("err.badJson|missing '{key}'"))
}

fn opt_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value[*key].as_str().map(str::to_string))
        .filter(|s| !s.is_empty())
}

fn opt_bool(value: &Value, keys: &[&str]) -> Option<bool> {
    keys.iter().find_map(|key| value[*key].as_bool())
}

fn snapshot_json(handle: &QueueHandle, extra: Vec<(&'static str, Value)>) -> Value {
    let mut map = match serde_json::to_value(handle.snapshot()) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    };
    map.insert("ok".to_string(), Value::Bool(true));
    for (key, value) in extra {
        map.insert(key.to_string(), value);
    }
    Value::Object(map)
}

fn events_json(page: wdmm_queue::EventsPage) -> Value {
    let mut map = match serde_json::to_value(page) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    };
    map.insert("ok".to_string(), Value::Bool(true));
    Value::Object(map)
}

fn clamp_concurrency(n: u64) -> usize {
    (n as usize).clamp(1, 8)
}

// ── Initialisation ────────────────────────────────────────────────────────────

/// Wire the queue singleton: store, executor adapter, pump and the one-time
/// purge of the legacy KV queue rows (contract §A5).
///
/// Idempotent — a second call returns the already-installed revision.
#[no_mangle]
pub extern "C" fn wdmm_queue_set_paths(
    app_docs: *const c_char,
    cache_root: *const c_char,
) -> crate::FfiString {
    let Some(app_docs) = crate::ptr_to_str(app_docs) else {
        return fail("err.badArgs|null app_docs");
    };
    let Some(cache_root) = crate::ptr_to_str(cache_root) else {
        return fail("err.badArgs|null cache_root");
    };
    match init_queue(app_docs, cache_root) {
        Ok(value) => crate::FfiString::from(value.to_string()),
        Err(error) => fail(error),
    }
}

fn init_queue(app_docs: &str, cache_root: &str) -> Result<Value, String> {
    if let Some(handle) = QueueHandle::global() {
        return Ok(json!({ "ok": true, "already": true, "rev": handle.rev() }));
    }

    let paths = JobPaths::new(app_docs, cache_root);
    let db = Arc::clone(crate::global_library_db());
    let store: Arc<dyn TaskStore> =
        Arc::new(DbStore::new(Arc::clone(&db), paths.payload_dir.clone()));
    let runner = Arc::new(production_runner(Arc::clone(&db), paths.clone()));

    // Construct inside the runtime so `kick()` (called by every mutation on the
    // FFI thread, which runs inside `rt().block_on`) can spawn the pump.
    let handle = crate::rt()
        .block_on(async { QueueHandle::try_new(store, runner) })
        .map_err(|e| format!("err.queueInit|{e}"))?;

    QueueHandle::install_global(handle.clone());
    let _ = CTL.set(QueueCtl {
        paths: paths.clone(),
    });

    // User ruled the legacy KV queue rows are discarded (contract §3/§A5).
    let legacy_purged = match crate::global_library_db().queue_purge_legacy_kv() {
        Ok(count) => count,
        Err(error) => {
            eprintln!("[queue] legacy KV purge failed: {error}");
            0
        }
    };

    // One pump to pick up rows recovered from the DB; later mutations restart it
    // through `QueueHandle::kick` after it exits while idle.
    let pump_handle = handle.clone();
    drop(crate::rt().spawn(async move { pump_handle.pump().await }));

    Ok(json!({
        "ok": true,
        "rev": handle.rev(),
        "legacy_purged": legacy_purged,
    }))
}

// ── Commands ──────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn wdmm_queue_cmd(cmd_json: *const c_char) -> crate::FfiString {
    let Some(raw) = crate::ptr_to_str(cmd_json) else {
        return fail("err.badArgs|null json");
    };
    let value: Value = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(error) => return fail(format!("err.badJson|{error}")),
    };
    let Some(cmd) = value["cmd"].as_str() else {
        return fail("err.badJson|missing 'cmd'");
    };
    // Mutations reach `kick()`, which needs the ambient runtime (see
    // `QueueHandle::kick`), so the whole dispatch runs inside it.
    match crate::rt().block_on(async { dispatch_cmd(cmd, &value) }) {
        Ok(result) => crate::FfiString::from(result.to_string()),
        Err(error) => fail(error),
    }
}

fn dispatch_cmd(cmd: &str, value: &Value) -> Result<Value, String> {
    let handle = handle()?;
    match cmd {
        "enqueue" => cmd_enqueue(value, None),

        "enqueue_batch" => cmd_enqueue_batch(value),

        // Named convenience forms from contract §5: the suffix is the `TaskKind`.
        other if other.starts_with("enqueue_") => {
            let suffix = other.trim_start_matches("enqueue_");
            let kind = TaskKind::from_str(suffix)
                .ok_or_else(|| format!("err.badJson|unknown kind '{suffix}'"))?;
            cmd_enqueue(value, Some(kind))
        }

        "cancel" => {
            let task_id = req_str(value, "task_id")?;
            let changed = handle.cancel(task_id).map_err(queue_err)?;
            Ok(snapshot_json(&handle, vec![("changed", json!(changed))]))
        }
        "cancel_all" => {
            let changed = handle.cancel_all().map_err(queue_err)?;
            Ok(snapshot_json(&handle, vec![("changed", json!(changed))]))
        }
        "retry" => {
            let task_id = req_str(value, "task_id")?;
            let task = handle.retry(task_id).map_err(queue_err)?;
            Ok(snapshot_json(&handle, vec![("task_id", json!(task.id))]))
        }
        "retry_all" => {
            let changed = handle.retry_all().map_err(queue_err)?;
            Ok(snapshot_json(&handle, vec![("changed", json!(changed))]))
        }
        "remove" => {
            let task_id = req_str(value, "task_id")?;
            let removed = handle.remove(task_id).map_err(queue_err)?;
            Ok(snapshot_json(&handle, vec![("removed", json!(removed))]))
        }
        "clear_completed" => {
            let removed = handle.clear_completed().map_err(queue_err)?;
            Ok(snapshot_json(&handle, vec![("removed", json!(removed))]))
        }
        "clear_all" => {
            let removed = handle.clear_all().map_err(queue_err)?;
            Ok(snapshot_json(&handle, vec![("removed", json!(removed))]))
        }
        "set_notifications" => {
            let enabled = opt_bool(value, &["enabled", "value"])
                .ok_or_else(|| "err.badJson|missing 'enabled'".to_string())?;
            handle.set_notifications(enabled);
            Ok(snapshot_json(&handle, vec![("enabled", json!(enabled))]))
        }
        "set_keep_alive" => {
            let enabled = opt_bool(value, &["enabled", "value"])
                .ok_or_else(|| "err.badJson|missing 'enabled'".to_string())?;
            handle.set_keep_alive(enabled);
            Ok(snapshot_json(&handle, vec![("enabled", json!(enabled))]))
        }
        "set_concurrency" => {
            let n = value["concurrency"]
                .as_u64()
                .or_else(|| value["n"].as_u64())
                .ok_or_else(|| "err.badJson|missing 'concurrency'".to_string())?;
            let concurrency = clamp_concurrency(n);
            handle.set_concurrency(concurrency).map_err(queue_err)?;
            Ok(snapshot_json(&handle, vec![("concurrency", json!(concurrency))]))
        }
        // Convenience alias for the three settings above (older briefs called
        // this `set_policy`; the queue owns no persisted policy object).
        "set_policy" => cmd_set_policy(&handle, value),
        "platform_reply" => {
            let req_id = req_str(value, "req_id")?;
            let delivered = handle.platform_reply(req_id, platform_result_from_json(value));
            Ok(snapshot_json(&handle, vec![("delivered", json!(delivered))]))
        }
        "fgs_timeout" => {
            handle.fgs_timeout().map_err(queue_err)?;
            Ok(snapshot_json(&handle, Vec::new()))
        }
        "shutdown" => {
            handle.shutdown();
            Ok(snapshot_json(&handle, Vec::new()))
        }
        "cleanup_stale_parts" => cmd_cleanup_stale_parts(&handle, value),
        "invalidate_missing_completed" => cmd_invalidate_missing_completed(&handle),
        "replace_public" => cmd_replace_public(value),
        other => Err(format!("err.unknownCmd|{other}")),
    }
}

fn cmd_set_policy(handle: &QueueHandle, value: &Value) -> Result<Value, String> {
    let mut applied: Vec<&str> = Vec::new();
    if let Some(enabled) = value.get("notifications").and_then(Value::as_bool) {
        handle.set_notifications(enabled);
        applied.push("notifications");
    }
    if let Some(enabled) = value.get("keep_alive").and_then(Value::as_bool) {
        handle.set_keep_alive(enabled);
        applied.push("keep_alive");
    }
    if let Some(n) = value.get("concurrency").and_then(Value::as_u64) {
        handle
            .set_concurrency(clamp_concurrency(n))
            .map_err(queue_err)?;
        applied.push("concurrency");
    }
    Ok(snapshot_json(handle, vec![("applied", json!(applied))]))
}

// ── Enqueue ───────────────────────────────────────────────────────────────────

fn enqueue_request(value: &Value, default_kind: Option<TaskKind>) -> Result<EnqueueRequest, String> {
    let kind = match default_kind {
        Some(kind) => kind,
        None => {
            let raw = req_str(value, "kind")?;
            TaskKind::from_str(raw).ok_or_else(|| format!("err.badJson|unknown kind '{raw}'"))?
        }
    };

    let account_id = opt_string(value, &["account_id", "account"]).unwrap_or_default();
    let remote_path = opt_string(value, &["remote_path", "remote"]).unwrap_or_default();
    let file_name = opt_string(value, &["file_name", "file", "name"]).unwrap_or_default();
    let source_name = opt_string(value, &["source_name", "source"])
        .unwrap_or_else(|| account_id.clone());

    let mut req = EnqueueRequest::new(kind, account_id, remote_path, file_name);
    req.source_name = source_name;
    req.target = match opt_string(value, &["target"]).as_deref() {
        Some("public") | Some("Public") => TaskTarget::Public,
        Some("cache") | Some("Cache") => TaskTarget::Cache,
        Some(other) => return Err(format!("err.badJson|unknown target '{other}'")),
        None => req.target,
    };
    req.batch_id = opt_string(value, &["batch_id", "batch"]);
    if let Some(origin) = opt_string(value, &["batch_origin"]).as_deref() {
        req.batch_origin = match origin {
            "user" => BatchOrigin::User,
            "auto_backfill" | "autoBackfill" => BatchOrigin::AutoBackfill,
            "system" => BatchOrigin::System,
            other => return Err(format!("err.badJson|unknown batch_origin '{other}'")),
        };
    }
    req.batch_label = opt_string(value, &["batch_label", "label"]);
    req.priority = value["priority"].as_i64().unwrap_or(0) as i32;
    req.cache_group_id = opt_string(value, &["cache_group_id", "cache_group"]);
    req.payload = match &value["payload"] {
        Value::Null => None,
        Value::String(raw) => Some(raw.clone()),
        other => Some(other.to_string()),
    };
    req.remote_size = value["remote_size"].as_u64();
    req.remote_modified = value["remote_modified"].as_i64();
    req.queued_while_foreground = opt_bool(value, &["queued_while_foreground"]).unwrap_or(false);
    req.dedupe = opt_bool(value, &["dedupe"]).unwrap_or(true);
    req.expected_total = value["expected_total"].as_u64().map(|n| n as u32);
    req.id = opt_string(value, &["task_id", "id"]);
    Ok(req)
}

fn enqueue_result_json(handle: &QueueHandle, result: wdmm_queue::EnqueueResult) -> Value {
    snapshot_json(
        handle,
        vec![
            ("task_id", json!(result.task_id)),
            ("deduped", json!(result.deduped)),
            ("batch_id", json!(result.batch_id)),
        ],
    )
}

fn cmd_enqueue(value: &Value, default_kind: Option<TaskKind>) -> Result<Value, String> {
    let handle = handle()?;
    let req = enqueue_request(value, default_kind)?;
    let result = handle.enqueue(req).map_err(queue_err)?;
    Ok(enqueue_result_json(&handle, result))
}

fn cmd_enqueue_batch(value: &Value) -> Result<Value, String> {
    let handle = handle()?;
    let tasks = value["tasks"]
        .as_array()
        .filter(|tasks| !tasks.is_empty())
        .ok_or_else(|| "err.badJson|missing 'tasks'".to_string())?;
    let batch_id = opt_string(value, &["batch_id", "batch"]).unwrap_or_else(new_id);
    let batch_label = opt_string(value, &["batch_label", "label"]);
    let batch_origin = match opt_string(value, &["batch_origin"]).as_deref() {
        Some("auto_backfill") | Some("autoBackfill") => BatchOrigin::AutoBackfill,
        Some("system") => BatchOrigin::System,
        _ => BatchOrigin::User,
    };
    let expected_total = value["expected_total"]
        .as_u64()
        .map(|n| n as u32)
        .unwrap_or(tasks.len() as u32);

    let mut task_ids = Vec::with_capacity(tasks.len());
    let mut deduped = 0usize;
    for item in tasks {
        let mut req = enqueue_request(item, None)?;
        req.batch_id = Some(batch_id.clone());
        req.batch_origin = batch_origin;
        req.batch_label = batch_label.clone();
        req.expected_total = Some(expected_total);
        let result = handle.enqueue(req).map_err(queue_err)?;
        if result.deduped {
            deduped += 1;
        }
        task_ids.push(result.task_id);
    }

    Ok(snapshot_json(
        &handle,
        vec![
            ("batch_id", json!(batch_id)),
            ("task_ids", json!(task_ids)),
            ("deduped", json!(deduped)),
        ],
    ))
}

/// Atomic `blockingPublicDuplicate` + `requeue` (contract §7): drop any task at
/// the same remote that also targets the public tree, then enqueue fresh.
fn cmd_replace_public(value: &Value) -> Result<Value, String> {
    let handle = handle()?;
    let mut req = enqueue_request(value, Some(TaskKind::DownloadPublic))?;
    req.target = TaskTarget::Public;
    req.dedupe = false;

    let mut replaced = 0usize;
    for task in handle.by_remote(&req.account_id, &req.remote_path) {
        if task.target == TaskTarget::Public && handle.remove(&task.id).unwrap_or(false) {
            replaced += 1;
        }
    }

    let result = handle.enqueue(req).map_err(queue_err)?;
    Ok(snapshot_json(
        &handle,
        vec![
            ("task_id", json!(result.task_id)),
            ("deduped", json!(result.deduped)),
            ("batch_id", json!(result.batch_id)),
            ("replaced", json!(replaced)),
        ],
    ))
}

// ── Maintenance commands ──────────────────────────────────────────────────────

/// Drop `.part` files that no in-flight task owns (contract §7/T7 semantics):
/// keep files of pending/active tasks, delete those older than `max_age_hours`,
/// then evict the oldest until the total fits `max_bytes`.
fn cmd_cleanup_stale_parts(handle: &QueueHandle, value: &Value) -> Result<Value, String> {
    let max_age_hours = value["max_age_hours"].as_u64().unwrap_or(72);
    let max_bytes = value["max_bytes"]
        .as_u64()
        .or_else(|| value["max_bytes_mb"].as_u64().map(|mb| mb * 1024 * 1024))
        .unwrap_or(512 * 1024 * 1024);

    let Some(ctl) = CTL.get() else {
        return Ok(snapshot_json(
            handle,
            vec![("removed", json!(0)), ("freed_bytes", json!(0))],
        ));
    };

    let alive: Vec<String> = handle
        .tasks()
        .into_iter()
        .filter(|task| matches!(task.status, TaskStatus::Pending | TaskStatus::Active))
        .map(|task| task.id)
        .collect();

    let mut roots = vec![ctl.paths.cache_root.clone()];
    if let Some(parent) = ctl.paths.cache_root.parent() {
        roots.push(parent.join("public_dl"));
    }

    let mut files: Vec<(PathBuf, SystemTime, u64)> = Vec::new();
    for root in &roots {
        collect_parts(root, 0, &alive, &mut files);
    }

    let now = SystemTime::now();
    let max_age = Duration::from_secs(max_age_hours.saturating_mul(3600));
    let mut removed = 0u64;
    let mut freed_bytes = 0u64;

    let mut kept: Vec<(PathBuf, SystemTime, u64)> = Vec::new();
    for (path, modified, size) in files {
        let too_old = now.duration_since(modified).map(|age| age > max_age).unwrap_or(false);
        if too_old && std::fs::remove_file(&path).is_ok() {
            removed += 1;
            freed_bytes += size;
        } else {
            kept.push((path, modified, size));
        }
    }

    let mut total: u64 = kept.iter().map(|(_, _, size)| *size).sum();
    if total > max_bytes {
        kept.sort_by_key(|(_, modified, _)| *modified);
        for (path, _, size) in kept {
            if total <= max_bytes {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(size);
                removed += 1;
                freed_bytes += size;
            }
        }
    }

    Ok(snapshot_json(
        handle,
        vec![
            ("removed", json!(removed)),
            ("freed_bytes", json!(freed_bytes)),
        ],
    ))
}

fn collect_parts(
    dir: &Path,
    depth: u8,
    alive: &[String],
    out: &mut Vec<(PathBuf, SystemTime, u64)>,
) {
    if depth > 3 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            collect_parts(&path, depth + 1, alive, out);
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("part") {
            continue;
        }
        let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("");
        if alive.iter().any(|id| name.contains(id)) {
            continue;
        }
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        out.push((path, modified, metadata.len()));
    }
}

/// Drop completed cache tasks whose file is gone (Dart
/// `TaskQueueService.invalidateMissingCompleted`).
fn cmd_invalidate_missing_completed(handle: &QueueHandle) -> Result<Value, String> {
    let mut removed = 0u64;
    for task in handle.tasks() {
        if task.status != TaskStatus::Completed || task.target == TaskTarget::Public {
            continue;
        }
        let missing = task
            .local_path
            .as_deref()
            .map(|path| !Path::new(path).exists())
            .unwrap_or(true);
        if missing && handle.remove(&task.id).unwrap_or(false) {
            removed += 1;
        }
    }
    Ok(snapshot_json(handle, vec![("removed", json!(removed))]))
}

// ── Queries ───────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn wdmm_queue_query(query_json: *const c_char) -> crate::FfiString {
    let Some(raw) = crate::ptr_to_str(query_json) else {
        return fail("err.badArgs|null json");
    };
    let value: Value = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(error) => return fail(format!("err.badJson|{error}")),
    };
    let Some(query) = value["query"].as_str().or_else(|| value["q"].as_str()) else {
        return fail("err.badJson|missing 'query'");
    };
    match dispatch_query(query, &value) {
        Ok(result) => crate::FfiString::from(result.to_string()),
        Err(error) => fail(error),
    }
}

fn dispatch_query(query: &str, value: &Value) -> Result<Value, String> {
    let handle = handle()?;
    match query {
        "snapshot" => Ok(snapshot_json(&handle, Vec::new())),
        "events" => {
            let since_seq = value["since_seq"].as_u64().unwrap_or(0);
            Ok(events_json(handle.events(since_seq)))
        }
        "rows" => Ok(snapshot_json(
            &handle,
            vec![("groups", serde_json::to_value(handle.rows()).unwrap_or_else(|_| json!([])))],
        )),
        "task" => {
            let task_id = req_str(value, "task_id")?;
            Ok(snapshot_json(
                &handle,
                vec![("task", serde_json::to_value(handle.task(task_id)).unwrap_or(Value::Null))],
            ))
        }
        "by_remote" => {
            let account_id = req_str(value, "account_id")?;
            let remote_path = value["remote_path"].as_str().unwrap_or_default();
            Ok(snapshot_json(
                &handle,
                vec![("tasks", serde_json::to_value(handle.by_remote(account_id, remote_path)).unwrap_or_else(|_| json!([])))],
            ))
        }
        "stats" => Ok(snapshot_json(
            &handle,
            vec![("stats", serde_json::to_value(handle.stats()).unwrap_or_else(|_| json!({})))],
        )),
        other => Err(format!("err.unknownQuery|{other}")),
    }
}

// ── Wait / platform reply ─────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn wdmm_queue_wait(task_id: *const c_char, timeout_ms: u64) -> crate::FfiString {
    let Some(task_id) = crate::ptr_to_str(task_id) else {
        return fail("err.badArgs|null task_id");
    };
    let Ok(handle) = handle() else {
        return fail("err.notInitialized|call wdmm_queue_set_paths first");
    };
    match handle.wait_blocking(task_id, timeout_ms) {
        WaitOutcome::Terminal { status } => {
            let task = serde_json::to_value(handle.task(task_id)).unwrap_or(Value::Null);
            crate::FfiString::from(
                json!({
                    "ok": true,
                    "rev": handle.rev(),
                    "status": status,
                    "task": task,
                })
                .to_string(),
            )
        }
        WaitOutcome::Timeout => fail("err.timeout"),
        WaitOutcome::NotFound => fail("err.notFound"),
    }
}

#[no_mangle]
pub extern "C" fn wdmm_queue_platform_result(
    req_id: *const c_char,
    result_json: *const c_char,
) -> crate::FfiString {
    let Some(req_id) = crate::ptr_to_str(req_id) else {
        return fail("err.badArgs|null req_id");
    };
    let Some(raw) = crate::ptr_to_str(result_json) else {
        return fail("err.badArgs|null result_json");
    };
    let value: Value = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(error) => return fail(format!("err.badJson|{error}")),
    };
    let Ok(handle) = handle() else {
        return fail("err.notInitialized|call wdmm_queue_set_paths first");
    };
    // `platform_reply` calls `kick()`, which spawns the pump: needs the runtime.
    let delivered = crate::rt().block_on(async {
        handle.platform_reply(req_id, platform_result_from_json(&value))
    });
    crate::FfiString::from(
        json!({ "ok": true, "rev": handle.rev(), "delivered": delivered }).to_string(),
    )
}

fn platform_result_from_json(value: &Value) -> PlatformResult {
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        return PlatformResult::error(error);
    }
    let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(true);
    let result_json = value
        .get("result")
        .or_else(|| value.get("result_json"))
        .map(|result| match result {
            Value::String(raw) => raw.clone(),
            other => other.to_string(),
        });
    PlatformResult {
        ok,
        result_json,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::{CStr, CString};
    use std::sync::Mutex;

    use super::*;
    use wdmm_queue::{MemStore, RunCtx, RunOutcome, TaskRunner};

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Runner that polls its cancel token like a real executor and otherwise
    /// takes ~2 s to finish, so cancel/dedupe/wait-timeout stay deterministic.
    struct SlowRunner;

    #[async_trait::async_trait]
    impl TaskRunner for SlowRunner {
        fn supports(&self, _kind: TaskKind) -> bool {
            true
        }

        async fn run(&self, ctx: RunCtx) -> RunOutcome {
            for _ in 0..100 {
                if ctx.cancelled() {
                    return RunOutcome::Cancelled;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            RunOutcome::Completed
        }
    }

    fn init_queue() -> QueueHandle {
        if let Some(handle) = QueueHandle::global() {
            return handle;
        }
        let store: Arc<dyn TaskStore> = Arc::new(MemStore::new());
        let handle = crate::rt()
            .block_on(async { QueueHandle::try_new(store, Arc::new(SlowRunner)) })
            .expect("test queue");
        let _ = QueueHandle::install_global(handle);
        // Avoid the cross-test scheduler racing on concurrency.
        let handle = QueueHandle::global().expect("global queue");
        let _ = crate::rt().block_on(async { handle.set_concurrency(8) });
        handle
    }

    fn call(f: extern "C" fn(*const c_char) -> crate::FfiString, json: &str) -> Value {
        let cstring = CString::new(json).expect("cstring");
        let out = f(cstring.as_ptr());
        let text = unsafe { CStr::from_ptr(out.ptr) }
            .to_str()
            .expect("utf8")
            .to_string();
        crate::wdmm_ffi_free_string(out);
        serde_json::from_str(&text).unwrap_or_else(|_| json!({ "raw": text }))
    }

    fn cmd(value: Value) -> Value {
        call(wdmm_queue_cmd, &value.to_string())
    }

    fn query(value: Value) -> Value {
        call(wdmm_queue_query, &value.to_string())
    }

    fn enqueue(remote: &str) -> Value {
        cmd(json!({
            "cmd": "enqueue",
            "kind": "scan",
            "account_id": "acc-ffi-test",
            "remote_path": remote,
            "file_name": "scan",
            "source_name": "NAS",
        }))
    }

    #[test]
    fn enqueue_snapshot_events_cancel_wait() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let handle = init_queue();

        let added = enqueue("/music/ffi-a");
        assert_eq!(added["ok"], true, "enqueue failed: {added}");
        let task_id = added["task_id"].as_str().expect("task_id").to_string();
        assert!(added["rev"].as_u64().unwrap_or(0) >= 1);
        assert!(added["tasks"].is_array(), "commands return the snapshot");
        assert!(added["deduped"].is_boolean());

        let snapshot = query(json!({ "query": "snapshot" }));
        assert_eq!(snapshot["ok"], true);
        assert!(snapshot["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|task| task["id"] == json!(task_id)));

        let events = query(json!({ "query": "events", "since_seq": 0 }));
        assert_eq!(events["ok"], true);
        assert!(events["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["type"] == "task_added" && event["task"]["id"] == json!(task_id)));
        assert!(events["next_seq"].as_u64().unwrap_or(0) > 0);

        let cancelled = cmd(json!({ "cmd": "cancel", "task_id": task_id }));
        assert_eq!(cancelled["ok"], true, "cancel failed: {cancelled}");
        assert_eq!(cancelled["changed"], true);

        let waited = {
            let cstring = CString::new(task_id.as_str()).unwrap();
            let out = wdmm_queue_wait(cstring.as_ptr(), 5_000);
            let text = unsafe { CStr::from_ptr(out.ptr) }.to_str().unwrap().to_string();
            crate::wdmm_ffi_free_string(out);
            serde_json::from_str::<Value>(&text).unwrap()
        };
        assert_eq!(waited["ok"], true, "wait failed: {waited}");
        assert_eq!(waited["status"], "cancelled");
        assert!(handle.rev() >= 2);
    }

    #[test]
    fn wait_times_out() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        init_queue();
        let added = enqueue("/music/ffi-wait");
        let task_id = added["task_id"].as_str().unwrap().to_string();
        let cstring = CString::new(task_id).unwrap();
        let out = wdmm_queue_wait(cstring.as_ptr(), 40);
        let text = unsafe { CStr::from_ptr(out.ptr) }.to_str().unwrap().to_string();
        crate::wdmm_ffi_free_string(out);
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"], "err.timeout");
    }

    #[test]
    fn enqueue_dedupes_in_flight_task() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        init_queue();
        let first = enqueue("/music/ffi-dedupe");
        let second = enqueue("/music/ffi-dedupe");
        assert_eq!(first["deduped"], false);
        assert_eq!(second["deduped"], true);
        assert_eq!(first["task_id"], second["task_id"]);
    }

    #[test]
    fn rev_is_monotonic() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        init_queue();
        let first = enqueue("/music/ffi-rev-1");
        let second = enqueue("/music/ffi-rev-2");
        let rev1 = first["rev"].as_u64().unwrap();
        let rev2 = second["rev"].as_u64().unwrap();
        assert!(rev2 > rev1, "rev must grow: {rev1} -> {rev2}");
        let snapshot = query(json!({ "query": "snapshot" }));
        assert!(snapshot["rev"].as_u64().unwrap() >= rev2);
    }

    #[test]
    fn invalid_json_is_reported() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        init_queue();
        let broken = call(wdmm_queue_cmd, "{not json");
        assert_eq!(broken["ok"], false);
        assert!(broken["error"].as_str().unwrap().starts_with("err.badJson"));

        let missing = cmd(json!({ "cmd": "nope" }));
        assert_eq!(missing["ok"], false);
        assert!(missing["error"].as_str().unwrap().starts_with("err.unknownCmd"));

        let query_error = query(json!({ "query": "nope" }));
        assert_eq!(query_error["ok"], false);
    }

    #[test]
    fn platform_result_for_unknown_request_is_not_delivered() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        init_queue();
        let cstring = CString::new("pr-does-not-exist").unwrap();
        let payload = CString::new("{\"ok\":true,\"result\":{\"uri\":\"content://x\"}}").unwrap();
        let out = wdmm_queue_platform_result(cstring.as_ptr(), payload.as_ptr());
        let text = unsafe { CStr::from_ptr(out.ptr) }.to_str().unwrap().to_string();
        crate::wdmm_ffi_free_string(out);
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(value["delivered"], false);
    }
}
