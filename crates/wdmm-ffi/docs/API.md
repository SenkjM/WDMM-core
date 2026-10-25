# wdmm-ffi — Public API

C ABI surface for flutter_rust_bridge (Android) and csbindgen (Windows).
Thin conversion layer — no business logic. All functions are `extern "C" #[no_mangle]`.

Strings are returned as `FfiString` (owned; caller must free via
`wdmm_ffi_free_string`). All `*const c_char` parameters must be valid
null-terminated UTF-8 C strings.

---

## String type

### `#[repr(C)] struct FfiString { pub ptr: *mut c_char }`
Owned C string allocated by Rust. Caller is responsible for freeing it.

### `extern "C" fn wdmm_ffi_free_string(s: FfiString)`
Free an `FfiString` returned by any FFI function. No-op if `ptr` is null.

---

## Version / smoke

### `extern "C" fn wdmm_version() -> FfiString`
Returns the `wdmm-core` crate version string (from `CARGO_PKG_VERSION`).

### `extern "C" fn wdmm_hello() -> FfiString`
Returns `"Hello from WDMM Rust core!"`.

---

## Queue

### `extern "C" fn wdmm_queue_status_json() -> FfiString`
Return all queue tasks as a JSON array.

Each element:
```json
{"id":"…","kind":"…","status":"…","progress":0.5,
 "total_bytes":1000,"transferred_bytes":500,"error":null}
```

### `extern "C" fn wdmm_queue_cancel(task_id: *const c_char) -> FfiString`
Cancel task by id. Returns `"ok"`, `"not_found"`, or an error message.

### `extern "C" fn wdmm_queue_enqueue(json: *const c_char) -> FfiString`
Enqueue a task from JSON. Required fields: `id`, `kind`. Optional: `remote_path`,
`local_path`, `total_bytes`.

`kind` strings: `download_track`, `download_playlist`, `upload_file`, `backup`,
`sync_credentials`, `sync_library`, `sync_playlists`, `sync_manga`, `<other>`.

Returns `"ok"` or `"error:<message>"`.

Feature note: gated by `kRustQueueEnabled` on the Dart side (default `false`).

### `extern "C" fn wdmm_queue_pending_count() -> i32`
Number of tasks with `Pending` status.

---

## OpenAPI server

### `extern "C" fn wdmm_openapi_start(port: u16) -> FfiString`
Start the OpenAPI server on `127.0.0.1:<port>`. Port 0 = ephemeral.
Returns the bearer token on success, or `"error:<message>"`.
No-op if already running (returns existing token).
Caller must free the result.

### `extern "C" fn wdmm_openapi_stop() -> FfiString`
Stop the server. No-op if not running. Returns `"ok"`.

### `extern "C" fn wdmm_openapi_is_running() -> i32`
`1` if running, `0` otherwise.

### `extern "C" fn wdmm_openapi_port() -> i32`
Bound port, or `-1` if not running.

---

## Token management

### `extern "C" fn wdmm_token_get() -> FfiString`
Returns the current bearer token, or `""` if the server is not running.

### `extern "C" fn wdmm_token_rotate() -> FfiString`
Rotate the bearer token in-process (invalidates old token immediately).
Returns the new token, or `"error:not_running"`.

---

## Crypto

### `extern "C" fn wdmm_crypto_encrypt(passphrase: *const c_char, data: *const u8, data_len: usize) -> FfiString`
Encrypt `data_len` bytes at `data` with AES-256-GCM.
Returns base64-encoded ciphertext, or `"error:<message>"`.

### `extern "C" fn wdmm_crypto_decrypt(passphrase: *const c_char, data_b64: *const c_char) -> FfiString`
Decrypt base64-encoded ciphertext. Returns plaintext as UTF-8 (best-effort for
text payloads), or `"error:<message>"`.

---

## Sync

### `extern "C" fn wdmm_sync_trigger(webdav_url: *const c_char, webdav_user: *const c_char, webdav_pass: *const c_char, remote_dir: *const c_char) -> FfiString`
Run credential + library + playlist sync against a WebDAV driver.

