//! `wdmm-drivers` — CloudDriver trait + implementations.
//!
//! Implemented drivers (production paths):
//! - `WebDavDriver`    — WebDAV list/get/mkdir/rename/remove/put_content/get_content
//! - `CtFileDriver`    — CtFile (城通网盘) list/get + write ops
//! - `BaiduDriver`     — Baidu NetDisk list/get/mkdir/rename/remove/content reads
//! - `Pan123Driver`    — 123 Pan list/get (write ops TODO)
//! - `AliyunDriver`    — Aliyun Drive list/get/mkdir/rename/remove/content reads
//! - `Open115Driver`   — 115 Network list/get (write ops TODO)
//!
//! Stubbed drivers (Round 3+): each returns a loud TODO error.
//! - terabox, netease_music.

#![forbid(unsafe_code)]
#![allow(dead_code)]

pub mod aliyun;
pub mod baidu;
pub mod ctfile;
pub mod driver;
pub mod open115;
pub mod pan123;
pub mod stubs;
pub mod webdav;

pub use aliyun::{AliyunConfig, AliyunDriver};
pub use baidu::{BaiduConfig, BaiduDriver};
pub use ctfile::{CtFileConfig, CtFileDriver};
pub use driver::{CloudDriver, CloudFileItem, ContentDeliveryKind, DriverError, Result};
pub use open115::{Open115Config, Open115Driver};
pub use pan123::{Pan123Config, Pan123Driver};
pub use stubs::{NeteaseMusicDriver, TeraboxDriver};
pub use webdav::WebDavDriver;

/// Driver registry — instantiates a `CloudDriver` from a provider type + config JSON.
///
/// Config is user-supplied at runtime (e.g. from the accounts store). No driver
/// embeds credentials in source. Missing required fields return `Config` errors.
pub struct DriverRegistry;

