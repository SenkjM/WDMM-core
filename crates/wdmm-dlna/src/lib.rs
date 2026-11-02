//! `wdmm-dlna` — DLNA/UPnP SSDP discovery + SOAP control of renderers.
//!
//! ## Ownership
//! Per constraints §3 the Rust core owns:
//! - SSDP renderer discovery ([`SsdpDiscovery`])
//! - AVTransport SOAP control ([`avtransport::AvTransportClient`])
//! - ConnectionManager protocol negotiation
//!   ([`connection_manager::ConnectionManagerClient`])
//! - DIDL-Lite metadata building ([`didl::build_didl_lite`])
//! - the cast session state machine ([`session::DlnaSession`])
//! - the unified local proxy, now in the dedicated [`wdmm_proxy`] crate
//!   (re-exported here as [`ProxyHandle`] for convenience).
//!
//! Platform frontends own playback stacks, media sessions and FGS lifetime;
//! they only ask the core for a proxy URL.

#![forbid(unsafe_code)]

pub mod avtransport;
pub mod connection_manager;
pub mod didl;
pub mod gena;
pub mod rendering_control;
pub mod session;

pub use connection_manager::ConnectionManagerClient;
pub use didl::{build_didl_lite, subtitle_content_type, subtitle_protocol_info, DidlRes};
pub use gena::{GenaEvent, GenaSubscription};
pub use rendering_control::RenderingControlClient;
pub use session::{DlnaSession, TickResult};

#[cfg(test)]
pub(crate) mod test_support;

use std::net::SocketAddr;
use std::time::Duration;
use thiserror::Error;
use tokio::net::UdpSocket;
use tracing::{debug, info};

pub use avtransport::AvTransportClient;
// The unified local proxy now lives in `wdmm-proxy`. DLNA cast callers still
// get a `ProxyHandle` through this crate for convenience; new code should depend
// on `wdmm-proxy` directly.
pub use wdmm_proxy::ProxyHandle;

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

/// Parse a UPnP device description XML and return the `controlURL` or
/// `eventSubURL` of the named UPnP service type, if present.
///
/// `url_tag` is `"controlURL"` for SOAP control or `"eventSubURL"` for GENA
/// event subscriptions. Used by `avtransport`, `rendering_control`,
/// `connection_manager`, and `gena`.
pub(crate) fn parse_service_url(xml: &str, service_type: &str, url_tag: &str) -> Option<String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut in_service = false;
    let mut in_target_tag = false;
    let mut current_service_type = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let local = e.name().local_name();
                let name = local.as_ref();
                if name == b"service" {
                    in_service = true;
                    current_service_type.clear();
                } else if in_service && name == url_tag.as_bytes() {
                    in_target_tag = true;
                } else if in_service && name == b"serviceType" {
                    if let Ok(Event::Text(t)) = reader.read_event_into(&mut buf) {
                        if let Ok(s) = t.unescape() {
                            current_service_type = s.to_string();
                        }
                    }
                }
            }
            Ok(Event::Text(t)) if in_target_tag => {
                if let Ok(s) = t.unescape() {
                    if current_service_type == service_type {
                        return Some(s.to_string());
                    }
                }
            }
            Ok(Event::End(e)) => {
                let local = e.name().local_name();
                let name = local.as_ref();
                if name == url_tag.as_bytes() {
                    in_target_tag = false;
                } else if name == b"service" {
                    in_service = false;
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    None
}

/// Fetch a UPnP device description document and return its XML body.
///
/// Shared by the service clients so they agree on error mapping. Non-2xx
/// responses are reported as [`DlnaError::Cast`].
pub(crate) async fn fetch_description(location: &str) -> DlnaResult<String> {
    let resp = reqwest::get(location)
        .await
        .map_err(|e| DlnaError::Cast(format!("device description fetch failed: {e}")))?;
    if !resp.status().is_success() {
        return Err(DlnaError::Cast(format!(
            "device description returned {}",
            resp.status()
        )));
    }
    resp.text()
        .await
        .map_err(|e| DlnaError::Cast(format!("device description read failed: {e}")))
}

/// Device description fields the discovery UI needs.
///
/// Only the handful of fields the renderer picker displays are extracted; the
/// raw XML stays inside this crate. Missing or empty elements are `None`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RendererDescription {
    pub friendly_name: Option<String>,
    pub model_name: Option<String>,
    pub manufacturer: Option<String>,
    pub model_number: Option<String>,
    /// The description advertises the AVTransport service.
    pub has_avtransport: bool,
}

/// Fetch the device description at `location` and extract the fields above.
///
/// Fails only when the description cannot be fetched (non-2xx is
/// [`DlnaError::Cast`]); malformed XML yields default fields instead of an
/// error so one broken renderer cannot break the discovery list.
pub async fn describe_renderer(location: &str) -> DlnaResult<RendererDescription> {
    let xml = fetch_description(location).await?;
    Ok(parse_renderer_description(&xml))
}

