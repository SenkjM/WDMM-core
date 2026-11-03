# wdmm-proxy — Public API

Unified local HTTP server with two back-source modes: token-only byte-range
proxying (`/s/<token>`) and pull-driven HLS VOD casting (`/h/<token>/index.m3u8`).
Callers compose `http://{host}:{port}{path_template}` from the registration plus
`addr()`; the proxy never builds a full URL.
## Design rules
- One `ProxyHandle` per process in the FFI; the server starts lazily on the first `register_*`, stops after the idle timeout (default 10 minutes) or on drop.
- Byte-range sources are a local file or a driver callback.
- HLS is **pull-driven VOD**: the proxy synthesises the playlist from a segment plan and transcodes segments on demand into a persistent cache. There is no live/push playlist and no seek call — a seek is a request for another segment index, and a cached one is served immediately.
- One concurrent transcode job per process (`JobGate`). A cast has exactly **one** long-lived job: instead of cancelling and reopening on every jump, the job is *paused* once it is more than `AHEAD_SEGMENTS` ahead of the newest request and resumed when the renderer catches up. Only a jump that leaves the covered window starts a second run. The gate is priority-aware: a cast's transcode always wins over a background subtitle probe, so opening a cast never queues behind an FFI probe (contract §3.6).
- The segment grid is fixed **before** the session is registered, from the source itself (`wdmm_ffmpeg::source_plan`): the container keyframe index when the video can be copied, an exact encoder grid when it must be re-encoded. A source with no usable grid is refused rather than served on a nominal one.
## Error types
### `pub enum ProxyError {`
- `Io(#[from] std::io::Error)`, `Ffmpeg(String)`, `Cache(String)`; `Busy(String)` = `"transcoding busy"` when the gate times out, or `CAST_PRIORITY_ERROR` (`"transcoding busy: cast transcode has priority"`) when a background job cannot start while a cast holds the gate.
### `pub type ProxyResult<T> = std::result::Result<T, ProxyError>;`
## Bind mode and handle
### `pub enum BindMode {`
- `Loopback` (`127.0.0.1`), `LanAny` (`0.0.0.0`), `LanIf(String)`,
  `Explicit(SocketAddr)`; `to_bind_addr(port) -> String`.
### `pub struct ProxyHandle {`
- `new(bind_mode: BindMode, port: u16)` (`port = 0` = OS-assigned; 10-minute idle timeout) and
  `with_idle_timeout(bind_mode: BindMode, port: u16, idle_timeout: Duration)`.
- `addr() -> Option<SocketAddr>` — `None` until the server starts.
- `transcode_cache() -> Arc<TranscodeCache>`, `job_gate() -> Arc<JobGate>`.
- `stop()` — drops the listener (same as dropping the handle).
## Byte-range registration
### `pub type BoxStream = std::pin::Pin<Box<dyn futures_core::Stream<Item = std::io::Result<Bytes>> + Send>>;`
### `pub struct Registration {`
- `token: String` (32-hex), `path_template: String` (e.g. `"/s/3f7a…"`).
### `pub async fn register_file(&self, path: impl Into<PathBuf>, name: Option<String>, ttl_secs: Option<u64>, allowed_peers: Option<Vec<IpAddr>>) -> ProxyResult<Registration>`
`name` only feeds Content-Type guessing; TTL and peer whitelist are optional.
### `pub async fn register_driver<F>(&self, size: u64, name: Option<String>, ttl_secs: Option<u64>, allowed_peers: Option<Vec<IpAddr>>, ranges_fn: F) -> ProxyResult<Registration>`
`start`/`end` are inclusive byte offsets; `end = -1` means read to EOF;
`size = 0` means unknown. `F: Fn(i64, i64) -> BoxStream + Send + Sync + 'static`.
### `pub fn renew_callback<F>(&self, token: &str, ranges_fn: F)`
Replaces the callback (e.g. after re-auth); no-op for unknown tokens/files.
### `pub fn unregister(&self, token: &str)`
### `pub fn revoke_token(&self, token: &str)`
Security-oriented alias; in-flight requests fail on their next hit.
### `pub fn resolve_range(header: &str, size: u64) -> Option<(u64, u64)>`
Single `bytes=` range only; `None` → respond `416`.
### `pub fn content_type_for(name: Option<&str>) -> &'static str`
Extension table shared with the Dart `LocalStreamBridge._contentTypeFor` table;
subtitle sidecars (`srt`/`ass`/`ssa`/`vtt`/`sub`) are `text/*`.
## HLS VOD sessions
### `pub struct HlsSessionOpts {`
- `device_id`, `input_url`, `preset: TranscodePreset`, `ttl_secs`,
  `allowed_peers`, `cache: HlsCacheOptions`.
- One session per `(device_id, input_url)`; a new registration replaces the old.
### `pub struct HlsCacheOptions {`
- `root: Option<PathBuf>` (default `<temp_dir>/wdmm-transcode`),
  `budget_bytes: Option<u64>` (default 2 GiB), `account_id`, `remote_path`,
  `preset_id`, `size: u64`, `ident: String` (etag/mtime; empty = ephemeral,
  rewritten per session); `cache_key() -> CacheKey` derives the cache identity.
