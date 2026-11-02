//! DLNA cast session facade.
//!
//! The whole cast path lives in Rust: a session owns the account driver, the
//! chunked proxy stream, the optional HLS transcode, sidecar subtitle matching
//! and the AVTransport / ConnectionManager SOAP state machine. Dart only starts
//! a session ([`wdmm_dlna_session_start`]), polls [`wdmm_dlna_session_state`]
//! once per second and sends user-level commands through
//! [`wdmm_dlna_session_cmd`]. No media byte ever passes through Dart, and no
//! Range is ever materialised whole in memory — the proxy pulls 1 MiB chunks
//! through [`crate::driver_range_stream`].
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
    build_didl_lite, describe_renderer, ConnectionManagerClient, DidlRes, DlnaRenderer,
    DlnaSession, SsdpDiscovery, SUBTITLE_PROTOCOL_INFO,
};
use wdmm_drivers::WdmmVfs;
use wdmm_proxy::{BindMode, BoxStream, ProxyHandle};

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

/// Containers that many renderers refuse or play badly as-is.
const PROBLEMATIC_CONTAINERS: &[&str] = &[
    "mkv", "avi", "mov", "ts", "m2ts", "mts", "flv", "rmvb", "rm", "wmv", "asf", "3gp", "webm",
];

/// Codecs that virtually no renderer can decode inline.
const BAD_CODECS: &[&str] = &["rmvb", "rm", "wmv", "asf", "flv"];

/// Upper bound for a sidecar subtitle we are willing to read and expose.
const MAX_SUBTITLE_BYTES: usize = 8 * 1024 * 1024;

/// Discovery timeout accepted by the extended discover call.
const DISCOVER_MIN_MS: u64 = 200;
const DISCOVER_MAX_MS: u64 = 10_000;

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

/// One sidecar subtitle discovered next to the playing video.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SubtitleCandidate {
    name: String,
    path: String,
    /// Canonical language key (`"zh"`, `"en"`, …); empty for the default track.
    lang: String,
    /// Whether the file name is exactly `<video stem>.<ext>`.
    exact: bool,
    size: u64,
}

impl SubtitleCandidate {
    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "path": self.path,
            "lang": self.lang,
            "size": self.size,
        })
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
    proxy: ProxyHandle,
    lan_ip: IpAddr,
    stream_token: String,
    hls_token: Option<String>,
    media_url: String,
    mime: String,
    protocol_info: String,
    subtitle_candidates: Vec<SubtitleCandidate>,
    selected_subtitle: Option<SubtitleCandidate>,
    subtitle_token: Option<String>,
}

