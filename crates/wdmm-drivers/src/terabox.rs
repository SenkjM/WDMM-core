//! Terabox driver — cookie-based unofficial API.
//!
//! Auth: user-supplied cookie string (session-based, no embedded credentials).
//! Implements: list(), get() (with raw_url + UA headers), mkdir(), rename(), remove().
//!
//! **Hard blockers (documented per spec § B):**
//! - Cookie session API is unofficial / may break without notice
//! - Regional errno 9000 ("not yet available in this area")
//! - No OAuth app credentials path confirmed for openapi.terabox.com
//! - Upload deliberately out of scope (product decision)
//!
//! API endpoints (cookie + query params):
//! - `/api/check/login` — verify cookie validity
//! - `/api/list` — paginated list (page size 100)
//! - `/api/home/info` — get sign keys for download signing
//! - `/api/download` — get dlink (official download URL)
//! - `/api/filemanager` — write operations (mkdir/rename/remove via opera param)
//!
//! Signing: RC4-style KSA+PRGA (teraboxSign function) — no MD5/SHA/AES.
//! Direct links: 3-hop flow (home/info → /api/download → dlink redirect → Location).

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use chrono::{TimeZone, Utc};
use reqwest::Client;
use serde_json::Value;

use crate::driver::{WdmmVfs, CloudFileItem, ContentDeliveryKind, DriverError, Result};

/// Configuration for the Terabox driver.
pub struct TeraboxConfig {
    /// User-supplied cookie string (must contain session tokens like ndus, ndut_fmt).
    pub cookie: String,
    /// API base URL. Defaults to `https://www.terabox.com`.
    pub base_url: String,
}

impl Default for TeraboxConfig {
    fn default() -> Self {
        Self {
            cookie: String::new(),
            base_url: "https://www.terabox.com".to_string(),
        }
    }
}

impl TeraboxConfig {
    pub fn new(cookie: impl Into<String>) -> Self {
        Self {
            cookie: cookie.into(),
            ..Default::default()
        }
    }
}

/// Terabox-specific error codes.
const ERRNO_REGION_UNAVAILABLE: i64 = 9000;
const ERRNO_DOMAIN_SWITCH: i64 = -6;

/// Common query parameters for all Terabox API calls.
const APP_ID: &str = "250528";
const WEB: &str = "1";
const CHANNEL: &str = "dubox";
const CLIENT_TYPE: &str = "0";

/// User-Agent for API requests and download links.
const API_UA: &str = "terabox;1.37.0.7;PC;PC-Windows;10.0.22631;WindowsTeraBox";

/// Pagination size for list operations.
const PAGE_SIZE: usize = 100;

/// Single-page size for get() stat operations.
const STAT_PAGE_SIZE: usize = 1000;

/// Terabox cloud driver.
pub struct TeraboxDriver {
    config: TeraboxConfig,
    client: Client,
}

impl TeraboxDriver {
    pub fn new(config: TeraboxConfig) -> Result<Self> {
        if config.cookie.is_empty() {
            return Err(DriverError::Config("terabox: missing cookie".into()));
        }
        let client = Client::builder()
            .build()
            .map_err(|e| DriverError::Config(e.to_string()))?;
        Ok(Self { config, client })
    }

    /// Map Terabox errno values to DriverError.
    fn map_errno(errno: i64) -> DriverError {
        match errno {
            ERRNO_REGION_UNAVAILABLE => {
                DriverError::Other("TeraBox not yet available in this area (errno 9000)".into())
            }
            ERRNO_DOMAIN_SWITCH => DriverError::AuthFailed,
            _ => DriverError::Network(format!("Terabox errno {}", errno)),
        }
    }

    /// Parse a Terabox file list entry into a CloudFileItem.
    /// List items do NOT include raw_url (only get() provides that).
    fn parse_item(item: &Value) -> Option<CloudFileItem> {
        let name = item["server_filename"].as_str()?.to_string();
        let is_dir = item["isdir"].as_i64().unwrap_or(0) == 1;
        let size = item["size"].as_u64().unwrap_or(0);
        let mtime = item["server_mtime"].as_i64();
        let modified = mtime.and_then(|ts| {
            if ts > 0 {
                Utc.timestamp_opt(ts, 0).single()
            } else {
                None
            }
        });
        Some(CloudFileItem {
            name,
            is_dir,
            size,
            modified,
            raw_url: None,
            raw_headers: vec![],
            delivery: ContentDeliveryKind::NetworkDirect,
            local_path: None,
        })
    }

