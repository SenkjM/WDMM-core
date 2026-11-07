//! Persistence boundary for the durable queue.
//!
//! [`TaskStore`] is the only thing the scheduler knows about storage, so the
//! state machine can be exercised against [`MemStore`] without a database.
//! [`DbStore`] adapts `wdmm-storage` schema v17 (`queue_tasks`,
//! `queue_batches`, KV `queue_*` cleanup) and the `<app_docs>/queue_payloads`
//! sidecar directory used by scan/manga-build tasks.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use thiserror::Error;
use wdmm_storage::{LibraryDatabase, QueueBatchRow, QueueTaskRow};

use crate::model::{
    BatchKind, BatchOrigin, ProgressMode, QueueBatch, QueueTask, TaskKind, TaskStatus, TaskTarget,
};

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("queue storage error: {0}")]
    Storage(String),

    #[error("queue payload i/o error: {0}")]
    Io(String),

    #[error("queue row decode error: {0}")]
    Decode(String),
}

type StoreResult<T> = std::result::Result<T, StoreError>;

/// Everything a fresh scheduler needs to resume after a process restart.
#[derive(Debug, Default)]
pub struct LoadedQueue {
    pub tasks: Vec<QueueTask>,
    pub batches: Vec<QueueBatch>,
}

/// Library data the pack→scrape follow-up needs to seed a scrape payload.
///
/// The queue crate must not depend on `wdmm-storage` row types directly (the
/// FFI layer owns that mapping), so this is the narrow view it consumes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MangaDraft {
    pub title: String,
    pub author: Option<String>,
    pub series: Option<String>,
    pub volume: Option<String>,
    pub chapter: Option<String>,
}

/// Durable backing for queue state.
///
/// Implementations must be cheap to call: the scheduler persists on every state
/// transition (progress is throttled by the caller).
pub trait TaskStore: Send + Sync {
    fn load(&self) -> StoreResult<LoadedQueue>;
    fn save_task(&self, task: &QueueTask) -> StoreResult<()>;
    fn delete_task(&self, id: &str) -> StoreResult<()>;
    fn delete_tasks(&self, ids: &[String]) -> StoreResult<()>;
    fn delete_all_tasks(&self) -> StoreResult<()>;
    fn save_batch(&self, batch: &QueueBatch) -> StoreResult<()>;
    fn delete_batch(&self, id: &str) -> StoreResult<()>;
    fn delete_all_batches(&self) -> StoreResult<()>;

    /// Write the sidecar payload for `task_id` and return the stored path.
    fn write_payload(&self, task_id: &str, contents: &str) -> StoreResult<String>;
    fn read_payload(&self, task_id: &str) -> StoreResult<Option<String>>;
    fn delete_payload(&self, task_id: &str) -> StoreResult<()>;

    /// Persist a whole queue snapshot (used by `clear_*` and batch pruning).
    fn save_tasks(&self, tasks: &[QueueTask]) -> StoreResult<()> {
        for task in tasks {
            self.save_task(task)?;
        }
        Ok(())
    }

    /// Resolve an account display name to its id (pack→upload hand-off).
    ///
    /// Defaults to "unknown": a store that cannot see accounts refuses the
    /// hand-off instead of guessing.
    fn account_id_by_display_name(&self, _display_name: &str) -> StoreResult<Option<String>> {
        Ok(None)
    }

    /// Read the manga row a packed archive was built from (pack→scrape seed).
    fn manga_draft(&self, _manga_id: &str) -> StoreResult<Option<MangaDraft>> {
        Ok(None)
    }
}

/// `TaskStore` backed by the library database (schema v17).
pub struct DbStore {
    db: Arc<LibraryDatabase>,
    payload_dir: PathBuf,
}

impl DbStore {
    pub fn new(db: Arc<LibraryDatabase>, payload_dir: impl Into<PathBuf>) -> Self {
        Self {
            db,
            payload_dir: payload_dir.into(),
        }
    }

    pub fn payload_dir(&self) -> &Path {
        &self.payload_dir
    }

    fn payload_path(&self, task_id: &str) -> PathBuf {
        self.payload_dir.join(format!("{task_id}.json"))
    }
}

