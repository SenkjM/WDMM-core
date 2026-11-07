//! Event ring and revision counter for the queue.
//!
//! Dart never polls task rows at 60 fps: it asks for `events(since_seq)` and
//! renders the merged result. The ring keeps the last
//! [`EVENT_RING_CAPACITY`] events, `seq` only grows, and progress events for the
//! same task within [`PROGRESS_MERGE_WINDOW`] collapse into the previous event
//! so a task emits at most 10 progress events per second.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::model::{ProgressMode, QueueBatch, QueueTask, TaskStatus};

/// Maximum number of events retained for `events(since_seq)` replay.
pub const EVENT_RING_CAPACITY: usize = 512;

/// Two progress updates for one task inside this window are merged into one
/// event (10 Hz).
pub const PROGRESS_MERGE_WINDOW: Duration = Duration::from_millis(100);

/// One queue event. Serialised as `{"seq":…,"rev":…,"type":"…",…}`.
#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub seq: u64,
    pub rev: u64,
    #[serde(flatten)]
    pub kind: EventKind,
}

/// Event payloads, tagged by `type` (`task_added`, `task_progress`, …).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    TaskAdded {
        task: QueueTask,
    },
    TaskStarted {
        task: QueueTask,
    },
    /// Merged per task at ≤10 Hz; carries only what the UI renders.
    TaskProgress {
        task_id: String,
        status: TaskStatus,
        progress: f64,
        progress_mode: ProgressMode,
        bytes_done: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        bytes_total: Option<u64>,
        units_done: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        units_total: Option<u32>,
    },
    TaskTerminal {
        task_id: String,
        status: TaskStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        error_message: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        local_path: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        local_uri: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        result_json: Option<String>,
        progress: f64,
    },
    BatchUpdated {
        batch: QueueBatch,
    },
    SessionDrained,
    Summary {
        batch_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        completed: u32,
        failed: u32,
        cancelled: u32,
    },
    KeepaliveHint {
        phase: String,
    },
    PlatformRequest {
        req_id: String,
        action: String,
        params_json: String,
    },
    FgsTimeoutHint,
}

impl EventKind {
    fn progress_task_id(&self) -> Option<&str> {
        match self {
            EventKind::TaskProgress { task_id, .. } => Some(task_id),
            _ => None,
        }
    }
}

/// Result of `events(since_seq)`.
#[derive(Debug, Clone, Serialize)]
pub struct EventsPage {
    pub rev: u64,
    pub events: Vec<Event>,
    pub next_seq: u64,
}

/// Ring buffer + monotonic `seq`/`rev` counters.
#[derive(Debug)]
pub struct EventHub {
    events: VecDeque<Event>,
    next_seq: u64,
    rev: u64,
    last_progress: std::collections::HashMap<String, Instant>,
}

impl EventHub {
    pub fn new() -> Self {
        Self {
            events: VecDeque::new(),
            next_seq: 1,
            rev: 1,
            last_progress: std::collections::HashMap::new(),
        }
    }

    pub fn rev(&self) -> u64 {
        self.rev
    }

    /// Force a revision bump without emitting an event (e.g. a batch label edit
    /// that the caller reports through `batch_updated` later).
    pub fn bump_rev(&mut self) -> u64 {
        self.rev = self.rev.saturating_add(1);
        self.rev
    }

    /// Append a non-progress event. Always bumps `rev`.
    pub fn push(&mut self, kind: EventKind) -> Event {
        self.rev = self.rev.saturating_add(1);
        self.append(kind, self.rev)
    }

    /// Append a progress event unless the same task reported within
    /// [`PROGRESS_MERGE_WINDOW`] (which caps it at 10 Hz). Dropped updates still
    /// bump `rev`, so a snapshot taken after them sees fresh counters.
    ///
    /// Returns the stored event, or `None` when the update was throttled.
    pub fn push_progress(&mut self, task_id: &str, kind: EventKind) -> Option<Event> {
        debug_assert_eq!(kind.progress_task_id(), Some(task_id));
        let now = Instant::now();
        self.rev = self.rev.saturating_add(1);

        if let Some(previous) = self.last_progress.get(task_id) {
            if now.duration_since(*previous) < PROGRESS_MERGE_WINDOW {
                return None;
            }
        }
        self.last_progress.insert(task_id.to_string(), now);

        // Bound the map: forget timestamps that can no longer throttle anything.
        if self.last_progress.len() > EVENT_RING_CAPACITY {
            self.last_progress
                .retain(|_, at| now.duration_since(*at) < PROGRESS_MERGE_WINDOW);
        }

        Some(self.append(kind, self.rev))
    }

    /// Forget a task's progress throttle (on terminal events).
    pub fn clear_progress(&mut self, task_id: &str) {
        self.last_progress.remove(task_id);
    }

    fn append(&mut self, kind: EventKind, rev: u64) -> Event {
        let event = Event {
            seq: self.next_seq,
            rev,
            kind,
        };
        self.next_seq = self.next_seq.saturating_add(1);
        if self.events.len() >= EVENT_RING_CAPACITY {
            self.events.pop_front();
        }
        self.events.push_back(event.clone());
        event
    }

    /// Replay every event with `seq > since_seq`.
    pub fn events_since(&self, since_seq: u64) -> EventsPage {
        let events: Vec<Event> = self
            .events
            .iter()
            .filter(|e| e.seq > since_seq)
            .cloned()
            .collect();
        EventsPage {
            rev: self.rev,
            events,
            next_seq: self.next_seq,
        }
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

impl Default for EventHub {
    fn default() -> Self {
        Self::new()
    }
}
