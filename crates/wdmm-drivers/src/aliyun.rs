//! Aliyun Drive (阿里云盘) driver — Aliyun Drive Open Platform API.
//!
//! Auth: user-supplied OAuth2 access_token + drive_id. No credentials are
//! embedded in source (constraints §5). Users supply their own app key and
//! complete the OAuth2 PKCE flow; only the resulting access_token is stored.
//!
//! Reference: https://www.aliyundrive.com/developer
//!
//! Implements:
//! - list(), get() (with raw_url via getDownloadUrl)
//! - mkdir()   — `/adrive/v1.0/openFile/create` (type=folder)
//! - rename()  — `/adrive/v1.0/openFile/update` (name)
//! - remove()  — `/adrive/v1.0/openFile/recyclebin/trash`

use async_trait::async_trait;
use chrono::DateTime;
use reqwest::Client;
use serde_json::Value;

use crate::driver::{CloudDriver, CloudFileItem, DriverError, Result};

// ── Config ────────────────────────────────────────────────────────────────────

/// Configuration for the Aliyun Drive driver.
pub struct AliyunConfig {
    /// OAuth2 access token (user-supplied; never embedded in source).
    pub access_token: String,
    /// The drive id to operate on. Required; must be obtained via
    /// `/v2/user/get` after auth.
    pub drive_id: String,
    /// API base URL. Defaults to `https://openapi.aliyundrive.com`.
    pub base_url: String,
}

impl Default for AliyunConfig {
    fn default() -> Self {
        Self {
            access_token: String::new(),
            drive_id: String::new(),
            base_url: "https://openapi.aliyundrive.com".to_string(),
        }
    }
}

impl AliyunConfig {
    pub fn new(access_token: impl Into<String>, drive_id: impl Into<String>) -> Self {
        Self {
            access_token: access_token.into(),
            drive_id: drive_id.into(),
            ..Default::default()
        }
    }
}

// ── Driver ────────────────────────────────────────────────────────────────────

pub struct AliyunDriver {
    config: AliyunConfig,
    client: Client,
}

impl AliyunDriver {
    pub fn new(config: AliyunConfig) -> Result<Self> {
        if config.access_token.is_empty() {
            return Err(DriverError::Config("aliyun: missing access_token".into()));
        }
        if config.drive_id.is_empty() {
            return Err(DriverError::Config("aliyun: missing drive_id".into()));
        }
        let client = Client::builder()
            .build()
            .map_err(|e| DriverError::Config(e.to_string()))?;
        Ok(Self { config, client })
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.config.access_token)
    }

    /// POST JSON and return the response body as Value. Retries once on 429/5xx.
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

    /// Resolve a remote slash-path to a file_id by walking segments.
    /// The root virtual folder is "root".
    async fn path_to_file_id(&self, path: &str) -> Result<String> {
        let segments: Vec<&str> = path
            .trim_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();

        let mut parent_id = "root".to_string();
        for seg in &segments {
            let url = format!("{}/adrive/v1.0/openFile/search", self.config.base_url);
            let body = serde_json::json!({
                "drive_id": self.config.drive_id,
                "query": format!("name = '{}' and parent_file_id = '{}'", seg, parent_id),
                "limit": 10
            });
            let json = self.post_json(&url, &body).await?;
            let items = json["items"].as_array().cloned().unwrap_or_default();
            let found = items.iter().find(|item| {
                item["name"].as_str() == Some(seg)
            });
            match found {
                Some(item) => {
                    parent_id = item["file_id"].as_str().unwrap_or("").to_string();
                }
                None => return Err(DriverError::NotFound(path.to_string())),
            }
        }
        Ok(parent_id)
    }

    /// Parse a single Aliyun Drive list item to CloudFileItem.
    fn parse_item(item: &Value) -> Option<CloudFileItem> {
        let name = item["name"].as_str()?.to_string();
        let ftype = item["type"].as_str().unwrap_or("file");
        let is_dir = ftype == "folder";
        let size = item["size"].as_u64().unwrap_or(0);
        let modified = item["updated_at"]
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
            delivery: crate::driver::ContentDeliveryKind::LocalStream,
            local_path: None,
        })
    }
}

