//! Time-bounded cache for provider-issued download links.
//!
//! Cloud providers hand out short-lived signed URLs (Aliyun
//! `getDownloadUrl`, 123 Pan `download_info`, 115 `downurl`, CtFile
//! `fetch_url`). Resolving one costs one or more API round trips, and ranged
//! reads — playback, `STREAM_CHUNK`-sized proxy reads, resumable downloads —
//! used to resolve the link again for *every* chunk. With 4 MiB chunks a 2.8 GB
//! video pays that cost hundreds of times per playback.
//!
//! One cache lives on each driver, keyed by the remote path. Entries are only
//! reused while fresh; a provider rejection (401/403/410) invalidates the entry
//! so the caller can re-resolve once. Nothing is persisted and no link outlives
//! the process.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Default lifetime of a cached link.
///
/// Provider links usually live 5–15 minutes; a shorter cache still absorbs the
/// burst of range reads that motivates the cache, and keeps the window for
/// stale-link rejections small.
pub const DEFAULT_LINK_TTL: Duration = Duration::from_secs(240);

struct LinkEntry {
    url: String,
    headers: Vec<(String, String)>,
    expires_at: Instant,
}

/// A small path → signed-link map with a fixed TTL.
pub struct SignedLinkCache {
    ttl: Duration,
    entries: Mutex<HashMap<String, LinkEntry>>,
}

impl SignedLinkCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Returns the cached `(url, headers)` pair while it is still fresh.
    pub fn get(&self, path: &str) -> Option<(String, Vec<(String, String)>)> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match entries.get(path) {
            Some(entry) if Instant::now() < entry.expires_at => {
                Some((entry.url.clone(), entry.headers.clone()))
            }
            Some(_) => {
                entries.remove(path);
                None
            }
            None => None,
        }
    }

    /// Stores the link for `path`, replacing any older entry.
    pub fn store(&self, path: &str, url: String, headers: Vec<(String, String)>) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        entries.insert(
            path.to_string(),
            LinkEntry {
                url,
                headers,
                expires_at: Instant::now() + self.ttl,
            },
        );
    }

    /// Drops the entry so the next read re-resolves the link.
    pub fn invalidate(&self, path: &str) {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_and_invalidates_links() {
        let cache = SignedLinkCache::new(Duration::from_secs(60));
        cache.store(
            "/a.mp4",
            "https://cdn.example/a".into(),
            vec![("User-Agent".into(), "ua".into())],
        );
        let (url, headers) = cache.get("/a.mp4").expect("cached link");
        assert_eq!(url, "https://cdn.example/a");
        assert_eq!(headers, vec![("User-Agent".to_string(), "ua".to_string())]);
        assert!(cache.get("/missing.mp4").is_none());

        cache.invalidate("/a.mp4");
        assert!(cache.get("/a.mp4").is_none());
    }

    #[test]
    fn expired_links_are_not_returned() {
        let cache = SignedLinkCache::new(Duration::ZERO);
        cache.store("/a.mp4", "https://cdn.example/a".into(), Vec::new());
        assert!(cache.get("/a.mp4").is_none());
    }
}
