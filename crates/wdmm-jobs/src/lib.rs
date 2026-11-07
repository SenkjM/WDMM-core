//! `wdmm-jobs` — executors for the Rust-owned task queue.
//!
//! The scheduler lives in `wdmm-queue`; this crate implements what each task
//! kind actually *does*. Executors are pure Rust: transfers go through
//! `WdmmVfs`, library writes through `wdmm-storage`, tag/CUE parsing through
//! `wdmm-text`/`lofty`, covers through `image`, and anything that needs Android
//! (SAF, MediaStore, foreground service) is delegated to the Flutter layer via
//! [`PlatformClient`] (event + reply, never a Rust → Dart callback).
//!
//! Ownership rule: one file per job family. `wdmm-ffi` only wires them up.

#![forbid(unsafe_code)]

pub mod cache_paths;
pub mod cbz;
pub mod download;
pub mod ingest;
pub mod manga;
pub mod match_remote;
pub mod platform;
pub mod retag;
pub mod scan;
pub mod thumbs;
pub mod upload;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use wdmm_drivers::WdmmVfs;
use wdmm_queue::model::{CancelToken, QueueTask, TaskKind};
use wdmm_queue::policy::FailureCategory;
use wdmm_storage::LibraryDatabase;

pub use platform::{PlatformAction, PlatformCall, PlatformClient};

/// Where an executor reports progress. The queue throttles/aggregates these
/// into events; executors may call as often as is natural.
pub trait ProgressSink: Send + Sync {
    /// Transfer progress in bytes.
    fn bytes(&self, done: u64, total: Option<u64>);
    /// Unit progress (files/pages processed).
    fn units(&self, done: u32, total: Option<u32>);
    /// Diagnostic note (goes to the diag log, not the UI).
    fn note(&self, message: &str);
}

/// No-op sink for tests and for contexts that do not need progress.
pub struct NoProgress;

impl ProgressSink for NoProgress {
    fn bytes(&self, _done: u64, _total: Option<u64>) {}
    fn units(&self, _done: u32, _total: Option<u32>) {}
    fn note(&self, _message: &str) {}
}

/// How a job ended. The scheduler maps this onto task status + retry policy.
#[derive(Debug)]
pub enum JobOutcome {
    Completed,
    /// Finished, but some items failed. `result` is
    /// `{"ok":n,"failed":[{"path":"…","error":"…"}]}`.
    Partial { result: Value },
    Failed {
        error: String,
        category: FailureCategory,
    },
    Cancelled,
}

impl JobOutcome {
    /// Placeholder used by the scaffolding stubs; replaced as jobs land.
    pub fn unimplemented(what: &str) -> Self {
        JobOutcome::Failed {
            error: format!("err.jobUnimplemented|{what}"),
            category: FailureCategory::Other,
        }
    }
}

/// Filesystem anchors the executors need. Provided by the FFI edge so Rust and
/// Dart agree on the exact directories (`appDocs`, cache root, payloads, covers).
#[derive(Debug, Clone)]
pub struct JobPaths {
    /// Flutter `getApplicationDocumentsDirectory()`.
    pub app_docs: PathBuf,
    /// Flutter `getApplicationCacheDirectory()` — the media cache root.
    pub cache_root: PathBuf,
    /// Directory holding scan/manga-build payload envelopes.
    pub payload_dir: PathBuf,
    /// Thumbnail directory (`<app_docs>/covers`).
    pub cover_dir: PathBuf,
}

impl JobPaths {
    pub fn new(app_docs: impl Into<PathBuf>, cache_root: impl Into<PathBuf>) -> Self {
        let app_docs = app_docs.into();
        let cache_root = cache_root.into();
        Self {
            payload_dir: app_docs.join("queue_payloads"),
            cover_dir: app_docs.join("covers"),
            app_docs,
            cache_root,
        }
    }
}

/// Everything a job may touch. Built by the scheduler for each run.
pub struct JobCtx {
    pub task: QueueTask,
    pub cancel: CancelToken,
    pub progress: Arc<dyn ProgressSink>,
    pub platform: Arc<PlatformClient>,
    pub db: Arc<LibraryDatabase>,
    pub vfs: Arc<dyn WdmmVfs>,
    pub paths: JobPaths,
}

impl JobCtx {
    pub fn cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

#[async_trait]
pub trait Job: Send + Sync {
    /// Which kind this executor handles. Jobs that serve several kinds (the
    /// download executor covers cache + public) are registered for each.
    fn kind(&self) -> TaskKind;

    async fn run(&self, ctx: JobCtx) -> JobOutcome;
}

/// Kind → executor map. `wdmm-ffi` builds the default registry once.
#[derive(Default)]
pub struct JobRegistry {
    jobs: HashMap<TaskKind, Arc<dyn Job>>,
}

impl JobRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, kinds: &[TaskKind], job: Arc<dyn Job>) {
        for kind in kinds {
            self.jobs.insert(*kind, Arc::clone(&job));
        }
    }

    pub fn get(&self, kind: TaskKind) -> Option<Arc<dyn Job>> {
        self.jobs.get(&kind).cloned()
    }

    pub fn kinds(&self) -> Vec<TaskKind> {
        let mut kinds: Vec<TaskKind> = self.jobs.keys().copied().collect();
        kinds.sort_by_key(|k| k.as_str());
        kinds
    }

    /// All executors wired up. Executor modules are filled in by their owners;
    /// until then they report `err.jobUnimplemented`.
    pub fn with_defaults() -> Self {
        let mut registry = Self::new();
        registry.register(
            &[TaskKind::DownloadCache, TaskKind::DownloadPublic],
            Arc::new(download::DownloadJob),
        );
        registry.register(&[TaskKind::Upload], Arc::new(upload::UploadJob));
        registry.register(&[TaskKind::Scan], Arc::new(scan::ScanJob));
        registry.register(&[TaskKind::RetagCloud], Arc::new(retag::RetagCloudJob));
        registry.register(
            &[TaskKind::MatchRemote],
            Arc::new(match_remote::MatchRemoteJob),
        );
        registry.register(
            &[TaskKind::MangaScrape],
            Arc::new(manga::MangaScrapeJob),
        );
        registry.register(&[TaskKind::MangaCache], Arc::new(manga::MangaCacheJob));
        registry.register(&[TaskKind::MangaBuild], Arc::new(manga::MangaBuildJob));
        registry.register(&[TaskKind::MangaPack], Arc::new(manga::MangaPackJob));
        registry
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_registry_covers_every_kind() {
        let registry = JobRegistry::with_defaults();
        for kind in TaskKind::ALL {
            assert!(
                registry.get(kind).is_some(),
                "no executor registered for {}",
                kind.as_str()
            );
        }
    }

    #[test]
    fn paths_derive_payload_and_cover_dirs_from_app_docs() {
        let paths = JobPaths::new("/data/app", "/data/cache");
        assert_eq!(paths.payload_dir, PathBuf::from("/data/app/queue_payloads"));
        assert_eq!(paths.cover_dir, PathBuf::from("/data/app/covers"));
        assert_eq!(paths.cache_root, PathBuf::from("/data/cache"));
    }
}
