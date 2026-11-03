//! DLNA cast session facade.
//!
//! The whole cast path lives in Rust: a session owns the account driver, the
//! chunked proxy stream, the optional HLS transcode, subtitle delivery and the
//! AVTransport / ConnectionManager SOAP state machine. Dart only starts a
//! session ([`wdmm_dlna_session_start`]), polls [`wdmm_dlna_session_state`]
//! once per second and sends user-level commands through
//! [`wdmm_dlna_session_cmd`]. No media byte ever passes through Dart, and no
//! Range is ever materialised whole in memory — the proxy pulls 1 MiB chunks
//! through [`crate::driver_range_stream`].
//!
//! Subtitles reach the renderer through the DIDL subtitle `<res>` only (HLS
//! `EXT-X-MEDIA` renditions are dead: FFmpeg's HLS demuxer refuses subtitle
//! renditions, so Kodi never sees them). The MIME subtype is load-bearing:
//! Kodi's UPnP renderer matches it against `SupportedSubFormats` and silently
//! drops anything else — hence `text/<real ext>` for sidecars and `text/vtt`
//! for generated embedded tracks, never the old `text/plain`.
//!
//! Kodi turns *every* subtitle resource in the DIDL into a selectable external
//! stream (`UPnPInternal.cpp`), so the whole lane is delivered at once and a
//! subtitle switch never needs a new `SetAVTransportURI`. It also derives the
//! stream name and language from the resource file name
//! (`CUtil::GetExternalStreamDetailsFromFilename`), which is why every subtitle
//! URL ends in `<video stem>[.<language>][.<title>].<ext>`.
//!
//! The lane is prepared *before* the transcode warm-up (§C.2): sidecars are
//! exposed, and (HLS only) the source container's text tracks are probed and
//! extracted in one libav pass. That keeps the shared transcode gate out of the
//! playback path — the old on-demand extraction during playback competed with
//! the cast job and surfaced `transcoding busy`.
//!
//! Session errors are JSON (`{"ok":false,"error":"…"}`) rather than the
//! `"error:…"` string convention used by the older one-shot FFI calls, because
//! every `wdmm_dlna_session_*` result is a JSON document the Dart decoder
//! parses (see contract §3).

use std::collections::HashMap;
use std::ffi::c_char;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{json, Value};
use wdmm_dlna::{
    build_didl_lite, describe_renderer, subtitle_protocol_info, ConnectionManagerClient, DidlRes,
    DlnaRenderer, DlnaSession, SsdpDiscovery,
};
use wdmm_drivers::WdmmVfs;
use wdmm_ffmpeg::subtitle::{classify_codec, TrackKind};
use wdmm_proxy::{
    BindMode, BoxStream, HlsCacheOptions, HlsProgress, HlsSessionOpts, ProxyHandle, AHEAD_SEGMENTS,
};

use crate::{
    create_account_driver, driver_range_stream, global_library_db, proxy_cell, ptr_to_str, rt,
    FfiString,
};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Sidecar subtitle extensions handled by the cast path (bitmap `.idx` is not
/// advertised — renderers can only consume text subtitles).
const SUBTITLE_EXTS: &[&str] = &["srt", "ass", "ssa", "vtt", "sub"];

/// Extensions treated as video when deriving the subtitle base name.
const VIDEO_EXTS: &[&str] = &[
    "mp4", "m4v", "mkv", "avi", "mov", "webm", "ts", "m2ts", "mts", "mpg", "mpeg", "wmv", "flv",
    "3gp", "rmvb", "rm", "asf", "ogv", "vob", "divx",
];

/// Extensions treated as audio-only media (`media_kind:"audio"`).
///
/// Classification is deliberately extension-only: a source probe would stall
/// the very start-up the warm-up path exists to avoid, and libav reports a
/// cover art attached to an MP3/FLAC as a video stream, so a probe is *less*
/// accurate than the file name for music files (user report + task-20).
const AUDIO_EXTS: &[&str] = &[
    "mp3", "flac", "m4a", "aac", "wav", "ape", "ogg", "opus", "wma", "dts", "mka", "aiff", "aif",
    "alac", "tak", "wv",
];

/// Containers that many renderers refuse or play badly as-is.
const PROBLEMATIC_CONTAINERS: &[&str] = &[
    "mkv", "avi", "mov", "ts", "m2ts", "mts", "flv", "rmvb", "rm", "wmv", "asf", "3gp", "webm",
];

/// Codecs that virtually no renderer can decode inline.
const BAD_CODECS: &[&str] = &["rmvb", "rm", "wmv", "asf", "flv"];

/// Upper bound for a sidecar subtitle we are willing to read and expose.
const MAX_SUBTITLE_BYTES: usize = 8 * 1024 * 1024;

/// How long a direct/HLS cast waits for the parent-directory subtitle scan.
///
/// A slow share must never delay `SetAVTransportURI`: after this the cast
/// proceeds without sidecars and `subtitle_status.message` explains why.
const SIDECAR_SCAN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Marker separating the media path from an embedded track index in a synthetic
/// candidate path (`"/dir/movie.mkv#embedded:2"`).
const EMBEDDED_MARKER: &str = "#embedded:";

/// Discovery timeout accepted by the extended discover call.
const DISCOVER_MIN_MS: u64 = 200;
const DISCOVER_MAX_MS: u64 = 10_000;

/// How long a background libav job (embedded subtitle probe / track extraction)
/// waits for the proxy's shared transcode gate (contract §3.6).
const JOB_ACQUIRE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Default transcode-cache budget when `cache_budget_mb` is absent (contract §3.5).
const DEFAULT_CACHE_BUDGET_MB: u64 = 2048;

/// Positions past this are restored after a subtitle switch (a re-sent
/// `SetAVTransportURI` restarts playback; contract §C.4).
const RESUME_POSITION_MS: i64 = 1_000;

/// Transcode-cache knobs carried by `config_json` (contract §3.5).
///
/// The cache key also feeds the account/path/preset/size of the cast; those come
/// from the media itself and are filled in by [`prepare_media`].
#[derive(Debug, Clone)]
struct CacheConfig {
    /// `cache_dir`; `None` lets the proxy pick `<temp_dir>/wdmm-transcode`.
    root: Option<std::path::PathBuf>,
    /// `cache_budget_mb` converted to bytes; `2048 MiB` when absent.
    budget_bytes: u64,
    /// `cache_ident` (etag or mtime). An empty string marks the entry
    /// *ephemeral*: the proxy may cache segments but never reuses them across
    /// sessions (contract §3.5).
    ident: String,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            root: None,
            budget_bytes: DEFAULT_CACHE_BUDGET_MB * 1024 * 1024,
            ident: String::new(),
        }
    }
}

impl CacheConfig {
    /// Read the optional cache keys from a session `config_json`.
    fn from_json(cfg: &Value) -> Self {
        let root = opt_string(cfg, "cache_dir")
            .filter(|dir| !dir.trim().is_empty())
            .map(std::path::PathBuf::from);
        let budget_bytes = cfg
            .get("cache_budget_mb")
            .and_then(Value::as_u64)
            .filter(|mb| *mb > 0)
            .unwrap_or(DEFAULT_CACHE_BUDGET_MB)
            * 1024
            * 1024;
        let ident = opt_string(cfg, "cache_ident").unwrap_or_default();
        Self {
            root,
            budget_bytes,
            ident,
        }
    }
}

// ── Session registry ─────────────────────────────────────────────────────────

static SESSIONS: OnceLock<Mutex<HashMap<u32, SessionEntry>>> = OnceLock::new();
static NEXT_HANDLE: AtomicU32 = AtomicU32::new(1);

fn sessions() -> &'static Mutex<HashMap<u32, SessionEntry>> {
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Remove a session for the duration of one FFI call.
///
/// Sessions are *taken* out of the map (never locked across an `await`) and put
/// back afterwards, so a failed tick/command can still be closed by Dart and
/// release its proxy tokens.
fn take_session(handle: u32) -> Option<SessionEntry> {
    sessions().lock().unwrap().remove(&handle)
}

fn put_session(entry: SessionEntry) {
    sessions().lock().unwrap().insert(entry.handle, entry);
}

// ── Session state ────────────────────────────────────────────────────────────

/// One subtitle the cast can hand to the renderer: a sidecar file next to the
/// video, or (HLS only) a text track embedded in the source container.
///
/// Embedded tracks use a synthetic path
/// `<media remote_path>#embedded:<stream index>` (contract §4). Embedded text
/// tracks are extracted once, before the transcode warm-up (§C.2); the
/// candidate itself only carries metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SubtitleCandidate {
    name: String,
    path: String,
    /// Canonical language key (`"zh"`, `"en"`, …); empty for the default track.
    lang: String,
    /// Language tag exactly as the source carries it: the raw ISO 639-2 code
    /// from AVStream metadata for embedded tracks (no normalisation — the app
    /// maps it to a display name), or the lowercased sidecar file-name suffix
    /// (`"zh"`, `"chi"`, …) for sidecars. `None` when unknown.
    language: Option<String>,
    /// Track title from the source (`SubtitleStream.title`); `None` for
    /// sidecars and for embedded tracks that carry no title.
    title: Option<String>,
    /// Whether the file name is exactly `<video stem>.<ext>` (sidecars only).
    exact: bool,
    size: u64,
    /// `true` for a container track advertised by the HLS source probe.
    embedded: bool,
    /// AV stream index for embedded tracks; `None` for sidecars.
    track: Option<u32>,
    /// libav codec name for embedded tracks; `None` for sidecars.
    codec: Option<String>,
    /// `"external"` (sidecar), `"webvtt"` (embedded text),
    /// `"bitmap-unsupported"` (embedded bitmap — listed, never selectable) or
    /// `"error"` (preparation failed: extraction, size limit or the shared job
    /// gate; the app shows the track as unavailable).
    support: &'static str,
}

impl SubtitleCandidate {
    fn sidecar(
        name: String,
        path: String,
        lang: String,
        language: Option<String>,
        exact: bool,
        size: u64,
    ) -> Self {
        Self {
            name,
            path,
            lang,
            language,
            title: None,
            exact,
            size,
            embedded: false,
            track: None,
            codec: None,
            support: "external",
        }
    }

    /// Advertise one embedded track of the cast source (HLS mode only).
    fn embedded(
        remote_path: &str,
        track: u32,
        codec: String,
        language: Option<&str>,
        title: Option<&str>,
    ) -> Self {
        let language = language
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let lang = language
            .as_deref()
            .map(display_subtitle_language)
            .unwrap_or_default()
            .to_lowercase();
        let name = if lang.is_empty() {
            format!("{codec} track #{track} (embedded)")
        } else {
            format!("{lang} ({codec}, embedded)")
        };
        let support = match classify_codec(&codec) {
            TrackKind::Text => "webvtt",
            TrackKind::Bitmap => "bitmap-unsupported",
            TrackKind::Other => "bitmap-unsupported",
        };
        Self {
            name,
            path: format!("{remote_path}{EMBEDDED_MARKER}{track}"),
            lang,
            language,
            title: title
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
            exact: false,
            size: 0,
            embedded: true,
            track: Some(track),
            codec: Some(codec),
            support,
        }
    }

    fn is_bitmap(&self) -> bool {
        self.support == "bitmap-unsupported"
    }

    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "path": self.path,
            "lang": self.lang,
            "language": self.language,
            "title": self.title,
            "size": self.size,
            "embedded": self.embedded,
            "track": self.track,
            "codec": self.codec,
            "support": self.support,
        })
    }
}

/// `protocolInfo` for the subtitle `<res>` of `candidate` (contract §2).
///
/// Sidecars use `text/<real ext>`; generated embedded WebVTT uses `text/vtt`.
/// Bitmap candidates have no deliverable resource and return `None`.
fn subtitle_didl_protocol(candidate: &SubtitleCandidate) -> Option<String> {
    if candidate.is_bitmap() {
        return None;
    }
    if candidate.embedded {
        return subtitle_protocol_info("vtt");
    }
    subtitle_protocol_info(&extension(&candidate.name))
}

/// Embedded-track index encoded in a synthetic candidate path.
///
/// The path is synthetic only when it ends in `#embedded:<digits>` and the
/// part before the **last** marker equals the current media path — a real
/// filename containing `#embedded:` therefore stays a real path.
fn embedded_track_index(path: &str, remote_path: &str) -> Option<u32> {
    let idx = path.rfind(EMBEDDED_MARKER)?;
    let suffix = &path[idx + EMBEDDED_MARKER.len()..];
    if suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if &path[..idx] != remote_path {
        return None;
    }
    suffix.parse().ok()
}

/// One prepared subtitle the renderer can fetch.
///
/// `url` already carries the renderer-visible file name (contract §C.3):
/// Kodi derives the track's language and label from the URL's last path
/// segment (`CUtil::GetExternalStreamDetailsFromFilename`), not from the DIDL,
/// so `url_name` is `<video stem>[.<lang>][.<title>].<ext>`.
#[derive(Debug, Clone)]
struct SubtitleResource {
    candidate: SubtitleCandidate,
    url: String,
    protocol: String,
    token: String,
}

/// Subtitle delivery status for the state document (contract §6).
#[derive(Debug, Clone, Default)]
struct SubtitleStatus {
    text_tracks: u32,
    bitmap_tracks: u32,
    bitmap_note: Option<String>,
    message: Option<String>,
}

impl SubtitleStatus {
    fn to_json(&self) -> Value {
        json!({
            "text_tracks": self.text_tracks,
            "bitmap_tracks": self.bitmap_tracks,
            "bitmap_note": self.bitmap_note,
            "message": self.message,
        })
    }

    /// Fill the track counts + bitmap note from a source probe.
    ///
    /// `mode` is the cast provider; direct casts get a `null` note because Kodi
    /// renders bitmap subtitles natively from the container.
    fn apply_probe(&mut self, tracks: &[wdmm_ffmpeg::SubtitleStream], provider: &str) {
        let mut text = 0u32;
        let mut bitmap = 0u32;
        for track in tracks {
            match classify_codec(&track.codec) {
                TrackKind::Text => text += 1,
                TrackKind::Bitmap => bitmap += 1,
                TrackKind::Other => {}
            }
        }
        self.text_tracks = text;
        self.bitmap_tracks = bitmap;
        self.bitmap_note = if provider == "hls" && bitmap > 0 {
            Some(format!(
                "{bitmap} bitmap subtitle track(s) (PGS/DVD/DVB) are not available for HLS cast: \
                 text subtitles only (no libass / bitmap rendering in the transcode pipeline)"
            ))
        } else {
            None
        };
    }
}

/// High-level cast phase (contract §3). The `state` key keeps its old
/// renderer-transport meaning; `phase` describes the *preparation* stage so the
/// UI can show "warming up / speed 1.8×" before the renderer gets a URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SessionPhase {
    /// Nothing is playing (stopped before the renderer was ever pointed at it).
    #[default]
    Idle,
    /// HLS transcode warming up; the renderer has *not* been given the playlist.
    Warming,
    /// The renderer received the media URL and is playing (or paused).
    Playing,
    /// The renderer reported end of stream.
    Ended,
    /// The HLS job failed; `error` carries the precise reason.
    Failed,
}

impl SessionPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Warming => "warming",
            Self::Playing => "playing",
            Self::Ended => "ended",
            Self::Failed => "failed",
        }
    }
}

