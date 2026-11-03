# wdmm-proxy — Public API

Unified local HTTP server with two back-source modes: token-only byte-range
proxying (`/s/<token>`) and pull-driven HLS VOD transcode casting
(`/h/<token>/index.m3u8`). The proxy never builds a full URL — callers compose
`http://{host}:{port}{path_template}` from the registration plus `addr()`.

## Design rules

- One `ProxyHandle` per process; the server starts lazily on the first
  `register_*` and stops after the idle timeout (default 10 minutes) or on drop.
- Byte-range sources are either a local file or a driver callback.
- HLS is **pull-driven VOD**: the proxy synthesises the playlist from a segment
  plan and transcodes segments on demand into a persistent cache. There is no
  live/push playlist and no seek call; a seek is a request for another segment
  index, and a cached one is served immediately.
- Doing so keeps one concurrent transcode job per process (`JobGate`), and a run
  parks itself once it is ~60 s ahead of the newest request.

## Error types

### `pub enum ProxyError {`

- `Io(#[from] std::io::Error)`
- `Ffmpeg(String)`
- `Cache(String)`
- `Busy(String)` — `"transcoding busy"` when the gate times out.

### `pub type ProxyResult<T> = std::result::Result<T, ProxyError>;`

## Bind mode and handle

### `pub enum BindMode {`

- `Loopback` — `127.0.0.1`; `LanAny` — `0.0.0.0`; `LanIf(String)` — one LAN
  address; `Explicit(SocketAddr)` — a fixed address.

### `pub fn to_bind_addr(&self, port: u16) -> String`

### `pub struct ProxyHandle {`

- `pub fn new(bind_mode: BindMode, port: u16) -> Self` — `port = 0` lets the OS choose.
- `pub fn with_idle_timeout(bind_mode: BindMode, port: u16, idle_timeout: Duration) -> Self`
- `pub fn addr(&self) -> Option<SocketAddr>` — `None` until the server starts.
- `pub fn transcode_cache(&self) -> Arc<TranscodeCache>`
- `pub fn job_gate(&self) -> Arc<JobGate>`
- `pub fn stop(&self)` — drops the listener; dropping the handle does the same.

## Byte-range registration

### `pub type BoxStream = std::pin::Pin<Box<dyn futures_core::Stream<Item = std::io::Result<Bytes>> + Send>>;`

### `pub struct Registration {`

- `token: String` (32-hex), `path_template: String` (e.g. `"/s/3f7a…"`).

### `pub async fn register_file(&self, path: impl Into<PathBuf>, name: Option<String>, ttl_secs: Option<u64>, allowed_peers: Option<Vec<IpAddr>>) -> ProxyResult<Registration>`

`name` only feeds Content-Type guessing; `ttl_secs`/`allowed_peers` are optional.

### `pub async fn register_driver<F>(&self, size: u64, name: Option<String>, ttl_secs: Option<u64>, allowed_peers: Option<Vec<IpAddr>>, ranges_fn: F) -> ProxyResult<Registration>`

`where F: Fn(i64, i64) -> BoxStream + Send + Sync + 'static`

`start`/`end` are inclusive byte offsets; `end = -1` means read to EOF.
`size = 0` when unknown.

### `pub fn renew_callback<F>(&self, token: &str, ranges_fn: F)`

Replaces the callback for an existing token (e.g. after re-auth); no-op for an
unknown token or a local file.

### `pub fn unregister(&self, token: &str)`

### `pub fn revoke_token(&self, token: &str)`

Alias with security-oriented naming; in-flight requests fail on their next hit.

### `pub fn resolve_range(header: &str, size: u64) -> Option<(u64, u64)>`

Single `bytes=` range only; `None` → respond `416`.

### `pub fn content_type_for(name: Option<&str>) -> &'static str`

Extension table shared with the Dart `LocalStreamBridge._contentTypeFor` table
(includes `srt`/`ass`/`ssa`/`vtt`/`sub` as text/plain).

## HLS VOD sessions

### `pub struct HlsSessionOpts {`

- `device_id`, `input_url`, `preset: TranscodePreset`, `ttl_secs`,
  `allowed_peers`, `cache: HlsCacheOptions`

One session per `(device_id, input_url)`; a new registration replaces the old.

### `pub struct HlsCacheOptions {`

- `root: Option<PathBuf>` (default `<temp_dir>/wdmm-transcode`),
  `budget_bytes: Option<u64>` (default 2 GiB),
  `account_id`, `remote_path`, `preset_id`, `size: u64`,
  `ident: String` (etag/mtime; empty = ephemeral, rewritten per session).

### `pub fn cache_key(&self) -> CacheKey`

### `pub struct HlsRegistration {`

- `token`, `playlist_template` (e.g. `"/h/3f7a…/index.m3u8"`).

### `pub async fn register_hls(&self, opts: HlsSessionOpts) -> ProxyResult<HlsRegistration>`

Waits until the plan exists (container keyframe index, or up to 20 s of warm-up
calibration). If calibration cannot finish in time the session starts on an
approximate grid (alignment `approx`, `drift_ms` reported) rather than failing
the cast. `input_url` is a local path or a loopback `http://127.0.0.1:{port}/…`
proxy URL for encrypted sources.