#[async_trait]
impl CloudDriver for AliyunDriver {
    async fn list(&self, path: &str) -> Result<Vec<CloudFileItem>> {
        let parent_id = if path == "/" || path.is_empty() {
            "root".to_string()
        } else {
            self.path_to_file_id(path).await?
        };

        let url = format!("{}/adrive/v1.0/openFile/list", self.config.base_url);
        let body = serde_json::json!({
            "drive_id": self.config.drive_id,
            "parent_file_id": parent_id,
            "limit": 200,
            "order_by": "name",
            "order_direction": "ASC"
        });
        let json = self.post_json(&url, &body).await?;
        let items = json["items"].as_array().cloned().unwrap_or_default();
        Ok(items.iter().filter_map(Self::parse_item).collect())
    }

    async fn get(&self, path: &str) -> Result<CloudFileItem> {
        let file_id = self.path_to_file_id(path).await?;

        // Get file metadata
        let meta_url = format!("{}/adrive/v1.0/openFile/get", self.config.base_url);
        let meta_body = serde_json::json!({
            "drive_id": self.config.drive_id,
            "file_id": file_id
        });
        let meta = self.post_json(&meta_url, &meta_body).await?;
        let name = meta["name"].as_str().unwrap_or("").to_string();
        let size = meta["size"].as_u64().unwrap_or(0);
        let modified = meta["updated_at"]
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc));

        // Get download URL (expires in ~15min per API docs)
        let dl_url = format!("{}/adrive/v1.0/openFile/getDownloadUrl", self.config.base_url);
        let dl_body = serde_json::json!({
            "drive_id": self.config.drive_id,
            "file_id": file_id
        });
        let dl_json = self.post_json(&dl_url, &dl_body).await?;
        let raw_url = dl_json["url"]
            .as_str()
            .ok_or_else(|| DriverError::Network("aliyun: missing download url".into()))?
            .to_string();

        Ok(CloudFileItem {
            name,
            is_dir: false,
            size,
            modified,
            raw_url: Some(raw_url),
            raw_headers: vec![],
            delivery: crate::driver::ContentDeliveryKind::NetworkDirect,
            local_path: None,
        })
    }

    /// Create a directory at `path`.
    ///
    /// Uses `POST /adrive/v1.0/openFile/create` with `type = "folder"`.
    /// If the immediate parent does not exist this returns `DriverError::NotFound`.
    async fn mkdir(&self, path: &str) -> Result<()> {
        let trimmed = path.trim_end_matches('/');
        let (parent_path, name) = match trimmed.rfind('/') {
            None | Some(0) => ("/", trimmed.trim_start_matches('/')),
            Some(idx) => (&trimmed[..idx], &trimmed[idx + 1..]),
        };
        if name.is_empty() {
            return Err(DriverError::Config("aliyun mkdir: empty folder name".into()));
        }
        let parent_id = if parent_path == "/" || parent_path.is_empty() {
            "root".to_string()
        } else {
            self.path_to_file_id(parent_path).await?
        };
        let url = format!("{}/adrive/v1.0/openFile/create", self.config.base_url);
        let body = serde_json::json!({
            "drive_id":        self.config.drive_id,
            "parent_file_id":  parent_id,
            "name":            name,
            "type":            "folder",
            "check_name_mode": "refuse"   // refuse → error on duplicate
        });
        self.post_json(&url, &body).await?;
        Ok(())
    }

    /// Rename/move a file or folder.
    ///
    /// Uses `POST /adrive/v1.0/openFile/update` to change the `name` field.
    /// Only single-segment renames (same parent) are supported; cross-directory
    /// moves are not supported by the Aliyun Drive Open Platform rename API.
    async fn rename(&self, old_path: &str, new_path: &str) -> Result<()> {
        let file_id = self.path_to_file_id(old_path).await?;
        // Derive the new name from new_path (last segment).
        let new_name = new_path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| DriverError::Config("aliyun rename: empty new name".into()))?;
        let url = format!("{}/adrive/v1.0/openFile/update", self.config.base_url);
        let body = serde_json::json!({
            "drive_id": self.config.drive_id,
            "file_id":  file_id,
            "name":     new_name,
            "check_name_mode": "refuse"
        });
        self.post_json(&url, &body).await?;
        Ok(())
    }

    /// Move a file or folder to the recycle bin.
    ///
    /// Uses `POST /adrive/v1.0/openFile/recyclebin/trash`.
    async fn remove(&self, path: &str) -> Result<()> {
        let file_id = self.path_to_file_id(path).await?;
        let url = format!(
            "{}/adrive/v1.0/openFile/recyclebin/trash",
            self.config.base_url
        );
        let body = serde_json::json!({
            "drive_id": self.config.drive_id,
            "file_id":  file_id
        });
        self.post_json(&url, &body).await?;
        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_requires_token() {
        let mut cfg = AliyunConfig::default();
        cfg.drive_id = "drv1".into();
        assert!(matches!(AliyunDriver::new(cfg), Err(DriverError::Config(_))));
    }

    #[test]
    fn config_requires_drive_id() {
        let mut cfg = AliyunConfig::default();
        cfg.access_token = "tok".into();
        assert!(matches!(AliyunDriver::new(cfg), Err(DriverError::Config(_))));
    }

    #[test]
    fn config_ok_with_both() {
        let cfg = AliyunConfig::new("tok", "drv1");
        assert!(AliyunDriver::new(cfg).is_ok());
    }

    #[test]
    fn parse_item_file() {
        let v = serde_json::json!({
            "file_id": "abc123",
            "name": "song.mp3",
            "type": "file",
            "size": 5_000_000u64,
            "updated_at": "2024-01-15T10:00:00Z"
        });
        let item = AliyunDriver::parse_item(&v).unwrap();
        assert_eq!(item.name, "song.mp3");
        assert!(!item.is_dir);
        assert_eq!(item.size, 5_000_000);
        assert!(item.modified.is_some());
    }

    #[test]
    fn parse_item_folder() {
        let v = serde_json::json!({
            "file_id": "folderid",
            "name": "Music",
            "type": "folder",
            "size": 0u64,
            "updated_at": "2024-01-15T10:00:00Z"
        });
        let item = AliyunDriver::parse_item(&v).unwrap();
        assert!(item.is_dir);
    }

    // ── Mock HTTP tests ───────────────────────────────────────────────────────

    async fn start_mock_server(responses: Vec<String>) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        use std::sync::{Arc, Mutex};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let responses = Arc::new(Mutex::new(responses));
        tokio::spawn(async move {
            let mut idx = 0;
            loop {
                if let Ok((mut sock, _)) = listener.accept().await {
                    let body = {
                        let r = responses.lock().unwrap();
                        if idx < r.len() { r[idx].clone() } else { r.last().cloned().unwrap_or_default() }
                    };
                    idx += 1;
                    let mut buf = vec![0u8; 4096];
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

    #[tokio::test]
    async fn list_parses_mock_response() {
        // Response 1: search for root children (parent_file_id='root')
        let list_resp = serde_json::json!({
            "items": [
                {"file_id": "f1", "name": "track.mp3", "type": "file", "size": 1_000_000u64, "updated_at": "2024-01-15T10:00:00Z"},
                {"file_id": "d1", "name": "Albums", "type": "folder", "size": 0u64, "updated_at": "2024-01-15T10:00:00Z"}
            ]
        }).to_string();

        let port = start_mock_server(vec![list_resp]).await;
        tokio::task::yield_now().await;

        let mut cfg = AliyunConfig::new("tok", "drv");
        cfg.base_url = format!("http://127.0.0.1:{}", port);
        let driver = AliyunDriver::new(cfg).unwrap();

        let items = driver.list("/").await.expect("list should succeed");
        assert_eq!(items.len(), 2);
        let track = items.iter().find(|i| i.name == "track.mp3").unwrap();
        assert!(!track.is_dir);
        assert_eq!(track.size, 1_000_000);
        let albums = items.iter().find(|i| i.name == "Albums").unwrap();
        assert!(albums.is_dir);
    }

    #[tokio::test]
    async fn list_returns_auth_error_on_401() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let resp = "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        tokio::task::yield_now().await;

        let mut cfg = AliyunConfig::new("bad", "drv");
        cfg.base_url = format!("http://127.0.0.1:{}", port);
        let driver = AliyunDriver::new(cfg).unwrap();

        let err = driver.list("/").await.unwrap_err();
        assert!(matches!(err, DriverError::AuthFailed));
    }

    // ── Write-op mock tests ───────────────────────────────────────────────────

    /// mkdir at "/" creates a folder at root — needs one POST to /create.
    #[tokio::test]
    async fn mkdir_root_level() {
        // Single response for POST /create → folder created
        let resp = serde_json::json!({
            "file_id": "new_folder_id",
            "name":    "NewAlbum",
            "type":    "folder"
        })
        .to_string();
        let port = start_mock_server(vec![resp]).await;
        tokio::task::yield_now().await;

        let mut cfg = AliyunConfig::new("tok", "drv");
        cfg.base_url = format!("http://127.0.0.1:{}", port);
        let driver = AliyunDriver::new(cfg).unwrap();

        driver.mkdir("/NewAlbum").await.expect("mkdir should succeed");
    }

    /// rename resolves the file_id first, then calls /update.
    /// Mock: two requests — (1) path_to_file_id search → file_id, (2) /update → ok.
    #[tokio::test]
    async fn rename_file() {
        let search_resp = serde_json::json!({
            "items": [{"file_id": "fid42", "name": "old.mp3", "type": "file",
                       "size": 0u64, "updated_at": "2024-01-15T10:00:00Z"}]
        })
        .to_string();
        let update_resp = serde_json::json!({
            "file_id": "fid42",
            "name":    "new.mp3",
            "type":    "file"
        })
        .to_string();
        let port = start_mock_server(vec![search_resp, update_resp]).await;
        tokio::task::yield_now().await;

        let mut cfg = AliyunConfig::new("tok", "drv");
        cfg.base_url = format!("http://127.0.0.1:{}", port);
        let driver = AliyunDriver::new(cfg).unwrap();

        driver
            .rename("/old.mp3", "/new.mp3")
            .await
            .expect("rename should succeed");
    }

    /// remove resolves the file_id then calls /recyclebin/trash.
    /// Mock: (1) path_to_file_id search → file_id, (2) /recyclebin/trash → ok.
    #[tokio::test]
    async fn remove_file() {
        let search_resp = serde_json::json!({
            "items": [{"file_id": "fid99", "name": "trash.mp3", "type": "file",
                       "size": 0u64, "updated_at": "2024-01-15T10:00:00Z"}]
        })
        .to_string();
        let trash_resp = serde_json::json!({ "file_id": "fid99" }).to_string();
        let port = start_mock_server(vec![search_resp, trash_resp]).await;
        tokio::task::yield_now().await;

        let mut cfg = AliyunConfig::new("tok", "drv");
        cfg.base_url = format!("http://127.0.0.1:{}", port);
        let driver = AliyunDriver::new(cfg).unwrap();

        driver.remove("/trash.mp3").await.expect("remove should succeed");
    }

    /// mkdir with an empty name segment is a config error (not a network call).
    #[test]
    fn mkdir_empty_name_is_config_error() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let cfg = AliyunConfig::new("tok", "drv");
        let driver = AliyunDriver::new(cfg).unwrap();
        let err = rt.block_on(driver.mkdir("/")).unwrap_err();
        assert!(matches!(err, DriverError::Config(_)), "expected Config error, got {err:?}");
    }
}