    /// Send a GET request with common query params and headers.
    async fn get_json(&self, path: &str, params: &[(&str, &str)]) -> Result<Value> {
        let url = format!("{}{}", self.config.base_url, path);
        let mut query_params = vec![
            ("app_id", APP_ID),
            ("web", WEB),
            ("channel", CHANNEL),
            ("clienttype", CLIENT_TYPE),
        ];
        query_params.extend_from_slice(params);

        let resp = self
            .client
            .get(&url)
            .query(&query_params)
            .header("Cookie", &self.config.cookie)
            .header("User-Agent", API_UA)
            .header("Accept", "application/json, text/plain, */*")
            .header("Referer", &self.config.base_url)
            .header("X-Requested-With", "XMLHttpRequest")
            .send()
            .await
            .map_err(|e| DriverError::Network(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() && status.as_u16() != 200 {
            return Err(DriverError::Network(format!("HTTP {}", status)));
        }

        resp.json::<Value>()
            .await
            .map_err(|e| DriverError::Network(e.to_string()))
    }

    /// POST with form-urlencoded body.
    async fn post_form(
        &self,
        path: &str,
        query: &[(&str, &str)],
        form: &[(&str, &str)],
    ) -> Result<Value> {
        let url = format!("{}{}", self.config.base_url, path);
        let mut query_params = vec![
            ("app_id", APP_ID),
            ("web", WEB),
            ("channel", CHANNEL),
            ("clienttype", CLIENT_TYPE),
        ];
        query_params.extend_from_slice(query);

        let resp = self
            .client
            .post(&url)
            .query(&query_params)
            .form(form)
            .header("Cookie", &self.config.cookie)
            .header("User-Agent", API_UA)
            .header("Accept", "application/json, text/plain, */*")
            .header("Referer", &self.config.base_url)
            .header("X-Requested-With", "XMLHttpRequest")
            .send()
            .await
            .map_err(|e| DriverError::Network(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(DriverError::Network(format!("HTTP {}", status)));
        }

        resp.json::<Value>()
            .await
            .map_err(|e| DriverError::Network(e.to_string()))
    }

    /// Check login status: GET /api/check/login, errno != 0 is failure.
    async fn check_login(&self) -> Result<()> {
        let json = self.get_json("/api/check/login", &[]).await?;
        let errno = json["errno"].as_i64().unwrap_or(0);
        if errno != 0 {
            return Err(Self::map_errno(errno));
        }
        Ok(())
    }

    /// List directory with pagination (page size 100, flips until empty).
    async fn list_dir(&self, dir: &str) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut page = 1;
        loop {
            let params = [
                ("dir", dir),
                ("page", &page.to_string()),
                ("num", &PAGE_SIZE.to_string()),
            ];
            let json = self.get_json("/api/list", &params).await?;
            let errno = json["errno"].as_i64().unwrap_or(0);
            if errno == ERRNO_REGION_UNAVAILABLE {
                return Err(Self::map_errno(errno));
            }
            if errno != 0 {
                return Err(Self::map_errno(errno));
            }
            let list = json["list"].as_array();
            if list.is_none() || list.unwrap().is_empty() {
                break;
            }
            out.extend_from_slice(list.unwrap());
            page += 1;
        }
        Ok(out)
    }

    /// Get sign keys from /api/home/info and compute terabox signature.
    async fn gen_sign(&self) -> Result<String> {
        let json = self.get_json("/api/home/info", &[]).await?;
        let data = json["data"]
            .as_object()
            .ok_or_else(|| DriverError::Other("terabox: home/info data missing".into()))?;
        let sign1 = data["sign1"]
            .as_str()
            .ok_or_else(|| DriverError::Other("terabox: sign1 missing".into()))?;
        let sign3 = data["sign3"]
            .as_str()
            .ok_or_else(|| DriverError::Other("terabox: sign3 missing".into()))?;
        if sign1.is_empty() || sign3.is_empty() {
            return Err(DriverError::Other(
                "terabox: sign keys empty from home/info".into(),
            ));
        }
        terabox_sign(sign3, sign1)
    }

    /// Get official download link for a file (3-hop: home/info → download → dlink redirect).
    async fn link_official(&self, fs_id: u64) -> Result<(String, Vec<(String, String)>)> {
        let sign = self.gen_sign().await?;
        let timestamp = chrono::Utc::now().timestamp();
        let params = [
            ("type", "dlink"),
            ("fidlist", &format!("[{}]", fs_id)),
            ("sign", &sign),
            ("vip", "2"),
            ("timestamp", &timestamp.to_string()),
        ];
        let json = self.get_json("/api/download", &params).await?;

        // Try both response shapes: dlink[0].dlink and info[0].dlink
        let dlink = json["dlink"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|obj| obj["dlink"].as_str())
            .or_else(|| {
                json["info"]
                    .as_array()
                    .and_then(|arr| arr.first())
                    .and_then(|obj| obj["dlink"].as_str())
            })
            .ok_or_else(|| {
                let errno = json["errno"].as_i64().unwrap_or(0);
                DriverError::Other(format!(
                    "terabox: fid {} no dlink found (errno: {})",
                    fs_id, errno
                ))
            })?;

        // Follow redirect once to get real URL (or fallback to dlink itself)
        let resp = self
            .client
            .get(dlink)
            .header("Cookie", &self.config.cookie)
            .header("User-Agent", API_UA)
            .send()
            .await
            .map_err(|e| DriverError::Network(e.to_string()))?;

        let final_url = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(dlink)
            .to_string();

        Ok((
            final_url,
            vec![("User-Agent".to_string(), API_UA.to_string())],
        ))
    }

    /// Manage operations (rename/remove/move/copy) via /api/filemanager.
    async fn manage(&self, opera: &str, filelist: Value) -> Result<()> {
        let query = [("onnest", "fail"), ("opera", opera)];
        let filelist_str =
            serde_json::to_string(&filelist).map_err(|e| DriverError::Other(e.to_string()))?;
        let form = [
            ("async", "0"),
            ("filelist", &filelist_str),
            ("ondup", "newcopy"),
        ];
        let json = self.post_form("/api/filemanager", &query, &form).await?;
        let errno = json["errno"].as_i64().unwrap_or(0);
        if errno != 0 {
            return Err(Self::map_errno(errno));
        }
        Ok(())
    }

    /// Extract parent directory from path.
    fn parent_dir(path: &str) -> &str {
        let trimmed = path.trim_end_matches('/');
        match trimmed.rfind('/') {
            Some(0) => "/",
            Some(idx) => &trimmed[..idx],
            None => "/",
        }
    }

    /// Extract file name from path.
    fn file_name(path: &str) -> &str {
        let trimmed = path.trim_end_matches('/');
        match trimmed.rfind('/') {
            Some(idx) => &trimmed[idx + 1..],
            None => trimmed,
        }
    }

    /// Normalize path: collapse slashes, remove trailing slash, ensure leading slash.
    fn normalize_path(path: &str) -> String {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", parts.join("/"))
        }
    }
}

#[async_trait]
impl WdmmVfs for TeraboxDriver {
    async fn list(&self, path: &str) -> Result<Vec<CloudFileItem>> {
        let normalized = Self::normalize_path(path);
        let files = self.list_dir(&normalized).await?;
        Ok(files.iter().filter_map(Self::parse_item).collect())
    }

