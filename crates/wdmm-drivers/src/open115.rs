//! 115 Network (115网盘) driver — 115 Open Platform API.
//!
//! Auth: user-supplied Bearer access_token from the 115 Open Platform.
//! No credentials are embedded in source (constraints §5).
//!
//! Reference: https://open.115.com
//!
//! Implements: list(), get() (with raw_url via download info API).
//! Write ops (mkdir/rename/remove): TODO stubs.

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use reqwest::Client;
use serde_json::Value;

use crate::driver::{CloudDriver, CloudFileItem, DriverError, Result};

// ── Config ────────────────────────────────────────────────────────────────────

/// Configuration for the 115 Network driver.
pub struct Open115Config {
    /// User-supplied Bearer access_token (115 Open Platform).
    pub access_token: String,
    /// API base URL. Defaults to `https://proapi.115.com`.
    pub base_url: String,
}

impl Default for Open115Config {
    fn default() -> Self {
        Self {
            access_token: String::new(),
            base_url: "https://proapi.115.com".to_string(),
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
}

impl Open115Driver {
    pub fn new(config: Open115Config) -> Result<Self> {
        if config.access_token.is_empty() {
            return Err(DriverError::Config("115: missing access_token".into()));
        }
        let client = Client::builder()
            .build()
            .map_err(|e| DriverError::Config(e.to_string()))?;
        Ok(Self { config, client })
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.config.access_token)
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
            let url = format!("{}/open/folder/list", self.config.base_url);
            let cid_str = cid.to_string();
            let params = [("cid", cid_str.as_str()), ("limit", "1150")];
            let json = self.get_json(&url, &params).await?;
            let items = json["data"].as_array().cloned().unwrap_or_default();
            let found = items.iter().find(|item| {
                item["n"].as_str() == Some(seg) && item["fc"].as_str() != Some("0")
            });
            match found {
                Some(item) => {
                    cid = item["cid"]
                        .as_u64()
                        .or_else(|| item["cid"].as_str().and_then(|s| s.parse().ok()))
                        .ok_or_else(|| DriverError::Network("115: missing cid".into()))?;
                }
                None => return Err(DriverError::NotFound(path.to_string())),
            }
        }
        Ok(cid)
    }

    /// Parse a single 115 list item into a CloudFileItem.
    fn parse_item(item: &Value) -> Option<CloudFileItem> {
        let name = item["n"].as_str()?.to_string();
        // In the 115 API: folders carry a `cid` and no `fid`/`sha`; files carry
        // `fid` and `sha`. Use that to tell them apart.
        let is_dir = item.get("fid").is_none() && item.get("cid").is_some()
            && item.get("sha").is_none();
        let size = item["s"]
            .as_u64()
            .or_else(|| item["s"].as_str().and_then(|s| s.parse().ok()))
            .unwrap_or(0);
        let mtime = item["te"]
            .as_i64()
            .or_else(|| item["te"].as_str().and_then(|s| s.parse().ok()));
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
}

#[async_trait]
impl CloudDriver for Open115Driver {
    async fn list(&self, path: &str) -> Result<Vec<CloudFileItem>> {
        let cid = self.path_to_cid(path).await?;
        let url = format!("{}/open/folder/list", self.config.base_url);
        let cid_str = cid.to_string();
        let params = [("cid", cid_str.as_str()), ("limit", "1150")];
        let json = self.get_json(&url, &params).await?;
        let items = json["data"].as_array().cloned().unwrap_or_default();
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
        let url = format!("{}/open/folder/list", self.config.base_url);
        let cid_str = cid.to_string();
        let params = [("cid", cid_str.as_str()), ("limit", "1150")];
        let json = self.get_json(&url, &params).await?;
        let items = json["data"].as_array().cloned().unwrap_or_default();
        let entry = items
            .iter()
            .find(|item| item["n"].as_str() == Some(name))
            .ok_or_else(|| DriverError::NotFound(path.to_string()))?;

        let fid = entry["fid"]
            .as_str()
            .ok_or_else(|| DriverError::Network("115: missing fid".into()))?;
        let size = entry["s"]
            .as_u64()
            .or_else(|| entry["s"].as_str().and_then(|s| s.parse().ok()))
            .unwrap_or(0);
        let mtime = entry["te"]
            .as_i64()
            .or_else(|| entry["te"].as_str().and_then(|s| s.parse().ok()));
        let modified = mtime.and_then(|ts| Utc.timestamp_opt(ts, 0).single());

        // Fetch download URL
        let dl_url = format!("{}/open/file/downurl", self.config.base_url);
        let dl_params = [("pick_code", fid)];
        let dl_json = self.get_json(&dl_url, &dl_params).await?;

        // The 115 downurl response: {"state": true, "data": {"url": {"url": "https://..."}}}
        let raw_url = dl_json["data"]["url"]["url"]
            .as_str()
            .ok_or_else(|| DriverError::Network("115: missing download url".into()))?
            .to_string();

        Ok(CloudFileItem {
            name: name.to_string(),
            is_dir: false,
            size,
            modified,
            raw_url: Some(raw_url),
            raw_headers: vec![],
        })
    }

