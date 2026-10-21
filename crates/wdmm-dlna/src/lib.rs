//! `wdmm-dlna` — DLNA/UPnP SSDP discovery + unified local HTTP range proxy.
//!
//! ## Ownership
//! Per constraints §3 the Rust core owns:
//! - SSDP renderer discovery ([`SsdpDiscovery`])
//! - the unified local proxy used for casting and streaming ([`proxy`])
//! - AVTransport SOAP control (stub until round 4)
//!
//! Platform frontends own playback stacks, media sessions and FGS lifetime;
//! they only ask the core for a proxy URL.

#![forbid(unsafe_code)]

pub mod proxy;

use std::net::SocketAddr;
use std::time::Duration;
use thiserror::Error;
use tokio::net::UdpSocket;
use tracing::{debug, info};

pub use proxy::ProxyHandle;

#[derive(Debug, Error)]
pub enum DlnaError {
    #[error("discovery failed: {0}")]
    Discovery(String),

    #[error("cast failed: {0}")]
    Cast(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("unsupported")]
    Unsupported,
}

pub type DlnaResult<T> = std::result::Result<T, DlnaError>;

/// DLNA renderer device discovered via SSDP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DlnaRenderer {
    /// `USN` header — stable unique id used for de-duplication.
    pub usn: String,
    /// `LOCATION` header — URL of the device description XML.
    pub location: String,
    /// `SERVER` header, if present (display hint until description is fetched).
    pub server: Option<String>,
}

const SSDP_ADDR: &str = "239.255.255.250:1900";
const MEDIA_RENDERER_ST: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";

/// SSDP discovery (M-SEARCH over UDP multicast).
pub struct SsdpDiscovery {
    target: SocketAddr,
}

impl SsdpDiscovery {
    pub fn new() -> Self {
        Self {
            target: SSDP_ADDR.parse().expect("valid SSDP address"),
        }
    }

    /// Override the M-SEARCH target (tests send to a local fake responder).
    pub fn with_target(target: SocketAddr) -> Self {
        Self { target }
    }

    /// Send an M-SEARCH and collect renderer responses until `timeout` elapses.
    /// Responses are de-duplicated by USN.
    pub async fn discover(&self, timeout: Duration) -> DlnaResult<Vec<DlnaRenderer>> {
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        let mx = timeout.as_secs().clamp(1, 5);
        let msg = format!(
            "M-SEARCH * HTTP/1.1\r\nHOST: {SSDP_ADDR}\r\nMAN: \"ssdp:discover\"\r\nMX: {mx}\r\nST: {MEDIA_RENDERER_ST}\r\n\r\n"
        );
        socket.send_to(msg.as_bytes(), self.target).await?;

        let mut found: Vec<DlnaRenderer> = Vec::new();
        let mut buf = [0u8; 2048];
        let deadline = tokio::time::Instant::now() + timeout;
        while let Ok(res) = tokio::time::timeout_at(deadline, socket.recv_from(&mut buf)).await {
            let (len, from) = res?;
            let text = String::from_utf8_lossy(&buf[..len]);
            debug!("ssdp response from {from}");
            if let Some(r) = parse_response(&text) {
                if !found.iter().any(|f| f.usn == r.usn) {
                    found.push(r);
                }
            }
        }
        info!("ssdp: {} renderer(s)", found.len());
        Ok(found)
    }
}

impl Default for SsdpDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse an SSDP `HTTP/1.1 200 OK` response. Header names are case-insensitive.
pub(crate) fn parse_response(text: &str) -> Option<DlnaRenderer> {
    let mut lines = text.lines();
    if !lines.next()?.trim().to_ascii_uppercase().starts_with("HTTP/1.1 200") {
        return None;
    }
    let (mut location, mut usn, mut server) = (None, None, None);
    for line in lines {
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim().to_string();
        match k.trim().to_ascii_lowercase().as_str() {
            "location" => location = Some(v),
            "usn" => usn = Some(v),
            "server" => server = Some(v),
            _ => {}
        }
    }
    let location = location?;
    Some(DlnaRenderer {
        usn: usn.unwrap_or_else(|| location.clone()),
        location,
        server,
    })
}

/// DLNA cast service — discovery plus (future) AVTransport control.
pub struct DlnaCastService {
    discovery: SsdpDiscovery,
}

