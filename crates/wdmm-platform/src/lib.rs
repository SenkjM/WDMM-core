//! `wdmm-platform` — adapter traits for keychain / notification / FS / wakelock.
//!
//! Implementations live in platform repos (Android: Kotlin JNI, Windows: C#).
//! This crate provides a `std` default impl for `cargo test`.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PlatformError {
    #[error("keychain operation failed: {0}")]
    Keychain(String),

    #[error("notification failed: {0}")]
    Notification(String),

    #[error("filesystem operation failed: {0}")]
    Filesystem(String),

    #[error("unsupported operation")]
    Unsupported,
}

pub type Result<T> = std::result::Result<T, PlatformError>;

// ── Keychain Provider ────────────────────────────────────────────────────────

/// Secure key-value store. Android: EncryptedSharedPreferences / Keystore.
/// Windows: DPAPI.
pub trait KeychainProvider: Send + Sync {
    fn store(&self, key: &str, value: &[u8]) -> Result<()>;
    fn load(&self, key: &str) -> Result<Option<Vec<u8>>>;
    fn delete(&self, key: &str) -> Result<()>;
}

// ── Notification Sink ────────────────────────────────────────────────────────

/// Post local notifications. Android: NotificationManager via JNI.
/// Windows: WinRT ToastNotification.
pub trait NotificationSink: Send + Sync {
    fn post_download_progress(&self, id: u32, title: &str, progress: f32);
    fn post_download_done(&self, id: u32, title: &str);
    fn cancel_notification(&self, id: u32);
}

// ── Filesystem Adapter ───────────────────────────────────────────────────────

/// Platform path resolution. Android: `getExternalFilesDir()` via JNI + SAF.
/// Windows: `KnownFolders` (Music/Downloads).
pub trait FileSystemAdapter: Send + Sync {
    fn cache_dir(&self) -> PathBuf;
    fn app_data_dir(&self) -> PathBuf;

    /// Save to platform downloads directory (Android: SAF, Windows: Downloads).
    /// Returns the final path or content:// URI.
    fn save_to_downloads(&self, src: &Path, name: &str) -> Result<String>;

    /// Align `.nomedia` file presence based on the enabled flag.
    fn align_nomedia(&self, enabled: bool) -> Result<()>;
}

// ── Wakelock / Background Task ───────────────────────────────────────────────

/// Prevent device sleep during long operations. Android: PowerManager wakelock.
pub trait BackgroundTaskHandle: Send + Sync {
    fn acquire_wakelock(&self);
    fn release_wakelock(&self);
}

// ── Default std implementation for cargo test ────────────────────────────────

pub struct StdPlatformAdapter {
    cache: PathBuf,
    data: PathBuf,
}

impl StdPlatformAdapter {
    pub fn new() -> Self {
        let tmp = std::env::temp_dir();
        Self {
            cache: tmp.join("wdmm-cache"),
            data: tmp.join("wdmm-data"),
        }
    }
}

impl Default for StdPlatformAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl KeychainProvider for StdPlatformAdapter {
    fn store(&self, _key: &str, _value: &[u8]) -> Result<()> {
        Ok(())
    }
    fn load(&self, _key: &str) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
    fn delete(&self, _key: &str) -> Result<()> {
        Ok(())
    }
}

impl NotificationSink for StdPlatformAdapter {
    fn post_download_progress(&self, _id: u32, _title: &str, _progress: f32) {}
    fn post_download_done(&self, _id: u32, _title: &str) {}
    fn cancel_notification(&self, _id: u32) {}
}

impl FileSystemAdapter for StdPlatformAdapter {
    fn cache_dir(&self) -> PathBuf {
        self.cache.clone()
    }
    fn app_data_dir(&self) -> PathBuf {
        self.data.clone()
    }
    fn save_to_downloads(&self, _src: &Path, name: &str) -> Result<String> {
        let dest = self.data.join(name);
        Ok(dest.display().to_string())
    }
    fn align_nomedia(&self, _enabled: bool) -> Result<()> {
        Ok(())
    }
}

impl BackgroundTaskHandle for StdPlatformAdapter {
    fn acquire_wakelock(&self) {}
    fn release_wakelock(&self) {}
}
