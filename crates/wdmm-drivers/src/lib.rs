//! `wdmm-drivers` — CloudDriver trait + WebDAV working implementation.
//!
//! WebDAV `list()` is fully implemented (referencing WDMM webdav_source.dart).
//! Other drivers (baidu, aliyundrive, ctfile, etc.) are stubbed with TODO.

#![forbid(unsafe_code)]
#![allow(dead_code)]

pub mod driver;
pub mod webdav;
pub mod stubs;

pub use driver::{CloudDriver, CloudFileItem, DriverError, Result};
pub use webdav::WebDavDriver;

/// Driver registry — stub for now; per brief drivers beyond WebDAV are TODO.
pub struct DriverRegistry;

impl DriverRegistry {
    pub fn create_driver(provider_type: &str, config: serde_json::Value) -> Result<Box<dyn CloudDriver>> {
        match provider_type {
            "webdav" => {
                let url = config["url"].as_str().ok_or_else(|| DriverError::Config("missing url".into()))?;
                let username = config["username"].as_str().unwrap_or("");
                let password = config["password"].as_str().unwrap_or("");
                Ok(Box::new(WebDavDriver::new(url, username, password)?))
            }
            _ => Err(DriverError::UnsupportedProvider(provider_type.to_string())),
        }
    }
}