/// Everything the renderer is currently pointing at.
struct SessionEntry {
    handle: u32,
    renderer: DlnaRenderer,
    session: DlnaSession,
    driver: Arc<dyn WdmmVfs>,
    account_id: String,
    remote_path: String,
    name: String,
    size: u64,
    preset_id: Option<String>,
    force_direct: bool,
    ui_lang: String,
    provider: &'static str,
    /// `"video"` or `"audio"` — the DLNA item class the renderer is given.
    media_kind: &'static str,
    proxy: ProxyHandle,
    lan_ip: IpAddr,
    stream_token: String,
    /// Loopback URL of the registered source stream (input for the HLS job and
    /// the URL the embedded-track extractor re-reads).
    loopback_url: String,
    hls_token: Option<String>,
    media_url: String,
    mime: String,
    protocol_info: String,
    subtitle_candidates: Vec<SubtitleCandidate>,
    selected_subtitle: Option<SubtitleCandidate>,
    /// Every subtitle resource handed to the renderer, selected track first.
    ///
    /// The whole lane is prepared before the transcode warm-up (contract §C.2),
    /// so selecting a subtitle never extracts or exposes anything during
    /// playback. The renderer learns language/label from the URL file name.
    subtitle_resources: Vec<SubtitleResource>,
    /// Proxy tokens of [`Self::subtitle_resources`], released together when the
    /// media switches, the session stops or the session closes.
    subtitle_tokens: Vec<String>,
    /// Per-track generated WebVTT, keyed by AV stream index (HLS only).
    embedded_cache: std::collections::HashMap<u32, Arc<Vec<u8>>>,
    /// Track counts / bitmap note / scan note surfaced to the app (contract §6).
    subtitle_status: SubtitleStatus,
    /// Preparation phase (contract §3).
    phase: SessionPhase,
    /// DIDL document handed to the renderer (re-sent by `cast`).
    didl: String,
    /// Transcode-cache knobs for the current media (contract §3.5).
    cache: CacheConfig,
    /// Failure reason for `phase == Failed`.
    last_error: Option<String>,
}

/// Freshly prepared media resources before they are handed to a renderer.
struct PreparedMedia {
    provider: &'static str,
    /// `"video"` or `"audio"`.
    media_kind: &'static str,
    stream_token: String,
    loopback_url: String,
    hls_token: Option<String>,
    media_url: String,
    mime: String,
    protocol_info: String,
    size: u64,
    subtitle_candidates: Vec<SubtitleCandidate>,
    selected_subtitle: Option<SubtitleCandidate>,
    /// Prepared subtitle lane: one resource per deliverable track (contract §C).
    subtitle_resources: Vec<SubtitleResource>,
    /// Proxy tokens of [`Self::subtitle_resources`].
    subtitle_tokens: Vec<String>,
    /// Generated WebVTT per embedded stream index (HLS only).
    embedded_cache: std::collections::HashMap<u32, Arc<Vec<u8>>>,
    subtitle_status: SubtitleStatus,
    didl: String,
    /// HLS job started without waiting for the playlist (`register_hls_nowait`).
    warming: bool,
}

/// Remember where the diagnostics log lives, so a developer can pull it with
/// `adb exec-out run-as … cat files/wdmm-hls-debug.log` (§7.4).
pub(crate) fn init_hls_diag(db_path: &str) {
    static INIT: OnceLock<()> = OnceLock::new();
    // In-memory databases have no directory to write next to.
    if db_path == ":memory:" {
        return;
    }
    INIT.get_or_init(|| {
        let Some(dir) = std::path::Path::new(db_path).parent() else {
            return;
        };
        if dir.as_os_str().is_empty() {
            return;
        }
        wdmm_ffmpeg::diag::set_file(Some(dir.join("wdmm-hls-debug.log")));
    });
}

// ── JSON helpers ─────────────────────────────────────────────────────────────

/// Build `{"ok":false,"error":"…"}`.
fn err_json(msg: impl std::fmt::Display) -> FfiString {
    FfiString::from(json!({"ok": false, "error": msg.to_string()}).to_string())
}

/// Read a required, non-empty string field from a config object.
fn str_field(cfg: &Value, key: &str) -> Result<String, String> {
    match cfg.get(key).and_then(Value::as_str) {
        Some(s) if !s.trim().is_empty() => Ok(s.to_string()),
        _ => Err(format!("missing or empty `{key}`")),
    }
}

/// Read an optional, non-empty string field.
fn opt_string(cfg: &Value, key: &str) -> Option<String> {
    cfg.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

// ── Path / mime helpers ──────────────────────────────────────────────────────

/// Lowercase extension of `name` (empty when there is none).
fn extension(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() => ext.to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// Content type advertised to the renderer.
///
/// Mirrors the legacy Dart `_mimeFor`, including the `video/mp4` default — the
/// renderers in use only care about the `video/*` prefix and the codecs inside.
fn mime_for(name: &str) -> &'static str {
    match extension(name).as_str() {
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "ts" | "m2ts" | "mts" => "video/mp2t",
        "mpg" | "mpeg" => "video/mpeg",
        "flv" => "video/x-flv",
        "wmv" => "video/x-ms-wmv",
        "3gp" => "video/3gpp",
        "ogv" => "video/ogg",
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "m4a" | "aac" => "audio/mp4",
        "wav" => "audio/wav",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "ape" => "audio/x-ape",
        "wv" => "audio/x-wavpack",
        "dts" => "audio/vnd.dts",
        "wma" => "audio/x-ms-wma",
        "mka" => "audio/x-matroska",
        "aiff" | "aif" => "audio/aiff",
        "alac" => "audio/mp4",
        "tak" => "audio/x-tak",
        _ => "video/mp4",
    }
}

/// `"audio"` when `name` denotes an audio-only file, `"video"` otherwise.
///
/// Extension-only by design (see [`AUDIO_EXTS`]): case-insensitive, and any
/// `?query` / `#fragment` a caller may have left on the name is ignored.
fn media_kind(name: &str) -> &'static str {
    let ext = extension(name);
    let ext = ext
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .trim_end_matches('.');
    if AUDIO_EXTS.contains(&ext) {
        "audio"
    } else {
        "video"
    }
}

/// Last path segment.
fn file_name(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit_once('/')
        .map(|(_, name)| name.to_string())
        .unwrap_or_else(|| path.trim_end_matches('/').to_string())
}

/// Parent directory of a `/`-separated absolute path (`"/"` at the root).
fn parent_dir(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(idx) => trimmed[..idx].to_string(),
    }
}

/// Join a `/`-separated directory and a child name.
fn join_path(dir: &str, name: &str) -> String {
    if dir == "/" || dir.is_empty() {
        format!("/{name}")
    } else {
        format!("{}/{}", dir.trim_end_matches('/'), name)
    }
}

/// Strip a known video extension from a file name.
fn video_stem(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if VIDEO_EXTS.contains(&ext.to_ascii_lowercase().as_str()) => {
            stem.to_string()
        }
        _ => name.to_string(),
    }
}

// ── Subtitle matching (Dart `subtitle_sidecar.dart` parity) ───────────────────

/// Normalise a raw language tag to the key used for matching
/// (`zh`/`zh-cn`/`chs`/`cht` → `"zh"`, `en`/`eng` → `"en"`, `und` → `""`).
fn display_subtitle_language(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let normalized = trimmed.to_ascii_lowercase().replace('_', "-");
    match normalized.as_str() {
        "und" | "undetermined" | "unknown" => return String::new(),
        _ => {}
    }
    const ZH: &[&str] = &[
        "zh", "zho", "chi", "chs", "cht", "zh-cn", "zh-sg", "zh-hans", "zh-tw", "zh-hk", "zh-mo",
        "zh-hant", "cn", "sc", "tc",
    ];
    if ZH.contains(&normalized.as_str()) || normalized.starts_with("zh-") {
        return "zh".to_string();
    }
    if normalized == "en" || normalized == "eng" || normalized.starts_with("en-") {
        return "en".to_string();
    }
    trimmed.to_string()
}

/// One matched sidecar subtitle.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SubtitleMatch {
    /// Canonical language key used for matching (`"zh"`, `"en"`, …).
    lang_key: String,
    /// Raw lowercase language suffix from the file name (`"zh-hans"`,
    /// `"chi"`); `None` for a bare `<stem>.<ext>` sidecar.
    language: Option<String>,
    /// The file name is exactly `<video stem>.<ext>`.
    exact: bool,
}

/// Match `subtitle_name` against the video's stem.
fn match_subtitle_name(video_name: &str, subtitle_name: &str) -> Option<SubtitleMatch> {
    let ext = extension(subtitle_name);
    if !SUBTITLE_EXTS.contains(&ext.as_str()) {
        return None;
    }
    let stem = video_stem(video_name).to_lowercase();
    let lower_name = subtitle_name.to_lowercase();
    if lower_name == format!("{stem}.{ext}") {
        return Some(SubtitleMatch {
            lang_key: String::new(),
            language: None,
            exact: true,
        });
    }
    let rest = lower_name.strip_prefix(&format!("{stem}."))?;
    let middle = rest.strip_suffix(&format!(".{ext}"))?;
    if middle.is_empty() {
        return None;
    }
    Some(SubtitleMatch {
        lang_key: display_subtitle_language(middle).to_lowercase(),
        language: Some(middle.to_string()),
        exact: false,
    })
}

/// Rank candidates the way the legacy Dart picker did: exact name first, then
/// the UI language, then everything else; ties break on the lowercased name.
fn pick_subtitle(candidates: &[SubtitleCandidate], ui_lang: &str) -> Option<usize> {
    let ui = display_subtitle_language(ui_lang).to_lowercase();
    let rank = |candidate: &SubtitleCandidate| -> u8 {
        if candidate.exact {
            0
        } else if !ui.is_empty() && candidate.lang == ui {
            1
        } else {
            2
        }
    };
    candidates
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            rank(a)
                .cmp(&rank(b))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        })
        .map(|(idx, _)| idx)
}

// ── Stream helpers ───────────────────────────────────────────────────────────

/// Serve a whole in-memory subtitle buffer through the proxy range contract.
fn mem_range_stream(data: Arc<Vec<u8>>, start: i64, end: i64) -> BoxStream {
    let len = data.len() as u64;
    let first = start.max(0) as u64;
    let last = if end < 0 {
        len.saturating_sub(1)
    } else {
        (end as u64).min(len.saturating_sub(1))
    };
    if len == 0 || first > last || first >= len {
        return Box::pin(futures::stream::empty::<std::io::Result<bytes::Bytes>>());
    }
    let payload = bytes::Bytes::copy_from_slice(&data[first as usize..=last as usize]);
    let item: std::io::Result<bytes::Bytes> = Ok(payload);
    Box::pin(futures::stream::iter(std::iter::once(item)))
}

// ── Network helpers ──────────────────────────────────────────────────────────

/// Extract the host IP from a renderer description URL.
fn renderer_ip_from_location(location: &str) -> Result<IpAddr, String> {
    let host_port = location.split("://").nth(1).unwrap_or(location);
    let host_port = host_port.split(['/', '?', '#']).next().unwrap_or("").trim();
    let host = match host_port.rsplit_once(':') {
        Some((host, port)) if port.parse::<u16>().is_ok() => host,
        _ => host_port,
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(ip);
    }
    (host, 0u16)
        .to_socket_addrs()
        .map_err(|e| format!("invalid renderer location: {e}"))?
        .next()
        .map(|addr| addr.ip())
        .ok_or_else(|| "invalid renderer location: no address".to_string())
}

/// Local LAN IP the renderer can reach us on.
///
/// Uses a connected UDP socket to let the kernel pick the outgoing interface
/// (no packet is sent). Loopback means the renderer is on another host and we
/// have no LAN address for it — the caller surfaces that as an error instead of
/// silently advertising `127.0.0.1`.
fn lan_ip_for(renderer_ip: IpAddr) -> Result<IpAddr, String> {
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("lan probe failed: {e}"))?;
    socket
        .connect(SocketAddr::new(renderer_ip, 9))
        .map_err(|e| format!("lan probe failed: {e}"))?;
    let ip = socket
        .local_addr()
        .map_err(|e| format!("lan probe failed: {e}"))?
        .ip();
    if ip.is_loopback() || ip.is_unspecified() {
        return Err("no lan address to renderer".to_string());
    }
    Ok(ip)
}

/// Is an existing proxy bind address reachable from the renderer's LAN?
fn proxy_reachable(bound: IpAddr, lan_ip: IpAddr) -> bool {
    bound.is_unspecified() || bound == lan_ip
}

/// Make sure the process-wide proxy is bound to a LAN-reachable address.
///
/// `wdmm_proxy_start` is an idempotent global singleton (it ignores a new bind
/// mode once running), so a proxy started for loopback-only MustProxy playback
/// cannot be widened in place. In that case the old handle is stopped and
/// replaced with a [`BindMode::LanAny`] one; tokens registered on the old proxy
/// die with it, which only matters if local playback was active at cast start.
async fn ensure_lan_proxy(lan_ip: IpAddr) -> Result<ProxyHandle, String> {
    {
        let guard = proxy_cell().lock().unwrap();
        if let Some(handle) = guard.as_ref() {
            if let Some(addr) = handle.addr() {
                if proxy_reachable(addr.ip(), lan_ip) {
                    return Ok(handle.clone());
                }
            }
        }
    }

    let stale = proxy_cell().lock().unwrap().take();
    if let Some(stale) = stale {
        stale.stop();
    }

    let handle = ProxyHandle::new(BindMode::LanAny, 0);
    // `register_file` is the cheapest public call that lazily binds the server.
    let trigger = std::env::temp_dir().join(format!(
        "wdmm-dlna-proxy-{}-{}.tmp",
        std::process::id(),
        NEXT_HANDLE.load(Ordering::Relaxed)
    ));
    std::fs::write(&trigger, b"").map_err(|e| format!("proxy start failed: {e}"))?;
    let started = handle
        .register_file(trigger.clone(), None, None, None)
        .await;
    let _ = std::fs::remove_file(&trigger);
    match started {
        Ok(registration) => handle.unregister(&registration.token),
        Err(e) => return Err(format!("proxy start failed: {e}")),
    }
    if handle.addr().is_none() {
        return Err("proxy start failed: no bound address".to_string());
    }
    *proxy_cell().lock().unwrap() = Some(handle.clone());
    Ok(handle)
}

// ── Media preparation ────────────────────────────────────────────────────────

/// `compat` hint for the Dart side: whether the current URL needs a transcode
/// and a lowercase reason that matches the Dart keyword mapping
/// (`audio` / `video` / `container` / `remux`).
fn compat_json(name: &str, provider: &str) -> Value {
    let ext = extension(name);
    let bad_codec = BAD_CODECS.contains(&ext.as_str());
    let problematic = PROBLEMATIC_CONTAINERS.contains(&ext.as_str());
    if provider == "hls" {
        let reason = if bad_codec {
            "video+audio codec"
        } else if problematic {
            "container may need remux"
        } else {
            "hls transcode"
        };
        return json!({"requires_transcode": true, "reason": reason});
    }
    if bad_codec {
        json!({"requires_transcode": true, "reason": "video+audio codec"})
    } else if problematic {
        json!({"requires_transcode": true, "reason": "container may need remux"})
    } else {
        json!({"requires_transcode": false, "reason": Value::Null})
    }
}

