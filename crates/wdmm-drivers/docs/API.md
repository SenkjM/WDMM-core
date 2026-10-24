# wdmm-drivers — Public API

The `CloudDriver` trait and per-provider implementations, plus a JSON-config
registry. No driver embeds credentials in source; all secrets are user-supplied.

---

## Error types (`wdmm_drivers::driver`)

### `enum DriverError`
`Network(String)`, `NotFound(String)`, `AuthFailed`, `UnsupportedProvider(String)`,
`Config(String)`, `Other(String)`.

### `type Result<T> = std::result::Result<T, DriverError>`

---

## Core trait (`wdmm_drivers::driver`)

### `enum ContentDeliveryKind`

How bytes are obtained for a `CloudFileItem`. `#[non_exhaustive]` — match with a
`_` catch-all.

| Variant | Meaning |
|---------|---------|
| `NetworkDirect` | A network URL is available (`raw_url` + optional `raw_headers`). media_kit can consume directly if the caller can pass custom headers; otherwise route through proxy. |
| `LocalStream` | No URL and no local path — bytes come from an in-process Range/byte stream only (e.g. a crypt-wrapper driver). Callers must bridge through `ProxyHandle::register_driver` (Rust) or `wdmm_proxy_register_stream` (FFI/Dart). |
| `LocalFile` | A local filesystem path (`local_path`) is directly readable. |

```rust
impl ContentDeliveryKind {
    /// Infer from presence of raw_url / local_path. Prefer explicit construction.
    pub fn infer(raw_url: Option<&str>, local_path: Option<&Path>) -> Self;
}
```

### `struct CloudFileItem`
A cloud file or directory.

Fields:
- `name: String`
- `is_dir: bool`
- `size: u64`
- `modified: Option<DateTime<Utc>>`
- `raw_url: Option<String>` — direct download URL; present when `delivery == NetworkDirect`.
- `raw_headers: Vec<(String, String)>` — custom headers for `raw_url`; empty otherwise.
- `delivery: ContentDeliveryKind` — how bytes are obtained; callers use this to decide routing.
- `local_path: Option<PathBuf>` — filesystem path; present when `delivery == LocalFile`.

Constructors (prefer these over struct literal):

```rust
// delivery = NetworkDirect
CloudFileItem::network_direct(name, is_dir, size, modified, raw_url, raw_headers)
// delivery = LocalStream (no URL, no path)
CloudFileItem::local_stream(name, is_dir, size, modified)
// delivery = LocalFile
CloudFileItem::local_file(name, is_dir, size, modified, local_path)
```

### `trait CloudDriver: Send + Sync`
Cloud storage abstraction. Required methods:

- `async fn init(&self) -> Result<()>` — default `Ok(())`.
- `async fn list(&self, path: &str) -> Result<Vec<CloudFileItem>>`
- `async fn get(&self, path: &str) -> Result<CloudFileItem>` — must populate `raw_url`.
- `async fn mkdir(&self, path: &str) -> Result<()>`
- `async fn rename(&self, old_path: &str, new_path: &str) -> Result<()>`
- `async fn remove(&self, path: &str) -> Result<()>`

Optional methods (default `Err(DriverError::Other("... not supported"))`):

- `async fn put_content(&self, path: &str, data: &[u8]) -> Result<()>` —
  push small JSON/metadata; **not** the user-upload path.
- `async fn get_content(&self, path: &str) -> Result<Vec<u8>>` — full raw bytes.
- `async fn get_content_range(&self, path: &str, start: u64, end: Option<u64>) -> Result<Vec<u8>>` —
  inclusive `start`/`end`; default falls back to `get_content` + slice.

---

## Registry

### `struct DriverRegistry`

### `fn DriverRegistry::create_driver(provider_type: &str, config: serde_json::Value) -> Result<Box<dyn CloudDriver>>`
Instantiate a driver from a provider type + config JSON object.

Recognised `provider_type` values and required config fields:

| provider_type                     | struct            | required keys                         | optional keys |
|-----------------------------------|-------------------|---------------------------------------|---------------|
| `webdav`                          | `WebDavDriver`    | `url`                                 | `username`, `password` |
| `ctfile`                          | `CtFileDriver`    | `session`                             | `cloud_type`, `base_url`, `order_by`, `order_direction`, `user_agent`, `list_cache_sec`, `request_interval_ms` |
| `baidu`, `baidu_netdisk`          | `BaiduDriver`     | `access_token`                        | `base_url` |
| `123`, `123_open`, `pan123`       | `Pan123Driver`   | `access_token`                        | `base_url` |
| `aliyundrive`, `aliyun`           | `AliyunDriver`   | `access_token`, `drive_id`           | `base_url` |
| `115`, `open115`                  | `Open115Driver`  | `access_token`                        | `base_url` |
| `terabox`                         | `TeraboxDriver`   | — (stub)                             | — |
| `netease_music`                   | `NeteaseMusicDriver` | — (stub)                          | — |

