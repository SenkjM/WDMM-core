//! 115 Network (115网盘) driver — 115 Open Platform API.
//!
//! Auth: user-supplied Bearer access_token from the 115 Open Platform.
//! No credentials are embedded in source (constraints §5).
//!
//! Reference: https://open.115.com
//!
//! Implements: list(), get() (with raw_url via download info API),
//! mkdir() — `/open/folder/create`,
//! rename() — `/open/file/edit` (same-parent name change),
//! remove() — `/open/rbfile/delete` (recycle bin).

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use reqwest::Client;
use serde_json::Value;

const OPEN115_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Safari/537.36 Chrome/142.0.0.0 OpenList/425.6.30";

/// Token-refresh endpoint (Open Platform passport API).
///
/// 115 rotates the refresh token on every exchange, so the caller must persist
/// **both** returned tokens.
const OPEN115_REFRESH_URL: &str = "https://passportapi.115.com/open/refreshToken";

use crate::driver::{WdmmVfs, CloudFileItem, DriverError, Result};
use crate::link_cache::{SignedLinkCache, DEFAULT_LINK_TTL};

// ── Config ────────────────────────────────────────────────────────────────────

/// Configuration for the 115 Network driver.
pub struct Open115Config {
    /// User-supplied Bearer access_token (115 Open Platform).
    pub access_token: String,
    /// API base URL. Defaults to `https://proapi.115.com`.
    pub base_url: String,
    /// OAuth2 refresh token (rotated on every refresh; persisted to Core vault).
    pub refresh_token: String,
    /// Token-refresh endpoint. Overridable so tests can point at a mock server;
    /// never exposed in the setup form.
    pub refresh_url: String,
}

impl Default for Open115Config {
    fn default() -> Self {
        Self {
            access_token: String::new(),
            base_url: "https://proapi.115.com".to_string(),
            refresh_token: String::new(),
            refresh_url: OPEN115_REFRESH_URL.to_string(),
        }
    }
}

impl Open115Config {
    pub fn new(access_token: impl Into<String>) -> Self {
        Self {
            access_token: access_token.into(),
            ..Default::default()
        }
    }
}

// ── Driver ────────────────────────────────────────────────────────────────────

pub struct Open115Driver {
    config: Open115Config,
    client: Client,
    /// Signed download urls, reused across ranged reads.
    link_cache: SignedLinkCache,
}

impl Open115Driver {
    pub fn new(config: Open115Config) -> Result<Self> {
        if config.access_token.is_empty() {
            return Err(DriverError::Config("115: missing access_token".into()));
        }
        // Download links require the same User-Agent as the Dart driver.
        let client = Client::builder()
            .user_agent(OPEN115_USER_AGENT)
            .build()
            .map_err(|e| DriverError::Config(e.to_string()))?;
        Ok(Self {
            config,
            client,
            link_cache: SignedLinkCache::new(DEFAULT_LINK_TTL),
        })
    }

    /// Build a driver for the initial-verify / token-exchange flow.
    ///
    /// Unlike [new], this does NOT require `access_token` — the caller is
    /// expected to immediately run [verify_initial_auth], which exchanges the
    /// stored `refresh_token`.
    pub fn new_for_verify(config: Open115Config) -> Result<Self> {
        let client = Client::builder()
            .user_agent(OPEN115_USER_AGENT)
            .build()
            .map_err(|e| DriverError::Config(e.to_string()))?;
        Ok(Self {
            config,
            client,
            link_cache: SignedLinkCache::new(DEFAULT_LINK_TTL),
        })
    }

    /// Resolve the signed download url (cached while fresh).
    async fn content_link(&self, path: &str) -> Result<(String, Vec<(String, String)>)> {
        if let Some(hit) = self.link_cache.get(path) {
            return Ok(hit);
        }
        let item = self.get(path).await?;
        let url = item
            .raw_url
            .ok_or_else(|| DriverError::Network("115: missing download url".into()))?;
        let headers = item.raw_headers;
        self.link_cache.store(path, url.clone(), headers.clone());
        Ok((url, headers))
    }