Returns a JSON summary on success:
```json
{"credentials":{"total":N,"pushed":N,"pulled":N,"conflict":N},
 "library":{…},"playlists":{…}}
```
or `"error:<message>"`. Caller must free.

### `extern "C" fn wdmm_webdav_list(webdav_url: *const c_char, webdav_user: *const c_char, webdav_pass: *const c_char, path: *const c_char) -> FfiString`
List a WebDAV directory (Depth:1 PROPFIND via `WebDavDriver`).

Returns a JSON array on success:
```json
[{"name":"Song.mp3","path":"/Music/Song.mp3","is_dir":false,"size":1234,"modified":"2024-01-02T03:04:05Z"}]
```
`path` is the request directory joined with each entry `name`. `modified` is
RFC3339 or `null`. On failure returns `"error:<message>"`. Caller must free.

### `extern "C" fn wdmm_webdav_mkdir(webdav_url, webdav_user, webdav_pass, path) -> FfiString`
MKCOL. Returns `"ok"` or `"error:…"`.

### `extern "C" fn wdmm_webdav_rename(webdav_url, webdav_user, webdav_pass, old_path, new_path) -> FfiString`
MOVE/rename. Returns `"ok"` or `"error:…"`.

### `extern "C" fn wdmm_webdav_remove(webdav_url, webdav_user, webdav_pass, path) -> FfiString`
DELETE. Returns `"ok"` or `"error:…"`.

---

## DLNA

### `extern "C" fn wdmm_dlna_discover_json(timeout_ms: u32) -> FfiString`
Discover DLNA MediaRenderer devices. `timeout_ms` clamped to `[200, 10000]`.

Returns JSON array:
```json
[{"usn":"uuid:…","location":"http://…","server":"…"}]
```
or `"error:<message>"`.

### `extern "C" fn wdmm_dlna_cast(renderer_location: *const c_char, media_url: *const c_char) -> FfiString`
Cast a media URL to a DLNA renderer (SetAVTransportURI → Play).
Returns `"ok"` or `"error:<message>"`.

---

## Local HTTP proxy

Process-global proxy singleton (backed by `wdmm_proxy::ProxyHandle`). Starts
lazily, has an idle TTL, and restarts transparently on the next register. The
proxy never returns a full URL — callers compose
`http://{bind_host}:{port}{path_template}` themselves.

### `extern "C" fn wdmm_proxy_start(port: u16) -> FfiString`
Start the proxy on `127.0.0.1:<port>` (port 0 = ephemeral). Returns the assigned
port as a decimal string, or `"error:<message>"`. No-op if already running
(returns existing port). Caller must free.

### `extern "C" fn wdmm_proxy_stop() -> FfiString`
Stop the proxy. No-op if not running. Returns `"ok"`.

### `extern "C" fn wdmm_proxy_port() -> i32`
Bound port, or `-1` if not running.

### `extern "C" fn wdmm_proxy_register_file(file_path: *const c_char, name: *const c_char) -> FfiString`
Register a local file. `name` (may be `NULL`) is decorative — used only for
Content-Type guessing. Returns JSON:
```json
{"token":"3f7a…","path_template":"/s/3f7a…","port":43221}
```
The caller builds the URL as `http://127.0.0.1:{port}{path_template}`.
Returns `"error:proxy not started …"` if `wdmm_proxy_start` was not called.

### `extern "C" fn wdmm_proxy_unregister(token: *const c_char) -> FfiString`
Revoke a token. Returns `"ok"`.

---

## Proxy HLS

Process-global HLS session management. Requires `wdmm_proxy_start` to have
been called first. DLNA cast still takes the URL from the caller — the proxy
supplies only the token path; callers compose the full URL.

### `extern "C" fn wdmm_proxy_register_hls(device_id: *const c_char, file_path: *const c_char, mode_json: *const c_char) -> FfiString`
Register an HLS session for `device_id` + `file_path`.

`mode_json` (may be `NULL` → `{"mode":"direct_copy"}`):
```json
{"mode":"direct_copy"}
{"mode":"compat","video_codec":"libx264","audio_codec":"aac"}
```

