//! On-disk cache for VOD HLS segments produced by pull-driven transcoding.
//!
//! Layout (contract §3.5):
//!
//! ```text
//! <root>/<sha256(cache key)[..16]>/
//!     plan.json          # SegmentPlan + cache key, used for resume
//!     seg<k>.ts          # one file per segment, k == SegmentEnt.start_number
//! ```
//!
//! The cache never deletes the segments a renderer is currently playing: the
//! owning session passes its own key as the protection scope, and cache
//! accounting allows a temporary overshoot of the budget instead of stealing
//! bytes from under the player (`over_budget = true` is then reported through
//! [`TranscodeCacheStats`] / `HlsProgress`).
//!
//! Every piece of bookkeeping here is pure host logic — no libav, no async —
//! so it is unit-tested on a host build without the `ffmpeg` feature.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{ProxyError, ProxyResult};

/// Cache root used when the caller does not configure one.
pub const DEFAULT_CACHE_SUBDIR: &str = "wdmm-transcode";
/// Default budget when the caller does not configure one (2 GiB).
pub const DEFAULT_CACHE_BUDGET_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Entries untouched for this long are dropped by the lazy purge.
pub const CACHE_PURGE_AFTER_SECS: u64 = 24 * 3600;
/// Eviction drives the total down to `budget * CACHE_TARGET_RATIO`.
pub const CACHE_TARGET_RATIO: f64 = 0.9;
/// Segments this many slots behind the play head are part of the guard band.
pub const GUARD_BACK_SEGMENTS: u64 = 30;

/// Identity of one logical media + preset combination.
///
/// `ident` is the renderer-independent file identity (etag, or an mtime/size
/// string). An empty `ident` marks an *ephemeral* key: the cache may hold the
/// segments for the lifetime of the process, but never reuses them across
/// sessions because two different media could otherwise collide.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CacheKey {
    pub account_id: String,
    pub remote_path: String,
    pub preset_id: String,
    pub size: u64,
    pub ident: String,
}

impl CacheKey {
    pub fn new(
        account_id: impl Into<String>,
        remote_path: impl Into<String>,
        preset_id: impl Into<String>,
        size: u64,
        ident: impl Into<String>,
    ) -> Self {
        Self {
            account_id: account_id.into(),
            remote_path: remote_path.into(),
            preset_id: preset_id.into(),
            size,
            ident: ident.into(),
        }
    }

    /// A key without a stable file identity is scoped to one process lifetime.
    pub fn is_ephemeral(&self) -> bool {
        self.ident.trim().is_empty()
    }

    /// Directory name under the cache root: 16 hex chars of SHA-256.
    pub fn dir_name(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.account_id.as_bytes());
        hasher.update([0u8]);
        hasher.update(self.remote_path.as_bytes());
        hasher.update([0u8]);
        hasher.update(self.preset_id.as_bytes());
        hasher.update([0u8]);
        hasher.update(self.size.to_le_bytes());
        hasher.update([0u8]);
        hasher.update(self.ident.as_bytes());
        let digest = hasher.finalize();
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        hex[..16].to_string()
    }
}

/// One bookkeeping row per cache directory.
#[derive(Debug, Clone)]
struct CacheEntry {
    dir: PathBuf,
    bytes: u64,
    last_used: u64,
}

/// Snapshot handed to the FFI (`cache_stats`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TranscodeCacheStats {
    pub bytes: u64,
    pub budget_bytes: u64,
    pub entries: usize,
    pub hits: u64,
    pub misses: u64,
    pub over_budget: bool,
}

/// Serialized `plan.json`: the plan is stored opaquely so this module stays
/// independent of `wdmm-ffmpeg`'s `SegmentPlan` shape.
#[derive(Debug, Serialize, Deserialize)]
struct CachedPlan {
    cache_key: CacheKey,
    plan: serde_json::Value,
    probed_at: u64,
}

/// Result of a cache lookup for one segment index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentLookup {
    /// The segment file exists on disk and is complete.
    Ready,
    /// Not cached yet.
    Missing,
}