/// Whether the renderer advertises the MIME class of `media_kind`.
///
/// An empty `SinkProtocolInfo` means the renderer did not answer the
/// ConnectionManager query; that must not reject the cast (task-20).
fn sink_advertises(sink_protocols: &[String], media_kind: &str) -> bool {
    let prefix = if media_kind == "audio" {
        "audio/"
    } else {
        "video/"
    };
    sink_protocols.iter().any(|info| {
        info.split(':')
            .nth(2)
            // `*/*` is a renderer saying "I take everything".
            .is_some_and(|m| m.starts_with(prefix) || m == "*/*")
    })
}

/// Build the DIDL-Lite document: the media resource plus one `<res>` per
/// prepared subtitle (contract §C.1).
///
/// Kodi's UPnP renderer turns every subtitle resource whose MIME subtype is a
/// real format (`text/srt`, `text/vtt`, …) into a selectable external subtitle
/// stream, so the whole lane is delivered in one document instead of being
/// re-sent per selection (evidence: Kodi `UPnPInternal.cpp` `SupportedSubFormats`
/// + live renderer test).
fn build_didl(
    title: &str,
    media_url: &str,
    protocol_info: &str,
    subtitles: &[(String, String)],
) -> String {
    let mut resources = vec![DidlRes::new(media_url, protocol_info)];
    for (url, subtitle_protocol_info) in subtitles {
        resources.push(DidlRes::new(url, subtitle_protocol_info));
    }
    build_didl_lite(title, &resources)
}

/// DIDL for either media kind.
fn build_media_didl(
    title: &str,
    media_kind: &str,
    media_url: &str,
    protocol_info: &str,
    subtitles: &[(String, String)],
) -> String {
    if media_kind == "audio" {
        build_audio_didl(title, media_url, protocol_info, subtitles)
    } else {
        build_didl(title, media_url, protocol_info, subtitles)
    }
}

/// DIDL for the media a live session currently points at.
fn build_session_didl(entry: &SessionEntry, subtitles: &[(String, String)]) -> String {
    build_media_didl(
        &entry.name,
        entry.media_kind,
        &entry.media_url,
        &entry.protocol_info,
        subtitles,
    )
}

/// Build the DIDL-Lite document for an audio-only item.
///
/// `wdmm_dlna::build_didl_lite` hard-codes `object.item.videoItem` and is
/// outside this crate's write scope, so the audio class is rendered locally
/// (task-20; fold into `wdmm-dlna` once it grows an audio builder).
fn build_audio_didl(
    title: &str,
    media_url: &str,
    protocol_info: &str,
    subtitles: &[(String, String)],
) -> String {
    let mut document = String::from(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
         xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
         xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">\
         <item id=\"0\" parentID=\"-1\" restricted=\"1\">",
    );
    document.push_str(&format!("<dc:title>{}</dc:title>", xml_escape(title)));
    document.push_str("<upnp:class>object.item.audioItem.musicTrack</upnp:class>");
    document.push_str(&format!(
        "<res protocolInfo=\"{}\">{}</res>",
        xml_escape(protocol_info),
        xml_escape(media_url)
    ));
    for (url, subtitle_protocol_info) in subtitles {
        document.push_str(&format!(
            "<res protocolInfo=\"{}\">{}</res>",
            xml_escape(subtitle_protocol_info),
            xml_escape(url)
        ));
    }
    document.push_str("</item></DIDL-Lite>");
    document
}

/// Escape XML text/attribute content the way `wdmm-dlna` does.
fn xml_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

/// Discover a file size through the driver when the caller did not know it.
async fn resolve_size(driver: Arc<dyn WdmmVfs>, path: &str, size: u64) -> u64 {
    if size > 0 {
        return size;
    }
    match driver.get(path).await {
        Ok(item) => item.size,
        Err(_) => 0,
    }
}

/// Register the chunked driver stream for `remote_path` on the proxy.
async fn register_driver_stream(
    handle: &ProxyHandle,
    driver: Arc<dyn WdmmVfs>,
    remote_path: String,
    name: Option<String>,
    size: u64,
) -> Result<wdmm_proxy::Registration, String> {
    let path = remote_path;
    let stream_driver = Arc::clone(&driver);
    handle
        .register_driver(size, name, None, None, move |start, end| {
            driver_range_stream(Arc::clone(&stream_driver), path.clone(), size, start, end)
        })
        .await
        .map_err(|e| e.to_string())
}

/// List sidecar subtitles next to `remote_path` and match them to `video_name`.
///
/// The driver list can fail (permissions, flaky share); the caller bounds this
/// with [`SIDECAR_SCAN_TIMEOUT`] and turns a failure into
/// `subtitle_status.message` instead of failing the cast.
async fn scan_subtitles(
    driver: Arc<dyn WdmmVfs>,
    remote_path: &str,
    video_name: &str,
) -> Result<Vec<SubtitleCandidate>, String> {
    let parent = parent_dir(remote_path);
    let items = driver.list(&parent).await.map_err(|e| e.to_string())?;
    let mut candidates = Vec::new();
    for item in items {
        if item.is_dir {
            continue;
        }
        if let Some(matched) = match_subtitle_name(video_name, &item.name) {
            candidates.push(SubtitleCandidate::sidecar(
                item.name.clone(),
                join_path(&parent, &item.name),
                matched.lang_key,
                matched.language,
                matched.exact,
                item.size,
            ));
        }
    }
    // Deterministic ordering keeps the auto-pick stable across casts.
    candidates.sort_by(|a, b| {
        a.exact
            .cmp(&b.exact)
            .reverse()
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(candidates)
}

/// Renderer-visible file name for one subtitle resource (contract §C.3).
///
/// A renderer derives the external track's language/label from the URL's last
/// path segment (`CUtil::GetExternalStreamDetailsFromFilename`), so the proxy
/// URL is registered as `<video stem>[.<lang>][.<title>].<ext>`. Two tracks of
/// the same format that carry neither a language nor a title fall back to
/// `<video stem>.<n>.<ext>`.
fn subtitle_url_name(video_name: &str, candidate: &SubtitleCandidate, index: usize) -> String {
    let stem = video_stem(video_name);
    let ext = if candidate.embedded {
        "vtt".to_string()
    } else {
        let ext = extension(&candidate.name);
        if ext.is_empty() {
            "srt".to_string()
        } else {
            ext
        }
    };
    let mut tokens = vec![stem];
    if let Some(language) = candidate
        .language
        .as_deref()
        .map(sanitize_url_token)
        .filter(|token| !token.is_empty())
    {
        tokens.push(language);
    }
    if let Some(title) = candidate
        .title
        .as_deref()
        .map(sanitize_url_token)
        .filter(|token| !token.is_empty())
    {
        tokens.push(title);
    }
    if tokens.len() == 1 {
        tokens.push((index + 1).to_string());
    }
    format!("{}.{}", tokens.join("."), ext)
}

/// Strip characters that break a URL path or a renderer's file-name parser
/// (`<>:"/\|?*`, control characters); spaces become dots.
fn sanitize_url_token(raw: &str) -> String {
    let mut token = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
            continue;
        }
        token.push(if ch == ' ' { '.' } else { ch });
    }
    token.trim_matches('.').to_string()
}

/// Register `data` as a named in-memory proxy stream and return the prepared
/// subtitle resource.
async fn expose_subtitle_data(
    handle: &ProxyHandle,
    lan_ip: IpAddr,
    video_name: &str,
    candidate: &SubtitleCandidate,
    index: usize,
    data: Arc<Vec<u8>>,
    protocol: String,
) -> Result<SubtitleResource, String> {
    let url_name = subtitle_url_name(video_name, candidate, index);
    let size = data.len() as u64;
    let stream_data = Arc::clone(&data);
    let registration = handle
        .register_driver_named(
            size,
            Some(candidate.name.clone()),
            Some(url_name),
            None,
            None,
            move |start, end| mem_range_stream(Arc::clone(&stream_data), start, end),
        )
        .await
        .map_err(|e| e.to_string())?;
    let port = handle
        .addr()
        .map(|addr| addr.port())
        .ok_or_else(|| "proxy has no bound address".to_string())?;
    Ok(SubtitleResource {
        candidate: candidate.clone(),
        url: format!("http://{lan_ip}:{port}{}", registration.path_template),
        protocol,
        token: registration.token,
    })
}

/// Read a sidecar subtitle and expose it through the proxy.
async fn expose_subtitle(
    handle: &ProxyHandle,
    driver: Arc<dyn WdmmVfs>,
    lan_ip: IpAddr,
    video_name: &str,
    candidate: &SubtitleCandidate,
    index: usize,
) -> Result<SubtitleResource, String> {
    let raw = match driver.get_content(&candidate.path).await {
        Ok(bytes) => bytes,
        Err(_) => driver
            .get_content_range(&candidate.path, 0, None)
            .await
            .map_err(|e| e.to_string())?,
    };
    if raw.len() > MAX_SUBTITLE_BYTES {
        return Err(format!(
            "subtitle too large ({} bytes > {MAX_SUBTITLE_BYTES})",
            raw.len()
        ));
    }
    let protocol = subtitle_didl_protocol(candidate)
        .ok_or_else(|| "subtitle format is not deliverable".to_string())?;
    expose_subtitle_data(
        handle,
        lan_ip,
        video_name,
        candidate,
        index,
        Arc::new(raw),
        protocol,
    )
    .await
}

/// Result of the pre-warm subtitle pass.
struct PreparedSubtitles {
    /// Candidate list for the app; bitmap tracks are listed but have no
    /// resource.
    candidates: Vec<SubtitleCandidate>,
    /// Deliverable resources, the auto-selected track first.
    resources: Vec<SubtitleResource>,
    /// Auto-picked track for this cast (also the first resource).
    selected: Option<SubtitleCandidate>,
    /// Generated WebVTT per embedded stream index (HLS only).
    embedded_cache: std::collections::HashMap<u32, Arc<Vec<u8>>>,
}

impl PreparedSubtitles {
    fn tokens(&self) -> Vec<String> {
        self.resources
            .iter()
            .map(|resource| resource.token.clone())
            .collect()
    }
}

/// Metadata probe of the cast source (opens the container and calls
/// `avformat_find_stream_info`; decodes nothing).
///
/// Runs before the transcode job exists (§C.2), so it never contends with a
/// cast. The old on-demand probe during playback did, and that is what turned
/// selecting a subtitle into `transcoding busy`.
async fn probe_source(loopback_url: &str) -> Result<wdmm_ffmpeg::ProbeResult, String> {
    let url = loopback_url.to_string();
    tokio::task::spawn_blocking(move || wdmm_ffmpeg::probe(&url).map_err(|e| e.to_string()))
        .await
        .map_err(|e| format!("subtitle probe failed: {e}"))?
}

/// Extract several embedded text tracks in one libav pass.
///
/// Extraction decodes the source's subtitle streams, so it holds the shared job
/// gate (§C.2 + the HLS contract's §3.6). It runs while the HLS job does not
/// exist yet, which is why the gate is free in the normal path.
async fn extract_subtitle_tracks(
    handle: &ProxyHandle,
    loopback_url: &str,
    indexes: &[u32],
) -> std::collections::HashMap<u32, String> {
    if indexes.is_empty() {
        return std::collections::HashMap::new();
    }
    let gate = handle.job_gate();
    let url = loopback_url.to_string();
    let indexes = indexes.to_vec();
    let extracted = tokio::task::spawn_blocking(move || {
        let _permit = gate.acquire(JOB_ACQUIRE_TIMEOUT)?;
        wdmm_ffmpeg::extract_subtitle_tracks(&url, &indexes)
            .map_err(|e| format!("subtitle extraction failed: {e}"))
    })
    .await;
    match extracted {
        Ok(Ok(tracks)) => tracks,
        Ok(Err(e)) => {
            wdmm_ffmpeg::diag::log(format!("[subtitle] {e}"));
            std::collections::HashMap::new()
        }
        Err(e) => {
            wdmm_ffmpeg::diag::log(format!("[subtitle] extraction task failed: {e}"));
            std::collections::HashMap::new()
        }
    }
}

/// Prepare the whole subtitle lane before the cast starts (§C.2).
///
/// Every auto-matched sidecar is exposed, and (HLS only) every embedded text
/// track is extracted in one pass and exposed as WebVTT. All of it happens
/// before `register_hls_nowait`, so a later app-side switch is a pure metadata
/// re-send. One failing subtitle never fails the cast: sidecar failures are
/// reported in `subtitle_status.message` and embedded tracks flip to
/// `support:"error"`.
#[allow(clippy::too_many_arguments)]
async fn prepare_subtitles(
    handle: &ProxyHandle,
    lan_ip: IpAddr,
    driver: Arc<dyn WdmmVfs>,
    provider: &'static str,
    video_name: &str,
    remote_path: &str,
    loopback_url: &str,
    ui_lang: &str,
    status: &mut SubtitleStatus,
) -> PreparedSubtitles {
    let started = std::time::Instant::now();

    // 1. Sidecars next to the media (bounded: a slow share must not stall the
    //    cast start forever).
    let mut candidates = match tokio::time::timeout(
        SIDECAR_SCAN_TIMEOUT,
        scan_subtitles(Arc::clone(&driver), remote_path, video_name),
    )
    .await
    {
        Ok(Ok(candidates)) => candidates,
        Ok(Err(e)) => {
            status.message = Some(format!("sidecar subtitle scan failed: {e}"));
            Vec::new()
        }
        Err(_) => {
            status.message = Some(format!(
                "sidecar subtitle scan timed out after {}s",
                SIDECAR_SCAN_TIMEOUT.as_secs()
            ));
            Vec::new()
        }
    };
    let sidecar_count = candidates.len();

    // 2. Embedded tracks (HLS only: a direct cast leaves them in the container
    //    for the renderer to demux itself).
    let mut embedded_cache: std::collections::HashMap<u32, Arc<Vec<u8>>> =
        std::collections::HashMap::new();
    let mut embedded_count = 0usize;
    if provider == "hls" {
        match probe_source(loopback_url).await {
            Ok(probe) => {
                status.apply_probe(&probe.subtitles, provider);
                let indexes: Vec<u32> = probe
                    .subtitles
                    .iter()
                    .filter(|track| matches!(classify_codec(&track.codec), TrackKind::Text))
                    .map(|track| track.index)
                    .collect();
                let extracted = extract_subtitle_tracks(handle, loopback_url, &indexes).await;
                for track in &probe.subtitles {
                    let mut candidate = SubtitleCandidate::embedded(
                        remote_path,
                        track.index,
                        track.codec.clone(),
                        track.language.as_deref(),
                        track.title.as_deref(),
                    );
                    if matches!(classify_codec(&track.codec), TrackKind::Text) {
                        match extracted.get(&track.index) {
                            Some(vtt) if vtt.len() <= MAX_SUBTITLE_BYTES => {
                                embedded_cache
                                    .insert(track.index, Arc::new(vtt.clone().into_bytes()));
                            }
                            Some(_) => {
                                wdmm_ffmpeg::diag::log(format!(
                                    "[subtitle] embedded track {} is larger than {} bytes",
                                    track.index, MAX_SUBTITLE_BYTES
                                ));
                                candidate.support = "error";
                            }
                            None => candidate.support = "error",
                        }
                    }
                    embedded_count += 1;
                    candidates.push(candidate);
                }
            }
            Err(e) => {
                let message = format!("embedded subtitle probe failed: {e}");
                wdmm_ffmpeg::diag::log(format!("[subtitle] {message}"));
                status.message = Some(message);
            }
        }
    }

    // 3. Expose every deliverable resource.
    let mut resources: Vec<SubtitleResource> = Vec::new();
    for (index, candidate) in candidates.iter_mut().enumerate() {
        if candidate.is_bitmap() {
            continue;
        }
        let is_embedded = candidate.embedded;
        let exposed = if is_embedded {
            match candidate
                .track
                .and_then(|track| embedded_cache.get(&track).cloned())
            {
                Some(data) => match subtitle_didl_protocol(candidate) {
                    Some(protocol) => {
                        expose_subtitle_data(
                            handle, lan_ip, video_name, candidate, index, data, protocol,
                        )
                        .await
                    }
                    None => {
                        candidate.support = "error";
                        continue;
                    }
                },
                None => continue,
            }
        } else {
            expose_subtitle(
                handle,
                Arc::clone(&driver),
                lan_ip,
                video_name,
                candidate,
                index,
            )
            .await
        };
        match exposed {
            Ok(resource) => resources.push(resource),
            Err(e) => {
                if is_embedded {
                    wdmm_ffmpeg::diag::log(format!(
                        "[subtitle] embedded track {} unavailable: {e}",
                        candidate.track.unwrap_or_default()
                    ));
                    candidate.support = "error";
                } else {
                    status.message = Some(format!("sidecar subtitle unavailable: {e}"));
                }
            }
        }
    }

    // 4. Auto-pick among what is actually deliverable and put it first, so a
    //    renderer that plays the first subtitle lands on the UI language.
    let pickable: Vec<SubtitleCandidate> = resources
        .iter()
        .map(|resource| resource.candidate.clone())
        .collect();
    let selected = pick_subtitle(&pickable, ui_lang).map(|index| pickable[index].clone());
    if let Some(selected) = &selected {
        if let Some(position) = resources
            .iter()
            .position(|resource| resource.candidate.path == selected.path)
        {
            let first = resources.remove(position);
            resources.insert(0, first);
        }
    }

    wdmm_ffmpeg::diag::log(format!(
        "[subtitle] prepared {} subtitle resource(s) from {sidecar_count} sidecar candidate(s) and \
         {embedded_count} embedded track(s) in {} ms, before the transcode warm-up",
        resources.len(),
        started.elapsed().as_millis()
    ));

    PreparedSubtitles {
        candidates,
        resources,
        selected,
        embedded_cache,
    }
}