impl TaskStore for DbStore {
    fn load(&self) -> StoreResult<LoadedQueue> {
        let mut tasks = Vec::new();
        for row in self.db.queue_list_tasks().map_err(storage_err)? {
            match task_from_row(&row) {
                Ok(task) => tasks.push(task),
                // A row written by a newer build with an unknown enum: skip it
                // instead of failing the whole startup.
                Err(_) => continue,
            }
        }

        let mut batches = Vec::new();
        for row in self.db.queue_list_batches().map_err(storage_err)? {
            match batch_from_row(&row) {
                Ok(batch) => batches.push(batch),
                Err(_) => continue,
            }
        }

        Ok(LoadedQueue { tasks, batches })
    }

    fn save_task(&self, task: &QueueTask) -> StoreResult<()> {
        let row = task_to_row(task);
        self.db.queue_upsert_task(&row).map_err(storage_err)
    }

    fn delete_task(&self, id: &str) -> StoreResult<()> {
        self.db.queue_delete_task(id).map_err(storage_err)?;
        Ok(())
    }

    fn delete_tasks(&self, ids: &[String]) -> StoreResult<()> {
        self.db.queue_delete_tasks(ids).map_err(storage_err)?;
        Ok(())
    }

    fn delete_all_tasks(&self) -> StoreResult<()> {
        self.db.queue_delete_all_tasks().map_err(storage_err)?;
        Ok(())
    }

    fn save_batch(&self, batch: &QueueBatch) -> StoreResult<()> {
        let row = batch_to_row(batch);
        self.db.queue_upsert_batch(&row).map_err(storage_err)
    }

    fn delete_batch(&self, id: &str) -> StoreResult<()> {
        self.db.queue_delete_batch(id).map_err(storage_err)?;
        Ok(())
    }

    fn delete_all_batches(&self) -> StoreResult<()> {
        self.db.queue_delete_all_batches().map_err(storage_err)?;
        Ok(())
    }

    fn write_payload(&self, task_id: &str, contents: &str) -> StoreResult<String> {
        std::fs::create_dir_all(&self.payload_dir).map_err(|e| StoreError::Io(e.to_string()))?;
        let path = self.payload_path(task_id);
        std::fs::write(&path, contents).map_err(|e| StoreError::Io(e.to_string()))?;
        Ok(path.to_string_lossy().into_owned())
    }

    fn read_payload(&self, task_id: &str) -> StoreResult<Option<String>> {
        let path = self.payload_path(task_id);
        match std::fs::read_to_string(&path) {
            Ok(contents) => Ok(Some(contents)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StoreError::Io(e.to_string())),
        }
    }

    fn delete_payload(&self, task_id: &str) -> StoreResult<()> {
        match std::fs::remove_file(self.payload_path(task_id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StoreError::Io(e.to_string())),
        }
    }

    fn account_id_by_display_name(&self, display_name: &str) -> StoreResult<Option<String>> {
        let accounts = self.db.list_accounts().map_err(storage_err)?;
        Ok(accounts
            .into_iter()
            .find(|account| account.display_name == display_name)
            .map(|account| account.id))
    }

    fn manga_draft(&self, manga_id: &str) -> StoreResult<Option<MangaDraft>> {
        let manga = self.db.get_manga(manga_id).map_err(storage_err)?;
        Ok(manga.map(|manga| MangaDraft {
            title: manga.title,
            author: manga.author,
            series: manga.series,
            volume: manga.volume,
            chapter: manga.chapter_number,
        }))
    }
}

fn storage_err(e: wdmm_storage::StorageError) -> StoreError {
    StoreError::Storage(e.to_string())
}

