//! Queue domain model — the single authority for task/batch state.
//!
//! Everything the Flutter layer shows about the queue is derived from these
//! types: the scheduler in this crate owns them, `wdmm-jobs` executes them, and
//! `wdmm-ffi::queue_api` serialises them. Dart keeps no second copy of the state
//! machine — it renders snapshots and consumes the event stream.
//!
//! Field ordering of [`QueueTask`] mirrors its JSON form; the stored DDL in
//! `wdmm-storage` (schema v17) uses the same names.

// The wire format uses snake_case names; `from_str` here is a deliberate
// counterpart to `as_str` rather than the `FromStr` trait.
#![allow(clippy::should_implement_trait)]

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Cooperative cancellation handle handed to a running job.
///
/// The scheduler keeps one clone per active task; jobs poll
/// [`CancelToken::is_cancelled`] at every loop boundary and inside chunk loops.
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

/// What a queue task does. `RetagLocal` is **not** a task kind: the old Dart
/// queue had it as dead code (`_runOne` cancelled it immediately). The live path
/// (`refreshTrackTagsFromCache`) is a synchronous library command, not a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    DownloadCache,
    DownloadPublic,
    Scan,
    RetagCloud,
    MatchRemote,
    MangaScrape,
    MangaCache,
    MangaBuild,
    MangaPack,
    Upload,
    CoverRefresh,
}

impl TaskKind {
    pub const ALL: [TaskKind; 11] = [
        TaskKind::DownloadCache,
        TaskKind::DownloadPublic,
        TaskKind::Scan,
        TaskKind::RetagCloud,
        TaskKind::MatchRemote,
        TaskKind::MangaScrape,
        TaskKind::MangaCache,
        TaskKind::MangaBuild,
        TaskKind::MangaPack,
        TaskKind::Upload,
        TaskKind::CoverRefresh,
    ];

    /// Stable wire/storage name (snake_case, matches the Dart `TaskKind.storageKey`).
    pub fn as_str(self) -> &'static str {
        match self {
            TaskKind::DownloadCache => "download_cache",
            TaskKind::DownloadPublic => "download_public",
            TaskKind::Scan => "scan",
            TaskKind::RetagCloud => "retag_cloud",
            TaskKind::MatchRemote => "match_remote",
            TaskKind::MangaScrape => "manga_scrape",
            TaskKind::MangaCache => "manga_cache",
            TaskKind::MangaBuild => "manga_build",
            TaskKind::MangaPack => "manga_pack",
            TaskKind::Upload => "upload",
            TaskKind::CoverRefresh => "cover_refresh",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "download_cache" => TaskKind::DownloadCache,
            "download_public" => TaskKind::DownloadPublic,
            "scan" => TaskKind::Scan,
            "retag_cloud" => TaskKind::RetagCloud,
            "match_remote" => TaskKind::MatchRemote,
            "manga_scrape" => TaskKind::MangaScrape,
            "manga_cache" => TaskKind::MangaCache,
            "manga_build" => TaskKind::MangaBuild,
            "manga_pack" => TaskKind::MangaPack,
            "upload" => TaskKind::Upload,
            "cover_refresh" => TaskKind::CoverRefresh,
            _ => return None,
        })
    }

    /// True for jobs that only touch local files + the library DB (no transfer
    /// session bookkeeping needed). Used by the notification decision.
    pub fn is_scrape_like(self) -> bool {
        matches!(
            self,
            TaskKind::Scan
                | TaskKind::RetagCloud
                | TaskKind::MatchRemote
                | TaskKind::MangaScrape
                | TaskKind::MangaBuild
                | TaskKind::CoverRefresh
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Active,
    /// Waiting for a Dart/Kotlin platform action (SAF save, SAF read).
    AwaitingPlatform,
    Completed,
    /// Finished with per-item failures; see [`QueueTask::result_json`].
    Partial,
    Failed,
    Cancelled,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::Active => "active",
            TaskStatus::AwaitingPlatform => "awaiting_platform",
            TaskStatus::Completed => "completed",
            TaskStatus::Partial => "partial",
            TaskStatus::Failed => "failed",
            TaskStatus::Cancelled => "cancelled",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => TaskStatus::Pending,
            "active" => TaskStatus::Active,
            "awaiting_platform" => TaskStatus::AwaitingPlatform,
            "completed" => TaskStatus::Completed,
            "partial" => TaskStatus::Partial,
            "failed" => TaskStatus::Failed,
            "cancelled" => TaskStatus::Cancelled,
            _ => return None,
        })
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskStatus::Completed | TaskStatus::Partial | TaskStatus::Failed | TaskStatus::Cancelled
        )
    }

    pub fn is_success(self) -> bool {
        matches!(self, TaskStatus::Completed | TaskStatus::Partial)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressMode {
    Bytes,
    Units,
}

impl ProgressMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ProgressMode::Bytes => "bytes",
            ProgressMode::Units => "units",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "bytes" => ProgressMode::Bytes,
            "units" => ProgressMode::Units,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskTarget {
    /// App-private cache (played from disk, evictable).
    Cache,
    /// User-visible storage (SAF tree / MediaStore / Downloads).
    Public,
}

impl TaskTarget {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskTarget::Cache => "cache",
            TaskTarget::Public => "public",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "cache" => TaskTarget::Cache,
            "public" => TaskTarget::Public,
            _ => return None,
        })
    }
}

