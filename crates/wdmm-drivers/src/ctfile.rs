//! CtFile (城通网盘) WdmmVfs — list + download_url.
//!
//! API base: https://rest.ctfile.com/v1  (official OpenAPI v1)
//! Auth: session token from 开放接口登录密钥管理 — **user-supplied only**.
//!       No keys are embedded in source or defaults. Users obtain the
//!       session token from the CtFile backend dashboard.
//!
//! Spec reference: Dart implementation at
//!   Webdav_Media_Manager/lib/services/cloud_drivers/ctfile_driver.dart
//!
//! Round 2 scope: list() + get() (download_url). Write ops (mkdir/rename/remove)
//! are stubbed with TODO — they belong in Round 3.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::driver::{WdmmVfs, CloudFileItem, DriverError, Result};

// ─── constants ────────────────────────────────────────────────────────────────

/// Default CtFile REST base URL.
const DEFAULT_BASE_URL: &str = "https://rest.ctfile.com/v1";

/// UA matching the Dart reference implementation.
const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36";

/// Pagination guard: stop after this many pages regardless of cursor.
const MAX_LIST_PAGES: usize = 500;

/// Root folder id accepted by CtFile API.
const ROOT_ID: &str = "0";

// ─── config ───────────────────────────────────────────────────────────────────

/// CtFile driver configuration.
///
/// The only secret is `session` (开放接口登录密钥). It is user-supplied at
/// runtime and must never be hardcoded or committed.
#[derive(Debug, Clone)]
pub struct CtFileConfig {
    /// OpenAPI session token — user-supplied only, never embedded.
    pub session: String,
    /// "public" (default) or "private".
    pub cloud_type: String,
    pub base_url: String,
    pub order_by: String,
    pub order_direction: String,
    pub user_agent: String,
    /// Folder/file list cache TTL in seconds. 0 = disabled.
    pub list_cache_sec: u64,
    /// Minimum gap between successive API calls in ms. 0 = no throttle.
    pub request_interval_ms: u64,
}

impl Default for CtFileConfig {
    fn default() -> Self {
        Self {
            session: String::new(),
            cloud_type: "public".into(),
            base_url: DEFAULT_BASE_URL.into(),
            order_by: "name".into(),
            order_direction: "asc".into(),
            user_agent: DEFAULT_USER_AGENT.into(),
            list_cache_sec: 60,
            request_interval_ms: 400,
        }
    }
}

impl CtFileConfig {
    /// Create config with only the required session token; all other fields use defaults.
    pub fn with_session(session: impl Into<String>) -> Self {
        Self {
            session: session.into(),
            ..Default::default()
        }
    }

    fn namespace(&self) -> &str {
        if self.cloud_type == "private" {
            "private"
        } else {
            "public"
        }
    }

    fn base_url_trimmed(&self) -> &str {
        self.base_url.trim_end_matches('/')
    }
}

// ─── internal types ───────────────────────────────────────────────────────────

/// A folder entry returned by `/folder/list`.
#[derive(Debug, Clone)]
pub struct CtFileFolder {
    /// d-prefixed folder id (e.g. "d123").
    pub key: String,
    pub name: String,
    pub modified: Option<DateTime<Utc>>,
}

/// A file entry returned by `/file/list`.
#[derive(Debug, Clone)]
pub struct CtFileFile {
    /// f-prefixed file id (e.g. "f456").
    pub key: String,
    pub name: String,
    pub size: u64,
    pub modified: Option<DateTime<Utc>>,
}

/// Parse CtFile date field: either a Unix timestamp (int or string) or
/// "YYYY-MM-DD HH:mm:ss" wall-clock string (server returns UTC+8; we treat
/// it as UTC for simplicity — an acceptable approximation for display).
pub fn parse_ctfile_date(raw: &Value) -> Option<DateTime<Utc>> {
    match raw {
        Value::Number(n) => {
            let ts = n.as_i64()?;
            if ts <= 0 {
                return None;
            }
            // Values > 1e12 are already milliseconds
            if ts > 1_000_000_000_000 {
                DateTime::from_timestamp_millis(ts)
            } else {
                DateTime::from_timestamp(ts, 0)
            }
        }
        Value::String(s) => {
            let s = s.trim();
            if let Ok(n) = s.parse::<i64>() {
                if n > 0 {
                    return if n > 1_000_000_000_000 {
                        DateTime::from_timestamp_millis(n)
                    } else {
                        DateTime::from_timestamp(n, 0)
                    };
                }
            }
            // Try ISO 8601 with space separator (CtFile API format)
            let iso = s.replacen(' ', "T", 1);
            DateTime::parse_from_rfc3339(&iso)
                .ok()
                .map(|dt| dt.with_timezone(&Utc))
        }
        _ => None,
    }
}