impl DlnaCastService {
    pub fn new() -> Self {
        Self {
            discovery: SsdpDiscovery::new(),
        }
    }

    pub async fn discover(&self, timeout: Duration) -> DlnaResult<Vec<DlnaRenderer>> {
        self.discovery.discover(timeout).await
    }

    /// Cast a proxy URL to a renderer. AVTransport SOAP lands in round 4.
    pub async fn cast(&self, _renderer: &DlnaRenderer, _url: &str) -> DlnaResult<()> {
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
    use crate::proxy::resolve_range;

    const RESP: &str = "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nLocation: http://192.168.1.20:49152/desc.xml\r\nSERVER: Linux UPnP/1.0 TV/1.0\r\nST: urn:schemas-upnp-org:device:MediaRenderer:1\r\nUSN: uuid:tv-1::urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n";

    #[test]
    fn parses_ssdp_response() {
        let r = parse_response(RESP).unwrap();
        assert_eq!(r.location, "http://192.168.1.20:49152/desc.xml");
        assert!(r.usn.starts_with("uuid:tv-1"));
        assert_eq!(r.server.as_deref(), Some("Linux UPnP/1.0 TV/1.0"));
        assert!(parse_response("NOTIFY * HTTP/1.1\r\n\r\n").is_none());
    }

    #[tokio::test]
    async fn discovery_loop_with_fake_responder() {
        // Fake renderer on loopback: answers every M-SEARCH twice (dedupe check).
        let responder = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let target = responder.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let (n, from) = responder.recv_from(&mut buf).await.unwrap();
            assert!(String::from_utf8_lossy(&buf[..n]).starts_with("M-SEARCH"));
            for _ in 0..2 {
                responder.send_to(RESP.as_bytes(), from).await.unwrap();
            }
        });

        let found = SsdpDiscovery::with_target(target)
            .discover(Duration::from_millis(300))
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn range_resolution() {
        assert_eq!(resolve_range("bytes=0-9", 100), Some((0, 9)));
        assert_eq!(resolve_range("bytes=90-", 100), Some((90, 99)));
        assert_eq!(resolve_range("bytes=-10", 100), Some((90, 99)));
        assert_eq!(resolve_range("bytes=50-500", 100), Some((50, 99)));
        assert_eq!(resolve_range("bytes=100-", 100), None);
        assert_eq!(resolve_range("bytes=9-0", 100), None);
        assert_eq!(resolve_range("bytes=0-1,5-6", 100), None);
        assert_eq!(resolve_range("bytes=abc", 100), None);
        assert_eq!(resolve_range("bytes=0-0", 0), None);
    }

    #[tokio::test]
    async fn proxy_serves_registered_file_with_ranges() {
        let path = std::env::temp_dir().join(format!("wdmm-proxy-{}.bin", std::process::id()));
        let data: Vec<u8> = (0..=255u8).collect();
        std::fs::write(&path, &data).unwrap();

        let proxy = ProxyHandle::start(([127, 0, 0, 1], 0).into()).await.unwrap();
        let url = proxy.register(&path);
        let client = reqwest::Client::new();

        let full = client.get(&url).send().await.unwrap();
        assert_eq!(full.status(), 200);
        assert_eq!(full.bytes().await.unwrap().as_ref(), data.as_slice());

        let part = client.get(&url).header("Range", "bytes=10-19").send().await.unwrap();
        assert_eq!(part.status(), 206);
        assert_eq!(part.headers()["content-range"], "bytes 10-19/256");
        assert_eq!(part.bytes().await.unwrap().as_ref(), &data[10..20]);

        let bad = client.get(&url).header("Range", "bytes=300-").send().await.unwrap();
        assert_eq!(bad.status(), 416);

        // Unregistered tokens and raw paths are never served.
        let base = format!("http://{}", proxy.addr());
        let raw = client.get(format!("{base}/stream/deadbeef")).send().await.unwrap();
        assert_eq!(raw.status(), 404);
        let traversal = client.get(format!("{base}/file/etc/passwd")).send().await.unwrap();
        assert_eq!(traversal.status(), 404);

        proxy.unregister(&url);
        assert_eq!(client.get(&url).send().await.unwrap().status(), 404);

        proxy.stop();
        let _ = std::fs::remove_file(&path);
    }
}
