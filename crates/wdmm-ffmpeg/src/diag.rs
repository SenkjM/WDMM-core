//! In-process diagnostics ring buffer for the HLS transcode path (§7.1).
//!
//! The FFI layer exposes the tail of this buffer to the UI so a failed cast can
//! explain *what* libav rejected instead of just timing out. This module is
//! compiled regardless of the `ffmpeg` feature: without libav there are no
//! transcode logs, but the API must still exist.
//!
//! Each line is prefixed with the elapsed milliseconds since this process
//! started, so lines from several jobs are orderable. Callers add their own
//! `[hls:<job>]` prefix. The buffer is capped at [`MAX_LINES`]; the oldest line
//! is dropped first. Logging must never panic and must never affect a job: all
//! locking is poison-tolerant and file I/O errors are ignored.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Instant;

/// Maximum number of retained log lines.
const MAX_LINES: usize = 512;

fn start() -> &'static Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now)
}

fn buffer() -> &'static Mutex<VecDeque<String>> {
    static BUF: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();
    BUF.get_or_init(|| Mutex::new(VecDeque::with_capacity(MAX_LINES)))
}

fn file_slot() -> &'static Mutex<Option<File>> {
    static FILE: OnceLock<Mutex<Option<File>>> = OnceLock::new();
    FILE.get_or_init(|| Mutex::new(None))
}

fn lock<T>(m: &'static Mutex<T>) -> MutexGuard<'static, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Append one diagnostic line (also mirrored to the debug file when set).
pub fn log(msg: impl AsRef<str>) {
    let line = format!("{} {}", start().elapsed().as_millis(), msg.as_ref());
    {
        let mut buf = lock(buffer());
        if buf.len() >= MAX_LINES {
            buf.pop_front();
        }
        buf.push_back(line.clone());
    }
    if let Some(f) = lock(file_slot()).as_mut() {
        // Diagnostics must never affect the transcode: ignore write failures.
        let _ = writeln!(f, "{line}");
        let _ = f.flush();
    }
}

/// Newest `max_lines` lines, oldest first.
pub fn tail(max_lines: usize) -> Vec<String> {
    let buf = lock(buffer());
    let skip = buf.len().saturating_sub(max_lines);
    buf.iter().skip(skip).cloned().collect()
}

/// Mirror every subsequent line into `path` (append + flush per line).
///
/// `None` stops file logging and drops the file handle. A file that cannot be
/// opened is silently ignored — diagnostics are best-effort.
pub fn set_file(path: Option<PathBuf>) {
    let mut slot = lock(file_slot());
    *slot = match path {
        None => None,
        Some(p) => OpenOptions::new().create(true).append(true).open(p).ok(),
    };
}

/// Drop every retained line (called when a new cast session starts).
pub fn clear() {
    lock(buffer()).clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The diagnostics ring buffer + file mirror are process-global, so tests
    /// must not run concurrently with each other.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn test_guard() -> MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn tail_returns_newest_lines_oldest_first() {
        let _guard = test_guard();
        clear();
        log("first");
        log("second");
        log("third");
        let t = tail(2);
        assert_eq!(t.len(), 2);
        assert!(t[0].ends_with("second"), "got {t:?}");
        assert!(t[1].ends_with("third"), "got {t:?}");
        clear();
    }

    #[test]
    fn tail_caps_at_capacity() {
        let _guard = test_guard();
        clear();
        for i in 0..(MAX_LINES + 10) {
            log(format!("line-{i}"));
        }
        let t = tail(MAX_LINES + 100);
        assert_eq!(t.len(), MAX_LINES);
        assert!(t[0].ends_with(&format!("line-{}", 10)));
        assert!(t
            .last()
            .unwrap()
            .ends_with(&format!("line-{}", MAX_LINES + 9)));
        clear();
    }

    #[test]
    fn file_mirror_is_optional_and_silent_on_failure() {
        let _guard = test_guard();
        let path = std::env::temp_dir().join(format!(
            "wdmm-diag-test-{}-{}.log",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        clear();
        set_file(Some(path.clone()));
        log("to-file");
        set_file(None);
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(text.contains("to-file"), "got {text:?}");
        // A file in a missing directory must not panic.
        let missing = std::env::temp_dir()
            .join(format!("wdmm-diag-missing-{}", std::process::id()))
            .join("nested/file.log");
        set_file(Some(missing));
        log("ignored");
        set_file(None);
        let _ = std::fs::remove_file(&path);
        clear();
    }
}
