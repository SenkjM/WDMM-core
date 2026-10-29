//! Baidu NetDisk driver — Baidu PCS OpenAPI.
//!
//! Auth: user-supplied OAuth2 access_token. No credentials are embedded in source.
//! Implements: list(), get() (with raw_url + download UA headers),
//! mkdir/rename/remove via filemanager API, get_content/get_content_range.

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use reqwest::Client;
use serde_json::Value;

use crate::driver::{WdmmVfs, CloudFileItem, DriverError, Result};

// ── Constants ─────────────────────────────────────────────────────────────────

/// User-Agent for API requests (aligned with Dart apiUA).
const API_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Safari/537.36 Chrome/142.0.0.0 OpenList/425.6.30";

/// User-Agent required for download links (Dart downloadUA).
const DOWNLOAD_UA: &str = "pan.baidu.com";

/// Default online renew-API (OpenList public proxy).
const DEFAULT_RENEW_API: &str = "https://api.oplist.org/baiduyun/renewapi";

/// Direct OAuth token endpoint.
const OAUTH_API: &str = "https://openapi.baidu.com/oauth/2.0/token";

/// `driver_txt` query param expected by the OpenList renew endpoint.
const DRIVER_TXT: &str = "baiduyun_go";

/// Configuration for the Baidu NetDisk driver.
pub struct BaiduConfig {
    /// User-supplied OAuth2 access token (from account settings — never embedded).
    pub access_token: String,
    /// API base URL. Defaults to `https://pan.baidu.com`.
    pub base_url: String,
    /// OAuth2 refresh token (rotated on each refresh; persisted to Core vault).
    pub refresh_token: String,
    /// OAuth2 client id (required for `local_refresh` direct OAuth).
    pub client_id: String,
    /// OAuth2 client secret (required for `local_refresh` direct OAuth).
    pub client_secret: String,
    /// Custom online renew-API address (OpenList-style proxy). Empty → default.
    pub api_url_address: String,
    /// When true, skip online renew and use direct OAuth (needs client_id/secret).
    pub local_refresh: bool,
    /// Account id in the Core DB, used to persist refreshed tokens.
    pub account_id: String,
}

impl Default for BaiduConfig {
    fn default() -> Self {
        Self {
            access_token: String::new(),
            base_url: "https://pan.baidu.com".to_string(),
            refresh_token: String::new(),
            client_id: String::new(),
            client_secret: String::new(),
            api_url_address: DEFAULT_RENEW_API.to_string(),
            local_refresh: false,
            account_id: String::new(),
        }
    }
}

impl BaiduConfig {
    pub fn new(access_token: impl Into<String>) -> Self {
        Self {
            access_token: access_token.into(),
            ..Default::default()
        }
    }
}

/// Map reqwest HTTP errors to DriverError.
fn map_http_error(e: &reqwest::Error) -> DriverError {
    if e.status() == Some(reqwest::StatusCode::UNAUTHORIZED)
        || e.status() == Some(reqwest::StatusCode::FORBIDDEN)
    {
        DriverError::AuthFailed
    } else if e.status() == Some(reqwest::StatusCode::NOT_FOUND) {
        DriverError::NotFound("resource".into())
    } else {
        DriverError::Network(e.to_string())
    }
}

/// Baidu NetDisk cloud driver.
pub struct BaiduDriver {
    config: BaiduConfig,
    client: Client,
}

impl BaiduDriver {
    pub fn new(config: BaiduConfig) -> Result<Self> {
        if config.access_token.is_empty() {
            return Err(DriverError::Config("baidu: missing access_token".into()));
        }
        let client = Client::builder()
            .build()
            .map_err(|e| DriverError::Config(e.to_string()))?;
        Ok(Self { config, client })
    }

    /// Map Baidu errno values to DriverError.
    /// Maps {111, -6, 20016} (token errors per Dart) to AuthFailed.
    fn map_errno(errno: i64) -> DriverError {
        match errno {
            111 | -6 | 20016 => DriverError::AuthFailed,
            _ => DriverError::Network(format!("Baidu errno {}", errno)),
        }
    }

