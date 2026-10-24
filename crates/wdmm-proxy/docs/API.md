# wdmm-proxy — Public API

Unified local HTTP range proxy. Serves registered streams over HTTP with
`Range` support, addressed by an unguessable token. Two back-source modes
(local file and driver Range callback), aligned with the Dart
`LocalStreamBridge` (`lib/services/cloud_drivers/stream_bridge.dart`).

## Design rules (brief §3)

- **Token-only register API**: `register_*` returns `Registration { token,
  path_template }`. Callers compose `http://{bind_host}:{port}{path_template}`
  themselves. The proxy **never** returns a full URL with scheme/host/port.
- **No OpenAPI coupling**: the proxy knows nothing about OpenAPI. OpenAPI
  (when enabled) may call the proxy to obtain a token for `mode=proxy`, or
  return a signed direct upstream URL for `mode=direct`.
- **HTTPS / custom certs**: out of scope.
- **Idle TTL**: default 10 min; every request resets the timer; the server
  auto-restarts on the next `register_*`.
- **Bind**: loopback default; DLNA passes a LAN address.

---

## Error types

### `enum ProxyError`
`Io(#[from] std::io::Error)`.

### `type ProxyResult<T> = std::result::Result<T, ProxyError>`

---

## Types

### `type BoxStream`
`Pin<Box<dyn Stream<Item = io::Result<Bytes>> + Send>>` — the byte stream
returned by a driver Range callback.

### `struct Registration`
Result of `register_*`. Callers build the URL as
`http://{bind_host}:{port}{path_template}`.
Fields:
- `token: String` — 32-hex, 128-bit random token.
- `path_template: String` — URL path, e.g. `"/s/3f7a…"`.

---

## Proxy handle (`wdmm_proxy::ProxyHandle`)

Owns a lazily-started axum server. Starts on the first `register_*`; shuts down
after the idle timeout of inactivity; restarts on the next `register_*`.
Dropping the handle stops the server.

### `fn ProxyHandle::new(bind: SocketAddr) -> Self`
Create a handle bound to `bind` with the default 10-minute idle timeout.
The server is **not** started yet.

### `fn ProxyHandle::with_idle_timeout(bind: SocketAddr, idle_timeout: Duration) -> Self`
Create a handle with a custom idle timeout.

### `fn ProxyHandle::addr(&self) -> Option<SocketAddr>`
The bound address, or `None` when the server is not running. Use this to read
the OS-assigned port when `bind` used port 0.

### `async fn ProxyHandle::register_file(&self, path: impl Into<PathBuf>, name: Option<String>) -> ProxyResult<Registration>`
Register a local file. `name` is used only for Content-Type guessing
(e.g. `"song.mp3"`); it is decorative and carries no auth. Returns a
`Registration`; the caller composes the URL.

### `async fn ProxyHandle::register_driver<F>(&self, size: u64, name: Option<String>, ranges_fn: F) -> ProxyResult<Registration>`
where `F: Fn(i64, i64) -> BoxStream + Send + Sync + 'static`.

Register a driver Range callback back-source.
- `size` — known file size in bytes; pass `0` if unknown (streamed to EOF).
- `ranges_fn(start, end)` — `start` and `end` are **inclusive** byte offsets.
  `end = -1` means read to EOF (size unknown).

### `fn ProxyHandle::unregister(&self, token: &str)`
Revoke a token immediately.

### `fn ProxyHandle::renew_callback<F>(&self, token: &str, ranges_fn: F)`
where `F: Fn(i64, i64) -> BoxStream + Send + Sync + 'static`.

Replace the driver callback for an existing token (e.g. after re-auth).
No-op if the token is unknown or its source is a local file.

### `fn ProxyHandle::stop(&self)`
Gracefully stop the server. The handle is still usable; the server restarts
on the next `register_*`. Also triggered on `Drop`.

---

## Free functions

### `fn resolve_range(header: &str, size: u64) -> Option<(u64, u64)>`
Parse a single-range `Range` header against `size` bytes. Returns `None` when
unsatisfiable (→ 416). Supports `a-b`, `a-` (open-ended), `-n` (suffix).
Multi-range → unsatisfiable.

### `fn content_type_for(name: Option<&str>) -> &'static str`
Guess a MIME type from a filename extension. Table matches Dart
`LocalStreamBridge._contentTypeFor` (mp4/mkv/webm/avi/mov/ts/mp3/flac/m4a/aac/
wav/ogg/opus; fallback `application/octet-stream`).

---

## HLS session API

### `async fn ProxyHandle::register_hls(&self, input: impl Into<PathBuf>, mode: wdmm_ffmpeg::HlsMode) -> ProxyResult<HlsRegistration>`
Register an HLS session for `input`.

Calls `wdmm_ffmpeg::open_job` to create the segment directory (and, once
vendored binaries are wired, spawn the transcode process). Use
`HlsMode::DirectCopy` (default) for codec-compatible media and
`HlsMode::Compat` for transcoding.

For the 「单文件直通」 compat option, use `register_file` with byte Range instead.

Returns `HlsRegistration`; callers compose:
```text
http://{bind_host}:{port}{playlist_template}
```

### `async fn ProxyHandle::seek_hls(&self, token: &str, offset_secs: f64) -> ProxyResult<()>`
Seek an active HLS session to `offset_secs`.

Kills the current transcode job and restarts it (`-ss offset_secs`). The old
segment directory is removed when the old `JobHandle` drops. No-op if the
token is unknown.

### `fn ProxyHandle::unregister_hls(&self, token: &str)`
Revoke an HLS session token. The transcode job is killed and the segment
directory is cleaned up.

---

### `struct HlsRegistration`
Result of `register_hls`.

Fields:
- `token: String` — 32-hex, 128-bit random token.
- `playlist_template: String` — URL path to the master playlist,
  e.g. `"/h/3f7a…/index.m3u8"`.

---

## HLS routes

The proxy adds the following routes automatically when the server starts:

| Method | Route | Description |
|--------|-------|-------------|
| GET | `/h/:token/index.m3u8` | Master HLS playlist (served from segment dir) |
| GET | `/h/:token/:seg.ts` | HLS segment file |

Content-Type mapping:
- `*.m3u8` → `application/vnd.apple.mpegurl`
- `*.ts` → `video/mp2t`

Path traversal (`..`, `/` in segment name) → `400 Bad Request`.
Unknown token → `404 Not Found`.

---

## URL composition

Callers (DLNA, OpenAPI `mode=proxy`, platform players) compose the URL
themselves:

```rust
let reg = proxy.register_file(&path, Some("track.mp3".into())).await?;
let addr = proxy.addr().unwrap(); // port was 0
let url = format!("http://{addr}{}", reg.path_template);
// → http://127.0.0.1:43221/s/3f7a…
```

For DLNA (LAN bind), use the LAN-facing host:

```rust
let url = format!("http://192.168.1.10:{}{}", addr.port(), reg.path_template);
```

The token is the only access control on a LAN bind; there is no separate auth
layer (matching the constraint that proxy and OpenAPI are independent).
