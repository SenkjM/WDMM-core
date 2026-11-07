//! FFI surface for the cache index (contract T2b).
//!
//! Storage schema v17 owns `cache_entries` / `cache_groups`; Dart only asks for
//! index data and then deletes the files itself. Nothing in this module touches
//! the filesystem: `path` is an opaque local absolute path supplied by Dart.
//!
//! * `wdmm_cache_cmd` — mutations (`touch`, `put`, `delete`, `forget_kind`,
//!   `evict`, `delete_group`).
//! * `wdmm_cache_query` — reads (`stats`, `lru`, `list`, `group_paths`,
//!   `groups_for_path`, `groups`).
//!
//! Every success is `{"ok":true, …}`; every failure is
//! `{"ok":false,"error":"err.<code>|<detail>"}` and never panics.

use std::os::raw::c_char;
use std::sync::Arc;

use serde_json::{json, Value};

use wdmm_storage::{CacheEntryRow, LibraryDatabase};

/// Kinds written by Dart `CacheKind` (`lib/models/cache_policy.dart`).
///
/// The enum only has `music` and `manga` — covers are indexed as `manga`.
const KINDS: [&str; 2] = ["music", "manga"];

// ── Helpers ───────────────────────────────────────────────────────────────────

fn fail(message: impl Into<String>) -> crate::FfiString {
    crate::FfiString::from(json!({ "ok": false, "error": message.into() }).to_string())
}

fn ok(value: Value) -> crate::FfiString {
    crate::FfiString::from(value.to_string())
}

fn db() -> Result<Arc<LibraryDatabase>, String> {
    crate::try_global_library_db()
        .cloned()
        .ok_or_else(|| "err.notInitialized|call wdmm_library_init first".to_string())
}

fn storage_err(error: impl std::fmt::Display) -> String {
    format!("err.storage|{error}")
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn req_str<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value[key]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("err.badJson|missing '{key}'"))
}