/// One unit of queued work: **one file per task** (a folder scan that discovers
/// its own files is still one task — the batch is what groups user intent).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueTask {
    pub id: String,
    pub kind: TaskKind,
    pub status: TaskStatus,
    /// Display name of the source account (UI only; `account_id` is authority).
    pub source_name: String,
    pub account_id: String,
    pub remote_path: String,
    pub file_name: String,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub progress: f64,
    pub progress_mode: ProgressMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_total: Option<u64>,
    #[serde(default)]
    pub bytes_done: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub units_total: Option<u32>,
    #[serde(default)]
    pub units_done: u32,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempts_cap: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_retry_at: Option<i64>,
    pub batch_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_group_id: Option<String>,
    pub target: TaskTarget,
    #[serde(default)]
    pub priority: i32,
    /// JSON envelope for scan/manga_build tasks (list of items to process).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_path: Option<String>,
    /// Per-item outcome for `Partial` tasks:
    /// `{"ok":n,"failed":[{"path":"…","error":"…"}]}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_json: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_modified: Option<i64>,
    /// Whether the app was in the foreground when the task was enqueued. Drives
    /// the "no notification for a foreground scrape" rule.
    #[serde(default)]
    pub queued_while_foreground: bool,
}

impl QueueTask {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: String,
        kind: TaskKind,
        source_name: String,
        account_id: String,
        remote_path: String,
        file_name: String,
        target: TaskTarget,
        batch_id: String,
        now: i64,
    ) -> Self {
        let progress_mode = match kind {
            TaskKind::Scan | TaskKind::MangaBuild => ProgressMode::Units,
            _ => ProgressMode::Bytes,
        };
        Self {
            id,
            kind,
            status: TaskStatus::Pending,
            source_name,
            account_id,
            remote_path,
            file_name,
            created_at: now,
            updated_at: now,
            local_path: None,
            local_uri: None,
            error_message: None,
            progress: 0.0,
            progress_mode,
            bytes_total: None,
            bytes_done: 0,
            units_total: None,
            units_done: 0,
            attempts: 0,
            attempts_cap: None,
            next_retry_at: None,
            batch_id,
            cache_group_id: None,
            target,
            priority: 0,
            payload_path: None,
            result_json: None,
            remote_size: None,
            remote_modified: None,
            queued_while_foreground: false,
        }
    }

    pub fn is_terminal(&self) -> bool {
        self.status.is_terminal()
    }

    /// Fraction shown by the UI: byte progress for transfers, unit progress for
    /// scrape/build jobs, falling back to the stored `progress` when the totals
    /// are unknown (same rule as the old `QueueTask.displayProgress`).
    pub fn display_progress(&self) -> f64 {
        match self.progress_mode {
            ProgressMode::Bytes => match self.bytes_total {
                Some(total) if total > 0 => (self.bytes_done as f64 / total as f64).clamp(0.0, 1.0),
                _ => self.progress,
            },
            ProgressMode::Units => match self.units_total {
                Some(total) if total > 0 => (self.units_done as f64 / total as f64).clamp(0.0, 1.0),
                _ => self.progress,
            },
        }
    }

    /// Copy counters into `progress` so a terminal task still reports 1.0.
    pub fn sync_progress_from_counters(&mut self) {
        self.progress = self.display_progress();
    }

    pub fn set_bytes(&mut self, done: u64, total: Option<u64>) {
        self.bytes_done = done;
        if let Some(total) = total {
            self.bytes_total = Some(total);
        }
        self.progress = self.display_progress();
    }

    pub fn set_units(&mut self, done: u32, total: Option<u32>) {
        self.units_done = done;
        if let Some(total) = total {
            self.units_total = Some(total);
        }
        self.progress = self.display_progress();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchKind {
    Scan,
    Download,
    Upload,
    Manga,
    Sync,
}

impl BatchKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BatchKind::Scan => "scan",
            BatchKind::Download => "download",
            BatchKind::Upload => "upload",
            BatchKind::Manga => "manga",
            BatchKind::Sync => "sync",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "scan" => BatchKind::Scan,
            "download" => BatchKind::Download,
            "upload" => BatchKind::Upload,
            "manga" => BatchKind::Manga,
            "sync" => BatchKind::Sync,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchOrigin {
    /// The user pressed a button (notifications follow the normal rules).
    User,
    /// Auto-scrape/backfill triggered by playback or settings.
    AutoBackfill,
    /// Internal housekeeping.
    System,
}

impl BatchOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            BatchOrigin::User => "user",
            BatchOrigin::AutoBackfill => "auto_backfill",
            BatchOrigin::System => "system",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "user" => BatchOrigin::User,
            "auto_backfill" => BatchOrigin::AutoBackfill,
            "system" => BatchOrigin::System,
            _ => return None,
        })
    }

    /// Non-user batches are silent: no progress events, one summary only if
    /// something failed, and their successful rows are pruned when they close.
    pub fn is_silent(self) -> bool {
        !matches!(self, BatchOrigin::User)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueBatch {
    pub id: String,
    pub kind: BatchKind,
    pub origin: BatchOrigin,
    pub silent: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_total: Option<u32>,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl QueueBatch {
    pub fn is_open(&self) -> bool {
        self.closed_at.is_none()
    }
}

pub fn batch_kind_for(kind: TaskKind) -> BatchKind {
    match kind {
        TaskKind::DownloadCache | TaskKind::DownloadPublic => BatchKind::Download,
        TaskKind::Scan
        | TaskKind::RetagCloud
        | TaskKind::MatchRemote
        | TaskKind::CoverRefresh => BatchKind::Scan,
        TaskKind::MangaScrape | TaskKind::MangaCache | TaskKind::MangaBuild | TaskKind::MangaPack => {
            BatchKind::Manga
        }
        TaskKind::Upload => BatchKind::Upload,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(kind: TaskKind) -> QueueTask {
        QueueTask::new(
            "t1".into(),
            kind,
            "nas".into(),
            "acc".into(),
            "/a/b.flac".into(),
            "b.flac".into(),
            TaskTarget::Cache,
            "b1".into(),
            1_700_000_000_000,
        )
    }

    #[test]
    fn kind_round_trips_through_storage_names() {
        for kind in TaskKind::ALL {
            assert_eq!(TaskKind::from_str(kind.as_str()), Some(kind));
        }
    }

    #[test]
    fn status_round_trips_and_marks_terminal() {
        for status in [
            TaskStatus::Pending,
            TaskStatus::Active,
            TaskStatus::AwaitingPlatform,
            TaskStatus::Completed,
            TaskStatus::Partial,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
        ] {
            assert_eq!(TaskStatus::from_str(status.as_str()), Some(status));
        }
        assert!(!TaskStatus::AwaitingPlatform.is_terminal());
        assert!(TaskStatus::Partial.is_terminal());
        assert!(TaskStatus::Partial.is_success());
        assert!(!TaskStatus::Failed.is_success());
    }

    #[test]
    fn progress_mode_defaults_follow_the_kind() {
        assert_eq!(task(TaskKind::Scan).progress_mode, ProgressMode::Units);
        assert_eq!(task(TaskKind::MangaBuild).progress_mode, ProgressMode::Units);
        assert_eq!(task(TaskKind::DownloadCache).progress_mode, ProgressMode::Bytes);
    }

    #[test]
    fn display_progress_prefers_known_totals() {
        let mut t = task(TaskKind::DownloadCache);
        t.bytes_total = Some(1000);
        t.set_bytes(250, None);
        assert!((t.display_progress() - 0.25).abs() < 1e-9);

        let mut s = task(TaskKind::Scan);
        s.units_total = Some(4);
        s.set_units(1, None);
        assert!((s.display_progress() - 0.25).abs() < 1e-9);
        s.sync_progress_from_counters();
        assert!((s.progress - 0.25).abs() < 1e-9);
    }

    #[test]
    fn progress_falls_back_to_stored_fraction_without_totals() {
        let mut t = task(TaskKind::DownloadCache);
        t.progress = 0.4;
        assert!((t.display_progress() - 0.4).abs() < 1e-9);
    }

    #[test]
    fn non_user_batches_are_silent() {
        assert!(!BatchOrigin::User.is_silent());
        assert!(BatchOrigin::AutoBackfill.is_silent());
        assert!(BatchOrigin::System.is_silent());
    }

    #[test]
    fn batch_kind_groups_task_kinds() {
        assert_eq!(batch_kind_for(TaskKind::Scan), BatchKind::Scan);
        assert_eq!(batch_kind_for(TaskKind::DownloadPublic), BatchKind::Download);
        assert_eq!(batch_kind_for(TaskKind::MangaPack), BatchKind::Manga);
        assert_eq!(batch_kind_for(TaskKind::Upload), BatchKind::Upload);
    }
}
