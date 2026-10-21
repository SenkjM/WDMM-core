//! `wdmm-drivers` — CloudDriver trait + implementations.
//!
//! Implemented drivers:
//! - `WebDavDriver` — WebDAV `list()` fully working (refs WDMM webdav_source.dart)
//! - `CtFileDriver` — CtFile (城通网盘) `list()` + `get()` download_url ported from
//!   Dart ctfile_driver.dart using official API patterns.
//!
//! Stubbed drivers (TODO, Round 3+): baidu, aliyundrive, 115, terabox,
//! netease_music, 123_open.

#![forbid(unsafe_code)]
#![allow(dead_code)]

pub mod ctfile;
pub mod driver;
pub mod stubs;
pub mod webdav;

pub use ctfile::{CtFileConfig, CtFileDriver};
pub use driver::{CloudDriver, CloudFileItem, DriverError, Result};
pub use stubs::{
    AliyunDriveDriver, BaiduDriver, NeteaseMusicDriver, Open115Driver, Pan123OpenDriver,
    TeraboxDriver,
};
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
                // Session token (开放接口登录密钥) is the only required secret.
                // It must come from the user's config — never embedded here.
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
            "baidu" | "baidu_netdisk" => Ok(Box::new(BaiduDriver)),
            "aliyundrive" | "aliyun" => Ok(Box::new(AliyunDriveDriver)),
            "115" | "open115" => Ok(Box::new(Open115Driver)),
            "terabox" => Ok(Box::new(TeraboxDriver)),
            "netease_music" => Ok(Box::new(NeteaseMusicDriver)),
            "123" | "123_open" | "pan123" => Ok(Box::new(Pan123OpenDriver)),
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
            other => panic!("expected UnsupportedProvider, got {:?}", other.map(|_| "ok")),
        }
    }

    #[test]
    fn registry_stubs_instantiate() {
        // Stubs build OK; their methods error at call-time, not build-time.
        assert!(DriverRegistry::create_driver("baidu", serde_json::json!({})).is_ok());
        assert!(DriverRegistry::create_driver("aliyundrive", serde_json::json!({})).is_ok());
        assert!(DriverRegistry::create_driver("115", serde_json::json!({})).is_ok());
        assert!(DriverRegistry::create_driver("terabox", serde_json::json!({})).is_ok());
        assert!(DriverRegistry::create_driver("netease_music", serde_json::json!({})).is_ok());
        assert!(DriverRegistry::create_driver("123_open", serde_json::json!({})).is_ok());
    }
}
