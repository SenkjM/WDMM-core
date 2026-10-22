# wdmm-platform — Public API

Adapter traits for keychain, notifications, filesystem, and wakelock.
Platform implementations live in platform repos (Android: Kotlin JNI, Windows: C#).
`StdPlatformAdapter` provides a no-op `std` implementation for `cargo test`.

---

## Error types

### `enum PlatformError`
`Keychain(String)`, `Notification(String)`, `Filesystem(String)`, `Unsupported`.

### `type Result<T> = std::result::Result<T, PlatformError>`

---

## Traits

### `trait KeychainProvider: Send + Sync`
Secure key-value store. Android: EncryptedSharedPreferences/Keystore. Windows: DPAPI.

- `fn store(&self, key: &str, value: &[u8]) -> Result<()>`
- `fn load(&self, key: &str) -> Result<Option<Vec<u8>>>`
- `fn delete(&self, key: &str) -> Result<()>`

### `trait NotificationSink: Send + Sync`
Post local notifications. Android: NotificationManager via JNI. Windows: WinRT Toast.

- `fn post_download_progress(&self, id: u32, title: &str, progress: f32)`
- `fn post_download_done(&self, id: u32, title: &str)`
- `fn cancel_notification(&self, id: u32)`

### `trait FileSystemAdapter: Send + Sync`
Platform path resolution and downloads. Android: SAF + `getExternalFilesDir()`.
Windows: KnownFolders (Music/Downloads).

- `fn cache_dir(&self) -> PathBuf`
- `fn app_data_dir(&self) -> PathBuf`
- `fn save_to_downloads(&self, src: &Path, name: &str) -> Result<String>`
  Save to platform downloads dir. Returns final path or content:// URI.
- `fn align_nomedia(&self, enabled: bool) -> Result<()>`
  Place or remove `.nomedia` in the cache directory.

### `trait BackgroundTaskHandle: Send + Sync`
Prevent device sleep during long operations. Android: PowerManager wakelock.

- `fn acquire_wakelock(&self)`
- `fn release_wakelock(&self)`

---

## Default std adapter

### `struct StdPlatformAdapter`
No-op implementations of all four traits. Uses `std::env::temp_dir()` as base.
Implements `KeychainProvider`, `NotificationSink`, `FileSystemAdapter`,
`BackgroundTaskHandle`.

### `fn StdPlatformAdapter::new() -> Self`