    /// Parse a Baidu file list entry into a CloudFileItem.
    fn parse_item(item: &Value) -> Option<CloudFileItem> {
        let name = item["server_filename"].as_str()?.to_string();
        let is_dir = item["isdir"].as_i64().unwrap_or(0) == 1;
        let size = item["size"].as_u64().unwrap_or(0);
        let mtime = item["server_mtime"].as_i64();
        let modified = mtime.and_then(|ts| Utc.timestamp_opt(ts, 0).single());
        Some(CloudFileItem {
            name,
            is_dir,
            size,
            modified,
            raw_url: None,
            raw_headers: vec![],
            delivery: crate::driver::ContentDeliveryKind::LocalStream,
            local_path: None,
        })
    }

    /// Send a GET request and return JSON. Retries once on 429/5xx.
    async fn get_json(&self, url: &str, params: &[(&str, &str)]) -> Result<Value> {
        let send = || {
            self.client
                .get(url)
                .query(params)
                .header("User-Agent", API_UA)
                .send()
        };
        let resp = send().await.map_err(|e| map_http_error(&e))?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(DriverError::AuthFailed);
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(DriverError::NotFound("resource".into()));
        }
        if status.as_u16() == 429 || status.is_server_error() {
            // Single retry
            let resp2 = send().await.map_err(|e| map_http_error(&e))?;
            let s2 = resp2.status();
            if !s2.is_success() {
                return Err(DriverError::Network(format!("HTTP {}", s2)));
            }
            return resp2
                .json::<Value>()
                .await
                .map_err(|e| DriverError::Network(e.to_string()));
        }
        if !status.is_success() {
            return Err(DriverError::Network(format!("HTTP {}", status)));
        }
        resp.json::<Value>()
            .await
            .map_err(|e| DriverError::Network(e.to_string()))
    }

    /// POST with query params and form-encoded body. Retries once on 429/5xx.
    /// The access_token is passed as a query param (Baidu convention); the
    /// actual payload (path / filelist / …) goes in the form body.
    async fn post_form(
        &self,
        url: &str,
        query: &[(&str, &str)],
        form: &[(&str, &str)],
    ) -> Result<Value> {
        let do_req = || {
            self.client
                .post(url)
                .query(query)
                .header("User-Agent", API_UA)
                .form(form)
                .send()
        };
        let resp = do_req().await.map_err(|e| map_http_error(&e))?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(DriverError::AuthFailed);
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(DriverError::NotFound("resource".into()));
        }
        if status.as_u16() == 429 || status.is_server_error() {
            let resp2 = do_req().await.map_err(|e| map_http_error(&e))?;
            if !resp2.status().is_success() {
                return Err(DriverError::Network(format!("HTTP {}", resp2.status())));
            }
            return resp2
                .json::<Value>()
                .await
                .map_err(|e| DriverError::Network(e.to_string()));
        }
        if !status.is_success() {
            return Err(DriverError::Network(format!("HTTP {}", status)));
        }
        resp.json::<Value>()
            .await
            .map_err(|e| DriverError::Network(e.to_string()))
    }

    /// List a directory, returning the raw JSON list array.
    async fn list_dir(&self, path: &str) -> Result<Vec<Value>> {
        let url = format!("{}/rest/2.0/xpan/file", self.config.base_url);
        let params = [
            ("method", "list"),
            ("access_token", self.config.access_token.as_str()),
            ("dir", path),
            ("web", "0"),
            ("fo", "0"),
            ("start", "0"),
            ("limit", "1000"),
        ];
        let json = self.get_json(&url, &params).await?;
        let errno = json["errno"].as_i64().unwrap_or(0);
        if errno != 0 {
            return Err(Self::map_errno(errno));
        }
        Ok(json["list"].as_array().cloned().unwrap_or_default())
    }

    /// Extract the parent directory path from a file path.
    fn parent_dir(path: &str) -> &str {
        let trimmed = path.trim_end_matches('/');
        match trimmed.rfind('/') {
            Some(0) => "/",
            Some(idx) => &trimmed[..idx],
            None => "/",
        }
    }

    /// Extract the file name from a path.
    fn file_name(path: &str) -> &str {
        path.trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(path)
    }

    /// Sanitize dlink by removing access_token query parameter (Dart _sanitizeDlink).
    fn sanitize_dlink(url: &str) -> String {
        if url.is_empty() {
            return url.to_string();
        }
        match reqwest::Url::parse(url) {
            Ok(mut parsed) => {
                {
                    let query_pairs: Vec<(String, String)> = parsed
                        .query_pairs()
                        .filter(|(k, _)| k != "access_token")
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect();
                    parsed.query_pairs_mut().clear();
                    for (k, v) in query_pairs {
                        parsed.query_pairs_mut().append_pair(&k, &v);
                    }
                }
                parsed.to_string()
            }
            Err(_) => url.to_string(),
        }
    }
}