pub(crate) fn task_to_row(task: &QueueTask) -> QueueTaskRow {
    QueueTaskRow {
        id: task.id.clone(),
        kind: task.kind.as_str().into(),
        status: task.status.as_str().into(),
        source_name: task.source_name.clone(),
        remote_path: task.remote_path.clone(),
        file_name: task.file_name.clone(),
        account_id: task.account_id.clone(),
        created_at: task.created_at,
        updated_at: task.updated_at,
        local_path: task.local_path.clone(),
        local_uri: task.local_uri.clone(),
        error_message: task.error_message.clone(),
        progress: task.progress,
        progress_mode: task.progress_mode.as_str().into(),
        bytes_total: task.bytes_total.map(|v| v as i64),
        bytes_done: task.bytes_done as i64,
        units_total: task.units_total.map(|v| v as i64),
        units_done: task.units_done as i64,
        attempts: task.attempts as i64,
        attempts_cap: task.attempts_cap.map(|v| v as i64),
        next_retry_at: task.next_retry_at,
        batch_id: task.batch_id.clone(),
        cache_group_id: task.cache_group_id.clone(),
        target: task.target.as_str().into(),
        priority: task.priority as i64,
        payload_path: task.payload_path.clone(),
        result_json: task.result_json.clone(),
        remote_size: task.remote_size.map(|v| v as i64),
        remote_modified: task.remote_modified,
    }
}

pub(crate) fn task_from_row(row: &QueueTaskRow) -> StoreResult<QueueTask> {
    let kind = TaskKind::from_str(&row.kind)
        .ok_or_else(|| StoreError::Decode(format!("unknown task kind: {}", row.kind)))?;
    let status = TaskStatus::from_str(&row.status)
        .ok_or_else(|| StoreError::Decode(format!("unknown task status: {}", row.status)))?;
    let progress_mode = ProgressMode::from_str(&row.progress_mode).ok_or_else(|| {
        StoreError::Decode(format!("unknown progress mode: {}", row.progress_mode))
    })?;
    let target = TaskTarget::from_str(&row.target)
        .ok_or_else(|| StoreError::Decode(format!("unknown task target: {}", row.target)))?;

    Ok(QueueTask {
        id: row.id.clone(),
        kind,
        status,
        source_name: row.source_name.clone(),
        account_id: row.account_id.clone(),
        remote_path: row.remote_path.clone(),
        file_name: row.file_name.clone(),
        created_at: row.created_at,
        updated_at: row.updated_at,
        local_path: row.local_path.clone(),
        local_uri: row.local_uri.clone(),
        error_message: row.error_message.clone(),
        progress: row.progress,
        progress_mode,
        bytes_total: row.bytes_total.map(|v| v.max(0) as u64),
        bytes_done: row.bytes_done.max(0) as u64,
        units_total: row.units_total.map(|v| v.max(0) as u32),
        units_done: row.units_done.max(0) as u32,
        attempts: row.attempts.max(0) as u32,
        attempts_cap: row.attempts_cap.map(|v| v.max(0) as u32),
        next_retry_at: row.next_retry_at,
        batch_id: row.batch_id.clone(),
        cache_group_id: row.cache_group_id.clone(),
        target,
        priority: row.priority as i32,
        payload_path: row.payload_path.clone(),
        result_json: row.result_json.clone(),
        remote_size: row.remote_size.map(|v| v.max(0) as u64),
        remote_modified: row.remote_modified,
        // Not persisted: a restart is never "queued while foreground".
        queued_while_foreground: false,
    })
}

pub(crate) fn batch_to_row(batch: &QueueBatch) -> QueueBatchRow {
    QueueBatchRow {
        id: batch.id.clone(),
        kind: batch.kind.as_str().into(),
        origin: batch.origin.as_str().into(),
        silent: batch.silent,
        expected_total: batch.expected_total.map(|v| v as i64),
        created_at: batch.created_at,
        closed_at: batch.closed_at,
        label: batch.label.clone(),
    }
}

pub(crate) fn batch_from_row(row: &QueueBatchRow) -> StoreResult<QueueBatch> {
    let kind = BatchKind::from_str(&row.kind)
        .ok_or_else(|| StoreError::Decode(format!("unknown batch kind: {}", row.kind)))?;
    let origin = BatchOrigin::from_str(&row.origin)
        .ok_or_else(|| StoreError::Decode(format!("unknown batch origin: {}", row.origin)))?;
    Ok(QueueBatch {
        id: row.id.clone(),
        kind,
        origin,
        silent: row.silent,
        expected_total: row.expected_total.map(|v| v.max(0) as u32),
        created_at: row.created_at,
        closed_at: row.closed_at,
        label: row.label.clone(),
    })
}

