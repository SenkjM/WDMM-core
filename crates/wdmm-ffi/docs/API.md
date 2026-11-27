# wdmm-ffi — Public API

C ABI surface for `flutter_rust_bridge` (Android) and `csbindgen` (Windows
P/Invoke). A thin conversion layer only: every call delegates to a `wdmm-core`
crate. `libwdmm_ffi.so` is built with `--features ffmpeg` and links the prebuilt
`libav*.so` in-process; there is no `ffmpeg`/`ffprobe` subprocess and no stub
mode.
Every `FfiString` is owned by the caller — free it with `wdmm_ffi_free_string`.
Functions return `"ok"`, `"error:<message>"`, or JSON.

## String type
### `pub struct FfiString {`
- `pub ptr: *mut c_char`
### `pub extern "C" fn wdmm_ffi_free_string(s: FfiString)`
### `pub extern "C" fn wdmm_version() -> FfiString`
### `pub extern "C" fn wdmm_hello() -> FfiString`
### `pub extern "C" fn wdmm_library_init(path: *const c_char) -> FfiString`
Must run once before library/queue calls; a later call is a no-op returning
`"ok"`. Derives `<db_dir>/api-token` and enables the HLS diagnostic log.

## Proxy lifecycle
### `pub unsafe extern "C" fn wdmm_proxy_start(bind_mode_cstr: *const c_char, port: u16) -> FfiString`
Bind modes: `"loopback"`, `"lan_any"`, `"lan_if:<ip>"`, `"explicit:<addr>"`.
`port = 0` lets the OS choose; idempotent. Returns the port as a decimal string.
### `pub extern "C" fn wdmm_proxy_stop() -> FfiString`
### `pub extern "C" fn wdmm_proxy_port() -> i32`
`-1` when the proxy is not running.
### `pub extern "C" fn wdmm_proxy_register_file(file_path: *const std::os::raw::c_char, name: *const std::os::raw::c_char) -> FfiString`
`name` is nullable and only used for Content-Type. Returns
`{"token","path_template","host","port"}`; the caller builds
`http://{host}:{port}{path_template}`.
### `pub extern "C" fn wdmm_proxy_unregister(token: *const std::os::raw::c_char) -> FfiString`

## Account-scoped streaming back-source
### `pub extern "C" fn wdmm_proxy_register_account(account_id: *const c_char, remote_path: *const c_char, name: *const c_char, size: u64) -> FfiString`
Returns `{"token","path_template","host","port"}`; the caller builds
`http://{host}:{port}{path_template}`. Core resolves the driver from the account
and streams byte ranges in 1 MiB `get_content_range` chunks — no Dart callback
and no Dart-side byte buffering is involved.

## Account file transfer
### `pub extern "C" fn wdmm_account_download_to_file(account_id: *const c_char, path: *const c_char, local_path: *const c_char, resume_from: *const c_char) -> FfiString`
Streams the driver's content in 1 MiB ranges into `local_path` inside Core (the
crypt wrapper decrypts per 64 KiB block). `resume_from` appends to a partial
file. Returns `"ok:<bytes on disk>"` or `"error:…"`; Dart never receives bytes.
### `pub extern "C" fn wdmm_account_upload_from_file(account_id: *const c_char, path: *const c_char, local_path: *const c_char) -> FfiString`
Reads `local_path`, encrypts when the account is a crypt wrapper and PUTs it.
Returns `"ok:<bytes sent>"` or `"error:…"`.

## HLS VOD cast
### `pub extern "C" fn wdmm_proxy_register_hls(device_id: *const c_char, input_url: *const c_char, preset_id: *const c_char) -> FfiString`
Resolves the `TranscodePreset` from the DB, starts a pull-driven VOD session and
returns `{"token","playlist_template","host","port"}`. The proxy synthesises
`index.m3u8` from the segment plan; seeking means requesting another
`seg<k>.ts` — there is no seek call. This binding carries no cache identity
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
HLS VOD fields: `run_state` (`"active"`/`"idle"`/`"error"`; a throttled job that
is paused stays `"active"`), `alignment` (`"exact"`/`"keyframe"` or null),
`drift_ms` (how far the run's output sits from the plan grid, bounded by one
segment), `ready_segments`, `seg_count`, `seg_secs`, `lookahead_segments`
(`wdmm_proxy::AHEAD_SEGMENTS`, the pause window), `cache_hits`, `cache_bytes`,
`cache_over_budget` — plus `phase`, `playlist_ready`, `segments`, `encode_speed`,
`encode_content_ms`, `encode_wall_ms`, `encode_mode`, `video_encoder`,
`audio_encoder`, `encode_warnings` and `diagnostics` (`wdmm_ffmpeg::diag::tail(40)`).
### `pub extern "C" fn wdmm_dlna_session_cmd(handle: u32, cmd_json: *const c_char) -> FfiString`
Commands: `play`, `pause`, `stop`, `seek` (`position_ms`), `select`
(`remote_path`, `name`, `size`, `preset_id`), `subtitle` (`path` sidecar or
`path#embedded:<idx>`, null = off) and `cast`. `cast` is valid only while
`phase == "warming"` and `playlist_ready` is true; it points the renderer at the
playlist and moves to `playing`.
### `pub extern "C" fn wdmm_dlna_session_close(handle: u32) -> FfiString`
### `pub extern "C" fn wdmm_dlna_sink_protocol_info_json(renderer_location: *const c_char) -> FfiString`
### `pub extern "C" fn wdmm_dlna_discover_ext_json(timeout_ms: u32) -> FfiString`

## From Dart
Dart owns every registration: it holds the token, builds the URL from the
returned `host`/`port`/`path_template`, and polls `wdmm_dlna_session_state` until
`playlist_ready` before sending `cast`. Stream callbacks may use
`NativeCallable.listener` for the async fill path.