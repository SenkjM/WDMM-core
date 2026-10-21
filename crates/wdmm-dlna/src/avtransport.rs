//! DLNA AVTransport control point — SOAP skeletons for Play/Pause/Stop/Seek
//! and SetAVTransportURI.
//!
//! ## Flow
//! 1. Fetch the device description XML from [`DlnaRenderer::location`].
//! 2. Parse the `<controlURL>` of the `urn:schemas-upnp-org:service:AVTransport:1`
//!    service (resolving it against the description base URL).
//! 3. POST SOAP envelopes to that control URL.
//!
//! The SOAP envelope shape matches the UPnP AVTransport:1 service template. This
//! is the "skeleton" level called for by the round-4 brief; real renderers may
//! require `CurrentURIMetaData` (DIDL-Lite) which can be added later —
//! `SetAVTransportURI` accepts an empty metadata string and most renderers
//! will still play a plain media URL.

use quick_xml::events::Event;
use quick_xml::Reader;
use reqwest::Client;
use url::Url;

use crate::{DlnaError, DlnaResult, DlnaRenderer};

const AVTRANSPORT_SERVICE_TYPE: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const SOAP_ENVELOPE_NS: &str = "http://schemas.xmlsoap.org/soap/envelope/";
const SOAP_ENCODING_NS: &str = "http://schemas.xmlsoap.org/soap/encoding/";

/// AVTransport SOAP control client. One instance per renderer.
pub struct AvTransportClient {
    control_url: String,
    client: Client,
}

impl AvTransportClient {
    /// Construct from an already-resolved control URL (rare; prefer
    /// [`AvTransportClient::from_renderer`]).
    pub fn new(control_url: String) -> Self {
        Self {
            control_url,
            client: Client::new(),
        }
    }

    /// Fetch the device description at `renderer.location`, parse the
    /// AVTransport control URL, and build a client targeting it.
    pub async fn from_renderer(renderer: &DlnaRenderer) -> DlnaResult<Self> {
        let control_url = resolve_avtransport_control_url(renderer).await?;
        Ok(Self::new(control_url))
    }

    /// `SetAVTransportURI` — point the renderer at a media URL.
    pub async fn set_uri(&self, instance_id: u32, uri: &str) -> DlnaResult<()> {
        let body = soap_action(
            "SetAVTransportURI",
            &[
                ("<InstanceID>", &instance_id.to_string()),
                ("<CurrentURI>", uri),
                ("<CurrentURIMetaData>", ""), // empty DIDL-Lite is accepted by most renderers
            ],
        );
        self.post_action("SetAVTransportURI", &body).await
    }

    /// `Play` — start playback at `Speed=1`.
    pub async fn play(&self, instance_id: u32, speed: u8) -> DlnaResult<()> {
        let speed_str = speed.to_string();
        let body = soap_action(
            "Play",
            &[
                ("<InstanceID>", &instance_id.to_string()),
                ("<Speed>", &speed_str),
            ],
        );
        self.post_action("Play", &body).await
    }

    /// `Pause`.
    pub async fn pause(&self, instance_id: u32) -> DlnaResult<()> {
        let body = soap_action(
            "Pause",
            &[("<InstanceID>", &instance_id.to_string())],
        );
        self.post_action("Pause", &body).await
    }

    /// `Stop`.
    pub async fn stop(&self, instance_id: u32) -> DlnaResult<()> {
        let body = soap_action(
            "Stop",
            &[("<InstanceID>", &instance_id.to_string())],
        );
        self.post_action("Stop", &body).await
    }

    /// `Seek` — `unit` is `REL_TIME` / `TRACK_NR` / `ABS_COUNT` etc.
    pub async fn seek(&self, instance_id: u32, unit: &str, target: &str) -> DlnaResult<()> {
        let body = soap_action(
            "Seek",
            &[
                ("<InstanceID>", &instance_id.to_string()),
                ("<Unit>", unit),
                ("<Target>", target),
            ],
        );
        self.post_action("Seek", &body).await
    }

