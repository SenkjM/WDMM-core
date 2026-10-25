//! Stub drivers for cloud providers not yet implemented.
//!
//! Per brief/constraints §9: implement against official APIs, with
//! user-supplied OAuth/credentials only — no keys are embedded in source.
//!
//! Implemented (production):
//! - `webdav`        (wdmm_drivers::WebDavDriver)
//! - `ctfile`        (wdmm_drivers::CtFileDriver)
//! - `baidu`         (wdmm_drivers::BaiduDriver) — list/get/mkdir/rename/remove/content reads
//! - `123`           (wdmm_drivers::Pan123Driver) — list/get/mkdir/rename/remove/content
//! - `aliyundrive`   (wdmm_drivers::AliyunDriver) — list/get/mkdir/rename/remove/content reads
//! - `open115`       (wdmm_drivers::Open115Driver) — list/get/mkdir/rename/remove/content
//! - `terabox`       (wdmm_drivers::TeraboxDriver) — list/get/mkdir/rename/remove (Wave C)
//! - `netease_music` (wdmm_drivers::NeteaseMusicDriver) — list/get/remove (Wave C)
//!
//! All drivers now implemented. This module remains for future stubs.

#![allow(dead_code)]

use crate::driver::DriverError;

/// Shared helper: build the standard "not yet implemented" driver error.
#[allow(dead_code)]
fn todo_err(name: &str) -> DriverError {
    DriverError::Other(format!("TODO: {} driver not yet implemented", name))
}

