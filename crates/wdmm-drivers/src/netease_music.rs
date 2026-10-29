//! NeteaseMusic driver — 网易云音乐云盘 API.
//!
//! Auth: user-supplied cookie (must contain `__csrf` and `MUSIC_U` tokens).
//! Implements: list(), get() (with weapi list + linuxapi song URL), remove().
//!
//! Behavior:
//! - Single-layer flat listing (no directory tree; songs only)
//! - list() always returns full song list up to `song_limit` (default 200)
//! - get() / remove() locate songs by filename within list
//! - mkdir/rename not supported (cloud storage is flat)
//!
//! API encryption (see wdmm_crypto::netease):
//! - weapi: 2-layer AES-CBC + raw RSA → params + encSecKey form fields
//! - linuxapi: AES-ECB → uppercase hex eparams field
//!
//! Endpoints:
//! - `https://music.163.com/weapi/v1/cloud/get` — list songs
//! - `http://music.163.com/weapi/cloud/del` — delete songs (http, not https)
//! - `https://music.163.com/api/linux/forward` — linuxapi forward (song URLs)

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use reqwest::Client;
use serde_json::{json, Value};

use crate::driver::{WdmmVfs, CloudFileItem, ContentDeliveryKind, DriverError, Result};
use wdmm_crypto::netease::{linuxapi_encrypt, weapi_encrypt};

/// Configuration for the NeteaseMusic driver.
pub struct NeteaseMusicConfig {
    /// User-supplied cookie string (must contain __csrf and MUSIC_U tokens).
    pub cookie: String,
    /// Maximum songs to fetch in list() (default 200).
    pub song_limit: u32,
}

impl Default for NeteaseMusicConfig {
    fn default() -> Self {
        Self {
            cookie: String::new(),
            song_limit: 200,
        }
    }
}

impl NeteaseMusicConfig {
    pub fn new(cookie: impl Into<String>) -> Self {
        Self {
            cookie: cookie.into(),
            ..Default::default()
        }
    }
}

/// User-Agent for normal API requests.
const API_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Safari/537.36 Chrome/142.0.0.0";

/// User-Agent for linuxapi requests (required by /api/linux/forward).
const LINUX_API_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/60.0.3112.90 Safari/537.36";

const REFERER: &str = "https://music.163.com";

/// Cloud storage list endpoint.
const CLOUD_GET_URL: &str = "https://music.163.com/weapi/v1/cloud/get";

/// Cloud storage delete endpoint (http, not https — matches Dart upstream).
const CLOUD_DEL_URL: &str = "http://music.163.com/weapi/cloud/del";

/// Linuxapi forward endpoint (for song play URLs).
const LINUX_API_FORWARD: &str = "https://music.163.com/api/linux/forward";

/// Song URL API (passed to linuxapi forward).
const SONG_URL_API: &str = "https://music.163.com/api/song/enhance/player/url";

/// NeteaseMusic cloud drive driver.
pub struct NeteaseMusicDriver {
    config: NeteaseMusicConfig,
    client: Client,
    csrf_token: String,
    music_u: String,
}

impl NeteaseMusicDriver {
    pub fn new(config: NeteaseMusicConfig) -> Result<Self> {
        if config.cookie.is_empty() {
            return Err(DriverError::Config("netease_music: missing cookie".into()));
        }

        // Parse __csrf and MUSIC_U from cookie
        let csrf_token = extract_cookie_value(&config.cookie, "__csrf")
            .ok_or_else(|| DriverError::Config("netease_music: missing __csrf in cookie".into()))?;
        let music_u = extract_cookie_value(&config.cookie, "MUSIC_U")
            .ok_or_else(|| DriverError::Config("netease_music: missing MUSIC_U in cookie".into()))?;

        let client = Client::builder()
            .build()
            .map_err(|e| DriverError::Config(e.to_string()))?;

        Ok(Self {
            config,
            client,
            csrf_token,
            music_u,
        })
    }

