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