#[async_trait]
impl WdmmVfs for BaiduDriver {
    async fn list(&self, path: &str) -> Result<Vec<CloudFileItem>> {
        let items = self.list_dir(path).await?;
        Ok(items.iter().filter_map(Self::parse_item).collect())
    }

    async fn get(&self, path: &str) -> Result<CloudFileItem> {
        // Step 1: find fs_id by listing parent directory.
        let parent = Self::parent_dir(path);
        let name = Self::file_name(path);
        let dir_items = self.list_dir(parent).await?;
        let entry = dir_items
            .iter()
            .find(|item| item["server_filename"].as_str() == Some(name))
            .ok_or_else(|| DriverError::NotFound(path.to_string()))?;

        let fs_id = entry["fs_id"]
            .as_i64()
            .ok_or_else(|| DriverError::Network("baidu: missing fs_id".into()))?;

        let size = entry["size"].as_u64().unwrap_or(0);
        let mtime = entry["server_mtime"].as_i64();
        let modified = mtime.and_then(|ts| Utc.timestamp_opt(ts, 0).single());

        // Step 2: fetch dlink via filemetas (plural, per Dart).
        let fsids_param = format!("[{}]", fs_id);
        let url = format!("{}/rest/2.0/xpan/multimedia", self.config.base_url);
        let params = [
            ("method", "filemetas"), // plural per Dart
            ("access_token", self.config.access_token.as_str()),
            ("fsids", fsids_param.as_str()),
            ("dlink", "1"),
            ("thumb", "0"),
            ("extra", "0"),
            ("needmedia", "0"),
        ];
        let json = self.get_json(&url, &params).await?;
        let errno = json["errno"].as_i64().unwrap_or(0);
        if errno != 0 {
            return Err(Self::map_errno(errno));
        }
        let dlink = json["list"][0]["dlink"]
            .as_str()
            .ok_or_else(|| DriverError::Network("baidu: missing dlink".into()))?
            .to_string();

        // Step 3: HEAD with download UA to follow redirects (Dart pattern).
        let dlink_with_token = format!("{}&access_token={}", dlink, self.config.access_token);
        let head_resp = self
            .client
            .head(&dlink_with_token)
            .header("User-Agent", DOWNLOAD_UA)
            .send()
            .await
            .map_err(|e| map_http_error(&e))?;

        // Take Location header if present (redirect), else use original dlink+token.
        let final_url = head_resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(&dlink_with_token)
            .to_string();

        // Step 4: sanitize access_token from final URL (Dart sanitizeDlink).
        let sanitized_url = Self::sanitize_dlink(&final_url);

        Ok(CloudFileItem {
            name: name.to_string(),
            is_dir: false,
            size,
            modified,
            raw_url: Some(sanitized_url),
            raw_headers: vec![("User-Agent".to_string(), DOWNLOAD_UA.to_string())],
            delivery: crate::driver::ContentDeliveryKind::NetworkDirect,
            local_path: None,
        })
    }

    /// Create a directory at the given absolute path.
    ///
    /// Uses `POST /rest/2.0/xpan/file?method=create` with `isdir=1`.
    async fn mkdir(&self, path: &str) -> Result<()> {
        let url = format!("{}/rest/2.0/xpan/file", self.config.base_url);
        let query = [
            ("method", "create"),
            ("access_token", self.config.access_token.as_str()),
        ];
        let form = [
            ("path", path),
            ("isdir", "1"),
            ("size", "0"),
            ("rtype", "3"),
        ];
        let json = self.post_form(&url, &query, &form).await?;
        let errno = json["errno"].as_i64().unwrap_or(0);
        if errno != 0 {
            return Err(Self::map_errno(errno));
        }
        Ok(())
    }

