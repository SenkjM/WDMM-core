//! Manga executors: scrape a packed CBZ, cache a chapter archive, pack loose
//! pages into a CBZ, and build the manga index from image directories.
//!
//! Ported from `lib/services/task_queue_service.dart`:
//!
//! * [`MangaScrapeJob`] — `_runMangaScrape` (:3246)
//! * [`MangaCacheJob`] — `_runMangaCache` (:3362)
//! * [`MangaPackJob`] — `_runMangaPack` (:3439)
//! * [`MangaBuildJob`] — `_runMangaBuild` (:3777) and
//!   `_runMangaBuildFromLocalUris` (:3935)
//!
//! ## Differences from the Dart executor (migration note)
//!
//! * `WdmmVfs` has no ranged *file* reader, so the scrape whole-file path
//!   downloads the CBZ through [`WdmmVfs::get_content_range`] into the queue
//!   payload directory first. Dart read ranges lazily (`CbzRangeZip`); the
//!   driver-level range API is the same one the download executor uses.
//! * Cover bytes are written unchanged. Dart resized to the user's
//!   `mangaCoverThumbSizePx` (default 500) inside `writeMangaCoverThumb`;
//!   `JobCtx` carries no settings, so the FFI/settings layer must resize later
//!   or the UI must accept full-size covers (reported to the Lead).
//! * Pack archives land in `<app_docs>/queue_payloads/<taskId>.cbz` instead of
//!   the OS temp dir, so the upload executor (which only cleans files under
//!   `payload_dir`) owns their lifetime after the hand-off.
//! * `JobCtx` has no queue handle. `MangaPackJob` therefore reports the pending
//!   upload as a [`JobOutcome::Partial`] result instead of calling
//!   `enqueueUpload` itself; the scheduler/FFI turns that result into an
//!   `Upload` task (Dart `_runMangaPack` did it inline). The same applies to
//!   the local-URI build variant, which may produce one upload per directory.
//! * Dart enqueued the upload even when the archive was incomplete. Here an
//!   incomplete pack fails and removes the archive — a CBZ missing pages must
//!   never reach the remote.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::fs;
use tokio::io::AsyncWriteExt;

use wdmm_drivers::{CloudFileItem, DriverError};
use wdmm_queue::model::TaskKind;
use wdmm_queue::policy::FailureCategory;
use wdmm_storage::{
    manga_id_for_remote, normalize_manga_remote_path, normalize_manga_source_name, CacheEntryRow,
    LibraryDatabase, Manga, MangaPage,
};

use crate::cache_paths::{part_path, sanitize_file_name, CachePaths};
use crate::cbz::{self, PackPage};
use crate::{Job, JobCtx, JobOutcome, PlatformAction};

/// Download chunk for the scrape/cache readers. Mirrors the download executor's
/// 8 MiB contract value.
const FETCH_CHUNK: u64 = 8 * 1024 * 1024;

/// Maximum archive entries the build walker may encounter (Dart
/// `kMangaBuildMaxEntries` default, clamped like `WalkOptions`).
const MAX_WALK_ENTRIES: usize = 50_000;

/// Hard cap on directory depth (Dart clamps to 20).
const MAX_WALK_DEPTH: u32 = 20;

/// Registered executor for `MangaScrape`.
pub struct MangaScrapeJob;

/// Registered executor for `MangaCache`.
pub struct MangaCacheJob;

/// Registered executor for `MangaPack`.
pub struct MangaPackJob;

/// Registered executor for `MangaBuild`.
pub struct MangaBuildJob;

#[async_trait]
impl Job for MangaScrapeJob {
    fn kind(&self) -> TaskKind {
        TaskKind::MangaScrape
    }

    async fn run(&self, ctx: JobCtx) -> JobOutcome {
        run_scrape(&ctx).await
    }
}

#[async_trait]
impl Job for MangaCacheJob {
    fn kind(&self) -> TaskKind {
        TaskKind::MangaCache
    }

    async fn run(&self, ctx: JobCtx) -> JobOutcome {
        run_cache(&ctx).await
    }
}

#[async_trait]
impl Job for MangaPackJob {
    fn kind(&self) -> TaskKind {
        TaskKind::MangaPack
    }

    async fn run(&self, ctx: JobCtx) -> JobOutcome {
        run_pack(&ctx).await
    }
}

#[async_trait]
impl Job for MangaBuildJob {
    fn kind(&self) -> TaskKind {
        TaskKind::MangaBuild
    }

    async fn run(&self, ctx: JobCtx) -> JobOutcome {
        run_build(&ctx).await
    }
}

// ---------------------------------------------------------------------------
// Scrape
// ---------------------------------------------------------------------------

/// Resolved scrape target: the library row id plus the draft sidecar that was
/// consumed to create it (if any).
struct ScrapeTarget {
    manga_id: String,
    payload: Option<PathBuf>,
}

/// Draft sidecar written by the queue layer for the pack→upload→scrape chain.
/// Shape: `{"draft":{"title":…,"author":…,"series":…,"volume":…,"chapter":…,"format":"cbz"}}`
/// (Dart `_followUpPackedUpload` + `MangaLibraryService.draftForComicFile`).
#[derive(Debug, Clone, Deserialize)]
struct ScrapeDraftPayload {
    #[serde(default)]
    draft: Option<ScrapeDraftSeed>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScrapeDraftSeed {
    #[serde(default)]
    title: String,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    series: Option<String>,
    #[serde(default)]
    volume: Option<String>,
    #[serde(default, alias = "chapterNumber")]
    chapter: Option<String>,
    #[serde(default)]
    format: Option<String>,
}

/// Resolve the row scraped by this task. `local_path == "manga:<id>"` keeps the
/// old behaviour; otherwise an existing `payload_path` sidecar is turned into a
/// row through [`save_draft`] (same rule as Dart `upsertFromDraft`: the remote
/// path comes from the task, the title falls back to the file stem, and the
/// format defaults to `cbz`).
async fn resolve_scrape_target(ctx: &JobCtx) -> Result<ScrapeTarget, JobOutcome> {
    if let Ok(id) = manga_id_from_task(&ctx.task.local_path) {
        return Ok(ScrapeTarget {
            manga_id: id,
            payload: None,
        });
    }
    let path = ctx
        .task
        .payload_path
        .as_deref()
        .filter(|path| !path.trim().is_empty())
        .map(PathBuf::from);
    let Some(path) = path else {
        return Err(failed("err.unknown|no manga id", FailureCategory::State));
    };
    let bytes = match fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(failed(
                "err.unknown|manga scrape payload missing",
                FailureCategory::State,
            ))
        }
        Err(error) => {
            return Err(failed(
                format!("err.local|read manga scrape payload: {error}"),
                FailureCategory::Other,
            ))
        }
    };
    let payload: ScrapeDraftPayload = serde_json::from_slice(&bytes).map_err(|error| {
        failed(
            format!("err.unknown|bad manga scrape payload: {error}"),
            FailureCategory::State,
        )
    })?;
    let Some(seed) = payload.draft else {
        return Err(failed(
            "err.unknown|manga scrape payload missing draft",
            FailureCategory::State,
        ));
    };
    let format = seed
        .format
        .as_deref()
        .map(str::trim)
        .filter(|format| !format.is_empty())
        .unwrap_or("cbz")
        .to_string();
    let draft = Draft {
        remote_path: ctx.task.remote_path.clone(),
        title: seed.title,
        author: non_empty_opt(&seed.author),
        series: non_empty_opt(&seed.series),
        volume: non_empty_opt(&seed.volume),
        chapter_number: non_empty_opt(&seed.chapter),
        format,
        pages: Vec::new(),
        cover: None,
    };
    let manga_id = save_draft(&ctx.db, &draft, None, &ctx.task.source_name)?;
    ctx.progress.note(&format!(
        "manga draft created: {manga_id} ({} / {})",
        ctx.task.source_name, ctx.task.remote_path
    ));
    Ok(ScrapeTarget {
        manga_id,
        payload: Some(path),
    })
}

/// Drop the consumed draft sidecar once the scrape succeeded. On failure the
/// file stays so the retry can resolve the same (deterministic) row id again.
async fn consume_scrape_payload(payload: Option<&Path>) {
    if let Some(path) = payload {
        let _ = fs::remove_file(path).await;
    }
}

