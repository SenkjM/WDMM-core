//! Rows: the queue the UI renders.
//!
//! One row is one *batch* (a user action such as "download this album"), not a
//! file. Tasks keep living individually for the scheduler; the row is a
//! read-only fold that the tasks screen can render with a single progress bar.

use serde::Serialize;

use crate::model::{
    batch_kind_for, BatchKind, BatchOrigin, ProgressMode, QueueBatch, QueueTask, TaskStatus,
};

/// Per-status counters for a folded row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RowCounts {
    pub completed: u32,
    pub failed: u32,
    pub cancelled: u32,
    pub pending: u32,
    pub active: u32,
}

impl RowCounts {
    pub fn total(&self) -> u32 {
        self.completed + self.failed + self.cancelled + self.pending + self.active
    }

    pub fn done(&self) -> u32 {
        self.completed + self.failed + self.cancelled
    }
}

/// One batch folded for display.
#[derive(Debug, Clone, Serialize)]
pub struct QueueRow {
    pub group_id: String,
    pub batch_id: String,
    pub kind: BatchKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub origin: BatchOrigin,
    pub silent: bool,
    pub head_task_id: String,
    pub status: TaskStatus,
    pub progress: f64,
    pub progress_mode: ProgressMode,
    pub units_done: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub units_total: Option<u32>,
    pub counts: RowCounts,
    pub task_ids: Vec<String>,
    pub earliest_created_at: i64,
}

/// Fold tasks into per-batch rows.
///
/// Ordering: rows with unfinished work first, then priority (desc), then the
/// oldest member (asc) — the same order Dart's queue list used.
pub fn fold_rows(tasks: &[QueueTask], batches: &[QueueBatch]) -> Vec<QueueRow> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: std::collections::HashMap<String, Vec<&QueueTask>> =
        std::collections::HashMap::new();
    for task in tasks {
        let entry = groups.entry(task.batch_id.clone()).or_insert_with(|| {
            order.push(task.batch_id.clone());
            Vec::new()
        });
        entry.push(task);
    }

    let by_id: std::collections::HashMap<&str, &QueueBatch> =
        batches.iter().map(|b| (b.id.as_str(), b)).collect();

    let mut rows: Vec<(i32, QueueRow)> = Vec::with_capacity(groups.len());
    for batch_id in order {
        let mut members = groups.remove(&batch_id).unwrap_or_default();
        members.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        if members.is_empty() {
            continue;
        }
        let priority = members.iter().map(|t| t.priority).max().unwrap_or(0);
        rows.push((
            priority,
            fold_one(&batch_id, &members, by_id.get(batch_id.as_str()).copied()),
        ));
    }

    rows.sort_by(|(pa, a), (pb, b)| {
        a.status
            .is_terminal()
            .cmp(&b.status.is_terminal())
            .then_with(|| pb.cmp(pa))
            .then_with(|| a.earliest_created_at.cmp(&b.earliest_created_at))
            .then_with(|| a.batch_id.cmp(&b.batch_id))
    });
    rows.into_iter().map(|(_, row)| row).collect()
}

fn fold_one(batch_id: &str, members: &[&QueueTask], batch: Option<&QueueBatch>) -> QueueRow {
    let mut counts = RowCounts::default();
    let mut awaiting = false;
    let mut any_partial = false;
    let mut any_failed = false;
    let mut units_done = 0u32;
    let mut units_total = 0u32;
    let mut know_units = false;
    let mut bytes_done = 0u64;
    let mut bytes_total = 0u64;
    let mut know_bytes = false;
    let mut progress_sum = 0.0f64;

    for task in members {
        match task.status {
            TaskStatus::Completed => counts.completed += 1,
            TaskStatus::Partial => {
                counts.completed += 1;
                any_partial = true;
            }
            TaskStatus::Failed => {
                counts.failed += 1;
                any_failed = true;
            }
            TaskStatus::Cancelled => counts.cancelled += 1,
            TaskStatus::Pending => counts.pending += 1,
            TaskStatus::Active => counts.active += 1,
            TaskStatus::AwaitingPlatform => {
                counts.active += 1;
                awaiting = true;
            }
        }
        units_done += task.units_done;
        if let Some(total) = task.units_total {
            units_total += total;
            know_units = true;
        }
        bytes_done += task.bytes_done;
        if let Some(total) = task.bytes_total {
            bytes_total += total;
            know_bytes = true;
        }
        progress_sum += task.display_progress();
    }

    let head = members
        .iter()
        .find(|t| !t.status.is_terminal())
        .copied()
        .unwrap_or(members[0]);

    let progress_mode = head.progress_mode;
    let progress = match progress_mode {
        ProgressMode::Bytes if know_bytes && bytes_total > 0 => {
            (bytes_done as f64 / bytes_total as f64).clamp(0.0, 1.0)
        }
        ProgressMode::Units if know_units && units_total > 0 => {
            (units_done as f64 / units_total as f64).clamp(0.0, 1.0)
        }
        _ => (progress_sum / members.len() as f64).clamp(0.0, 1.0),
    };

    let status = if counts.active > 0 {
        if awaiting {
            TaskStatus::AwaitingPlatform
        } else {
            TaskStatus::Active
        }
    } else if counts.pending > 0 {
        TaskStatus::Pending
    } else if any_partial {
        TaskStatus::Partial
    } else if any_failed {
        TaskStatus::Failed
    } else if counts.completed > 0 {
        TaskStatus::Completed
    } else {
        TaskStatus::Cancelled
    };

    let label = batch.and_then(|b| b.label.clone()).or_else(|| {
        members
            .iter()
            .find_map(|t| t.cache_group_id.clone())
            .filter(|group| group.starts_with("cue"))
    });

    QueueRow {
        group_id: batch_id.to_string(),
        batch_id: batch_id.to_string(),
        kind: batch
            .map(|b| b.kind)
            .unwrap_or_else(|| batch_kind_for(head.kind)),
        label,
        origin: batch.map(|b| b.origin).unwrap_or(BatchOrigin::User),
        silent: batch.is_some_and(|b| b.silent),
        head_task_id: head.id.clone(),
        status,
        progress,
        progress_mode,
        units_done,
        units_total: if know_units { Some(units_total) } else { None },
        counts,
        task_ids: members.iter().map(|t| t.id.clone()).collect(),
        earliest_created_at: members.iter().map(|t| t.created_at).min().unwrap_or(0),
    }
}