/// Register the proxy stream(s), negotiate the protocol and build the DIDL.
///
/// Shared by [`start_session`] and the `select` command so switching episodes
/// and starting a cast take exactly the same path.
#[allow(clippy::too_many_arguments)]
async fn prepare_media(
    handle: &ProxyHandle,
    lan_ip: IpAddr,
    renderer: &DlnaRenderer,
    driver: Arc<dyn WdmmVfs>,
    account_id: &str,
    remote_path: &str,
    name: &str,
    size: u64,
    preset_id: Option<&str>,
    force_direct: bool,
    warmup: bool,
    ui_lang: &str,
    cache: &CacheConfig,
) -> Result<PreparedMedia, String> {
    let size = resolve_size(Arc::clone(&driver), remote_path, size).await;
    let port = handle
        .addr()
        .map(|addr| addr.port())
        .ok_or_else(|| "proxy has no bound address".to_string())?;

    let registration = register_driver_stream(
        handle,
        Arc::clone(&driver),
        remote_path.to_string(),
        Some(name.to_string()),
        size,
    )
    .await
    .map_err(|e| format!("proxy register failed: {e}"))?;
    let stream_token = registration.token.clone();
    let loopback_url = format!("http://127.0.0.1:{port}{}", registration.path_template);

    // `force_direct`, or the passthrough preset, means the renderer fetches the
    // original bytes from the proxy; anything else goes through HLS.
    let wants_direct = force_direct || matches!(preset_id, Some("passthrough"));
    let provider: &'static str = if wants_direct { "direct" } else { "hls" };

    // Audio-only sources negotiate and advertise audio classes; the renderer
    // must not be rejected by the video/* gate (task-20).
    let media_kind = media_kind(name);
    let mime = if media_kind == "audio" && provider == "hls" {
        // The transcode output is an MPEG-TS audio stream (AAC), so the
        // playlist's audio resource is announced as `audio/mpeg`.
        "audio/mpeg".to_string()
    } else {
        mime_for(name).to_string()
    };
    let mime_ext = extension(name).to_uppercase();

    // Subtitle lane (§C.2): every matched sidecar and (HLS only) every embedded
    // text track is extracted and exposed *before* the transcode warm-up, so
    // switching a subtitle never competes for the shared libav gate mid-play.
    let mut subtitle_status = SubtitleStatus::default();
    let subtitles = prepare_subtitles(
        handle,
        lan_ip,
        Arc::clone(&driver),
        provider,
        name,
        remote_path,
        &loopback_url,
        ui_lang,
        &mut subtitle_status,
    )
    .await;
    let subtitle_tokens = subtitles.tokens();

    let mut sink_protocols: Vec<String> = Vec::new();
    let mut connection_manager = None;
    if let Ok(client) = ConnectionManagerClient::from_renderer(renderer).await {
        if let Ok(protocols) = client.get_sink_protocol_info().await {
            if !protocols.is_empty() {
                sink_protocols = protocols;
                connection_manager = Some(client);
            }
        }
    }
    let wanted_prefix = if media_kind == "audio" {
        "audio/"
    } else {
        "video/"
    };
    if !sink_protocols.is_empty() && !sink_advertises(&sink_protocols, media_kind) {
        unregister_media(handle, Some(&stream_token), None, &subtitle_tokens);
        return Err(format!(
            "renderer does not advertise {wanted_prefix}* in SinkProtocolInfo"
        ));
    }
    let protocol_info = match connection_manager {
        Some(client) => client.negotiate(&mime, &mime_ext, &sink_protocols),
        None => format!("http-get:*:{mime}:DLNA.ORG_OP=01"),
    };

    // The transcode warm-up starts last: the renderer is only pointed at the
    // playlist once the whole subtitle lane is on the proxy (§C.2).
    let (media_url, hls_token, warming): (String, Option<String>, bool) = if wants_direct {
        (
            format!("http://{lan_ip}:{port}{}", registration.path_template),
            None,
            false,
        )
    } else {
        let Some(preset_id) = preset_id else {
            unregister_media(handle, Some(&stream_token), None, &subtitle_tokens);
            return Err("preset_id required for a transcoded cast".to_string());
        };
        let preset = match global_library_db().get_transcode_preset(preset_id) {
            Ok(Some(preset)) => preset,
            Ok(None) => {
                unregister_media(handle, Some(&stream_token), None, &subtitle_tokens);
                return Err(format!("transcode preset not found: {preset_id}"));
            }
            Err(e) => {
                unregister_media(handle, Some(&stream_token), None, &subtitle_tokens);
                return Err(format!("transcode preset: {e}"));
            }
        };
        // §3: warm-up returns as soon as the job is registered; the renderer
        // is only pointed at the playlist after `cast`.
        let opts = HlsSessionOpts {
            device_id: renderer.usn.clone(),
            input_url: loopback_url.clone(),
            preset,
            ttl_secs: None,
            allowed_peers: None,
            cache: HlsCacheOptions {
                root: cache.root.clone(),
                budget_bytes: Some(cache.budget_bytes),
                account_id: account_id.to_string(),
                remote_path: remote_path.to_string(),
                preset_id: preset_id.to_string(),
                size,
                ident: cache.ident.clone(),
            },
        };
        let registered = if warmup {
            handle.register_hls_nowait(opts).await
        } else {
            handle.register_hls(opts).await
        };
        match registered {
            Ok(hls) => (
                format!("http://{lan_ip}:{port}{}", hls.playlist_template),
                Some(hls.token),
                warmup,
            ),
            Err(e) => {
                unregister_media(handle, Some(&stream_token), None, &subtitle_tokens);
                return Err(format!("hls registration failed: {e}"));
            }
        }
    };

    let subtitle_list: Vec<(String, String)> = subtitles
        .resources
        .iter()
        .map(|resource| (resource.url.clone(), resource.protocol.clone()))
        .collect();
    let didl = build_media_didl(name, media_kind, &media_url, &protocol_info, &subtitle_list);

    Ok(PreparedMedia {
        provider,
        media_kind,
        stream_token,
        loopback_url,
        hls_token,
        media_url,
        mime,
        protocol_info,
        size,
        subtitle_candidates: subtitles.candidates,
        selected_subtitle: subtitles.selected,
        subtitle_resources: subtitles.resources,
        subtitle_tokens,
        embedded_cache: subtitles.embedded_cache,
        subtitle_status,
        didl,
        warming,
    })
}

/// Release every proxy token a prepared/active media set holds.
fn unregister_media(
    handle: &ProxyHandle,
    stream_token: Option<&str>,
    hls_token: Option<&str>,
    subtitle_tokens: &[String],
) {
    if let Some(token) = stream_token {
        handle.unregister(token);
    }
    if let Some(token) = hls_token {
        handle.unregister_hls(token);
    }
    for token in subtitle_tokens {
        handle.unregister(token);
    }
}

// ── Session lifecycle ────────────────────────────────────────────────────────

/// Full server-side start: driver → proxy → protocol negotiation → subtitles →
/// DIDL → AVTransport.
async fn start_session(cfg: &Value) -> Result<Value, String> {
    let renderer_location = str_field(cfg, "renderer_location")?;
    let account_id = str_field(cfg, "account_id")?;
    let remote_path = str_field(cfg, "remote_path")?;
    let renderer_usn = opt_string(cfg, "renderer_usn").unwrap_or_else(|| renderer_location.clone());
    let name = opt_string(cfg, "name").unwrap_or_else(|| file_name(&remote_path));
    let size = cfg.get("size").and_then(Value::as_u64).unwrap_or(0);
    let force_direct = cfg
        .get("force_direct")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let preset_id = opt_string(cfg, "preset_id");
    let ui_lang = opt_string(cfg, "ui_lang").unwrap_or_default();
    // §3: warm-up is the default for HLS casts; `warmup:false` keeps the old
    // (blocking, up to 20 s) behaviour as an emergency switch.
    let warmup = cfg.get("warmup").and_then(Value::as_bool).unwrap_or(true);
    // §3.5: transcode-cache location/budget/identity (all optional).
    let cache = CacheConfig::from_json(cfg);

    let renderer = DlnaRenderer {
        usn: renderer_usn,
        location: renderer_location,
        server: None,
    };

    // Resolve the account first: a bad account id must fail before any network
    // or proxy work happens.
    let driver = create_account_driver(&account_id)?;

    let renderer_ip = renderer_ip_from_location(&renderer.location)?;
    let lan_ip = lan_ip_for(renderer_ip)?;
    let proxy = ensure_lan_proxy(lan_ip).await?;

    let wants_hls = !(force_direct || matches!(preset_id.as_deref(), Some("passthrough")));
    if wants_hls {
        // §7.4: one fresh diagnostics window per HLS session.
        wdmm_ffmpeg::diag::clear();
    }

    let prepared = prepare_media(
        &proxy,
        lan_ip,
        &renderer,
        Arc::clone(&driver),
        &account_id,
        &remote_path,
        &name,
        size,
        preset_id.as_deref(),
        force_direct,
        warmup,
        &ui_lang,
        &cache,
    )
    .await?;

    let mut session = match DlnaSession::open(&renderer).await {
        Ok(session) => session,
        Err(e) => {
            unregister_media(
                &proxy,
                Some(&prepared.stream_token),
                prepared.hls_token.as_deref(),
                &prepared.subtitle_tokens,
            );
            return Err(format!("open renderer failed: {e}"));
        }
    };
    // Warming: do not touch the renderer yet — `cast` does that once the
    // playlist is actually on disk (the renderer would 404 in 46 ms otherwise).
    let phase = if prepared.warming {
        SessionPhase::Warming
    } else {
        if let Err(e) = session.start(&prepared.media_url, &prepared.didl).await {
            unregister_media(
                &proxy,
                Some(&prepared.stream_token),
                prepared.hls_token.as_deref(),
                &prepared.subtitle_tokens,
            );
            return Err(format!("SetAVTransportURI failed: {e}"));
        }
        SessionPhase::Playing
    };

    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    let subtitle_status = prepared.subtitle_status;
    let subtitle_status_json = subtitle_status.to_json();
    let loopback_url = prepared.loopback_url;
    let entry = SessionEntry {
        handle,
        renderer,
        session,
        driver,
        account_id,
        remote_path: remote_path.clone(),
        name: name.clone(),
        size: prepared.size,
        preset_id,
        force_direct,
        ui_lang,
        provider: prepared.provider,
        media_kind: prepared.media_kind,
        proxy,
        lan_ip,
        stream_token: prepared.stream_token,
        loopback_url: loopback_url.clone(),
        hls_token: prepared.hls_token,
        media_url: prepared.media_url.clone(),
        mime: prepared.mime,
        protocol_info: prepared.protocol_info.clone(),
        subtitle_candidates: prepared.subtitle_candidates,
        selected_subtitle: prepared.selected_subtitle,
        subtitle_resources: prepared.subtitle_resources,
        subtitle_tokens: prepared.subtitle_tokens,
        embedded_cache: prepared.embedded_cache,
        subtitle_status,
        phase,
        didl: prepared.didl,
        cache,
        last_error: None,
    };
    put_session(entry);

    Ok(json!({
        "ok": true,
        "handle": handle,
        "url": prepared.media_url,
        "provider": prepared.provider,
        "media_kind": prepared.media_kind,
        "protocol_info": prepared.protocol_info,
        "phase": phase.as_str(),
        "compat": compat_json(&name, prepared.provider),
        "subtitle_status": subtitle_status_json,
    }))
}

/// Apply one `{"cmd":…}` document to a session.
async fn apply_cmd(entry: &mut SessionEntry, cmd: &Value) -> Result<Option<Value>, CmdError> {
    let command = cmd
        .get("cmd")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    match command.as_str() {
        "play" => entry
            .session
            .play()
            .await
            .map(|()| None)
            .map_err(|e| CmdError::plain(e.to_string())),
        "pause" => entry
            .session
            .pause()
            .await
            .map(|()| None)
            .map_err(|e| CmdError::plain(e.to_string())),
        "stop" => stop_session(entry)
            .await
            .map(|()| None)
            .map_err(CmdError::from),
        "seek" => {
            let position_ms = cmd
                .get("position_ms")
                .and_then(Value::as_i64)
                .ok_or_else(|| CmdError::plain("seek requires `position_ms`"))?;
            entry
                .session
                .seek(position_ms)
                .await
                .map(|()| None)
                .map_err(|e| CmdError::plain(e.to_string()))
        }
        "select" => select_media(entry, cmd)
            .await
            .map(|()| None)
            .map_err(CmdError::from),
        "subtitle" => select_subtitle(entry, cmd).await.map(Some),
        "cast" => cast_to_renderer(entry)
            .await
            .map(|()| None)
            .map_err(CmdError::from),
        other => Err(CmdError::plain(format!("unknown cmd: {other}"))),
    }
}

/// Gate for the `cast` command (§3): only a warming session whose playlist is
/// already on disk may point the renderer at it.
fn cast_precondition(phase: SessionPhase, playlist_ready: bool) -> Result<(), &'static str> {
    if phase != SessionPhase::Warming {
        return Err("not warming");
    }
    if !playlist_ready {
        return Err("playlist not ready");
    }
    Ok(())
}