/// `_runMangaScrape`: read the packed CBZ, adopt ComicInfo metadata, replace
/// the page list with the archive's image entries, and keep the first page as
/// the cover.
async fn run_scrape(ctx: &JobCtx) -> JobOutcome {
    let (manga_id, payload) = match resolve_scrape_target(ctx).await {
        Ok(target) => (target.manga_id, target.payload),
        Err(outcome) => return outcome,
    };
    let row = match load_manga(ctx, &manga_id) {
        Ok(row) => row,
        Err(outcome) => return outcome,
    };
    if !is_cbz(&row.format) {
        // Dart: non-CBZ archives have nothing to scrape; the task still counts.
        ctx.progress.units(1, Some(1));
        consume_scrape_payload(payload.as_deref()).await;
        return JobOutcome::Completed;
    }

    ctx.progress.units(0, Some(2));
    let remote = ctx.task.remote_path.clone();
    let scratch = ctx
        .paths
        .payload_dir
        .join(format!("{}.scrape.cbz", ctx.task.id));
    let total = match download_remote_to(ctx, &remote, &scratch).await {
        Ok(total) => total,
        Err(outcome) => return outcome,
    };
    if total == 0 {
        let _ = fs::remove_file(&scratch).await;
        return failed(
            format!("err.cloud.notFound|{remote}"),
            FailureCategory::Other,
        );
    }

    let mut archive = match cbz::open_cbz_path(&scratch) {
        Ok(archive) => archive,
        Err(error) => {
            let _ = fs::remove_file(&scratch).await;
            return failed(error, FailureCategory::VfsData);
        }
    };

    let comic_info = archive.comic_info();
    let image_names = archive.sorted_image_names();
    let image_indices = archive.sorted_image_indices();
    let cover = match image_indices.first() {
        Some(index) => match archive.read_entry(*index) {
            Ok(bytes) if !bytes.is_empty() => Some(bytes),
            _ => None,
        },
        None => None,
    };
    drop(archive);
    let _ = fs::remove_file(&scratch).await;

    let pages: Vec<MangaPage> = image_names
        .iter()
        .enumerate()
        .map(|(index, name)| MangaPage {
            manga_id: manga_id.clone(),
            page_index: index as i64,
            rel_path: name.clone(),
        })
        .collect();

    // Write the cover bytes first so the metadata write below can flip
    // `cover_state` in the same revision instead of bumping `rev` twice.
    let mut has_cover = false;
    if let Some(bytes) = cover {
        match write_cover(ctx, &manga_id, &bytes).await {
            Ok(written) => has_cover = written,
            Err(error) => ctx.progress.note(&format!("cover write failed: {error}")),
        }
    }

    let mut updated = row.clone();
    updated.page_count = pages.len() as i64;
    if has_cover {
        updated.cover_state = 2;
    }
    if let Some(info) = &comic_info {
        if let Some(title) = non_empty_opt(&info.title) {
            updated.title = title;
        }
        if let Some(writer) = non_empty_opt(&info.writer) {
            updated.author = Some(writer);
        }
        if let Some(series) = non_empty_opt(&info.series) {
            updated.series = Some(series);
        }
    }
    updated.rev += 1;
    updated.updated_at = now_rfc3339();
    // `upsert_manga` is `INSERT OR REPLACE`, and `manga_pages.manga_id` is
    // `ON DELETE CASCADE`, so the row has to be rewritten *before* the pages.
    if let Err(error) = ctx.db.upsert_manga(&updated) {
        return failed(format!("err.storage|{error}"), FailureCategory::State);
    }
    if let Err(error) = ctx.db.replace_pages(&manga_id, &pages) {
        return failed(format!("err.storage|{error}"), FailureCategory::State);
    }

    delete_pack_origin(ctx, &manga_id).await;

    consume_scrape_payload(payload.as_deref()).await;
    ctx.progress.units(2, Some(2));
    JobOutcome::Completed
}

/// `_deletePackOriginIfMarked`: a pack-then-scrape chain deletes the loose
/// `images` row it was built from once the CBZ page list exists.
async fn delete_pack_origin(ctx: &JobCtx, scraped_id: &str) {
    let Some(origin_id) = pack_delete_marker(ctx.task.cache_group_id.as_deref()) else {
        return;
    };
    if origin_id == scraped_id {
        return;
    }
    let Ok(Some(origin)) = ctx.db.get_manga(&origin_id) else {
        return;
    };
    if !origin.format.eq_ignore_ascii_case("images") {
        return;
    }
    clear_manga_artifacts(ctx, &origin).await;
    if let Err(error) = ctx.db.delete_manga(&origin_id) {
        ctx.progress
            .note(&format!("pack origin delete skipped: {error}"));
    }
}

/// Remove the cached archive, cover and prefetch pages of a manga row
/// (Dart `CacheService.clearMangaArtifacts`).
async fn clear_manga_artifacts(ctx: &JobCtx, row: &Manga) {
    let paths = CachePaths::new(&ctx.paths.cache_root);
    let archive = paths.manga_file_for(&row.remote_path, &row.source_name);
    let _ = fs::remove_file(part_path(&archive)).await;
    let _ = fs::remove_file(&archive).await;
    let _ = fs::remove_file(paths.manga_cover_file(&row.id)).await;
    let _ = fs::remove_dir_all(paths.manga_page_dir(&row.id)).await;
}

// ---------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------

/// `_runMangaCache`: ensure the chapter archive exists under the manga cache.
async fn run_cache(ctx: &JobCtx) -> JobOutcome {
    let manga_id = match manga_id_from_task(&ctx.task.local_path) {
        Ok(id) => id,
        Err(outcome) => return outcome,
    };
    let row = match load_manga(ctx, &manga_id) {
        Ok(row) => row,
        Err(outcome) => return outcome,
    };
    if !is_cbz(&row.format) {
        ctx.progress.units(1, Some(1));
        return JobOutcome::Completed;
    }

    let dest = CachePaths::new(&ctx.paths.cache_root)
        .manga_file_for(&ctx.task.remote_path, &ctx.task.source_name);
    let already_cached = matches!(fs::metadata(&dest).await, Ok(meta) if meta.len() > 0);
    if !already_cached {
        let total = match download_remote_to(ctx, &ctx.task.remote_path, &dest).await {
            Ok(total) => total,
            Err(outcome) => return outcome,
        };
        if total == 0 {
            let _ = fs::remove_file(&dest).await;
            return failed(
                format!("err.cloud.notFound|{}", ctx.task.remote_path),
                FailureCategory::Other,
            );
        }
    }

    let size = match fs::metadata(&dest).await {
        Ok(meta) => meta.len(),
        Err(error) => return local_failure("err.local|stat manga cache file", error),
    };
    // Dart `registerMangaFileWritten`; bookkeeping never fails the task.
    register_manga_cache_entry(ctx, &dest, size).await;
    ctx.progress.units(1, Some(1));
    JobOutcome::Completed
}

/// Schema-v17 cache index registration for a landed manga archive. Mirrors the
/// download executor: upsert the `cache_entries` row, then bind it into any
/// `cache_group_id` the task carries.
async fn register_manga_cache_entry(ctx: &JobCtx, local_path: &Path, size: u64) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default();
    let path = local_path.display().to_string();
    let entry = CacheEntryRow {
        path: path.clone(),
        kind: "manga".to_string(),
        size: size as i64,
        accessed_at: now,
        created_at: now,
        account_id: Some(ctx.task.account_id.clone()),
        remote_path: Some(ctx.task.remote_path.clone()),
    };
    if let Err(error) = ctx.db.cache_put_entry(&entry) {
        ctx.progress
            .note(&format!("cache_put_entry(manga) skipped: {error}"));
        return;
    }
    if let Some(group_id) = ctx
        .task
        .cache_group_id
        .as_deref()
        .filter(|group| !group.is_empty())
    {
        if let Err(error) = ctx.db.cache_bind_group(group_id, &[path]) {
            ctx.progress
                .note(&format!("cache_bind_group({group_id}) skipped: {error}"));
        }
    }
}

// ---------------------------------------------------------------------------
// Pack
// ---------------------------------------------------------------------------

