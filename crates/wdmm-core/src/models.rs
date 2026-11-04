//! Core data models — account, track, folder, sync state.
//!
//! Matches schema direction of the Dart `music_library.db` (schema v18).
//! This is the *subset* needed for the Rust core to operate. Full FFI
//! serialisation lives in `wdmm-ffi`.

use std::collections::BTreeMap;

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

// ── Transcode presets (B3.5, schema v14) ─────────────────────────────────────

/// Hardware (MediaCodec) video encoders a preset may select in
/// [`TranscodePreset::hw_video_encoder`].
pub const HW_VIDEO_ENCODERS: [&str; 3] = ["h264_mediacodec", "hevc_mediacodec", "mpeg4_mediacodec"];

/// Software video encoders a preset may select in
/// [`TranscodePreset::sw_video_encoder`].
///
/// There is deliberately no `libx265`: software HEVC encoding is not shipped.
pub const SW_VIDEO_ENCODERS: [&str; 3] = ["libx264", "mpeg2video", "mpeg4"];

/// libx264 `preset` names a preset may select in
/// [`TranscodePreset::sw_video_preset`].
///
/// Only `libx264` has such a name ladder; `mpeg2video`/`mpeg4` leave the field
/// empty and are tuned through [`TranscodePreset::sw_video_options`] instead.
pub const X264_PRESETS: [&str; 10] = [
    "ultrafast",
    "superfast",
    "veryfast",
    "faster",
    "fast",
    "medium",
    "slow",
    "slower",
    "veryslow",
    "placebo",
];

/// Video rate-control modes a preset may select in
/// [`TranscodePreset::video_rate_control`].
///
/// `auto` leaves the encoder defaults alone, `vbr`/`cbr` map onto the encoder's
/// bitrate mode, and `cq` additionally requires
/// [`TranscodePreset::video_quality`].
pub const RATE_CONTROL_MODES: [&str; 4] = ["auto", "vbr", "cbr", "cq"];

/// Default for [`TranscodePreset::video_rate_control`] when a client omits it.
pub const DEFAULT_VIDEO_RATE_CONTROL: &str = "vbr";

/// Output audio codecs a preset may select in [`TranscodePreset::audio_codec`].
pub const AUDIO_CODECS: [&str; 4] = ["aac", "ac3", "eac3", "mp2"];