Errors: `Config` for a missing required field; `UnsupportedProvider` for an
unknown type.

---

## Driver structs & configs

Each driver is constructed via its `Config` + `Driver::new(config)`, or through
the registry above. Stub drivers (`TeraboxDriver`, `NeteaseMusicDriver`) are
unit structs whose every method returns `DriverError::Other("TODO: … not yet implemented")`.

### `struct WebDavDriver`
`pub fn new(url: &str, username: &str, password: &str) -> Result<Self>`
Implements list/get/mkdir/rename/remove/put_content/get_content.

### `struct CtFileDriver` / `struct CtFileConfig`
`pub fn CtFileConfig::with_session(session: &str) -> Self`
`pub fn CtFileDriver::new(cfg: CtFileConfig) -> Result<Self>`
List/get + write ops.

### `struct BaiduDriver` / `struct BaiduConfig`
`pub fn BaiduConfig::new(access_token: impl Into<String>) -> Self`
`pub fn BaiduDriver::new(cfg: BaiduConfig) -> Result<Self>`
List/get (with `raw_url` via dlink + Bearer header) and write ops (Round 10):
- `mkdir` — `POST /rest/2.0/xpan/file?method=create` with `path`, `isdir=1`, `size=0`, `block_list=[]`.
- `rename` — `POST /rest/2.0/xpan/file?method=rename` with `filelist=[{"path": old, "newname": new_name}]`; same-parent rename only.
- `remove` — `POST /rest/2.0/xpan/file?method=delete` with `filelist=["path"]`, `async=0`.
Auth: `access_token` as query param (Baidu convention). Retries once on 429/5xx.

### `struct Pan123Driver` / `struct Pan123Config`
`pub fn Pan123Config::new(access_token: impl Into<String>) -> Self`
`pub fn Pan123Driver::new(cfg: Pan123Config) -> Result<Self>`
List/get (with `raw_url` via `getDownloadUrl`) and write ops (Round 10):
- `mkdir` — `POST /api/v1/upload/mkdir` with `{"name": …, "parentFileId": …}`; resolves parent path to `parentFileId`.
- `rename` — `POST /api/v1/file/rename` with `{"fileId": …, "fileName": …}`; same-parent name change; resolves old path to `fileId`.
- `remove` — `POST /api/v1/file/trash` with `{"fileTrashInfoList": [{"fileId": …}]}`; resolves path to `fileId`.
Auth header: `Bearer <access_token>`. Retries once on 429/5xx.

### `struct AliyunDriver` / `struct AliyunConfig`
`pub fn AliyunConfig::new(access_token: impl Into<String>, drive_id: impl Into<String>) -> Self`
`pub fn AliyunDriver::new(cfg: AliyunConfig) -> Result<Self>`
Requires non-empty `access_token` and `drive_id`.
List/get (with `raw_url` via `getDownloadUrl`)/mkdir/rename/remove.
Auth header: `Bearer <access_token>`. Retries once on 429/5xx.
`rename` supports single-segment renames only (same parent).

### `struct Open115Driver` / `struct Open115Config`
`pub fn Open115Config::new(access_token: impl Into<String>) -> Self`
`pub fn Open115Driver::new(cfg: Open115Config) -> Result<Self>`
List/get (with `raw_url` via `/open/file/downurl`) + write ops (Round 9):
- `mkdir` — `POST /open/folder/create` with `pid` + `cname`; parent must exist.
- `rename` — `POST /open/file/edit` with `fid` + `file_name`; same-parent only.
- `remove` — `POST /open/rbfile/delete` with `fid[]`; moves item to recycle bin.
Auth header: `Bearer <access_token>`. Retries once on 429/5xx.

### `struct TeraboxDriver` / `struct NeteaseMusicDriver`
Unit-struct stubs. All `CloudDriver` methods return `DriverError::Other("TODO: …")`.

---

Callers (media_kit, DLNA, future mounts) decide proxy need from this kind. Enum is `#[non_exhaustive]`. See `/workspace/agent-tools/wdmm-crypt-proxy-path.md` §6.