    /// Rename or move a file or folder using Baidu's filemanager API.
    async fn rename(&self, old_path: &str, new_path: &str) -> Result<()> {
        let new_name = Self::file_name(new_path);
        if new_name.is_empty() {
            return Err(DriverError::Config("baidu rename: empty new name".into()));
        }
        let (opera, entry) = if Self::parent_dir(old_path) == Self::parent_dir(new_path) {
            (
                "rename",
                serde_json::json!({"path": old_path, "newname": new_name}),
            )
        } else {
            (
                "move",
                serde_json::json!({"path": old_path, "dest": Self::parent_dir(new_path), "newname": new_name}),
            )
        };
        let filelist = serde_json::json!([entry]).to_string();
        let url = format!("{}/rest/2.0/xpan/file", self.config.base_url);
        let query = [
            ("method", "filemanager"),
            ("opera", opera),
            ("access_token", self.config.access_token.as_str()),
        ];
        let form = [
            ("async", "0"),
            ("filelist", filelist.as_str()),
            ("ondup", "fail"),
        ];
        let json = self.post_form(&url, &query, &form).await?;
        let errno = json["errno"].as_i64().unwrap_or(0);
        if errno != 0 {
            return Err(Self::map_errno(errno));
        }
        Ok(())
    }

    /// Uses filemanager `opera=delete`.
    async fn remove(&self, path: &str) -> Result<()> {
        let filelist = serde_json::json!([path]).to_string();
        let url = format!("{}/rest/2.0/xpan/file", self.config.base_url);
        let query = [
            ("method", "filemanager"),
            ("opera", "delete"),
            ("access_token", self.config.access_token.as_str()),
        ];
        let form = [
            ("async", "0"),
            ("filelist", filelist.as_str()),
            ("ondup", "fail"),
        ];
        let json = self.post_form(&url, &query, &form).await?;
        let errno = json["errno"].as_i64().unwrap_or(0);
        if errno != 0 {
            return Err(Self::map_errno(errno));
        }
        Ok(())
    }

    async fn put_content(&self, path: &str, data: &[u8]) -> Result<()> {
        let _ = (path, data);
        Err(DriverError::Other(
            "baidu: put_content/upload not supported (Dart parity)".into(),
        ))
    }

    async fn get_content(&self, path: &str) -> Result<Vec<u8>> {
        let item = self.get(path).await?;
        let url = item
            .raw_url
            .ok_or_else(|| DriverError::Network("baidu: missing download url".into()))?;
        let mut req = self.client.get(url);
        for (key, value) in item.raw_headers {
            req = req.header(&key, &value);
        }
        let resp = req.send().await.map_err(|e| map_http_error(&e))?;
        if !resp.status().is_success() {
            return Err(http_status_error(resp.status(), path));
        }
        Ok(resp
            .bytes()
            .await
            .map_err(|e| DriverError::Network(e.to_string()))?
            .to_vec())
    }

    async fn get_content_range(&self, path: &str, start: u64, end: Option<u64>) -> Result<Vec<u8>> {
        let item = self.get(path).await?;
        let url = item
            .raw_url
            .ok_or_else(|| DriverError::Network("baidu: missing download url".into()))?;
        let range = end
            .map(|e| format!("bytes={start}-{e}"))
            .unwrap_or_else(|| format!("bytes={start}-"));
        let mut req = self.client.get(url).header(reqwest::header::RANGE, range);
        for (key, value) in item.raw_headers {
            req = req.header(&key, &value);
        }
        let resp = req.send().await.map_err(|e| map_http_error(&e))?;
        if !resp.status().is_success() {
            return Err(http_status_error(resp.status(), path));
        }
        Ok(resp
            .bytes()
            .await
            .map_err(|e| DriverError::Network(e.to_string()))?
            .to_vec())
    }

    async fn refresh_credentials(&self) -> Result<(String, String)> {
        self.refresh_access_token().await
    }
}

fn http_status_error(status: reqwest::StatusCode, path: &str) -> DriverError {
    match status {
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
            DriverError::AuthFailed
        }
        reqwest::StatusCode::NOT_FOUND => DriverError::NotFound(path.to_string()),
        _ => DriverError::Network(format!("HTTP {status}")),
    }
}