fn opt_str(value: &Value, key: &str) -> Option<String> {
    value[key]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn req_kind(value: &Value) -> Result<&str, String> {
    let kind = req_str(value, "kind")?;
    if KINDS.contains(&kind) {
        Ok(kind)
    } else {
        Err(format!(
            "err.badJson|unknown kind '{kind}' (expected one of: {})",
            KINDS.join(", ")
        ))
    }
}

fn parse(json: *const c_char) -> Result<Value, String> {
    let Some(text) = crate::ptr_to_str(json) else {
        return Err("err.badArgs|null json".to_string());
    };
    let value: Value =
        serde_json::from_str(text).map_err(|e| format!("err.badJson|{e}"))?;
    if !value.is_object() {
        return Err("err.badJson|expected a JSON object".to_string());
    }
    Ok(value)
}

fn entry_json(entry: &CacheEntryRow) -> Value {
    json!({
        "path": &entry.path,
        "kind": &entry.kind,
        "size": entry.size,
        "accessed_at": entry.accessed_at,
        "created_at": entry.created_at,
        "account_id": &entry.account_id,
        "remote_path": &entry.remote_path,
    })
}

fn entries_json(entries: &[CacheEntryRow]) -> Value {
    Value::Array(entries.iter().map(entry_json).collect())
}

/// Resolve `(account_id, remote_path)` → index rows.
///
/// The storage layer keys entries by local `path` only, so remote lookups fan
/// out over the (small) index instead of adding a second access path.
fn entries_for_remote(
    db: &LibraryDatabase,
    account_id: Option<&str>,
    remote_path: &str,
) -> Result<Vec<CacheEntryRow>, String> {
    let entries = db.cache_list_entries().map_err(storage_err)?;
    Ok(entries
        .into_iter()
        .filter(|e| e.remote_path.as_deref() == Some(remote_path))
        .filter(|e| match account_id {
            Some(id) => e.account_id.as_deref() == Some(id),
            None => true,
        })
        .collect())
}

// ── Commands ──────────────────────────────────────────────────────────────────

/// Cache index mutations. See the module docs for the per-command JSON shape.
#[no_mangle]
pub extern "C" fn wdmm_cache_cmd(json: *const c_char) -> crate::FfiString {
    let value = match parse(json) {
        Ok(value) => value,
        Err(error) => return fail(error),
    };
    let db = match db() {
        Ok(db) => db,
        Err(error) => return fail(error),
    };
    match run_cmd(&db, &value) {
        Ok(value) => ok(value),
        Err(error) => fail(error),
    }
}

fn run_cmd(db: &LibraryDatabase, value: &Value) -> Result<Value, String> {
    let cmd = req_str(value, "cmd")?;
    match cmd {
        "touch" => cmd_touch(db, value),
        "put" => cmd_put(db, value),
        "delete" => cmd_delete(db, value),
        "forget_kind" => {
            let kind = req_kind(value)?;
            let removed = db.cache_forget_kind(kind).map_err(storage_err)?;
            Ok(json!({ "ok": true, "removed": removed }))
        }
        "evict" => cmd_evict(db, value),
        "delete_group" => {
            let group_id = req_str(value, "group_id")?;
            let removed = db.cache_delete_group(group_id).map_err(storage_err)?;
            Ok(json!({ "ok": true, "removed": removed }))
        }
        // Not in the task-9 table, but Dart `CacheService.bindCacheGroup` needs it:
        // storage keys groups by local path, so Dart binds the path it just wrote.
        "bind_group" => cmd_bind_group(db, value),
        other => Err(format!("err.unknownCommand|{other}")),
    }
}

fn cmd_bind_group(db: &LibraryDatabase, value: &Value) -> Result<Value, String> {
    let group_id = req_str(value, "group_id")?;
    let Some(paths) = value["paths"].as_array() else {
        return Err("err.badJson|missing 'paths'".to_string());
    };
    let paths: Vec<String> = paths
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    if paths.is_empty() {
        return Ok(json!({ "ok": true, "added": 0 }));
    }
    let before = db.cache_group_paths(group_id).map_err(storage_err)?;
    db.cache_bind_group(group_id, &paths).map_err(storage_err)?;
    let after = db.cache_group_paths(group_id).map_err(storage_err)?;
    Ok(json!({ "ok": true, "added": after.len().saturating_sub(before.len()) }))
}

fn cmd_touch(db: &LibraryDatabase, value: &Value) -> Result<Value, String> {
    let remote_path = req_str(value, "remote_path")?;
    let account_id = opt_str(value, "account_id");
    let entries = entries_for_remote(db, account_id.as_deref(), remote_path)?;
    let Some(entry) = entries.first() else {
        return Ok(json!({ "ok": true, "touched": false }));
    };
    let path = entry.path.clone();
    let kind = entry.kind.clone();
    db.cache_touch(&path, &kind, now_ms()).map_err(storage_err)?;
    Ok(json!({ "ok": true, "touched": true, "path": path }))
}

fn cmd_put(db: &LibraryDatabase, value: &Value) -> Result<Value, String> {
    let kind = req_kind(value)?;
    let path = req_str(value, "path")?;
    let size = value["size"].as_i64().unwrap_or(0).max(0);
    let account_id = opt_str(value, "account_id");
    let remote_path = opt_str(value, "remote_path");
    let now = now_ms();
    let existing = db.cache_get_entry(path).map_err(storage_err)?;
    let created_at = existing.as_ref().map(|e| e.created_at).unwrap_or(now);
    let inserted = existing.is_none();
    let entry = CacheEntryRow {
        path: path.to_string(),
        kind: kind.to_string(),
        size,
        accessed_at: now,
        created_at,
        account_id: account_id.or_else(|| existing.as_ref().and_then(|e| e.account_id.clone())),
        remote_path: remote_path.or_else(|| existing.as_ref().and_then(|e| e.remote_path.clone())),
    };
    db.cache_put_entry(&entry).map_err(storage_err)?;
    Ok(json!({ "ok": true, "inserted": inserted }))
}

fn cmd_delete(db: &LibraryDatabase, value: &Value) -> Result<Value, String> {
    if let Some(paths) = value["paths"].as_array() {
        // Local-path form: used by `pruneMissing` (Dart already knows the file
        // is gone and only needs the stale index row dropped).
        let paths: Vec<String> = paths
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        let removed = db.cache_delete_entries(&paths).map_err(storage_err)?;
        return Ok(json!({ "ok": true, "removed": removed }));
    }
    let Some(remote_paths) = value["remote_paths"].as_array() else {
        return Err("err.badJson|missing 'remote_paths'".to_string());
    };
    let remote_paths: Vec<&str> = remote_paths.iter().filter_map(Value::as_str).collect();
    let account_id = opt_str(value, "account_id");
    let entries = db.cache_list_entries().map_err(storage_err)?;
    let paths: Vec<String> = entries
        .into_iter()
        .filter(|e| match &e.remote_path {
            Some(remote) => remote_paths.contains(&remote.as_str()),
            None => false,
        })
        .filter(|e| match &account_id {
            Some(id) => e.account_id.as_deref() == Some(id.as_str()),
            None => true,
        })
        .map(|e| e.path)
        .collect();
    let removed = db.cache_delete_entries(&paths).map_err(storage_err)?;
    Ok(json!({ "ok": true, "removed": removed }))
}

fn cmd_evict(db: &LibraryDatabase, value: &Value) -> Result<Value, String> {
    let max_bytes = value["max_bytes"]
        .as_u64()
        .ok_or_else(|| "err.badJson|missing 'max_bytes'".to_string())? as i64;
    // `cache_list_entries` is ordered accessed_at ASC, path ASC — LRU first.
    let entries = db.cache_list_entries().map_err(storage_err)?;
    let total: i64 = entries.iter().map(|e| e.size.max(0)).sum();
    if total <= max_bytes {
        return Ok(json!({ "ok": true, "removed": 0, "freed_bytes": 0 }));
    }
    let mut remaining = total;
    let mut freed: i64 = 0;
    let mut victims: Vec<String> = Vec::new();
    for entry in &entries {
        if remaining <= max_bytes {
            break;
        }
        let size = entry.size.max(0);
        remaining -= size;
        freed += size;
        victims.push(entry.path.clone());
    }
    let removed = db.cache_delete_entries(&victims).map_err(storage_err)?;
    Ok(json!({ "ok": true, "removed": removed, "freed_bytes": freed }))
}

// ── Queries ───────────────────────────────────────────────────────────────────

/// Cache index reads. See the module docs for the per-query JSON shape.
#[no_mangle]
pub extern "C" fn wdmm_cache_query(json: *const c_char) -> crate::FfiString {
    let value = match parse(json) {
        Ok(value) => value,
        Err(error) => return fail(error),
    };
    let db = match db() {
        Ok(db) => db,
        Err(error) => return fail(error),
    };
    match run_query(&db, &value) {
        Ok(value) => ok(value),
        Err(error) => fail(error),
    }
}

fn run_query(db: &LibraryDatabase, value: &Value) -> Result<Value, String> {
    let query = value["q"].as_str().or_else(|| value["query"].as_str());
    let Some(query) = query.map(str::trim).filter(|q| !q.is_empty()) else {
        return Err("err.badJson|missing 'q'".to_string());
    };
    match query {
        "stats" => query_stats(db),
        "lru" => {
            let limit = value["limit"].as_u64().unwrap_or(100).clamp(1, 1000) as usize;
            let entries = db.cache_lru(limit).map_err(storage_err)?;
            Ok(json!({ "ok": true, "entries": entries_json(&entries) }))
        }
        "list" => {
            let kind = req_kind(value)?;
            let entries = db.cache_list_entries_by_kind(kind).map_err(storage_err)?;
            Ok(json!({ "ok": true, "entries": entries_json(&entries) }))
        }
        "group_paths" => {
            let group_id = req_str(value, "group_id")?;
            let paths = db.cache_group_paths(group_id).map_err(storage_err)?;
            Ok(json!({ "ok": true, "paths": paths }))
        }
        "groups_for_path" => {
            let remote_path = req_str(value, "remote_path")?;
            let account_id = opt_str(value, "account_id");
            let mut group_ids: Vec<String> = Vec::new();
            for entry in entries_for_remote(db, account_id.as_deref(), remote_path)? {
                for group_id in db.cache_groups_for_path(&entry.path).map_err(storage_err)? {
                    if !group_ids.contains(&group_id) {
                        group_ids.push(group_id);
                    }
                }
            }
            group_ids.sort();
            Ok(json!({ "ok": true, "group_ids": group_ids }))
        }
        "groups" => query_groups(db, value),
        other => Err(format!("err.unknownQuery|{other}")),
    }
}

fn query_stats(db: &LibraryDatabase) -> Result<Value, String> {
    let entries = db.cache_list_entries().map_err(storage_err)?;
    let mut total_bytes: i64 = 0;
    let mut by_kind: std::collections::BTreeMap<String, (i64, i64)> = Default::default();
    for entry in &entries {
        let size = entry.size.max(0);
        total_bytes += size;
        let slot = by_kind.entry(entry.kind.clone()).or_insert((0, 0));
        slot.0 += size;
        slot.1 += 1;
    }
    let by_kind: serde_json::Map<String, Value> = by_kind
        .into_iter()
        .map(|(kind, (bytes, count))| (kind, json!({ "bytes": bytes, "entries": count })))
        .collect();
    Ok(json!({
        "ok": true,
        "total_bytes": total_bytes,
        "total_entries": entries.len(),
        "by_kind": by_kind,
    }))
}

fn query_groups(db: &LibraryDatabase, value: &Value) -> Result<Value, String> {
    let kind = value["kind"].as_str().map(str::trim).filter(|k| !k.is_empty());
    if let Some(kind) = kind {
        if !KINDS.contains(&kind) {
            return Err(format!("err.badJson|unknown kind '{kind}'"));
        }
    }
    let entries = db.cache_list_entries().map_err(storage_err)?;
    let groups = db.cache_list_groups().map_err(storage_err)?;
    let mut out: Vec<Value> = Vec::new();
    for (group_id, paths) in groups {
        let sizes: Vec<i64> = paths
            .iter()
            .map(|path| {
                entries
                    .iter()
                    .find(|e| &e.path == path)
                    .map(|e| e.size.max(0))
                    .unwrap_or(0)
            })
            .collect();
        if let Some(kind) = kind {
            let matches = paths
                .iter()
                .any(|path| entries.iter().any(|e| &e.path == path && e.kind == kind));
            if !matches {
                continue;
            }
        }
        out.push(json!({
            "group_id": group_id,
            "paths": paths,
            "bytes": sizes.iter().sum::<i64>(),
        }));
    }
    Ok(json!({ "ok": true, "groups": out }))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::ffi::CString;

    use super::*;

    fn test_db() -> Arc<LibraryDatabase> {
        Arc::new(
            LibraryDatabase::open(std::path::Path::new(":memory:"))
                .expect("cache_api: in-memory DB"),
        )
    }

    fn row(path: &str, kind: &str, size: i64, accessed: i64) -> CacheEntryRow {
        CacheEntryRow {
            path: path.to_string(),
            kind: kind.to_string(),
            size,
            accessed_at: accessed,
            created_at: accessed,
            account_id: Some("acc1".to_string()),
            remote_path: Some(format!("/remote{path}")),
        }
    }

    fn insert(db: &LibraryDatabase, entry: CacheEntryRow) {
        db.cache_put_entry(&entry).expect("put entry");
    }

    fn cmd(db: &LibraryDatabase, value: Value) -> Value {
        run_cmd(db, &value).expect("command")
    }

    fn query(db: &LibraryDatabase, value: Value) -> Value {
        run_query(db, &value).expect("query")
    }

    #[test]
    fn evict_removes_lru_first() {
        let db = test_db();
        insert(&db, row("/a", "music", 100, 10));
        insert(&db, row("/b", "music", 100, 20));
        insert(&db, row("/c", "manga", 100, 30));

        let out = cmd(&db, json!({ "cmd": "evict", "max_bytes": 150 }));
        assert_eq!(out["ok"], true);
        assert_eq!(out["removed"], 2);
        assert_eq!(out["freed_bytes"], 200);

        let left = db.cache_list_entries().expect("entries");
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].path, "/c");
    }

    #[test]
    fn evict_noop_when_under_budget() {
        let db = test_db();
        insert(&db, row("/a", "music", 100, 10));
        let out = cmd(&db, json!({ "cmd": "evict", "max_bytes": 1024 }));
        assert_eq!(out["removed"], 0);
        assert_eq!(out["freed_bytes"], 0);
        assert_eq!(db.cache_list_entries().expect("entries").len(), 1);
    }

    #[test]
    fn stats_aggregates_by_kind() {
        let db = test_db();
        insert(&db, row("/a", "music", 100, 10));
        insert(&db, row("/b", "music", 300, 20));
        insert(&db, row("/c", "manga", 50, 30));

        let out = query(&db, json!({ "q": "stats" }));
        assert_eq!(out["ok"], true);
        assert_eq!(out["total_bytes"], 450);
        assert_eq!(out["total_entries"], 3);
        assert_eq!(out["by_kind"]["music"]["bytes"], 400);
        assert_eq!(out["by_kind"]["music"]["entries"], 2);
        assert_eq!(out["by_kind"]["manga"]["bytes"], 50);
        assert_eq!(out["by_kind"]["manga"]["entries"], 1);
    }

    #[test]
    fn touch_and_delete_resolve_by_remote_path() {
        let db = test_db();
        insert(&db, row("/a", "music", 100, 10));

        let out = cmd(
            &db,
            json!({ "cmd": "touch", "remote_path": "/remote/a", "account_id": "acc1" }),
        );
        assert_eq!(out["touched"], true);
        assert!(db.cache_accessed_at("/a").expect("accessed").unwrap_or(0) > 10);

        let missing = cmd(
            &db,
            json!({ "cmd": "touch", "remote_path": "/missing", "account_id": "acc1" }),
        );
        assert_eq!(missing["touched"], false);

        let deleted = cmd(
            &db,
            json!({ "cmd": "delete", "remote_paths": ["/remote/a"], "account_id": "acc1" }),
        );
        assert_eq!(deleted["removed"], 1);
        assert!(db.cache_list_entries().expect("entries").is_empty());
    }

    #[test]
    fn put_reports_inserted_and_preserves_created_at() {
        let db = test_db();
        insert(&db, row("/a", "music", 100, 10));

        let out = cmd(
            &db,
            json!({ "cmd": "put", "kind": "music", "account_id": "acc1",
                    "remote_path": "/remote/a", "size": 200, "path": "/a" }),
        );
        assert_eq!(out["inserted"], false);
        let entry = db.cache_get_entry("/a").expect("get").expect("row");
        assert_eq!(entry.size, 200);
        assert_eq!(entry.created_at, 10);

        let fresh = cmd(
            &db,
            json!({ "cmd": "put", "kind": "manga", "account_id": "acc1",
                    "remote_path": "/remote/b", "size": 1, "path": "/b" }),
        );
        assert_eq!(fresh["inserted"], true);
    }

    #[test]
    fn group_queries_use_local_paths() {
        let db = test_db();
        insert(&db, row("/a", "music", 100, 10));
        insert(&db, row("/b", "music", 200, 20));
        db.cache_bind_group("g1", &["/a".to_string(), "/b".to_string()])
            .expect("bind group");

        let paths = query(&db, json!({ "q": "group_paths", "group_id": "g1" }));
        assert_eq!(paths["paths"], json!(["/a", "/b"]));

        let groups = query(
            &db,
            json!({ "q": "groups_for_path", "remote_path": "/remote/a", "account_id": "acc1" }),
        );
        assert_eq!(groups["group_ids"], json!(["g1"]));

        let by_kind = query(&db, json!({ "q": "groups", "kind": "music" }));
        assert_eq!(by_kind["groups"][0]["group_id"], "g1");
        assert_eq!(by_kind["groups"][0]["bytes"], 300);

        let deleted = cmd(&db, json!({ "cmd": "delete_group", "group_id": "g1" }));
        assert_eq!(deleted["removed"], 2);
        let paths = query(&db, json!({ "q": "group_paths", "group_id": "g1" }));
        assert_eq!(paths["paths"], json!([]));
    }

    #[test]
    fn lru_and_list_are_oldest_first() {
        let db = test_db();
        insert(&db, row("/b", "music", 100, 20));
        insert(&db, row("/a", "music", 100, 10));
        insert(&db, row("/c", "manga", 100, 30));

        let lru = query(&db, json!({ "q": "lru", "limit": 2 }));
        assert_eq!(lru["entries"][0]["path"], "/a");
        assert_eq!(lru["entries"][1]["path"], "/b");

        let list = query(&db, json!({ "q": "list", "kind": "music" }));
        assert_eq!(list["entries"].as_array().map(Vec::len), Some(2));
        assert_eq!(list["entries"][0]["kind"], "music");
    }

    #[test]
    fn bind_group_is_idempotent_and_delete_accepts_local_paths() {
        let db = test_db();
        insert(&db, row("/a", "music", 100, 10));
        insert(&db, row("/b", "music", 100, 20));

        let first = cmd(&db, json!({ "cmd": "bind_group", "group_id": "g2",
                                     "paths": ["/a", "/b"] }));
        assert_eq!(first["added"], 2);
        let again = cmd(&db, json!({ "cmd": "bind_group", "group_id": "g2",
                                     "paths": ["/a"] }));
        assert_eq!(again["added"], 0);

        // Stale-row pruning: drop the index row by local path.
        let pruned = cmd(&db, json!({ "cmd": "delete", "paths": ["/a"] }));
        assert_eq!(pruned["removed"], 1);
        assert_eq!(db.cache_list_entries().expect("entries").len(), 1);
    }

    #[test]
    fn unknown_kind_and_missing_fields_are_reported() {
        let db = test_db();
        let bad_kind = run_cmd(&db, &json!({ "cmd": "forget_kind", "kind": "video" }))
            .expect_err("unknown kind rejected");
        assert!(bad_kind.starts_with("err.badJson|"));

        let missing = run_query(&db, &json!({ "q": "list" })).expect_err("kind required");
        assert!(missing.starts_with("err.badJson|"));

        let unknown = run_cmd(&db, &json!({ "cmd": "nope" })).expect_err("unknown cmd");
        assert!(unknown.starts_with("err.unknownCommand|"));
    }

    #[test]
    fn ffi_symbols_report_bad_arguments() {
        let out = wdmm_cache_cmd(std::ptr::null());
        let text = unsafe { std::ffi::CStr::from_ptr(out.ptr) }
            .to_str()
            .expect("utf8")
            .to_string();
        crate::wdmm_ffi_free_string(out);
        let value: Value = serde_json::from_str(&text).expect("json");
        assert_eq!(value["ok"], false);
        assert!(value["error"].as_str().unwrap_or("").starts_with("err."));

        let input = CString::new("not json").expect("cstring");
        let out = wdmm_cache_query(input.as_ptr());
        let text = unsafe { std::ffi::CStr::from_ptr(out.ptr) }
            .to_str()
            .expect("utf8")
            .to_string();
        crate::wdmm_ffi_free_string(out);
        let value: Value = serde_json::from_str(&text).expect("json");
        assert_eq!(value["ok"], false);
        assert!(value["error"]
            .as_str()
            .unwrap_or("")
            .starts_with("err.badJson|"));
    }
}