    async fn post_action(&self, action: &str, body: &str) -> DlnaResult<()> {
        let soap_action = format!("\"{AVTRANSPORT_SERVICE_TYPE}#{action}\"");
        let resp = self
            .client
            .post(&self.control_url)
            .header("Content-Type", "text/xml; charset=utf-8")
            .header("SOAPACTION", &soap_action)
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| DlnaError::Cast(format!("soap request failed: {e}")))?;

        if !resp.status().is_success() {
            return Err(DlnaError::Cast(format!(
                "soap action {action} returned {}",
                resp.status()
            )));
        }
        Ok(())
    }
}

/// Build a SOAP envelope body for an AVTransport action.
fn soap_action(action: &str, args: &[(&str, &str)]) -> String {
    let mut s = String::new();
    s.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>");
    s.push_str(&format!(
        "<s:Envelope xmlns:s=\"{SOAP_ENVELOPE_NS}\" s:encodingStyle=\"{SOAP_ENCODING_NS}\">"
    ));
    s.push_str("<s:Body>");
    s.push_str(&format!(
        "<u:{action} xmlns:u=\"{AVTRANSPORT_SERVICE_TYPE}\">"
    ));
    for (tag, val) in args {
        // Basic XML-escape of ampersand/angle brackets in values.
        let v = val
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        s.push_str(&format!("{tag}{v}</{}>", tag.trim_start_matches('<')));
    }
    s.push_str(&format!("</u:{action}>"));
    s.push_str("</s:Body>");
    s.push_str("</s:Envelope>");
    s
}

/// Fetch the device description at `renderer.location` and return the
/// AVTransport service control URL (absolute).
pub async fn resolve_avtransport_control_url(renderer: &DlnaRenderer) -> DlnaResult<String> {
    let client = Client::new();
    let resp = client
        .get(&renderer.location)
        .send()
        .await
        .map_err(|e| DlnaError::Cast(format!("device description fetch failed: {e}")))?;
    if !resp.status().is_success() {
        return Err(DlnaError::Cast(format!(
            "device description returned {}",
            resp.status()
        )));
    }
    let xml = resp
        .text()
        .await
        .map_err(|e| DlnaError::Cast(format!("body read failed: {e}")))?;
    let relative = parse_avtransport_control_url(&xml)
        .ok_or_else(|| DlnaError::Cast("AVTransport service not found in description".into()))?;
    let base = Url::parse(&renderer.location)
        .map_err(|e| DlnaError::Cast(format!("bad location url: {e}")))?;
    let joined = base
        .join(&relative)
        .map_err(|e| DlnaError::Cast(format!("control URL join failed: {e}")))?;
    Ok(joined.to_string())
}