/// `_runMangaPack`: download every page of an `images` row and write one CBZ.
///
/// The finished archive plus the upload task fields are returned in
/// [`JobOutcome::Partial`]; this executor never enqueues work itself.
async fn run_pack(ctx: &JobCtx) -> JobOutcome {
    let manga_id = match manga_id_from_task(&ctx.task.local_path) {
        Ok(id) => id,
        Err(outcome) => return outcome,
    };
    if let Err(outcome) = load_manga(ctx, &manga_id) {
        return outcome;
    }
    if ctx.task.account_id.trim().is_empty() {
        return failed(
            format!("err.sourceUnbound|{}", ctx.task.source_name),
            FailureCategory::State,
        );
    }
    let pages = match ctx.db.list_pages(&manga_id) {
        Ok(pages) => pages,
        Err(error) => return failed(format!("err.storage|{error}"), FailureCategory::State),
    };
    if pages.is_empty() {
        return failed("err.unknown|no pages to pack", FailureCategory::Other);
    }

    let total = pages.len() as u32;
    ctx.progress.units(0, Some(total));

    let dir = ctx.paths.payload_dir.clone();
    if let Err(error) = fs::create_dir_all(&dir).await {
        return local_failure("err.local|create payload dir", error);
    }
    let archive = dir.join(format!("{}.cbz", ctx.task.id));

    let mut packed: Vec<PackPage> = Vec::with_capacity(pages.len());
    for (index, page) in pages.iter().enumerate() {
        if ctx.cancelled() {
            let _ = fs::remove_file(&archive).await;
            return JobOutcome::Cancelled;
        }
        let bytes = match ctx.vfs.get_content(&page.rel_path).await {
            Ok(bytes) => bytes,
            Err(DriverError::NotFound(message)) => {
                let _ = fs::remove_file(&archive).await;
                return failed(
                    format!("err.cloud.notFound|{} ({message})", page.rel_path),
                    FailureCategory::Other,
                );
            }
            Err(error) => {
                let _ = fs::remove_file(&archive).await;
                return failed(format!("{error}"), FailureCategory::Dio);
            }
        };
        packed.push(PackPage {
            name: format!(
                "{:05}_{}",
                index + 1,
                cbz::entry_basename(&page.rel_path)
            ),
            data: bytes,
        });
        ctx.progress.units(index as u32 + 1, Some(total));
    }

    if let Err(error) = cbz::write_cbz(&archive, &packed, None) {
        let _ = fs::remove_file(&archive).await;
        return failed(error, FailureCategory::Other);
    }

    let upload_source = pack_upload_source(ctx);
    let origin = pack_upload_origin(ctx).unwrap_or_else(|| manga_id.clone());
    let result = json!({
        "archive": archive.display().to_string(),
        "upload_remote_path": ctx.task.remote_path,
        "upload_source_name": upload_source,
        "display_name": ctx.task.file_name,
        "cache_group_id": format!("wmm-pack-origin:{origin}"),
    });
    ctx.progress.note(&format!("packed {} pages for upload", packed.len()));
    JobOutcome::Partial { result }
}

/// Destination source from the `wmm-pack-up:` marker, else the task source.
fn pack_upload_source(ctx: &JobCtx) -> String {
    if let Some(marker) = ctx.task.cache_group_id.as_deref() {
        if let Some(rest) = marker.strip_prefix("wmm-pack-up:") {
            if let Ok(value) = serde_json::from_str::<Value>(rest.trim()) {
                if let Some(dest) = value.get("dest").and_then(Value::as_str) {
                    if !dest.trim().is_empty() {
                        return dest.trim().to_string();
                    }
                }
            }
        }
    }
    ctx.task.source_name.clone()
}

/// Origin manga row from the `wmm-pack-up:` marker, when present.
fn pack_upload_origin(ctx: &JobCtx) -> Option<String> {
    let marker = ctx.task.cache_group_id.as_deref()?;
    let rest = marker.strip_prefix("wmm-pack-up:")?;
    let value: Value = serde_json::from_str(rest.trim()).ok()?;
    value
        .get("origin")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .map(str::to_string)
}

/// Origin row from a `wmm-pack-del:<json id>` marker.
fn pack_delete_marker(marker: Option<&str>) -> Option<String> {
    let rest = marker?.trim().strip_prefix("wmm-pack-del:")?.trim();
    let id = serde_json::from_str::<String>(rest)
        .ok()
        .unwrap_or_else(|| rest.trim_matches('"').to_string());
    if id.is_empty() {
        None
    } else {
        Some(id)
    }
}

// ---------------------------------------------------------------------------
// Build
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
enum BuildMode {
    #[serde(alias = "merged")]
    #[default]
    Merged,
    #[serde(alias = "per_dir", alias = "perDir")]
    PerDir,
}

/// Sidecar payload written by `enqueueMangaBuild` (Dart `MangaBuildPayload`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MangaBuildPayload {
    #[serde(default)]
    mode: BuildMode,
    // The driver in `JobCtx::vfs` is already account-scoped; the field stays so
    // the sidecar round-trips the Dart payload shape verbatim.
    #[allow(dead_code)]
    #[serde(default)]
    account_id: String,
    #[serde(default)]
    source_name: String,
    #[serde(default)]
    remote_path: String,
    #[serde(default)]
    folder_name: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    series_title: Option<String>,
    #[serde(default = "default_true")]
    include_subdirs: bool,
    #[serde(default = "default_max_depth")]
    max_depth: u32,
    #[serde(default = "default_max_entries")]
    max_entries: usize,
    #[serde(default)]
    image_extensions: Vec<String>,
    #[serde(default)]
    target_manga_id: Option<String>,
    #[serde(default)]
    local_file_uris: Option<Vec<LocalFileEntry>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LocalFileEntry {
    #[serde(default)]
    uri: String,
    #[serde(default)]
    relative_path: String,
    // Archive entry names use the basename of the materialised file (Dart does
    // the same); the display name is still part of the sidecar contract.
    #[allow(dead_code)]
    #[serde(default)]
    file_name: String,
}

fn default_true() -> bool {
    true
}

fn default_max_depth() -> u32 {
    1
}

fn default_max_entries() -> usize {
    2000
}

impl MangaBuildPayload {
    fn walk_plan(&self) -> WalkPlan {
        let extensions = if self.image_extensions.is_empty() {
            cbz::IMAGE_EXTENSIONS.iter().map(|ext| (*ext).to_string()).collect()
        } else {
            self.image_extensions
                .iter()
                .map(|ext| ext.trim().trim_start_matches('.').to_ascii_lowercase())
                .filter(|ext| !ext.is_empty())
                .collect()
        };
        WalkPlan {
            include_subdirs: self.include_subdirs,
            max_depth: self.max_depth.min(MAX_WALK_DEPTH),
            max_entries: self.max_entries.clamp(1, MAX_WALK_ENTRIES),
            extensions,
        }
    }
}

/// Directory walker limits (Dart `WalkOptions`).
struct WalkPlan {
    include_subdirs: bool,
    max_depth: u32,
    max_entries: usize,
    extensions: Vec<String>,
}

/// A still-to-be-written manga row. Mirrors Dart `MangaIngestDraft`.
struct Draft {
    remote_path: String,
    title: String,
    author: Option<String>,
    series: Option<String>,
    volume: Option<String>,
    chapter_number: Option<String>,
    format: String,
    pages: Vec<String>,
    cover: Option<String>,
}

/// `_runMangaBuild`: sidecar payload → merged book or one book per image dir.
async fn run_build(ctx: &JobCtx) -> JobOutcome {
    let payload = match read_build_payload(ctx).await {
        Ok(payload) => payload,
        Err(outcome) => return outcome,
    };
    if let Some(entries) = payload.local_file_uris.clone() {
        return run_build_local(ctx, &payload, &entries).await;
    }

    let plan = payload.walk_plan();
    let files = match walk_image_files(ctx, &payload.remote_path, &plan).await {
        Ok(files) => files,
        Err(outcome) => return outcome,
    };
    if files.is_empty() {
        return failed(
            format!("err.unknown|no images under {}", payload.remote_path),
            FailureCategory::Other,
        );
    }

    match payload.mode {
        BuildMode::Merged => run_build_merged(ctx, &payload, files).await,
        BuildMode::PerDir => run_build_per_dir(ctx, &payload, files).await,
    }
}

async fn run_build_merged(
    ctx: &JobCtx,
    payload: &MangaBuildPayload,
    files: Vec<(String, String)>,
) -> JobOutcome {
    ctx.progress.units(0, Some(1));
    let mut pages: Vec<String> = files
        .iter()
        .map(|(dir, name)| rel_from_root(&payload.remote_path, dir, name))
        .collect();
    pages.sort_by_key(|page| cbz::page_sort_key(page));

    let draft = Draft {
        remote_path: normalize_manga_remote_path(&payload.remote_path),
        title: payload.title.clone(),
        author: payload.author.clone(),
        series: payload.series_title.clone(),
        volume: None,
        chapter_number: None,
        format: "images".to_string(),
        cover: pages.first().cloned(),
        pages,
    };
    let id = match save_draft(
        &ctx.db,
        &draft,
        payload.target_manga_id.as_deref(),
        &payload.source_name,
    ) {
        Ok(id) => id,
        Err(outcome) => return outcome,
    };
    if let Some(cover) = draft.cover.as_deref() {
        write_draft_cover(ctx, &id, &payload.remote_path, cover).await;
    }
    ctx.progress.units(1, Some(1));
    JobOutcome::Completed
}