    /// GET a (possibly ranged) slice from the signed link.
    ///
    /// An expired link is rejected with 401/403/410; that invalidates the cache
    /// and the request is retried once with a re-resolved url.
    async fn fetch_bytes(
        &self,
        path: &str,
        range: Option<(u64, Option<u64>)>,
    ) -> Result<Vec<u8>> {
        for attempt in 0..2 {
            let (url, headers) = self.content_link(path).await?;
            let mut request = self.client.get(url);
            if let Some((start, end)) = range {
                let value = match end {
                    Some(end) => format!("bytes={start}-{end}"),
                    None => format!("bytes={start}-"),
                };
                request = request.header(reqwest::header::RANGE, value);
            }
            for (name, value) in &headers {
                request = request.header(name, value);
            }
            let response = request.send().await?;
            let status = response.status();
            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
                || status.as_u16() == 410
            {
                self.link_cache.invalidate(path);
                if attempt == 0 {
                    continue;
                }
                return Err(DriverError::AuthFailed);
            }
            if status == reqwest::StatusCode::NOT_FOUND {
                return Err(DriverError::NotFound(path.into()));
            }
            if !status.is_success() {
                return Err(DriverError::Network(format!("GET failed: {status}")));
            }
            return Ok(response.bytes().await?.to_vec());
        }
        Err(DriverError::Network("115: link retry exhausted".into()))
    }

    /// Exchange the stored `refresh_token` for a fresh `(access_token,
    /// refresh_token)` pair.
    ///
    /// Wire format: `POST {refresh_url}` with
    /// `application/x-www-form-urlencoded` body `refresh_token=…` and **no**
    /// Bearer auth (the access token is what we are exchanging). 115 rotates the
    /// refresh token on every call, so callers must persist both values.
    pub async fn refresh_access_token(&self) -> Result<(String, String)> {
        let refresh = self.config.refresh_token.trim();
        if refresh.is_empty() {
            return Err(DriverError::Config(
                "err.open115MissingRefreshToken".into(),
            ));
        }
        let resp = self
            .client
            .post(&self.config.refresh_url)
            .form(&[("refresh_token", refresh)])
            .send()
            .await
            .map_err(|e| DriverError::Network(e.to_string()))?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN
        {
            return Err(DriverError::AuthFailed);
        }
        if !status.is_success() {
            return Err(DriverError::Network(format!("HTTP {status}")));
        }
        let body: Value = resp
            .json()
            .await
            .map_err(|e| DriverError::Network(format!("non-JSON: {e}")))?;
        let code = body["code"]
            .as_i64()
            .or_else(|| body["code"].as_str().and_then(|s| s.parse().ok()));
        // Some deployments nest the tokens under `data`; accept both shapes.
        let payload = body
            .get("data")
            .filter(|v| v.is_object())
            .unwrap_or(&body);
        let access = payload["access_token"]
            .as_str()
            .filter(|s| !s.is_empty());
        let new_refresh = payload["refresh_token"]
            .as_str()
            .filter(|s| !s.is_empty());
        let code_ok = matches!(code, None | Some(0));
        match (code_ok, access, new_refresh) {
            (true, Some(access), Some(refresh)) => {
                Ok((access.to_string(), refresh.to_string()))
            }
            _ => {
                let message = body["message"].as_str().unwrap_or("");
                Err(DriverError::Network(format!(
                    "115 token refresh failed (code {} {message})",
                    code.map(|c| c.to_string()).unwrap_or_else(|| "?".into())
                )))
            }
        }
    }

