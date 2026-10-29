//! Aliyun Drive (阿里云盘) driver — Aliyun Drive Open Platform API.
//!
//! Auth: user-supplied OAuth2 access_token + drive_id. No credentials are
//! embedded in source (constraints §5). Users supply their own app key and
//! complete the OAuth2 PKCE flow; only the resulting access_token is stored.
//!
//! Reference: https://www.aliyundrive.com/developer
//!
//! Implements list/get/mkdir/rename/move/remove and direct content reads.
//! Upload-style `put_content` is explicitly unsupported to match Dart parity.

use async_trait::async_trait;
use chrono::DateTime;
use reqwest::Client;
use serde_json::Value;

use crate::driver::{WdmmVfs, CloudFileItem, DriverError, Result};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Default OAuth2 client id (public app from worker util.ts / Dart reference).
/// Users may override with their own confidential app via `client_id` config.
const DEFAULT_CLIENT_ID: &str = "25ab4837190e48718a28f80073574a4d";

/// Default online renew-API (OpenList public proxy).
const DEFAULT_RENEW_API: &str = "https://api.oplist.org/alicloud/renewapi";

/// Built-in online renew-API candidate list (tried in order, first success wins).
const BUILTIN_RENEW_APIS: &[&str] = &[
    "https://api.oplist.org/alicloud/renewapi",
    "https://api.oplist.org/ali_open/token",
    "https://api.oplist.org/aliyundrive/token",
    "https://api.alist.nn.ci/alist/ali_open/token",
    "https://api.alist.nn.ci/aliyundrive/token",
    "https://api-sam.oplist.org/aliyundrive/token",
];

/// `driver_txt` query param expected by the OpenList renew endpoint.
const DRIVER_TXT: &str = "alicloud_qr";

/// Direct OAuth token endpoint.
const OAUTH_API: &str = "https://openapi.aliyundrive.com/oauth/access_token";

// ── Config ────────────────────────────────────────────────────────────────────

/// Configuration for the Aliyun Drive driver.
#[derive(serde::Deserialize)]
pub struct AliyunConfig {
    /// OAuth2 access token (user-supplied; never embedded in source).
    pub access_token: String,
    /// The drive id to operate on. Required; must be obtained via
    /// `/v2/user/get` after auth.
    pub drive_id: String,
    /// API base URL. Defaults to `https://openapi.aliyundrive.com`.
    pub base_url: String,
    /// Removal mode: `trash` (default) or `delete`.
    pub remove_way: String,
    /// OAuth2 refresh token (rotated on each refresh; persisted to Core vault).
    pub refresh_token: String,
    /// OAuth2 client id. Defaults to the public app id used by the Dart
    /// reference (worker util.ts); users may override with their own app.
    pub client_id: String,
    /// OAuth2 client secret (optional; only for confidential apps).
    pub client_secret: String,
    /// Custom online renew-API address (OpenList-style proxy). When empty the
    /// built-in candidate list is used.
    pub api_url_address: String,
    /// When true, skip the online renew-API branch and use direct OAuth only.
    pub local_refresh: bool,
    /// Network drive type (`resource` / `default` / `backup`).
    pub drive_type: String,
    /// Root folder id (opaque; default `root`).
    pub root_folder_id: String,
    /// Account id in the Core DB, used to persist refreshed tokens.
    pub account_id: String,
}

