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
    // ── v9 field ──────────────────────────────────────────────────────────────
    /// Revision (for library sync merge).
    #[serde(default)]
    pub rev: i64,
    // ── v10 field ─────────────────────────────────────────────────────────────
    /// Cover art availability: 0=Unknown, 1=None, 2=Present.
    #[serde(default)]
    pub cover_state: i64,
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


// ── Transcode presets (B3.5) ──────────────────────────────────────────────────

/// A DLNA cast transcode preset. Canonical type shared by wdmm-storage and
/// wdmm-ffmpeg. See `wdmm-storage` schema v11 for the DB layout.
///
/// `video_codec = None`      → audio-only (no video stream).
/// `container  = "original"` → pass-through without re-muxing.
/// `audio_codec = "copy"`    → copy audio unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscodePreset {
    /// Stable identifier (e.g. `"dlna-compat"`, `"passthrough"`).
    pub id: String,
    /// Human-readable label shown in the UI.
    pub label: String,
    /// Output container: `"mpegts"`, `"mp4"`, or `"original"`.
    pub container: String,
    /// Video codec: `"h264"` → tries `h264_mediacodec`, then `libx264`.
    /// `None` = audio-only.
    pub video_codec: Option<String>,
    /// Prefer hardware encoder (`h264_mediacodec`) before `libx264`.
    pub video_hw_preferred: bool,
    /// Max output width px (scale-down only). `None` = no limit.
    pub video_max_w: Option<u32>,
    /// Max output height px. `None` = no limit.
    pub video_max_h: Option<u32>,
    /// Max video bitrate bits/s. `None` = no limit.
    pub video_max_bitrate: Option<u32>,
    /// Frame-rate cap (e.g. 30). `None` = no cap.
    pub video_fps_cap: Option<u32>,
    /// Audio codec: `"aac"`, `"mp3"`, or `"copy"`.
    pub audio_codec: String,
    /// Audio bitrate bits/s (e.g. `128_000`).
    pub audio_bitrate: u32,
    /// Output channel count (1 = mono, 2 = stereo).
    pub audio_channels: u8,
    /// Output sample rate Hz. `None` = keep source.
    pub audio_sample_rate: Option<u32>,
    /// HLS segment duration in seconds.
    pub hls_segment_secs: u32,
    /// Try remux (copy streams) before falling back to transcode.
    pub remux_first: bool,
    /// UI sort order (lower = earlier).
    pub sort_order: u32,
    /// `true` for the 5 factory presets.
    pub is_builtin: bool,
}
