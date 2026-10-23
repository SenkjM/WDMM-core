//! 123 Pan driver — 123 Pan open-api.
//!
//! Auth: user-supplied Bearer access_token. No credentials are embedded in source.
//! Implements: list(), get() (with raw_url),
//! mkdir() — `/api/v1/upload/mkdir`,
//! rename() — `/api/v1/file/rename` (same-parent),
//! remove() — `/api/v1/file/trash`.

use async_trait::async_trait;
use chrono::DateTime;
use reqwest::Client;
use serde_json::Value;

use crate::driver::{CloudDriver, CloudFileItem, DriverError, Result};

/// Configuration for the 123 Pan driver.
pub struct Pan123Config {
    /// User-supplied access token from 123 Pan OAuth (account settings — never embedded).
    pub access_token: String,
    /// API base URL. Defaults to `https://open-api.123pan.com`.
    pub base_url: String,
}

impl Default for Pan123Config {
    fn default() -> Self {
        Self {
            access_token: String::new(),
            base_url: "https://open-api.123pan.com".to_string(),
        }
    }
}

impl Pan123Config {
    pub fn new(access_token: impl Into<String>) -> Self {
        Self {
            access_token: access_token.into(),
            ..Default::default()
        }
    }
}

/// Map a 123 Pan API code to a DriverError.
fn map_code(code: i64) -> DriverError {
    match code {
        401 => DriverError::AuthFailed,
        404 => DriverError::NotFound("resource".into()),
        _ => DriverError::Network(format!("123Pan code {}", code)),
    }
}

/// 123 Pan cloud driver.
pub struct Pan123Driver {
    config: Pan123Config,
    client: Client,
}

impl Pan123Driver {
    pub fn new(config: Pan123Config) -> Result<Self> {
        if config.access_token.is_empty() {
            return Err(DriverError::Config("123pan: missing access_token".into()));
        }
        let client = Client::builder()
            .build()
            .map_err(|e| DriverError::Config(e.to_string()))?;
        Ok(Self { config, client })
    }

    /// Build Authorization header value.
    fn auth_header(&self) -> String {
        format!("Bearer {}", self.config.access_token)
    }

    /// Perform a GET with Bearer auth and return JSON. Retries once on 429/5xx.
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

    /// POST JSON body with Bearer auth and return JSON. Retries once on 429/5xx.
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

    /// List files under a given parentFileId (single page, pageSize=100).
    async fn list_by_id(&self, parent_id: u64) -> Result<Vec<Value>> {
        let parent_str = parent_id.to_string();
        let url = format!("{}/api/v1/file/list", self.config.base_url);
        let params = [
            ("parentFileId", parent_str.as_str()),
            ("page", "1"),
            ("pageSize", "100"),
            ("orderBy", "file_name"),
            ("orderDirection", "asc"),
            ("trashed", "false"),
        ];
        let json = self.get_json(&url, &params).await?;
        let code = json["code"].as_i64().unwrap_or(0);
        if code != 0 {
            return Err(map_code(code));
        }
        Ok(json["data"]["fileList"].as_array().cloned().unwrap_or_default())
    }

    /// Walk a path (split by '/') from root (id=0) and return the fileId of the target.
    /// Returns (fileId, entry Value) for the last segment.
    async fn resolve_path(&self, path: &str) -> Result<(u64, Value)> {
        let segments: Vec<&str> = path
            .trim_start_matches('/')
            .trim_end_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();

        if segments.is_empty() {
            // Root directory
            return Ok((0, serde_json::json!({"fileId": 0, "filename": "", "type": 1})));
        }

        let mut current_parent: u64 = 0;
        let mut found_entry = serde_json::Value::Null;

        for (i, segment) in segments.iter().enumerate() {
            let entries = self.list_by_id(current_parent).await?;
            let entry = entries
                .into_iter()
                .find(|e| e["filename"].as_str() == Some(segment))
                .ok_or_else(|| {
                    DriverError::NotFound(format!("{} (segment {})", path, i))
                })?;
            let file_id = entry["fileId"].as_u64().unwrap_or(0);
            found_entry = entry;
            current_parent = file_id;
        }

        Ok((current_parent, found_entry))
    }