    /// Send weapi encrypted POST request.
    async fn weapi_request(&self, url: &str, data: Value) -> Result<Value> {
        let encrypted = weapi_encrypt(&data, None);
        let form = [
            ("params", encrypted.params.as_str()),
            ("encSecKey", encrypted.enc_sec_key.as_str()),
        ];

        let cookie_header = format!("{}; os=pc", self.config.cookie);

        let resp = self
            .client
            .post(url)
            .header("Cookie", &cookie_header)
            .header("User-Agent", API_UA)
            .header("Referer", REFERER)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .form(&form)
            .send()
            .await
            .map_err(|e| DriverError::Network(format!("weapi request failed: {}", e)))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| DriverError::Network(format!("failed to read response: {}", e)))?;

        let body: Value = serde_json::from_str(&text)
            .map_err(|_| DriverError::Other(format!("invalid JSON response: {}", text.chars().take(200).collect::<String>())))?;

        // Check response code field
        if let Some(code) = body["code"].as_i64() {
            if code != 200 {
                let msg = body["message"].as_str().unwrap_or("");
                return Err(DriverError::Other(format!("netease API error: code {} {}", code, msg)));
            }
        }

        if !status.is_success() {
            return Err(DriverError::Network(format!("HTTP {}", status.as_u16())));
        }

        Ok(body)
    }

    /// Send linuxapi encrypted POST request.
    async fn linuxapi_request(&self, params_json: Value) -> Result<Value> {
        let encrypted = linuxapi_encrypt(&params_json);
        let form = [("eparams", encrypted.as_str())];

        let cookie_header = format!("{}; os=pc", self.config.cookie);

        let resp = self
            .client
            .post(LINUX_API_FORWARD)
            .header("Cookie", &cookie_header)
            .header("User-Agent", LINUX_API_UA)
            .header("Referer", REFERER)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .form(&form)
            .send()
            .await
            .map_err(|e| DriverError::Network(format!("linuxapi request failed: {}", e)))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| DriverError::Network(format!("failed to read response: {}", e)))?;

        let body: Value = serde_json::from_str(&text)
            .map_err(|_| DriverError::Other(format!("invalid JSON response: {}", text.chars().take(200).collect::<String>())))?;

        // Check response code
        if let Some(code) = body["code"].as_i64() {
            if code != 200 {
                let msg = body["message"].as_str().unwrap_or("");
                return Err(DriverError::Other(format!("netease API error: code {} {}", code, msg)));
            }
        }

        if !status.is_success() {
            return Err(DriverError::Network(format!("HTTP {}", status.as_u16())));
        }

        Ok(body)
    }

    /// Get song list from cloud storage.
    async fn get_songs(&self) -> Result<Vec<NeteaseSong>> {
        let body = self
            .weapi_request(
                CLOUD_GET_URL,
                json!({
                    "limit": self.config.song_limit.to_string(),
                    "offset": "0"
                }),
            )
            .await?;

        let data = body["data"]
            .as_array()
            .ok_or_else(|| DriverError::Other("netease: missing data array".into()))?;

        let songs: Vec<NeteaseSong> = data
            .iter()
            .filter_map(|item| NeteaseSong::from_value(item))
            .collect();

        Ok(songs)
    }

    /// Get song play URL via linuxapi.
    async fn get_song_url(&self, song_id: u64) -> Result<String> {
        let payload = json!({
            "url": SONG_URL_API,
            "method": "POST",
            "params": {
                "ids": format!("[{}]", song_id),
                "br": "999000"
            }
        });

        let body = self.linuxapi_request(payload).await?;

        let data = body["data"]
            .as_array()
            .ok_or_else(|| DriverError::Other("netease: missing data array in song URL response".into()))?;

        if data.is_empty() {
            return Err(DriverError::Other("netease: no song URL available (may require VIP or unavailable)".into()));
        }

        let url = data[0]["url"]
            .as_str()
            .ok_or_else(|| DriverError::Other("netease: empty song URL (may require VIP or unavailable)".into()))?;

        if url.is_empty() {
            return Err(DriverError::Other("netease: empty song URL (may require VIP or unavailable)".into()));
        }

        Ok(url.to_string())
    }

    /// Find song by filename.
    async fn find_song_by_name(&self, name: &str) -> Result<NeteaseSong> {
        let songs = self.get_songs().await?;
        songs
            .into_iter()
            .find(|s| s.file_name == name)
            .ok_or_else(|| DriverError::NotFound(format!("song not found: {}", name)))
    }
}

