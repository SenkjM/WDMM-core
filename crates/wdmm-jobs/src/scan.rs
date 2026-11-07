//! Library scans: music/video tag ingest plus CUE album ingestion.
//!
//! A scan task owns its own discovery: given a folder root it walks it, and
//! when given a payload envelope it processes the listed paths. `units_total`
//! grows while discovering. Items are processed cues-first (like Dart
//! `_runScan`) so the audio files covered by a CUE sheet are skipped.
//!
//! Per-item errors are collected instead of aborting the scan; a mixture of
//! successes and failures ends as [`JobOutcome::Partial`] with
//! `{"ok":n,"failed":[{"path","error"}]}`.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use wdmm_queue::model::TaskKind;
use wdmm_queue::policy::FailureCategory;

use crate::ingest::{basename, ingest_cue_bytes, ingest_path, is_cue_path, is_music_path};
use crate::{Job, JobCtx, JobOutcome};

/// Maximum directory depth a folder-root scan descends.
const MAX_DEPTH: u32 = 64;
/// Guards against pathological trees / infinite remote listings.
const MAX_DIRS: u32 = 10_000;
/// Same guard for discovered files.
const MAX_ENTRIES: usize = 100_000;

/// One item of the scan payload. Dart `ScanPathEntry.toJson` writes
/// `path`/`name`/`cue`; the executor also accepts `isCue`/`is_cue` because the
/// queue contract describes the field that way.
#[derive(Debug, Clone, Deserialize)]
struct ScanPathEntry {
    path: String,
    #[serde(default)]
    name: String,
    #[serde(default, alias = "cue", alias = "isCue")]
    is_cue: bool,
}

pub struct ScanJob;

#[async_trait]
impl Job for ScanJob {
    fn kind(&self) -> TaskKind {
        TaskKind::Scan
    }

    async fn run(&self, ctx: JobCtx) -> JobOutcome {
        run(&ctx).await
    }
}