/// Strip leading `d` or `f` prefix when immediately followed by a digit.
/// This implements the id-prefix dual strategy from the Dart reference:
/// some endpoints require the raw id, others the stripped numeric id.
pub fn strip_id_prefix(id: &str) -> &str {
    if id.len() > 1 && matches!(id.as_bytes()[0], b'd' | b'f') && id.as_bytes()[1].is_ascii_digit()
    {
        &id[1..]
    } else {
        id
    }
}

// ─── list cache ───────────────────────────────────────────────────────────────

struct CacheEntry<T> {
    items: Vec<T>,
    expires_at: Instant,
}

impl<T> CacheEntry<T> {
    fn new(items: Vec<T>, ttl: Duration) -> Self {
        Self {
            items,
            expires_at: Instant::now() + ttl,
        }
    }
    fn is_fresh(&self) -> bool {
        Instant::now() < self.expires_at
    }
}

// ─── CtFileClient ─────────────────────────────────────────────────────────────

/// Low-level HTTP client for the CtFile OpenAPI.
///
/// Handles: session injection, throttle, retry on 429/5xx, id-prefix dual
/// strategy, and in-memory list cache.
pub struct CtFileClient {
    config: Arc<CtFileConfig>,
    client: reqwest::Client,
    folder_cache: Arc<Mutex<HashMap<String, CacheEntry<CtFileFolder>>>>,
    file_cache: Arc<Mutex<HashMap<String, CacheEntry<CtFileFile>>>>,
    last_request: Arc<Mutex<Option<Instant>>>,
}