    /// Initial-auth flow for account setup / auth retry: exchanges the stored
    /// `refresh_token` and returns the credential patches that must be merged
    /// into the account config and persisted to the Core vault.
    pub async fn verify_initial_auth(
        &self,
    ) -> Result<std::collections::HashMap<String, String>> {
        let (access_token, refresh_token) = self.refresh_access_token().await?;
        let mut patches = std::collections::HashMap::new();
        patches.insert("access_token".into(), access_token);
        patches.insert("refresh_token".into(), refresh_token);
        Ok(patches)
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.config.access_token)
    }

    /// POST form-urlencoded fields with Bearer auth.
    async fn post_form(&self, url: &str, fields: &[(&str, &str)]) -> Result<Value> {
        let resp = self.client.post(url)
            .header("Authorization", self.auth_header())
            .form(fields).send().await.map_err(|e| DriverError::Network(e.to_string()))?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED || resp.status() == reqwest::StatusCode::FORBIDDEN {
            return Err(DriverError::AuthFailed);
        }
        if !resp.status().is_success() { return Err(DriverError::Network(format!("HTTP {}", resp.status()))); }
        resp.json::<Value>().await.map_err(|e| DriverError::Network(e.to_string()))
    }

    async fn list_cid(&self, cid: &str) -> Result<Vec<Value>> {
        let url = format!("{}/open/ufile/files", self.config.base_url);
        let mut offset = 0usize;
        let mut all = Vec::new();
        let mut previous_page_size = None;
        loop {
            let offset_text = offset.to_string();
            let params = [("cid", cid), ("limit", "200"), ("offset", offset_text.as_str()), ("asc", "1"), ("o", "file_name"), ("showDir", "1")];
            let json = self.get_json(&url, &params).await?;
            let page = json["data"].as_array().cloned().unwrap_or_default();
            let len = page.len();
            if len == 0 { break; }
            let count = json["count"].as_u64().or_else(|| json["count"].as_str().and_then(|v| v.parse().ok())).unwrap_or(0) as usize;
            if previous_page_size == Some(len) && count == 0 { break; }
            previous_page_size = Some(len);
            all.extend(page);
            offset += len;
            if count > 0 && offset >= count { break; }
        }
        Ok(all)
    }

    /// GET with Bearer auth and return JSON. Retries once on 429/5xx.
    async fn get_json(&self, url: &str, params: &[(&str, &str)]) -> Result<Value> {
        let do_req = || {
            self.client
                .get(url)
                .header("Authorization", self.auth_header())
                .query(params)
                .send()
        };
        let resp = do_req().await.map_err(|e| DriverError::Network(e.to_string()))?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(DriverError::AuthFailed);
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(DriverError::NotFound("resource".into()));
        }
        if status.as_u16() == 429 || status.is_server_error() {
            let resp2 = do_req().await.map_err(|e| DriverError::Network(e.to_string()))?;
            if !resp2.status().is_success() {
                return Err(DriverError::Network(format!("HTTP {}", resp2.status())));
            }
            return resp2.json::<Value>().await.map_err(|e| DriverError::Network(e.to_string()));
        }
        if !status.is_success() {
            return Err(DriverError::Network(format!("HTTP {}", status)));
        }
        resp.json::<Value>().await.map_err(|e| DriverError::Network(e.to_string()))
    }

    /// POST JSON body and return JSON. Retries once on 429/5xx.
    async fn post_json(&self, url: &str, body: &Value) -> Result<Value> {
        let do_req = || {
            self.client
                .post(url)
                .header("Authorization", self.auth_header())
                .header("Content-Type", "application/json")
                .json(body)
                .send()
        };
        let resp = do_req().await.map_err(|e| DriverError::Network(e.to_string()))?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(DriverError::AuthFailed);
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(DriverError::NotFound("resource".into()));
        }
        if status.as_u16() == 429 || status.is_server_error() {
            let resp2 = do_req().await.map_err(|e| DriverError::Network(e.to_string()))?;
            if !resp2.status().is_success() {
                return Err(DriverError::Network(format!("HTTP {}", resp2.status())));
            }
            return resp2.json::<Value>().await.map_err(|e| DriverError::Network(e.to_string()));
        }
        if !status.is_success() {
            return Err(DriverError::Network(format!("HTTP {}", status)));
        }
        resp.json::<Value>().await.map_err(|e| DriverError::Network(e.to_string()))
    }

    /// Resolve a path to an item's id. Lists the parent directory.
    async fn path_to_item_id(&self, path: &str) -> Result<(String, bool)> {
        let trimmed = path.trim_end_matches('/');
        let (parent, name) = match trimmed.rfind('/') {
            Some(0) | None => ("/", trimmed.trim_start_matches('/')),
            Some(idx) => (&trimmed[..idx], &trimmed[idx + 1..]),
        };
        let cid = self.path_to_cid(parent).await?;
        let items = self.list_cid(&cid.to_string()).await?;
        let entry = items.iter().find(|item| item["fn"].as_str() == Some(name))
            .ok_or_else(|| DriverError::NotFound(path.to_string()))?;
        let id = entry["fid"].as_str().map(str::to_owned).or_else(|| entry["fid"].as_u64().map(|n| n.to_string()))
            .ok_or_else(|| DriverError::Network("115: missing fid".into()))?;
        Ok((id, entry["fc"].as_str() == Some("0")))
    }

    /// The 115 Open API uses numeric folder IDs. Resolve a slash-path to a
    /// cid (category id). The root is cid=0.
    async fn path_to_cid(&self, path: &str) -> Result<u64> {
        let segments: Vec<&str> = path
            .trim_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();

        let mut cid: u64 = 0;
        for seg in &segments {
            let items = self.list_cid(&cid.to_string()).await?;
            let found = items.iter().find(|item| item["fn"].as_str() == Some(seg) && item["fc"].as_str() == Some("0"));
            match found {
                Some(item) => {
                    cid = item["fid"].as_u64().or_else(|| item["fid"].as_str().and_then(|s| s.parse().ok()))
                        .ok_or_else(|| DriverError::Network("115: missing fid".into()))?;
                }
                None => return Err(DriverError::NotFound(path.to_string())),
            }
        }
        Ok(cid)
    }

    /// Parse a single 115 list item into a CloudFileItem.
    fn parse_item(item: &Value) -> Option<CloudFileItem> {
        let name = item["fn"].as_str()?.to_string();
        let is_dir = item["fc"].as_str() == Some("0");
        let size = item["fs"]
            .as_u64()
            .or_else(|| item["fs"].as_str().and_then(|s| s.parse().ok()))
            .unwrap_or(0);
        let mtime = item["upt"]
            .as_i64()
            .or_else(|| item["upt"].as_str().and_then(|s| s.parse().ok()));
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
}

