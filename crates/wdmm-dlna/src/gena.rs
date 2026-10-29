//! UPnP GENA (General Event Notification Architecture) subscription client.
//!
//! ## Overview
//! GENA lets a UPnP control point receive asynchronous state-change events from
//! a renderer (e.g. volume change, playback position, transport state).
//!
//! Flow:
//! 1. Spin up a local TCP server (`GenaSubscription::subscribe`).
//! 2. `SUBSCRIBE` the target service, advertising the local callback URL.
//! 3. The renderer POSTs XML event notifications to our callback server.
//! 4. The server parses events and sends `GenaEvent` values on an `mpsc` channel.
//! 5. Call `GenaSubscription::renew` before `timeout_secs` elapses or the renderer
//!    unsubscribes us.  Call `GenaSubscription::unsubscribe` to clean up.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use quick_xml::events::Event;
use quick_xml::Reader;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::{DlnaError, DlnaResult};

/// A single property-change event delivered by the renderer.
#[derive(Debug, Clone)]
pub struct GenaEvent {
    /// Sequence number supplied by the renderer (`SEQ` header; starts at 0).
    pub seq: u32,
    /// Parsed property name → value pairs from the `<e:propertyset>` body.
    pub properties: HashMap<String, String>,
}

/// An active GENA subscription.  Drop or call `unsubscribe` to clean up.
pub struct GenaSubscription {
    /// HTTP client for SUBSCRIBE / UNSUBSCRIBE requests.
    client: reqwest::Client,
    /// Absolute event-subscription URL (from the device description `eventSubURL`).
    event_sub_url: String,
    /// Subscription ID assigned by the renderer (SID header).
    sid: String,
    /// Local callback server address (for renewal / debugging).
    local_addr: SocketAddr,
    /// Requested timeout in seconds.
    timeout_secs: u32,
    /// Handle for the background callback-server task.
    _server_task: tokio::task::JoinHandle<()>,
}

impl GenaSubscription {
    /// Subscribe to `event_sub_url` and start a local callback server.
    ///
    /// `callback_host` is the IP or hostname that the renderer can reach us on.
    /// `timeout_secs` is the requested subscription duration (typical: 1800).
    /// Returns the active subscription and a channel receiver for events.
    pub async fn subscribe(
        event_sub_url: impl Into<String>,
        callback_host: &str,
        timeout_secs: u32,
    ) -> DlnaResult<(Self, mpsc::Receiver<GenaEvent>)> {
        let event_sub_url = event_sub_url.into();

        // Bind callback server on an OS-assigned port.
        let listener = TcpListener::bind(format!("{callback_host}:0"))
            .await
            .map_err(|e| DlnaError::Cast(format!("gena listener bind: {e}")))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| DlnaError::Cast(format!("gena local addr: {e}")))?;
        let callback_url = format!("http://{local_addr}/notify");

        let (tx, rx) = mpsc::channel(64);

        // Spawn the callback server.
        let server_tx = tx.clone();
        let server_task = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _peer)) => {
                        let tx2 = server_tx.clone();
                        tokio::spawn(handle_notify(stream, tx2));
                    }
                    Err(e) => {
                        warn!("gena accept error: {e}");
                        break;
                    }
                }
            }
        });

        // Send SUBSCRIBE.
        let client = reqwest::Client::new();
        let resp = client
            .request(
                reqwest::Method::from_bytes(b"SUBSCRIBE").unwrap(),
                &event_sub_url,
            )
            .header("NT", "upnp:event")
            .header("CALLBACK", format!("<{callback_url}>"))
            .header("TIMEOUT", format!("Second-{timeout_secs}"))
            .send()
            .await
            .map_err(|e| DlnaError::Cast(format!("SUBSCRIBE failed: {e}")))?;

        if !resp.status().is_success() {
            return Err(DlnaError::Cast(format!(
                "SUBSCRIBE returned {}",
                resp.status()
            )));
        }

        let sid = resp
            .headers()
            .get("SID")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| DlnaError::Cast("SUBSCRIBE response missing SID header".into()))?
            .to_owned();

        debug!("GENA subscribed sid={sid} callback={callback_url}");

        Ok((
            Self {
                client,
                event_sub_url,
                sid,
                local_addr,
                timeout_secs,
                _server_task: server_task,
            },
            rx,
        ))
    }

    /// Renew the subscription before it expires.
    pub async fn renew(&self) -> DlnaResult<()> {
        let resp = self
            .client
            .request(
                reqwest::Method::from_bytes(b"SUBSCRIBE").unwrap(),
                &self.event_sub_url,
            )
            .header("SID", &self.sid)
            .header("TIMEOUT", format!("Second-{}", self.timeout_secs))
            .send()
            .await
            .map_err(|e| DlnaError::Cast(format!("SUBSCRIBE renew failed: {e}")))?;

        if !resp.status().is_success() {
            return Err(DlnaError::Cast(format!(
                "SUBSCRIBE renew returned {}",
                resp.status()
            )));
        }
        debug!("GENA renewed sid={}", self.sid);
        Ok(())
    }

    /// Unsubscribe and stop receiving events.  Idempotent (ignores 4xx errors).
    pub async fn unsubscribe(self) -> DlnaResult<()> {
        let resp = self
            .client
            .request(
                reqwest::Method::from_bytes(b"UNSUBSCRIBE").unwrap(),
                &self.event_sub_url,
            )
            .header("SID", &self.sid)
            .send()
            .await
            .map_err(|e| DlnaError::Cast(format!("UNSUBSCRIBE failed: {e}")))?;

        if !resp.status().is_success() && !resp.status().is_client_error() {
            return Err(DlnaError::Cast(format!(
                "UNSUBSCRIBE returned {}",
                resp.status()
            )));
        }
        debug!("GENA unsubscribed sid={}", self.sid);
        self._server_task.abort();
        Ok(())
    }

    /// Local address the callback server is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Subscription ID (SID) assigned by the renderer.
    pub fn sid(&self) -> &str {
        &self.sid
    }
}