impl BaiduDriver {
    /// Refresh the OAuth2 access token. Pure network call — does NOT mutate
    /// `self`; returns the new `(access_token, refresh_token)` pair so the
    /// caller (FFI layer) can persist them to the Core vault and rebuild the
    /// driver.
    ///
    /// Strategy mirrors the Dart reference:
    /// - `local_refresh == false` (default): GET the online renew-API (custom
    ///   `api_url_address` or the default) — response must contain BOTH
    ///   `refresh_token` and a non-empty `access_token`.
    /// - `local_refresh == true`: direct OAuth using `client_id`/`client_secret`
    ///   (both required); response `refresh_token` required, `access_token`
    ///   may be empty in some Baidu responses.
    pub async fn refresh_access_token(&self) -> Result<(String, String)> {
        let token = self.config.refresh_token.trim();
        if token.is_empty() {
            return Err(DriverError::AuthFailed);
        }

        if !self.config.local_refresh {
            let url = {
                let custom = self.config.api_url_address.trim();
                if !custom.is_empty() { custom } else { DEFAULT_RENEW_API }
            };
            return self.renew_via_online_api(url, token).await;
        }

        self.renew_via_oauth(token).await
    }

    /// Online renew-API branch. Requires BOTH refresh_token and a non-empty
    /// access_token in the response.
    async fn renew_via_online_api(&self, url: &str, token: &str) -> Result<(String, String)> {
        let resp = self
            .client
            .get(url)
            .query(&[
                ("refresh_ui", token),
                ("server_use", "true"),
                ("driver_txt", DRIVER_TXT),
            ])
            .send()
            .await
            .map_err(|e| DriverError::Network(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(DriverError::Network(format!("HTTP {status}")));
        }
        let body: Value = resp
            .json()
            .await
            .map_err(|e| DriverError::Network(format!("non-JSON: {e}")))?;
        let refresh = body["refresh_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| DriverError::Network("empty refresh_token".into()))?;
        let access = body["access_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| DriverError::Network("empty access_token".into()))?;
        Ok((access.to_string(), refresh.to_string()))
    }

    /// Direct OAuth refresh. Requires non-empty `client_id`/`client_secret`.
    async fn renew_via_oauth(&self, token: &str) -> Result<(String, String)> {
        let client_id = self.config.client_id.trim();
        let client_secret = self.config.client_secret.trim();
        if client_id.is_empty() || client_secret.is_empty() {
            return Err(DriverError::Config(
                "baidu: local_refresh needs client_id and client_secret".into(),
            ));
        }
        let resp = self
            .client
            .get(OAUTH_API)
            .query(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", token),
                ("client_id", client_id),
                ("client_secret", client_secret),
            ])
            .send()
            .await
            .map_err(|e| DriverError::Network(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(DriverError::Network(format!("HTTP {status}")));
        }
        let body: Value = resp
            .json()
            .await
            .map_err(|e| DriverError::Network(format!("non-JSON: {e}")))?;
        let refresh = body["refresh_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| DriverError::Network("empty refresh_token".into()))?;
        // Baidu direct OAuth may return an empty access_token string; keep old.
        let access = body["access_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(self.config.access_token.as_str());
        Ok((access.to_string(), refresh.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults() {
        let cfg = BaiduConfig::default();
        assert_eq!(cfg.base_url, "https://pan.baidu.com");
        assert!(cfg.access_token.is_empty());
    }

    #[test]
    fn config_new_sets_token() {
        let cfg = BaiduConfig::new("my_token");
        assert_eq!(cfg.access_token, "my_token");
        assert_eq!(cfg.base_url, "https://pan.baidu.com");
    }

    #[test]
    fn new_requires_access_token() {
        let cfg = BaiduConfig::default();
        let result = BaiduDriver::new(cfg);
        assert!(matches!(result, Err(DriverError::Config(_))));
    }

    #[test]
    fn errno_mapping() {
        assert!(matches!(
            BaiduDriver::map_errno(111),
            DriverError::AuthFailed
        ));
        assert!(matches!(
            BaiduDriver::map_errno(-6),
            DriverError::AuthFailed
        ));
        match BaiduDriver::map_errno(31304) {
            DriverError::Network(msg) => assert!(msg.contains("31304")),
            other => panic!("expected Network, got {:?}", other),
        }
    }

    #[test]
    fn parse_item_file() {
        let item = serde_json::json!({
            "fs_id": 12345,
            "server_filename": "foo.mp3",
            "path": "/Music/foo.mp3",
            "size": 123456,
            "isdir": 0,
            "server_mtime": 1700000000
        });
        let fi = BaiduDriver::parse_item(&item).unwrap();
        assert_eq!(fi.name, "foo.mp3");
        assert!(!fi.is_dir);
        assert_eq!(fi.size, 123456);
        assert!(fi.modified.is_some());
        assert!(fi.raw_url.is_none());
    }

    #[test]
    fn parse_item_dir() {
        let item = serde_json::json!({
            "fs_id": 9999,
            "server_filename": "Music",
            "path": "/Music",
            "size": 0,
            "isdir": 1,
            "server_mtime": 1700000000
        });
        let fi = BaiduDriver::parse_item(&item).unwrap();
        assert!(fi.is_dir);
    }

    #[test]
    fn dlink_sanitization_removes_only_access_token() {
        let url =
            BaiduDriver::sanitize_dlink("https://download.test/file?a=1&access_token=secret&b=2");
        assert!(url.contains("a=1"));
        assert!(url.contains("b=2"));
        assert!(!url.contains("secret"));
    }

    #[tokio::test]
    async fn put_content_is_loudly_unsupported() {
        let driver = BaiduDriver::new(BaiduConfig::new("tok")).unwrap();
        assert!(
            matches!(driver.put_content("/x", b"x").await, Err(DriverError::Other(message)) if message.contains("Dart parity"))
        );
    }

    #[tokio::test]
    async fn cross_directory_rename_uses_filemanager_move() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let captured_task = captured.clone();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0; 8192];
            let n = sock.read(&mut buf).await.unwrap();
            *captured_task.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).into_owned();
            let body = r#"{"errno":0}"#;
            let _ = sock
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .as_bytes(),
                )
                .await;
        });
        let mut cfg = BaiduConfig::new("tok");
        cfg.base_url = format!("http://127.0.0.1:{port}");
        BaiduDriver::new(cfg)
            .unwrap()
            .rename("/a/old", "/b/new")
            .await
            .unwrap();
        let request = captured.lock().unwrap().to_lowercase();
        assert!(request.contains("method=filemanager"));
        assert!(request.contains("opera=move"));
        assert!(
            request.contains("%22dest%22%3a%22%2fb%22")
                || request.contains("%22dest%22%3A%22%2Fb%22")
        );
    }

    // ── Mock server tests ──────────────────────────────────────────────────────

    async fn start_mock_baidu_list_server(response_body: String) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 8192];
                let _ = sock.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        port
    }

