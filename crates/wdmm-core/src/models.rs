//! Core data models — account, track, folder, sync state.
//!
//! Matches schema direction of the Dart `music_library.db` (schema v18).
//! This is the *subset* needed for the Rust core to operate. Full FFI
//! serialisation lives in `wdmm-ffi`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A library track (music/video file). Schema v2: extended with library fields.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LibraryTrack {
    /// SHA-1 identity hash of normalised remote path.
    pub id: String,
    /// Display title.
    pub title: String,
    /// Primary artist name (optional; denormalised for display).
    pub artist: Option<String>,
    /// Remote path on the source account (before normalisation).
    pub remote_path: String,
    /// Account ID this track belongs to.
    pub account_id: String,
    /// Size in bytes.
    pub size: u64,
    /// Last modified timestamp.
    pub modified: Option<DateTime<Utc>>,
    // ── v2 fields ─────────────────────────────────────────────────────────────
    /// FK to albums.id (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_id: Option<String>,
    /// FK to artists.id (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist_id: Option<String>,
    // ── v7 denormalized text fields ───────────────────────────────────────────
    /// Album title (denormalized from metadata for aggregation).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    /// Album artist name (denormalized from metadata for aggregation).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_artist: Option<String>,
    /// Duration in milliseconds (0 = unknown).
    #[serde(default)]
    pub duration_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub genre: Option<String>,
    /// 1-based track number within the album.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_number: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disc_number: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    /// MIME type (e.g. "audio/mpeg").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// An album in the music library. Schema v2.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LibraryAlbum {
    pub id: String,
    pub title: String,
    /// FK to artists.id (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    /// Remote URL to album art (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub art_url: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// An artist in the music library. Schema v2.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LibraryArtist {
    pub id: String,
    pub name: String,
    /// Sort name (e.g. "Beatles, The").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_name: Option<String>,
    pub created_at: DateTime<Utc>,
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