async fn run_build_per_dir(
    ctx: &JobCtx,
    payload: &MangaBuildPayload,
    files: Vec<(String, String)>,
) -> JobOutcome {
    let mut by_dir: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (dir, name) in files {
        by_dir.entry(dir).or_default().push(name);
    }
    let mut dirs: Vec<(String, Vec<String>)> = by_dir.into_iter().collect();
    dirs.sort_by_key(|dir| dir.0.to_ascii_lowercase());
    if dirs.is_empty() {
        return failed(
            format!("err.unknown|no images under {}", payload.remote_path),
            FailureCategory::Other,
        );
    }

    let total = dirs.len() as u32;
    let single = dirs.len() == 1;
    ctx.progress.units(0, Some(total));
    for (index, (dir, mut names)) in dirs.into_iter().enumerate() {
        if ctx.cancelled() {
            return JobOutcome::Cancelled;
        }
        names.sort_by_key(|name| cbz::page_sort_key(name));
        let folder = normalize_manga_remote_path(&dir);
        let folder_name = last_segment(&folder).to_string();
        let pages: Vec<String> = names
            .iter()
            .map(|name| format!("{}/{}", folder.trim_end_matches('/'), name))
            .collect();
        let draft = Draft {
            remote_path: folder.clone(),
            title: folder_name,
            author: None,
            series: None,
            volume: None,
            chapter_number: None,
            format: "images".to_string(),
            cover: pages.first().cloned(),
            pages,
        };
        let target = if single {
            payload.target_manga_id.as_deref()
        } else {
            None
        };
        let id = match save_draft(&ctx.db, &draft, target, &payload.source_name) {
            Ok(id) => id,
            Err(outcome) => return outcome,
        };
        if let Some(cover) = draft.cover.as_deref() {
            write_draft_cover(ctx, &id, &folder, cover).await;
        }
        ctx.progress.units(index as u32 + 1, Some(total));
    }
    JobOutcome::Completed
}

/// `_runMangaBuildFromLocalUris`: materialise SAF URIs, pack one CBZ per
/// relative directory, register the remote placeholder row, and hand the upload
/// list back as the job result.
async fn run_build_local(
    ctx: &JobCtx,
    payload: &MangaBuildPayload,
    entries: &[LocalFileEntry],
) -> JobOutcome {
    let mut by_dir: BTreeMap<String, Vec<LocalFileEntry>> = BTreeMap::new();
    for entry in entries {
        let dir = entry
            .relative_path
            .rsplit_once('/')
            .map(|(dir, _)| dir.to_string())
            .unwrap_or_default();
        by_dir.entry(dir).or_default().push(entry.clone());
    }
    let mut dirs: Vec<(String, Vec<LocalFileEntry>)> = by_dir.into_iter().collect();
    dirs.sort_by_key(|dir| dir.0.to_ascii_lowercase());
    if dirs.is_empty() {
        return failed(
            "err.unknown|no images in selected folder",
            FailureCategory::Other,
        );
    }

    let dir = ctx.paths.payload_dir.clone();
    if let Err(error) = fs::create_dir_all(&dir).await {
        return local_failure("err.local|create payload dir", error);
    }

    let total = dirs.len() as u32;
    let single = dirs.len() == 1;
    ctx.progress.units(0, Some(total));
    let mut uploads: Vec<Value> = Vec::new();
    let mut failed_items: Vec<Value> = Vec::new();

    for (index, (relative_dir, mut files)) in dirs.into_iter().enumerate() {
        if ctx.cancelled() {
            return JobOutcome::Cancelled;
        }
        files.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
        let dir_name = if relative_dir.is_empty() {
            payload.folder_name.clone()
        } else {
            last_segment(&relative_dir).to_string()
        };

        let mut local_paths: Vec<PathBuf> = Vec::new();
        for entry in &files {
            let uri = entry.uri.trim();
            if uri.is_empty() {
                failed_items.push(json!({
                    "path": entry.relative_path,
                    "error": "err.unknown|empty local uri",
                }));
                continue;
            }
            match materialize(ctx, uri).await {
                Ok(path) => local_paths.push(path),
                Err(error) => failed_items.push(json!({
                    "path": entry.relative_path,
                    "error": error,
                })),
            }
        }
        if local_paths.is_empty() {
            ctx.progress.units(index as u32 + 1, Some(total));
            continue;
        }

        let title = if single {
            non_empty(&payload.title).unwrap_or_else(|| dir_name.clone())
        } else {
            dir_name.clone()
        };
        let cbz_name = format!("{}.cbz", sanitize_file_name(&title));
        let archive = ctx
            .paths
            .payload_dir
            .join(format!("{}_{}.cbz", ctx.task.id, index));

        let mut pages: Vec<PackPage> = Vec::with_capacity(local_paths.len());
        let mut pack_error: Option<String> = None;
        for (page_index, path) in local_paths.iter().enumerate() {
            match fs::read(path).await {
                Ok(data) => pages.push(PackPage {
                    name: format!(
                        "{:05}_{}",
                        page_index + 1,
                        filename_of(path)
                    ),
                    data,
                }),
                Err(error) => {
                    pack_error = Some(format!("err.local|read page: {error}"));
                    break;
                }
            }
        }
        if pack_error.is_none() {
            if let Err(error) = cbz::write_cbz(&archive, &pages, None) {
                pack_error = Some(error);
            }
        }
        // The platform handed us temp copies (Dart `readUriToCache` files);
        // their lifetime ends with this directory.
        for path in &local_paths {
            let _ = fs::remove_file(path).await;
        }
        if let Some(error) = pack_error {
            let _ = fs::remove_file(&archive).await;
            failed_items.push(json!({
                "path": if relative_dir.is_empty() { dir_name } else { relative_dir.clone() },
                "error": error,
            }));
            ctx.progress.units(index as u32 + 1, Some(total));
            continue;
        }

        let remote_base = payload.remote_path.trim().trim_end_matches('/');
        let upload_remote_path = if remote_base.is_empty() {
            format!("/{cbz_name}")
        } else {
            format!("{remote_base}/{cbz_name}")
        };
        let draft = Draft {
            remote_path: normalize_manga_remote_path(&upload_remote_path),
            title: title.clone(),
            author: payload.author.clone(),
            series: payload.series_title.clone(),
            volume: None,
            chapter_number: None,
            format: "cbz".to_string(),
            pages: Vec::new(),
            cover: None,
        };
        let registered = match save_draft(&ctx.db, &draft, None, &payload.source_name) {
            Ok(id) => id,
            Err(outcome) => {
                let _ = fs::remove_file(&archive).await;
                return outcome;
            }
        };
        uploads.push(json!({
            "archive": archive.display().to_string(),
            "upload_remote_path": upload_remote_path,
            "upload_source_name": payload.source_name,
            "display_name": cbz_name,
            "cache_group_id": format!("wmm-pack-origin:{registered}"),
        }));
        ctx.progress.units(index as u32 + 1, Some(total));
    }

    if uploads.is_empty() {
        let error = failed_items
            .first()
            .and_then(|item| item.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("err.unknown|no CBZ could be packed")
            .to_string();
        return JobOutcome::Failed {
            error,
            category: FailureCategory::Other,
        };
    }

    JobOutcome::Partial {
        result: json!({
            "ok": uploads.len(),
            "failed": failed_items,
            "uploads": uploads,
        }),
    }
}

/// Read (and consume) the build sidecar. Dart deletes it right after the read.
async fn read_build_payload(ctx: &JobCtx) -> Result<MangaBuildPayload, JobOutcome> {
    let path = ctx
        .task
        .payload_path
        .as_deref()
        .or(ctx.task.local_path.as_deref())
        .filter(|path| !path.trim().is_empty())
        .map(PathBuf::from);
    let Some(path) = path else {
        return Err(failed(
            "err.unknown|manga build payload missing",
            FailureCategory::State,
        ));
    };
    let bytes = match fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(failed(
                "err.unknown|manga build payload missing",
                FailureCategory::State,
            ))
        }
        Err(error) => {
            return Err(failed(
                format!("err.local|read manga build payload: {error}"),
                FailureCategory::Other,
            ))
        }
    };
    let _ = fs::remove_file(&path).await;
    serde_json::from_slice(&bytes).map_err(|error| {
        failed(
            format!("err.unknown|bad manga build payload: {error}"),
            FailureCategory::State,
        )
    })
}