    /// Parse a 123 Pan file list entry into a CloudFileItem.
    fn parse_item(item: &Value) -> Option<CloudFileItem> {
        let name = item["filename"].as_str()?.to_string();
        let file_type = item["type"].as_i64().unwrap_or(0);
        let is_dir = file_type == 1;
        let size = item["size"].as_u64().unwrap_or(0);
        let modified = item["updateAt"]
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc));
        Some(CloudFileItem {
            name,
            is_dir,
            size,
            modified,
            raw_url: None,
            raw_headers: vec![],
        })
    }
}

#[async_trait]
impl CloudDriver for Pan123Driver {
    async fn list(&self, path: &str) -> Result<Vec<CloudFileItem>> {
        let (parent_id, _) = self.resolve_path(path).await?;
        let entries = self.list_by_id(parent_id).await?;
        Ok(entries.iter().filter_map(Self::parse_item).collect())
    }

    async fn get(&self, path: &str) -> Result<CloudFileItem> {
        let (_file_id, entry) = self.resolve_path(path).await?;

        let file_id = entry["fileId"]
            .as_u64()
            .ok_or_else(|| DriverError::Network("123pan: missing fileId".into()))?;
        let s3_key_flag = entry["s3KeyFlag"]
            .as_str()
            .ok_or_else(|| DriverError::Network("123pan: missing s3KeyFlag".into()))?
            .to_string();
        let etag = entry["etag"].as_str().unwrap_or("").to_string();
        let size = entry["size"].as_u64().unwrap_or(0);
        let name = entry["filename"]
            .as_str()
            .unwrap_or(path.rsplit('/').next().unwrap_or(path))
            .to_string();
        let modified = entry["updateAt"]
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc));

        // Fetch download URL.
        let file_id_str = file_id.to_string();
        let url = format!("{}/api/v1/file/download_info", self.config.base_url);
        let params = [
            ("s3KeyFlag", s3_key_flag.as_str()),
            ("fileId", file_id_str.as_str()),
            ("etag", etag.as_str()),
        ];
        let json = self.get_json(&url, &params).await?;
        let code = json["code"].as_i64().unwrap_or(0);
        if code != 0 {
            return Err(map_code(code));
        }
        let download_url = json["data"]["downloadUrl"]
            .as_str()
            .ok_or_else(|| DriverError::Network("123pan: missing downloadUrl".into()))?
            .to_string();

        Ok(CloudFileItem {
            name,
            is_dir: false,
            size,
            modified,
            raw_url: Some(download_url),
            raw_headers: vec![],
        })
    }

    /// Create a directory at the given path.
    ///
    /// Resolves parent to a `parentFileId` then calls
    /// `POST /api/v1/upload/mkdir` with `{"name": …, "parentFileId": …}`.
    async fn mkdir(&self, path: &str) -> Result<()> {
        let trimmed = path.trim_end_matches('/');
        let (parent_path, name) = match trimmed.rfind('/') {
            None | Some(0) => ("/", trimmed.trim_start_matches('/')),
            Some(idx) => (&trimmed[..idx], &trimmed[idx + 1..]),
        };
        if name.is_empty() {
            return Err(DriverError::Config("123pan mkdir: empty folder name".into()));
        }
        let (parent_id, _) = self.resolve_path(parent_path).await?;
        let url = format!("{}/api/v1/upload/mkdir", self.config.base_url);
        let json = self
            .post_json(&url, &serde_json::json!({"name": name, "parentFileId": parent_id}))
            .await?;
        let code = json["code"].as_i64().unwrap_or(0);
        if code != 0 {
            return Err(map_code(code));
        }
        Ok(())
    }

    /// Rename a file or folder (same-parent name change).
    ///
    /// Resolves `old_path` to a `fileId` then calls
    /// `POST /api/v1/file/rename` with `{"fileId": …, "fileName": …}`.
    async fn rename(&self, old_path: &str, new_path: &str) -> Result<()> {
        // Validate new name before any HTTP round-trips.
        let new_name = new_path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| DriverError::Config("123pan rename: empty new name".into()))?;
        let (file_id, _) = self.resolve_path(old_path).await?;
        if file_id == 0 {
            return Err(DriverError::Config("123pan rename: cannot rename root".into()));
        }
        let url = format!("{}/api/v1/file/rename", self.config.base_url);
        let json = self
            .post_json(&url, &serde_json::json!({"fileId": file_id, "fileName": new_name}))
            .await?;
        let code = json["code"].as_i64().unwrap_or(0);
        if code != 0 {
            return Err(map_code(code));
        }
        Ok(())
    }

    /// Move a file or folder to the trash.
    ///
    /// Resolves `path` to a `fileId` then calls
    /// `POST /api/v1/file/trash` with `{"fileTrashInfoList": [{"fileId": …}]}`.
    async fn remove(&self, path: &str) -> Result<()> {
        let (file_id, _) = self.resolve_path(path).await?;
        if file_id == 0 {
            return Err(DriverError::Config("123pan remove: cannot remove root".into()));
        }
        let url = format!("{}/api/v1/file/trash", self.config.base_url);
        let json = self
            .post_json(
                &url,
                &serde_json::json!({"fileTrashInfoList": [{"fileId": file_id}]}),
            )
            .await?;
        let code = json["code"].as_i64().unwrap_or(0);
        if code != 0 {
            return Err(map_code(code));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults() {
        let cfg = Pan123Config::default();
        assert_eq!(cfg.base_url, "https://open-api.123pan.com");
        assert!(cfg.access_token.is_empty());
    }

    #[test]
    fn config_new_sets_token() {
        let cfg = Pan123Config::new("my_token");
        assert_eq!(cfg.access_token, "my_token");
        assert_eq!(cfg.base_url, "https://open-api.123pan.com");
    }

    #[test]
    fn new_requires_access_token() {
        let cfg = Pan123Config::default();
        let result = Pan123Driver::new(cfg);
        assert!(matches!(result, Err(DriverError::Config(_))));
    }

    #[test]
    fn code_mapping() {
        assert!(matches!(map_code(401), DriverError::AuthFailed));
        assert!(matches!(map_code(404), DriverError::NotFound(_)));
        match map_code(500) {
            DriverError::Network(msg) => assert!(msg.contains("500")),
            other => panic!("expected Network, got {:?}", other),
        }
    }

    #[test]
    fn parse_item_file() {
        let item = serde_json::json!({
            "fileId": 1234,
            "filename": "foo.mp3",
            "type": 0,
            "size": 12345,
            "etag": "xxx",
            "s3KeyFlag": "region/key",
            "updateAt": "2024-01-01T00:00:00.000Z"
        });
        let fi = Pan123Driver::parse_item(&item).unwrap();
        assert_eq!(fi.name, "foo.mp3");
        assert!(!fi.is_dir);
        assert_eq!(fi.size, 12345);
        assert!(fi.modified.is_some());
        assert!(fi.raw_url.is_none());
    }

    #[test]
    fn parse_item_dir() {
        let item = serde_json::json!({
            "fileId": 5678,
            "filename": "Music",
            "type": 1,
            "size": 0,
            "etag": "",
            "s3KeyFlag": "",
            "updateAt": "2024-01-01T00:00:00.000Z"
        });
        let fi = Pan123Driver::parse_item(&item).unwrap();
        assert!(fi.is_dir);
    }

    // ── Mock server tests ──────────────────────────────────────────────────────

    async fn start_mock_pan123_server(response_body: String) -> u16 {
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
            "code": 0,
            "message": "ok",
            "data": {
                "fileList": [
                    {
                        "fileId": 1001,
                        "filename": "track.mp3",
                        "type": 0,
                        "size": 9999999,
                        "etag": "abc",
                        "s3KeyFlag": "cn/key1",
                        "updateAt": "2024-06-01T12:00:00.000Z"
                    },
                    {
                        "fileId": 1002,
                        "filename": "Playlists",
                        "type": 1,
                        "size": 0,
                        "etag": "",
                        "s3KeyFlag": "",
                        "updateAt": "2024-06-01T10:00:00.000Z"
                    }
                ],
                "total": 2
            }
        }).to_string();

        let port = start_mock_pan123_server(body).await;
        tokio::task::yield_now().await;

        let mut cfg = Pan123Config::new("test_token");
        cfg.base_url = format!("http://127.0.0.1:{}", port);
        let driver = Pan123Driver::new(cfg).unwrap();

        // list("/") — root, parentFileId=0 — hits the mock server
        let items = driver.list("/").await.expect("list should succeed");
        assert_eq!(items.len(), 2);

        let track = items.iter().find(|i| i.name == "track.mp3").unwrap();
        assert!(!track.is_dir);
        assert_eq!(track.size, 9999999);
        assert!(track.modified.is_some());

        let pl = items.iter().find(|i| i.name == "Playlists").unwrap();
        assert!(pl.is_dir);
    }

    #[tokio::test]
    async fn list_maps_auth_code() {
        let body = serde_json::json!({"code": 401, "message": "unauthorized", "data": {}}).to_string();
        let port = start_mock_pan123_server(body).await;
        tokio::task::yield_now().await;

        let mut cfg = Pan123Config::new("bad_token");
        cfg.base_url = format!("http://127.0.0.1:{}", port);
        let driver = Pan123Driver::new(cfg).unwrap();

        let err = driver.list("/").await.unwrap_err();
        assert!(matches!(err, DriverError::AuthFailed));
    }

    #[tokio::test]
    async fn list_maps_network_code() {
        let body = serde_json::json!({"code": 500, "message": "server error", "data": {}}).to_string();
        let port = start_mock_pan123_server(body).await;
        tokio::task::yield_now().await;

        let mut cfg = Pan123Config::new("token");
        cfg.base_url = format!("http://127.0.0.1:{}", port);
        let driver = Pan123Driver::new(cfg).unwrap();

        let err = driver.list("/").await.unwrap_err();
        assert!(matches!(err, DriverError::Network(_)));
    }

    // ── Multi-response mock server ─────────────────────────────────────────────
    // Serves responses in order; repeats the last one when exhausted.

    async fn start_mock_pan123_multi_server(responses: Vec<String>) -> u16 {
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
                        if idx < r.len() { r[idx].clone() } else { r.last().cloned().unwrap_or_default() }
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

    /// mkdir at root level: resolve_path("/") returns (0, root) without HTTP,
    /// so only one POST to /api/v1/upload/mkdir is needed.
    #[tokio::test]
    async fn mkdir_root_level() {
        let post_resp = serde_json::json!({
            "code": 0,
            "message": "ok",
            "data": { "dirID": 9001 }
        }).to_string();
        let port = start_mock_pan123_multi_server(vec![post_resp]).await;
        tokio::task::yield_now().await;

        let mut cfg = Pan123Config::new("tok");
        cfg.base_url = format!("http://127.0.0.1:{}", port);
        let driver = Pan123Driver::new(cfg).unwrap();

        driver.mkdir("/NewFolder").await.expect("mkdir should succeed");
    }

    #[tokio::test]
    async fn mkdir_empty_name_is_config_error() {
        // path="/" → name="" after trimming, no HTTP needed.
        let cfg = Pan123Config::new("tok");
        let driver = Pan123Driver::new(cfg).unwrap();
        let err = driver.mkdir("/").await.unwrap_err();
        assert!(matches!(err, DriverError::Config(_)));
    }

    #[tokio::test]
    async fn mkdir_maps_auth_code() {
        let post_resp = serde_json::json!({"code": 401, "message": "unauthorized", "data": {}}).to_string();
        let port = start_mock_pan123_multi_server(vec![post_resp]).await;
        tokio::task::yield_now().await;

        let mut cfg = Pan123Config::new("bad");
        cfg.base_url = format!("http://127.0.0.1:{}", port);
        let driver = Pan123Driver::new(cfg).unwrap();

        let err = driver.mkdir("/Fail").await.unwrap_err();
        assert!(matches!(err, DriverError::AuthFailed));
    }

    // ── rename tests ───────────────────────────────────────────────────────────

    /// rename a root-level file: one GET (list parentFileId=0) + one POST rename.
    #[tokio::test]
    async fn rename_root_file() {
        let list_resp = serde_json::json!({
            "code": 0,
            "message": "ok",
            "data": {
                "fileList": [{
                    "fileId": 1001,
                    "filename": "song.mp3",
                    "type": 0,
                    "size": 5000000,
                    "etag": "abc",
                    "s3KeyFlag": "cn/key",
                    "updateAt": "2024-01-01T00:00:00.000Z"
                }],
                "total": 1
            }
        }).to_string();
        let rename_resp = serde_json::json!({"code": 0, "message": "ok", "data": {}}).to_string();

        let port = start_mock_pan123_multi_server(vec![list_resp, rename_resp]).await;
        tokio::task::yield_now().await;

        let mut cfg = Pan123Config::new("tok");
        cfg.base_url = format!("http://127.0.0.1:{}", port);
        let driver = Pan123Driver::new(cfg).unwrap();

        driver
            .rename("/song.mp3", "/renamed.mp3")
            .await
            .expect("rename should succeed");
    }

    #[tokio::test]
    async fn rename_empty_new_name_is_config_error() {
        // Validation fires before any HTTP — no mock needed.
        let cfg = Pan123Config::new("tok");
        let driver = Pan123Driver::new(cfg).unwrap();
        let err = driver.rename("/foo.mp3", "/").await.unwrap_err();
        assert!(matches!(err, DriverError::Config(_)));
    }

    // ── remove tests ───────────────────────────────────────────────────────────

    /// remove a root-level file: one GET (list parentFileId=0) + one POST trash.
    #[tokio::test]
    async fn remove_root_file() {
        let list_resp = serde_json::json!({
            "code": 0,
            "message": "ok",
            "data": {
                "fileList": [{
                    "fileId": 2001,
                    "filename": "old.mp3",
                    "type": 0,
                    "size": 3000000,
                    "etag": "def",
                    "s3KeyFlag": "cn/key2",
                    "updateAt": "2024-01-01T00:00:00.000Z"
                }],
                "total": 1
            }
        }).to_string();
        let trash_resp = serde_json::json!({"code": 0, "message": "ok", "data": {}}).to_string();

        let port = start_mock_pan123_multi_server(vec![list_resp, trash_resp]).await;
        tokio::task::yield_now().await;

        let mut cfg = Pan123Config::new("tok");
        cfg.base_url = format!("http://127.0.0.1:{}", port);
        let driver = Pan123Driver::new(cfg).unwrap();

        driver.remove("/old.mp3").await.expect("remove should succeed");
    }

    #[tokio::test]
    async fn remove_maps_not_found() {
        // resolve_path fails → NotFound before POST
        let list_resp = serde_json::json!({
            "code": 0,
            "message": "ok",
            "data": { "fileList": [], "total": 0 }
        }).to_string();

        let port = start_mock_pan123_multi_server(vec![list_resp]).await;
        tokio::task::yield_now().await;

        let mut cfg = Pan123Config::new("tok");
        cfg.base_url = format!("http://127.0.0.1:{}", port);
        let driver = Pan123Driver::new(cfg).unwrap();

        let err = driver.remove("/ghost.mp3").await.unwrap_err();
        assert!(matches!(err, DriverError::NotFound(_)));
    }
}