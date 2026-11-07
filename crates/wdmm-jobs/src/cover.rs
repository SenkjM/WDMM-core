//! `CoverRefresh`: fill in cover thumbnails the library already knows about.
//!
//! `retag_cloud` re-reads tags (and writes a thumbnail when the file happens to
//! carry one) but never records "this file has no cover", so rows without art
//! stay at `cover_state = 0` and would be re-read forever. This job is the
//! missing half: it probes exactly the rows whose state is still unknown (`0`)
//! or whose cached thumbnail vanished (`2` without the file), stores a thumbnail
//! when art is found, and records `cover_state = 1` when the source
//! authoritatively has none. A failed read leaves the state alone so the row is
//! picked up by a later run.
//!
//! Candidates come from the library itself, so the Dart side only has to enqueue
//! one task per account (no payload). A task runs with its account's driver, so
//! rows from other accounts are out of scope; an empty or `*` source keeps the
//! old "every row" behaviour for callers that still queue one global task.
//! Rows whose `remote_path` is a CUE virtual slice are skipped: they share their
//! album file and their covers come from the CUE scan.

use async_trait::async_trait;

use wdmm_queue::model::TaskKind;
use wdmm_queue::policy::FailureCategory;

use crate::ingest::probe_cover;
use crate::thumbs::{self, COVER_THUMB_SIZE};
use crate::{Job, JobCtx, JobOutcome};

pub struct CoverRefreshJob;

#[async_trait]
impl Job for CoverRefreshJob {
    fn kind(&self) -> TaskKind {
        TaskKind::CoverRefresh
    }

    async fn run(&self, ctx: JobCtx) -> JobOutcome {
        run(&ctx).await
    }
}

/// Whether a row is worth probing.
///
/// `1` = authoritatively "no cover" (skip), `0` = never determined, `2` = the
/// row claims art but the cached thumbnail is gone.
fn needs_probe(cover_state: i64, thumb_exists: bool) -> bool {
    match cover_state {
        0 => true,
        2 => !thumb_exists,
        _ => false,
    }
}

/// CUE slices are virtual rows sharing their album's file; their covers are
/// refreshed by the CUE scan, not by a per-track probe.
fn is_cue_virtual(remote_path: &str) -> bool {
    remote_path.contains("#cue:")
}

async fn run(ctx: &JobCtx) -> JobOutcome {
    let tracks = match ctx.db.list_tracks() {
        Ok(tracks) => tracks,
        Err(error) => {
            return JobOutcome::Failed {
                error: format!("err.storage.tracks|{error}"),
                category: FailureCategory::State,
            }
        }
    };

    // A task only has the driver of its own account. `account_id` on a library
    // row is the account's display name, which is what Dart queues as the
    // source name; `*`/empty marks a global task that never had a driver.
    let only_source = ctx.task.source_name.trim();
    let scoped = !only_source.is_empty() && only_source != "*";

    let candidates: Vec<_> = tracks
        .into_iter()
        .filter(|track| {
            if scoped && track.account_id != only_source {
                return false;
            }
            if is_cue_virtual(&track.remote_path) {
                return false;
            }
            let thumb =
                thumbs::thumb_path(&ctx.paths.cover_dir, &track.account_id, &track.remote_path);
            needs_probe(track.cover_state, thumb.exists())
        })
        .collect();

    let total = u32::try_from(candidates.len()).unwrap_or(u32::MAX);
    ctx.progress.units(0, Some(total));
    ctx.progress
        .note(&format!("cover refresh: {total} candidate(s)"));

    let mut failed: Vec<serde_json::Value> = Vec::new();
    let mut first_error: Option<(String, FailureCategory)> = None;
    let mut done = 0_u32;

    for track in &candidates {
        if ctx.cancelled() {
            return JobOutcome::Cancelled;
        }
        match refresh_one(ctx, track).await {
            Ok(()) => {}
            Err(error) => {
                ctx.progress.note(&format!(
                    "cover refresh failed: {}: {error}",
                    track.remote_path
                ));
                if first_error.is_none() {
                    first_error = Some((error.clone(), FailureCategory::Other));
                }
                failed.push(serde_json::json!({ "path": track.remote_path, "error": error }));
            }
        }
        done = done.saturating_add(1);
        ctx.progress.units(done, Some(total));
    }

    let failed_count = u32::try_from(failed.len()).unwrap_or(u32::MAX);
    let ok = total.saturating_sub(failed_count);
    if failed.is_empty() {
        JobOutcome::Completed
    } else if ok == 0 {
        let (error, category) = first_error
            .unwrap_or_else(|| ("err.cover.failed".to_string(), FailureCategory::Other));
        JobOutcome::Failed { error, category }
    } else {
        JobOutcome::Partial {
            result: serde_json::json!({ "ok": ok, "failed": failed }),
        }
    }
}

async fn refresh_one(ctx: &JobCtx, track: &wdmm_core::models::LibraryTrack) -> Result<(), String> {
    let item = ctx
        .vfs
        .get(&track.remote_path)
        .await
        .map_err(|error| format!("err.cloud.get|{}: {error}", track.remote_path))?;
    let cover = probe_cover(ctx, &track.remote_path, item.size).await?;
    let thumb = thumbs::thumb_path(&ctx.paths.cover_dir, &track.account_id, &track.remote_path);
    match cover {
        Some(bytes) => {
            thumbs::write_thumb_at(&bytes, &thumb, COVER_THUMB_SIZE)
                .map_err(|error| format!("err.cover.thumb|{}: {error}", track.remote_path))?;
            ctx.db
                .set_track_cover_state(&track.id, 2)
                .map_err(|error| format!("err.storage.cover|{}: {error}", track.remote_path))?;
        }
        None => {
            ctx.db
                .set_track_cover_state(&track.id, 1)
                .map_err(|error| format!("err.storage.cover|{}: {error}", track.remote_path))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_and_missing_thumb_states_need_a_probe() {
        assert!(needs_probe(0, false));
        assert!(needs_probe(0, true));
        assert!(needs_probe(2, false));
        assert!(!needs_probe(2, true));
        assert!(!needs_probe(1, false));
        assert!(!needs_probe(1, true));
    }

    #[test]
    fn cue_virtual_rows_are_skipped() {
        assert!(is_cue_virtual("/m/album.flac#cue:3"));
        assert!(!is_cue_virtual("/m/album.flac"));
    }
}