/// Handle one incoming NOTIFY connection from a renderer.
async fn handle_notify(mut stream: TcpStream, tx: mpsc::Sender<GenaEvent>) {
    let mut buf = vec![0u8; 8192];
    let n = match stream.read(&mut buf).await {
        Ok(n) => n,
        Err(e) => {
            warn!("gena read error: {e}");
            return;
        }
    };
    let raw = String::from_utf8_lossy(&buf[..n]);

    // Parse HTTP request line + headers to find SEQ and locate the body.
    let seq = parse_seq_header(&raw).unwrap_or(0);
    let body = raw
        .split_once("\r\n\r\n")
        .map(|(_, b)| b)
        .unwrap_or_default();

    let properties = parse_propertyset(body);

    let _ = tx
        .send_timeout(GenaEvent { seq, properties }, Duration::from_millis(100))
        .await;

    // Return 200 OK to the renderer.
    let resp = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let _ = stream.write_all(resp).await;
}

/// Extract the `SEQ` header value from a raw HTTP request.
fn parse_seq_header(raw: &str) -> Option<u32> {
    for line in raw.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("seq:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

/// Parse a UPnP `<e:propertyset>` body into a property name → value map.
/// Handles both `<e:property><Name>value</Name></e:property>` and flat
/// `<Name>value</Name>` shapes.
fn parse_propertyset(xml: &str) -> HashMap<String, String> {
    let mut result = HashMap::new();
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut current_prop: Option<String> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let local = e.name().local_name();
                let name = String::from_utf8_lossy(local.as_ref()).to_string();
                // Skip envelope/body tags; everything inside <e:property> is a property.
                if name != "propertyset" && name != "property" {
                    current_prop = Some(name);
                }
            }
            Ok(Event::Text(t)) => {
                if let Some(ref key) = current_prop {
                    if let Ok(val) = t.unescape() {
                        result.entry(key.clone()).or_insert_with(|| val.to_string());
                    }
                }
            }
            Ok(Event::End(_)) => {
                current_prop = None;
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_propertyset_volume() {
        let xml = r#"<?xml version="1.0"?>
<e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0">
  <e:property>
    <Volume>75</Volume>
  </e:property>
  <e:property>
    <Mute>0</Mute>
  </e:property>
</e:propertyset>"#;
        let props = parse_propertyset(xml);
        assert_eq!(props.get("Volume").map(|s| s.as_str()), Some("75"));
        assert_eq!(props.get("Mute").map(|s| s.as_str()), Some("0"));
    }

    #[test]
    fn parse_seq_header_found() {
        let raw = "NOTIFY /notify HTTP/1.1\r\nSEQ: 5\r\nContent-Length: 0\r\n\r\n";
        assert_eq!(parse_seq_header(raw), Some(5));
    }

    #[test]
    fn parse_seq_header_missing() {
        let raw = "NOTIFY /notify HTTP/1.1\r\nContent-Length: 0\r\n\r\n";
        assert_eq!(parse_seq_header(raw), None);
    }

    #[test]
    fn gena_event_debug() {
        let ev = GenaEvent {
            seq: 3,
            properties: [("Volume".into(), "50".into())].into(),
        };
        assert!(format!("{ev:?}").contains("seq: 3"));
    }
}