/// Parse a device description XML and return the `controlURL` text of the
/// AVTransport service, if present.
pub(crate) fn parse_avtransport_control_url(xml: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut in_service = false;
    let mut in_control_url = false;
    let mut current_service_type = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let local_name = e.name().local_name();
                let name = local_name.as_ref();
                if name == b"service" {
                    in_service = true;
                    current_service_type.clear();
                } else if in_service && name == b"controlURL" {
                    in_control_url = true;
                } else if in_service && name == b"serviceType" {
                    if let Ok(Event::Text(t)) = reader.read_event_into(&mut buf) {
                        if let Ok(s) = t.unescape() {
                            current_service_type = s.to_string();
                        }
                    }
                }
            }
            Ok(Event::Empty(e)) => {
                let local_name = e.name().local_name();
                if in_service && local_name.as_ref() == b"controlURL" {
                    // self-closing controlURL has no text content — nothing to do.
                }
            }
            Ok(Event::Text(t)) if in_control_url => {
                if let Ok(s) = t.unescape() {
                    if current_service_type == AVTRANSPORT_SERVICE_TYPE {
                        return Some(s.to_string());
                    }
                }
            }
            Ok(Event::End(e)) => {
                let local_name = e.name().local_name();
                let name = local_name.as_ref();
                if name == b"controlURL" {
                    in_control_url = false;
                } else if name == b"service" {
                    in_service = false;
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESC_XML: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <device>
    <friendlyName>Mock TV</friendlyName>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>
        <controlURL>/ctl/RenderingControl</controlURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
        <controlURL>/ctl/AVTransport</controlURL>
      </service>
    </serviceList>
  </device>
</root>"#;

    #[test]
    fn parses_avtransport_control_url() {
        let url = parse_avtransport_control_url(DESC_XML).unwrap();
        assert_eq!(url, "/ctl/AVTransport");
    }

    #[test]
    fn resolves_relative_control_url() {
        // Direct relative→absolute join (no async needed).
        let base = Url::parse("http://192.168.1.20:49152/desc.xml").unwrap();
        assert_eq!(
            base.join("/ctl/AVTransport").unwrap().to_string(),
            "http://192.168.1.20:49152/ctl/AVTransport"
        );
    }

    #[test]
    fn soap_envelope_shape() {
        let body = soap_action("Play", &[
            ("<InstanceID>", "0"),
            ("<Speed>", "1"),
        ]);
        assert!(body.contains("<s:Envelope"));
        assert!(body.contains("<u:Play xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\">"));
        assert!(body.contains("<InstanceID>0</InstanceID>"));
        assert!(body.contains("<Speed>1</Speed>"));
    }

    #[test]
    fn soap_envelope_escapes_values() {
        let body = soap_action(
            "SetAVTransportURI",
            &[("<CurrentURI>", "http://h.x/a?b=1&c=2")],
        );
        assert!(body.contains("b=1&amp;c=2"));
        assert!(!body.contains("b=1&c=2"));
    }

    // ── discover → setURI → play smoke test over mock HTTP ────────────────────

    /// Minimal DLNA mock: serves the device description, then two SOAP POSTs
    /// (SetAVTransportURI, Play), asserting on the request shape.
    #[tokio::test]
    async fn cast_smoke_test_over_mock_http() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let soap_count = Arc::new(AtomicUsize::new(0));
        let desc_count = Arc::new(AtomicUsize::new(0));
        let soap_count_clone = Arc::clone(&soap_count);
        let desc_count_clone = Arc::clone(&desc_count);

        tokio::spawn(async move {
            // Serve 3 connections: 1 GET desc + 2 POSTs (SetAVTransportURI, Play).
            for _ in 0..3 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);

                let (status, body) = if req.starts_with("GET ") {
                    desc_count_clone.fetch_add(1, Ordering::SeqCst);
                    ("200 OK", DESC_XML.to_string())
                } else if req.starts_with("POST ") {
                    let n = soap_count_clone.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        // SetAVTransportURI
                        assert!(
                            req.contains("SetAVTransportURI"),
                            "expected SetAVTransportURI: {req}"
                        );
                        assert!(req.contains("http://media/song.mp3"));
                    } else {
                        // Play
                        assert!(req.contains("u:Play"), "expected Play: {req}");
                        assert!(req.contains("<Speed>1</Speed>"));
                    }
                    assert!(req.to_ascii_lowercase().contains("soapaction"));
                    ("200 OK", String::from_utf8_lossy(b"<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:PlayResponse xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\"/></s:Body></s:Envelope>").to_string())
                } else {
                    ("404 Not Found", String::new())
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/xml; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        tokio::task::yield_now().await;

        let renderer = DlnaRenderer {
            usn: "uuid:mock-tv".into(),
            location: format!("http://127.0.0.1:{port}/desc.xml"),
            server: Some("Mock".into()),
        };

        // Use the cast service: discover → resolve → setURI → play.
        let cast = crate::DlnaCastService::new();
        cast.cast(&renderer, "http://media/song.mp3")
            .await
            .expect("cast should succeed");

        assert_eq!(desc_count.load(Ordering::SeqCst), 1);
        assert_eq!(soap_count.load(Ordering::SeqCst), 2);
    }
}
