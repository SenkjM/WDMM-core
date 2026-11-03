# wdmm-ffi — Public API

C ABI surface for `flutter_rust_bridge` (Android) and `csbindgen` (Windows
P/Invoke). Thin conversion layer only: every call delegates to a `wdmm-core`
crate. `libwdmm_ffi.so` is built in-process with `--features ffmpeg` and links
the prebuilt `libav*.so`; there is no `ffmpeg`/`ffprobe` subprocess and no stub
mode.

Strings are returned as owned `FfiString`s; free every one with
`wdmm_ffi_free_string`. Functions return `"ok"`, `"error:<message>"`, or JSON.

## String type

### `pub struct FfiString {`

- `pub ptr: *mut c_char`

### `pub extern "C" fn wdmm_ffi_free_string(s: FfiString)`

### `pub extern "C" fn wdmm_version() -> FfiString`

### `pub extern "C" fn wdmm_hello() -> FfiString`

### `pub extern "C" fn wdmm_library_init(path: *const c_char) -> FfiString`

Must run once before library/queue calls; a second call is a no-op returning
`"ok"`. Derives `<db_dir>/api-token` and enables the HLS diagnostic log.

## Proxy lifecycle

### `pub unsafe extern "C" fn wdmm_proxy_start(bind_mode_cstr: *const c_char, port: u16) -> FfiString`

Bind modes: `"loopback"`, `"lan_any"`, `"lan_if:<ip>"`, `"explicit:<addr>"`.
`port = 0` lets the OS choose. Returns the port as a decimal string; idempotent.

### `pub extern "C" fn wdmm_proxy_stop() -> FfiString`

### `pub extern "C" fn wdmm_proxy_port() -> i32`

`-1` when the proxy is not running.

### `pub extern "C" fn wdmm_proxy_register_file(file_path: *const std::os::raw::c_char, name: *const std::os::raw::c_char) -> FfiString`

`name` is nullable and only used for Content-Type. Returns
`{"token","path_template","host","port"}`; the caller builds
`http://{host}:{port}{path_template}`.

### `pub extern "C" fn wdmm_proxy_unregister(token: *const std::os::raw::c_char) -> FfiString`

## Stream callback (driver byte ranges)

### `pub type WdmmProxyRangeCallback = unsafe extern "C" fn(user_data: *mut std::os::raw::c_void, start: i64, end: i64, out_buf: *mut *mut u8, out_len: *mut usize, request_id: u64) -> i32;`

- **Sync fill**: allocate with `wdmm_proxy_stream_alloc_buf`, set `*out_buf`
  and `*out_len`, return `0`. Rust frees the buffer after consuming it.
- **Async fill**: return `0` with `*out_buf == null`, then call
  `wdmm_proxy_stream_set_result(request_id, …)` later (Dart
  `NativeCallable.listener`).
- `start`/`end` are inclusive; `end = -1` means read to EOF. Non-zero return = error.

### `pub extern "C" fn wdmm_proxy_stream_alloc_buf(len: usize) -> *mut u8`

### `pub unsafe extern "C" fn wdmm_proxy_stream_free_buf(buf: *mut u8, len: usize)`

### `pub unsafe extern "C" fn wdmm_proxy_stream_set_result(request_id: u64, buf: *mut u8, len: usize, status: i32)`

### `pub unsafe extern "C" fn wdmm_proxy_register_stream(size: u64, name: *const c_char, cb: WdmmProxyRangeCallback, user_data: *mut std::os::raw::c_void) -> FfiString`

Returns `{"token","path_template","host","port"}`; `size = 0` means unknown.

### `pub unsafe extern "C" fn wdmm_proxy_renew_stream_callback(token: *const c_char, cb: WdmmProxyRangeCallback, user_data: *mut std::os::raw::c_void) -> FfiString`

## HLS VOD cast

### `pub extern "C" fn wdmm_proxy_register_hls(device_id: *const c_char, input_url: *const c_char, preset_id: *const c_char) -> FfiString`

