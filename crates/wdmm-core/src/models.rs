//! Core data models — account, track, folder, sync state.
//!
//! Matches schema direction of the Dart `music_library.db` (schema v18).
//! This is the *subset* needed for the Rust core to operate. Full FFI
//! serialisation lives in `wdmm-ffi`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A library track (music/video file).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryTrack {
    /// SHA-1 identity hash of normalised remote path.
    pub id: String,
    /// Display title.
    pub title: String,
    /// Artist name (optional).
    pub artist: Option<String>,
    /// Remote path on the source account (before normalisation).
    pub remote_path: String,
    /// Account ID this track belongs to.
    pub account_id: String,
    /// Size in bytes.
    pub size: u64,
    /// Last modified timestamp.
    pub modified: Option<DateTime<Utc>>,
}

/// A cloud account (WebDAV or driver-backed).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudAccount {
    pub id: String,
    pub provider_type: String, // "webdav" | "baidu" | "aliyundrive" | …
    pub display_name: String,
    /// For WebDAV: base URL (e.g. "https://example.com/dav/")
    pub base_url: Option<String>,
    /// Encrypted credentials (via wdmm-crypto).
    pub credentials_encrypted: Vec<u8>,
}

/// A user folder in the library.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserFolder {
    pub id: String,
    pub name: String,
    pub parent_id: Option<String>,
    pub created: DateTime<Utc>,
}

/// Queue task status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Pending,
    Active,
    Completed,
    Failed,
    Cancelled,
    Partial, // partial failure for batch tasks
}

/// Task kind enumeration — aligns with Dart `TaskKind` (9 types).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    DownloadTrack,
    DownloadPlaylist,
    UploadFile,
    Backup,
    SyncCredentials,
    SyncLibrary,
    SyncPlaylists,
    SyncManga,
    Other,
}

/// A download/upload task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueTask {
    pub id: String,
    pub kind: TaskKind,
    pub status: TaskStatus,
    pub progress: f32,       // 0.0..=1.0
    pub total_bytes: u64,
    pub transferred_bytes: u64,
    pub created: DateTime<Utc>,
    pub updated: DateTime<Utc>,
    pub attempts: u32,       // retry attempts
    pub error: Option<String>, // last error message
    pub remote_path: Option<String>, // source path
    pub local_path: Option<String>,  // destination path
}

