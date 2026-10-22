//! Stub drivers for cloud providers not yet implemented.
//!
//! Per brief/constraints §9: implement against official APIs, with
//! user-supplied OAuth/credentials only — no keys are embedded in source.
//!
//! Implemented (production):
//! - `webdav`        (wdmm_drivers::WebDavDriver)
//! - `ctfile`        (wdmm_drivers::CtFileDriver)
//! - `baidu`         (wdmm_drivers::BaiduDriver) — list/get; write ops TODO
//! - `123`           (wdmm_drivers::Pan123Driver) — list/get; write ops TODO
//! - `aliyundrive`   (wdmm_drivers::AliyunDriver) — ✓ list/get/mkdir/rename/remove (Round 8)
//! - `open115`       (wdmm_drivers::Open115Driver) — list/get; write ops TODO
//!
//! Remaining stubs:
//! - terabox       — Official Terabox OpenAPI exists but **detailed endpoint
//!                   docs for list + download URL have not been confirmed**.
//!                   TODO: once https://developers.terabox.com API reference
//!                   for `openapi/xpan/file?method=list` and the presigned
//!                   download URL endpoint (`/xpan/multimedia`) are confirmed
//!                   with OAuth2 app credentials, implement list/get + write ops
//!                   following the same pattern as AliyunDriver.
//!                   Blocked on: confirming pagination API shape and whether
//!                   `access_token` alone is sufficient (no app-secret in binary).
//! - netease_music — NetEase Cloud Music; excluded from public release (§9)

use async_trait::async_trait;
use crate::driver::{CloudDriver, CloudFileItem, DriverError, Result};

/// Shared helper: build the standard "not yet implemented" driver error.
fn todo_err(name: &str) -> DriverError {
    DriverError::Other(format!("TODO: {} driver not yet implemented", name))
}

macro_rules! impl_stub_driver {
    ($struct_name:ident, $label:literal) => {
        pub struct $struct_name;

        #[async_trait]
        impl CloudDriver for $struct_name {
            async fn list(&self, _path: &str) -> Result<Vec<CloudFileItem>> {
                Err(todo_err($label))
            }
            async fn get(&self, _path: &str) -> Result<CloudFileItem> {
                Err(todo_err($label))
            }
            async fn mkdir(&self, _path: &str) -> Result<()> {
                Err(todo_err($label))
            }
            async fn rename(&self, _old: &str, _new: &str) -> Result<()> {
                Err(todo_err($label))
            }
            async fn remove(&self, _path: &str) -> Result<()> {
                Err(todo_err($label))
            }
        }
    };
}

impl_stub_driver!(TeraboxDriver, "terabox");
impl_stub_driver!(NeteaseMusicDriver, "netease_music");