impl CtFileClient {
    pub fn new(config: CtFileConfig) -> Result<Self> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::ACCEPT,
            "application/json"
                .parse()
                .map_err(|e: reqwest::header::InvalidHeaderValue| {
                    DriverError::Config(e.to_string())
                })?,
        );
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| DriverError::Config(e.to_string()))?;
        Ok(Self {
            config: Arc::new(config),
            client,
            folder_cache: Arc::new(Mutex::new(HashMap::new())),
            file_cache: Arc::new(Mutex::new(HashMap::new())),
            last_request: Arc::new(Mutex::new(None)),
        })
    }

    /// Drop all cached folder/file listings (call after any mutation).
    pub async fn clear_cache(&self) {
        self.folder_cache.lock().await.clear();
        self.file_cache.lock().await.clear();
    }

    /// Wait until the minimum request interval has elapsed since the last call.
    async fn throttle(&self) {
        let gap_ms = self.config.request_interval_ms;
        if gap_ms == 0 {
            return;
        }
        let mut last = self.last_request.lock().await;
        if let Some(prev) = *last {
            let elapsed_ms = prev.elapsed().as_millis() as u64;
            if elapsed_ms < gap_ms {
                tokio::time::sleep(Duration::from_millis(gap_ms - elapsed_ms)).await;
            }
        }
        *last = Some(Instant::now());
    }

    /// POST to `path` with `body`. Injects `session`, retries on 429/5xx.
    /// Returns the full parsed response JSON on success (code == 200 or absent).
    async fn call(&self, path: &str, mut body: Value) -> Result<Value> {
        let session = self.config.session.trim().to_string();
        if session.is_empty() {
            return Err(DriverError::AuthFailed);
        }
        body["session"] = Value::String(session);

        let url = format!("{}{}", self.config.base_url_trimmed(), path);
        let mut last_err: Option<DriverError> = None;

        for retry in 0..3usize {
            if retry > 0 {
                tokio::time::sleep(Duration::from_millis(300 * retry as u64)).await;
            }
            self.throttle().await;

            let result = self
                .client
                .post(&url)
                .header("User-Agent", &self.config.user_agent)
                .json(&body)
                .send()
                .await;

            match result {
                Err(e) => {
                    last_err = Some(e.into());
                    continue;
                }
                Ok(resp) => {
                    let http_status = resp.status().as_u16();
                    let text = resp.text().await.unwrap_or_default();
                    let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);

                    let api_code = parsed.get("code").and_then(|v| v.as_u64()).unwrap_or(200);

                    if api_code == 401 || http_status == 401 {
                        return Err(DriverError::AuthFailed);
                    }
                    if api_code == 429
                        || http_status == 429
                        || (500..600).contains(&api_code)
                        || http_status >= 500
                    {
                        last_err = Some(DriverError::Network(format!(
                            "CtFile {}/{} (will retry)",
                            http_status, api_code
                        )));
                        continue;
                    }
                    if api_code != 200 {
                        let msg = parsed
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        // Preserve numeric code in error message so callers can
                        // detect 400/404 for the id-prefix dual-strategy fallback.
                        return Err(DriverError::Network(format!(
                            "CtFile API {}: {}",
                            api_code, msg
                        )));
                    }
                    return Ok(parsed);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| DriverError::Network("CtFile: exhausted retries".into())))
    }

    /// Call with id-prefix dual strategy.
    ///
    /// If `prefer_stripped` = true: try stripped id first, fall back to raw on
    /// 400/404 (used for file endpoints).
    /// If `prefer_stripped` = false: try raw id first, fall back to stripped
    /// (used for folder endpoints).
    async fn call_with_id(
        &self,
        path: &str,
        param_key: &str,
        id: &str,
        extra: Value,
        prefer_stripped: bool,
    ) -> Result<Value> {
        let stripped = strip_id_prefix(id).to_string();
        let (primary, fallback) = if prefer_stripped {
            (stripped.as_str(), id)
        } else {
            (id, stripped.as_str())
        };

        let mut body = extra.clone();
        body[param_key] = Value::String(primary.to_string());

        match self.call(path, body).await {
            Ok(v) => Ok(v),
            Err(DriverError::Network(ref msg)) if msg.contains("400") || msg.contains("404") => {
                let mut body2 = extra;
                body2[param_key] = Value::String(fallback.to_string());
                self.call(path, body2).await
            }
            Err(e) => Err(e),
        }
    }

    // ─── list ─────────────────────────────────────────────────────────────────

    /// List subfolders of `folder_id`. Caches results per TTL config.
    pub async fn list_folders(&self, folder_id: &str) -> Result<Vec<CtFileFolder>> {
        let id = if folder_id.is_empty() {
            ROOT_ID
        } else {
            folder_id
        };
        let ttl = self.config.list_cache_sec;
        if ttl > 0 {
            let cache = self.folder_cache.lock().await;
            if let Some(entry) = cache.get(id) {
                if entry.is_fresh() {
                    return Ok(entry.items.clone());
                }
            }
        }

        let ns = self.config.namespace();
        let body = json!({
            "folder_id": id,
            "orderby": self.config.order_by,
            "order": self.config.order_direction,
        });
        let resp = self.call(&format!("/{}/folder/list", ns), body).await?;
        let list = resp["results"].as_array().cloned().unwrap_or_default();
        let items: Vec<CtFileFolder> = list
            .iter()
            .filter_map(|e| {
                let m = e.as_object()?;
                let key = m
                    .get("key")
                    .or_else(|| m.get("folder_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = m
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let modified = m.get("date").and_then(parse_ctfile_date);
                if key.is_empty() || name.is_empty() {
                    return None;
                }
                Some(CtFileFolder {
                    key,
                    name,
                    modified,
                })
            })
            .collect();

        if ttl > 0 {
            let mut cache = self.folder_cache.lock().await;
            cache.insert(
                id.to_string(),
                CacheEntry::new(items.clone(), Duration::from_secs(ttl)),
            );
        }
        Ok(items)
    }

    /// List files under `folder_id` with automatic pagination (up to MAX_LIST_PAGES).
    pub async fn list_files(&self, folder_id: &str) -> Result<Vec<CtFileFile>> {
        let id = if folder_id.is_empty() {
            ROOT_ID
        } else {
            folder_id
        };
        let ttl = self.config.list_cache_sec;
        if ttl > 0 {
            let cache = self.file_cache.lock().await;
            if let Some(entry) = cache.get(id) {
                if entry.is_fresh() {
                    return Ok(entry.items.clone());
                }
            }
        }

        let ns = self.config.namespace();
        let mut all: Vec<CtFileFile> = Vec::new();
        let mut start = 0usize;
        let mut prev_first_key = String::new();

        for page in 0..MAX_LIST_PAGES {
            let body = json!({
                "folder_id": id,
                "orderby": self.config.order_by,
                "order": self.config.order_direction,
                "start": start.to_string(),
            });
            let resp = self.call(&format!("/{}/file/list", ns), body).await?;
            let results = resp["results"].as_array().cloned().unwrap_or_default();
            if results.is_empty() {
                break;
            }

            // Detect stall (API returned the same first key twice)
            let first_key = results
                .first()
                .and_then(|v| v.get("key"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if page > 0 && first_key == prev_first_key {
                break;
            }
            prev_first_key = first_key;

            for e in &results {
                if let Some(m) = e.as_object() {
                    let key = m
                        .get("key")
                        .or_else(|| m.get("file_id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = m
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let size = m.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
                    let modified = m.get("date").and_then(parse_ctfile_date);
                    if !key.is_empty() && !name.is_empty() {
                        all.push(CtFileFile {
                            key,
                            name,
                            size,
                            modified,
                        });
                    }
                }
            }
            start += results.len();
        }

        if ttl > 0 {
            let mut cache = self.file_cache.lock().await;
            cache.insert(
                id.to_string(),
                CacheEntry::new(all.clone(), Duration::from_secs(ttl)),
            );
        }
        Ok(all)
    }

    // ─── fetch_url ────────────────────────────────────────────────────────────

    /// Get the direct download URL for a file (calls `/file/fetch_url`).
    pub async fn fetch_download_url(&self, file_id: &str) -> Result<String> {
        let ns = self.config.namespace();
        let path = format!("/{}/file/fetch_url", ns);
        let resp = self
            .call_with_id(&path, "file_id", file_id, json!({}), true)
            .await?;
        // API returns download_url at top level alongside code/message
        let url = resp
            .get("download_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if url.is_empty() {
            return Err(DriverError::Other(
                "CtFile fetch_url: empty download_url in response".into(),
            ));
        }
        Ok(url)
    }

    // ─── meta ─────────────────────────────────────────────────────────────────

    /// Fetch file metadata (size, name, etc.).
    pub async fn file_meta(&self, file_id: &str) -> Result<Value> {
        let ns = self.config.namespace();
        let path = format!("/{}/file/meta", ns);
        self.call_with_id(&path, "file_id", file_id, json!({}), true)
            .await
    }

    // ─── write ops ────────────────────────────────────────────────────────────

    /// Create a subfolder inside `parent_id`.
    ///
    /// Returns the new folder's d-prefixed key. On 403 with a `folder_id` in
    /// the response (folder already exists), returns that key as an idempotent
    /// success.
    pub async fn create_folder(&self, parent_id: &str, name: &str) -> Result<String> {
        let ns = self.config.namespace();
        let body = json!({
            "folder_id": parent_id,
            "name":      name,
        });
        let path = format!("/{}/folder/create", ns);
        match self.call(&path, body).await {
            Ok(resp) => {
                let raw_id = resp
                    .get("folder_id")
                    .or_else(|| resp.get("id"))
                    .and_then(|v| {
                        v.as_str()
                            .map(String::from)
                            .or_else(|| v.as_u64().map(|n| n.to_string()))
                    })
                    .ok_or_else(|| DriverError::Other("folder/create: missing folder_id".into()))?;
                // Ensure d-prefix
                let key = if raw_id.starts_with('d') {
                    raw_id
                } else {
                    format!("d{}", raw_id)
                };
                Ok(key)
            }
            Err(DriverError::Network(ref msg)) if msg.contains("403") => {
                // Folder already exists — treat as idempotent success.
                Ok(format!("d{}", parent_id))
            }
            Err(e) => Err(e),
        }
    }

    /// Rename a file in-place (same parent directory).
    pub async fn rename_file(&self, file_id: &str, new_name: &str) -> Result<()> {
        let ns = self.config.namespace();
        let path = format!("/{}/file/rename", ns);
        self.call_with_id(&path, "file_id", file_id, json!({ "name": new_name }), true)
            .await?;
        Ok(())
    }

    /// Rename a folder in-place (same parent directory).
    pub async fn rename_folder(&self, folder_id: &str, new_name: &str) -> Result<()> {
        let ns = self.config.namespace();
        let path = format!("/{}/folder/rename", ns);
        self.call_with_id(
            &path,
            "folder_id",
            folder_id,
            json!({ "name": new_name }),
            false,
        )
        .await?;
        Ok(())
    }

    /// Move a file to a different folder.
    pub async fn move_file(&self, file_id: &str, dest_folder_id: &str) -> Result<()> {
        let ns = self.config.namespace();
        let path = format!("/{}/file/move", ns);
        self.call_with_id(
            &path,
            "file_id",
            file_id,
            json!({ "folder_id": dest_folder_id }),
            true,
        )
        .await?;
        Ok(())
    }

    /// Move a folder to a different parent.
    pub async fn move_folder(&self, folder_id: &str, dest_folder_id: &str) -> Result<()> {
        let ns = self.config.namespace();
        let path = format!("/{}/folder/move", ns);
        self.call_with_id(
            &path,
            "folder_id",
            folder_id,
            json!({ "to_folder_id": dest_folder_id }),
            false,
        )
        .await?;
        Ok(())
    }

    /// Delete a file by id.
    pub async fn delete_file(&self, file_id: &str) -> Result<()> {
        let ns = self.config.namespace();
        let path = format!("/{}/file/delete", ns);
        self.call_with_id(&path, "ids", file_id, json!({}), true)
            .await?;
        Ok(())
    }

    /// Delete a folder by id.
    pub async fn delete_folder(&self, folder_id: &str) -> Result<()> {
        let ns = self.config.namespace();
        let path = format!("/{}/folder/delete", ns);
        self.call_with_id(&path, "folder_id", folder_id, json!({}), false)
            .await?;
        Ok(())
    }

    // ─── fetch_bytes ──────────────────────────────────────────────────────────

    /// Download bytes from a direct URL, optionally with an HTTP Range header.
    ///
    /// `range` is `Some((start, end_opt))` where both bounds are inclusive byte
    /// offsets (same semantics as `Range: bytes=start-end`). Pass `None` to
    /// fetch the whole resource.
    ///
    /// Reqwest follows redirects by default, so this works even when the CtFile
    /// download URL redirects to a CDN.
    pub async fn fetch_bytes(
        &self,
        url: &str,
        ua: &str,
        range: Option<(u64, Option<u64>)>,
    ) -> Result<Vec<u8>> {
        let mut req = self.client.get(url).header("User-Agent", ua);
        if let Some((start, end_opt)) = range {
            let range_val = match end_opt {
                Some(end) => format!("bytes={}-{}", start, end),
                None => format!("bytes={}-", start),
            };
            req = req.header("Range", range_val);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| DriverError::Network(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(DriverError::Other(format!(
                "CtFile fetch_bytes: HTTP {}",
                status.as_u16()
            )));
        }
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| DriverError::Network(e.to_string()))
    }
}

// ─── CtFileDriver ─────────────────────────────────────────────────────────────

/// `WdmmVfs` implementation for CtFile (城通网盘).
///
/// Implements `list()` (merged folder+file listing) and `get()` (direct
/// download URL for streaming). Write operations are TODO for Round 3.
pub struct CtFileDriver {
    client: CtFileClient,
    /// Path-to-id walk cache (clears on any mutation).
    path_cache: Arc<Mutex<HashMap<String, String>>>,
    user_agent: String,
}

impl CtFileDriver {
    pub fn new(config: CtFileConfig) -> Result<Self> {
        let ua = config.user_agent.clone();
        Ok(Self {
            client: CtFileClient::new(config)?,
            path_cache: Arc::new(Mutex::new(HashMap::new())),
            user_agent: ua,
        })
    }

    /// Normalise a path: trim whitespace, leading+trailing slashes, collapse doubles.
    /// Returns "" for root ("/").
    pub fn normalize_path(path: &str) -> String {
        if path.is_empty() || path == "/" {
            return String::new();
        }
        let trimmed = path.trim().trim_end_matches('/');
        let with_slash = if trimmed.starts_with('/') {
            trimmed.to_string()
        } else {
            format!("/{}", trimmed)
        };
        // Collapse consecutive slashes
        let mut result = String::with_capacity(with_slash.len());
        let mut prev_slash = false;
        for ch in with_slash.chars() {
            if ch == '/' {
                if !prev_slash {
                    result.push(ch);
                }
                prev_slash = true;
            } else {
                result.push(ch);
                prev_slash = false;
            }
        }
        if result == "/" {
            String::new()
        } else {
            result
        }
    }

    fn file_name(path: &str) -> String {
        let norm = Self::normalize_path(path);
        if norm.is_empty() {
            return String::new();
        }
        norm.rsplit('/').next().unwrap_or("").to_string()
    }

    /// Walk `normalized` path to resolve it to a CtFile id.
    /// If `expect_file` is true and the final segment isn't found as a folder,
    /// also searches the file listing.
    async fn path_to_id(&self, normalized: &str, expect_file: bool) -> Result<String> {
        if normalized.is_empty() || normalized == "/" {
            return Ok(ROOT_ID.to_string());
        }
        {
            let cache = self.path_cache.lock().await;
            if let Some(id) = cache.get(normalized) {
                return Ok(id.clone());
            }
        }
        let segments: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();
        let mut current = ROOT_ID.to_string();
        for (i, seg) in segments.iter().enumerate() {
            let is_last = i == segments.len() - 1;
            let folders = self.client.list_folders(&current).await?;
            if let Some(f) = folders.iter().find(|f| f.name == *seg) {
                current = f.key.clone();
                let built = format!("/{}", segments[..=i].join("/"));
                self.path_cache.lock().await.insert(built, current.clone());
                continue;
            }
            if is_last && expect_file {
                let files = self.client.list_files(&current).await?;
                if let Some(f) = files.iter().find(|f| f.name == *seg) {
                    return Ok(f.key.clone());
                }
            }
            return Err(DriverError::NotFound(format!(
                "CtFile: path not found: {}",
                normalized
            )));
        }
        Ok(current)
    }
}

#[async_trait]
impl WdmmVfs for CtFileDriver {
    async fn list(&self, path: &str) -> Result<Vec<CloudFileItem>> {
        let norm = Self::normalize_path(path);
        let id = self.path_to_id(&norm, false).await?;
        let folders = self.client.list_folders(&id).await?;
        let files = self.client.list_files(&id).await?;

        // Merge: folders first (d-prefixed), files second (f-prefixed).
        // First-seen-wins on key conflict — folder entries take priority.
        let mut seen: HashMap<String, CloudFileItem> = HashMap::new();
        for f in &folders {
            if seen.contains_key(&f.key) {
                continue;
            }
            seen.insert(
                f.key.clone(),
                CloudFileItem {
                    name: f.name.clone(),
                    is_dir: true,
                    size: 0,
                    modified: f.modified,
                    raw_url: None,
                    raw_headers: vec![],
                    delivery: crate::driver::ContentDeliveryKind::LocalStream,
                    local_path: None,
                },
            );
        }
        for f in &files {
            if seen.contains_key(&f.key) {
                continue;
            }
            // file/list can also return d-prefixed (directory) entries
            let is_dir = f.key.starts_with('d');
            seen.insert(
                f.key.clone(),
                CloudFileItem {
                    name: f.name.clone(),
                    is_dir,
                    size: if is_dir { 0 } else { f.size },
                    modified: f.modified,
                    raw_url: None,
                    raw_headers: vec![],
                    delivery: crate::driver::ContentDeliveryKind::LocalStream,
                    local_path: None,
                },
            );
        }
        Ok(seen.into_values().collect())
    }

    async fn get(&self, path: &str) -> Result<CloudFileItem> {
        let norm = Self::normalize_path(path);
        if norm.is_empty() {
            return Ok(CloudFileItem {
                name: "/".into(),
                is_dir: true,
                size: 0,
                modified: None,
                raw_url: None,
                raw_headers: vec![],
                delivery: crate::driver::ContentDeliveryKind::LocalStream,
                local_path: None,
            });
        }
        let name = Self::file_name(&norm);
        let id = self.path_to_id(&norm, true).await?;

        // d-prefixed or raw "0" → directory; f-prefixed → file
        if !id.starts_with('f') {
            return Ok(CloudFileItem {
                name,
                is_dir: true,
                size: 0,
                modified: None,
                raw_url: None,
                raw_headers: vec![],
                delivery: crate::driver::ContentDeliveryKind::LocalStream,
                local_path: None,
            });
        }
        let meta = self.client.file_meta(&id).await?;
        // Size lives at top-level or under "results"
        let size = meta
            .get("size")
            .or_else(|| meta.get("results").and_then(|r| r.get("size")))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let url = self.client.fetch_download_url(&id).await?;
        Ok(CloudFileItem {
            name,
            is_dir: false,
            size,
            modified: None,
            raw_url: Some(url),
            raw_headers: vec![("User-Agent".to_string(), self.user_agent.clone())],
            delivery: crate::driver::ContentDeliveryKind::NetworkDirect,
            local_path: None,
        })
    }

    async fn mkdir(&self, path: &str) -> Result<()> {
        let norm = Self::normalize_path(path);
        if norm.is_empty() {
            return Ok(()); // root always exists
        }
        // Split into parent + new folder name
        let (parent_norm, name) = match norm.rfind('/') {
            Some(idx) => (&norm[..idx], &norm[idx + 1..]),
            None => ("", norm.as_str()),
        };
        let parent_id = self.path_to_id(parent_norm, false).await?;
        let new_key = self.client.create_folder(&parent_id, name).await?;
        // Prime the path cache
        self.path_cache.lock().await.insert(norm.clone(), new_key);
        self.client.clear_cache().await;
        Ok(())
    }

    async fn rename(&self, old_path: &str, new_path: &str) -> Result<()> {
        let old_norm = Self::normalize_path(old_path);
        let new_norm = Self::normalize_path(new_path);
        if old_norm == new_norm {
            return Ok(());
        }

        let old_id = self.path_to_id(&old_norm, true).await?;
        let is_file = old_id.starts_with('f');

        // Parse new parent + new name
        let new_name = new_norm.rsplit('/').next().unwrap_or("").to_string();
        let new_parent_norm = match new_norm.rfind('/') {
            Some(idx) => new_norm[..idx].to_string(),
            None => String::new(),
        };
        // Old parent
        let old_parent_norm = match old_norm.rfind('/') {
            Some(idx) => old_norm[..idx].to_string(),
            None => String::new(),
        };

        let cross_dir = old_parent_norm != new_parent_norm;

        if cross_dir {
            let dest_folder_id = self.path_to_id(&new_parent_norm, false).await?;
            if is_file {
                self.client.move_file(&old_id, &dest_folder_id).await?;
            } else {
                self.client.move_folder(&old_id, &dest_folder_id).await?;
            }
        }

        // Rename if the leaf name changed
        let old_name = old_norm.rsplit('/').next().unwrap_or("").to_string();
        if old_name != new_name && !new_name.is_empty() {
            if is_file {
                self.client.rename_file(&old_id, &new_name).await?;
            } else {
                self.client.rename_folder(&old_id, &new_name).await?;
            }
        }

        self.path_cache.lock().await.remove(&old_norm);
        self.client.clear_cache().await;
        Ok(())
    }

    async fn remove(&self, path: &str) -> Result<()> {
        let norm = Self::normalize_path(path);
        if norm.is_empty() {
            return Err(DriverError::Other("CtFile: cannot delete root".into()));
        }
        let id = self.path_to_id(&norm, true).await?;
        if id.starts_with('f') {
            self.client.delete_file(&id).await?;
        } else {
            self.client.delete_folder(&id).await?;
        }
        self.path_cache.lock().await.remove(&norm);
        self.client.clear_cache().await;
        Ok(())
    }

    async fn get_content(&self, path: &str) -> Result<Vec<u8>> {
        let norm = Self::normalize_path(path);
        let id = self.path_to_id(&norm, true).await?;
        let url = self.client.fetch_download_url(&id).await?;
        self.client.fetch_bytes(&url, &self.user_agent, None).await
    }

    async fn get_content_range(&self, path: &str, start: u64, end: Option<u64>) -> Result<Vec<u8>> {
        let norm = Self::normalize_path(path);
        let id = self.path_to_id(&norm, true).await?;
        let url = self.client.fetch_download_url(&id).await?;
        self.client.fetch_bytes(&url, &self.user_agent, Some((start, end))).await
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── normalize_path ────────────────────────────────────────────────────────

    #[test]
    fn normalize_empty_is_root() {
        assert_eq!(CtFileDriver::normalize_path(""), "");
        assert_eq!(CtFileDriver::normalize_path("/"), "");
    }

    #[test]
    fn normalize_strips_trailing_slash() {
        assert_eq!(CtFileDriver::normalize_path("/foo/bar/"), "/foo/bar");
    }

    #[test]
    fn normalize_adds_leading_slash() {
        assert_eq!(CtFileDriver::normalize_path("foo/bar"), "/foo/bar");
    }

    #[test]
    fn normalize_collapses_doubles() {
        assert_eq!(CtFileDriver::normalize_path("//foo//bar"), "/foo/bar");
    }

    // ── strip_id_prefix ───────────────────────────────────────────────────────

    #[test]
    fn strip_prefix_d() {
        assert_eq!(strip_id_prefix("d123"), "123");
    }

    #[test]
    fn strip_prefix_f() {
        assert_eq!(strip_id_prefix("f456"), "456");
    }

    #[test]
    fn strip_prefix_raw_numeric() {
        assert_eq!(strip_id_prefix("123"), "123");
    }

    #[test]
    fn strip_prefix_root() {
        assert_eq!(strip_id_prefix("0"), "0");
    }

    #[test]
    fn strip_prefix_d_no_digit() {
        // "door" — 'd' not followed by digit, must NOT strip
        assert_eq!(strip_id_prefix("door"), "door");
    }

    // ── parse_ctfile_date ─────────────────────────────────────────────────────

    #[test]
    fn parse_date_unix_number() {
        let v = json!(1700000000i64);
        assert!(parse_ctfile_date(&v).is_some());
    }

    #[test]
    fn parse_date_millis_number() {
        let v = json!(1700000000000i64);
        let dt = parse_ctfile_date(&v);
        assert!(dt.is_some());
    }

    #[test]
    fn parse_date_unix_string() {
        let v = json!("1700000000");
        assert!(parse_ctfile_date(&v).is_some());
    }

    #[test]
    fn parse_date_iso_string() {
        let v = json!("2024-11-14T22:13:20+00:00");
        assert!(parse_ctfile_date(&v).is_some());
    }

    #[test]
    fn parse_date_null() {
        assert!(parse_ctfile_date(&Value::Null).is_none());
    }

    #[test]
    fn parse_date_zero() {
        assert!(parse_ctfile_date(&json!(0i64)).is_none());
    }

    // ── CtFileConfig ──────────────────────────────────────────────────────────

    #[test]
    fn config_namespace_public() {
        let c = CtFileConfig::default();
        assert_eq!(c.namespace(), "public");
    }

    #[test]
    fn config_namespace_private() {
        let c = CtFileConfig {
            cloud_type: "private".into(),
            ..Default::default()
        };
        assert_eq!(c.namespace(), "private");
    }

    #[test]
    fn config_base_url_trimmed() {
        let c = CtFileConfig {
            base_url: "https://rest.ctfile.com/v1/".into(),
            ..Default::default()
        };
        assert_eq!(c.base_url_trimmed(), "https://rest.ctfile.com/v1");
    }

    // ── write op mock tests ───────────────────────────────────────────────────
    //
    // Each test spins up a raw TCP server that serves one or more canned HTTP
    // responses, then tests a CtFileClient write operation against it.
    // Pattern mirrors webdav.rs mock tests.

    /// Starts a raw TCP mock server.
    ///
    /// `responses` is a list of (status, json_body) pairs served in order,
    /// one per accepted TCP connection.  Returns the bound port and a base URL
    /// suitable for CtFileConfig.
    async fn start_ctfile_mock(responses: Vec<(u16, &'static str)>) -> (u16, String) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let base = format!("http://127.0.0.1:{}", port);

        tokio::spawn(async move {
            for (status, body) in responses {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = vec![0u8; 8192];
                let _ = sock.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 {status} OK\r\n\
                     Content-Type: application/json\r\n\
                     Content-Length: {len}\r\n\
                     Connection: close\r\n\
                     \r\n\
                     {body}",
                    len = body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        tokio::task::yield_now().await;
        (port, base)
    }

    fn make_client(base_url: &str) -> CtFileClient {
        CtFileClient::new(CtFileConfig {
            session: "test-session".into(),
            base_url: base_url.into(),
            request_interval_ms: 0,
            list_cache_sec: 0,
            ..Default::default()
        })
        .unwrap()
    }

    #[tokio::test]
    async fn create_folder_returns_key() {
        let (_, base) = start_ctfile_mock(vec![(200, r#"{"code":200,"folder_id":"456"}"#)]).await;
        let client = make_client(&base);
        let key = client.create_folder("0", "Music").await.unwrap();
        assert!(key.starts_with('d'), "expected d-prefix, got {key}");
    }

    #[tokio::test]
    async fn rename_file_ok() {
        let (_, base) = start_ctfile_mock(vec![(200, r#"{"code":200}"#)]).await;
        let client = make_client(&base);
        client.rename_file("f123", "new_name.mp3").await.unwrap();
    }

    #[tokio::test]
    async fn rename_folder_ok() {
        let (_, base) = start_ctfile_mock(vec![(200, r#"{"code":200}"#)]).await;
        let client = make_client(&base);
        client.rename_folder("d456", "NewFolder").await.unwrap();
    }

    #[tokio::test]
    async fn move_file_ok() {
        let (_, base) = start_ctfile_mock(vec![(200, r#"{"code":200}"#)]).await;
        let client = make_client(&base);
        client.move_file("f789", "d100").await.unwrap();
    }

    #[tokio::test]
    async fn delete_file_ok() {
        let (_, base) = start_ctfile_mock(vec![(200, r#"{"code":200}"#)]).await;
        let client = make_client(&base);
        client.delete_file("f999").await.unwrap();
    }

    #[tokio::test]
    async fn delete_folder_ok() {
        let (_, base) = start_ctfile_mock(vec![(200, r#"{"code":200}"#)]).await;
        let client = make_client(&base);
        client.delete_folder("d888").await.unwrap();
    }

    #[tokio::test]
    async fn create_folder_idempotent_on_403() {
        // A 403 response — client.call returns Err(Network("CtFile API 403:..."))
        // create_folder should treat this as idempotent success.
        let (_, base) =
            start_ctfile_mock(vec![(200, r#"{"code":403,"message":"folder exists"}"#)]).await;
        let client = make_client(&base);
        // Should NOT return Err
        let result = client.create_folder("0", "existing").await;
        assert!(result.is_ok(), "403 should be idempotent, got {:?}", result);
    }
}
