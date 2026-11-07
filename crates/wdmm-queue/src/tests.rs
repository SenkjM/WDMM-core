//! Behavioural tests for the queue authority: recovery, ordering, retry,
//! cancellation, batch closing, event monotonicity and row folding.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use crate::events::{EventHub, EventKind};
use crate::handle::{QueueHandle, RunCtx, RunOutcome, TaskRunner, WaitOutcome};
use crate::model::{
    BatchKind, BatchOrigin, QueueBatch, QueueTask, TaskKind, TaskStatus, TaskTarget,
};
use crate::now_ms;
use crate::policy::FailureCategory;
use crate::queue::{EnqueueRequest, QueueState};
use crate::rows::fold_rows;
use crate::store::{LoadedQueue, MangaDraft, MemStore, TaskStore};

/// Runner that replays a script of outcomes and records the task order.
struct ScriptedRunner {
    outcomes: Mutex<Vec<RunOutcome>>,
    seen: Mutex<Vec<String>>,
    delay: Duration,
}

impl ScriptedRunner {
    fn quiet() -> Arc<Self> {
        Arc::new(Self {
            outcomes: Mutex::new(Vec::new()),
            seen: Mutex::new(Vec::new()),
            delay: Duration::from_millis(1),
        })
    }

    fn with(outcomes: Vec<RunOutcome>) -> Arc<Self> {
        Arc::new(Self {
            outcomes: Mutex::new(outcomes),
            seen: Mutex::new(Vec::new()),
            delay: Duration::from_millis(1),
        })
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

#[async_trait]
impl TaskRunner for ScriptedRunner {
    fn supports(&self, _kind: TaskKind) -> bool {
        true
    }

    async fn run(&self, ctx: RunCtx) -> RunOutcome {
        self.seen.lock().unwrap().push(ctx.task.id.clone());
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        let mut outcomes = self.outcomes.lock().unwrap();
        if outcomes.is_empty() {
            RunOutcome::Completed
        } else {
            outcomes.remove(0)
        }
    }
}

fn task(id: &str, remote: &str, batch_id: &str, created: i64) -> QueueTask {
    QueueTask::new(
        id.to_string(),
        TaskKind::DownloadCache,
        "src".to_string(),
        "acc".to_string(),
        remote.to_string(),
        format!("{id}.mkv"),
        TaskTarget::Cache,
        batch_id.to_string(),
        created,
    )
}

fn batch(id: &str, origin: BatchOrigin, now: i64) -> QueueBatch {
    QueueBatch {
        id: id.to_string(),
        kind: BatchKind::Download,
        origin,
        silent: origin.is_silent(),
        expected_total: None,
        created_at: now,
        closed_at: None,
        label: None,
    }
}

async fn wait_until(mut probe: impl FnMut() -> bool) -> bool {
    for _ in 0..200 {
        if probe() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    probe()
}

#[test]
fn recovery_resets_active_but_keeps_progress_and_retry() {
    let mut task = task("t1", "/media/a.mkv", "b1", 1_000);
    task.status = TaskStatus::Active;
    task.bytes_total = Some(100);
    task.bytes_done = 50;
    task.progress = 0.5;
    task.next_retry_at = Some(9_000);
    let loaded = LoadedQueue {
        tasks: vec![task],
        batches: vec![batch("b1", BatchOrigin::User, 1_000)],
    };

    let state = QueueState::from_loaded(loaded, 2_000);
    let restored = state.task("t1").expect("task restored");
    assert_eq!(restored.status, TaskStatus::Pending);
    assert_eq!(restored.bytes_done, 50);
    assert_eq!(restored.bytes_total, Some(100));
    assert_eq!(restored.progress, 0.5);
    assert_eq!(restored.next_retry_at, Some(9_000));
}

#[test]
fn next_runnable_prefers_priority_then_age_and_waits_for_retry() {
    let mut state = QueueState::new();
    for (id, created, priority) in [("old", 1_000, 0), ("new", 3_000, 0), ("urgent", 2_000, 5)] {
        let mut req = EnqueueRequest::new(
            TaskKind::DownloadCache,
            "acc",
            format!("/media/{id}.mkv"),
            format!("{id}.mkv"),
        );
        req.id = Some(id.to_string());
        req.priority = priority;
        req.now = Some(created);
        req.dedupe = false;
        state.enqueue(req);
    }

    let order = {
        let mut order = Vec::new();
        while let Some(id) = state.next_runnable(now_ms()) {
            let _ = state.mark_active(&id, now_ms());
            order.push(id);
        }
        order
    };
    assert_eq!(order[0], "urgent");
    assert_eq!(order[1], "old");
    assert_eq!(order[2], "new");

    // A pending task due in the future is skipped until its retry time.
    let mut req = EnqueueRequest::new(TaskKind::DownloadCache, "acc", "/media/w.mkv", "w.mkv");
    req.id = Some("waiting".into());
    req.now = Some(now_ms());
    req.dedupe = false;
    state.enqueue(req);
    {
        let task = state.task_mut("waiting").unwrap();
        task.next_retry_at = Some(now_ms() + 60_000);
    }
    assert!(state.next_runnable(now_ms()).is_none());
    assert!(state.next_runnable(now_ms() + 60_001).is_some());
}

#[test]
fn enqueue_dedupes_in_flight_and_creates_a_single_task_batch() {
    let mut state = QueueState::new();
    let mut req = EnqueueRequest::new(TaskKind::DownloadCache, "acc", "/media/a.mkv", "a.mkv");
    req.id = Some("a".into());
    req.now = Some(1_000);
    let first = state.enqueue(req);

    let mut again = EnqueueRequest::new(TaskKind::DownloadCache, "acc", "/media/a.mkv", "a.mkv");
    again.now = Some(1_100);
    let second = state.enqueue(again);

    assert!(!first.deduped);
    assert!(second.deduped);
    assert_eq!(second.task.id, "a");
    assert_eq!(state.len(), 1);
    assert_eq!(state.open_batches(), 1);
    assert!(!state.batch(&first.batch.id).unwrap().silent);

    // The dedupe key includes the kind: the same remote path with another
    // kind is a separate piece of work.
    let mut scan = EnqueueRequest::new(TaskKind::Scan, "acc", "/media/a.mkv", "a.mkv");
    scan.now = Some(1_200);
    let third = state.enqueue(scan);
    assert!(!third.deduped);
    assert_ne!(third.task.id, "a");
    assert_eq!(state.len(), 2);
}

#[tokio::test]
async fn enqueue_dedupes_on_the_full_key_and_keeps_one_store_row() {
    let store = Arc::new(MemStore::new());
    let runner: Arc<dyn TaskRunner> = Arc::new(ScriptedRunner {
        outcomes: Mutex::new(vec![RunOutcome::Completed]),
        seen: Mutex::new(Vec::new()),
        delay: Duration::from_millis(300),
    });
    let handle = QueueHandle::try_new(store.clone(), runner).expect("queue");

    let first = handle
        .enqueue(EnqueueRequest::new(
            TaskKind::DownloadCache,
            "acc",
            "/media/b.mkv",
            "b.mkv",
        ))
        .expect("first");
    let second = handle
        .enqueue(EnqueueRequest::new(
            TaskKind::DownloadCache,
            "acc",
            "/media/b.mkv",
            "b.mkv",
        ))
        .expect("second");

    assert!(!first.deduped);
    assert!(second.deduped);
    assert_eq!(second.task_id, first.task_id);
    assert_eq!(store.task_count(), 1);
    assert_eq!(handle.tasks().len(), 1);
}

/// Runner that records the payload sidecar path each started task carried.
struct PayloadRunner {
    seen: Mutex<Vec<Option<String>>>,
}

#[async_trait]
impl TaskRunner for PayloadRunner {
    fn supports(&self, _kind: TaskKind) -> bool {
        true
    }

    async fn run(&self, ctx: RunCtx) -> RunOutcome {
        self.seen
            .lock()
            .unwrap()
            .push(ctx.task.payload_path.clone());
        RunOutcome::Completed
    }
}

#[tokio::test]
async fn enqueue_delivers_the_payload_sidecar_to_the_running_task() {
    let store = Arc::new(MemStore::new());
    let runner = Arc::new(PayloadRunner {
        seen: Mutex::new(Vec::new()),
    });
    let handle =
        QueueHandle::try_new(store, runner.clone() as Arc<dyn TaskRunner>).expect("queue");

    let mut req = EnqueueRequest::new(TaskKind::Scan, "acc", "/media", "Music");
    req.payload = Some(r#"[{"path":"/media/a.mp3"}]"#.to_string());
    let enqueued = handle.enqueue(req).expect("enqueue");

    // The queued row and the started task must both see the sidecar: jobs read
    // `task.payload_path`, which the scheduler takes from the in-memory state.
    assert_eq!(
        handle.task(&enqueued.task_id).unwrap().payload_path,
        Some(format!("mem://{}.json", enqueued.task_id))
    );
    assert!(wait_until(|| !runner.seen.lock().unwrap().is_empty()).await);
    assert_eq!(
        runner.seen.lock().unwrap().as_slice(),
        &[Some(format!("mem://{}.json", enqueued.task_id))]
    );
}

#[tokio::test]
async fn wait_reports_not_found_separately_from_timeout() {
    let store = Arc::new(MemStore::new());
    let runner: Arc<dyn TaskRunner> = Arc::new(ScriptedRunner {
        outcomes: Mutex::new(vec![RunOutcome::Completed]),
        seen: Mutex::new(Vec::new()),
        delay: Duration::from_millis(400),
    });
    let handle = QueueHandle::try_new(store, runner).expect("queue");

    assert!(matches!(
        handle.wait("does-not-exist", 20).await,
        WaitOutcome::NotFound
    ));

    let enqueued = handle
        .enqueue(EnqueueRequest::new(
            TaskKind::DownloadCache,
            "acc",
            "/media/c.mkv",
            "c.mkv",
        ))
        .expect("enqueue");
    assert!(matches!(
        handle.wait(&enqueued.task_id, 30).await,
        WaitOutcome::Timeout
    ));
    assert!(matches!(
        handle.wait(&enqueued.task_id, 5_000).await,
        WaitOutcome::Terminal {
            status: TaskStatus::Completed
        }
    ));
}

#[tokio::test]
async fn completed_task_becomes_terminal_and_announces_drain() {
    let store = Arc::new(MemStore::new());
    let runner = ScriptedRunner::quiet();
    let handle = QueueHandle::try_new(store.clone(), runner).expect("queue");

    let req = EnqueueRequest::new(TaskKind::DownloadCache, "acc", "/media/a.mkv", "a.mkv");
    let enqueued = handle.enqueue(req).expect("enqueue");
    handle
        .drain_for_tests(Duration::from_secs(2))
        .await
        .expect("drain");

    let task = handle.task(&enqueued.task_id).expect("task");
    assert_eq!(task.status, TaskStatus::Completed);
    assert!(task.progress >= 1.0);
    assert_eq!(
        store.load().unwrap().tasks.len(),
        1,
        "terminal state must be persisted"
    );
    assert!(handle.snapshot().session.drained);
    let types: Vec<String> = handle
        .events(0)
        .events
        .iter()
        .map(|event| format!("{:?}", event.kind))
        .collect();
    assert!(types.iter().any(|t| t.starts_with("TaskStarted")));
    assert!(types.iter().any(|t| t.starts_with("TaskTerminal")));
}

#[tokio::test]
async fn failures_go_through_the_retry_policy() {
    let store = Arc::new(MemStore::new());
    let runner = ScriptedRunner::with(vec![RunOutcome::Failed {
        error: "dio: connection reset by peer".into(),
        category: FailureCategory::Dio,
    }]);
    let handle = QueueHandle::try_new(store, runner).expect("queue");

    let req = EnqueueRequest::new(TaskKind::DownloadCache, "acc", "/media/a.mkv", "a.mkv");
    let enqueued = handle.enqueue(req).expect("enqueue");

    let retried = wait_until(|| {
        handle
            .task(&enqueued.task_id)
            .map(|task| task.next_retry_at.is_some())
            .unwrap_or(false)
    })
    .await;
    assert!(retried, "task should be scheduled for a retry");

    let task = handle.task(&enqueued.task_id).expect("task");
    assert_eq!(task.status, TaskStatus::Pending);
    assert_eq!(task.attempts, 1);
    assert_eq!(task.attempts_cap, Some(5));
    assert!(task.next_retry_at.unwrap() > now_ms());
    assert!(task.error_message.unwrap().contains("connection reset"));
}

#[test]
fn cancellation_is_immediate_for_pending_tasks() {
    let mut state = QueueState::new();
    let mut req = EnqueueRequest::new(TaskKind::DownloadCache, "acc", "/media/a.mkv", "a.mkv");
    req.id = Some("a".into());
    req.now = Some(1_000);
    state.enqueue(req);

    match state.cancel("a", 2_000).expect("queued task") {
        crate::queue::CancelOutcome::Terminal(task) => {
            assert_eq!(task.status, TaskStatus::Cancelled);
            assert_eq!(task.error_message.as_deref(), Some("err.cancelled"));
        }
        other => panic!("expected a terminal cancel, got {other:?}"),
    }
    state.close_finished_batches(2_000);
    assert!(state.is_drained());
}

#[tokio::test]
async fn silent_batches_drop_successful_members_when_they_close() {
    let store = Arc::new(MemStore::new());
    let runner = ScriptedRunner::quiet();
    let handle = QueueHandle::try_new(store.clone(), runner.clone()).expect("queue");

    for index in 0..3 {
        let mut req = EnqueueRequest::new(
            TaskKind::DownloadCache,
            "acc",
            format!("/media/auto{index}.mkv"),
            format!("auto{index}.mkv"),
        );
        req.batch_id = Some("auto-batch".into());
        req.batch_origin = BatchOrigin::AutoBackfill;
        req.dedupe = false;
        handle.enqueue(req).expect("enqueue");
    }

    handle
        .drain_for_tests(Duration::from_secs(3))
        .await
        .expect("drain");

    assert!(handle.tasks().is_empty(), "silent successes are pruned");
    assert!(handle.batches().is_empty(), "the silent batch is dropped");
    assert_eq!(runner.seen().len(), 3, "each member ran once");
    let stored = store.load().unwrap();
    assert!(stored.tasks.is_empty());
    assert!(stored.batches.is_empty());
    assert!(
        !handle
            .events(0)
            .events
            .iter()
            .any(|event| matches!(event.kind, EventKind::TaskAdded { .. })),
        "silent batches must not emit task-level events"
    );
}

#[test]
fn event_sequences_are_monotonic_and_progress_coalesces() {
    let mut hub = EventHub::new();
    assert_eq!(hub.rev(), 1);
    let mut task = task("t1", "/media/a.mkv", "b1", 1_000);
    task.status = TaskStatus::Active;

    let added = hub.push(EventKind::TaskAdded { task });
    let first = hub
        .push_progress(
            "t1",
            EventKind::TaskProgress {
                task_id: "t1".into(),
                status: TaskStatus::Active,
                progress: 0.1,
                progress_mode: crate::model::ProgressMode::Bytes,
                bytes_done: 10,
                bytes_total: Some(100),
                units_done: 0,
                units_total: None,
            },
        )
        .expect("first progress passes");
    let second = hub.push_progress(
        "t1",
        EventKind::TaskProgress {
            task_id: "t1".into(),
            status: TaskStatus::Active,
            progress: 0.2,
            progress_mode: crate::model::ProgressMode::Bytes,
            bytes_done: 20,
            bytes_total: Some(100),
            units_done: 0,
            units_total: None,
        },
    );
    assert!(second.is_none(), "coalesced inside the merge window");

    assert_eq!(first.seq, added.seq + 1);
    assert!(first.rev > added.rev);
    let page = hub.events_since(0);
    assert_eq!(page.next_seq, first.seq + 1);
    assert_eq!(page.events.len(), 2);
    assert!(hub.events_since(first.seq).events.is_empty());
}

#[test]
fn rows_fold_members_by_batch_and_put_open_groups_first() {
    let mut completed = task("done", "/media/a.mkv", "b1", 1_000);
    completed.status = TaskStatus::Completed;
    let mut failed = task("bad", "/media/b.mkv", "b1", 1_100);
    failed.status = TaskStatus::Failed;
    let pending = task("wait", "/media/c.mkv", "b1", 1_200);
    let mut closed = task("old", "/media/d.mkv", "b2", 500);
    closed.status = TaskStatus::Completed;

    let mut batches = vec![
        batch("b1", BatchOrigin::User, 1_000),
        batch("b2", BatchOrigin::User, 500),
    ];
    batches[1].closed_at = Some(2_000);

    let rows = fold_rows(&[completed, failed, pending, closed], &batches);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].batch_id, "b1", "open group sorts first");
    assert_eq!(rows[0].head_task_id, "wait");
    assert_eq!(rows[0].counts.completed, 1);
    assert_eq!(rows[0].counts.failed, 1);
    assert_eq!(rows[0].counts.pending, 1);
    assert_eq!(rows[0].task_ids.len(), 3);
    assert_eq!(rows[0].status, TaskStatus::Pending);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_blocking_returns_the_terminal_status() {
    let store = Arc::new(MemStore::new());
    let runner = ScriptedRunner::quiet();
    let handle = QueueHandle::try_new(store, runner).expect("queue");
    let req = EnqueueRequest::new(TaskKind::DownloadCache, "acc", "/media/a.mkv", "a.mkv");
    let enqueued = handle.enqueue(req).expect("enqueue");

    let outcome = spawn_blocking_wait(handle.clone(), enqueued.task_id.clone());
    assert!(matches!(
        outcome,
        WaitOutcome::Terminal {
            status: TaskStatus::Completed
        }
    ));
}

fn spawn_blocking_wait(handle: QueueHandle, task_id: String) -> WaitOutcome {
    // Run the blocking wait on a dedicated OS thread: `block_on` is only legal
    // outside the test runtime.
    let thread = std::thread::spawn(move || handle.wait_blocking(&task_id, 5_000));
    thread.join().expect("wait thread")
}

#[test]
fn db_store_round_trips_the_durable_columns() {
    use crate::store::DbStore;
    use wdmm_storage::LibraryDatabase;

    let stamp = format!("{}_{}", std::process::id(), now_ms());
    let db_path = std::env::temp_dir().join(format!("wdmm_queue_store_{stamp}.db"));
    let payload_dir = std::env::temp_dir().join(format!("wdmm_queue_payloads_{stamp}"));
    let db = Arc::new(LibraryDatabase::open(&db_path).expect("open library db"));
    let store = DbStore::new(db.clone(), payload_dir);

    let mut queued = task("t1", "/media/a.mkv", "b1", 1_000);
    queued.status = TaskStatus::Active;
    queued.bytes_total = Some(2_048);
    queued.bytes_done = 1_024;
    queued.progress = 0.5;
    queued.priority = 3;
    queued.cache_group_id = Some("cue-album".into());
    queued.result_json = Some("{\"ok\":1}".into());
    queued.remote_size = Some(2_048);
    queued.remote_modified = Some(999);
    queued.payload_path = Some(store.write_payload("t1", "{\"scan\":1}").expect("payload"));
    store.save_task(&queued).expect("save task");
    store
        .save_batch(&batch("b1", BatchOrigin::User, 1_000))
        .expect("save batch");

    let loaded = store.load().expect("load");
    assert_eq!(loaded.tasks.len(), 1);
    let restored = &loaded.tasks[0];
    assert_eq!(restored.id, queued.id);
    assert_eq!(restored.status, TaskStatus::Active);
    assert_eq!(restored.bytes_done, 1_024);
    assert_eq!(restored.bytes_total, Some(2_048));
    assert_eq!(restored.priority, 3);
    assert_eq!(restored.cache_group_id.as_deref(), Some("cue-album"));
    assert_eq!(restored.result_json.as_deref(), Some("{\"ok\":1}"));
    assert_eq!(restored.remote_modified, Some(999));
    assert_eq!(
        store.read_payload("t1").expect("read payload").as_deref(),
        Some("{\"scan\":1}")
    );
    assert_eq!(loaded.batches.len(), 1);
    assert!(!loaded.batches[0].silent);

    let _ = std::fs::remove_file(&db_path);
}

struct CancellableRunner;

#[async_trait]
impl TaskRunner for CancellableRunner {
    fn supports(&self, _kind: TaskKind) -> bool {
        true
    }

    async fn run(&self, ctx: RunCtx) -> RunOutcome {
        for _ in 0..200 {
            if ctx.cancelled() {
                return RunOutcome::Cancelled;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        RunOutcome::Completed
    }
}

#[tokio::test]
async fn clear_all_cancels_running_work_and_empties_the_store() {
    let store = Arc::new(MemStore::new());
    let handle = QueueHandle::try_new(store.clone(), Arc::new(CancellableRunner)).expect("queue");
    let req = EnqueueRequest::new(TaskKind::DownloadCache, "acc", "/media/a.mkv", "a.mkv");
    let enqueued = handle.enqueue(req).expect("enqueue");

    let started = wait_until(|| {
        handle
            .task(&enqueued.task_id)
            .map(|task| task.status == TaskStatus::Active)
            .unwrap_or(false)
    })
    .await;
    assert!(started, "task should reach the runner");

    assert_eq!(handle.clear_all().expect("clear"), 1);
    assert!(handle.tasks().is_empty());
    assert!(store.load().unwrap().tasks.is_empty());
    assert!(handle.snapshot().session.drained);
}

// ------------------------------------------------------- follow-up chains (T6a)

/// Runner that scripts outcomes and records the payload sidecar each task had
/// when it started. The sidecar is deleted on success, so this is the only
/// stable place to observe it.
struct ChainRunner {
    store: Arc<MemStore>,
    outcomes: Mutex<Vec<RunOutcome>>,
    payloads: Mutex<Vec<(TaskKind, String)>>,
}

impl ChainRunner {
    fn new(store: Arc<MemStore>, outcomes: Vec<RunOutcome>) -> Arc<Self> {
        Arc::new(Self {
            store,
            outcomes: Mutex::new(outcomes),
            payloads: Mutex::new(Vec::new()),
        })
    }

    fn payload_for(&self, kind: TaskKind) -> Option<String> {
        self.payloads
            .lock()
            .unwrap()
            .iter()
            .find(|(seen, _)| *seen == kind)
            .map(|(_, payload)| payload.clone())
    }
}

#[async_trait]
impl TaskRunner for ChainRunner {
    fn supports(&self, _kind: TaskKind) -> bool {
        true
    }

    async fn run(&self, ctx: RunCtx) -> RunOutcome {
        let payload = self.store.payload(&ctx.task.id).unwrap_or_default();
        self.payloads.lock().unwrap().push((ctx.task.kind, payload));
        tokio::time::sleep(Duration::from_millis(1)).await;
        let mut outcomes = self.outcomes.lock().unwrap();
        if outcomes.is_empty() {
            RunOutcome::Completed
        } else {
            outcomes.remove(0)
        }
    }
}

fn pack_result(archive: &str) -> String {
    serde_json::json!({
        "archive": archive,
        "upload_remote_path": "/manga/dest/vol1.cbz",
        "upload_source_name": "My Disk",
        "display_name": "Vol 1.cbz",
        "cache_group_id": "wmm-pack-origin:m1",
    })
    .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn packed_archive_hands_off_to_upload_and_scrape() {
    let store = Arc::new(MemStore::new());
    store.bind_account("My Disk", "acc-dest");
    store.bind_manga(
        "m1",
        MangaDraft {
            title: "Berserk".to_string(),
            author: Some("Miura".to_string()),
            series: Some("Berserk".to_string()),
            volume: Some("1".to_string()),
            chapter: Some("1".to_string()),
        },
    );
    let runner = ChainRunner::new(
        store.clone(),
        vec![RunOutcome::Partial {
            result_json: pack_result("/docs/queue_payloads/pack-1.cbz"),
        }],
    );
    let handle = QueueHandle::try_new(store.clone(), runner.clone()).expect("queue");

    let mut req = EnqueueRequest::new(TaskKind::MangaPack, "acc-src", "/manga/src", "Vol 1.cbz");
    req.source_name = "src".to_string();
    let pack = handle.enqueue(req).expect("enqueue pack");
    let pack_batch = pack.batch_id.clone();

    let chained = wait_until(|| {
        let tasks = handle.tasks();
        tasks.iter().any(|task| task.kind == TaskKind::Upload)
            && tasks
                .iter()
                .any(|task| task.kind == TaskKind::MangaScrape && task.status.is_terminal())
    })
    .await;
    assert!(chained, "pack should chain into upload and scrape");

    let tasks = handle.tasks();
    let upload = tasks
        .iter()
        .find(|task| task.kind == TaskKind::Upload)
        .expect("upload");
    assert_eq!(upload.status, TaskStatus::Completed);
    assert_eq!(upload.account_id, "acc-dest");
    assert_eq!(upload.batch_id, pack_batch);
    assert_eq!(upload.target, TaskTarget::Public);
    assert_eq!(
        upload.local_path.as_deref(),
        Some("/docs/queue_payloads/pack-1.cbz")
    );
    assert_eq!(upload.cache_group_id.as_deref(), Some("wmm-pack-origin:m1"));

    let scrape = tasks
        .iter()
        .find(|task| task.kind == TaskKind::MangaScrape)
        .expect("scrape");
    assert_eq!(scrape.status, TaskStatus::Completed);
    assert_eq!(scrape.remote_path, "/manga/dest/vol1.cbz");
    assert_eq!(
        scrape.cache_group_id.as_deref(),
        Some("wmm-pack-del:\"m1\"")
    );
    assert!(
        scrape.local_path.is_none(),
        "scrape must not inherit archive"
    );

    let payload = runner.payload_for(TaskKind::MangaScrape).expect("payload");
    assert!(payload.contains("\"title\":\"Berserk\""), "{payload}");
    assert!(payload.contains("\"format\":\"cbz\""), "{payload}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pack_upload_is_refused_when_the_account_is_unknown() {
    let store = Arc::new(MemStore::new());
    let runner = ChainRunner::new(
        store.clone(),
        vec![RunOutcome::Partial {
            result_json: pack_result("/docs/queue_payloads/pack-1.cbz"),
        }],
    );
    let handle = QueueHandle::try_new(store.clone(), runner.clone()).expect("queue");
    let req = EnqueueRequest::new(TaskKind::MangaPack, "acc-src", "/manga/src", "Vol 1.cbz");
    let pack = handle.enqueue(req).expect("enqueue pack");

    let partial = wait_until(|| {
        handle
            .task(&pack.task_id)
            .map(|task| task.status == TaskStatus::Partial)
            .unwrap_or(false)
    })
    .await;
    assert!(partial);
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(
        handle
            .tasks()
            .iter()
            .all(|task| task.kind != TaskKind::Upload),
        "unknown destination account must not enqueue the upload"
    );
    let error = handle.last_store_error().unwrap_or_default();
    assert!(error.contains("unknown account"), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn existing_scrape_marker_keeps_the_upload_from_re_queuing() {
    let store = Arc::new(MemStore::new());
    store.bind_account("My Disk", "acc-dest");
    let mut existing = QueueTask::new(
        "scrape-existing".to_string(),
        TaskKind::MangaScrape,
        "My Disk".to_string(),
        "acc-dest".to_string(),
        "/manga/dest/vol1.cbz".to_string(),
        "Vol 1.cbz".to_string(),
        TaskTarget::Cache,
        "b0".to_string(),
        500,
    );
    existing.status = TaskStatus::Completed;
    existing.cache_group_id = Some("wmm-pack-del:\"m1\"".to_string());
    store.save_task(&existing).expect("seed scrape");

    let runner = ChainRunner::new(store.clone(), Vec::new());
    let handle = QueueHandle::try_new(store.clone(), runner.clone()).expect("queue");

    let mut req = EnqueueRequest::new(
        TaskKind::Upload,
        "acc-dest",
        "/manga/dest/vol1.cbz",
        "Vol 1.cbz",
    );
    req.target = TaskTarget::Public;
    req.source_name = "My Disk".to_string();
    req.cache_group_id = Some("wmm-pack-origin:m1".to_string());
    let upload = handle.enqueue(req).expect("enqueue upload");

    let completed = wait_until(|| {
        handle
            .task(&upload.task_id)
            .map(|task| task.status == TaskStatus::Completed)
            .unwrap_or(false)
    })
    .await;
    assert!(completed);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let scrapes: Vec<QueueTask> = handle
        .tasks()
        .into_iter()
        .filter(|task| task.kind == TaskKind::MangaScrape)
        .collect();
    assert_eq!(scrapes.len(), 1, "the completed marker must dedupe");
    assert_eq!(scrapes[0].id, "scrape-existing");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cue_group_triggers_one_sheet_scan_after_all_members_finish() {
    let store = Arc::new(MemStore::new());
    let runner = ChainRunner::new(store.clone(), Vec::new());
    let handle = QueueHandle::try_new(store.clone(), runner.clone()).expect("queue");

    let group = "cue\u{0}My Disk\u{0}/Music/Album/album.cue";
    let mut members = Vec::new();
    let mut member_batches = Vec::new();
    for (id, remote) in [
        ("d1", "/Music/Album/01.flac"),
        ("d2", "/Music/Album/02.flac"),
    ] {
        let mut req =
            EnqueueRequest::new(TaskKind::DownloadCache, "acc", remote, format!("{id}.flac"));
        req.id = Some(id.to_string());
        req.source_name = "My Disk".to_string();
        req.cache_group_id = Some(group.to_string());
        let enqueued = handle.enqueue(req).expect("enqueue download");
        members.push(enqueued.task_id);
        member_batches.push(enqueued.batch_id);
    }

    let all_done = wait_until(|| {
        members.iter().all(|id| {
            handle
                .task(id)
                .map(|task| task.status == TaskStatus::Completed)
                .unwrap_or(false)
        })
    })
    .await;
    assert!(all_done);

    let scanned = wait_until(|| {
        handle
            .tasks()
            .iter()
            .any(|task| task.kind == TaskKind::Scan && task.status.is_terminal())
    })
    .await;
    assert!(scanned, "cue group should enqueue the sheet scan");

    tokio::time::sleep(Duration::from_millis(50)).await;
    let scans: Vec<QueueTask> = handle
        .tasks()
        .into_iter()
        .filter(|task| task.kind == TaskKind::Scan)
        .collect();
    assert_eq!(scans.len(), 1, "a completed group scans exactly once");
    let scan = &scans[0];
    assert_eq!(scan.remote_path, "/Music/Album/album.cue");
    assert_eq!(scan.file_name, "album.cue");
    assert_eq!(scan.cache_group_id.as_deref(), Some(group));
    assert!(member_batches.contains(&scan.batch_id));

    let payload = runner.payload_for(TaskKind::Scan).expect("scan payload");
    assert!(
        payload.contains("\"path\":\"/Music/Album/album.cue\""),
        "{payload}"
    );
    assert!(payload.contains("\"cue\":true"), "{payload}");
}