async fn run(ctx: &JobCtx) -> JobOutcome {
    let mut entries = match load_entries(ctx).await {
        Ok(entries) => entries,
        Err(error) => {
            return JobOutcome::Failed {
                error,
                category: FailureCategory::Other,
            };
        }
    };
    if ctx.cancelled() {
        return JobOutcome::Cancelled;
    }

    // Cues first: they add their backing audio to `covered`, and a music item
    // that is covered must not be ingested as a standalone track.
    entries.sort_by_key(|entry| !entry.is_cue);

    let total = u32::try_from(entries.len()).unwrap_or(u32::MAX);
    ctx.progress.units(0, Some(total));

    let mut covered: HashSet<String> = HashSet::new();
    let mut failed: Vec<serde_json::Value> = Vec::new();
    let mut done = 0_u32;

    for entry in &entries {
        if ctx.cancelled() {
            return JobOutcome::Cancelled;
        }
        if let Err(error) = process_entry(ctx, entry, &mut covered).await {
            ctx.progress
                .note(&format!("scan failed: {}: {error}", entry.path));
            failed.push(json!({ "path": entry.path, "error": error }));
        }
        done = done.saturating_add(1);
        ctx.progress.units(done, Some(total));
    }

    let failed_count = u32::try_from(failed.len()).unwrap_or(u32::MAX);
    let ok = total.saturating_sub(failed_count);
    if failed.is_empty() {
        JobOutcome::Completed
    } else if ok == 0 {
        // Dart marks an all-failed batch as failed; keep the first error as the
        // task's message (the per-path list is only meaningful for Partial).
        let error = failed
            .first()
            .and_then(|item| item.get("error"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("err.scan.failed")
            .to_string();
        JobOutcome::Failed {
            error,
            category: FailureCategory::Other,
        }
    } else {
        JobOutcome::Partial {
            result: json!({ "ok": ok, "failed": failed }),
        }
    }
}

async fn process_entry(
    ctx: &JobCtx,
    entry: &ScanPathEntry,
    covered: &mut HashSet<String>,
) -> Result<(), String> {
    let name = if entry.name.trim().is_empty() {
        basename(&entry.path).to_string()
    } else {
        entry.name.clone()
    };

    if entry.is_cue || is_cue_path(&name) {
        let bytes = ctx
            .vfs
            .get_content(&entry.path)
            .await
            .map_err(|error| format!("err.cloud.notFound|{}: {error}", entry.path))?;
        let (_outcome, audio) = ingest_cue_bytes(ctx, &entry.path, &bytes, false).await?;
        covered.extend(audio);
        return Ok(());
    }

    // Already ingested standalone or covered by a CUE sheet: count as success
    // without probing the file again (Dart skips these too).
    if covered.contains(&entry.path) {
        return Ok(());
    }
    if ctx
        .db
        .get_track_by_source_path(&ctx.task.source_name, &entry.path)
        .map_err(|error| format!("err.storage.track|{}: {error}", entry.path))?
        .is_some()
    {
        return Ok(());
    }

    ingest_path(ctx, &entry.path, &name, false)
        .await
        .map(|_outcome| ())
}

async fn load_entries(ctx: &JobCtx) -> Result<Vec<ScanPathEntry>, String> {
    if let Some(raw) = ctx
        .task
        .payload_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
    {
        let path = payload_file(ctx, raw);
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|error| format!("err.scan.payload|{}: {error}", path.display()))?;
        let entries: Vec<ScanPathEntry> = serde_json::from_slice(&bytes)
            .map_err(|error| format!("err.scan.payloadJson|{}: {error}", path.display()))?;
        if !entries.is_empty() {
            return Ok(entries);
        }
        // An empty payload is treated like "no payload" (Dart falls back to its
        // own discovery in that case).
    }

    let root = ctx.task.remote_path.trim();
    if root.is_empty() {
        return Err("err.scan.noRoot".into());
    }
    let root_name = if ctx.task.file_name.trim().is_empty() {
        basename(root).to_string()
    } else {
        ctx.task.file_name.clone()
    };
    if root_is_file(root) || root_is_file(&root_name) {
        return Ok(vec![ScanPathEntry {
            path: root.to_string(),
            name: root_name.clone(),
            is_cue: is_cue_path(root) || is_cue_path(&root_name),
        }]);
    }

    walk(ctx, root).await
}

fn payload_file(ctx: &JobCtx, raw: &str) -> PathBuf {
    let candidate = PathBuf::from(raw);
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

fn root_is_file(path: &str) -> bool {
    is_cue_path(path) || is_music_path(path)
}

async fn walk(ctx: &JobCtx, root: &str) -> Result<Vec<ScanPathEntry>, String> {
    let root = normalize_root(root);
    let mut queue: VecDeque<(String, u32)> = VecDeque::from([(root, 0)]);
    let mut seen: HashSet<String> = HashSet::new();
    let mut entries: Vec<ScanPathEntry> = Vec::new();
    let mut directories = 0_u32;

    while let Some((dir, depth)) = queue.pop_front() {
        if ctx.cancelled() || directories >= MAX_DIRS || entries.len() >= MAX_ENTRIES {
            break;
        }
        directories = directories.saturating_add(1);
        let items = ctx
            .vfs
            .list(&dir)
            .await
            .map_err(|error| format!("err.scan.list|{dir}: {error}"))?;
        for item in items {
            if item.name.is_empty() || item.name == "." || item.name == ".." {
                continue;
            }
            let path = join_path(&dir, &item.name);
            if !seen.insert(path.clone()) {
                continue;
            }
            if item.is_dir {
                if depth < MAX_DEPTH {
                    queue.push_back((path, depth + 1));
                }
                continue;
            }
            if is_cue_path(&item.name) {
                entries.push(ScanPathEntry {
                    path,
                    name: item.name,
                    is_cue: true,
                });
            } else if is_music_path(&item.name) {
                entries.push(ScanPathEntry {
                    path,
                    name: item.name,
                    is_cue: false,
                });
            }
        }
    }

    Ok(entries)
}

fn normalize_root(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return "/".to_string();
    }
    let mut root = trimmed.replace('\\', "/");
    if !root.starts_with('/') {
        root.insert(0, '/');
    }
    while root.len() > 1 && root.ends_with('/') {
        root.pop();
    }
    root
}

fn join_path(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use wdmm_queue::model::TaskKind;

    use crate::ingest::test_support::{
        memory_db, mp3_frame, tagged_mp3, test_ctx, test_task, FakeVfs, Recorder,
    };

    use super::*;

    fn write_payload(dir: &std::path::Path, entries: &serde_json::Value) -> String {
        let payload_dir = dir.join("docs").join("queue_payloads");
        std::fs::create_dir_all(&payload_dir).unwrap();
        std::fs::write(
            payload_dir.join("scan.json"),
            serde_json::to_vec(entries).unwrap(),
        )
        .unwrap();
        "scan.json".to_string()
    }

    #[tokio::test]
    async fn scan_reports_partial_when_one_item_fails() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        let vfs = Arc::new(
            FakeVfs::new().with("/Music/Album/good.mp3", tagged_mp3("Good", false)),
        );
        let progress = Arc::new(Recorder::default());
        let payload = write_payload(
            dir.path(),
            &json!([
                { "path": "/Music/Album/good.mp3", "name": "good.mp3", "cue": false },
                { "path": "/Music/Album/missing.mp3", "name": "missing.mp3", "cue": false }
            ]),
        );
        let mut task = test_task(TaskKind::Scan, "My Disk", "/Music/Album", "Album");
        task.payload_path = Some(payload);
        let ctx = test_ctx(
            dir.path(),
            Arc::clone(&db),
            vfs,
            task,
            Arc::clone(&progress),
        );

        let outcome = ScanJob.run(ctx).await;
        match outcome {
            JobOutcome::Partial { result } => {
                assert_eq!(result["ok"], 1);
                let failed = result["failed"].as_array().unwrap();
                assert_eq!(failed.len(), 1);
                assert_eq!(failed[0]["path"], "/Music/Album/missing.mp3");
            }
            other => panic!("expected partial, got {other:?}"),
        }

        let track = db
            .get_track_by_source_path("My Disk", "/Music/Album/good.mp3")
            .unwrap()
            .unwrap();
        assert_eq!(track.title, "Good");
        assert_eq!(
            progress.units.lock().unwrap().last().copied(),
            Some((2, Some(2)))
        );
    }

    #[tokio::test]
    async fn scan_ingests_cue_and_skips_its_backing_audio() {
        let cue_text = "\
TITLE \"Album Title\"
FILE \"album.flac\" WAVE
  TRACK 01 AUDIO
    TITLE \"First\"
    INDEX 01 00:00:00
";
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        let vfs = Arc::new(
            FakeVfs::new()
                .with("/Music/Album/album.cue", cue_text.as_bytes().to_vec())
                .with("/Music/Album/album.flac", mp3_frame()),
        );
        let progress = Arc::new(Recorder::default());
        let payload = write_payload(
            dir.path(),
            &json!([
                // The audio item is listed first on purpose: the executor must
                // still process the cue first and skip the covered file.
                { "path": "/Music/Album/album.flac", "name": "album.flac", "cue": false },
                { "path": "/Music/Album/album.cue", "name": "album.cue", "cue": true }
            ]),
        );
        let mut task = test_task(TaskKind::Scan, "My Disk", "/Music/Album", "Album");
        task.payload_path = Some(payload);
        let ctx = test_ctx(dir.path(), Arc::clone(&db), vfs, task, progress);

        let outcome = ScanJob.run(ctx).await;
        assert!(matches!(outcome, JobOutcome::Completed), "{outcome:?}");
        assert_eq!(db.list_cue_slices().unwrap().len(), 1);
        assert!(db.list_tracks().unwrap().is_empty());
    }

    #[tokio::test]
    async fn scan_walks_a_folder_root_without_a_payload() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        let vfs = Arc::new(
            FakeVfs::new()
                .with("/Music/Album/track.mp3", tagged_mp3("Track", false))
                .with(
                    "/Music/Album/album.cue",
                    b"FILE \"album.flac\" WAVE\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n"
                        .to_vec(),
                ),
        );
        let progress = Arc::new(Recorder::default());
        let task = test_task(TaskKind::Scan, "My Disk", "/Music", "Music");
        let ctx = test_ctx(
            dir.path(),
            Arc::clone(&db),
            vfs,
            task,
            Arc::clone(&progress),
        );

        let outcome = ScanJob.run(ctx).await;
        assert!(matches!(outcome, JobOutcome::Completed), "{outcome:?}");
        // The cue is processed first and covers track.mp3's path only when the
        // FILE name matches; here it does not, so both rows exist.
        assert_eq!(db.list_cue_slices().unwrap().len(), 1);
        assert_eq!(db.list_tracks().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn scan_returns_cancelled_before_processing() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        let vfs = Arc::new(FakeVfs::new());
        let progress = Arc::new(Recorder::default());
        let payload = write_payload(
            dir.path(),
            &json!([{ "path": "/Music/Album/x.mp3", "name": "x.mp3", "cue": false }]),
        );
        let mut task = test_task(TaskKind::Scan, "My Disk", "/Music/Album", "Album");
        task.payload_path = Some(payload);
        let ctx = test_ctx(dir.path(), db, vfs, task, progress);
        ctx.cancel.cancel();

        assert!(matches!(ScanJob.run(ctx).await, JobOutcome::Cancelled));
    }
}