/// Common `stop` phase transition. Returns whether the renderer must also be
/// stopped (it must not when it was never pointed at the media).
fn stop_phase_transition(phase: &mut SessionPhase) -> bool {
    let needs_renderer_stop = matches!(*phase, SessionPhase::Playing | SessionPhase::Ended);
    *phase = SessionPhase::Idle;
    needs_renderer_stop
}

/// `{"cmd":"stop"}`: in warm-up nothing was sent to the renderer yet, so only
/// the transcode job + proxy tokens are released and the phase goes `idle`.
async fn stop_session(entry: &mut SessionEntry) -> Result<(), String> {
    let result = if matches!(entry.phase, SessionPhase::Playing | SessionPhase::Ended) {
        entry.session.stop().await.map_err(|e| e.to_string())
    } else {
        // Warming / failed / idle: cancelling the job and releasing the proxy
        // tokens is the whole stop — do not touch the renderer.
        let hls = entry.hls_token.take();
        let subtitle = std::mem::take(&mut entry.subtitle_tokens);
        entry.subtitle_resources.clear();
        unregister_media(
            &entry.proxy,
            Some(&entry.stream_token),
            hls.as_deref(),
            &subtitle,
        );
        Ok(())
    };
    let _ = stop_phase_transition(&mut entry.phase);
    result
}

/// `{"cmd":"cast"}`: hand the renderer the playlist once it is ready, then play.
async fn cast_to_renderer(entry: &mut SessionEntry) -> Result<(), String> {
    let playlist_ready = entry
        .hls_token
        .as_deref()
        .and_then(|token| entry.proxy.hls_progress(token))
        .is_some_and(|progress| progress.playlist_ready);
    cast_precondition(entry.phase, playlist_ready)?;
    entry
        .session
        .start(&entry.media_url, &entry.didl)
        .await
        .map_err(|e| format!("SetAVTransportURI failed: {e}"))?;
    entry.phase = SessionPhase::Playing;
    Ok(())
}

/// Switch the session to another file (next episode, or a manual pick).
async fn select_media(entry: &mut SessionEntry, cmd: &Value) -> Result<(), String> {
    let remote_path = opt_string(cmd, "remote_path").unwrap_or_else(|| entry.remote_path.clone());
    let name = opt_string(cmd, "name").unwrap_or_else(|| file_name(&remote_path));
    let size = cmd.get("size").and_then(Value::as_u64).unwrap_or(0);
    let preset_id = opt_string(cmd, "preset_id");
    // A `null` preset id means the Dart layer wants a direct cast.
    let force_direct = preset_id.is_none();
    let proxy = entry.proxy.clone();
    let driver = Arc::clone(&entry.driver);
    let lan_ip = entry.lan_ip;
    // §3.5: the cache identity belongs to one file. Switching to another file
    // without an explicit `cache_ident` makes the segments session-local.
    let mut cache = entry.cache.clone();
    if remote_path != entry.remote_path {
        cache.ident = opt_string(cmd, "cache_ident").unwrap_or_default();
    } else if let Some(ident) = opt_string(cmd, "cache_ident") {
        cache.ident = ident;
    }

    let wants_hls = !(force_direct || matches!(preset_id.as_deref(), Some("passthrough")));
    if wants_hls {
        wdmm_ffmpeg::diag::clear();
    }

    // Switching media is synchronous: `switch_to` must only see a ready
    // playlist, so the warm-up path is not used here (the current renderer is
    // already playing — a `warming` phase would stop the UI from knowing).
    let prepared = prepare_media(
        &proxy,
        lan_ip,
        &entry.renderer,
        Arc::clone(&driver),
        &entry.account_id,
        &remote_path,
        &name,
        size,
        preset_id.as_deref(),
        force_direct,
        false,
        &entry.ui_lang,
        &cache,
    )
    .await?;

    if let Err(e) = entry
        .session
        .switch_to(&prepared.media_url, &prepared.didl)
        .await
    {
        unregister_media(
            &proxy,
            Some(&prepared.stream_token),
            prepared.hls_token.as_deref(),
            &prepared.subtitle_tokens,
        );
        return Err(format!("SetAVTransportURI failed: {e}"));
    }

    unregister_media(
        &proxy,
        Some(&entry.stream_token),
        entry.hls_token.as_deref(),
        &entry.subtitle_tokens,
    );
    entry.remote_path = remote_path;
    entry.name = name;
    entry.size = prepared.size;
    entry.preset_id = preset_id;
    entry.force_direct = force_direct;
    entry.provider = prepared.provider;
    entry.media_kind = prepared.media_kind;
    entry.stream_token = prepared.stream_token;
    entry.loopback_url = prepared.loopback_url;
    entry.hls_token = prepared.hls_token;
    entry.media_url = prepared.media_url;
    entry.mime = prepared.mime;
    entry.protocol_info = prepared.protocol_info;
    entry.subtitle_candidates = prepared.subtitle_candidates;
    entry.selected_subtitle = prepared.selected_subtitle;
    entry.subtitle_resources = prepared.subtitle_resources;
    entry.subtitle_tokens = prepared.subtitle_tokens;
    entry.embedded_cache = prepared.embedded_cache;
    entry.subtitle_status = prepared.subtitle_status;
    entry.didl = prepared.didl;
    entry.cache = cache;
    entry.phase = SessionPhase::Playing;
    entry.last_error = None;
    Ok(())
}

/// Failure of a session command.
///
/// `Unsupported` carries the machine-readable reason the app shows for a track
/// it cannot deliver instead of silently doing nothing (contract §5).
#[derive(Debug)]
enum CmdError {
    Plain(String),
    Unsupported {
        message: String,
        support: &'static str,
    },
}

impl CmdError {
    fn plain(message: impl Into<String>) -> Self {
        Self::Plain(message.into())
    }

    fn unsupported(message: impl Into<String>, support: &'static str) -> Self {
        Self::Unsupported {
            message: message.into(),
            support,
        }
    }

    fn json(&self) -> Value {
        match self {
            Self::Plain(message) => json!({"ok": false, "error": message}),
            Self::Unsupported { message, support } => {
                json!({"ok": false, "error": message, "support": support})
            }
        }
    }
}

impl std::fmt::Display for CmdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Plain(message) | Self::Unsupported { message, .. } => f.write_str(message),
        }
    }
}

impl From<String> for CmdError {
    fn from(value: String) -> Self {
        Self::Plain(value)
    }
}

impl From<&str> for CmdError {
    fn from(value: &str) -> Self {
        Self::Plain(value.to_string())
    }
}

/// Turn subtitles off (`path: null`) or switch to a sidecar / embedded track.
///
/// Every deliverable subtitle was prepared and exposed before the transcode
/// warm-up (§C.2), so a switch normally just re-sends the URI. Only a track
/// outside the prepared set falls back to preparing it on the spot.
async fn select_subtitle(entry: &mut SessionEntry, cmd: &Value) -> Result<Value, CmdError> {
    let wanted = opt_string(cmd, "path");
    // A renderer only accepts a new subtitle through a new SetAVTransportURI,
    // which restarts playback: remember the position and jump back afterwards.
    let resume = entry.session.tick().await.ok().map(|tick| tick.position_ms);

    let subtitles: Vec<(String, String)> = match wanted {
        Some(path) => {
            let resource = match find_ready_subtitle(entry, &path) {
                Some(resource) => resource,
                None => select_unprepared_subtitle(entry, &path).await?,
            };
            let pair = (resource.url.clone(), resource.protocol.clone());
            entry.selected_subtitle = Some(resource.candidate.clone());
            vec![pair]
        }
        None => {
            entry.selected_subtitle = None;
            Vec::new()
        }
    };

    let didl = build_session_didl(entry, &subtitles);
    entry.didl = didl.clone();
    entry
        .session
        .start(&entry.media_url, &didl)
        .await
        .map_err(|e| CmdError::plain(e.to_string()))?;
    if let Some(position) = resume.filter(|position| *position > RESUME_POSITION_MS) {
        if let Err(e) = entry.session.seek(position).await {
            wdmm_ffmpeg::diag::log(format!("[subtitle] seek back to {position} ms failed: {e}"));
        }
    }
    Ok(json!({ "ok": true, "resumed_at_ms": resume }))
}

/// The prepared resource for one candidate path, if the pre-warm pass exposed it.
fn find_ready_subtitle(entry: &SessionEntry, path: &str) -> Option<SubtitleResource> {
    if let Some(resource) = entry
        .subtitle_resources
        .iter()
        .find(|resource| resource.candidate.path == path)
    {
        return Some(resource.clone());
    }
    let track = embedded_track_index(path, &entry.remote_path)?;
    entry
        .subtitle_resources
        .iter()
        .find(|resource| resource.candidate.embedded && resource.candidate.track == Some(track))
        .cloned()
}

/// Fallback for a track outside the prepared set (e.g. a sidecar that appeared
/// after the cast started). This is the only path that can still prepare a
/// subtitle during playback.
async fn select_unprepared_subtitle(
    entry: &mut SessionEntry,
    path: &str,
) -> Result<SubtitleResource, CmdError> {
    let track = embedded_track_index(path, &entry.remote_path);
    let resource = match track {
        Some(track) => select_embedded_subtitle(entry, track).await?,
        None => select_sidecar_subtitle(entry, path).await?,
    };
    entry.subtitle_tokens.push(resource.token.clone());
    entry.subtitle_resources.push(resource.clone());
    Ok(resource)
}

/// Expose a real sidecar file as a renderer-selectable external subtitle.
async fn select_sidecar_subtitle(
    entry: &mut SessionEntry,
    path: &str,
) -> Result<SubtitleResource, CmdError> {
    let driver = Arc::clone(&entry.driver);
    let candidate = entry
        .subtitle_candidates
        .iter()
        .find(|candidate| candidate.path == path)
        .cloned()
        .unwrap_or_else(|| {
            let matched = match_subtitle_name(&entry.name, &file_name(path));
            SubtitleCandidate::sidecar(
                file_name(path),
                path.to_string(),
                matched
                    .as_ref()
                    .map(|m| m.lang_key.clone())
                    .unwrap_or_default(),
                matched.and_then(|m| m.language),
                false,
                0,
            )
        });
    if candidate.is_bitmap() {
        return Err(CmdError::unsupported(
            "bitmap subtitle sidecar cannot be delivered to a DLNA renderer",
            "bitmap-unsupported",
        ));
    }
    let index = entry.subtitle_resources.len();
    let resource = expose_subtitle(
        &entry.proxy,
        driver,
        entry.lan_ip,
        &entry.name,
        &candidate,
        index,
    )
    .await
    .map_err(|e| CmdError::plain(format!("subtitle failed: {e}")))?;
    entry.selected_subtitle = Some(candidate);
    Ok(resource)
}

/// Expose one embedded text track as WebVTT (cached when already extracted).
async fn select_embedded_subtitle(
    entry: &mut SessionEntry,
    track: u32,
) -> Result<SubtitleResource, CmdError> {
    let candidate = match entry
        .subtitle_candidates
        .iter()
        .find(|candidate| candidate.track == Some(track) && candidate.embedded)
        .cloned()
    {
        Some(candidate) => candidate,
        None => {
            // The pre-warm pass normally listed every track; a stale app list
            // (or a direct cast) can still name one we have not probed.
            let loopback = entry.loopback_url.clone();
            let probe = tokio::task::spawn_blocking(move || {
                wdmm_ffmpeg::probe(&loopback).map_err(|e| e.to_string())
            })
            .await
            .map_err(|e| CmdError::plain(format!("subtitle probe failed: {e}")))?
            .map_err(|e| CmdError::plain(format!("subtitle probe failed: {e}")))?;
            let stream = probe
                .subtitles
                .iter()
                .find(|stream| stream.index == track)
                .ok_or_else(|| CmdError::plain(format!("unknown subtitle track {track}")))?;
            let candidate = SubtitleCandidate::embedded(
                &entry.remote_path,
                stream.index,
                stream.codec.clone(),
                stream.language.as_deref(),
                stream.title.as_deref(),
            );
            entry
                .subtitle_status
                .apply_probe(&probe.subtitles, entry.provider);
            entry.subtitle_candidates.push(candidate.clone());
            candidate
        }
    };
    if candidate.is_bitmap() {
        return Err(CmdError::unsupported(
            "bitmap subtitle track (PGS/DVD/DVB) cannot be delivered over HLS: \
             the transcode pipeline renders text subtitles only",
            "bitmap-unsupported",
        ));
    }
    let track = candidate
        .track
        .ok_or_else(|| CmdError::plain("embedded subtitle candidate has no track index"))?;
    let data = match entry.embedded_cache.get(&track) {
        Some(cached) => Arc::clone(cached),
        None => {
            // Extraction decodes the whole source stream: hold the shared gate
            // for the duration (contract §3.6).
            let gate = entry.proxy.job_gate();
            let loopback = entry.loopback_url.clone();
            let extracted = tokio::task::spawn_blocking(move || {
                let _permit = gate
                    .acquire(JOB_ACQUIRE_TIMEOUT)
                    .map_err(|e| format!("transcoding busy: {e}"))?;
                let mut tracks = wdmm_ffmpeg::extract_subtitle_tracks(&loopback, &[track])
                    .map_err(|e| e.to_string())?;
                Ok::<Option<String>, String>(tracks.remove(&track))
            })
            .await
            .map_err(|e| CmdError::plain(format!("subtitle extraction failed: {e}")))?;
            let vtt = match extracted {
                Ok(Some(vtt)) => vtt,
                other => {
                    let reason = match other {
                        Err(e) => e,
                        _ => format!("subtitle track {track} produced no cues"),
                    };
                    // Busy/timed-out extraction is reported against this track
                    // (`support:"error"`), never by silently running a second
                    // libav job in parallel.
                    wdmm_ffmpeg::diag::log(format!("[subtitle] extraction failed: {reason}"));
                    if let Some(stale) = entry
                        .subtitle_candidates
                        .iter_mut()
                        .find(|candidate| candidate.track == Some(track))
                    {
                        stale.support = "error";
                    }
                    return Err(CmdError::plain(format!(
                        "subtitle extraction failed: {reason}"
                    )));
                }
            };
            let data = Arc::new(vtt.into_bytes());
            entry.embedded_cache.insert(track, Arc::clone(&data));
            data
        }
    };
    if data.len() > MAX_SUBTITLE_BYTES {
        return Err(CmdError::plain(format!(
            "subtitle too large ({} bytes > {MAX_SUBTITLE_BYTES})",
            data.len()
        )));
    }
    let protocol = subtitle_protocol_info("vtt")
        .ok_or_else(|| CmdError::plain("no protocol info for text/vtt"))?;
    let index = entry.subtitle_resources.len();
    let resource = expose_subtitle_data(
        &entry.proxy,
        entry.lan_ip,
        &entry.name,
        &candidate,
        index,
        data,
        protocol,
    )
    .await
    .map_err(|e| CmdError::plain(format!("subtitle failed: {e}")))?;
    entry.selected_subtitle = Some(candidate);
    Ok(resource)
}