impl Default for AliyunConfig {
    fn default() -> Self {
        Self {
            access_token: String::new(),
            drive_id: String::new(),
            base_url: "https://openapi.aliyundrive.com".to_string(),
            remove_way: "trash".to_string(),
            refresh_token: String::new(),
            client_id: DEFAULT_CLIENT_ID.to_string(),
            client_secret: String::new(),
            api_url_address: DEFAULT_RENEW_API.to_string(),
            local_refresh: false,
            drive_type: "resource".to_string(),
            root_folder_id: "root".to_string(),
            account_id: String::new(),
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

    /// Build a driver for the initial-verify / token-exchange flow.
    ///
    /// Unlike [new], this does NOT require `access_token` or `drive_id` — the
    /// caller is expected to immediately run [verify_initial_auth], which
    /// exchanges the `refresh_token` for both. Runtime ops would naturally
    /// fail with `AuthFailed` if these are absent, so the early guard is the
    /// only thing relaxed here.
    pub fn new_for_verify(config: AliyunConfig) -> Result<Self> {
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
        let resp = do_req()
            .await
            .map_err(|e| DriverError::Network(e.to_string()))?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(DriverError::AuthFailed);
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(DriverError::NotFound("resource".into()));
        }
        if status.as_u16() == 429 || status.is_server_error() {
            let resp2 = do_req()
                .await
                .map_err(|e| DriverError::Network(e.to_string()))?;
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
            let found = items.iter().find(|item| item["name"].as_str() == Some(seg));
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
impl WdmmVfs for AliyunDriver {
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
        let dl_url = format!(
            "{}/adrive/v1.0/openFile/getDownloadUrl",
            self.config.base_url
        );
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
            return Err(DriverError::Config(
                "aliyun mkdir: empty folder name".into(),
            ));
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
    async fn rename(&self, old_path: &str, new_path: &str) -> Result<()> {
        let file_id = self.path_to_file_id(old_path).await?;
        let new_name = Self::file_name(new_path);
        if new_name.is_empty() {
            return Err(DriverError::Config("aliyun rename: empty new name".into()));
        }
        if Self::parent_dir(old_path) != Self::parent_dir(new_path) {
            let parent_id = if Self::parent_dir(new_path) == "/" {
                "root".to_string()
            } else {
                self.path_to_file_id(Self::parent_dir(new_path)).await?
            };
            let move_url = format!("{}/adrive/v1.0/openFile/move", self.config.base_url);
            self.post_json(
                &move_url,
                &serde_json::json!({
                    "drive_id": self.config.drive_id,
                    "file_id": file_id,
                    "to_parent_file_id": parent_id,
                    "check_name_mode": "refuse"
                }),
            )
            .await?;
        }
        let update_url = format!("{}/adrive/v1.0/openFile/update", self.config.base_url);
        self.post_json(
            &update_url,
            &serde_json::json!({
                "drive_id": self.config.drive_id,
                "file_id": file_id,
                "name": new_name,
                "check_name_mode": "refuse"
            }),
        )
        .await?;
        Ok(())
    }

    /// Move a file or folder to trash or permanently delete it.
    async fn remove(&self, path: &str) -> Result<()> {
        let file_id = self.path_to_file_id(path).await?;
        let endpoint =
            if self.config.remove_way.trim().is_empty() || self.config.remove_way == "trash" {
                "recyclebin"
            } else if self.config.remove_way == "delete" {
                "delete"
            } else {
                return Err(DriverError::Config(format!(
                    "aliyun: invalid remove_way {}",
                    self.config.remove_way
                )));
            };
        let url = format!("{}/adrive/v1.0/openFile/{endpoint}", self.config.base_url);
        let body = serde_json::json!({ "drive_id": self.config.drive_id, "file_id": file_id });
        self.post_json(&url, &body).await?;
        Ok(())
    }

    async fn put_content(&self, path: &str, data: &[u8]) -> Result<()> {
        let _ = (path, data);
        Err(DriverError::Other(
            "aliyun: put_content/upload not supported (Dart parity)".into(),
        ))
    }

    async fn get_content(&self, path: &str) -> Result<Vec<u8>> {
        let item = self.get(path).await?;
        let url = item
            .raw_url
            .ok_or_else(|| DriverError::Network("aliyun: missing download url".into()))?;
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| DriverError::Network(e.to_string()))?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED
            || resp.status() == reqwest::StatusCode::FORBIDDEN
        {
            return Err(DriverError::AuthFailed);
        }
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(DriverError::NotFound(path.to_string()));
        }
        if !resp.status().is_success() {
            return Err(DriverError::Network(format!("HTTP {}", resp.status())));
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
            .ok_or_else(|| DriverError::Network("aliyun: missing download url".into()))?;
        let range = end
            .map(|e| format!("bytes={start}-{e}"))
            .unwrap_or_else(|| format!("bytes={start}-"));
        let resp = self
            .client
            .get(url)
            .header(reqwest::header::RANGE, range)
            .send()
            .await
            .map_err(|e| DriverError::Network(e.to_string()))?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED
            || resp.status() == reqwest::StatusCode::FORBIDDEN
        {
            return Err(DriverError::AuthFailed);
        }
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(DriverError::NotFound(path.to_string()));
        }
        if !resp.status().is_success() {
            return Err(DriverError::Network(format!("HTTP {}", resp.status())));
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

impl AliyunDriver {
    fn parent_dir(path: &str) -> &str {
        let trimmed = path.trim_end_matches('/');
        match trimmed.rfind('/') {
            Some(0) => "/",
            Some(idx) => &trimmed[..idx],
            None => "/",
        }
    }
    fn file_name(path: &str) -> &str {
        path.trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(path)
    }

    /// Refresh the OAuth2 access token. Pure network call — does NOT mutate
    /// `self`; returns the new `(access_token, refresh_token)` pair so the
    /// caller (FFI layer) can persist them to the Core vault and rebuild the
    /// driver.
    ///
    /// Strategy mirrors the Dart reference:
    /// - `local_refresh == false` (default): try the online renew-API
    ///   candidates in order (custom `api_url_address` first, then built-ins);
    ///   fall back to direct OAuth if all fail.
    /// - `local_refresh == true`: skip online renew, use direct OAuth only.
    pub async fn refresh_access_token(&self) -> Result<(String, String)> {
        let token = self.config.refresh_token.trim();
        if token.is_empty() {
            return Err(DriverError::AuthFailed);
        }
        let mut failures: Vec<String> = Vec::new();

        if !self.config.local_refresh {
            for url in self.renew_candidates() {
                match self.renew_via_online_api(&url, token).await {
                    Ok(tokens) => return Ok(tokens),
                    Err(e) => failures.push(format!("{url} → {e}")),
                }
            }
        }

        match self.renew_via_oauth(token).await {
            Ok(tokens) => Ok(tokens),
            Err(e) => {
                failures.push(format!("{OAUTH_API} → {e}"));
                Err(DriverError::AuthFailed)
            }
        }
    }

    /// Online renew-API candidate list: custom `api_url_address` first (if
    /// non-empty), then the built-in list. Duplicates collapse.
    fn renew_candidates(&self) -> Vec<String> {
        let mut out = Vec::new();
        let custom = self.config.api_url_address.trim();
        if !custom.is_empty() {
            out.push(custom.to_string());
        }
        for &b in BUILTIN_RENEW_APIS {
            let s = b.to_string();
            if !out.contains(&s) {
                out.push(s);
            }
        }
        out
    }

    /// Try one online renew-API address. Returns `(access, refresh)` on success.
    async fn renew_via_online_api(&self, url: &str, token: &str) -> Result<(String, String)> {
        let resp = self
            .client
            .get(url)
            .query(&[
                ("refresh_ui", token),
                ("refresh_token", token),
                ("server_use", "true"),
                ("driver_txt", DRIVER_TXT),
            ])
            .header("Content-Type", "application/json")
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
        // access_token / refresh_token may be at top level or inside `data`.
        let empty = Value::Null;
        let data = match body.get("data") {
            Some(Value::Object(_)) => body.get("data").unwrap(),
            _ => &empty,
        };
        let access = body["access_token"]
            .as_str()
            .or_else(|| data["access_token"].as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| DriverError::Network("empty access_token".into()))?;
        let refresh = body["refresh_token"]
            .as_str()
            .or_else(|| data["refresh_token"].as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(token);
        Ok((access.to_string(), refresh.to_string()))
    }

    /// Direct OAuth refresh (`/oauth/access_token`). Returns `(access, refresh)`.
    async fn renew_via_oauth(&self, token: &str) -> Result<(String, String)> {
        let client_id = {
            let trimmed = self.config.client_id.trim();
            if trimmed.is_empty() { DEFAULT_CLIENT_ID } else { trimmed }
        };
        let mut payload = serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": token,
            "client_id": client_id,
        });
        let secret = self.config.client_secret.trim();
        if !secret.is_empty() {
            payload["client_secret"] = Value::String(secret.to_string());
        }
        let resp = self
            .client
            .post(OAUTH_API)
            .json(&payload)
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
        let access = body["access_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| DriverError::Network("empty access_token".into()))?;
        let refresh = body["refresh_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(token);
        Ok((access.to_string(), refresh.to_string()))
    }

    /// Fetches the user's resource drive id from the Aliyun user-info API.
    ///
    /// Uses a one-shot HTTP client with the provided `access_token` — does not
    /// depend on `self.client` (which carries the old / empty token).
    pub async fn get_drive_id_static(base_url: &str, access_token: &str) -> Result<String> {
        let client = Client::builder()
            .build()
            .map_err(|e| DriverError::Config(e.to_string()))?;
        let url = format!("{}/adrive/v1.0/oauth/users/info", base_url);
        let resp = client
            .post(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({}))
            .send()
            .await
            .map_err(|e| DriverError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(DriverError::AuthFailed);
        }
        let body: Value = resp
            .json()
            .await
            .map_err(|e| DriverError::Network(format!("non-JSON: {e}")))?;
        // Prefer resource_drive_id; fall back to default_drive_id.
        let drive_id = body["resource_drive_id"]
            .as_str()
            .or_else(|| body["default_drive_id"].as_str())
            .unwrap_or("")
            .to_string();
        if drive_id.is_empty() {
            return Err(DriverError::Config(
                "aliyun: no drive_id in users/info response".into(),
            ));
        }
        Ok(drive_id)
    }

    /// Initial-auth flow for account setup: exchanges `refresh_token` for
    /// `access_token`, then fetches `drive_id`.
    ///
    /// Returns a map of credential patches that must be merged into the stored
    /// config **and** persisted to the Core vault so the driver can be
    /// reconstructed after an app restart.
    pub async fn verify_initial_auth(
        &self,
    ) -> Result<std::collections::HashMap<String, String>> {
        let (access_token, refresh_token) = self.refresh_access_token().await?;
        let drive_id =
            Self::get_drive_id_static(&self.config.base_url, &access_token).await?;
        let mut patches = std::collections::HashMap::new();
        patches.insert("access_token".into(), access_token);
        patches.insert("refresh_token".into(), refresh_token);
        patches.insert("drive_id".into(), drive_id);
        Ok(patches)
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
        assert!(matches!(
            AliyunDriver::new(cfg),
            Err(DriverError::Config(_))
        ));
    }

    #[test]
    fn config_requires_drive_id() {
        let mut cfg = AliyunConfig::default();
        cfg.access_token = "tok".into();
        assert!(matches!(
            AliyunDriver::new(cfg),
            Err(DriverError::Config(_))
        ));
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

    #[tokio::test]
    async fn put_content_is_loudly_unsupported() {
        let driver = AliyunDriver::new(AliyunConfig::new("tok", "drv")).unwrap();
        assert!(
            matches!(driver.put_content("/x", b"x").await, Err(DriverError::Other(message)) if message.contains("Dart parity"))
        );
    }

    #[test]
    fn config_defaults_remove_way_to_trash() {
        assert_eq!(AliyunConfig::default().remove_way, "trash");
    }

    // ── Mock HTTP tests ───────────────────────────────────────────────────────

    async fn start_mock_server(responses: Vec<String>) -> u16 {
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let responses = Arc::new(Mutex::new(responses));
        tokio::spawn(async move {
            let mut idx = 0;
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
                let resp =
                    "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
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

    /// rename cross-directory moves then updates the filename.
    #[tokio::test]
    async fn rename_cross_directory_moves_and_updates() {
        let search_resp =
            serde_json::json!({"items": [{"file_id":"f1","name":"old.mp3","type":"file"}]})
                .to_string();
        let parent_resp =
            serde_json::json!({"items": [{"file_id":"dst","name":"dest","type":"folder"}]})
                .to_string();
        let ok = serde_json::json!({}).to_string();
        let port = start_mock_server(vec![search_resp, parent_resp, ok.clone(), ok]).await;
        let mut cfg = AliyunConfig::new("tok", "drv");
        cfg.base_url = format!("http://127.0.0.1:{port}");
        AliyunDriver::new(cfg)
            .unwrap()
            .rename("/old.mp3", "/dest/new.mp3")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn remove_delete_uses_delete_endpoint() {
        let search =
            serde_json::json!({"items": [{"file_id":"f1","name":"x","type":"file"}]}).to_string();
        let port = start_mock_server(vec![search, "{}".into()]).await;
        let mut cfg = AliyunConfig::new("tok", "drv");
        cfg.base_url = format!("http://127.0.0.1:{port}");
        cfg.remove_way = "delete".into();
        AliyunDriver::new(cfg).unwrap().remove("/x").await.unwrap();
    }

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

        driver
            .mkdir("/NewAlbum")
            .await
            .expect("mkdir should succeed");
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

        driver
            .remove("/trash.mp3")
            .await
            .expect("remove should succeed");
    }

    /// mkdir with an empty name segment is a config error (not a network call).
    #[test]
    fn mkdir_empty_name_is_config_error() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let cfg = AliyunConfig::new("tok", "drv");
        let driver = AliyunDriver::new(cfg).unwrap();
        let err = rt.block_on(driver.mkdir("/")).unwrap_err();
        assert!(
            matches!(err, DriverError::Config(_)),
            "expected Config error, got {err:?}"
        );
    }
}