    #[tokio::test]
    async fn list_parses_mock_response() {
        let body = serde_json::json!({
            "errno": 0,
            "list": [
                {
                    "fs_id": 1001,
                    "server_filename": "track.mp3",
                    "path": "/Music/track.mp3",
                    "size": 5000000,
                    "isdir": 0,
                    "server_mtime": 1700000000
                },
                {
                    "fs_id": 1002,
                    "server_filename": "Albums",
                    "path": "/Music/Albums",
                    "size": 0,
                    "isdir": 1,
                    "server_mtime": 1700000000
                }
            ]
        })
        .to_string();

        let port = start_mock_baidu_list_server(body).await;
        tokio::task::yield_now().await;

        let mut cfg = BaiduConfig::new("test_token");
        cfg.base_url = format!("http://127.0.0.1:{}", port);
        let driver = BaiduDriver::new(cfg).unwrap();

        let items = driver.list("/Music").await.expect("list should succeed");
        assert_eq!(items.len(), 2);

        let track = items.iter().find(|i| i.name == "track.mp3").unwrap();
        assert!(!track.is_dir);
        assert_eq!(track.size, 5000000);
        assert!(track.modified.is_some());

        let album = items.iter().find(|i| i.name == "Albums").unwrap();
        assert!(album.is_dir);
    }

    #[tokio::test]
    async fn list_maps_errno_to_auth_error() {
        let body = serde_json::json!({ "errno": 111, "list": [] }).to_string();
        let port = start_mock_baidu_list_server(body).await;
        tokio::task::yield_now().await;

        let mut cfg = BaiduConfig::new("bad_token");
        cfg.base_url = format!("http://127.0.0.1:{}", port);
        let driver = BaiduDriver::new(cfg).unwrap();

        let err = driver.list("/").await.unwrap_err();
        assert!(matches!(err, DriverError::AuthFailed));
    }

    #[tokio::test]
    async fn list_maps_errno_network_error() {
        let body = serde_json::json!({ "errno": 31304, "list": [] }).to_string();
        let port = start_mock_baidu_list_server(body).await;
        tokio::task::yield_now().await;

        let mut cfg = BaiduConfig::new("token");
        cfg.base_url = format!("http://127.0.0.1:{}", port);
        let driver = BaiduDriver::new(cfg).unwrap();

        let err = driver.list("/").await.unwrap_err();
        assert!(matches!(err, DriverError::Network(_)));
    }

