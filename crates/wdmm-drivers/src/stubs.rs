//! Stub drivers for cloud providers beyond WebDAV and CtFile.
//!
//! Per brief/constraints §9: implement against official APIs, with
//! user-supplied OAuth/credentials only — no keys are embedded in source.
//! These are TODO stubs for Round 3+. Each returns a clear `TODO` error so
//! callers fail loudly rather than silently no-op'ing.
//!
//! Implemented this round:
//! - `webdav`  (wdmm_drivers::WebDavDriver)
//! - `ctfile`  (wdmm_drivers::CtFileDriver)
//! - `baidu`   (wdmm_drivers::BaiduDriver) — list/get production; write ops TODO
//! - `123`     (wdmm_drivers::Pan123Driver) — list/get production; write ops TODO
//!
//! Stubbed (Round 3+):
//! - aliyundrive     — Aliyun Drive OpenAPI (OAuth, user-supplied app key)
//! - open115         — 115 OpenAPI
//! - terabox         — Terabox
//! - netease_music   — NetEase Cloud Music (excluded from public release per §9)

use async_trait::async_trait;
use crate::driver::{CloudDriver, CloudFileItem, DriverError, Result};

/// Shared helper: build the standard "not yet implemented" driver error.
fn todo(name: &str) -> DriverError {
    DriverError::Other(format!("TODO: {} driver not yet implemented (Round 3+)", name))
}

macro_rules! impl_stub_driver {
    ($struct_name:ident, $label:literal) => {
        pub struct $struct_name;

        #[async_trait]
        impl CloudDriver for $struct_name {
            async fn list(&self, _path: &str) -> Result<Vec<CloudFileItem>> {
                Err(todo($label))
            }
            async fn get(&self, _path: &str) -> Result<CloudFileItem> {
                Err(todo($label))
            }
            async fn mkdir(&self, _path: &str) -> Result<()> {
                Err(todo($label))
            }
            async fn rename(&self, _old: &str, _new: &str) -> Result<()> {
                Err(todo($label))
            }
            async fn remove(&self, _path: &str) -> Result<()> {
                Err(todo($label))
            }
        }
    };
}

impl_stub_driver!(AliyunDriveDriver, "aliyundrive");
impl_stub_driver!(Open115Driver, "open115");
impl_stub_driver!(TeraboxDriver, "terabox");
impl_stub_driver!(NeteaseMusicDriver, "netease_music");
impl_stub_driver!(BaiduDriver, "baidu_netdisk");
impl_stub_driver!(Pan123OpenDriver, "123_open");