Returns JSON on success:
```json
{"token":"3f7a…","playlist_template":"/h/3f7a…/index.m3u8","port":43221}
```
or `"error:<message>"`. Caller composes `http://127.0.0.1:{port}{playlist_template}`.

### `extern "C" fn wdmm_proxy_hls_seek(token: *const c_char, offset_secs: f64) -> FfiString`
Seek an active HLS session to `offset_secs`. Kills the current transcode job
and restarts from the new offset. Returns `"ok"` or `"error:<message>"`.

### `extern "C" fn wdmm_proxy_unregister_hls(token: *const c_char) -> FfiString`
Revoke an HLS session. The transcode job is killed and the segment directory
is cleaned up. Returns `"ok"`.

---

## Proxy stream (LocalStream / MustProxy)

For `ContentDeliveryKind::LocalStream` items — files that have no `raw_url`
and no filesystem path (e.g. crypt-wrapper drivers, streaming-only protocols).
The proxy serves bytes on demand via a C callback.

### `type WdmmProxyRangeCallback`

```c
typedef int32_t (*WdmmProxyRangeCallback)(
    void*     user_data,    // opaque; passed through from register_stream
    int64_t   start,        // inclusive byte offset
    int64_t   end,          // inclusive end offset; -1 = read to EOF
    uint8_t** out_buf,      // sync path: set to wdmm_proxy_stream_alloc_buf output
    size_t*   out_len,      // sync path: set to byte count in *out_buf
    uint64_t  request_id    // NEW — async reply channel id (see below)
);
```

Return `0` on success (either sync fill or async intent); any negative value is
an immediate error (proxy emits IO error to the HTTP client).

#### Sync fill path (native C callers)

Fill `*out_buf` (must be allocated via `wdmm_proxy_stream_alloc_buf`) and
`*out_len` before returning `0`. Rust takes ownership and frees the buffer
after consuming. The waiter for `request_id` is cancelled automatically.

#### Async fill path (Dart `NativeCallable.listener`)

Return `0` and leave `*out_buf` null. Rust parks the Tokio request-handler
thread (up to **30 seconds** — `PROXY_STREAM_TIMEOUT_SECS`) waiting for a call
to `wdmm_proxy_stream_set_result`. The Dart listener should:

1. Schedule `openContentRange(start, end)` on the Dart isolate.
2. Allocate a buffer: `buf = wdmm_proxy_stream_alloc_buf(len)`.
3. Copy bytes into `buf`.
4. Call `wdmm_proxy_stream_set_result(request_id, buf, len, 0)`.
5. On any error: `wdmm_proxy_stream_set_result(request_id, NULL, 0, -1)`.

`request_id` is always passed as an **input parameter** on the callback —
thread-locals cannot be used because the Dart listener runs later on the Dart
isolate, not on the Tokio thread that entered the callback.

On timeout the proxy surfaces an IO error to the HTTP client. Waiter map entries
are cleaned up on timeout, `set_result`, or `unregister`.

The `user_data` and `cb` must remain callable from any thread for the full
lifetime of the registration.

### `extern "C" fn wdmm_proxy_stream_alloc_buf(len: usize) -> *mut u8`
Allocate `len` bytes in Rust's heap for use as a callback output buffer.
The C callback **must** use this to allocate `*out_buf`; Rust reclaims the
memory after consuming it. Returns null if `len == 0`.

### `unsafe extern "C" fn wdmm_proxy_stream_free_buf(buf: *mut u8, len: usize)`
Free a buffer previously returned by `wdmm_proxy_stream_alloc_buf`. Exported for
C error paths. No-op if `buf` is null or `len == 0`.

### `extern "C" fn wdmm_proxy_stream_set_result(request_id: u64, buf: *mut u8, len: usize, status: i32)`

Deliver async Range bytes to a parked `invoke_c_callback` invocation.

- `request_id` — id received by the `WdmmProxyRangeCallback`.
- `buf`        — buffer allocated via `wdmm_proxy_stream_alloc_buf`; Rust takes
  ownership on the success path. Pass `NULL` on the error path.
- `len`        — byte count in `buf` (ignored when `status < 0`).
- `status`     — `0` = success (buf/len valid); negative = error.