/// In-memory store: used by unit tests and headless handle construction.
#[derive(Default)]
pub struct MemStore {
    inner: Mutex<MemState>,
}

#[derive(Default)]
struct MemState {
    tasks: HashMap<String, QueueTask>,
    batches: HashMap<String, QueueBatch>,
    payloads: HashMap<String, String>,
    accounts: HashMap<String, String>,
    mangas: HashMap<String, MangaDraft>,
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn task_count(&self) -> usize {
        self.inner.lock().unwrap().tasks.len()
    }

    /// Test hook: expose an account display name for pack→upload resolution.
    pub fn bind_account(&self, display_name: &str, account_id: &str) {
        self.inner
            .lock()
            .unwrap()
            .accounts
            .insert(display_name.to_string(), account_id.to_string());
    }

    /// Test hook: expose a manga row for pack→scrape seeding.
    pub fn bind_manga(&self, manga_id: &str, draft: MangaDraft) {
        self.inner
            .lock()
            .unwrap()
            .mangas
            .insert(manga_id.to_string(), draft);
    }

    /// Test hook: read a payload sidecar written through the store.
    pub fn payload(&self, task_id: &str) -> Option<String> {
        self.inner.lock().unwrap().payloads.get(task_id).cloned()
    }
}

impl TaskStore for MemStore {
    fn load(&self) -> StoreResult<LoadedQueue> {
        let state = self.inner.lock().unwrap();
        let mut tasks: Vec<QueueTask> = state.tasks.values().cloned().collect();
        tasks.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        let mut batches: Vec<QueueBatch> = state.batches.values().cloned().collect();
        batches.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(LoadedQueue { tasks, batches })
    }

    fn save_task(&self, task: &QueueTask) -> StoreResult<()> {
        self.inner
            .lock()
            .unwrap()
            .tasks
            .insert(task.id.clone(), task.clone());
        Ok(())
    }

    fn delete_task(&self, id: &str) -> StoreResult<()> {
        self.inner.lock().unwrap().tasks.remove(id);
        Ok(())
    }

    fn delete_tasks(&self, ids: &[String]) -> StoreResult<()> {
        let mut state = self.inner.lock().unwrap();
        for id in ids {
            state.tasks.remove(id);
        }
        Ok(())
    }

    fn delete_all_tasks(&self) -> StoreResult<()> {
        self.inner.lock().unwrap().tasks.clear();
        Ok(())
    }

    fn save_batch(&self, batch: &QueueBatch) -> StoreResult<()> {
        self.inner
            .lock()
            .unwrap()
            .batches
            .insert(batch.id.clone(), batch.clone());
        Ok(())
    }

    fn delete_batch(&self, id: &str) -> StoreResult<()> {
        self.inner.lock().unwrap().batches.remove(id);
        Ok(())
    }

    fn delete_all_batches(&self) -> StoreResult<()> {
        self.inner.lock().unwrap().batches.clear();
        Ok(())
    }

    fn write_payload(&self, task_id: &str, contents: &str) -> StoreResult<String> {
        self.inner
            .lock()
            .unwrap()
            .payloads
            .insert(task_id.to_string(), contents.to_string());
        Ok(format!("mem://{task_id}.json"))
    }

    fn read_payload(&self, task_id: &str) -> StoreResult<Option<String>> {
        Ok(self.inner.lock().unwrap().payloads.get(task_id).cloned())
    }

    fn delete_payload(&self, task_id: &str) -> StoreResult<()> {
        self.inner.lock().unwrap().payloads.remove(task_id);
        Ok(())
    }

    fn account_id_by_display_name(&self, display_name: &str) -> StoreResult<Option<String>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .accounts
            .get(display_name)
            .cloned())
    }

    fn manga_draft(&self, manga_id: &str) -> StoreResult<Option<MangaDraft>> {
        Ok(self.inner.lock().unwrap().mangas.get(manga_id).cloned())
    }
}
