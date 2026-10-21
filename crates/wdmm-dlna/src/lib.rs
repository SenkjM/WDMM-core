//! `wdmm-dlna` — DLNA/UPnP SSDP discovery + AVTransport SOAP stub.
//!
//! Full impl per brief: module/interface stubs, README notes ownership.

#![forbid(unsafe_code)]

use thiserror::Error;

#[derive(Debug, Error)]
pub enum DlnaError {
    #[error("discovery failed: {0}")]
    Discovery(String),

    #[error("cast failed: {0}")]
    Cast(String),

    #[error("unsupported")]
    Unsupported,
}

pub type DlnaResult<T> = std::result::Result<T, DlnaError>;

/// DLNA renderer device.
#[derive(Debug, Clone)]
pub struct DlnaRenderer {
    pub name: String,
    pub url: String,
}

/// DLNA/UPnP cast service stub.
pub struct DlnaCastService;

impl DlnaCastService {
    pub fn new() -> Self {
        Self
    }

    /// Discover renderers via SSDP multicast.
    pub async fn discover(&self) -> DlnaResult<Vec<DlnaRenderer>> {
        // TODO: SSDP M-SEARCH multicast
        Ok(vec![])
    }

    /// Cast a URL to a renderer.
    pub async fn cast(&self, _renderer: &DlnaRenderer, _url: &str) -> DlnaResult<()> {
        // TODO: AVTransport SetAVTransportURI SOAP call
        Err(DlnaError::Unsupported)
    }
}

impl Default for DlnaCastService {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn discover_stub() {
        let svc = DlnaCastService::new();
        let devices = svc.discover().await.unwrap();
        assert!(devices.is_empty());
    }
}