#[async_trait]
impl WdmmVfs for Open115Driver {
    /// Token exchange through `dyn WdmmVfs` (used by `create_token_driver`).
    async fn verify_initial_auth(
        &self,
    ) -> Result<std::collections::HashMap<String, String>> {
        Open115Driver::verify_initial_auth(self).await
    }

    async fn list(&self, path: &str) -> Result<Vec<CloudFileItem>> {
        let cid = self.path_to_cid(path).await?;
        let items = self.list_cid(&cid.to_string()).await?;
        Ok(items.iter().filter_map(Self::parse_item).collect())
    }

    async fn get(&self, path: &str) -> Result<CloudFileItem> {
        // Split parent dir and file name
        let trimmed = path.trim_end_matches('/');
        let (parent, name) = match trimmed.rfind('/') {
            Some(0) | None => ("/", trimmed.trim_start_matches('/')),
            Some(idx) => (&trimmed[..idx], &trimmed[idx + 1..]),
        };

        // Find the file's fid by listing parent
        let cid = self.path_to_cid(parent).await?;
        let items = self.list_cid(&cid.to_string()).await?;
        let entry = items.iter().find(|item| item["fn"].as_str() == Some(name))
            .ok_or_else(|| DriverError::NotFound(path.to_string()))?;

        let size = entry["fs"]
            .as_u64()
            .or_else(|| entry["fs"].as_str().and_then(|s| s.parse().ok()))
            .unwrap_or(0);
        let mtime = entry["upt"]
            .as_i64()
            .or_else(|| entry["upt"].as_str().and_then(|s| s.parse().ok()));
        let modified = mtime.and_then(|ts| Utc.timestamp_opt(ts, 0).single());

        let pick_code = entry["pc"]
            .as_str()
            .ok_or_else(|| DriverError::Network("115: missing pick_code".into()))?;
        let dl_url = format!("{}/open/ufile/downurl", self.config.base_url);
        let dl_json = self.post_form(&dl_url, &[("pick_code", pick_code)]).await?;
        let raw_url = dl_json["data"].as_object().and_then(|m| m.values().next())
            .and_then(|v| v["url"]["url"].as_str())
            .ok_or_else(|| DriverError::Network("115: missing download url".into()))?
            .to_string();

        Ok(CloudFileItem {
            name: name.to_string(),
            is_dir: false,
            size,
            modified,
            raw_url: Some(raw_url),
            raw_headers: vec![("User-Agent".into(), OPEN115_USER_AGENT.into())],
            delivery: crate::driver::ContentDeliveryKind::NetworkDirect,
            local_path: None,
        })
    }

