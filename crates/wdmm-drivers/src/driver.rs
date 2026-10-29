use async_trait::async_trait;
use chrono::{DateTime, Utc};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DriverError {
    #[error("network: {0}")]
    Network(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("auth failed")]
    AuthFailed,

    #[error("unsupported provider: {0}")]
    UnsupportedProvider(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("other: {0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, DriverError>;

impl From<reqwest::Error> for DriverError {
    fn from(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            DriverError::Network("timeout".into())
        } else if e.status() == Some(reqwest::StatusCode::NOT_FOUND) {
            DriverError::NotFound("resource".into())
        } else if e.status() == Some(reqwest::StatusCode::UNAUTHORIZED) {
            DriverError::AuthFailed
        } else {
            DriverError::Network(e.to_string())
        }
    }
}

// ── ContentDeliveryKind ───────────────────────────────────────────────────────

/// How bytes are obtained for a [`CloudFileItem`].
///
/// Callers (media player, DLNA, download engine, future mounts) read this field
/// to decide whether to delegate to the local proxy rather than consuming a URL
/// directly.
///
/// The enum is `#[non_exhaustive]` — match with a `_` catch-all so future
/// variants (e.g. `P2P`, `LocalMount`) do not break existing callers.
///
/// ## Decision pseudocode
/// ```text
/// match item.delivery {
///   NetworkDirect if can_http(player) && !must_proxy_policy => play(url, headers),
///   LocalFile if can_file(player) => play(path),
///   LocalStream | _ => proxy.register_driver(…) // or wdmm_proxy_register_stream
/// }
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentDeliveryKind {
    /// A network URL is available (`raw_url` + optional `raw_headers`).
    ///
    /// media_kit can consume this over HTTP. If the caller cannot pass custom
    /// headers (some player APIs do not support it), or if a MustProxy policy
    /// applies, route through the proxy via `register_file` / `register_driver`
    /// with a side-channel auth header injection.
    NetworkDirect,

    /// No URL and no local path — bytes come from an in-process Range/byte
    /// stream only (e.g. a crypt-wrapper driver, a streaming-only protocol).
    ///
    /// media_kit **cannot** consume this directly. Callers must bridge through
    /// `ProxyHandle::register_driver` (Rust) or `wdmm_proxy_register_stream`
    /// (FFI / Dart) to get a proxy token URL, or materialise to disk first.
    LocalStream,

    /// A local filesystem path (`local_path`) is directly readable.
    ///
    /// media_kit can play a local file path. DLNA / HLS can still use
    /// `register_file` / `register_hls` for wider renderer compatibility.
    LocalFile,
}

impl ContentDeliveryKind {
    /// Infer the delivery kind from the presence of `raw_url` and `local_path`.
    ///
    /// Priority: `NetworkDirect` > `LocalFile` > `LocalStream`.
    ///
    /// Prefer setting `delivery` **explicitly** at construction sites — this
    /// helper exists for migration of existing struct literals.
    pub fn infer(raw_url: Option<&str>, local_path: Option<&std::path::Path>) -> Self {
        if raw_url.is_some() {
            ContentDeliveryKind::NetworkDirect
        } else if local_path.is_some() {
            ContentDeliveryKind::LocalFile
        } else {
            ContentDeliveryKind::LocalStream
        }
    }
}

// ── CloudFileItem ─────────────────────────────────────────────────────────────

/// A cloud file or directory entry returned by a [`WdmmVfs`].
#[derive(Debug, Clone)]
pub struct CloudFileItem {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<DateTime<Utc>>,
    /// Direct download URL (with auth headers if needed).
    /// Present when `delivery == NetworkDirect`.
    pub raw_url: Option<String>,
    /// Custom request headers for `raw_url` (e.g. `Authorization`).
    /// Empty when `delivery != NetworkDirect`.
    pub raw_headers: Vec<(String, String)>,
    /// How bytes are obtained. Callers use this to decide whether to proxy.
    pub delivery: ContentDeliveryKind,
    /// Local filesystem path, present when `delivery == LocalFile`.
    pub local_path: Option<std::path::PathBuf>,
}

impl CloudFileItem {
    /// Construct a `NetworkDirect` item (has a download URL).
    pub fn network_direct(
        name: impl Into<String>,
        is_dir: bool,
        size: u64,
        modified: Option<DateTime<Utc>>,
        raw_url: impl Into<String>,
        raw_headers: Vec<(String, String)>,
    ) -> Self {
        Self {
            name: name.into(),
            is_dir,
            size,
            modified,
            raw_url: Some(raw_url.into()),
            raw_headers,
            delivery: ContentDeliveryKind::NetworkDirect,
            local_path: None,
        }
    }

    /// Construct a `LocalStream` item (no URL, no path — in-process stream only).
    ///
    /// Callers must route through `register_driver` / `wdmm_proxy_register_stream`.
    pub fn local_stream(
        name: impl Into<String>,
        is_dir: bool,
        size: u64,
        modified: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            name: name.into(),
            is_dir,
            size,
            modified,
            raw_url: None,
            raw_headers: vec![],
            delivery: ContentDeliveryKind::LocalStream,
            local_path: None,
        }
    }

    /// Construct a `LocalFile` item (local filesystem path).
    pub fn local_file(
        name: impl Into<String>,
        is_dir: bool,
        size: u64,
        modified: Option<DateTime<Utc>>,
        local_path: impl Into<std::path::PathBuf>,
    ) -> Self {
        Self {
            name: name.into(),
            is_dir,
            size,
            modified,
            raw_url: None,
            raw_headers: vec![],
            delivery: ContentDeliveryKind::LocalFile,
            local_path: Some(local_path.into()),
        }
    }
}

/// Cloud driver abstraction — aligns with Dart `WdmmVfs`.
#[async_trait]
pub trait WdmmVfs: Send + Sync {
    /// Initialise the driver (fetch tokens, validate credentials, etc.).
    async fn init(&self) -> Result<()> {
        Ok(())
    }

    /// List directory at remote path.
    async fn list(&self, path: &str) -> Result<Vec<CloudFileItem>>;

    /// Get a single file item (must include `raw_url` for streaming/download).
    async fn get(&self, path: &str) -> Result<CloudFileItem>;

    /// Create a directory.
    async fn mkdir(&self, path: &str) -> Result<()>;

    /// Rename/move.
    async fn rename(&self, old_path: &str, new_path: &str) -> Result<()>;

    /// Delete a file or directory.
    async fn remove(&self, path: &str) -> Result<()>;

    /// Write bytes to a remote file path (used by sync engines for metadata).
    ///
    /// This is **not** the user-upload path (see drivers guide §8): it exists so
    /// sync engines can push small JSON/metadata files to remote storage.
    /// Default impl returns `Err(Unsupported)`.
    async fn put_content(&self, path: &str, data: &[u8]) -> Result<()> {
        let _ = (path, data);
        Err(DriverError::Other(
            "put_content not supported by this driver".into(),
        ))
    }

    /// Fetch the full raw bytes of a remote file.
    ///
    /// Default impl returns `Err(Unsupported)`. WebDAV implements via HTTP GET.
    async fn get_content(&self, path: &str) -> Result<Vec<u8>> {
        let _ = path;
        Err(DriverError::Other(
            "get_content not supported by this driver".into(),
        ))
    }

    /// Fetch a byte range of a remote file (resumable / partial-content download).
    ///
    /// `start` is inclusive, `end` is inclusive (same semantics as HTTP
    /// `Range: bytes=start-end`). Pass `end = None` to fetch from `start` to
    /// end-of-file.
    ///
    /// Default impl falls back to [`WdmmVfs::get_content`] and slices in
    /// memory, which is correct but wasteful for large files. Drivers that
    /// support HTTP Range (WebDAV, CtFile after redirect) should override this.
    async fn get_content_range(&self, path: &str, start: u64, end: Option<u64>) -> Result<Vec<u8>> {
        // Fallback: full download + slice. Only suitable for small files.
        let full = self.get_content(path).await?;
        let s = start as usize;
        if s >= full.len() {
            return Ok(Vec::new());
        }
        let e = end.map(|e| (e as usize + 1).min(full.len())).unwrap_or(full.len());
        Ok(full[s..e].to_vec())
    }

    /// Server-side copy of [src] to [dst] (both account-absolute paths).
    ///
    /// Default returns `Err(Unsupported)`. Drivers that expose a server-side
    /// copy primitive (e.g. WebDAV `COPY`) should override it. When unsupported,
    /// callers may fall back to `get_content` + `put_content`.
    async fn copy(&self, src: &str, dst: &str) -> Result<()> {
        let _ = (src, dst);
        Err(DriverError::Other(
            "copy not supported by this driver".into(),
        ))
    }

    /// Idempotent directory creation: a path that already exists is **not** an
    /// error. Drivers that can distinguish "exists" from a real failure should
    /// override; the default delegates to [`WdmmVfs::mkdir`] and swallows the
    /// "already exists" signal where the driver surfaces it via
    /// [`DriverError::Other`] text or a tolerated HTTP status.
    async fn ensure_dir(&self, path: &str) -> Result<()> {
        match self.mkdir(path).await {
            Ok(()) => Ok(()),
            // WebDAV MKCOL on an existing collection returns 405; some servers
            // return 409. Other drivers may phrase it as "already exists".
            Err(DriverError::Other(msg))
                if msg.contains("already")
                    || msg.contains("exists")
                    || msg.contains("405")
                    || msg.contains("409") =>
            {
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Short status labels for the accounts list (VIP tier, bandwidth quota, …).
    ///
    /// Default returns empty — drivers without an account-status API don't need
    /// to override. Implementations **must fail-soft**: catch network/auth
    /// errors and return `Ok(vec![])` rather than propagating, so the accounts
    /// list never blocks on a flaky status endpoint.
    async fn status_tags(&self) -> Result<Vec<StatusTag>> {
        Ok(Vec::new())
    }
}

/// A short status label emitted by [`WdmmVfs::status_tags`].
///
/// Mirrors the Dart `AccountStatusTag` (`lib/services/wdmm_vfs.dart`):
/// `key` is stable for tests, `label` is display-ready. The FFI layer
/// (`wdmm_account_status_tags`) serialises a `Vec<StatusTag>` to JSON.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StatusTag {
    pub key: String,
    pub label: String,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infer_priority_network_over_file() {
        // A raw URL takes priority over a local path.
        let kind = ContentDeliveryKind::infer(Some("https://x"), Some(std::path::Path::new("/a")));
        assert_eq!(kind, ContentDeliveryKind::NetworkDirect);
    }

    #[test]
    fn infer_local_file_when_no_url() {
        let kind = ContentDeliveryKind::infer(None, Some(std::path::Path::new("/cache/a.mp4")));
        assert_eq!(kind, ContentDeliveryKind::LocalFile);
    }

    #[test]
    fn infer_local_stream_when_neither() {
        let kind = ContentDeliveryKind::infer(None, None);
        assert_eq!(kind, ContentDeliveryKind::LocalStream);
    }

    #[test]
    fn constructors_set_delivery_correctly() {
        let nd = CloudFileItem::network_direct(
            "a.mp3", false, 100, None, "https://x/a.mp3", vec![],
        );
        assert_eq!(nd.delivery, ContentDeliveryKind::NetworkDirect);
        assert!(nd.raw_url.is_some());
        assert!(nd.local_path.is_none());

        let ls = CloudFileItem::local_stream("a.mp3", false, 100, None);
        assert_eq!(ls.delivery, ContentDeliveryKind::LocalStream);
        assert!(ls.raw_url.is_none());
        assert!(ls.local_path.is_none());

        let lf = CloudFileItem::local_file("a.mp3", false, 100, None, "/cache/a.mp3");
        assert_eq!(lf.delivery, ContentDeliveryKind::LocalFile);
        assert!(lf.local_path.is_some());
        assert!(lf.raw_url.is_none());
    }

    #[test]
    fn discriminant_distinct() {
        // The three kinds must have distinct discriminants — callers rely on
        // `match item.delivery` routing to a single arm.
        assert_ne!(
            ContentDeliveryKind::NetworkDirect as usize,
            ContentDeliveryKind::LocalStream as usize,
        );
        assert_ne!(
            ContentDeliveryKind::LocalStream as usize,
            ContentDeliveryKind::LocalFile as usize,
        );
        assert_ne!(
            ContentDeliveryKind::NetworkDirect as usize,
            ContentDeliveryKind::LocalFile as usize,
        );
    }
}