    async fn get(&self, path: &str) -> Result<CloudFileItem> {
        let normalized = Self::normalize_path(path);
        if normalized == "/" {
            return Ok(CloudFileItem {
                name: "/".to_string(),
                is_dir: true,
                size: 0,
                modified: None,
                raw_url: None,
                raw_headers: vec![],
                delivery: ContentDeliveryKind::NetworkDirect,
                local_path: None,
            });
        }

        let parent = Self::parent_dir(&normalized);
        let file_name = Self::file_name(&normalized);

        let params = [
            ("dir", parent),
            ("page", "1"),
            ("num", &STAT_PAGE_SIZE.to_string()),
        ];
        let json = self.get_json("/api/list", &params).await?;
        let errno = json["errno"].as_i64().unwrap_or(0);
        if errno == ERRNO_REGION_UNAVAILABLE {
            return Err(Self::map_errno(errno));
        }
        if errno != 0 {
            return Err(Self::map_errno(errno));
        }

        let list = json["list"]
            .as_array()
            .ok_or_else(|| DriverError::NotFound(format!("file not found: {}", file_name)))?;

        let found = list
            .iter()
            .find(|item| item["server_filename"].as_str() == Some(file_name))
            .ok_or_else(|| DriverError::NotFound(format!("file not found: {}", file_name)))?;

        let is_dir = found["isdir"].as_i64().unwrap_or(0) == 1;
        if is_dir {
            return Self::parse_item(found)
                .ok_or_else(|| DriverError::Other("failed to parse directory item".into()));
        }

        // File: must get direct link
        let fs_id = found["fs_id"]
            .as_u64()
            .ok_or_else(|| DriverError::Other("terabox: file missing fs_id".into()))?;
        let (url, headers) = self.link_official(fs_id).await?;

        let size = found["size"].as_u64().unwrap_or(0);
        let mtime = found["server_mtime"].as_i64();
        let modified = mtime.and_then(|ts| {
            if ts > 0 {
                Utc.timestamp_opt(ts, 0).single()
            } else {
                None
            }
        });

        Ok(CloudFileItem {
            name: file_name.to_string(),
            is_dir: false,
            size,
            modified,
            raw_url: Some(url),
            raw_headers: headers,
            delivery: ContentDeliveryKind::NetworkDirect,
            local_path: None,
        })
    }

