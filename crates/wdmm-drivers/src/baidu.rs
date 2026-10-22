//! Baidu NetDisk driver — Baidu PCS OpenAPI.
//!
//! Auth: user-supplied OAuth2 access_token. No credentials are embedded in source.
//! Implements: list(), get() (with raw_url + auth headers).
//! Write ops (mkdir/rename/remove) are TODO stubs.

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use reqwest::Client;
use serde_json::Value;

use crate::driver::{CloudDriver, CloudFileItem, DriverError, Result};

/// Configuration for the Baidu NetDisk driver.
pub struct BaiduConfig {
    /// User-supplied OAuth2 access token (from account settings — never embedded).
    pub access_token: String,
    /// API base URL. Defaults to `https://pan.baidu.com`.
    pub base_url: String,
}

impl Default for BaiduConfig {
    fn default() -> Self {
        Self {
            access_token: String::new(),
            base_url: "https://pan.baidu.com".to_string(),
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
    fn map_errno(errno: i64) -> DriverError {
        match errno {
            111 | -6 => DriverError::AuthFailed,
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
        })
    }

    /// Send a GET request and return JSON. Retries once on 429/5xx.
    async fn get_json(&self, url: &str, params: &[(&str, &str)]) -> Result<Value> {
        let send = |retry: bool| {
            let _ = retry;
            self.client.get(url).query(params).send()
        };
        let resp = send(false).await.map_err(|e| map_http_error(&e))?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(DriverError::AuthFailed);
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(DriverError::NotFound("resource".into()));
        }
        if status.as_u16() == 429 || status.is_server_error() {
            // Single retry
            let resp2 = send(true).await.map_err(|e| map_http_error(&e))?;
            let s2 = resp2.status();
            if !s2.is_success() {
                return Err(DriverError::Network(format!("HTTP {}", s2)));
            }
            return resp2.json::<Value>().await.map_err(|e| DriverError::Network(e.to_string()));
        }
        if !status.is_success() {
            return Err(DriverError::Network(format!("HTTP {}", status)));
        }
        resp.json::<Value>().await.map_err(|e| DriverError::Network(e.to_string()))
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
        path.trim_end_matches('/').rsplit('/').next().unwrap_or(path)
    }
}

#[async_trait]
impl CloudDriver for BaiduDriver {
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

        // Step 2: fetch dlink via filemeta.
        let fsids_param = format!("[{}]", fs_id);
        let url = format!("{}/rest/2.0/xpan/multimedia", self.config.base_url);
        let params = [
            ("method", "filemeta"),
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

        Ok(CloudFileItem {
            name: name.to_string(),
            is_dir: false,
            size,
            modified,
            raw_url: Some(dlink),
            raw_headers: vec![(
                "Authorization".to_string(),
                format!("Bearer {}", self.config.access_token),
            )],
        })
    }

    async fn mkdir(&self, _path: &str) -> Result<()> {
        Err(DriverError::Other(
            "TODO: Baidu mkdir — implement via /rest/2.0/xpan/file?method=create".into(),
        ))
    }

    async fn rename(&self, _old: &str, _new: &str) -> Result<()> {
        Err(DriverError::Other(
            "TODO: Baidu rename — implement via /rest/2.0/xpan/file?method=rename".into(),
        ))
    }

    async fn remove(&self, _path: &str) -> Result<()> {
        Err(DriverError::Other(
            "TODO: Baidu remove — implement via /rest/2.0/xpan/file?method=delete".into(),
        ))
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
        assert!(matches!(BaiduDriver::map_errno(111), DriverError::AuthFailed));
        assert!(matches!(BaiduDriver::map_errno(-6), DriverError::AuthFailed));
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
        }).to_string();

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
}