/// Breadth-first image walk rooted at `root`. Returns `(directory, name)` pairs
/// in listing order; callers sort by [`cbz::page_sort_key`].
async fn walk_image_files(
    ctx: &JobCtx,
    root: &str,
    plan: &WalkPlan,
) -> Result<Vec<(String, String)>, JobOutcome> {
    let root = normalize_manga_remote_path(root);
    let depth_limit = if plan.include_subdirs { plan.max_depth } else { 0 };
    let mut queue: VecDeque<(String, u32)> = VecDeque::new();
    queue.push_back((root, 0));
    let mut files: Vec<(String, String)> = Vec::new();
    let mut seen = 0usize;

    while let Some((dir, depth)) = queue.pop_front() {
        if ctx.cancelled() {
            return Err(JobOutcome::Cancelled);
        }
        let items: Vec<CloudFileItem> = ctx.vfs.list(&dir).await.map_err(|error| {
            JobOutcome::Failed {
                error: format!("{error}"),
                category: FailureCategory::Dio,
            }
        })?;
        for item in items {
            seen += 1;
            if seen > plan.max_entries {
                break;
            }
            if item.is_dir {
                if depth < depth_limit {
                    queue.push_back((join_remote(&dir, &item.name), depth + 1));
                }
            } else if is_image_ext(&item.name, &plan.extensions) {
                files.push((dir.clone(), item.name));
            }
        }
        if seen > plan.max_entries {
            break;
        }
    }
    Ok(files)
}

/// `MangaLibraryService.upsertFromDraft` / `rescanLoose`.
///
/// `target` rescans an existing row (id preserved) the way `rescanLoose` does;
/// otherwise a deterministic id is minted from source + remote path.
fn save_draft(
    db: &LibraryDatabase,
    draft: &Draft,
    target: Option<&str>,
    source_name: &str,
) -> Result<String, JobOutcome> {
    let source = normalize_manga_source_name(source_name);
    let remote = normalize_manga_remote_path(&draft.remote_path);
    let existing = if let Some(id) = target.filter(|id| !id.is_empty()) {
        match db.get_manga(id) {
            Ok(row) => Some((id.to_string(), row)),
            Err(error) => {
                return Err(failed(
                    format!("err.storage|{error}"),
                    FailureCategory::State,
                ))
            }
        }
    } else {
        None
    };

    if let Some((id, row)) = existing {
        let Some(mut updated) = row else {
            return Err(failed(
                format!("err.mangaMissing|{id}"),
                FailureCategory::State,
            ));
        };
        updated.remote_path = remote.clone();
        updated.page_count = draft.pages.len() as i64;
        updated.rev += 1;
        updated.updated_at = now_rfc3339();
        if let Err(error) = db.upsert_manga(&updated) {
            return Err(failed(
                format!("err.storage|{error}"),
                FailureCategory::State,
            ));
        }
        replace_pages(db, &id, &draft.pages)?;
        return Ok(id);
    }

    let id = manga_id_for_remote(&source, &remote);
    let previous = match db.get_manga(&id) {
        Ok(row) => row,
        Err(error) => {
            return Err(failed(
                format!("err.storage|{error}"),
                FailureCategory::State,
            ))
        }
    };
    let title = non_empty(&draft.title).unwrap_or_else(|| title_from_path(&remote));
    let row = Manga {
        id: id.clone(),
        source_name: source,
        remote_path: remote,
        title,
        author: non_empty_opt(&draft.author),
        series: non_empty_opt(&draft.series),
        volume: non_empty_opt(&draft.volume),
        chapter_number: non_empty_opt(&draft.chapter_number),
        page_count: draft.pages.len() as i64,
        cover_state: previous.as_ref().map(|row| row.cover_state).unwrap_or(0),
        format: draft.format.clone(),
        remote_missing: previous.as_ref().map(|row| row.remote_missing).unwrap_or(0),
        page_turn: previous.and_then(|row| row.page_turn),
        rev: match db.next_rev() {
            Ok(rev) => rev,
            Err(error) => {
                return Err(failed(
                    format!("err.storage.rev|{error}"),
                    FailureCategory::State,
                ))
            }
        },
        updated_at: now_rfc3339(),
    };
    if let Err(error) = db.upsert_manga(&row) {
        return Err(failed(
            format!("err.storage|{error}"),
            FailureCategory::State,
        ));
    }
    replace_pages(db, &id, &draft.pages)?;
    Ok(id)
}

fn replace_pages(db: &LibraryDatabase, id: &str, rel_paths: &[String]) -> Result<(), JobOutcome> {
    let pages: Vec<MangaPage> = rel_paths
        .iter()
        .enumerate()
        .map(|(index, rel_path)| MangaPage {
            manga_id: id.to_string(),
            page_index: index as i64,
            rel_path: rel_path.clone(),
        })
        .collect();
    db.replace_pages(id, &pages)
        .map_err(|error| failed(format!("err.storage|{error}"), FailureCategory::State))
}