    async fn mkdir(&self, _path: &str) -> Result<()> {
        Err(DriverError::Other(
            "TODO: 115 mkdir — implement via /open/folder/create".into(),
        ))
    }

    async fn rename(&self, _old: &str, _new: &str) -> Result<()> {
        Err(DriverError::Other(
            "TODO: 115 rename — implement via /open/file/edit (file_name)".into(),
        ))
    }

    async fn remove(&self, _path: &str) -> Result<()> {
        Err(DriverError::Other(
            "TODO: 115 remove — implement via /open/rbfile/delete".into(),
        ))
    }
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
        let v = serde_json::json!({
            "fid": "fid123",
            "n": "track.mp3",
            "s": 5_000_000u64,
            "sha": "abc123",
            "te": 1700000000i64
        });
        let item = Open115Driver::parse_item(&v).unwrap();
        assert_eq!(item.name, "track.mp3");
        assert!(!item.is_dir);
        assert_eq!(item.size, 5_000_000);
        assert!(item.modified.is_some());
    }

    #[test]
    fn parse_item_folder() {
        let v = serde_json::json!({
            "cid": "999",
            "n": "Music",
            "s": 0u64,
            "te": 1700000000i64
        });
        let item = Open115Driver::parse_item(&v).unwrap();
        assert_eq!(item.name, "Music");
        assert!(item.is_dir);
    }

    // ── Mock HTTP tests ───────────────────────────────────────────────────────

    async fn start_mock_server(response_body: String) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                if let Ok((mut sock, _)) = listener.accept().await {
                    let mut buf = vec![0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        response_body.len(),
                        response_body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                }
            }
        });
        port
    }

    #[tokio::test]
    async fn list_parses_mock_response() {
        // path "/" → cid=0 already known; one GET to /open/folder/list
        let body = serde_json::json!({
            "state": true,
            "data": [
                {
                    "fid": "fid1",
                    "n": "song.mp3",
                    "s": 4_000_000u64,
                    "sha": "deadbeef",
                    "te": 1700000000i64
                },
                {
                    "cid": "888",
                    "n": "Albums",
                    "s": 0u64,
                    "te": 1700000000i64
                }
            ]
        }).to_string();

        let port = start_mock_server(body).await;
        tokio::task::yield_now().await;

        let mut cfg = Open115Config::new("test_token");
        cfg.base_url = format!("http://127.0.0.1:{}", port);
        let driver = Open115Driver::new(cfg).unwrap();

        let items = driver.list("/").await.expect("list should succeed");
        assert_eq!(items.len(), 2);
        let song = items.iter().find(|i| i.name == "song.mp3").unwrap();
        assert!(!song.is_dir);
        assert_eq!(song.size, 4_000_000);
        let albums = items.iter().find(|i| i.name == "Albums").unwrap();
        assert!(albums.is_dir);
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
                let _ = sock.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
            }
        });
        tokio::task::yield_now().await;

        let mut cfg = Open115Config::new("bad");
        cfg.base_url = format!("http://127.0.0.1:{}", port);
        let driver = Open115Driver::new(cfg).unwrap();

        let err = driver.list("/").await.unwrap_err();
        assert!(matches!(err, DriverError::AuthFailed));
    }
}
