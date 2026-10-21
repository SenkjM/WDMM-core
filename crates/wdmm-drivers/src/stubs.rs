//! Stub drivers for baidu, aliyundrive, ctfile, 115, terabox, netease.
//!
//! Per brief: official API, user-supplied OAuth keys, no crates.io.
//! TODO: implement per architecture report §4.3 phasing.

use async_trait::async_trait;
use crate::driver::{CloudDriver, CloudFileItem, DriverError, Result};

pub struct BaiduDriver;

#[async_trait]
impl CloudDriver for BaiduDriver {
    async fn list(&self, _path: &str) -> Result<Vec<CloudFileItem>> {
        Err(DriverError::Other("TODO: BaiduDriver not yet implemented".into()))
    }

    async fn get(&self, _path: &str) -> Result<CloudFileItem> {
        Err(DriverError::Other("TODO: BaiduDriver not yet implemented".into()))
    }

    async fn mkdir(&self, _path: &str) -> Result<()> {
        Err(DriverError::Other("TODO: BaiduDriver not yet implemented".into()))
    }

    async fn rename(&self, _old: &str, _new: &str) -> Result<()> {
        Err(DriverError::Other("TODO: BaiduDriver not yet implemented".into()))
    }

    async fn remove(&self, _path: &str) -> Result<()> {
        Err(DriverError::Other("TODO: BaiduDriver not yet implemented".into()))
    }
}

// Repeat for Aliyun, CtFile, 115, Terabox, Netease — all stubs for now.