/// Download first image of a draft and store it as the manga cover. Failures
/// are non-fatal, matching Dart `_writeLooseCover`.
async fn write_draft_cover(ctx: &JobCtx, manga_id: &str, root: &str, rel_path: &str) {
    let remote = absolute_page_path(root, rel_path);
    match ctx.vfs.get_content(&remote).await {
        Ok(bytes) if !bytes.is_empty() => match write_cover(ctx, manga_id, &bytes).await {
            Ok(true) => {
                if let Err(error) = ctx.db.set_manga_cover_state(manga_id, 2) {
                    ctx.progress
                        .note(&format!("cover state not stored: {error}"));
                }
            }
            Ok(false) => {}
            Err(error) => ctx.progress.note(&format!("cover write failed: {error}")),
        },
        Ok(_) => ctx.progress.note(&format!("empty cover image: {remote}")),
        Err(error) => ctx.progress.note(&format!("cover fetch failed: {remote}: {error}")),
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn failed(error: impl Into<String>, category: FailureCategory) -> JobOutcome {
    JobOutcome::Failed {
        error: error.into(),
        category,
    }
}

fn local_failure(context: &str, error: std::io::Error) -> JobOutcome {
    failed(format!("{context}: {error}"), FailureCategory::Other)
}

fn is_cbz(format: &str) -> bool {
    format.eq_ignore_ascii_case("cbz")
}

/// `task.localPath == "manga:<id>"`.
fn manga_id_from_task(local_path: &Option<String>) -> Result<String, JobOutcome> {
    let Some(path) = local_path.as_deref() else {
        return Err(failed("err.unknown|no manga id", FailureCategory::State));
    };
    let Some(id) = path.strip_prefix("manga:") else {
        return Err(failed("err.unknown|no manga id", FailureCategory::State));
    };
    if id.trim().is_empty() {
        return Err(failed("err.unknown|no manga id", FailureCategory::State));
    }
    Ok(id.trim().to_string())
}

fn load_manga(ctx: &JobCtx, manga_id: &str) -> Result<Manga, JobOutcome> {
    match ctx.db.get_manga(manga_id) {
        Ok(Some(row)) => Ok(row),
        Ok(None) => Err(failed(
            format!("err.mangaMissing|{manga_id}"),
            FailureCategory::State,
        )),
        Err(error) => Err(failed(
            format!("err.storage|{error}"),
            FailureCategory::State,
        )),
    }
}

/// Stream `remote_path` into `dest` through ranged reads. Existing `.part`
/// data is discarded: scrape/pack always need a complete fresh copy.
async fn download_remote_to(
    ctx: &JobCtx,
    remote_path: &str,
    dest: &Path,
) -> Result<u64, JobOutcome> {
    let tmp = part_path(dest);
    if let Some(parent) = tmp.parent() {
        if let Err(error) = fs::create_dir_all(parent).await {
            return Err(local_failure("err.local|create dir", error));
        }
    }
    let _ = fs::remove_file(&tmp).await;

    let total = match ctx.vfs.get(remote_path).await {
        Ok(item) => item.size,
        Err(DriverError::NotFound(message)) => {
            return Err(failed(
                format!("err.cloud.notFound|{remote_path} ({message})"),
                FailureCategory::Other,
            ))
        }
        Err(error) => return Err(failed(format!("{error}"), FailureCategory::Dio)),
    };

    let mut file = match fs::File::create(&tmp).await {
        Ok(file) => file,
        Err(error) => return Err(local_failure("err.local|create part", error)),
    };
    let mut written: u64 = 0;
    while written < total {
        if ctx.cancelled() {
            drop(file);
            let _ = fs::remove_file(&tmp).await;
            return Err(JobOutcome::Cancelled);
        }
        let end = (written + FETCH_CHUNK).min(total) - 1;
        let bytes = match ctx.vfs.get_content_range(remote_path, written, Some(end)).await {
            Ok(bytes) => bytes,
            Err(DriverError::NotFound(message)) => {
                drop(file);
                return Err(failed(
                    format!("err.cloud.notFound|{remote_path} ({message})"),
                    FailureCategory::Other,
                ));
            }
            Err(error) => {
                drop(file);
                return Err(failed(format!("{error}"), FailureCategory::Dio));
            }
        };
        if bytes.is_empty() {
            drop(file);
            return Err(failed(
                format!("err.download.range|short read at {written} of {total}"),
                FailureCategory::Dio,
            ));
        }
        if let Err(error) = file.write_all(&bytes).await {
            drop(file);
            return Err(local_failure("err.local|write part", error));
        }
        written += bytes.len() as u64;
        ctx.progress.bytes(written.min(total), Some(total));
    }
    if let Err(error) = file.flush().await {
        drop(file);
        return Err(local_failure("err.local|flush part", error));
    }
    drop(file);

    if fs::metadata(dest).await.is_ok() {
        if let Err(error) = fs::remove_file(dest).await {
            return Err(failed(
                format!("err.local|remove old cache file: {error}"),
                FailureCategory::Other,
            ));
        }
    }
    if let Err(error) = fs::rename(&tmp, dest).await {
        return Err(failed(
            format!("err.local|rename part: {error}"),
            FailureCategory::Other,
        ));
    }
    Ok(total)
}

/// Write cover bytes to `manga_cache/covers/<id[..16]>.jpg`. `false` when the
/// bytes are empty (Dart `writeMangaCoverThumb` returns false).
async fn write_cover(ctx: &JobCtx, manga_id: &str, bytes: &[u8]) -> Result<bool, String> {
    if bytes.is_empty() {
        return Ok(false);
    }
    let path = CachePaths::new(&ctx.paths.cache_root).manga_cover_file(manga_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .await
            .map_err(|error| format!("err.local|create cover dir: {error}"))?;
    }
    fs::write(&path, bytes)
        .await
        .map_err(|error| format!("err.local|write cover: {error}"))?;
    Ok(true)
}

/// `PlatformAction::Materialize` → local temp path (Dart
/// `PlatformExportService.readUriToCache`).
async fn materialize(ctx: &JobCtx, uri: &str) -> Result<PathBuf, String> {
    let reply = ctx
        .platform
        .request(PlatformAction::Materialize {
            uri: uri.to_string(),
        })
        .await
        .map_err(|error| format!("err.platform|{error}"))?;
    let ok = reply.get("ok").and_then(Value::as_bool).unwrap_or(true);
    if !ok {
        return Err(reply
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("err.unknown|failed to materialise SAF URI")
            .to_string());
    }
    let path = reply
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.trim().is_empty())
        .ok_or_else(|| "err.unknown|failed to materialise SAF URI".to_string())?;
    Ok(PathBuf::from(path))
}

fn is_image_ext(name: &str, extensions: &[String]) -> bool {
    let base = cbz::entry_basename(name);
    let Some((_, ext)) = base.rsplit_once('.') else {
        return false;
    };
    let ext = ext.to_ascii_lowercase();
    extensions.contains(&ext)
}

fn join_remote(root: &str, name: &str) -> String {
    let root = root.trim_end_matches('/');
    if root.is_empty() {
        format!("/{name}")
    } else {
        format!("{root}/{name}")
    }
}

fn absolute_page_path(root: &str, rel_path: &str) -> String {
    if rel_path.starts_with('/') {
        rel_path.to_string()
    } else {
        join_remote(root, rel_path)
    }
}

fn rel_from_root(root: &str, dir: &str, name: &str) -> String {
    let root = root.trim_end_matches('/');
    let dir = dir.trim_end_matches('/');
    let rest = if dir == root {
        String::new()
    } else if let Some(rest) = dir.strip_prefix(root) {
        rest.trim_start_matches('/').to_string()
    } else {
        dir.to_string()
    };
    if rest.is_empty() {
        name.to_string()
    } else {
        format!("{rest}/{name}")
    }
}

fn last_segment(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
}

fn filename_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn title_from_path(remote: &str) -> String {
    let name = last_segment(remote);
    match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem.to_string(),
        _ => name.to_string(),
    }
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn non_empty_opt(value: &Option<String>) -> Option<String> {
    value.as_deref().and_then(non_empty)
}

/// RFC-3339 UTC timestamp. `wdmm-jobs` does not depend on `chrono`; the
/// `SystemTime` → civil-date conversion is Howard Hinnant's `civil_from_days`.
fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    (year + i64::from(month <= 2), month as u32, day as u32)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    use crate::cbz::{self, ComicInfo};
    use crate::{JobPaths, ProgressSink};
    use wdmm_drivers::{ContentDeliveryKind, Result as VfsResult, WdmmVfs};
    use wdmm_queue::model::{CancelToken, QueueTask, TaskTarget};

    struct FakeVfs {
        files: HashMap<String, Vec<u8>>,
        dirs: HashMap<String, Vec<CloudFileItem>>,
    }

    impl FakeVfs {
        fn new() -> Self {
            Self {
                files: HashMap::new(),
                dirs: HashMap::new(),
            }
        }

        fn with_file(mut self, path: &str, data: Vec<u8>) -> Self {
            self.files.insert(path.to_string(), data);
            self
        }

        fn with_dir(mut self, path: &str, entries: &[(&str, bool)]) -> Self {
            let items = entries
                .iter()
                .map(|(name, is_dir)| CloudFileItem {
                    name: (*name).to_string(),
                    is_dir: *is_dir,
                    size: 0,
                    modified: None,
                    raw_url: None,
                    raw_headers: Vec::new(),
                    delivery: ContentDeliveryKind::NetworkDirect,
                    local_path: None,
                })
                .collect();
            self.dirs.insert(path.to_string(), items);
            self
        }
    }

    #[async_trait]
    impl WdmmVfs for FakeVfs {
        async fn list(&self, path: &str) -> VfsResult<Vec<CloudFileItem>> {
            Ok(self.dirs.get(path).cloned().unwrap_or_default())
        }

        async fn get(&self, path: &str) -> VfsResult<CloudFileItem> {
            match self.files.get(path) {
                Some(data) => Ok(CloudFileItem {
                    name: cbz::entry_basename(path).to_string(),
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

        async fn get_content_range(
            &self,
            path: &str,
            start: u64,
            end: Option<u64>,
        ) -> VfsResult<Vec<u8>> {
            let data = self
                .files
                .get(path)
                .ok_or_else(|| DriverError::NotFound(path.to_string()))?;
            let start = start as usize;
            if start >= data.len() {
                return Ok(Vec::new());
            }
            let last = end
                .map(|end| end as usize)
                .unwrap_or(data.len() - 1)
                .min(data.len() - 1);
            Ok(data[start..=last].to_vec())
        }
    }

    #[derive(Default)]
    struct Recorder {
        notes: std::sync::Mutex<Vec<String>>,
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

    fn build_ctx(
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

    fn task(kind: TaskKind, id: &str, local_path: Option<&str>) -> QueueTask {
        let mut task = QueueTask::new(
            id.to_string(),
            kind,
            "nas".to_string(),
            "acc".to_string(),
            "/Manga/Vol 1.cbz".to_string(),
            "Vol 1.cbz".to_string(),
            TaskTarget::Cache,
            "batch".to_string(),
            0,
        );
        task.local_path = local_path.map(str::to_string);
        task
    }

    fn manga(id: &str, format: &str, remote: &str) -> Manga {
        Manga {
            id: id.to_string(),
            source_name: "nas".to_string(),
            remote_path: remote.to_string(),
            title: "Old title".to_string(),
            author: None,
            series: None,
            volume: None,
            chapter_number: None,
            page_count: 0,
            cover_state: 0,
            format: format.to_string(),
            remote_missing: 0,
            page_turn: None,
            rev: 1,
            updated_at: "2025-01-01T00:00:00Z".to_string(),
        }
    }

    fn seed_pages(db: &LibraryDatabase, id: &str, rel_paths: &[&str]) {
        let pages: Vec<MangaPage> = rel_paths
            .iter()
            .enumerate()
            .map(|(index, rel)| MangaPage {
                manga_id: id.to_string(),
                page_index: index as i64,
                rel_path: (*rel).to_string(),
            })
            .collect();
        db.replace_pages(id, &pages).unwrap();
    }

    fn sample_cbz() -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.cbz");
        let pages = vec![
            PackPage {
                name: "001.jpg".into(),
                data: b"page-one".to_vec(),
            },
            PackPage {
                name: "002.jpg".into(),
                data: b"page-two".to_vec(),
            },
        ];
        let info = ComicInfo {
            title: Some("Volume One".into()),
            series: Some("Saga".into()),
            writer: Some("The Writer".into()),
            volume: Some("1".into()),
            number: Some("2".into()),
        };
        cbz::write_cbz(&path, &pages, Some(&info)).unwrap();
        std::fs::read(path).unwrap()
    }

    #[tokio::test]
    async fn pack_writes_ordered_cbz_and_reports_upload() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        let vfs = Arc::new(
            FakeVfs::new()
                .with_file("/M/01.jpg", b"one".to_vec())
                .with_file("/M/02.jpg", b"two".to_vec())
                .with_file("/M/10.jpg", b"ten".to_vec()),
        );
        let mut pack_task = task(TaskKind::MangaPack, "t-pack", Some("manga:m1"));
        pack_task.cache_group_id =
            Some("wmm-pack-up:{\"dest\":\"backup\",\"origin\":\"m-origin\"}".to_string());
        let ctx = build_ctx(
            dir.path(),
            pack_task,
            vfs,
            Arc::new(Recorder::default()),
            &db,
        );
        db.upsert_manga(&manga("m1", "images", "/Manga/Vol 1.cbz"))
            .unwrap();
        seed_pages(&db, "m1", &["/M/01.jpg", "/M/02.jpg", "/M/10.jpg"]);

        let outcome = MangaPackJob.run(ctx).await;
        let value = match outcome {
            JobOutcome::Partial { result } => result,
            other => panic!("expected Partial, got {other:?}"),
        };
        assert_eq!(value["upload_source_name"], "backup");
        assert_eq!(value["upload_remote_path"], "/Manga/Vol 1.cbz");
        assert_eq!(value["cache_group_id"], "wmm-pack-origin:m-origin");
        let archive = PathBuf::from(value["archive"].as_str().unwrap());
        assert!(archive.exists());
        let mut archive = cbz::open_cbz_path(&archive).unwrap();
        assert_eq!(
            archive.sorted_image_names(),
            vec!["00001_01.jpg", "00002_02.jpg", "00003_10.jpg"]
        );
        assert_eq!(archive.read_entry(2).unwrap(), b"ten");
    }

    #[tokio::test]
    async fn pack_without_pages_fails() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        let ctx = build_ctx(
            dir.path(),
            task(TaskKind::MangaPack, "t-pack-empty", Some("manga:m1")),
            Arc::new(FakeVfs::new()),
            Arc::new(Recorder::default()),
            &db,
        );
        db.upsert_manga(&manga("m1", "images", "/Manga/Vol 1.cbz"))
            .unwrap();
        match MangaPackJob.run(ctx).await {
            JobOutcome::Failed { error, .. } => {
                assert!(error.contains("no pages to pack"), "{error}")
            }
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn scrape_parses_comicinfo_and_replaces_pages() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        let vfs = Arc::new(FakeVfs::new().with_file("/Manga/Vol 1.cbz", sample_cbz()));
        let ctx = build_ctx(
            dir.path(),
            task(TaskKind::MangaScrape, "t-scrape", Some("manga:m1")),
            vfs,
            Arc::new(Recorder::default()),
            &db,
        );
        db.upsert_manga(&manga("m1", "cbz", "/Manga/Vol 1.cbz"))
            .unwrap();

        let outcome = MangaScrapeJob.run(ctx).await;
        assert!(matches!(outcome, JobOutcome::Completed), "{outcome:?}");

        let row = db.get_manga("m1").unwrap().unwrap();
        assert_eq!(row.title, "Volume One");
        assert_eq!(row.author.as_deref(), Some("The Writer"));
        assert_eq!(row.series.as_deref(), Some("Saga"));
        assert_eq!(row.page_count, 2);
        assert_eq!(row.cover_state, 2);
        assert_eq!(row.rev, 2);
        let pages = db.list_pages("m1").unwrap();
        assert_eq!(
            pages.iter().map(|p| p.rel_path.as_str()).collect::<Vec<_>>(),
            vec!["001.jpg", "002.jpg"]
        );

        let cover = CachePaths::new(dir.path().join("cache")).manga_cover_file("m1");
        assert_eq!(std::fs::read(cover).unwrap(), b"page-one");
    }

    #[tokio::test]
    async fn scrape_creates_row_from_payload_draft() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        let vfs = Arc::new(FakeVfs::new().with_file("/Manga/Vol 1.cbz", sample_cbz()));
        let payload_path = dir.path().join("scrape-draft.json");
        std::fs::write(
            &payload_path,
            r#"{"draft":{"title":"Payload Title","author":"Seed Author","series":"Seed Series","volume":"2","chapter":"3","format":"cbz"}}"#,
        )
        .unwrap();

        let mut scrape_task = task(TaskKind::MangaScrape, "t-scrape-draft", None);
        scrape_task.payload_path = Some(payload_path.display().to_string());
        let recorder = Arc::new(Recorder::default());
        let ctx = build_ctx(dir.path(), scrape_task, vfs, Arc::clone(&recorder), &db);

        let outcome = MangaScrapeJob.run(ctx).await;
        assert!(matches!(outcome, JobOutcome::Completed), "{outcome:?}");

        // Dart `upsertFromDraft` mints the id from source + remote path.
        let expected = manga_id_for_remote("nas", "/Manga/Vol 1.cbz");
        let row = db
            .get_manga(&expected)
            .unwrap()
            .unwrap_or_else(|| panic!("draft row {expected} not created"));
        assert_eq!(row.format, "cbz");
        // ComicInfo wins for title/author/series; scrape leaves the rest alone.
        assert_eq!(row.title, "Volume One");
        assert_eq!(row.author.as_deref(), Some("The Writer"));
        assert_eq!(row.series.as_deref(), Some("Saga"));
        assert_eq!(row.volume.as_deref(), Some("2"));
        assert_eq!(row.chapter_number.as_deref(), Some("3"));
        assert_eq!(row.page_count, 2);
        assert_eq!(db.list_pages(&expected).unwrap().len(), 2);
        assert!(
            recorder
                .notes
                .lock()
                .unwrap()
                .iter()
                .any(|note| note.contains("manga draft created")),
            "missing draft creation note"
        );
        assert!(!payload_path.exists(), "consumed sidecar must be removed");
    }

    #[tokio::test]
    async fn scrape_bad_payload_fails_without_creating_row() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        for body in ["not json at all", r#"{"unrelated":{}}"#] {
            let payload_path = dir.path().join("bad-scrape-draft.json");
            std::fs::write(&payload_path, body).unwrap();

            let mut scrape_task = task(TaskKind::MangaScrape, "t-scrape-bad", None);
            scrape_task.payload_path = Some(payload_path.display().to_string());
            let ctx = build_ctx(
                dir.path(),
                scrape_task,
                Arc::new(FakeVfs::new()),
                Arc::new(Recorder::default()),
                &db,
            );

            match MangaScrapeJob.run(ctx).await {
                JobOutcome::Failed { error, .. } => assert!(
                    error.contains("bad manga scrape payload")
                        || error.contains("missing draft"),
                    "{error}"
                ),
                other => panic!("expected failure, got {other:?}"),
            }
            assert!(db.list_mangas().unwrap().is_empty(), "no row on bad payload");
            assert!(payload_path.exists(), "failed payload stays for the retry");
        }
    }

    #[tokio::test]
    async fn scrape_keeps_manga_id_when_payload_present() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        let vfs = Arc::new(FakeVfs::new().with_file("/Manga/Vol 1.cbz", sample_cbz()));
        let payload_path = dir.path().join("scrape-draft.json");
        std::fs::write(
            &payload_path,
            r#"{"draft":{"title":"Payload Title","format":"cbz"}}"#,
        )
        .unwrap();

        let mut scrape_task = task(TaskKind::MangaScrape, "t-scrape-existing", Some("manga:m1"));
        scrape_task.payload_path = Some(payload_path.display().to_string());
        let ctx = build_ctx(
            dir.path(),
            scrape_task,
            vfs,
            Arc::new(Recorder::default()),
            &db,
        );
        db.upsert_manga(&manga("m1", "cbz", "/Manga/Vol 1.cbz"))
            .unwrap();

        assert!(matches!(
            MangaScrapeJob.run(ctx).await,
            JobOutcome::Completed
        ));
        assert_eq!(db.list_mangas().unwrap().len(), 1);
        let row = db.get_manga("m1").unwrap().unwrap();
        assert_eq!(row.title, "Volume One");
        assert!(
            db.get_manga(&manga_id_for_remote("nas", "/Manga/Vol 1.cbz"))
                .unwrap()
                .is_none(),
            "payload must not create a second row"
        );
        assert!(
            payload_path.exists(),
            "unused payload is left untouched for the owning task"
        );
    }

    #[tokio::test]
    async fn scrape_non_cbz_completes_without_touching_vfs() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        let ctx = build_ctx(
            dir.path(),
            task(TaskKind::MangaScrape, "t-scrape-img", Some("manga:m1")),
            Arc::new(FakeVfs::new()),
            Arc::new(Recorder::default()),
            &db,
        );
        db.upsert_manga(&manga("m1", "images", "/Manga/Vol 1"))
            .unwrap();
        assert!(matches!(
            MangaScrapeJob.run(ctx).await,
            JobOutcome::Completed
        ));
    }

    #[tokio::test]
    async fn cache_downloads_archive_into_manga_cache() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        let data = sample_cbz();
        let ctx = build_ctx(
            dir.path(),
            task(TaskKind::MangaCache, "t-cache", Some("manga:m1")),
            Arc::new(FakeVfs::new().with_file("/Manga/Vol 1.cbz", data.clone())),
            Arc::new(Recorder::default()),
            &db,
        );
        db.upsert_manga(&manga("m1", "cbz", "/Manga/Vol 1.cbz"))
            .unwrap();
        assert!(matches!(
            MangaCacheJob.run(ctx).await,
            JobOutcome::Completed
        ));
        let dest = CachePaths::new(dir.path().join("cache"))
            .manga_file_for("/Manga/Vol 1.cbz", "nas");
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert!(!part_path(&dest).exists());
        let entry = db
            .cache_get_entry(&dest.display().to_string())
            .unwrap()
            .expect("cache_entries row");
        assert_eq!(entry.kind, "manga");
        assert_eq!(entry.size, data.len() as i64);
        assert_eq!(entry.remote_path.as_deref(), Some("/Manga/Vol 1.cbz"));
        assert_eq!(entry.account_id.as_deref(), Some("acc"));
    }

    #[tokio::test]
    async fn build_merged_upserts_row_with_natural_page_order() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        let vfs = Arc::new(
            FakeVfs::new()
                .with_dir(
                    "/Manga/Vol",
                    &[("010.jpg", false), ("001.jpg", false), ("sub", true)],
                )
                .with_dir("/Manga/Vol/sub", &[("002.jpg", false)])
                .with_file("/Manga/Vol/001.jpg", b"first".to_vec())
                .with_file("/Manga/Vol/010.jpg", b"tenth".to_vec())
                .with_file("/Manga/Vol/sub/002.jpg", b"second".to_vec()),
        );
        let payload = json!({
            "mode": "merged",
            "accountId": "acc",
            "sourceName": "nas",
            "remotePath": "/Manga/Vol",
            "folderName": "Vol",
            "title": "My Volume",
            "imageExtensions": ["jpg"],
            "maxDepth": 1,
        });
        let payload_path = dir.path().join("payload.json");
        std::fs::write(&payload_path, payload.to_string()).unwrap();

        let mut build_task = task(TaskKind::MangaBuild, "t-build", None);
        build_task.file_name = "Vol".to_string();
        build_task.payload_path = Some(payload_path.display().to_string());
        let ctx = build_ctx(
            dir.path(),
            build_task,
            vfs,
            Arc::new(Recorder::default()),
            &db,
        );

        let outcome = MangaBuildJob.run(ctx).await;
        assert!(matches!(outcome, JobOutcome::Completed), "{outcome:?}");
        let id = manga_id_for_remote("nas", "/Manga/Vol");
        let row = db.get_manga(&id).unwrap().unwrap();
        assert_eq!(row.title, "My Volume");
        assert_eq!(row.format, "images");
        assert_eq!(row.page_count, 3);
        assert_eq!(row.cover_state, 2);
        let pages = db.list_pages(&id).unwrap();
        assert_eq!(
            pages.iter().map(|p| p.rel_path.as_str()).collect::<Vec<_>>(),
            vec!["001.jpg", "010.jpg", "sub/002.jpg"]
        );
        let cover = CachePaths::new(dir.path().join("cache")).manga_cover_file(&id);
        assert!(cover.exists());
        assert!(!payload_path.exists(), "sidecar must be consumed");
    }

    #[tokio::test]
    async fn build_per_dir_writes_one_row_per_directory() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        let vfs = Arc::new(
            FakeVfs::new()
                .with_dir("/Lib", &[("A", true), ("B", true)])
                .with_dir("/Lib/A", &[("1.jpg", false)])
                .with_dir("/Lib/B", &[("1.jpg", false)])
                .with_file("/Lib/A/1.jpg", b"a".to_vec())
                .with_file("/Lib/B/1.jpg", b"b".to_vec()),
        );
        let payload = json!({
            "mode": "per_dir",
            "sourceName": "nas",
            "remotePath": "/Lib",
            "folderName": "Lib",
            "title": "Lib",
        });
        let payload_path = dir.path().join("payload.json");
        std::fs::write(&payload_path, payload.to_string()).unwrap();
        let mut build_task = task(TaskKind::MangaBuild, "t-build-dirs", None);
        build_task.payload_path = Some(payload_path.display().to_string());
        let ctx = build_ctx(
            dir.path(),
            build_task,
            vfs,
            Arc::new(Recorder::default()),
            &db,
        );

        let outcome = MangaBuildJob.run(ctx).await;
        assert!(matches!(outcome, JobOutcome::Completed), "{outcome:?}");
        let mangas = db.list_mangas().unwrap();
        assert_eq!(mangas.len(), 2);
        let mut titles: Vec<&str> = mangas.iter().map(|row| row.title.as_str()).collect();
        titles.sort_unstable();
        assert_eq!(titles, vec!["A", "B"]);
    }

    #[tokio::test]
    async fn build_missing_payload_fails() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        let mut build_task = task(TaskKind::MangaBuild, "t-build-missing", None);
        build_task.payload_path = Some(dir.path().join("absent.json").display().to_string());
        let ctx = build_ctx(
            dir.path(),
            build_task,
            Arc::new(FakeVfs::new()),
            Arc::new(Recorder::default()),
            &db,
        );
        match MangaBuildJob.run(ctx).await {
            JobOutcome::Failed { error, .. } => {
                assert!(error.contains("manga build payload missing"), "{error}")
            }
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn build_local_uris_materialises_and_reports_uploads() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        let page_a = dir.path().join("a.jpg");
        let page_b = dir.path().join("b.jpg");
        std::fs::write(&page_a, b"a").unwrap();
        std::fs::write(&page_b, b"b").unwrap();

        let payload = json!({
            "mode": "merged",
            "sourceName": "nas",
            "remotePath": "/Lib",
            "folderName": "Lib",
            "title": "Batch",
            "localFileUris": [
                {"uri": "content://a", "relativePath": "dirA/a.jpg", "fileName": "a.jpg"},
                {"uri": "content://b", "relativePath": "dirA/b.jpg", "fileName": "b.jpg"},
            ],
        });
        let payload_path = dir.path().join("payload-local.json");
        std::fs::write(&payload_path, payload.to_string()).unwrap();

        let (client, mut rx) = crate::PlatformClient::channel(4);
        let path_a = page_a.display().to_string();
        let path_b = page_b.display().to_string();
        let responder = tokio::spawn(async move {
            let first = rx.recv().await.expect("first materialize");
            let _ = first.reply.send(Ok(json!({"ok": true, "path": path_a})));
            let second = rx.recv().await.expect("second materialize");
            let _ = second.reply.send(Ok(json!({"ok": true, "path": path_b})));
        });

        let mut build_task = task(TaskKind::MangaBuild, "t-build-local", None);
        build_task.payload_path = Some(payload_path.display().to_string());
        let mut ctx = build_ctx(
            dir.path(),
            build_task,
            Arc::new(FakeVfs::new()),
            Arc::new(Recorder::default()),
            &db,
        );
        ctx.platform = Arc::new(client);

        let outcome = MangaBuildJob.run(ctx).await;
        responder.await.unwrap();
        let value = match outcome {
            JobOutcome::Partial { result } => result,
            other => panic!("expected Partial, got {other:?}"),
        };
        let uploads = value["uploads"].as_array().unwrap();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0]["upload_remote_path"], "/Lib/Batch.cbz");
        let archive = PathBuf::from(uploads[0]["archive"].as_str().unwrap());
        assert!(archive.exists());
        let archive = cbz::open_cbz_path(&archive).unwrap();
        assert_eq!(
            archive.sorted_image_names(),
            vec!["00001_a.jpg", "00002_b.jpg"]
        );
        let registered = db
            .get_manga(&manga_id_for_remote("nas", "/Lib/Batch.cbz"))
            .unwrap()
            .unwrap();
        assert_eq!(registered.format, "cbz");
        assert_eq!(registered.title, "Batch");
    }

    #[test]
    fn rfc3339_formatter_matches_epochs() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(365), (1971, 1, 1));
        assert_eq!(civil_from_days(19_000), (2022, 1, 8));
    }
}