/// Freshly prepared media resources before they are handed to a renderer.
struct PreparedMedia {
    provider: &'static str,
    stream_token: String,
    hls_token: Option<String>,
    media_url: String,
    mime: String,
    protocol_info: String,
    size: u64,
    subtitle_candidates: Vec<SubtitleCandidate>,
    selected_subtitle: Option<SubtitleCandidate>,
    subtitle_token: Option<String>,
    didl: String,
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
        "aac" | "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "ogg" | "oga" => "audio/ogg",
        _ => "video/mp4",
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

/// Match `subtitle_name` against the video's stem.
///
/// Returns `(language_key, exact)` where `language_key` is the canonical key of
/// the language suffix (empty for a bare `<stem>.<ext>` sidecar).
fn match_subtitle_name(video_name: &str, subtitle_name: &str) -> Option<(String, bool)> {
    let ext = extension(subtitle_name);
    if !SUBTITLE_EXTS.contains(&ext.as_str()) {
        return None;
    }
    let stem = video_stem(video_name).to_lowercase();
    let lower_name = subtitle_name.to_lowercase();
    if lower_name == format!("{stem}.{ext}") {
        return Some((String::new(), true));
    }
    let rest = lower_name.strip_prefix(&format!("{stem}."))?;
    let middle = rest.strip_suffix(&format!(".{ext}"))?;
    if middle.is_empty() {
        return None;
    }
    Some((display_subtitle_language(middle).to_lowercase(), false))
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

/// Build the DIDL-Lite document (media resource + optional subtitle resource).
fn build_didl(
    title: &str,
    media_url: &str,
    protocol_info: &str,
    subtitle_url: Option<&str>,
) -> String {
    let mut resources = vec![DidlRes::new(media_url, protocol_info)];
    if let Some(url) = subtitle_url {
        resources.push(DidlRes::new(url, SUBTITLE_PROTOCOL_INFO));
    }
    build_didl_lite(title, &resources)
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
async fn scan_subtitles(
    driver: Arc<dyn WdmmVfs>,
    remote_path: &str,
    video_name: &str,
) -> Vec<SubtitleCandidate> {
    let parent = parent_dir(remote_path);
    let items = match driver.list(&parent).await {
        Ok(items) => items,
        Err(_) => return Vec::new(),
    };
    let mut candidates = Vec::new();
    for item in items {
        if item.is_dir {
            continue;
        }
        if let Some((lang, exact)) = match_subtitle_name(video_name, &item.name) {
            candidates.push(SubtitleCandidate {
                path: join_path(&parent, &item.name),
                name: item.name.clone(),
                lang,
                exact,
                size: item.size,
            });
        }
    }
    candidates
}

/// Read a sidecar subtitle and expose it through the proxy.
///
/// Returns `(token, url)`.
async fn expose_subtitle(
    handle: &ProxyHandle,
    driver: Arc<dyn WdmmVfs>,
    lan_ip: IpAddr,
    candidate: &SubtitleCandidate,
) -> Result<(String, String), String> {
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
    let data = Arc::new(raw);
    let size = data.len() as u64;
    let stream_data = Arc::clone(&data);
    let registration = handle
        .register_driver(
            size,
            Some(candidate.name.clone()),
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
    let url = format!("http://{lan_ip}:{port}{}", registration.path_template);
    Ok((registration.token, url))
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
    remote_path: &str,
    name: &str,
    size: u64,
    preset_id: Option<&str>,
    force_direct: bool,
    ui_lang: &str,
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
    let (provider, media_url, hls_token): (&'static str, String, Option<String>) = if wants_direct {
        (
            "direct",
            format!("http://{lan_ip}:{port}{}", registration.path_template),
            None,
        )
    } else {
        let Some(preset_id) = preset_id else {
            handle.unregister(&stream_token);
            return Err("preset_id required for a transcoded cast".to_string());
        };
        let preset = match global_library_db().get_transcode_preset(preset_id) {
            Ok(Some(preset)) => preset,
            Ok(None) => {
                handle.unregister(&stream_token);
                return Err(format!("transcode preset not found: {preset_id}"));
            }
            Err(e) => {
                handle.unregister(&stream_token);
                return Err(format!("transcode preset: {e}"));
            }
        };
        match handle
            .register_hls(renderer.usn.clone(), loopback_url, preset, None, None)
            .await
        {
            Ok(hls) => (
                "hls",
                format!("http://{lan_ip}:{port}{}", hls.playlist_template),
                Some(hls.token),
            ),
            Err(e) => {
                handle.unregister(&stream_token);
                return Err(format!("hls registration failed: {e}"));
            }
        }
    };

    let mime = mime_for(name).to_string();
    let mime_ext = extension(name).to_uppercase();
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
    let advertises_video = sink_protocols.iter().any(|info| {
        info.split(':')
            .nth(2)
            .is_some_and(|m| m.starts_with("video/"))
    });
    if !sink_protocols.is_empty() && !advertises_video {
        unregister_media(handle, Some(&stream_token), hls_token.as_deref(), None);
        return Err("renderer does not advertise video/* in SinkProtocolInfo".to_string());
    }
    let protocol_info = match connection_manager {
        Some(client) => client.negotiate(&mime, &mime_ext, &sink_protocols),
        None => format!("http-get:*:{mime}:DLNA.ORG_OP=01"),
    };

    let subtitle_candidates = scan_subtitles(Arc::clone(&driver), remote_path, name).await;
    let selected_index = pick_subtitle(&subtitle_candidates, ui_lang);
    let (selected_subtitle, subtitle_token, subtitle_url) = match selected_index {
        Some(idx) => {
            let candidate = subtitle_candidates[idx].clone();
            match expose_subtitle(handle, Arc::clone(&driver), lan_ip, &candidate).await {
                Ok((token, url)) => (Some(candidate), Some(token), Some(url)),
                // A missing/oversized subtitle must never fail the cast itself.
                Err(_) => (None, None, None),
            }
        }
        None => (None, None, None),
    };

    let didl = build_didl(name, &media_url, &protocol_info, subtitle_url.as_deref());

    Ok(PreparedMedia {
        provider,
        stream_token,
        hls_token,
        media_url,
        mime,
        protocol_info,
        size,
        subtitle_candidates,
        selected_subtitle,
        subtitle_token,
        didl,
    })
}

/// Release every proxy token a prepared/active media set holds.
fn unregister_media(
    handle: &ProxyHandle,
    stream_token: Option<&str>,
    hls_token: Option<&str>,
    subtitle_token: Option<&str>,
) {
    if let Some(token) = stream_token {
        handle.unregister(token);
    }
    if let Some(token) = hls_token {
        handle.unregister_hls(token);
    }
    if let Some(token) = subtitle_token {
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

    let prepared = prepare_media(
        &proxy,
        lan_ip,
        &renderer,
        Arc::clone(&driver),
        &remote_path,
        &name,
        size,
        preset_id.as_deref(),
        force_direct,
        &ui_lang,
    )
    .await?;

    let mut session = match DlnaSession::open(&renderer).await {
        Ok(session) => session,
        Err(e) => {
            unregister_media(
                &proxy,
                Some(&prepared.stream_token),
                prepared.hls_token.as_deref(),
                prepared.subtitle_token.as_deref(),
            );
            return Err(format!("open renderer failed: {e}"));
        }
    };
    if let Err(e) = session.start(&prepared.media_url, &prepared.didl).await {
        unregister_media(
            &proxy,
            Some(&prepared.stream_token),
            prepared.hls_token.as_deref(),
            prepared.subtitle_token.as_deref(),
        );
        return Err(format!("SetAVTransportURI failed: {e}"));
    }

    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
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
        proxy,
        lan_ip,
        stream_token: prepared.stream_token,
        hls_token: prepared.hls_token,
        media_url: prepared.media_url.clone(),
        mime: prepared.mime,
        protocol_info: prepared.protocol_info.clone(),
        subtitle_candidates: prepared.subtitle_candidates,
        selected_subtitle: prepared.selected_subtitle,
        subtitle_token: prepared.subtitle_token,
    };
    put_session(entry);

    Ok(json!({
        "ok": true,
        "handle": handle,
        "url": prepared.media_url,
        "provider": prepared.provider,
        "protocol_info": prepared.protocol_info,
        "compat": compat_json(&name, prepared.provider),
    }))
}

/// Apply one `{"cmd":…}` document to a session.
async fn apply_cmd(entry: &mut SessionEntry, cmd: &Value) -> Result<(), String> {
    let command = cmd
        .get("cmd")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    match command.as_str() {
        "play" => entry.session.play().await.map_err(|e| e.to_string()),
        "pause" => entry.session.pause().await.map_err(|e| e.to_string()),
        "stop" => entry.session.stop().await.map_err(|e| e.to_string()),
        "seek" => {
            let position_ms = cmd
                .get("position_ms")
                .and_then(Value::as_i64)
                .ok_or_else(|| "seek requires `position_ms`".to_string())?;
            entry
                .session
                .seek(position_ms)
                .await
                .map_err(|e| e.to_string())
        }
        "select" => select_media(entry, cmd).await,
        "subtitle" => select_subtitle(entry, cmd).await,
        other => Err(format!("unknown cmd: {other}")),
    }
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

    let prepared = prepare_media(
        &proxy,
        lan_ip,
        &entry.renderer,
        Arc::clone(&driver),
        &remote_path,
        &name,
        size,
        preset_id.as_deref(),
        force_direct,
        &entry.ui_lang,
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
            prepared.subtitle_token.as_deref(),
        );
        return Err(format!("SetAVTransportURI failed: {e}"));
    }

    unregister_media(
        &proxy,
        Some(&entry.stream_token),
        entry.hls_token.as_deref(),
        entry.subtitle_token.as_deref(),
    );
    entry.remote_path = remote_path;
    entry.name = name;
    entry.size = prepared.size;
    entry.preset_id = preset_id;
    entry.force_direct = force_direct;
    entry.provider = prepared.provider;
    entry.stream_token = prepared.stream_token;
    entry.hls_token = prepared.hls_token;
    entry.media_url = prepared.media_url;
    entry.mime = prepared.mime;
    entry.protocol_info = prepared.protocol_info;
    entry.subtitle_candidates = prepared.subtitle_candidates;
    entry.selected_subtitle = prepared.selected_subtitle;
    entry.subtitle_token = prepared.subtitle_token;
    Ok(())
}

/// Enable a sidecar subtitle (or turn subtitles off) by re-sending the DIDL.
async fn select_subtitle(entry: &mut SessionEntry, cmd: &Value) -> Result<(), String> {
    let wanted = opt_string(cmd, "path");
    let proxy = entry.proxy.clone();
    let driver = Arc::clone(&entry.driver);
    let lan_ip = entry.lan_ip;

    let subtitle_url = match wanted {
        Some(path) => {
            let candidate = entry
                .subtitle_candidates
                .iter()
                .find(|candidate| candidate.path == path)
                .cloned()
                .unwrap_or_else(|| SubtitleCandidate {
                    name: file_name(&path),
                    path: path.clone(),
                    lang: String::new(),
                    exact: false,
                    size: 0,
                });
            let (token, url) = expose_subtitle(&proxy, Arc::clone(&driver), lan_ip, &candidate)
                .await
                .map_err(|e| format!("subtitle failed: {e}"))?;
            if let Some(old) = entry.subtitle_token.replace(token) {
                proxy.unregister(&old);
            }
            entry.selected_subtitle = Some(candidate);
            Some(url)
        }
        None => {
            if let Some(old) = entry.subtitle_token.take() {
                proxy.unregister(&old);
            }
            entry.selected_subtitle = None;
            None
        }
    };

    let didl = build_didl(
        &entry.name,
        &entry.media_url,
        &entry.protocol_info,
        subtitle_url.as_deref(),
    );
    // Re-send the URI (with the new metadata) and play: renderers do not pick
    // up a subtitle-only metadata change any other way.
    entry
        .session
        .start(&entry.media_url, &didl)
        .await
        .map_err(|e| e.to_string())
}

/// Full state document for one tick.
fn state_json(entry: &SessionEntry, tick: &wdmm_dlna::TickResult) -> Value {
    let ended_reason = match tick.ended_reason {
        Some(reason) => Value::String(reason.to_string()),
        None => Value::Null,
    };
    json!({
        "ok": true,
        "state": if tick.ended { "ended" } else { "casting" },
        "transport": tick.transport,
        "position_ms": tick.position_ms,
        "duration_ms": tick.duration_ms,
        "current_path": entry.remote_path,
        "current_name": entry.name,
        "provider": entry.provider,
        "protocol_info": entry.protocol_info,
        "ended": tick.ended,
        "ended_reason": ended_reason,
        "subtitle_candidates": entry
            .subtitle_candidates
            .iter()
            .map(SubtitleCandidate::to_json)
            .collect::<Vec<_>>(),
        "selected_subtitle": entry.selected_subtitle.as_ref().map(SubtitleCandidate::to_json),
        "compat": compat_json(&entry.name, entry.provider),
    })
}

// ── FFI: session facade ──────────────────────────────────────────────────────

/// Start a DLNA cast session.
///
/// `config_json` (UTF-8, null-terminated):
/// ```json
/// {"renderer_location":"http://…/desc.xml","renderer_usn":"uuid:…",
///  "account_id":"…","remote_path":"/dir/movie.mkv","name":"movie.mkv",
///  "size":123,"preset_id":null,"force_direct":true,"ui_lang":"zh"}
/// ```
///
/// Returns `{"ok":true,"handle":N,"url":"…","provider":"direct"|"hls",
/// "protocol_info":"…","compat":{…}}` or `{"ok":false,"error":"…"}`.
/// Caller must free the result via [`crate::wdmm_ffi_free_string`].
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
/// "subtitle_candidates":[…],"selected_subtitle":{…}|null,"compat":{…}}`, or
/// `{"ok":false,"error":"…"}` for an unknown handle / renderer failure.
#[no_mangle]
pub extern "C" fn wdmm_dlna_session_state(handle: u32) -> FfiString {
    let Some(mut entry) = take_session(handle) else {
        return err_json("unknown session handle");
    };
    let result = rt().block_on(entry.session.tick());
    let value = match result {
        Ok(tick) => state_json(&entry, &tick),
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
/// "preset_id":null}` and `{"cmd":"subtitle","path":"/dir/b.zh.srt"}`
/// (`path:null` turns subtitles off).
///
/// Returns `"ok"` on success or `{"ok":false,"error":"…"}`.
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
        Ok(()) => FfiString::from("ok"),
        Err(e) => err_json(e),
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
    unregister_media(
        &entry.proxy,
        Some(&entry.stream_token),
        entry.hls_token.as_deref(),
        entry.subtitle_token.as_deref(),
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
pub extern "C" fn wdmm_dlna_sink_protocol_info_json(
    renderer_location: *const c_char,
) -> FfiString {
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

    #[test]
    fn matches_sidecar_subtitles() {
        assert_eq!(
            match_subtitle_name("Movie.mkv", "Movie.srt"),
            Some((String::new(), true))
        );
        assert_eq!(
            match_subtitle_name("Movie.mkv", "Movie.zh.srt"),
            Some(("zh".to_string(), false))
        );
        assert_eq!(
            match_subtitle_name("Movie.mkv", "movie.ZH-Hans.ass"),
            Some(("zh".to_string(), false))
        );
        assert_eq!(
            match_subtitle_name("Movie.mkv", "Movie.en-US.vtt"),
            Some(("en".to_string(), false))
        );
        assert_eq!(match_subtitle_name("Movie.mkv", "Other.srt"), None);
        assert_eq!(match_subtitle_name("Movie.mkv", "Movie.idx"), None);
        assert_eq!(match_subtitle_name("Movie.mkv", "Movie.mkv"), None);
        assert_eq!(match_subtitle_name("Movie.mkv", "Movie..srt"), None);
    }

    fn candidate(name: &str, lang: &str, exact: bool) -> SubtitleCandidate {
        SubtitleCandidate {
            name: name.to_string(),
            path: format!("/v/{name}"),
            lang: lang.to_string(),
            exact,
            size: 10,
        }
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
    fn didl_has_subtitle_resource_only_when_requested() {
        let plain = build_didl("t", "http://h/a.mkv", "proto", None);
        assert_eq!(plain.matches("<res ").count(), 1);
        let with_sub = build_didl("t", "http://h/a.mkv", "proto", Some("http://h/a.srt"));
        assert_eq!(with_sub.matches("<res ").count(), 2);
        assert!(with_sub.contains(SUBTITLE_PROTOCOL_INFO));
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
}