    async fn mkdir(&self, path: &str) -> Result<()> {
        let normalized = Self::normalize_path(path);
        let query = [("a", "commit")];
        let form = [
            ("path", normalized.as_str()),
            ("isdir", "1"),
            ("block_list", "[]"),
        ];
        let json = self.post_form("/api/create", &query, &form).await?;
        let errno = json["errno"].as_i64().unwrap_or(0);
        if errno != 0 {
            return Err(Self::map_errno(errno));
        }
        Ok(())
    }

    async fn rename(&self, _old_path: &str, new_path: &str) -> Result<()> {
        let normalized_new = Self::normalize_path(new_path);
        let new_name = Self::file_name(&normalized_new);
        let filelist = serde_json::json!([{
            "path": normalized_new,
            "newname": new_name,
        }]);
        self.manage("rename", filelist).await
    }

    async fn remove(&self, path: &str) -> Result<()> {
        let normalized = Self::normalize_path(path);
        let filelist = serde_json::json!([normalized]);
        self.manage("delete", filelist).await
    }
}

/// Terabox RC4-style signing function (KSA + PRGA + base64).
/// s1 = sign3 (key), s2 = sign1 (plaintext).
/// No MD5/SHA/AES — pure XOR cipher.
fn terabox_sign(s1: &str, s2: &str) -> Result<String> {
    if s1.is_empty() {
        return Err(DriverError::Other(
            "terabox: sign3 key empty (cannot generate signature)".into(),
        ));
    }

    let mut a = [0u8; 256];
    let mut p: Vec<u8> = (0..=255).collect();
    let v = s1.len();

    // KSA
    for q in 0..256 {
        a[q] = s1.as_bytes()[q % v];
    }

    let mut u = 0usize;
    for q in 0..256 {
        u = (u + p[q] as usize + a[q] as usize) % 256;
        p.swap(q, u);
    }

    // PRGA
    let mut i = 0usize;
    u = 0;
    let mut output = Vec::new();
    for byte in s2.bytes() {
        i = (i + 1) % 256;
        u = (u + p[i] as usize) % 256;
        p.swap(i, u);
        let k = p[(p[i] as usize + p[u] as usize) % 256];
        output.push((byte ^ k) & 0xFF);
    }

    Ok(BASE64.encode(&output))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_path() {
        assert_eq!(TeraboxDriver::normalize_path("/"), "/");
        assert_eq!(
            TeraboxDriver::normalize_path("//A//B//c.mkv/"),
            "/A/B/c.mkv"
        );
        assert_eq!(TeraboxDriver::normalize_path("foo/bar"), "/foo/bar");
    }

    #[test]
    fn test_parent_and_file_name() {
        assert_eq!(TeraboxDriver::parent_dir("/Movies/file.mkv"), "/Movies");
        assert_eq!(TeraboxDriver::file_name("/Movies/file.mkv"), "file.mkv");
        assert_eq!(TeraboxDriver::parent_dir("/"), "/");
        assert_eq!(TeraboxDriver::file_name("/root"), "root");
    }

    #[test]
    fn test_terabox_sign_golden_vectors() {
        // Golden vectors from Dart test
        assert_eq!(
            terabox_sign("sign3key", "sign1data").unwrap(),
            "RF9iI+bxb964"
        );
        assert_eq!(terabox_sign("b", "a").unwrap(), "PA==");
        assert_eq!(terabox_sign("b", "X").unwrap(), "BQ==");
        assert_eq!(terabox_sign("b", "\0").unwrap(), "XQ==");
        assert_eq!(terabox_sign("key", "").unwrap(), "");
        assert_eq!(
            terabox_sign("abc", "hello world").unwrap(),
            "pfi1RTf/mhxeiws="
        );
        assert_eq!(
            terabox_sign("0123456789abcdef", "The quick brown fox").unwrap(),
            "0AAleYw7yfti1jjsr+Q9ZYSCUA=="
        );
    }

    #[test]
    fn test_terabox_sign_empty_key_errors() {
        assert!(terabox_sign("", "data").is_err());
    }
}