Wakes the waiter for `request_id`. Safe to call from any thread. No-op (and
safe) if `request_id` is unknown (already timed out or double-called — orphaned
buffer is freed).

**Timeout**: if `set_result` is not called within 30 s, `invoke_c_callback`
returns a `TimedOut` IO error to the proxy. The waiter is removed from the map
at that point; a late `set_result` call is a safe no-op.

### `unsafe extern "C" fn wdmm_proxy_register_stream(size: u64, name: *const c_char, cb: WdmmProxyRangeCallback, user_data: *mut c_void) -> FfiString`
Register an in-process Range/byte stream with the proxy.

Parameters:
- `size` — known file size in bytes; `0` if unknown (stream until EOF).
- `name` — nullable; used only for Content-Type guessing (e.g. `"video.mp4"`).
- `cb` — Range callback (see `WdmmProxyRangeCallback` above).
- `user_data` — opaque pointer passed to every `cb` invocation.

Returns JSON on success:
```json
{"token":"3f7a…","path_template":"/s/3f7a…","port":43221}
```
or `"error:<message>"`. Caller builds URL as `http://127.0.0.1:{port}{path_template}`.

Requires `wdmm_proxy_start` to have been called. Caller must free the result.

### `unsafe extern "C" fn wdmm_proxy_renew_stream_callback(token: *const c_char, cb: WdmmProxyRangeCallback, user_data: *mut c_void) -> FfiString`
Replace the Range callback for an existing stream registration. Use after
credential refresh or decryption key rotation. No-op if `token` is unknown or
refers to a file (not stream) registration. Returns `"ok"`.

---

## Library database

### `extern "C" fn wdmm_library_init(path: *const c_char) -> FfiString`
Initialise the process-global library DB. Must be called once at app start.
`path` may be a file path or `":memory:"`.
No-op if already initialised. Returns `"ok"` or `"error:<message>"`.

### `extern "C" fn wdmm_track_upsert(json: *const c_char) -> FfiString`
Insert or replace a track from JSON. Required fields: `id`, `title`,
`account_id`, `remote_path`. Returns `"ok"` or `"error:<message>"`.

### `extern "C" fn wdmm_track_get(id: *const c_char) -> FfiString`
Get a track by id as JSON, `"null"` if not found, or `"error:<message>"`.

### `extern "C" fn wdmm_track_list() -> FfiString`
List all tracks as a JSON array.

### `extern "C" fn wdmm_track_search(query: *const c_char) -> FfiString`
Case-insensitive substring search on title/artist. Returns JSON array.

### `extern "C" fn wdmm_track_delete(id: *const c_char) -> FfiString`
Delete a track by id. Returns `"ok"` or `"error:<message>"`.

### `extern "C" fn wdmm_library_import_from_sqflite(old_db_path: *const c_char) -> FfiString`
One-shot import from an old sqflite `music_library.db`.
Returns JSON summary:
```json
{"already_done":false,"tracks":42,"playlists":3,"entries":120}
```
or `"error:<message>"`. Idempotent (guarded by KV flag `sqflite_imported = 1`).

---

## User folders

Process-global library DB must be initialised via `wdmm_library_init` first.

**Virtual root**: there is no DB row for the root folder. Its label is `我的文件夹`
(Android `kUserFolderRootId = "root"`). Use `null`, `""`, or `"root"` as
`parent_id` / `folder_id` to address it. Storage represents virtual-root
children as rows with `parent_id IS NULL`.

**Ref kinds**: `"album"` | `"artist"` | `"genre"` | `"track"` | `"manga"` | `"mangaSeries"` (mirrors Android `UserFolderRefKind`).

### Folder object shape
```json
{
  "id":         "uuid-…",
  "name":       "Rock",
  "parent_id":  null,
  "sort_order": 0,
  "created_at": 1700000000
}
```
`parent_id` is `null` for virtual-root children.

### Item object shape
```json
{
  "folder_id":  "uuid-… | root",
  "ref_kind":   "track",
  "ref_key":    "sha1hex…",
  "sort_order": 0,
  "added_at":   1700000000
}
```

---