/// Recompute the cast phase from the HLS job telemetry + renderer tick.
///
/// Returns the fresh [`HlsProgress`] (if any) so [`state_json`] can expose the
/// encoder/diagnostics fields without a second lock acquisition.
fn refresh_phase(entry: &mut SessionEntry, tick: &wdmm_dlna::TickResult) -> Option<HlsProgress> {
    let progress = entry
        .hls_token
        .as_deref()
        .and_then(|token| entry.proxy.hls_progress(token));
    if entry.provider == "hls" {
        if let Some(progress) = &progress {
            if let Some(error) = &progress.error {
                // §3 hard requirement: a failed transcode surfaces immediately,
                // never "thread alive, UI waits 20 s".
                entry.phase = SessionPhase::Failed;
                entry.last_error = Some(error.clone());
            } else if entry.phase == SessionPhase::Warming
                && progress.ended
                && !progress.playlist_ready
            {
                entry.phase = SessionPhase::Failed;
                entry.last_error =
                    Some("transcode job exited before writing the HLS playlist".to_string());
            }
        }
    }
    match entry.phase {
        SessionPhase::Warming | SessionPhase::Failed | SessionPhase::Idle => {}
        SessionPhase::Playing | SessionPhase::Ended => {
            entry.phase = if tick.ended {
                SessionPhase::Ended
            } else {
                SessionPhase::Playing
            };
        }
    }
    progress
}

/// `null` until the job has been running for a stable window, then whether the
/// encoder keeps up with realtime (§3 `estimated_realtime`).
///
/// The window used to be "produced at least 3 s of media", but the proxy now
/// reports the *absolute* buffered front (`encode_content_ms`), which is minutes
/// long the moment a seek re-anchors a run. Keying on the wall clock keeps the
/// panel from flashing "slower than realtime" right after every seek.
fn estimate_realtime(wall_ms: u64, speed: f64) -> Option<bool> {
    (wall_ms >= 2_000).then_some(speed >= 1.0)
}

/// Full state document for one tick.
fn state_json(
    entry: &SessionEntry,
    tick: &wdmm_dlna::TickResult,
    progress: Option<&HlsProgress>,
) -> Value {
    let ended_reason = match tick.ended_reason {
        Some(reason) => Value::String(reason.to_string()),
        None => Value::Null,
    };
    let encoder = progress.map(|p| p.encoder.clone()).unwrap_or_default();
    let speed = progress.map_or(0.0, |p| p.speed);
    let content_ms = progress.map_or(0, |p| p.content_ms);
    let wall_ms = progress.map_or(0, |p| p.wall_ms);
    let playlist_ready = progress.is_some_and(|p| p.playlist_ready);
    let segments = progress.map_or(0, |p| p.segments);
    // Only meaningful once the job has been running for a stable window.
    let estimated_realtime = estimate_realtime(wall_ms, speed);
    let warnings = progress.map(|p| p.warnings.clone()).unwrap_or_default();
    let duration_ms = vod_duration_ms(entry.provider, progress, tick.duration_ms);
    let mut doc = json!({
        "ok": true,
        "state": if tick.ended { "ended" } else { "casting" },
        "transport": tick.transport,
        "position_ms": tick.position_ms,
        "duration_ms": duration_ms,
        "current_path": entry.remote_path,
        "current_name": entry.name,
        "provider": entry.provider,
        "media_kind": entry.media_kind,
        "protocol_info": entry.protocol_info,
        "ended": tick.ended,
        "ended_reason": ended_reason,
        "subtitle_candidates": entry
            .subtitle_candidates
            .iter()
            .map(SubtitleCandidate::to_json)
            .collect::<Vec<_>>(),
        "selected_subtitle": entry.selected_subtitle.as_ref().map(SubtitleCandidate::to_json),
        "subtitle_status": entry.subtitle_status.to_json(),
        "compat": compat_json(&entry.name, entry.provider),
        // ── contract §3 / §7.3 ──────────────────────────────────────────────
        "phase": entry.phase.as_str(),
        "encode_speed": speed,
        "encode_content_ms": content_ms,
        "encode_wall_ms": wall_ms,
        "playlist_ready": playlist_ready,
        "segments": segments,
        "estimated_realtime": estimated_realtime,
        "error": entry.last_error,
        "encode_mode": encoder.mode,
        "video_encoder": encoder.video,
        "audio_encoder": encoder.audio,
        "encode_warnings": warnings,
        "diagnostics": wdmm_ffmpeg::diag::tail(40),
    });
    // ── contract §3.3–§3.6 VOD scheduler + cache telemetry ──────────────────
    if let (Value::Object(doc), Value::Object(vod)) = (&mut doc, vod_telemetry(progress)) {
        doc.extend(vod);
    }
    doc
}

/// `duration_ms` for one state tick (contract §4).
///
/// An HLS cast owns its duration from the segment plan — the renderer's
/// `CurrentTrackDuration` is 0 while the playlist is still warming. A direct
/// cast keeps the renderer's value verbatim.
fn vod_duration_ms(provider: &str, progress: Option<&HlsProgress>, tick_ms: i64) -> i64 {
    if provider == "hls" {
        progress.and_then(|p| p.plan_duration_ms).unwrap_or(tick_ms)
    } else {
        tick_ms
    }
}

/// VOD scheduler + transcode-cache telemetry (contract §3.3–§3.6, §4).
///
/// A direct cast has no transcode job, so every counter falls back to a neutral
/// value there (`run_state:"idle"`, `alignment:null`, zeros).
fn vod_telemetry(progress: Option<&HlsProgress>) -> Value {
    let run_state = progress.map_or("idle", |p| p.run_state);
    let alignment = progress
        .and_then(|p| p.alignment)
        .map_or(Value::Null, Value::from);
    let drift_ms = progress
        .and_then(|p| p.drift_ms)
        .map_or(Value::Null, Value::from);
    let ready_segments = progress.map_or(0, |p| p.segments as u64);
    let seg_count = progress.map_or(0, |p| p.segment_count as u64);
    let seg_secs = progress.map_or(0.0, |p| p.seg_secs);
    // The single session job is paused once it is this far ahead of the newest
    // request, so this is the real window between the ready front and the
    // request front.
    let lookahead_segments = if progress.is_some() {
        AHEAD_SEGMENTS
    } else {
        0
    };
    let cache_hits = progress.map_or(0, |p| p.cache_hits);
    let cache_bytes = progress.map_or(0, |p| p.cache_bytes);
    let cache_over_budget = progress.is_some_and(|p| p.over_budget);
    json!({
        "run_state": run_state,
        "alignment": alignment,
        "drift_ms": drift_ms,
        "ready_segments": ready_segments,
        "seg_count": seg_count,
        "seg_secs": seg_secs,
        "lookahead_segments": lookahead_segments,
        "cache_hits": cache_hits,
        "cache_bytes": cache_bytes,
        "cache_over_budget": cache_over_budget,
    })
}

// ── FFI: session facade ──────────────────────────────────────────────────────

/// Start a DLNA cast session.
///
/// `config_json` (UTF-8, null-terminated):
/// ```json
/// {"renderer_location":"http://…/desc.xml","renderer_usn":"uuid:…",
///  "account_id":"…","remote_path":"/dir/movie.mkv","name":"movie.mkv",
///  "size":123,"preset_id":null,"force_direct":true,"warmup":true,"ui_lang":"zh",
///  "cache_dir":null,"cache_budget_mb":2048,"cache_ident":"<etag|mtime>"}
/// ```
///
/// The optional transcode-cache keys (contract §3.5) map to
/// `HlsCacheOptions`: `cache_dir` (default `<temp>/wdmm-transcode`),
/// `cache_budget_mb` (default 2048 MiB) and `cache_ident` (etag or mtime; an
/// absent value keeps the segments session-local, never reused across casts).
///
/// `warmup` defaults to `true`: an HLS cast then starts the transcode and
/// returns immediately with `"phase":"warming"`; the caller polls
/// [`wdmm_dlna_session_state`] and sends `{"cmd":"cast"}` once
/// `playlist_ready` is true. `"warmup":false` blocks until the proxy has the
/// playlist (bounded by the proxy's own wait). Direct casts ignore `warmup` and
/// always return `"phase":"playing"`.
///
/// `"media_kind"` is derived from the file extension alone (task-20): audio
/// files are announced with an `audio/*` protocolInfo and an
/// `object.item.audioItem.musicTrack` DIDL item, so they are never rejected by
/// the renderer's `video/*` sink gate.
///
/// The whole subtitle lane is prepared *before* the transcode warm-up (§C.2):
/// every auto-matched sidecar is exposed and, for HLS, every embedded text
/// track is extracted in one libav pass. The DIDL therefore carries one `<res>`
/// per deliverable subtitle from the first second, which is what lets the
/// renderer offer them as selectable streams without a second
/// `SetAVTransportURI`.
///
/// Returns `{"ok":true,"handle":N,"url":"…","provider":"direct"|"hls",
/// "media_kind":"video"|"audio","protocol_info":"…","phase":"playing"|"warming",
/// "compat":{…},"subtitle_status":{…}}` or
/// `{"ok":false,"error":"…"}`. Caller must free the result via
/// [`crate::wdmm_ffi_free_string`].
///
/// # Safety
/// `config_json` must be a valid null-terminated UTF-8 C string (or null).
#[no_mangle]
pub extern "C" fn wdmm_dlna_session_start(config_json: *const c_char) -> FfiString {
    let raw = match ptr_to_str(config_json) {
        Some(raw) => raw,
        None => return err_json("invalid config_json"),
    };
    let config: Value = match serde_json::from_str(raw) {
        Ok(config) => config,
        Err(e) => return err_json(format!("invalid config_json: {e}")),
    };
    match rt().block_on(start_session(&config)) {
        Ok(value) => FfiString::from(value.to_string()),
        Err(e) => err_json(e),
    }
}

/// Poll one tick of a session and return its state document.
///
/// Returns `{"ok":true,"state":…,"transport":…,"position_ms":…,
/// "duration_ms":…,"ended":bool,"ended_reason":"eof"|"stopped_by_device"|null,
/// "subtitle_candidates":[…],"selected_subtitle":{…}|null,
/// "subtitle_status":{…},"compat":{…}}` plus
/// the contract §3 warm-up keys:
/// `phase` (`warming`/`playing`/`ended`/`failed`/`idle`), `encode_speed`
/// (0.0 when direct/unknown), `encode_content_ms`, `encode_wall_ms`,
/// `playlist_ready`, `segments`, `estimated_realtime` (bool|null),
/// `error` (string|null, set when `phase == "failed"`), and the §7.3
/// diagnostics keys `encode_mode` (`remux`/`hybrid`/`hw`/`sw`),
/// `video_encoder`, `audio_encoder`, `encode_warnings`, `diagnostics`.
///
/// Every subtitle candidate also carries `embedded` (bool), `track`
/// (AV stream index or null), `codec` (libav codec name or null),
/// `language` (raw ISO 639-2 tag from the source, or the lowercased sidecar
/// file-name suffix, or null), `title` (source track title or null) and
/// `support` (`"external"`, `"webvtt"`, `"bitmap-unsupported"` or `"error"`).
/// `media_kind` is `"video"` or `"audio"` and is repeated on every tick.
/// `subtitle_status` is `{text_tracks, bitmap_tracks, bitmap_note, message}`:
/// `bitmap_note` is non-null only for an HLS cast with bitmap tracks (they are
/// dropped — text subtitles only), and `message` explains a failed/timed-out
/// sidecar scan or a skipped embedded probe. All keys are additive.
///
/// The VOD scheduler / cache telemetry from contract §3.3–§3.6 is additive
/// too: `run_state` (`"active"`/`"idle"`/`"error"`), `alignment`
/// (`"exact"`/`"keyframe"`/`"calibrated"`/`"approx"` or null), `drift_ms`
/// (int or null), `ready_segments`, `seg_count`, `seg_secs`,
/// `lookahead_segments`, `cache_hits`, `cache_bytes` and `cache_over_budget`.
/// For an HLS cast `duration_ms` comes from the segment plan (a warming
/// renderer reports 0); a direct cast keeps the renderer's value.
/// Returns `{"ok":false,"error":"…"}` for an unknown handle / renderer failure.
#[no_mangle]
pub extern "C" fn wdmm_dlna_session_state(handle: u32) -> FfiString {
    let Some(mut entry) = take_session(handle) else {
        return err_json("unknown session handle");
    };
    let result = rt().block_on(entry.session.tick());
    let value = match result {
        Ok(tick) => {
            let progress = refresh_phase(&mut entry, &tick);
            state_json(&entry, &tick, progress.as_ref())
        }
        Err(e) => json!({"ok": false, "error": e.to_string()}),
    };
    put_session(entry);
    FfiString::from(value.to_string())
}

/// Send a user-level command to a session.
///
/// `cmd_json` forms: `{"cmd":"play"}`, `{"cmd":"pause"}`, `{"cmd":"stop"}`,
/// `{"cmd":"seek","position_ms":60000}`,
/// `{"cmd":"select","remote_path":"/dir/b.mkv","name":"b.mkv","size":0,
/// "preset_id":null}`,
/// `{"cmd":"subtitle","path":"/dir/b.zh.srt"}` (sidecar),
/// `{"cmd":"subtitle","path":"/dir/b.mkv#embedded:2"}` (embedded text track of
/// the current media path; `path:null` turns subtitles off) and
/// `{"cmd":"cast"}`.
///
/// `cast` is only valid while `phase == "warming"` and `playlist_ready` is
/// true; it sends the playlist URL to the renderer and moves the phase to
/// `playing`. While warming, `stop` cancels the transcode job without touching
/// the renderer and moves the phase to `idle`.
///
/// Returns `"ok"` on success. `subtitle` returns
/// `{"ok":true,"resumed_at_ms":12345}` (the position it jumped back to, `null`
/// when playback had barely started). A bitmap embedded track is refused loudly
/// with `{"ok":false,"error":"…","support":"bitmap-unsupported"}`; every other
/// failure returns `{"ok":false,"error":"…"}`.
#[no_mangle]
pub extern "C" fn wdmm_dlna_session_cmd(handle: u32, cmd_json: *const c_char) -> FfiString {
    let raw = match ptr_to_str(cmd_json) {
        Some(raw) => raw,
        None => return err_json("invalid cmd_json"),
    };
    let cmd: Value = match serde_json::from_str(raw) {
        Ok(cmd) => cmd,
        Err(e) => return err_json(format!("invalid cmd_json: {e}")),
    };
    let Some(mut entry) = take_session(handle) else {
        return err_json("unknown session handle");
    };
    let result = rt().block_on(apply_cmd(&mut entry, &cmd));
    put_session(entry);
    match result {
        Ok(Some(doc)) => FfiString::from(doc.to_string()),
        Ok(None) => FfiString::from("ok"),
        Err(e) => FfiString::from(e.json().to_string()),
    }
}

/// Close a session: stop playback and release every proxy token it holds.
///
/// Returns `"ok"`, or `{"ok":false,"error":"…"}` for an unknown handle.
#[no_mangle]
pub extern "C" fn wdmm_dlna_session_close(handle: u32) -> FfiString {
    let Some(mut entry) = take_session(handle) else {
        return err_json("unknown session handle");
    };
    let _ = rt().block_on(entry.session.stop());
    let subtitle = std::mem::take(&mut entry.subtitle_tokens);
    unregister_media(
        &entry.proxy,
        Some(&entry.stream_token),
        entry.hls_token.as_deref(),
        &subtitle,
    );
    FfiString::from("ok")
}

// ── FFI: renderer protocol info ──────────────────────────────────────────────