### `pub async fn register_hls_nowait(&self, opts: HlsSessionOpts) -> ProxyResult<HlsRegistration>`

Returns immediately in a warming state; poll `hls_progress().playlist_ready`
before pointing the renderer at the playlist.

### `pub fn hls_progress(&self, token: &str) -> Option<HlsProgress>`

### `pub fn unregister_hls(&self, token: &str)`

Cancels the job and removes the session; the cached segments stay on disk.

Segment requests wait up to 20 s for the running job (`504` on timeout, `500` on
job error, `404` past the end). Paths are flat: `seg<k>.ts` only.

## Progress and runtime state

### `pub struct HlsProgress {`

- Plan: `playlist_ready`, `alignment: Option<&'static str>`,
  `plan_duration_ms: Option<i64>`, `seg_secs: f64`, `segment_count: usize`,
  `ready_k`, `max_requested_k: Option<u64>`, `drift_ms: Option<i64>`.
- Run: `run_state: &'static str` (`active` | `idle` | `error`), `ended`,
  `error`, `segments: u32`, `content_ms`, `wall_ms`, `speed`, `encoder`,
  `warnings`.
- Cache: `cache_hits: u64`, `cache_bytes: u64`, `over_budget: bool`.

`drift_ms` is only meaningful for `alignment == "approx"`; the other alignments
do not drift. A run parks after ~60 s of lookahead (`run_state = "idle"`) and a
segment request resumes it from the requested index.

## Transcode cache

`<root>/<dir_name>/plan.json` + `seg<k>.ts`, where `dir_name` is a 16-hex
SHA-256 of the cache key.

### `pub struct CacheKey {`

- `account_id`, `remote_path`, `preset_id`, `size: u64`, `ident: String`.

### `pub const DEFAULT_CACHE_SUBDIR: &str = "wdmm-transcode";`

### `pub const DEFAULT_CACHE_BUDGET_BYTES: u64 = 2 * 1024 * 1024 * 1024;`

### `pub const CACHE_PURGE_AFTER_SECS: u64 = 24 * 3600;`

### `pub const GUARD_BACK_SEGMENTS: u64 = 30;`

### `pub struct TranscodeCache {`

- `pub fn stats(&self) -> TranscodeCacheStats` (`bytes`, `budget_bytes`,
  `entries`, `hits`, `misses`, `over_budget`)
- `pub fn evict(&self, protect: Option<&CacheKey>)` — LRU down to 90 % of the
  budget; the playing session is never evicted, so the cache may stay over
  budget rather than stall the renderer.
- `pub fn purge_stale(&self)` — drops entries untouched for 24 h.
- `pub fn clear(&self) -> ProxyResult<()>`, `pub fn invalidate(&self, key: &CacheKey) -> ProxyResult<()>`
- `pub fn save_plan<P: Serialize>(&self, key: &CacheKey, dir: &Path, plan: &P) -> ProxyResult<()>`
- `pub fn load_plan<P: DeserializeOwned>(&self, key: &CacheKey, dir: &Path) -> Option<P>`

## Scheduling and admission

### `pub const LOOKAHEAD_SECS: f64 = 60.0;`

### `pub const RESUME_MARGIN_SEGMENTS: u64 = 3;`

### `pub struct SchedulePolicy {`

- `new(plan_len, seg_secs)`, `lookahead()`, `decide(run, running, k) -> Action`
  (`Serve`/`Wait`/`Start{from_k}`/`Ended`), `should_stop`, `resume_from`,
  `within_resume_margin`.
- `pub fn lookahead_segments(seg_secs: f64) -> u64` — `ceil(60 / seg_secs)`, min 1.

### `pub const MAX_CONCURRENT_HLS_JOBS: usize = 1;`

### `pub const JOB_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(15);`

### `pub const BUSY_ERROR: &str = "transcoding busy";`

### `pub struct JobGate {`

- `new(max_concurrent)`, `global()`, `capacity()`, `in_use()`, `waiters()`,
  `acquire(timeout)`, `acquire_async(timeout)`; a `JobPermit` releases on drop.
- Busy after 15 s → `ProxyError::Busy("transcoding busy")`.

## Playlist rendering

### `pub const PLAYLIST_CONTENT_TYPE: &str = "application/vnd.apple.mpegurl";`

### `pub const SEGMENT_CONTENT_TYPE: &str = "video/mp2t";`

### `pub fn render_playlist(segments: &[PlaylistSegment]) -> String`

```text
#EXTM3U
#EXT-X-VERSION:3
#EXT-X-PLAYLIST-TYPE:VOD
#EXT-X-TARGETDURATION:<max ceil(seconds)>
#EXT-X-MEDIA-SEQUENCE:0
#EXTINF:<seconds.3>,
seg<k>.ts
...
#EXT-X-ENDLIST
```

## From Dart

`wdmm-ffi` exposes the process-wide handle; the Dart side composes URLs from
`{host, port, path_template}` and drives HLS warm-up by polling
`wdmm_dlna_session_state` for `playlist_ready`.