### `extern "C" fn wdmm_user_folder_list_children(parent_id: *const c_char) -> FfiString`
List direct children of a folder. `parent_id` null / `""` / `"root"` → virtual
root (rows with `parent_id IS NULL`). Returns a JSON array of folder objects
(ordered by `sort_order`, then name), or `"error:<message>"`.

### `extern "C" fn wdmm_user_folder_create(json: *const c_char) -> FfiString`
Create a folder. Input:
```json
{"name":"Rock","parent_id":null,"sort_order":0}
```
`parent_id` absent/null/`"root"` → virtual root. A UUID `id` is generated.
Returns the new folder as JSON, or `"error:<message>"`.

### `extern "C" fn wdmm_user_folder_rename(id: *const c_char, name: *const c_char) -> FfiString`
Rename a folder by id. Returns `"ok"` or `"error:<message>"` (including
`"error:not found: …"` if the id is absent).

### `extern "C" fn wdmm_user_folder_move(id: *const c_char, new_parent_id: *const c_char) -> FfiString`
Move a folder to a new parent. `new_parent_id` null / `""` / `"root"` → virtual
root. Returns `"ok"` or `"error:<message>"` (including `"error:…cycle…"` when
the move would create a cycle).

### `extern "C" fn wdmm_user_folder_delete(id: *const c_char) -> FfiString`
Delete a folder and recursively all descendants + their items (child-first; no
FK cascade required). Returns `"ok"` or `"error:<message>"`.

### `extern "C" fn wdmm_user_folder_list_items(folder_id: *const c_char) -> FfiString`
List items in a folder. `folder_id = "root"` is valid (virtual root items).
Returns a JSON array of item objects ordered by `sort_order`, or
`"error:<message>"`.

### `extern "C" fn wdmm_user_folder_add_item(json: *const c_char) -> FfiString`
Insert or update an item reference. Input:
```json
{"folder_id":"uuid… | root","ref_kind":"album","ref_key":"…","sort_order":0}
```
`added_at` defaults to the current Unix timestamp if absent.
Returns `"ok"` or `"error:<message>"`.

### `extern "C" fn wdmm_user_folder_remove_item(folder_id: *const c_char, ref_kind: *const c_char, ref_key: *const c_char) -> FfiString`
Remove a specific item by `(folder_id, ref_kind, ref_key)`. Returns `"ok"` or
`"error:<message>"`.

### `extern "C" fn wdmm_user_folder_set_items(folder_id: *const c_char, json_array: *const c_char) -> FfiString`
Atomically replace **all** items in a folder in one shot. Input is a JSON
array of item objects:
```json
[
  {"ref_kind":"genre","ref_key":"…","sort_order":0},
  {"ref_kind":"manga","ref_key":"…","sort_order":1},
  {"ref_kind":"mangaSeries","ref_key":"…","sort_order":2}
]
```
`folder_id` may be `"root"` (virtual root). `sort_order` defaults to the element's
array index if absent; `added_at` defaults to the current Unix timestamp if
absent. The previous items for `folder_id` are deleted atomically before the
new ones are inserted (single transaction); other folders are unaffected.
Returns `"ok"` or `"error:<message>"`.

---

## Playlists

Process-global library DB must be initialised via `wdmm_library_init` first.

### Playlist object shape
```json
{
  "id":         "uuid-…",
  "name":       "Favourites",
  "created_at": "2024-01-01T00:00:00Z",
  "updated_at": "2024-01-01T00:00:00Z"
}
```

### Entry object shape
```json
{
  "playlist_id": "uuid-…",
  "position":    0,
  "music_id":    "sha1hex…",
  "source_name": "my_nas",
  "remote_path": "/music/song.mp3",
  "title":       "Song Title",
  "duration_ms": 210000
}
```

---

### `extern "C" fn wdmm_playlist_list() -> FfiString`
List all playlists. Returns a JSON array of playlist objects ordered by name,
or `"error:<message>"`.

### `extern "C" fn wdmm_playlist_create(json: *const c_char) -> FfiString`
Create a playlist. Input:
```json
{"id":"optional-uuid","name":"My Mix"}
```
`id` is generated (UUID v4) if absent. Returns the new playlist as JSON, or
`"error:<message>"`.