/// Query a renderer's ConnectionManager `GetProtocolInfo` without casting.
///
/// `renderer_location` — the renderer's device-description URL (the SSDP
/// `<LOCATION>` a discovery result carries). No session, proxy or account is
/// touched, so the UI can ask "would this renderer accept the file?" before
/// starting a cast.
///
/// Returns `{"ok":true,"source":"…","sink":"…","sink_count":N}`:
/// * `source` — the renderer's raw `<Source>` protocol list (empty on
///   MediaRenderer-only devices such as Kodi),
/// * `sink` — the renderer's `<Sink>` list as a comma-separated string
///   (trimmed, empty entries dropped),
/// * `sink_count` — number of sink entries,
///
/// or `{"ok":false,"error":"…"}` when the description cannot be fetched, the
/// device exposes no ConnectionManager:1 service, or the SOAP call fails.
/// Caller must free the result via [`crate::wdmm_ffi_free_string`].
///
/// # Safety
/// `renderer_location` must be a valid null-terminated UTF-8 C string (or null).
#[no_mangle]
pub extern "C" fn wdmm_dlna_sink_protocol_info_json(renderer_location: *const c_char) -> FfiString {
    let Some(location) = ptr_to_str(renderer_location).filter(|value| !value.is_empty()) else {
        return err_json("invalid renderer_location");
    };
    let renderer = DlnaRenderer {
        usn: location.to_string(),
        location: location.to_string(),
        server: None,
    };
    match rt().block_on(read_sink_protocol_info(&renderer)) {
        Ok(value) => FfiString::from(value.to_string()),
        Err(e) => err_json(e),
    }
}

/// Fetch and shape a renderer's `GetProtocolInfo` document.
async fn read_sink_protocol_info(renderer: &DlnaRenderer) -> Result<Value, String> {
    let client = ConnectionManagerClient::from_renderer(renderer)
        .await
        .map_err(|e| e.to_string())?;
    let (source, sink) = client
        .get_protocol_info()
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "ok": true,
        "source": source,
        "sink": sink.join(","),
        "sink_count": sink.len(),
    }))
}

// ── FFI: extended discovery ──────────────────────────────────────────────────