#[async_trait]
impl WdmmVfs for NeteaseMusicDriver {
    async fn list(&self, _path: &str) -> Result<Vec<CloudFileItem>> {
        let songs = self.get_songs().await?;
        Ok(songs.into_iter().map(|s| s.into()).collect())
    }

    async fn get(&self, path: &str) -> Result<CloudFileItem> {
        let name = path.trim_start_matches('/');
        if name.is_empty() || path == "/" {
            // Root directory item
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

        let song = self.find_song_by_name(name).await?;
        let url = self.get_song_url(song.song_id).await?;

        let modified = song.modified();
        Ok(CloudFileItem {
            name: song.file_name,
            is_dir: false,
            size: song.file_size,
            modified,
            raw_url: Some(url),
            raw_headers: vec![("User-Agent".to_string(), API_UA.to_string())],
            delivery: ContentDeliveryKind::NetworkDirect,
            local_path: None,
        })
    }

    async fn mkdir(&self, _path: &str) -> Result<()> {
        Err(DriverError::Other("netease_music: mkdir not supported (flat storage)".into()))
    }

    async fn rename(&self, _old: &str, _new: &str) -> Result<()> {
        Err(DriverError::Other("netease_music: rename not supported".into()))
    }

    async fn remove(&self, path: &str) -> Result<()> {
        let name = path.trim_start_matches('/');
        if name.is_empty() {
            return Err(DriverError::Other("netease_music: cannot delete root".into()));
        }

        let song = self.find_song_by_name(name).await?;

        self.weapi_request(
            CLOUD_DEL_URL,
            json!({
                "songIds": format!("[{}]", song.song_id)
            }),
        )
        .await?;

        Ok(())
    }
}

/// Helper: extract cookie value by name from cookie string.
fn extract_cookie_value(cookie: &str, name: &str) -> Option<String> {
    let pattern = format!("{}=", name);
    for part in cookie.split(';') {
        let trimmed = part.trim();
        if let Some(value) = trimmed.strip_prefix(&pattern) {
            return Some(value.split(';').next().unwrap_or("").to_string());
        }
    }
    None
}

/// NeteaseMusic song item.
#[derive(Debug, Clone)]
struct NeteaseSong {
    song_id: u64,
    file_name: String,
    file_size: u64,
    add_time_ms: i64,
}

impl NeteaseSong {
    fn from_value(val: &Value) -> Option<Self> {
        Some(Self {
            song_id: val["songId"].as_u64()?,
            file_name: val["fileName"].as_str()?.to_string(),
            file_size: val["fileSize"].as_u64().unwrap_or(0),
            add_time_ms: val["addTime"].as_i64().unwrap_or(0),
        })
    }

    fn modified(&self) -> Option<DateTime<Utc>> {
        if self.add_time_ms > 0 {
            Utc.timestamp_millis_opt(self.add_time_ms).single()
        } else {
            None
        }
    }
}

impl From<NeteaseSong> for CloudFileItem {
    fn from(song: NeteaseSong) -> Self {
        let modified = song.modified();
        Self {
            name: song.file_name,
            is_dir: false,
            size: song.file_size,
            modified,
            raw_url: None, // Only get() provides raw_url
            raw_headers: vec![],
            delivery: ContentDeliveryKind::NetworkDirect,
            local_path: None,
        }
    }
}