### `extern "C" fn wdmm_playlist_delete(id: *const c_char) -> FfiString`
Delete a playlist and all its entries (ON DELETE CASCADE). Returns `"ok"` or
`"error:<message>"`.

### `extern "C" fn wdmm_playlist_list_entries(id: *const c_char) -> FfiString`
List entries of a playlist ordered by `position`. Returns a JSON array of entry
objects, or `"error:<message>"`.

### `extern "C" fn wdmm_playlist_set_entries(id: *const c_char, json_array: *const c_char) -> FfiString`
Replace **all** entries of a playlist in one shot. Input is a JSON array:
```json
[
  {"music_id":"sha1…","source_name":"nas","remote_path":"/m/a.mp3",
   "title":"A","duration_ms":180000},
  {"music_id":"sha1…","source_name":"nas","remote_path":"/m/b.mp3",
   "title":"B","duration_ms":240000}
]
```
`position` is assigned by array index (0-based). The previous entries are
deleted atomically before the new ones are inserted (transaction). Returns
`"ok"` or `"error:<message>"`.

---

## KV store

Process-global library DB must be initialised via `wdmm_library_init` first.

### `extern "C" fn wdmm_kv_get(key: *const c_char) -> FfiString`
Get a value from the KV store. Returns the stored string, `"null"` if the key
is absent, or `"error:<message>"` on failure.

### `extern "C" fn wdmm_kv_set(key: *const c_char, value: *const c_char) -> FfiString`
Insert or replace a KV entry. Returns `"ok"` or `"error:<message>"`.

### `extern "C" fn wdmm_kv_delete(key: *const c_char) -> FfiString`
Delete a KV entry. Returns `"ok"` or `"error:<message>"`.

---

## Settings

Portable settings backed by the KV store. `network_last_path` is **session-only**
(in-process `Mutex<String>`, never written to the KV store) — it resets to `"/"`
on every process start.

### `extern "C" fn wdmm_settings_get_network_remember_last_path() -> FfiString`
Returns `"true"`, `"false"` (default if never set), or `"error:<message>"`.

### `extern "C" fn wdmm_settings_set_network_remember_last_path(enabled: i32) -> FfiString`
Non-zero → `"true"`, `0` → `"false"`. Also strips the legacy key `network_last_path`
from the KV store. Returns `"ok"` or `"error:<message>"`.

### `extern "C" fn wdmm_settings_get_network_last_path() -> FfiString`
Returns the current session-only path, or `"/"` if never set this session.
Never reads from the KV store.

### `extern "C" fn wdmm_settings_set_network_last_path(path: *const c_char) -> FfiString`
Set the session-only path (in-process only; never persisted). Returns `"ok"`.

---

## Manga (schema v4)

Manga storage added in Wave D. Schema: `mangas`, `manga_pages`, `manga_progress`,
`deleted_mangas` tables. The `manga_series` / `manga_chapters` stub tables from
earlier schemas are dropped by the v4 migration.

### Manga object shape
```json
{
  "id":             "string (UUID or app-chosen)",
  "source_name":    "string",
  "remote_path":    "string",
  "title":          "string",
  "author":         "string|null",
  "series":         "string|null",
  "volume":         "string|null",
  "chapter_number": "string|null",
  "page_count":     0,
  "cover_relpath":  "string|null",
  "format":         "cbz|pdf|images|unknown",
  "remote_missing": 0,
  "page_turn":      "ltr|rtl|null",
  "rev":            0,
  "updated_at":     "RFC-3339 string"
}
```

**Partial unique index:** `(source_name, remote_path)` must be unique when
`format <> 'images'`. Multiple entries with `format = 'images'` at the same path
are allowed (loose-image folders that may be distinct volumes/chapters).

### Page object shape
```json
{"manga_id": "string", "page_index": 0, "rel_path": "string"}
```

### Progress object shape
```json
{"manga_id": "string", "page_index": 0, "updated_at": "RFC-3339 string"}
```

### Series id formula
```
sha1( utf8("mangaSeries") || utf8(title) )   encoded as lower-case hex
```
This matches the client-side `mangaSeriesId` helper. The formula is implemented
in `wdmm_storage::series_id_for_title`.