    // ── Write-op mock helpers ──────────────────────────────────────────────────
    // Serves responses in order; repeats the last one when exhausted.

    async fn start_mock_baidu_multi_server(responses: Vec<String>) -> u16 {
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let responses = Arc::new(Mutex::new(responses));
        tokio::spawn(async move {
            let mut idx = 0usize;
            loop {
                if let Ok((mut sock, _)) = listener.accept().await {
                    let body = {
                        let r = responses.lock().unwrap();
                        if idx < r.len() {
                            r[idx].clone()
                        } else {
                            r.last().cloned().unwrap_or_default()
                        }
                    };
                    idx += 1;
                    let mut buf = vec![0u8; 8192];
                    let _ = sock.read(&mut buf).await;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(), body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                }
            }
        });
        port
    }

    // ── mkdir tests ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn mkdir_succeeds() {
        let body = serde_json::json!({
            "errno": 0,
            "fs_id": 99999,
            "category": 6,
            "path": "/Music/NewAlbum"
        })
        .to_string();
        let port = start_mock_baidu_multi_server(vec![body]).await;
        tokio::task::yield_now().await;

        let mut cfg = BaiduConfig::new("test_token");
        cfg.base_url = format!("http://127.0.0.1:{}", port);
        let driver = BaiduDriver::new(cfg).unwrap();

        driver
            .mkdir("/Music/NewAlbum")
            .await
            .expect("mkdir should succeed");
    }

    #[tokio::test]
    async fn mkdir_maps_errno_to_auth_error() {
        let body = serde_json::json!({ "errno": 111 }).to_string();
        let port = start_mock_baidu_multi_server(vec![body]).await;
        tokio::task::yield_now().await;

        let mut cfg = BaiduConfig::new("bad_token");
        cfg.base_url = format!("http://127.0.0.1:{}", port);
        let driver = BaiduDriver::new(cfg).unwrap();

        let err = driver.mkdir("/Fail").await.unwrap_err();
        assert!(matches!(err, DriverError::AuthFailed));
    }

    // ── rename tests ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn rename_succeeds() {
        let body = serde_json::json!({
            "errno": 0,
            "info": [{"path": "/Music/new.mp3"}]
        })
        .to_string();
        let port = start_mock_baidu_multi_server(vec![body]).await;
        tokio::task::yield_now().await;

        let mut cfg = BaiduConfig::new("tok");
        cfg.base_url = format!("http://127.0.0.1:{}", port);
        let driver = BaiduDriver::new(cfg).unwrap();

        driver
            .rename("/Music/old.mp3", "/Music/new.mp3")
            .await
            .expect("rename should succeed");
    }

    #[tokio::test]
    async fn rename_empty_new_name_is_config_error() {
        // No HTTP needed — validation fires before any request.
        let cfg = BaiduConfig::new("tok");
        let driver = BaiduDriver::new(cfg).unwrap();
        let err = driver.rename("/foo.mp3", "/").await.unwrap_err();
        assert!(matches!(err, DriverError::Config(_)));
    }

    // ── remove tests ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn remove_succeeds() {
        let body = serde_json::json!({ "errno": 0, "info": { "taskid": 1 } }).to_string();
        let port = start_mock_baidu_multi_server(vec![body]).await;
        tokio::task::yield_now().await;

        let mut cfg = BaiduConfig::new("tok");
        cfg.base_url = format!("http://127.0.0.1:{}", port);
        let driver = BaiduDriver::new(cfg).unwrap();

        driver
            .remove("/Music/old.mp3")
            .await
            .expect("remove should succeed");
    }

    #[tokio::test]
    async fn remove_maps_errno_to_network_error() {
        let body = serde_json::json!({ "errno": 31021 }).to_string();
        let port = start_mock_baidu_multi_server(vec![body]).await;
        tokio::task::yield_now().await;

        let mut cfg = BaiduConfig::new("tok");
        cfg.base_url = format!("http://127.0.0.1:{}", port);
        let driver = BaiduDriver::new(cfg).unwrap();

        let err = driver.remove("/gone.mp3").await.unwrap_err();
        assert!(matches!(err, DriverError::Network(_)));
    }
}
