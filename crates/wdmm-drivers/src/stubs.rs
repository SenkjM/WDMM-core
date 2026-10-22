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
//! - `aliyundrive`   (wdmm_drivers::AliyunDriver) — list/get; write ops TODO
//! - `open115`       (wdmm_drivers::Open115Driver) — list/get; write ops TODO
//!
//! Remaining stubs (sharper TODO per Round 7 brief):
//! - terabox       — Terabox official API; add when API docs confirmed
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