/// Parse a UPnP device description into [`RendererDescription`].
///
/// Looks at the first occurrence of each tag inside `<device>` and trims the
/// text; malformed XML never panics.
pub fn parse_renderer_description(xml: &str) -> RendererDescription {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut desc = RendererDescription::default();
    let mut device_depth = 0usize;
    // Tag currently being read, as one of the four fields we care about.
    let mut current: Option<&'static str> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let local = e.name().local_name();
                let name = local.as_ref();
                if name == b"device" {
                    device_depth += 1;
                }
                current = if device_depth > 0 {
                    match name {
                        b"friendlyName" => Some("friendlyName"),
                        b"modelName" => Some("modelName"),
                        b"manufacturer" => Some("manufacturer"),
                        b"modelNumber" => Some("modelNumber"),
                        _ => None,
                    }
                } else {
                    None
                };
            }
            Ok(Event::Text(t)) if device_depth > 0 => {
                if let Some(tag) = current {
                    if let Ok(text) = t.unescape() {
                        let text = text.trim();
                        if !text.is_empty() {
                            let slot = match tag {
                                "friendlyName" => &mut desc.friendly_name,
                                "modelName" => &mut desc.model_name,
                                "manufacturer" => &mut desc.manufacturer,
                                _ => &mut desc.model_number,
                            };
                            if slot.is_none() {
                                *slot = Some(text.to_string());
                            }
                        }
                    }
                }
            }
            Ok(Event::End(e)) => {
                let local = e.name().local_name();
                let name = local.as_ref();
                if name == b"device" {
                    device_depth = device_depth.saturating_sub(1);
                }
                current = None;
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    desc.has_avtransport =
        parse_service_url(xml, avtransport::AVTRANSPORT_SERVICE_TYPE, "controlURL").is_some();
    desc
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

    /// Cast a proxy URL to a renderer using AVTransport SOAP.
    ///
    /// Flow: fetch device description → resolve AVTransport control URL →
    /// SetAVTransportURI → Play(Speed=1).
    pub async fn cast(&self, renderer: &DlnaRenderer, url: &str) -> DlnaResult<()> {
        let client = AvTransportClient::from_renderer(renderer).await?;
        client.set_uri(0, url).await?;
        client.play(0, 1).await?;
        info!("cast to {} url={url}", renderer.usn);
        Ok(())
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
    use wdmm_proxy::resolve_range;

    const RESP: &str = "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nLocation: http://192.168.1.20:49152/desc.xml\r\nSERVER: Linux UPnP/1.0 TV/1.0\r\nST: urn:schemas-upnp-org:device:MediaRenderer:1\r\nUSN: uuid:tv-1::urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n";

    const DESC_FULL: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
    <friendlyName> Living Room TV </friendlyName>
    <manufacturer>Acme</manufacturer>
    <modelName>SmartTV 9000</modelName>
    <modelNumber>9000-X</modelNumber>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
        <controlURL>/ctl/AVTransport</controlURL>
      </service>
    </serviceList>
  </device>
</root>"#;

    #[test]
    fn parses_ssdp_response() {
        let r = parse_response(RESP).unwrap();
        assert_eq!(r.location, "http://192.168.1.20:49152/desc.xml");
        assert!(r.usn.starts_with("uuid:tv-1"));
        assert_eq!(r.server.as_deref(), Some("Linux UPnP/1.0 TV/1.0"));
        assert!(parse_response("NOTIFY * HTTP/1.1\r\n\r\n").is_none());
    }

    #[test]
    fn parses_renderer_description() {
        let d = parse_renderer_description(DESC_FULL);
        assert_eq!(d.friendly_name.as_deref(), Some("Living Room TV"));
        assert_eq!(d.manufacturer.as_deref(), Some("Acme"));
        assert_eq!(d.model_name.as_deref(), Some("SmartTV 9000"));
        assert_eq!(d.model_number.as_deref(), Some("9000-X"));
        assert!(d.has_avtransport);

        let bare = parse_renderer_description(
            "<root><device><friendlyName>Bedroom</friendlyName></device></root>",
        );
        assert_eq!(bare.friendly_name.as_deref(), Some("Bedroom"));
        assert_eq!(bare.model_name, None);
        assert_eq!(bare.manufacturer, None);
        assert_eq!(bare.model_number, None);
        assert!(!bare.has_avtransport);

        // Malformed XML must not panic; whatever was parsed before the error is
        // kept and the service check simply fails.
        let broken = parse_renderer_description("<root><device><friendlyName>X</device>");
        assert!(!broken.has_avtransport);
    }

    #[tokio::test]
    async fn describe_renderer_fetches_and_parses_description() {
        let server = crate::test_support::spawn_fake_http(|req| {
            if req.starts_with("GET ") {
                crate::test_support::FakeResponse::ok(DESC_FULL)
            } else {
                crate::test_support::FakeResponse::not_found()
            }
        })
        .await;
        let desc = describe_renderer(&server.url("/desc.xml")).await.unwrap();
        assert_eq!(desc.friendly_name.as_deref(), Some("Living Room TV"));
        assert_eq!(desc.model_name.as_deref(), Some("SmartTV 9000"));
        assert!(desc.has_avtransport);

        let failing = crate::test_support::spawn_fake_http(|_| {
            crate::test_support::FakeResponse::not_found()
        })
        .await;
        assert!(describe_renderer(&failing.url("/desc.xml")).await.is_err());
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

        let proxy = ProxyHandle::new(wdmm_proxy::BindMode::Loopback, 0);
        let reg = proxy.register_file(&path, None, None, None).await.unwrap();
        let addr = proxy.addr().unwrap();
        let url = format!("http://{addr}{}", reg.path_template);
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

        // Unknown token → 404
        let raw = client
            .get(format!("http://{addr}/s/deadbeef00deadbeef00deadbeef0000"))
            .send()
            .await
            .unwrap();
        assert_eq!(raw.status(), 404);

        // Unregister → 404
        proxy.unregister(&reg.token);
        assert_eq!(client.get(&url).send().await.unwrap().status(), 404);

        proxy.stop();
        let _ = std::fs::remove_file(&path);
    }
}