/// Discover DLNA renderers and enrich each entry with its device description.
///
/// `timeout_ms` — SSDP window in milliseconds (clamped to 200–10000).
///
/// Returns a JSON array:
/// ```json
/// [{"usn":"uuid:…","location":"http://…","server":"…",
///   "friendly_name":"Living room","has_avtransport":true,
///   "model_name":null,"manufacturer":null,"model_number":null}]
/// ```
/// `has_avtransport` defaults to `true` when the description cannot be fetched
/// (Dart then still offers the device instead of silently hiding it). Returns
/// `"error:<message>"` when SSDP discovery itself fails.
#[no_mangle]
pub extern "C" fn wdmm_dlna_discover_ext_json(timeout_ms: u32) -> FfiString {
    let timeout = (timeout_ms as u64).clamp(DISCOVER_MIN_MS, DISCOVER_MAX_MS);
    match rt().block_on(discover_ext(timeout)) {
        Ok(renderers) => {
            FfiString::from(serde_json::to_string(&renderers).unwrap_or_else(|_| "[]".into()))
        }
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

/// SSDP discovery + parallel device-description fetches.
async fn discover_ext(timeout_ms: u64) -> Result<Vec<Value>, String> {
    use std::time::Duration;

    let renderers = SsdpDiscovery::new()
        .discover(Duration::from_millis(timeout_ms))
        .await
        .map_err(|e| e.to_string())?;

    // One HTTP GET per renderer, all in flight at once.
    let descriptions = futures::future::join_all(
        renderers
            .iter()
            .map(|renderer| describe_renderer(&renderer.location)),
    )
    .await;

    let mut out = Vec::with_capacity(renderers.len());
    for (renderer, description) in renderers.iter().zip(descriptions) {
        let description = description.ok();
        out.push(json!({
            "usn": renderer.usn,
            "location": renderer.location,
            "server": renderer.server,
            "friendly_name": description.as_ref().and_then(|d| d.friendly_name.clone()),
            "has_avtransport": description.as_ref().map(|d| d.has_avtransport).unwrap_or(true),
            "model_name": description.as_ref().and_then(|d| d.model_name.clone()),
            "manufacturer": description.as_ref().and_then(|d| d.manufacturer.clone()),
            "model_number": description.as_ref().and_then(|d| d.model_number.clone()),
        }));
    }
    Ok(out)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    /// Read and free an `FfiString`.
    fn read(result: FfiString) -> String {
        let text = unsafe { CStr::from_ptr(result.ptr) }
            .to_str()
            .expect("utf-8 FFI string")
            .to_string();
        crate::wdmm_ffi_free_string(result);
        text
    }

    fn json_of(result: FfiString) -> Value {
        serde_json::from_str(&read(result)).expect("json result")
    }

    #[test]
    fn mime_and_extension_mapping() {
        assert_eq!(mime_for("a.MKV"), "video/x-matroska");
        assert_eq!(mime_for("a.mp4"), "video/mp4");
        assert_eq!(mime_for("a.ts"), "video/mp2t");
        assert_eq!(mime_for("a.bin"), "video/mp4");
        assert_eq!(extension("a.B.MKV"), "mkv");
        assert_eq!(extension("noext"), "");
        assert_eq!(extension("trailing."), "");
    }

    #[test]
    fn path_helpers() {
        assert_eq!(file_name("/a/b/c.mkv"), "c.mkv");
        assert_eq!(file_name("/a/b/"), "b");
        assert_eq!(parent_dir("/a/b/c.mkv"), "/a/b");
        assert_eq!(parent_dir("/c.mkv"), "/");
        assert_eq!(parent_dir("c.mkv"), "/");
        assert_eq!(parent_dir("/a/b/"), "/a");
        assert_eq!(join_path("/", "c.mkv"), "/c.mkv");
        assert_eq!(join_path("/a/b", "c.mkv"), "/a/b/c.mkv");
        assert_eq!(join_path("/a/b/", "c.mkv"), "/a/b/c.mkv");
        assert_eq!(video_stem("Show.S01E01.mkv"), "Show.S01E01");
        assert_eq!(video_stem("Show.S01E01.zh.srt"), "Show.S01E01.zh.srt");
    }

    #[test]
    fn subtitle_language_normalisation() {
        assert_eq!(display_subtitle_language("zh"), "zh");
        assert_eq!(display_subtitle_language("zh-CN"), "zh");
        assert_eq!(display_subtitle_language("CHS"), "zh");
        assert_eq!(display_subtitle_language("cht"), "zh");
        assert_eq!(display_subtitle_language("zh_Hant_TW"), "zh");
        assert_eq!(display_subtitle_language("eng"), "en");
        assert_eq!(display_subtitle_language("en-US"), "en");
        assert_eq!(display_subtitle_language("und"), "");
        assert_eq!(display_subtitle_language("ja"), "ja");
    }

    /// `(language key, raw language, exact)` for the assertions below.
    fn matched(video: &str, subtitle: &str) -> Option<(String, Option<String>, bool)> {
        match_subtitle_name(video, subtitle)
            .map(|matched| (matched.lang_key, matched.language, matched.exact))
    }

    #[test]
    fn matches_sidecar_subtitles() {
        assert_eq!(
            matched("Movie.mkv", "Movie.srt"),
            Some((String::new(), None, true))
        );
        assert_eq!(
            matched("Movie.mkv", "Movie.zh.srt"),
            Some(("zh".to_string(), Some("zh".to_string()), false))
        );
        assert_eq!(
            matched("Movie.mkv", "movie.ZH-Hans.ass"),
            Some(("zh".to_string(), Some("zh-hans".to_string()), false))
        );
        assert_eq!(
            matched("Movie.mkv", "Movie.chi.vtt"),
            Some(("zh".to_string(), Some("chi".to_string()), false))
        );
        assert_eq!(
            matched("Movie.mkv", "Movie.en-US.vtt"),
            Some(("en".to_string(), Some("en-us".to_string()), false))
        );
        assert_eq!(match_subtitle_name("Movie.mkv", "Other.srt"), None);
        assert_eq!(match_subtitle_name("Movie.mkv", "Movie.idx"), None);
        assert_eq!(match_subtitle_name("Movie.mkv", "Movie.mkv"), None);
        assert_eq!(match_subtitle_name("Movie.mkv", "Movie..srt"), None);
    }

    fn candidate(name: &str, lang: &str, exact: bool) -> SubtitleCandidate {
        SubtitleCandidate::sidecar(
            name.to_string(),
            format!("/v/{name}"),
            lang.to_string(),
            (!lang.is_empty()).then(|| lang.to_string()),
            exact,
            10,
        )
    }

    #[test]
    fn picks_exact_then_ui_language() {
        let candidates = vec![
            candidate("Movie.fr.srt", "fr", false),
            candidate("Movie.zh.srt", "zh", false),
            candidate("Movie.srt", "", true),
        ];
        // Exact name wins even when the UI language differs.
        assert_eq!(pick_subtitle(&candidates, "fr"), Some(2));
        let no_exact = &candidates[..2];
        assert_eq!(pick_subtitle(no_exact, "zh-CN"), Some(1));
        assert_eq!(pick_subtitle(no_exact, "de"), Some(0));
        assert_eq!(pick_subtitle(&[], "zh"), None);
    }

    #[test]
    fn serves_memory_range() {
        let data = Arc::new((0..100u8).collect::<Vec<u8>>());
        let collect = |start: i64, end: i64| {
            rt().block_on(async {
                use futures::StreamExt;
                let stream = mem_range_stream(Arc::clone(&data), start, end);
                stream.collect::<Vec<_>>().await
            })
        };
        // Whole file, EOF marker, explicit range, clamped tail.
        let all: Vec<u8> = collect(0, -1)
            .into_iter()
            .flat_map(|chunk| chunk.expect("chunk").to_vec())
            .collect();
        assert_eq!(all, Vec::<u8>::from_iter(0..100));
        let first = collect(0, 9).pop().unwrap().expect("chunk");
        assert_eq!(first.len(), 10);
        assert_eq!(first[0], 0);
        assert_eq!(first[9], 9);
        let tail = collect(90, 200).pop().unwrap().expect("chunk");
        assert_eq!(tail.len(), 10);
        assert_eq!(tail[9], 99);
        // Out-of-range and empty data produce no chunks.
        assert!(collect(200, 300).is_empty());
        let empty = Arc::new(Vec::<u8>::new());
        let stream = mem_range_stream(empty, 0, -1);
        assert!(rt().block_on(async {
            use futures::StreamExt;
            stream.collect::<Vec<_>>().await.is_empty()
        }));
    }

    #[test]
    fn compat_hints() {
        assert_eq!(compat_json("a.mp4", "direct")["requires_transcode"], false);
        assert_eq!(compat_json("a.mkv", "direct")["requires_transcode"], true);
        assert!(compat_json("a.mkv", "direct")["reason"]
            .as_str()
            .unwrap()
            .contains("container"));
        assert!(compat_json("a.rmvb", "direct")["reason"]
            .as_str()
            .unwrap()
            .contains("video"));
        assert_eq!(compat_json("a.mp4", "hls")["requires_transcode"], true);
    }

    #[test]
    fn didl_lists_every_prepared_subtitle() {
        let plain = build_didl("t", "http://h/a.mkv", "proto", &[]);
        assert_eq!(plain.matches("<res ").count(), 1);
        let srt = subtitle_protocol_info("srt").unwrap();
        let vtt = subtitle_protocol_info("vtt").unwrap();
        let subtitles = vec![
            ("http://h/a.zh.srt".to_string(), srt.clone()),
            ("http://h/a.vtt".to_string(), vtt.clone()),
        ];
        let with_subs = build_didl("t", "http://h/a.mkv", "proto", &subtitles);
        assert_eq!(with_subs.matches("<res ").count(), 3);
        // Order is preserved: the renderer numbers the streams in document order.
        let first = with_subs.find("a.zh.srt").unwrap();
        let second = with_subs.find("a.vtt").unwrap();
        assert!(first < second);
        // Channel (b) evidence: the MIME subtype must be the real format, or
        // Kodi's UPnP renderer silently ignores the resource.
        assert!(with_subs.contains("http-get:*:text/srt:*"));
        assert!(with_subs.contains("http-get:*:text/vtt:*"));
        assert!(!with_subs.contains("text/plain"));
    }

    #[test]
    fn subtitle_url_name_follows_the_video_name() {
        // `.zh.chi.Signs.ass` -> stem + language + title + real format, which is
        // what Kodi's filename-based stream list parses.
        let mut candidate = SubtitleCandidate::sidecar(
            "a.zh.chi.Signs.ass".to_string(),
            "/v/a.zh.chi.Signs.ass".to_string(),
            "zh".to_string(),
            Some("chi".to_string()),
            false,
            0,
        );
        candidate.title = Some("Signs".to_string());
        assert_eq!(subtitle_url_name("a.mkv", &candidate, 0), "a.chi.Signs.ass");
        // No language, no title: a stable ordinal keeps the names unique.
        let plain = SubtitleCandidate::sidecar(
            "a.srt".to_string(),
            "/v/a.srt".to_string(),
            String::new(),
            None,
            false,
            0,
        );
        assert_eq!(subtitle_url_name("a.mkv", &plain, 2), "a.3.srt");
        // Embedded tracks are always WebVTT.
        let embedded = SubtitleCandidate::embedded("/v/a.mkv", 3, "subrip".to_string(), None, None);
        assert_eq!(subtitle_url_name("a.mkv", &embedded, 0), "a.1.vtt");
        // Sanitising keeps the extension intact.
        let dirty = SubtitleCandidate::sidecar(
            "a.srt".to_string(),
            "/v/a.srt".to_string(),
            String::new(),
            Some("Bad<>:\"/\\|?*Name".to_string()),
            false,
            0,
        );
        assert_eq!(subtitle_url_name("a.mkv", &dirty, 0), "a.BadName.srt");
    }

    #[test]
    fn media_kind_follows_the_extension() {
        assert_eq!(media_kind("a.mp3"), "audio");
        assert_eq!(media_kind("song.FLAC"), "audio");
        assert_eq!(media_kind("track.m4a"), "audio");
        assert_eq!(media_kind("a.mka"), "audio");
        assert_eq!(media_kind("a.mkv"), "video");
        assert_eq!(media_kind("a.MP4"), "video");
        assert_eq!(media_kind("no-extension"), "video");
        // A query/fragment left on the name must not change the class.
        assert_eq!(media_kind("a.mp3?token=1"), "audio");
        assert_eq!(media_kind("a.flac#frag"), "audio");
        // Audio extensions get audio MIME types even for exotic formats.
        assert_eq!(mime_for("a.ape"), "audio/x-ape");
        assert_eq!(mime_for("a.wv"), "audio/x-wavpack");
        assert_eq!(mime_for("a.mka"), "audio/x-matroska");
    }

    #[test]
    fn audio_didl_uses_the_music_track_class() {
        let didl = build_audio_didl("Song", "http://h/a.mp3", "http-get:*:audio/mpeg:*", &[]);
        assert!(didl.contains("object.item.audioItem.musicTrack"));
        assert!(!didl.contains("videoItem"));
        assert_eq!(didl.matches("<res ").count(), 1);
        assert!(didl.contains("http-get:*:audio/mpeg:*"));
        // Titles are escaped exactly like the video builder.
        let escaped = build_audio_didl("A & B", "http://h/a.mp3", "proto", &[]);
        assert!(escaped.contains("A &amp; B"));
    }

    #[test]
    fn sink_gate_follows_the_media_kind() {
        let audio_only = vec![
            "http-get:*:audio/mpeg:*".to_string(),
            "http-get:*:audio/flac:*".to_string(),
        ];
        let video_only = vec![
            "http-get:*:video/mp4:*".to_string(),
            "http-get:*:video/x-matroska:*".to_string(),
        ];
        // An audio source must pass a renderer that only lists audio types.
        assert!(sink_advertises(&audio_only, "audio"));
        assert!(!sink_advertises(&audio_only, "video"));
        assert!(sink_advertises(&video_only, "video"));
        assert!(!sink_advertises(&video_only, "audio"));
        // A renderer that accepts everything (`*/*`) passes for both kinds.
        assert!(sink_advertises(&["http-get:*:*/*:*".to_string()], "audio"));
        assert!(sink_advertises(&["http-get:*:*/*:*".to_string()], "video"));
    }

    #[test]
    fn embedded_marker_only_matches_the_current_media_path() {
        assert_eq!(
            embedded_track_index("/v/a.mkv#embedded:2", "/v/a.mkv"),
            Some(2)
        );
        assert_eq!(
            embedded_track_index("/v/a.mkv#embedded:0", "/v/a.mkv"),
            Some(0)
        );
        // Wrong prefix: a real filename that itself contains the marker must
        // never be mistaken for a synthetic track.
        assert_eq!(
            embedded_track_index("/v/a.mkv#embedded:2", "/v/other.mkv"),
            None
        );
        assert_eq!(
            embedded_track_index("/v/a#embedded:1.mkv", "/v/a#embedded:1.mkv"),
            None
        );
        // Not a number / no index / no marker.
        assert_eq!(
            embedded_track_index("/v/a.mkv#embedded:x", "/v/a.mkv"),
            None
        );
        assert_eq!(embedded_track_index("/v/a.mkv#embedded:", "/v/a.mkv"), None);
        assert_eq!(embedded_track_index("/v/a.mkv", "/v/a.mkv"), None);
    }

    #[test]
    fn subtitle_protocol_follows_the_real_format() {
        let srt = candidate("Movie.srt", "", true);
        assert_eq!(
            subtitle_didl_protocol(&srt).as_deref(),
            Some("http-get:*:text/srt:*")
        );
        let ass = candidate("Movie.zh.ass", "zh", false);
        assert_eq!(
            subtitle_didl_protocol(&ass).as_deref(),
            Some("http-get:*:text/ass:*")
        );
        let embedded = SubtitleCandidate::embedded(
            "/v/a.mkv",
            3,
            "subrip".to_string(),
            Some("zh-CN"),
            Some("Forced"),
        );
        assert_eq!(embedded.support, "webvtt");
        assert_eq!(
            subtitle_didl_protocol(&embedded).as_deref(),
            Some("http-get:*:text/vtt:*")
        );
        assert_eq!(embedded.path, "/v/a.mkv#embedded:3");
        assert_eq!(embedded.name, "zh (subrip, embedded)");
        // `lang` stays the canonical key, `language` is the raw source tag
        // (`zh-CN`), and `title` comes straight from the container.
        assert_eq!(embedded.lang, "zh");
        assert_eq!(embedded.language.as_deref(), Some("zh-CN"));
        assert_eq!(embedded.title.as_deref(), Some("Forced"));
        let embedded_json = embedded.to_json();
        assert_eq!(embedded_json["language"], "zh-CN");
        assert_eq!(embedded_json["title"], "Forced");
        let bitmap =
            SubtitleCandidate::embedded("/v/a.mkv", 4, "hdmv_pgs_subtitle".to_string(), None, None);
        assert!(bitmap.is_bitmap());
        assert_eq!(bitmap.language, None);
        assert_eq!(bitmap.title, None);
        assert_eq!(subtitle_didl_protocol(&bitmap), None);
        assert_eq!(bitmap.to_json()["support"], "bitmap-unsupported");
    }

    #[test]
    fn sidecar_candidates_expose_their_language_suffix() {
        let matched = match_subtitle_name("Movie.mkv", "Movie.chi.srt").unwrap();
        let candidate = SubtitleCandidate::sidecar(
            "Movie.chi.srt".to_string(),
            "/v/Movie.chi.srt".to_string(),
            matched.lang_key,
            matched.language,
            matched.exact,
            42,
        );
        let json = candidate.to_json();
        // `lang` is normalised for matching, `language` keeps the raw suffix.
        assert_eq!(json["lang"], "zh");
        assert_eq!(json["language"], "chi");
        assert_eq!(json["title"], Value::Null);
        // A bare `<stem>.srt` has no language at all.
        let plain = match_subtitle_name("Movie.mkv", "Movie.srt").unwrap();
        assert_eq!(plain.language, None);
    }

    #[test]
    fn subtitle_status_counts_tracks_and_explains_bitmaps() {
        let tracks = vec![
            wdmm_ffmpeg::SubtitleStream {
                index: 2,
                codec: "subrip".to_string(),
                language: Some("zh".to_string()),
                title: Some("Chinese".to_string()),
            },
            wdmm_ffmpeg::SubtitleStream {
                index: 3,
                codec: "hdmv_pgs_subtitle".to_string(),
                language: None,
                title: None,
            },
        ];
        let mut hls = SubtitleStatus::default();
        hls.apply_probe(&tracks, "hls");
        assert_eq!(hls.text_tracks, 1);
        assert_eq!(hls.bitmap_tracks, 1);
        assert!(hls.bitmap_note.as_deref().unwrap().contains("bitmap"));

        // Direct mode: Kodi renders bitmap tracks itself, so no note.
        let mut direct = SubtitleStatus::default();
        direct.apply_probe(&tracks, "direct");
        assert_eq!(direct.bitmap_note, None);
        assert_eq!(hls.to_json()["text_tracks"], 1);
        assert_eq!(direct.to_json()["bitmap_tracks"], 1);
    }

    #[test]
    fn start_rejects_bad_config() {
        let bad_json = read(wdmm_dlna_session_start(std::ptr::null()));
        assert!(bad_json.contains("\"ok\":false"));

        let cfg = std::ffi::CString::new("{not json").unwrap();
        let value = json_of(wdmm_dlna_session_start(cfg.as_ptr()));
        assert_eq!(value["ok"], false);
        assert!(value["error"]
            .as_str()
            .unwrap()
            .contains("invalid config_json"));

        let missing = std::ffi::CString::new(r#"{"account_id":"a"}"#).unwrap();
        let value = json_of(wdmm_dlna_session_start(missing.as_ptr()));
        assert_eq!(value["ok"], false);
        assert!(value["error"]
            .as_str()
            .unwrap()
            .contains("renderer_location"));
    }

    #[test]
    fn start_reports_unknown_account() {
        let cfg = std::ffi::CString::new(
            r#"{"renderer_location":"http://192.0.2.10:1234/desc.xml",
                "account_id":"does-not-exist","remote_path":"/v/a.mkv"}"#,
        )
        .unwrap();
        let value = json_of(wdmm_dlna_session_start(cfg.as_ptr()));
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"], "account not found");
    }

    #[test]
    fn unknown_handles_return_json_errors() {
        let state = json_of(wdmm_dlna_session_state(9_999_999));
        assert_eq!(state["ok"], false);
        assert!(state["error"].as_str().unwrap().contains("unknown session"));

        let cmd = std::ffi::CString::new(r#"{"cmd":"play"}"#).unwrap();
        let value = json_of(wdmm_dlna_session_cmd(9_999_999, cmd.as_ptr()));
        assert_eq!(value["ok"], false);

        let close = json_of(wdmm_dlna_session_close(9_999_999));
        assert_eq!(close["ok"], false);
    }

    #[test]
    fn cmd_rejects_bad_payload() {
        let cfg = std::ffi::CString::new("{\"cmd\":\"play\"}").unwrap();
        let value = json_of(wdmm_dlna_session_cmd(1, cfg.as_ptr()));
        // Handle 1 does not exist in this test binary's registry.
        assert_eq!(value["ok"], false);
    }

    #[test]
    fn renderer_location_host_parsing() {
        assert_eq!(
            renderer_ip_from_location("http://192.168.1.20:49152/desc.xml").unwrap(),
            "192.168.1.20".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            renderer_ip_from_location("http://192.168.1.20/desc.xml").unwrap(),
            "192.168.1.20".parse::<IpAddr>().unwrap()
        );
        assert!(renderer_ip_from_location("not a url").is_err());
    }

    // ── §3 warm-up phase machine ─────────────────────────────────────────────

    #[test]
    fn phase_strings_cover_all_states() {
        assert_eq!(SessionPhase::Idle.as_str(), "idle");
        assert_eq!(SessionPhase::Warming.as_str(), "warming");
        assert_eq!(SessionPhase::Playing.as_str(), "playing");
        assert_eq!(SessionPhase::Ended.as_str(), "ended");
        assert_eq!(SessionPhase::Failed.as_str(), "failed");
    }

    #[test]
    fn cast_rejected_until_playlist_is_ready() {
        assert_eq!(
            cast_precondition(SessionPhase::Playing, true),
            Err("not warming")
        );
        assert_eq!(
            cast_precondition(SessionPhase::Idle, true),
            Err("not warming")
        );
        assert_eq!(
            cast_precondition(SessionPhase::Warming, false),
            Err("playlist not ready")
        );
        assert_eq!(cast_precondition(SessionPhase::Warming, true), Ok(()));
    }

    #[test]
    fn stop_while_warming_skips_renderer_and_resets_phase() {
        let mut phase = SessionPhase::Warming;
        assert!(
            !stop_phase_transition(&mut phase),
            "warming stop must not touch the renderer"
        );
        assert_eq!(phase, SessionPhase::Idle);
        assert_eq!(phase.as_str(), "idle");

        let mut phase = SessionPhase::Playing;
        assert!(
            stop_phase_transition(&mut phase),
            "playing stop still stops the renderer"
        );
        assert_eq!(phase, SessionPhase::Idle);

        // Failed/idle sessions never reached the renderer either.
        let mut phase = SessionPhase::Failed;
        assert!(!stop_phase_transition(&mut phase));
        assert_eq!(phase, SessionPhase::Idle);
        let mut phase = SessionPhase::Idle;
        assert!(!stop_phase_transition(&mut phase));
        assert_eq!(phase, SessionPhase::Idle);
    }

    #[test]
    fn estimated_realtime_needs_a_stable_window() {
        assert_eq!(estimate_realtime(0, 0.0), None);
        assert_eq!(estimate_realtime(1_999, 2.0), None);
        assert_eq!(estimate_realtime(2_000, 1.0), Some(true));
        assert_eq!(estimate_realtime(2_000, 0.99), Some(false));
    }

    // ── contract §3.3–§3.6: VOD telemetry + transcode cache ──────────────────

    fn sample_progress() -> HlsProgress {
        HlsProgress {
            playlist_ready: true,
            segments: 9,
            content_ms: 54_000,
            wall_ms: 60_000,
            speed: 0.9,
            ended: false,
            error: None,
            encoder: Default::default(),
            warnings: Vec::new(),
            run_state: "active",
            alignment: Some("keyframe"),
            ready_k: Some(8),
            max_requested_k: Some(6),
            plan_duration_ms: Some(7_200_000),
            // floor would give 5, ceil gives 4 — pins the LOOKAHEAD rule.
            seg_secs: 13.6,
            segment_count: 530,
            drift_ms: Some(-250),
            cache_hits: 12,
            cache_bytes: 34_000_000,
            over_budget: true,
        }
    }

    #[test]
    fn vod_telemetry_is_neutral_without_a_transcode_job() {
        let value = vod_telemetry(None);
        assert_eq!(value["run_state"], "idle");
        assert!(value["alignment"].is_null());
        assert!(value["drift_ms"].is_null());
        assert_eq!(value["ready_segments"], 0);
        assert_eq!(value["seg_count"], 0);
        assert_eq!(value["seg_secs"], 0.0);
        assert_eq!(value["lookahead_segments"], 0);
        assert_eq!(value["cache_hits"], 0);
        assert_eq!(value["cache_bytes"], 0);
        assert_eq!(value["cache_over_budget"], false);
    }

    #[test]
    fn vod_telemetry_mirrors_the_proxy_progress() {
        let value = vod_telemetry(Some(&sample_progress()));
        assert_eq!(value["run_state"], "active");
        assert_eq!(value["alignment"], "keyframe");
        assert_eq!(value["drift_ms"], -250);
        assert_eq!(value["ready_segments"], 9);
        assert_eq!(value["seg_count"], 530);
        assert_eq!(value["seg_secs"], 13.6);
        assert_eq!(value["lookahead_segments"], AHEAD_SEGMENTS);
        assert_eq!(value["cache_hits"], 12);
        assert_eq!(value["cache_bytes"], 34_000_000);
        assert_eq!(value["cache_over_budget"], true);
    }

    #[test]
    fn vod_duration_comes_from_the_plan_only_for_hls() {
        let progress = sample_progress();
        assert_eq!(
            vod_duration_ms("hls", Some(&progress), 0),
            7_200_000,
            "HLS takes the plan duration, not the renderer's 0"
        );
        assert_eq!(vod_duration_ms("hls", None, 12_345), 12_345);
        assert_eq!(
            vod_duration_ms("direct", Some(&progress), 12_345),
            12_345,
            "direct keeps the renderer value"
        );
    }

    #[test]
    fn cache_config_uses_contract_defaults() {
        let defaults = CacheConfig::from_json(&json!({}));
        assert!(defaults.root.is_none());
        assert_eq!(defaults.budget_bytes, 2048 * 1024 * 1024);
        assert!(defaults.ident.is_empty(), "no ident => ephemeral segments");

        let configured = CacheConfig::from_json(&json!({
            "cache_dir": "E:\\cache\\wdmm",
            "cache_budget_mb": 512,
            "cache_ident": "etag-1234",
        }));
        assert_eq!(
            configured.root.as_deref(),
            Some(std::path::Path::new("E:\\cache\\wdmm"))
        );
        assert_eq!(configured.budget_bytes, 512 * 1024 * 1024);
        assert_eq!(configured.ident, "etag-1234");

        // A blank directory / zero budget fall back to the defaults.
        let blanks = CacheConfig::from_json(&json!({"cache_dir": "  ", "cache_budget_mb": 0}));
        assert!(blanks.root.is_none());
        assert_eq!(blanks.budget_bytes, 2048 * 1024 * 1024);
    }

    #[test]
    fn cache_ffi_reports_not_started_as_json() {
        // Both cache APIs are session-style JSON even though they are not
        // `wdmm_dlna_session_*` (contract §4); never the `error:` prefix.
        // Another test may have the process-wide proxy running in parallel —
        // only the not-started path is asserted here.
        if proxy_cell().lock().unwrap().is_some() {
            return;
        }
        let stats = json_of(crate::wdmm_transcode_cache_stats_json());
        assert_eq!(stats["ok"], false);
        assert_eq!(stats["error"], "proxy not started");
        assert!(stats.get("bytes").is_none());

        let cleared = json_of(crate::wdmm_transcode_cache_clear());
        assert_eq!(cleared["ok"], false);
        assert_eq!(cleared["error"], "proxy not started");
    }

    #[test]
    fn diagnostics_tail_is_available_without_the_ffmpeg_feature() {
        // §7.3/§7.4: the failed-phase `diagnostics` key must always exist.
        wdmm_ffmpeg::diag::clear();
        wdmm_ffmpeg::diag::log("transcode job start");
        let lines = wdmm_ffmpeg::diag::tail(40);
        assert!(lines.last().unwrap().contains("transcode job start"));
        wdmm_ffmpeg::diag::clear();
    }
}