/// A DLNA cast transcode preset. Canonical type shared by wdmm-storage,
/// wdmm-ffmpeg and the FFI layer. See `wdmm-storage` schema v14 for the DB
/// layout.
///
/// The video side is decided in three layers, in this order:
///
/// 1. **copy** — [`video_copy`](Self::video_copy): pass the source stream
///    through when MPEG-TS can carry it (no re-encode at all);
/// 2. **hardware encode** — [`hw_video_encoder`](Self::hw_video_encoder)
///    (empty = disabled), tuned by [`video_max_bitrate`](Self::video_max_bitrate),
///    [`video_rate_control`](Self::video_rate_control) and the advanced
///    [`hw_video_options`](Self::hw_video_options);
/// 3. **software encode** — [`sw_video_encoder`](Self::sw_video_encoder)
///    (empty = disabled), the fallback when hardware is missing or fails, tuned
///    by [`sw_video_preset`](Self::sw_video_preset), the same bitrate/rate
///    control knobs and [`sw_video_options`](Self::sw_video_options).
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
    /// Advanced `AVDictionary` options handed to the hardware encoder's
    /// `avcodec_open2`, serialised as a JSON object. Whitelisted by
    /// `wdmm-ffmpeg`; empty = none.
    #[serde(default)]
    pub hw_video_options: BTreeMap<String, String>,
    // ── ③ software encoder fallback ("" = disabled) ─────────────────────────
    /// Software encoder name, one of [`SW_VIDEO_ENCODERS`] or `""`.
    pub sw_video_encoder: String,
    /// libx264 `preset` name, one of [`X264_PRESETS`] or `""` when the selected
    /// software encoder has no preset ladder (`mpeg2video`/`mpeg4`).
    #[serde(default)]
    pub sw_video_preset: String,
    /// Advanced `AVDictionary` options handed to the software encoder's
    /// `avcodec_open2`, serialised as a JSON object. Whitelisted by
    /// `wdmm-ffmpeg`; empty = none.
    #[serde(default)]
    pub sw_video_options: BTreeMap<String, String>,
    // ── output envelope (independent of the layer) ──────────────────────────
    /// Max output width px (scale-down only). `None` = no limit.
    pub video_max_w: Option<u32>,
    /// Max output height px. `None` = no limit.
    pub video_max_h: Option<u32>,
    /// Max video bitrate bits/s. `None` = derive from the envelope.
    pub video_max_bitrate: Option<u32>,
    /// Frame-rate cap (e.g. 30). `None` = no cap.
    pub video_fps_cap: Option<u32>,
    /// Video rate-control mode, one of [`RATE_CONTROL_MODES`]. Defaults to
    /// [`DEFAULT_VIDEO_RATE_CONTROL`] when an older client omits it.
    #[serde(default = "default_video_rate_control")]
    pub video_rate_control: String,
    /// Constant-quality value used when
    /// [`video_rate_control`](Self::video_rate_control) is `"cq"` (hardware
    /// `global_quality` 0–100, libx264 CRF 0–51). `None` otherwise.
    #[serde(default)]
    pub video_quality: Option<u8>,
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
    ///
    /// Assigned by the storage layer on every save, so callers may leave it out
    /// of the JSON they hand to `wdmm_transcode_preset_save`: a client has no
    /// business inventing this timestamp, and requiring it made the preset
    /// editor fail with `error:parse:missing field updated_at`.
    #[serde(default)]
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
        if !self.sw_video_preset.is_empty()
            && !X264_PRESETS.contains(&self.sw_video_preset.as_str())
        {
            return Err(format!("unknown encoder preset: {}", self.sw_video_preset));
        }
        if !RATE_CONTROL_MODES.contains(&self.video_rate_control.as_str()) {
            return Err(format!("unknown rate control: {}", self.video_rate_control));
        }
        if self.video_rate_control == "cq" && self.video_quality.is_none() {
            return Err("cq rate control needs a quality value".to_string());
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

/// `serde` default for [`TranscodePreset::video_rate_control`].
fn default_video_rate_control() -> String {
    DEFAULT_VIDEO_RATE_CONTROL.to_string()
}

#[cfg(test)]
mod tests {
    use super::TranscodePreset;

    /// The exact shape the Flutter editor handed to `wdmm_transcode_preset_save`
    /// **before v14**: `TranscodePreset.toJson()` in
    /// `lib/models/transcode_preset.dart` omits every `null` envelope field,
    /// never sends `updated_at`, and has no v14 keys at all. Deserialization
    /// must keep accepting it — an older client must not start failing with
    /// `error:parse:missing field …` just because the model grew fields.
    #[test]
    fn preset_json_from_the_old_editor_parses_with_v14_defaults() {
        let json = r#"{
            "id": "my-h265",
            "label": "My H.265",
            "video_copy": false,
            "audio_copy": false,
            "hw_video_encoder": "hevc_mediacodec",
            "sw_video_encoder": "libx264",
            "audio_codec": "aac",
            "audio_bitrate": 128000,
            "audio_channels": 2,
            "hls_segment_secs": 4,
            "sort_order": 100,
            "is_builtin": false
        }"#;
        let preset: TranscodePreset =
            serde_json::from_str(json).expect("old editor JSON must deserialize");
        assert_eq!(preset.id, "my-h265");
        assert_eq!(preset.hw_video_encoder, "hevc_mediacodec");
        assert_eq!(
            preset.sw_video_preset, "",
            "omitted preset defaults to none"
        );
        assert_eq!(
            preset.video_rate_control, "vbr",
            "omitted rate control defaults to vbr"
        );
        assert_eq!(preset.video_quality, None);
        assert!(preset.hw_video_options.is_empty());
        assert!(preset.sw_video_options.is_empty());
        assert_eq!(preset.video_max_w, None);
        assert_eq!(preset.audio_sample_rate, None);
        assert_eq!(preset.updated_at, 0, "omitted timestamps default to zero");
        preset.validate().expect("editor JSON must pass validation");
    }

    /// `list`/`get` output is fed straight back into the editor, so the round
    /// trip has to survive a timestamp and the v14 structured fields, including
    /// serialising the advanced options as JSON objects.
    #[test]
    fn preset_json_with_a_timestamp_round_trips() {
        let json = r#"{
            "id": "h264",
            "label": "H.264",
            "video_copy": true,
            "audio_copy": true,
            "hw_video_encoder": "h264_mediacodec",
            "hw_video_options": {"profile": "high", "level": "4.1"},
            "sw_video_encoder": "libx264",
            "sw_video_preset": "veryfast",
            "sw_video_options": {"crf": "23", "tune": "film"},
            "video_max_w": 1920,
            "video_max_h": 1080,
            "video_max_bitrate": 4000000,
            "video_fps_cap": 30,
            "video_rate_control": "cq",
            "video_quality": 23,
            "audio_codec": "aac",
            "audio_bitrate": 128000,
            "audio_channels": 2,
            "audio_sample_rate": 44100,
            "hls_segment_secs": 4,
            "sort_order": 10,
            "is_builtin": true,
            "updated_at": 1760000000
        }"#;
        let preset: TranscodePreset =
            serde_json::from_str(json).expect("stored JSON must deserialize");
        assert_eq!(preset.updated_at, 1_760_000_000);
        assert_eq!(preset.video_max_bitrate, Some(4_000_000));
        assert_eq!(preset.video_rate_control, "cq");
        assert_eq!(preset.video_quality, Some(23));
        assert_eq!(preset.sw_video_preset, "veryfast");
        assert_eq!(
            preset.hw_video_options.get("profile").map(String::as_str),
            Some("high")
        );
        let again: TranscodePreset =
            serde_json::from_str(&serde_json::to_string(&preset).unwrap()).unwrap();
        assert_eq!(again.updated_at, preset.updated_at);
        assert_eq!(again.id, preset.id);
        assert_eq!(again.sw_video_options, preset.sw_video_options);
        assert_eq!(again.video_quality, preset.video_quality);
    }

    /// The v14 enum-like fields are validated before they reach the DB, with
    /// the exact user-facing messages the FFI prefixes with `error:`.
    #[test]
    fn validate_rejects_unknown_rate_control_and_preset() {
        let mut preset: TranscodePreset = serde_json::from_str(
            r#"{
                "id": "x", "label": "X",
                "video_copy": true, "audio_copy": true,
                "hw_video_encoder": "h264_mediacodec",
                "sw_video_encoder": "libx264",
                "audio_codec": "aac", "audio_bitrate": 128000,
                "audio_channels": 2, "hls_segment_secs": 4,
                "sort_order": 0, "is_builtin": false
            }"#,
        )
        .unwrap();
        preset.sw_video_preset = "turbo".into();
        assert_eq!(
            preset.validate().unwrap_err(),
            "unknown encoder preset: turbo"
        );
        preset.sw_video_preset = "veryfast".into();
        preset.video_rate_control = "crf".into();
        assert_eq!(preset.validate().unwrap_err(), "unknown rate control: crf");
        preset.video_rate_control = "cq".into();
        preset.video_quality = None;
        assert_eq!(
            preset.validate().unwrap_err(),
            "cq rate control needs a quality value"
        );
        preset.video_quality = Some(23);
        preset.validate().expect("cq with a quality value is valid");
    }
}