/// Process-wide VOD segment cache.
pub struct TranscodeCache {
    root: Mutex<PathBuf>,
    budget_bytes: AtomicU64,
    entries: Mutex<HashMap<CacheKey, CacheEntry>>,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl TranscodeCache {
    /// Create a cache rooted at `root` with `budget_bytes` of allowed usage.
    pub fn new(root: PathBuf, budget_bytes: u64) -> Self {
        Self {
            root: Mutex::new(root),
            budget_bytes: AtomicU64::new(budget_bytes.max(1)),
            entries: Mutex::new(HashMap::new()),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Root under the OS temp dir, used when the caller passes none.
    pub fn default_root() -> PathBuf {
        std::env::temp_dir().join(DEFAULT_CACHE_SUBDIR)
    }

    /// Apply caller-provided root/budget overrides.
    ///
    /// Changing the root drops the in-memory index (the directories on disk
    /// are not moved — they stay `last_used`-purgeable on the old root).
    pub fn configure(&self, root: Option<PathBuf>, budget_bytes: Option<u64>) {
        if let Some(root) = root {
            let mut current = self.root.lock().expect("cache root poisoned");
            if *current != root {
                *current = root;
                self.entries.lock().expect("cache index poisoned").clear();
            }
        }
        if let Some(budget) = budget_bytes {
            self.budget_bytes.store(budget.max(1), Ordering::Relaxed);
        }
    }

    pub fn root(&self) -> PathBuf {
        self.root.lock().expect("cache root poisoned").clone()
    }

    pub fn budget_bytes(&self) -> u64 {
        self.budget_bytes.load(Ordering::Relaxed)
    }

    /// Directory that holds the segments of `key` (not created here).
    pub fn dir_for(&self, key: &CacheKey) -> PathBuf {
        self.root().join(key.dir_name())
    }

    /// Create the directory for `key` and register it in the index.
    pub fn prepare(&self, key: &CacheKey) -> ProxyResult<PathBuf> {
        let dir = self.dir_for(key);
        fs::create_dir_all(&dir).map_err(|e| ProxyError::Cache(format!("cache dir: {e}")))?;
        let mut entries = self.entries.lock().expect("cache index poisoned");
        let entry = entries.entry(key.clone()).or_insert_with(|| CacheEntry {
            dir: dir.clone(),
            bytes: dir_bytes(&dir),
            last_used: now_secs(),
        });
        entry.dir = dir.clone();
        entry.last_used = now_secs();
        entry.bytes = dir_bytes(&dir);
        Ok(dir)
    }

    /// Path of segment `k` inside `dir`.
    pub fn segment_path(dir: &Path, k: u64) -> PathBuf {
        dir.join(format!("seg{k}.ts"))
    }

    /// Whether segment `k` is finished on disk.
    ///
    /// A non-empty file is not enough: the muxer writes `segK.ts` incrementally,
    /// so a segment that is still being produced looks identical to a finished
    /// one. Serving it truncates the MPEG-TS mid-packet, and the renderer keeps
    /// the audio going while the picture freezes on the last decoded frame. The
    /// muxer only appends a segment to its own playlist (`index.m3u8`) once the
    /// file is closed, so that playlist is the completion marker.
    pub fn segment_ready(dir: &Path, k: u64) -> SegmentLookup {
        match fs::metadata(Self::segment_path(dir, k)) {
            Ok(meta) if meta.len() > 0 => {}
            _ => return SegmentLookup::Missing,
        }
        if muxer_playlist_lists(dir, k) {
            SegmentLookup::Ready
        } else {
            SegmentLookup::Missing
        }
    }

    /// Record that `key` was just used and refresh its byte count.
    pub fn touch(&self, key: &CacheKey, dir: &Path) {
        let bytes = dir_bytes(dir);
        let mut entries = self.entries.lock().expect("cache index poisoned");
        let entry = entries.entry(key.clone()).or_insert_with(|| CacheEntry {
            dir: dir.to_path_buf(),
            bytes,
            last_used: now_secs(),
        });
        entry.dir = dir.to_path_buf();
        entry.bytes = bytes;
        entry.last_used = now_secs();
    }

    pub fn note_hit(&self) {
        self.hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn note_miss(&self) {
        self.misses.fetch_add(1, Ordering::Relaxed);
    }

    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    /// Bytes currently attributed to `key` (0 when unknown).
    pub fn bytes_of(&self, key: &CacheKey) -> u64 {
        let entries = self.entries.lock().expect("cache index poisoned");
        entries.get(key).map(|entry| entry.bytes).unwrap_or(0)
    }

    /// Full snapshot for the FFI / playback page.
    pub fn stats(&self) -> TranscodeCacheStats {
        let entries = self.entries.lock().expect("cache index poisoned");
        let bytes: u64 = entries.values().map(|entry| entry.bytes).sum();
        let budget = self.budget_bytes();
        TranscodeCacheStats {
            bytes,
            budget_bytes: budget,
            entries: entries.len(),
            hits: self.hits(),
            misses: self.misses(),
            over_budget: bytes > budget,
        }
    }

    /// Evict least-recently-used directories until the total drops below
    /// `budget * CACHE_TARGET_RATIO`.
    ///
    /// `protect` names the session that is currently playing: its directory is
    /// never removed, so a guard band that alone exceeds the budget leaves the
    /// cache over budget rather than stalling the renderer.
    pub fn evict(&self, protect: Option<&CacheKey>) {
        let budget = self.budget_bytes();
        let target = (budget as f64 * CACHE_TARGET_RATIO) as u64;
        let mut entries = self.entries.lock().expect("cache index poisoned");
        let mut total: u64 = entries.values().map(|entry| entry.bytes).sum();
        if total <= target {
            return;
        }
        let mut order: Vec<(u64, CacheKey)> = entries
            .iter()
            .filter(|(key, _)| Some(*key) != protect)
            .map(|(key, entry)| (entry.last_used, key.clone()))
            .collect();
        order.sort_by_key(|(last_used, _)| *last_used);
        for (_, key) in order {
            if total <= target {
                break;
            }
            if let Some(entry) = entries.remove(&key) {
                let _ = fs::remove_dir_all(&entry.dir);
                total = total.saturating_sub(entry.bytes);
                tracing::debug!(
                    target: "wdmm_proxy::cache",
                    dir = %entry.dir.display(),
                    bytes = entry.bytes,
                    "evicted transcode cache entry"
                );
            }
        }
    }

    /// Drop every entry untouched for longer than [`CACHE_PURGE_AFTER_SECS`].
    pub fn purge_stale(&self) {
        let cutoff = now_secs().saturating_sub(CACHE_PURGE_AFTER_SECS);
        let mut entries = self.entries.lock().expect("cache index poisoned");
        let stale: Vec<CacheKey> = entries
            .iter()
            .filter(|(_, entry)| entry.last_used < cutoff)
            .map(|(key, _)| key.clone())
            .collect();
        for key in stale {
            if let Some(entry) = entries.remove(&key) {
                let _ = fs::remove_dir_all(&entry.dir);
            }
        }
    }

    /// Remove the directory (and index row) of one key.
    pub fn invalidate(&self, key: &CacheKey) -> ProxyResult<()> {
        let entry = self
            .entries
            .lock()
            .expect("cache index poisoned")
            .remove(key);
        let dir = entry
            .map(|entry| entry.dir)
            .unwrap_or_else(|| self.dir_for(key));
        if dir.exists() {
            fs::remove_dir_all(&dir).map_err(|e| ProxyError::Cache(format!("cache purge: {e}")))?;
        }
        Ok(())
    }

    /// Delete everything under the cache root and reset the index.
    pub fn clear(&self) -> ProxyResult<()> {
        let root = self.root();
        self.entries.lock().expect("cache index poisoned").clear();
        if root.exists() {
            fs::remove_dir_all(&root)
                .map_err(|e| ProxyError::Cache(format!("cache clear: {e}")))?;
        }
        fs::create_dir_all(&root).map_err(|e| ProxyError::Cache(format!("cache clear: {e}")))?;
        self.hits.store(0, Ordering::Relaxed);
        self.misses.store(0, Ordering::Relaxed);
        Ok(())
    }

    /// Persist `plan` as `<dir>/plan.json`, tagged with the cache key.
    pub fn save_plan<P: Serialize>(&self, key: &CacheKey, dir: &Path, plan: &P) -> ProxyResult<()> {
        let plan = serde_json::to_value(plan)
            .map_err(|e| ProxyError::Cache(format!("plan serialize: {e}")))?;
        let cached = CachedPlan {
            cache_key: key.clone(),
            plan,
            probed_at: now_secs(),
        };
        let bytes = serde_json::to_vec(&cached)
            .map_err(|e| ProxyError::Cache(format!("plan serialize: {e}")))?;
        let path = dir.join("plan.json");
        fs::write(&path, bytes).map_err(|e| ProxyError::Cache(format!("plan write: {e}")))?;
        Ok(())
    }

    /// Load `<dir>/plan.json`, rejecting it when it belongs to another key.
    ///
    /// A rejected plan is deleted so the directory is rebuilt from scratch.
    pub fn load_plan<P: DeserializeOwned>(&self, key: &CacheKey, dir: &Path) -> Option<P> {
        let path = dir.join("plan.json");
        let raw = fs::read(&path).ok()?;
        let cached: CachedPlan = match serde_json::from_slice(&raw) {
            Ok(cached) => cached,
            Err(_) => {
                let _ = fs::remove_dir_all(dir);
                return None;
            }
        };
        if cached.cache_key != *key {
            let _ = fs::remove_dir_all(dir);
            return None;
        }
        match serde_json::from_value(cached.plan) {
            Ok(plan) => Some(plan),
            Err(_) => {
                let _ = fs::remove_dir_all(dir);
                None
            }
        }
    }
}

impl Default for TranscodeCache {
    fn default() -> Self {
        Self::new(Self::default_root(), DEFAULT_CACHE_BUDGET_BYTES)
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn dir_bytes(dir: &Path) -> u64 {
    let Ok(read) = fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in read.flatten() {
        if let Ok(meta) = entry.metadata() {
            if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

/// Whether the muxer's own playlist already lists `seg{k}.ts`.
///
/// The HLS muxer appends the `segK.ts` URI line only after the segment file is
/// closed, which is what [`TranscodeCache::segment_ready`] needs to distinguish
/// a finished segment from one that is still being written.
fn muxer_playlist_lists(dir: &Path, k: u64) -> bool {
    muxer_playlist_segments(dir).contains(&k)
}

/// Segment indices the muxer has already closed, per its own playlist.
///
/// Empty when the muxer has not written `index.m3u8` yet (no segment is closed
/// in that case either).
pub fn muxer_playlist_segments(dir: &Path) -> HashSet<u64> {
    let Ok(text) = fs::read_to_string(dir.join("index.m3u8")) else {
        return HashSet::new();
    };
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let stem = line.strip_prefix("seg")?.strip_suffix(".ts")?;
            stem.parse::<u64>().ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wdmm-cache-test-{tag}-{}-{}",
            std::process::id(),
            now_secs()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_key(ident: &str) -> CacheKey {
        CacheKey::new("acct", "/movies/a.mkv", "balanced", 4_000, ident)
    }

    #[test]
    fn dir_name_is_sixteen_hex_and_key_sensitive() {
        let a = make_key("etag-a");
        let name = a.dir_name();
        assert_eq!(name.len(), 16);
        assert!(name.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(name, make_key("etag-a").dir_name());
        assert_ne!(name, make_key("etag-b").dir_name());
        let mut other = make_key("etag-a");
        other.size += 1;
        assert_ne!(name, other.dir_name());
        other.size -= 1;
        other.preset_id = "high".into();
        assert_ne!(name, other.dir_name());
    }

    #[test]
    fn empty_ident_marks_ephemeral_key() {
        assert!(make_key("").is_ephemeral());
        assert!(!make_key("mtime:123").is_ephemeral());
    }

    #[test]
    fn segment_ready_requires_the_muxer_to_have_closed_the_segment() {
        let root = temp_root("ready");
        let cache = TranscodeCache::new(root, 1_000);
        let key = make_key("etag");
        let dir = cache.prepare(&key).unwrap();
        // seg0 is closed (the muxer listed it), seg1 is still being written.
        fs::write(TranscodeCache::segment_path(&dir, 0), vec![0u8; 100]).unwrap();
        fs::write(TranscodeCache::segment_path(&dir, 1), vec![0u8; 50]).unwrap();
        fs::write(
            dir.join("index.m3u8"),
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:4.000,\nseg0.ts\n",
        )
        .unwrap();
        assert_eq!(
            TranscodeCache::segment_ready(&dir, 0),
            SegmentLookup::Ready
        );
        assert_eq!(
            TranscodeCache::segment_ready(&dir, 1),
            SegmentLookup::Missing,
            "a half-written segment must not be served"
        );
        assert_eq!(
            TranscodeCache::segment_ready(&dir, 7),
            SegmentLookup::Missing
        );
        fs::write(
            dir.join("index.m3u8"),
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:4.000,\nseg0.ts\n#EXTINF:0.500,\nseg1.ts\n",
        )
        .unwrap();
        assert_eq!(
            TranscodeCache::segment_ready(&dir, 1),
            SegmentLookup::Ready,
            "closing the segment makes it servable"
        );
        fs::remove_dir_all(cache.root()).ok();
    }

    #[test]
    fn records_bytes_and_reports_stats() {
        let root = temp_root("stats");
        let cache = TranscodeCache::new(root, 1_000);
        let key = make_key("etag");
        let dir = cache.prepare(&key).unwrap();
        fs::write(TranscodeCache::segment_path(&dir, 0), vec![0u8; 100]).unwrap();
        fs::write(TranscodeCache::segment_path(&dir, 1), vec![0u8; 50]).unwrap();
        cache.touch(&key, &dir);
        cache.note_hit();
        cache.note_hit();
        cache.note_miss();
        let stats = cache.stats();
        assert_eq!(stats.entries, 1);
        assert_eq!(stats.bytes, 150);
        assert_eq!(stats.hits, 2);
        assert_eq!(stats.misses, 1);
        assert!(!stats.over_budget);
        assert_eq!(
            TranscodeCache::segment_ready(&dir, 7),
            SegmentLookup::Missing
        );
        fs::remove_dir_all(cache.root()).ok();
    }

    #[test]
    fn eviction_skips_the_playing_key_and_stops_at_target() {
        let root = temp_root("evict");
        let cache = TranscodeCache::new(root, 1_000);
        let playing = make_key("playing");
        let playing_dir = cache.prepare(&playing).unwrap();
        // The playing entry alone exceeds the budget: eviction must not stall
        // the renderer, so it is allowed to overshoot and reports it.
        fs::write(
            TranscodeCache::segment_path(&playing_dir, 0),
            vec![0u8; 1_200],
        )
        .unwrap();
        cache.touch(&playing, &playing_dir);

        let old = make_key("old");
        let old_dir = cache.prepare(&old).unwrap();
        fs::write(TranscodeCache::segment_path(&old_dir, 0), vec![0u8; 400]).unwrap();
        cache.touch(&old, &old_dir);

        // 1600 > 1000: eviction runs but must leave the playing entry alone,
        // even though that keeps the cache over budget.
        cache.evict(Some(&playing));
        assert!(playing_dir.exists());
        assert!(!old_dir.exists());
        let stats = cache.stats();
        assert!(stats.over_budget, "guard band is allowed to overshoot");
        assert_eq!(stats.entries, 1);
        fs::remove_dir_all(cache.root()).ok();
    }

    #[test]
    fn eviction_stops_at_the_target_watermark() {
        let root = temp_root("evict-target");
        let cache = TranscodeCache::new(root, 1_000);
        let keys: Vec<CacheKey> = (0..5).map(|i| make_key(&format!("k{i}"))).collect();
        for (i, key) in keys.iter().enumerate() {
            let dir = cache.prepare(key).unwrap();
            fs::write(TranscodeCache::segment_path(&dir, 0), vec![0u8; 300 + i]).unwrap();
            cache.touch(key, &dir);
        }
        assert_eq!(cache.stats().bytes, 1_510);
        cache.evict(None);
        let stats = cache.stats();
        assert!(
            stats.bytes <= 900,
            "eviction must get under budget*0.9, got {}",
            stats.bytes
        );
        assert!(!stats.over_budget);
        fs::remove_dir_all(cache.root()).ok();
    }

    #[test]
    fn plan_round_trips_and_rejects_foreign_keys() {
        let root = temp_root("plan");
        let cache = TranscodeCache::new(root, 1_000);
        let key = make_key("etag");
        let dir = cache.prepare(&key).unwrap();
        #[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
        struct Plan {
            seg_secs: f64,
            starts: Vec<i64>,
        }
        let plan = Plan {
            seg_secs: 6.0,
            starts: vec![0, 6_000, 12_000],
        };
        cache.save_plan(&key, &dir, &plan).unwrap();
        let loaded: Option<Plan> = cache.load_plan(&key, &dir);
        assert_eq!(loaded, Some(plan));

        // A different key (same directory name impossible, but simulate a
        // moved/corrupted plan) invalidates the whole directory.
        let foreign = CacheKey::new("other", "/movies/a.mkv", "balanced", 4_000, "etag");
        let loaded: Option<Plan> = cache.load_plan(&foreign, &dir);
        assert!(loaded.is_none());
        assert!(!dir.exists(), "mismatched plan invalidates the directory");
        fs::remove_dir_all(cache.root()).ok();
    }

    #[test]
    fn stale_entries_are_purged_and_clear_wipes_everything() {
        let root = temp_root("purge");
        let cache = TranscodeCache::new(root.clone(), 1_000);
        let key = make_key("etag");
        let dir = cache.prepare(&key).unwrap();
        fs::write(TranscodeCache::segment_path(&dir, 0), vec![0u8; 10]).unwrap();
        cache.touch(&key, &dir);
        {
            let mut entries = cache.entries.lock().unwrap();
            let entry = entries.get_mut(&key).unwrap();
            entry.last_used = now_secs().saturating_sub(CACHE_PURGE_AFTER_SECS + 5);
        }
        cache.purge_stale();
        assert!(!dir.exists());
        assert_eq!(cache.stats().entries, 0);

        let another = make_key("etag2");
        let dir = cache.prepare(&another).unwrap();
        fs::write(TranscodeCache::segment_path(&dir, 0), vec![0u8; 10]).unwrap();
        cache.touch(&another, &dir);
        cache.clear().unwrap();
        assert!(!dir.exists());
        assert_eq!(cache.stats().bytes, 0);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn configure_switches_root_and_budget() {
        let a = temp_root("cfg-a");
        let b = temp_root("cfg-b");
        let cache = TranscodeCache::new(a.clone(), 10);
        let key = make_key("etag");
        let dir = cache.prepare(&key).unwrap();
        fs::write(TranscodeCache::segment_path(&dir, 0), vec![0u8; 10]).unwrap();
        cache.touch(&key, &dir);
        cache.configure(Some(b.clone()), Some(2_000));
        assert_eq!(cache.root(), b);
        assert_eq!(cache.budget_bytes(), 2_000);
        assert_eq!(cache.stats().entries, 0, "root switch drops the index");
        fs::remove_dir_all(a).ok();
        fs::remove_dir_all(b).ok();
    }

    #[test]
    fn missing_directory_is_not_an_error_for_bytes() {
        let root = temp_root("missing");
        let cache = TranscodeCache::new(root.clone(), 100);
        assert_eq!(cache.bytes_of(&make_key("x")), 0);
        assert_eq!(dir_bytes(Path::new("does-not-exist-xyz")), 0);
        fs::remove_dir_all(root).ok();
    }
}