### `pub struct HlsRegistration {`
- `token`, `playlist_template` (e.g. `"/h/3f7a…/index.m3u8"`).
### `pub async fn register_hls(&self, opts: HlsSessionOpts) -> ProxyResult<HlsRegistration>`
Resolves the segment plan (a persisted `plan.json`, else `source_plan` with the
video copied, else `source_plan` with it re-encoded), persists it, registers the
session and starts a pre-buffering run at segment 0, so the playlist is servable
as soon as this returns. `input_url` is a local path or a loopback proxy URL for
encrypted sources. Fails with `ProxyError::Cache` (`Ffmpeg` when the `ffmpeg`
feature is off) when no grid can be planned.
### `pub async fn register_hls_nowait(&self, opts: HlsSessionOpts) -> ProxyResult<HlsRegistration>`
Same work as `register_hls`; kept as a separate entry point so the FFI can keep
its two-phase (`phase: "warming"` → cast) contract. `playlist_ready` is true as
soon as the session exists.
### `pub fn hls_progress(&self, token: &str) -> Option<HlsProgress>`
### `pub fn unregister_hls(&self, token: &str)`
Cancels the job and removes the session; cached segments stay on disk.
Segment requests wait up to 20 s (`504` on timeout, `500` on job error, `404`
past the end). Paths are flat: `seg<k>.ts` only.
## Progress and runtime state
### `pub struct HlsProgress {`
- Plan: `playlist_ready` (always true — the plan precedes the session),
  `alignment: Option<&'static str>` (`exact` | `keyframe`),
  `plan_duration_ms: Option<i64>`, `seg_secs: f64`, `segment_count: usize`,
  `ready_k`, `max_requested_k: Option<u64>`, `drift_ms: Option<i64>`.
- Run: `run_state: &'static str` (`active` | `idle` | `error`), `ended`,
  `error`, `segments: u32`, `content_ms`, `wall_ms`, `speed`, `encoder`, `warnings`.
- Cache: `cache_hits: u64`, `cache_bytes: u64`, `over_budget: bool`.

`content_ms` and `drift_ms` are absolute media time: `content_ms` is the end of the
ready front on the plan grid, `drift_ms` is how far the run's own output differs
from that grid (bounded by one segment). A run that is more than `AHEAD_SEGMENTS`
ahead of the newest request is paused, not cancelled, so `run_state` stays
`active` while a job is held.
## Transcode cache
Layout: `<root>/<dir_name>/plan.json` + `seg<k>.ts`, `dir_name` = 16-hex
SHA-256 of the cache key. Defaults: `DEFAULT_CACHE_SUBDIR = "wdmm-transcode"`,
`DEFAULT_CACHE_BUDGET_BYTES = 2 GiB`, `CACHE_PURGE_AFTER_SECS = 24 h`,
`GUARD_BACK_SEGMENTS = 30`.
### `pub struct CacheKey {`
- `account_id`, `remote_path`, `preset_id`, `size: u64`, `ident: String`.
### `pub struct TranscodeCache {`
- `stats()` → `bytes`, `budget_bytes`, `entries`, `hits`, `misses`, `over_budget`.
- `evict(protect)` — LRU down to 90 % of the budget; the playing session's
  directory (its guard band) is never evicted, so the cache may stay over budget
  rather than stalling the renderer.
- `purge_stale()` — drops entries untouched for 24 h; `clear()`, `invalidate(key)`.
- `save_plan(key, dir, plan)`, `load_plan(key, dir)` — a persisted plan is a
  cache hit across sessions.
## Scheduling and admission
A request for segment `k` is served from disk when the muxer has closed it, waits
while the running job covers it (`anchor_k <= k <= front + AHEAD_SEGMENTS`), and
otherwise starts a run at `k` (merged to warm `k+1`/`k+2`), throttled to one
reopen per second. `AHEAD_SEGMENTS = 12` bounds how far ahead of the newest
request the single job may run before it is paused; `HLS_SEGMENT_WAIT = 20 s`
bounds how long a request blocks.
Limits: `MAX_CONCURRENT_HLS_JOBS = 1`, `MAX_CONCURRENT_BACKGROUND_JOBS = 1`,
`JOB_ACQUIRE_TIMEOUT = 15 s`, `BUSY_ERROR = "transcoding busy"`,
`CAST_PRIORITY_ERROR = "transcoding busy: cast transcode has priority"`.
### `pub struct JobGate {`
- `new(max_concurrent)`, `global()`, `capacity()`, `in_use()`, `waiters()`,
  `preemptions()`, `background_rejections()`, `acquire(timeout)`,
  `acquire_async(timeout)`. A `JobPermit` releases on drop; waiting more than 15 s
  yields `ProxyError::Busy("transcoding busy")`.
- **Priority classes (contract §3.6).** `acquire_async` is the cast class (HLS
  transcode: warm-up and renderer re-opens) and is never queued behind background
  work — if a background job holds the slot it is revoked and the cast takes it at
  once (`JobPermit::preempted() == true`). `acquire` is the background class (FFI
  subtitle probe / extractor): it starts only while no cast job is running or
  waiting, and fails with `CAST_PRIORITY_ERROR` when a cast had priority.
  `JobPermit::is_revoked()` reports a background permit whose slot the cast took.
- Preemption revokes the slot, it does not kill the running libav call: a revoked
  background job keeps executing but still counts against
  `MAX_CONCURRENT_BACKGROUND_JOBS`, so at most one background decode overlaps the
  single cast job. Unit tests for the rule live in `src/gate.rs`; the measured
  contention numbers are in `tests/gate_priority.rs`.
## Playlist rendering
### `pub const PLAYLIST_CONTENT_TYPE: &str = "application/vnd.apple.mpegurl";`
### `pub const SEGMENT_CONTENT_TYPE: &str = "video/mp2t";`
### `pub fn render_playlist(segments: &[PlaylistSegment]) -> String`
Emits `#EXTM3U`, `#EXT-X-VERSION:3`, `#EXT-X-PLAYLIST-TYPE:VOD`,
`#EXT-X-TARGETDURATION` (ceil of the longest segment), `#EXT-X-MEDIA-SEQUENCE:0`,
then `#EXTINF:<seconds.3>,` + `seg<k>.ts` per entry, and `#EXT-X-ENDLIST`.