    /// Create a directory at `path`.
    ///
    /// Uses `POST /open/folder/create` with `pid` (parent cid) and `cname`
    /// (folder name). Parent must already exist; use "/" for root.
    async fn mkdir(&self, path: &str) -> Result<()> {
        let trimmed = path.trim_end_matches('/');
        let (parent_path, name) = match trimmed.rfind('/') {
            None | Some(0) => ("/", trimmed.trim_start_matches('/')),
            Some(idx) => (&trimmed[..idx], &trimmed[idx + 1..]),
        };
        if name.is_empty() {
            return Err(DriverError::Config("115 mkdir: empty folder name".into()));
        }
        let parent_cid = self.path_to_cid(parent_path).await?;
        let url = format!("{}/open/folder/add", self.config.base_url);
        let pid = parent_cid.to_string();
        self.post_form(&url, &[("pid", &pid), ("file_name", name)]).await?;
        Ok(())
    }

    /// Rename a file or folder (same-parent name change only).
    ///
    /// Uses `POST /open/file/edit` with `fid` and `file_name`. The `fid` field
    /// also accepts folder cids for the 115 Open Platform.
    async fn rename(&self, old_path: &str, new_path: &str) -> Result<()> {
        let (id, _) = self.path_to_item_id(old_path).await?;
        let (new_parent, new_name) = split_parent_name(new_path)?;
        let old_parent = split_parent_name(old_path)?.0;
        if new_parent != old_parent {
            let parent_id = self.path_to_cid(new_parent).await?.to_string();
            let url = format!("{}/open/ufile/move", self.config.base_url);
            self.post_form(&url, &[("file_ids", &id), ("to_cid", &parent_id)]).await?;
        }
        let old_name = old_path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
        if old_name != new_name {
            let url = format!("{}/open/ufile/update", self.config.base_url);
            self.post_form(&url, &[("file_id", &id), ("file_name", new_name)]).await?;
        }
        Ok(())
    }

    /// Move a file or folder to the recycle bin.
    ///
    /// Uses `POST /open/rbfile/delete` with `fid[]` containing the item id.
    async fn remove(&self, path: &str) -> Result<()> {
        let (id, is_dir) = self.path_to_item_id(path).await?;
        let parent_id = self.path_to_cid(split_parent_name(path)?.0).await?.to_string();
        let url = format!("{}/open/ufile/delete", self.config.base_url);
        let ids = id;
        let _ = is_dir;
        self.post_form(&url, &[("file_ids", &ids), ("parent_id", &parent_id)]).await?;
        Ok(())
    }

