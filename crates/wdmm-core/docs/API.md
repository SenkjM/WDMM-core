# wdmm-core — Public API

Pure data models, track identity hashing, and revision clock.
Zero platform dependencies; no `std::fs` or OS-specific I/O.

---

## Error types

### `enum WdmmError`
Top-level error enum re-exported from the crate root.

Variants: `Driver(String)`, `Storage(String)`, `Crypto(String)`,
`Network(String)`, `NotFound(String)`, `Unsupported(String)`,
`InvalidArgument(String)`, `Io(String)`, `Other(String)`.

### `type Result<T> = std::result::Result<T, WdmmError>`

---

## Models (`wdmm_core::models`)

### `struct LibraryTrack`
A music/video file entry. `id` is the SHA-1 hex of the normalised remote path.

Fields: `id`, `title`, `artist?`, `remote_path`, `account_id`, `size: u64`,
`modified?`, `album_id?`, `artist_id?`, `duration_ms: i64`, `genre?`,
`track_number?`, `disc_number?`, `year?`, `mime_type?`.

Derives `Serialize`, `Deserialize`, `Default`.

### `struct LibraryAlbum`
Fields: `id`, `title`, `artist_id?`, `year?`, `art_url?`, `created_at: DateTime<Utc>`.

### `struct LibraryArtist`
Fields: `id`, `name`, `sort_name?`, `created_at: DateTime<Utc>`.

### `struct CloudAccount`
Fields: `id`, `provider_type`, `display_name`, `base_url?`,
`credentials_encrypted: Vec<u8>` (AES-256-GCM via `wdmm-crypto`).

`provider_type` values: `"webdav"`, `"baidu"`, `"aliyundrive"`, `"ctfile"`,
`"123"`, `"open115"`, `"terabox"`, `"netease_music"`.

### `struct UserFolder`
Fields: `id`, `name`, `parent_id?`, `created: DateTime<Utc>`.

### `enum TaskStatus`
`Pending | Active | Completed | Failed | Cancelled | Partial`

### `enum TaskKind`
`DownloadTrack | DownloadPlaylist | UploadFile | Backup |
SyncCredentials | SyncLibrary | SyncPlaylists | SyncManga | Other`

### `struct QueueTask`
Fields: `id`, `kind: TaskKind`, `status: TaskStatus`, `progress: f32`,
`total_bytes: u64`, `transferred_bytes: u64`, `created`, `updated`,
`attempts: u32`, `error?`, `remote_path?`, `local_path?`.

---

## Identity (`wdmm_core::identity`)

### `fn normalize_remote_path(path: &str) -> String`
Lowercase, trim leading/trailing slashes, normalise `\` → `/`.
Cross-language golden vectors must match `LibraryTrack.computeIdentity` in Dart.

### `fn track_identity(remote_path: &str) -> String`
SHA-1 hex of the normalised path.
Errors: none (always returns a 40-char hex string).

---

## Revision clock (`wdmm_core::rev_clock`)

### `struct RevClock(pub u64)`
Monotonically increasing counter; higher value = newer revision.

### `RevClock::ZERO: RevClock`
The zero-value sentinel.

### `fn RevClock::new(v: u64) -> Self`

### `fn RevClock::increment(self) -> Self`
Returns `RevClock(self.0.saturating_add(1))`.

### `fn RevClock::value(self) -> u64`

Derives `PartialOrd`, `Ord`, `Serialize`, `Deserialize`.