impl DriverRegistry {
    pub fn create_driver(
        provider_type: &str,
        config: serde_json::Value,
    ) -> Result<Box<dyn CloudDriver>> {
        match provider_type {
            "webdav" => {
                let url = config["url"]
                    .as_str()
                    .ok_or_else(|| DriverError::Config("webdav: missing url".into()))?;
                let username = config["username"].as_str().unwrap_or("");
                let password = config["password"].as_str().unwrap_or("");
                Ok(Box::new(WebDavDriver::new(url, username, password)?))
            }
            "ctfile" => {
                let session = config["session"]
                    .as_str()
                    .ok_or_else(|| DriverError::Config("ctfile: missing session token".into()))?;
                let mut cfg = CtFileConfig::with_session(session);
                if let Some(s) = config["cloud_type"].as_str() {
                    cfg.cloud_type = s.to_string();
                }
                if let Some(s) = config["base_url"].as_str() {
                    cfg.base_url = s.to_string();
                }
                if let Some(s) = config["order_by"].as_str() {
                    cfg.order_by = s.to_string();
                }
                if let Some(s) = config["order_direction"].as_str() {
                    cfg.order_direction = s.to_string();
                }
                if let Some(s) = config["user_agent"].as_str() {
                    cfg.user_agent = s.to_string();
                }
                if let Some(n) = config["list_cache_sec"].as_u64() {
                    cfg.list_cache_sec = n;
                }
                if let Some(n) = config["request_interval_ms"].as_u64() {
                    cfg.request_interval_ms = n;
                }
                Ok(Box::new(CtFileDriver::new(cfg)?))
            }
            "baidu" | "baidu_netdisk" => {
                let token = config["access_token"]
                    .as_str()
                    .ok_or_else(|| DriverError::Config("baidu: missing access_token".into()))?;
                let mut cfg = BaiduConfig::new(token);
                if let Some(s) = config["base_url"].as_str() {
                    cfg.base_url = s.to_string();
                }
                Ok(Box::new(BaiduDriver::new(cfg)?))
            }
            "123" | "123_open" | "pan123" => {
                let token = config["access_token"]
                    .as_str()
                    .ok_or_else(|| DriverError::Config("123pan: missing access_token".into()))?;
                let mut cfg = Pan123Config::new(token);
                if let Some(s) = config["base_url"].as_str() {
                    cfg.base_url = s.to_string();
                }
                Ok(Box::new(Pan123Driver::new(cfg)?))
            }
            "aliyundrive" | "aliyun" => {
                let token = config["access_token"]
                    .as_str()
                    .ok_or_else(|| DriverError::Config("aliyun: missing access_token".into()))?;
                let drive_id = config["drive_id"]
                    .as_str()
                    .ok_or_else(|| DriverError::Config("aliyun: missing drive_id".into()))?;
                let mut cfg = AliyunConfig::new(token, drive_id);
                if let Some(s) = config["base_url"].as_str() {
                    cfg.base_url = s.to_string();
                }
                if let Some(s) = config["remove_way"].as_str() {
                    cfg.remove_way = s.to_string();
                }
                Ok(Box::new(AliyunDriver::new(cfg)?))
            }
            "115" | "open115" => {
                let token = config["access_token"]
                    .as_str()
                    .ok_or_else(|| DriverError::Config("115: missing access_token".into()))?;
                let mut cfg = Open115Config::new(token);
                if let Some(s) = config["base_url"].as_str() {
                    cfg.base_url = s.to_string();
                }
                Ok(Box::new(Open115Driver::new(cfg)?))
            }
            "terabox" => Ok(Box::new(TeraboxDriver)),
            "netease_music" => Ok(Box::new(NeteaseMusicDriver)),
            _ => Err(DriverError::UnsupportedProvider(provider_type.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_webdav_requires_url() {
        match DriverRegistry::create_driver("webdav", serde_json::json!({})) {
            Err(DriverError::Config(_)) => {}
            other => panic!("expected Config error, got {:?}", other.map(|_| "ok")),
        }
    }

    #[test]
    fn registry_ctfile_requires_session() {
        match DriverRegistry::create_driver("ctfile", serde_json::json!({})) {
            Err(DriverError::Config(_)) => {}
            other => panic!("expected Config error, got {:?}", other.map(|_| "ok")),
        }
    }

    #[test]
    fn registry_unsupported() {
        match DriverRegistry::create_driver("dropbox", serde_json::json!({})) {
            Err(DriverError::UnsupportedProvider(s)) => assert_eq!(s, "dropbox"),
            other => panic!(
                "expected UnsupportedProvider, got {:?}",
                other.map(|_| "ok")
            ),
        }
    }

    #[test]
    fn registry_aliyun_requires_token_and_drive_id() {
        // Missing access_token
        match DriverRegistry::create_driver("aliyundrive", serde_json::json!({})) {
            Err(DriverError::Config(_)) => {}
            other => panic!("expected Config error, got {:?}", other.map(|_| "ok")),
        }
        // Missing drive_id
        match DriverRegistry::create_driver("aliyun", serde_json::json!({"access_token":"tok"})) {
            Err(DriverError::Config(_)) => {}
            other => panic!("expected Config error, got {:?}", other.map(|_| "ok")),
        }
        // Both present
        assert!(DriverRegistry::create_driver(
            "aliyundrive",
            serde_json::json!({"access_token": "tok", "drive_id": "drv"})
        )
        .is_ok());
    }

    #[test]
    fn registry_115_requires_token() {
        match DriverRegistry::create_driver("115", serde_json::json!({})) {
            Err(DriverError::Config(_)) => {}
            other => panic!("expected Config error, got {:?}", other.map(|_| "ok")),
        }
        assert!(DriverRegistry::create_driver(
            "open115",
            serde_json::json!({"access_token": "tok"})
        )
        .is_ok());
    }

    #[test]
    fn registry_production_drivers_instantiate() {
        assert!(
            DriverRegistry::create_driver("baidu", serde_json::json!({"access_token": "tok"}))
                .is_ok()
        );
        assert!(DriverRegistry::create_driver(
            "123_open",
            serde_json::json!({"access_token": "tok"})
        )
        .is_ok());
        // Remaining stubs need no config
        assert!(DriverRegistry::create_driver("terabox", serde_json::json!({})).is_ok());
        assert!(DriverRegistry::create_driver("netease_music", serde_json::json!({})).is_ok());
    }
}