Resolves the `TranscodePreset` from the DB, starts a pull-driven VOD session and
returns `{"token","playlist_template","host","port"}`. The proxy synthesises
`index.m3u8` from the segment plan; there is no seek call — the renderer seeks
by requesting another `seg<k>.ts`. This binding carries no cache identity
(legacy Dart-owned token), so its segments stay session-local.

### `pub extern "C" fn wdmm_proxy_unregister_hls(token: *const c_char) -> FfiString`

Cancels the job and returns `"ok"`; cached segments are kept.

### `pub extern "C" fn wdmm_transcode_presets_json() -> FfiString`

### `pub extern "C" fn wdmm_transcode_preset_save(json: *const c_char) -> FfiString`

### `pub extern "C" fn wdmm_transcode_preset_delete(id: *const c_char) -> FfiString`

Built-in presets cannot be deleted (`"error:builtin"`).

## Transcode cache

### `pub extern "C" fn wdmm_transcode_cache_stats_json() -> FfiString`

`{"ok":true,"bytes":N,"budget":N,"entries":N,"hits":N,"misses":N,"over_budget":bool}`
or `{"ok":false,"error":"proxy not started"}`.

### `pub extern "C" fn wdmm_transcode_cache_clear() -> FfiString`

`{"ok":true}` or `{"ok":false,"error":…}`.

## DLNA sessions

### `pub extern "C" fn wdmm_dlna_session_start(config_json: *const c_char) -> FfiString`

Config keys: `renderer_location`, `renderer_usn`, `account_id`, `remote_path`,
`name`, `size`, `preset_id`, `force_direct`, `warmup` (default `true`),
`ui_lang`, `cache_dir` (default `<temp>/wdmm-transcode`), `cache_budget_mb`
(default 2048) and `cache_ident` (etag/mtime; absent ⇒ session-local).
Returns `{"ok":true,"handle":N,"url","provider":"direct"|"hls","protocol_info","phase":"playing"|"warming","compat","subtitle_status"}`.

### `pub extern "C" fn wdmm_dlna_session_state(handle: u32) -> FfiString`

Adds the HLS VOD fields: `run_state` (`"active"`/`"idle"`/`"error"`),
`alignment` (`"exact"`/`"keyframe"`/`"calibrated"`/`"approx"` or null),
`drift_ms` (only meaningful for `approx`), `ready_segments`, `seg_count`,
`seg_secs`, `lookahead_segments` (`ceil(60 / seg_secs)`), `cache_hits`,
`cache_bytes`, `cache_over_budget` — plus `playlist_ready`, `phase`, `segments`,
`encode_speed`, `encode_mode`, `video_encoder`, `audio_encoder`,
`encode_warnings` and `diagnostics` (`wdmm_ffmpeg::diag::tail(40)`).

### `pub extern "C" fn wdmm_dlna_session_cmd(handle: u32, cmd_json: *const c_char) -> FfiString`

Commands: `play`, `pause`, `stop`, `seek` (`position_ms`), `select`
(`remote_path`, `name`, `size`, `preset_id`), `subtitle` (`path` sidecar or
`path#embedded:<idx>`, null = off) and `cast`. `cast` is valid only while
`phase == "warming"` and `playlist_ready` is true; it points the renderer at
the playlist and moves to `playing`.

### `pub extern "C" fn wdmm_dlna_session_close(handle: u32) -> FfiString`

### `pub extern "C" fn wdmm_dlna_sink_protocol_info_json(renderer_location: *const c_char) -> FfiString`

### `pub extern "C" fn wdmm_dlna_discover_ext_json(timeout_ms: u32) -> FfiString`

## From Dart

Registrations are owned by the Dart side: it holds the token, builds the URL
from the returned `host`/`port`/`path_template`, and polls
`wdmm_dlna_session_state` until `playlist_ready` before sending `cast`. Stream
callbacks may use `NativeCallable.listener` for the async fill path.