### `extern "C" fn wdmm_manga_list() -> FfiString`
Return all mangas as a JSON array, ordered by `updated_at DESC`.
Returns `"error:…"` on failure.

### `extern "C" fn wdmm_manga_get(id: *const c_char) -> FfiString`
Get a single manga by id. Returns the manga as JSON, `"null"` if not found, or
`"error:…"` on failure.

### `extern "C" fn wdmm_manga_upsert(json: *const c_char) -> FfiString`
Insert or replace a manga. `json` must be a UTF-8 manga JSON object (all fields
required; use `null` for optional fields). Returns `"ok"` or `"error:…"`.

### `extern "C" fn wdmm_manga_delete(id: *const c_char) -> FfiString`
Delete a manga. Cascades to `manga_pages` and `manga_progress`. Appends a row to
`deleted_mangas` with `sync_key = "{source_name}:{remote_path}"` and `pushed = 0`.
Returns `"ok"`, `"not_found"`, or `"error:…"`.

### `extern "C" fn wdmm_manga_list_pages(manga_id: *const c_char) -> FfiString`
Return all pages for a manga as a JSON array ordered by `page_index`.
Returns `"error:…"` on failure.

### `extern "C" fn wdmm_manga_set_pages(manga_id: *const c_char, json_array: *const c_char) -> FfiString`
Atomically replace all pages for a manga. `json_array` is a JSON array of page
objects. Existing pages are deleted before the new ones are inserted.
Returns `"ok"` or `"error:…"`.

### `extern "C" fn wdmm_manga_get_progress(manga_id: *const c_char) -> FfiString`
Get reading progress for a manga. Returns the progress JSON object, `"null"` if no
progress recorded, or `"error:…"`.

### `extern "C" fn wdmm_manga_set_progress(manga_id: *const c_char, page_index: i64) -> FfiString`
Upsert reading progress (0-based page index). Returns `"ok"` or `"error:…"`.

### `extern "C" fn wdmm_manga_list_progress() -> FfiString`
Return reading progress for all mangas as a JSON array ordered by `updated_at DESC`.
Returns `"error:…"` on failure.

### `extern "C" fn wdmm_manga_list_series() -> FfiString`
Return distinct non-null series tags as a JSON array of `{"id","title"}` objects,
ordered alphabetically (case-insensitive). The `id` field is derived via the
series id formula above. Returns `"error:…"` on failure.

---

## Queue concurrency (Wave D)

The default concurrency is **1** (serial FIFO, matching the original Dart behaviour).
Set it higher to allow `ConcurrentPump` to run multiple tasks in parallel.

### `extern "C" fn wdmm_queue_get_concurrency() -> i32`
Return the current concurrency limit (always in `[1, 8]`).

### `extern "C" fn wdmm_queue_set_concurrency(n: i32) -> FfiString`
Set the concurrency limit. `n` is clamped to `1..=8`.
Returns `"ok"`. Caller must free the result.



---

## Cloud accounts

Accounts live in the library SQLite `accounts` table (`CloudAccount`).
Credentials are opaque bytes (`credentials_encrypted`); FFI exposes them as
`credentials_b64`. A `credentials` JSON object may be supplied on upsert:
with a non-empty `passphrase` it is encrypted via `CredentialEncoder`; otherwise
the map is stored as UTF-8 JSON bytes (Desktop interim until vault UX lands).

### `extern "C" fn wdmm_account_list() -> FfiString`
JSON array of accounts (each with `credentials_b64`), or `"error:…"`.

### `extern "C" fn wdmm_account_get(id: *const c_char) -> FfiString`
One account JSON, `"null"`, or `"error:…"`.

### `extern "C" fn wdmm_account_upsert(json: *const c_char) -> FfiString`
Insert-or-replace. Returns the account JSON (with `credentials_b64`) or `"error:…"`.

### `extern "C" fn wdmm_account_delete(id: *const c_char) -> FfiString`
`"ok"` or `"error:…"`.

### Sync note
`wdmm_sync_trigger` uses the process-global library DB (same as account/playlist/manga).
