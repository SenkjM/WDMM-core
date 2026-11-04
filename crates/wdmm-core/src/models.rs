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


// ── Transcode presets (B3.5, schema v13) ─────────────────────────────────────

/// Hardware (MediaCodec) video encoders a preset may select in
/// [`TranscodePreset::hw_video_encoder`].
pub const HW_VIDEO_ENCODERS: [&str; 3] = ["h264_mediacodec", "hevc_mediacodec", "mpeg4_mediacodec"];

/// Software video encoders a preset may select in
/// [`TranscodePreset::sw_video_encoder`].
///
/// There is deliberately no `libx265`: software HEVC encoding is not shipped.
pub const SW_VIDEO_ENCODERS: [&str; 3] = ["libx264", "mpeg2video", "mpeg4"];

/// Speed/quality tiers a preset may select in `hw_video_tier`/`sw_video_tier`.
pub const VIDEO_TIERS: [&str; 3] = ["fast", "balanced", "quality"];

/// Output audio codecs a preset may select in [`TranscodePreset::audio_codec`].
pub const AUDIO_CODECS: [&str; 4] = ["aac", "ac3", "eac3", "mp2"];

/// A DLNA cast transcode preset. Canonical type shared by wdmm-storage,
/// wdmm-ffmpeg and the FFI layer. See `wdmm-storage` schema v13 for the DB
/// layout.
///
/// The video side is decided in three layers, in this order:
///
/// 1. **copy** — [`video_copy`](Self::video_copy): pass the source stream
///    through when MPEG-TS can carry it (no re-encode at all);
/// 2. **hardware encode** — [`hw_video_encoder`](Self::hw_video_encoder)
///    (empty = disabled) with the [`hw_video_tier`](Self::hw_video_tier) knobs;
/// 3. **software encode** — [`sw_video_encoder`](Self::sw_video_encoder)
///    (empty = disabled), the fallback when hardware is missing or fails.
///
/// Audio has its own two-state copy/encode switch ([`audio_copy`](Self::audio_copy)).
/// HLS output is always MPEG-TS fragments — pass-through is a session-level
/// `forceDirect` decision, not a preset field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscodePreset {
    /// Stable identifier (e.g. `"h264"`, `"hevc"`).
    pub id: String,
    /// Human-readable label shown in the UI.
    pub label: String,
    // ── ① copy layer (highest priority) ─────────────────────────────────────
    /// Allow the video stream to be copied when the source codec is MPEG-TS
    /// compatible.
    pub video_copy: bool,
    /// Allow the audio stream to be copied when the source codec is MPEG-TS
    /// compatible.
    pub audio_copy: bool,
    // ── ② hardware encoder layer ("" = disabled) ────────────────────────────
    /// MediaCodec encoder name, one of [`HW_VIDEO_ENCODERS`] or `""`.
    pub hw_video_encoder: String,
    /// Speed/quality tier for the hardware encoder, one of [`VIDEO_TIERS`].
    pub hw_video_tier: String,
    // ── ③ software encoder fallback ("" = disabled) ─────────────────────────
    /// Software encoder name, one of [`SW_VIDEO_ENCODERS`] or `""`.
    pub sw_video_encoder: String,
    /// Speed/quality tier for the software encoder, one of [`VIDEO_TIERS`].
    pub sw_video_tier: String,
    // ── output envelope (independent of the layer) ──────────────────────────
    /// Max output width px (scale-down only). `None` = no limit.
    pub video_max_w: Option<u32>,
    /// Max output height px. `None` = no limit.
    pub video_max_h: Option<u32>,
    /// Max video bitrate bits/s. `None` = derive from the envelope.
    pub video_max_bitrate: Option<u32>,
    /// Frame-rate cap (e.g. 30). `None` = no cap.
    pub video_fps_cap: Option<u32>,
    // ── audio ───────────────────────────────────────────────────────────────
    /// Audio codec used when [`audio_copy`](Self::audio_copy) is off: one of
    /// [`AUDIO_CODECS`].
    pub audio_codec: String,
    /// Audio bitrate bits/s (e.g. `128_000`).
    pub audio_bitrate: u32,
    /// Output channel count (1 = mono, 2 = stereo).
    pub audio_channels: u8,
    /// Output sample rate Hz. `None` = keep source.
    pub audio_sample_rate: Option<u32>,
    // ── session ─────────────────────────────────────────────────────────────
    /// HLS segment duration in seconds.
    pub hls_segment_secs: u32,
    /// UI sort order (lower = earlier).
    pub sort_order: i64,
    /// `true` for the factory presets (`h264`, `hevc`).
    pub is_builtin: bool,
    /// Last edit time, seconds since the Unix epoch.
    pub updated_at: i64,
}

impl TranscodePreset {
    /// Check the enum-like string fields against the contract's allowed
    /// literals and require at least one usable video path unless
    /// [`video_copy`](Self::video_copy) is enabled.
    ///
    /// Returns the user-facing error message (the FFI/storage layers prefix
    /// it with `error:`), e.g. `unknown encoder: libx265`.
    pub fn validate(&self) -> Result<(), String> {
        if !self.hw_video_encoder.is_empty()
            && !HW_VIDEO_ENCODERS.contains(&self.hw_video_encoder.as_str())
        {
            return Err(format!("unknown encoder: {}", self.hw_video_encoder));
        }
        if !self.sw_video_encoder.is_empty()
            && !SW_VIDEO_ENCODERS.contains(&self.sw_video_encoder.as_str())
        {
            return Err(format!("unknown encoder: {}", self.sw_video_encoder));
        }
        if !VIDEO_TIERS.contains(&self.hw_video_tier.as_str()) {
            return Err(format!("unknown tier: {}", self.hw_video_tier));
        }
        if !VIDEO_TIERS.contains(&self.sw_video_tier.as_str()) {
            return Err(format!("unknown tier: {}", self.sw_video_tier));
        }
        if !self.has_usable_video_path() {
            return Err("no usable video path".to_string());
        }
        Ok(())
    }

    /// `true` when at least one of the three video layers can produce output:
    /// copy is allowed, or a hardware/software encoder is configured.
    pub fn has_usable_video_path(&self) -> bool {
        self.video_copy || !self.hw_video_encoder.is_empty() || !self.sw_video_encoder.is_empty()
    }
}