    async fn get_content(&self, path: &str) -> Result<Vec<u8>> {
        self.fetch_bytes(path, None).await
    }

    async fn get_content_range(&self, path: &str, start: u64, end: Option<u64>) -> Result<Vec<u8>> {
        self.fetch_bytes(path, Some((start, end))).await
    }
}

fn split_parent_name(path: &str) -> Result<(&str, &str)> {
    let trimmed = path.trim_end_matches('/');
    let (parent, name) = match trimmed.rfind('/') {
        None | Some(0) => ("/", trimmed.trim_start_matches('/')),
        Some(idx) => (&trimmed[..idx], &trimmed[idx + 1..]),
    };
    if name.is_empty() { return Err(DriverError::Config("115: empty path name".into())); }
    Ok((parent, name))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_requires_token() {
        let cfg = Open115Config::default();
        assert!(matches!(Open115Driver::new(cfg), Err(DriverError::Config(_))));
    }

    #[test]
    fn config_ok_with_token() {
        let cfg = Open115Config::new("tok");
        assert!(Open115Driver::new(cfg).is_ok());
    }

    #[test]
    fn parse_item_file() {
        let v = serde_json::json!({"fid":"fid123","pid":"0","fc":"1","fn":"track.mp3","pc":"pick123","fs":5_000_000u64,"upt":1700000000i64});
        let item = Open115Driver::parse_item(&v).unwrap();
        assert_eq!(item.name, "track.mp3");
        assert!(!item.is_dir);
        assert_eq!(item.size, 5_000_000);
        assert!(item.modified.is_some());
    }

    #[test]
    fn parse_item_folder() {
        let v = serde_json::json!({"fid":"999","pid":"0","fc":"0","fn":"Music","fs":0u64,"upt":1700000000i64});
        let item = Open115Driver::parse_item(&v).unwrap();
        assert_eq!(item.name, "Music");
        assert!(item.is_dir);
    }

    #[test]
    fn mkdir_empty_name_is_config_error() {
        // path "/" → name="" → Config error without any HTTP
        let cfg = Open115Config::new("tok");
        let driver = Open115Driver::new(cfg).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt.block_on(driver.mkdir("/")).unwrap_err();
        assert!(matches!(err, DriverError::Config(_)), "expected Config err, got {err:?}");
    }

    // ── Mock HTTP helpers ─────────────────────────────────────────────────────
    //
    // Serves responses in order; repeats the last one when the list is exhausted.

    async fn start_mock_server(responses: Vec<String>) -> u16 {
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
                    let mut buf = vec![0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let request = String::from_utf8_lossy(&buf);
                    assert!(request.contains("/open/ufile/files") || request.contains("/open/folder/add") || request.contains("/open/ufile/update") || request.contains("/open/ufile/delete"), "unexpected Open115 endpoint: {request}");
                    if request.starts_with("POST ") {
                        assert!(request.to_ascii_lowercase().contains("content-type: application/x-www-form-urlencoded"), "expected form request: {request}");
                    }
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                }
            }
        });
        port
    }

    #[tokio::test]
    async fn list_parses_mock_response() {
        let body = serde_json::json!({"state":true,"data":[{"fid":"fid1","pid":"0","fc":"1","fn":"song.mp3","pc":"pc1","fs":4000000u64,"upt":1700000000i64},{"fid":"888","pid":"0","fc":"0","fn":"Albums","pc":"","fs":0u64,"upt":1700000000i64}],"count":2}).to_string();
        let port = start_mock_server(vec![body]).await;
        tokio::task::yield_now().await;
        let mut cfg = Open115Config::new("test_token");
        cfg.base_url = format!("http://127.0.0.1:{}", port);
        let driver = Open115Driver::new(cfg).unwrap();
        let items = driver.list("/").await.expect("list should succeed");
        assert_eq!(items.len(), 2);
        assert!(items.iter().any(|item| item.name == "Albums" && item.is_dir));
        assert!(items.iter().any(|item| item.name == "song.mp3" && item.size == 4_000_000));
    }

    #[tokio::test]
    async fn list_auth_error_on_401() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let _ = sock
                    .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
            }
        });
        tokio::task::yield_now().await;

        let mut cfg = Open115Config::new("bad");
        cfg.base_url = format!("http://127.0.0.1:{}", port);
        let driver = Open115Driver::new(cfg).unwrap();

        let err = driver.list("/").await.unwrap_err();
        assert!(matches!(err, DriverError::AuthFailed));
    }

    // ── Token exchange ────────────────────────────────────────────────────────

    /// One-shot mock server that answers any request with [body].
    async fn start_plain_mock_server(body: String) -> u16 {
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
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        port
    }

    #[tokio::test]
    async fn refresh_rotates_both_tokens() {
        let body =
            serde_json::json!({"code": 0, "data": {"access_token": "a2", "refresh_token": "r2"}})
                .to_string();
        let port = start_plain_mock_server(body).await;
        tokio::task::yield_now().await;

        let mut cfg = Open115Config::new("");
        cfg.refresh_token = "r1".into();
        cfg.refresh_url = format!("http://127.0.0.1:{port}/open/refreshToken");
        let driver = Open115Driver::new_for_verify(cfg).unwrap();

        let patches = driver.verify_initial_auth().await.unwrap();
        assert_eq!(patches.get("access_token").map(String::as_str), Some("a2"));
        assert_eq!(
            patches.get("refresh_token").map(String::as_str),
            Some("r2")
        );
    }

    #[tokio::test]
    async fn refresh_reports_code_failure() {
        let body =
            serde_json::json!({"code": 401, "message": "invalid refresh token"}).to_string();
        let port = start_plain_mock_server(body).await;
        tokio::task::yield_now().await;

        let mut cfg = Open115Config::new("");
        cfg.refresh_token = "bad".into();
        cfg.refresh_url = format!("http://127.0.0.1:{port}/open/refreshToken");
        let driver = Open115Driver::new_for_verify(cfg).unwrap();

        let err = driver.verify_initial_auth().await.unwrap_err();
        match err {
            DriverError::Network(msg) => {
                assert!(msg.contains("401"), "unexpected message: {msg}");
                assert!(msg.contains("invalid refresh token"));
            }
            other => panic!("expected Network, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn refresh_requires_refresh_token() {
        let driver = Open115Driver::new_for_verify(Open115Config::default()).unwrap();
        assert!(matches!(
            driver.verify_initial_auth().await,
            Err(DriverError::Config(_))
        ));
    }

    // ── Write-op mock tests ───────────────────────────────────────────────────

    /// mkdir at root level uses the form-encoded Dart endpoint.
    #[tokio::test]
    async fn mkdir_root_level() {
        let resp = serde_json::json!({
            "state": true,
            "file_id": "new_folder_cid",
            "cname": "NewAlbum"
        })
        .to_string();
        let port = start_mock_server(vec![resp]).await;
        tokio::task::yield_now().await;

        let mut cfg = Open115Config::new("tok");
        cfg.base_url = format!("http://127.0.0.1:{}", port);
        let driver = Open115Driver::new(cfg).unwrap();

        driver.mkdir("/NewAlbum").await.expect("mkdir should succeed");
    }

    /// rename a root-level file — list then form POST to `/open/ufile/update`.
    #[tokio::test]
    async fn rename_root_file() {
        // response 1: list cid=0 to find fid of "song.mp3"
        let list_resp = serde_json::json!({"state":true,"data":[{"fid":"fid_song","pid":"0","fc":"1","fn":"song.mp3","pc":"pick_song","fs":1000000u64,"upt":1700000000i64}],"count":1}).to_string();
        // response 2: POST /open/file/edit → ok
        let edit_resp =
            serde_json::json!({ "state": true, "file_id": "fid_song", "file_name": "renamed.mp3" })
                .to_string();

        let port = start_mock_server(vec![list_resp, edit_resp]).await;
        tokio::task::yield_now().await;

        let mut cfg = Open115Config::new("tok");
        cfg.base_url = format!("http://127.0.0.1:{}", port);
        let driver = Open115Driver::new(cfg).unwrap();

        driver
            .rename("/song.mp3", "/renamed.mp3")
            .await
            .expect("rename should succeed");
    }

    /// remove a root-level file — list then form POST to `/open/ufile/delete`.
    #[tokio::test]
    async fn remove_root_file() {
        let list_resp = serde_json::json!({
            "state": true,
            "data": [{"fid":"fid_del","pid":"0","fc":"1","fn":"old.mp3","pc":"pick_del","fs":500000u64,"upt":1700000000i64}], "count": 1
        })
        .to_string();
        let del_resp = serde_json::json!({ "state": true }).to_string();

        let port = start_mock_server(vec![list_resp, del_resp]).await;
        tokio::task::yield_now().await;

        let mut cfg = Open115Config::new("tok");
        cfg.base_url = format!("http://127.0.0.1:{}", port);
        let driver = Open115Driver::new(cfg).unwrap();

        driver
            .remove("/old.mp3")
            .await
            .expect("remove should succeed");
    }

    /// A signed 115 download url is valid for a while; ranged reads must not
    /// re-list the parent and re-request `downurl` on every chunk.
    #[tokio::test]
    async fn ranged_reads_reuse_the_signed_link() {
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let log = Arc::new(Mutex::new(Vec::new()));
        let log2 = Arc::clone(&log);
        tokio::spawn(async move {
            loop {
                if let Ok((mut sock, _)) = listener.accept().await {
                    let mut buf = vec![0u8; 8192];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    let first = head.lines().next().unwrap_or("").to_string();
                    log2.lock().unwrap().push(first.clone());
                    let path = first.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let body = if path.starts_with("/open/ufile/files") {
                        serde_json::json!({
                            "state": true,
                            "data": [{
                                "fid": "f1",
                                "pid": "0",
                                "fc": "1",
                                "fn": "track.mp3",
                                "pc": "pick1",
                                "fs": 3u64,
                                "upt": 1700000000i64
                            }],
                            "count": 1
                        })
                        .to_string()
                    } else if path.starts_with("/open/ufile/downurl") {
                        serde_json::json!({
                            "state": true,
                            "data": {"f1": {"url": {"url": format!("http://127.0.0.1:{port}/dl/f")}}}
                        })
                        .to_string()
                    } else if path.starts_with("/dl/f") {
                        "abc".to_string()
                    } else {
                        serde_json::json!({"state": true, "data": {}}).to_string()
                    };
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                }
            }
        });
        tokio::task::yield_now().await;

        let mut cfg = Open115Config::new("test_token");
        cfg.base_url = format!("http://127.0.0.1:{port}");
        let driver = Open115Driver::new(cfg).unwrap();
        driver.get_content_range("/track.mp3", 0, Some(2)).await.unwrap();
        driver.get_content_range("/track.mp3", 0, Some(2)).await.unwrap();

        let reqs = log.lock().unwrap().clone();
        let lists = reqs.iter().filter(|r| r.contains("/open/ufile/files")).count();
        let urls = reqs.iter().filter(|r| r.contains("/open/ufile/downurl")).count();
        let dl = reqs.iter().filter(|r| r.starts_with("GET /dl/f ")).count();
        assert_eq!(lists, 1, "parent listing must happen once: {reqs:?}");
        assert_eq!(urls, 1, "downurl must be resolved once: {reqs:?}");
        assert_eq!(dl, 2, "both ranged reads hit the data url: {reqs:?}");
    }
}
