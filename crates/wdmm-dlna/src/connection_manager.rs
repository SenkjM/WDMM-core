//! UPnP ConnectionManager:1 client — renderer sink protocol capabilities.
//!
//! Ports the Dart `_connMgrControlUrl` / `_soapConnMgr` /
//! `_getSinkProtocolInfo` / `_negotiateProtocolInfo` pair from
//! `lib/services/dlna_cast_service.dart`, with the same semantics:
//!
//! - `GetProtocolInfo` returns a comma-separated `<Sink>` list of
//!   `protocolInfo` four-tuples.
//! - [`ConnectionManagerClient::negotiate`] picks the first sink entry whose
//!   content format matches the MIME type exactly, then a wildcard entry
//!   (`video/*` / `*/*`) rewritten to the real MIME, and finally falls back to
//!   `http-get:*:<mime>:DLNA.ORG_OP=01`.

use reqwest::Client;

use crate::avtransport::{extract_soap_values, post_soap, soap_envelope};
use crate::{fetch_description, parse_service_url, DlnaError, DlnaRenderer, DlnaResult};

/// UPnP service type of ConnectionManager:1.
pub const CONNECTION_MANAGER_SERVICE_TYPE: &str =
    "urn:schemas-upnp-org:service:ConnectionManager:1";

/// ConnectionManager SOAP control client. One instance per renderer.
pub struct ConnectionManagerClient {
    control_url: String,
    client: Client,
}

impl ConnectionManagerClient {
    /// Construct from an already-resolved control URL (mainly for tests).
    pub fn new(control_url: impl Into<String>) -> Self {
        Self {
            control_url: control_url.into(),
            client: Client::new(),
        }
    }

    /// Fetch the device description at `renderer.location`, parse the
    /// ConnectionManager control URL, and build a client targeting it.
    ///
    /// Returns [`DlnaError::Cast`] when the description cannot be fetched or
    /// does not advertise ConnectionManager (a strict renderer would reject
    /// the cast anyway — Dart treats this as an empty sink list).
    pub async fn from_renderer(renderer: &DlnaRenderer) -> DlnaResult<Self> {
        let xml = fetch_description(&renderer.location).await?;
        let relative = parse_service_url(&xml, CONNECTION_MANAGER_SERVICE_TYPE, "controlURL")
            .ok_or_else(|| {
                DlnaError::Cast(
                    "ConnectionManager controlURL not found in device description".into(),
                )
            })?;
        let base = url::Url::parse(&renderer.location)
            .map_err(|e| DlnaError::Cast(format!("invalid location URL: {e}")))?;
        let control_url = base
            .join(&relative)
            .map_err(|e| DlnaError::Cast(format!("joining control URL: {e}")))?
            .to_string();
        Ok(Self::new(control_url))
    }

    /// Absolute ConnectionManager control URL.
    pub fn control_url(&self) -> &str {
        &self.control_url
    }

    /// `GetProtocolInfo` — renderer sink `protocolInfo` entries.
    ///
    /// The `<Sink>` text is split on `,`, trimmed and empty entries dropped
    /// (same as Dart `_getSinkProtocolInfo`). Transport/parse errors surface as
    /// [`DlnaError::Cast`]; callers that want Dart's "empty list on failure"
    /// behaviour can use `unwrap_or_default()`.
    pub async fn get_sink_protocol_info(&self) -> DlnaResult<Vec<String>> {
        let body = soap_envelope(CONNECTION_MANAGER_SERVICE_TYPE, "GetProtocolInfo", &[]);
        let resp = post_soap(
            &self.client,
            &self.control_url,
            CONNECTION_MANAGER_SERVICE_TYPE,
            "GetProtocolInfo",
            &body,
        )
        .await?;
        let mut vals = extract_soap_values(&resp, &["Sink"]);
        Ok(split_sink_protocol_info(
            vals.remove("Sink").unwrap_or_default().as_str(),
        ))
    }

    /// Pick the `protocolInfo` string to embed in DIDL-Lite for `mime`.
    ///
    /// `_mime_ext` (uppercase file extension) is intentionally unused: the
    /// negotiation is driven purely by the sink list, matching the Dart
    /// implementation. It is kept in the signature for the FFI layer, which
    /// passes the extension through, and reserved for a future `DLNA.ORG_PN`
    /// mapping table.
    pub fn negotiate(&self, mime: &str, _mime_ext: &str, sink: &[String]) -> String {
        negotiate_protocol_info(mime, sink)
    }
}

/// Pure Dart-parity negotiation: exact content format, then wildcard, then
/// `http-get:*:<mime>:DLNA.ORG_OP=01`.
///
/// Exposed as a free function so callers without a ConnectionManager service
/// (or with an already-fetched sink list) can negotiate the same way.
pub fn negotiate_protocol_info(mime: &str, sink: &[String]) -> String {
    // 1. Exact contentFormat (third field) match — return the sink entry as-is.
    let exact = sink.iter().find(|p| {
        let parts: Vec<&str> = p.split(':').collect();
        parts.len() >= 3 && parts[2] == mime
    });
    if let Some(found) = exact {
        return found.clone();
    }

    // 2. Main-type wildcard (`video/*` or `*/*`) — rewrite contentFormat to the
    //    real MIME, keeping the sink's fourth field when present.
    let main_type = mime.split('/').next().unwrap_or(mime);
    let wildcard = sink.iter().find(|p| {
        let parts: Vec<&str> = p.split(':').collect();
        if parts.len() < 3 {
            return false;
        }
        parts[2] == format!("{main_type}/*") || parts[2] == "*/*"
    });
    if let Some(found) = wildcard {
        let base: Vec<&str> = found.split(':').collect();
        let fourth = if base.len() > 3 {
            base[3..].join(":")
        } else {
            "DLNA.ORG_OP=01".to_string()
        };
        return format!("{}:{}:{}:{}", base[0], base[1], mime, fourth);
    }

    // 3. No match — Dart's hard-coded basic format.
    format!("http-get:*:{mime}:DLNA.ORG_OP=01")
}

/// Split the raw `<Sink>` protocol list: comma-separated, trimmed, non-empty.
fn split_sink_protocol_info(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{spawn_fake_http, FakeResponse};

    const DESC_WITH_CM: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <device>
    <friendlyName>Mock TV</friendlyName>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>
        <controlURL>/ctl/ConnectionManager</controlURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
        <controlURL>/ctl/AVTransport</controlURL>
      </service>
    </serviceList>
  </device>
</root>"#;

    const DESC_WITHOUT_CM: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <device>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
        <controlURL>/ctl/AVTransport</controlURL>
      </service>
    </serviceList>
  </device>
</root>"#;

    const SINK_RESP: &str = r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body>
    <u:GetProtocolInfoResponse xmlns:u="urn:schemas-upnp-org:service:ConnectionManager:1">
      <Source></Source>
      <Sink>http-get:*:video/mp4:*,http-get:*:video/x-matroska:DLNA.ORG_PN=MKV;DLNA.ORG_OP=01;DLNA.ORG_CI=0, ,http-get:*:audio/mpeg:*</Sink>
    </u:GetProtocolInfoResponse>
  </s:Body>
</s:Envelope>"#;

    async fn renderer_for(server: &crate::test_support::FakeHttpServer) -> DlnaRenderer {
        DlnaRenderer {
            usn: "uuid:mock-tv".into(),
            location: server.url("/desc.xml"),
            server: Some("Mock".into()),
        }
    }

    #[tokio::test]
    async fn fetches_sink_protocol_info_over_http() {
        let server = spawn_fake_http(|req| {
            if req.starts_with("GET ") {
                FakeResponse::ok(DESC_WITH_CM)
            } else if req.contains("GetProtocolInfo") {
                FakeResponse::ok(SINK_RESP)
            } else {
                FakeResponse::not_found()
            }
        })
        .await;
        let renderer = renderer_for(&server).await;

        let client = ConnectionManagerClient::from_renderer(&renderer)
            .await
            .expect("ConnectionManager client");
        assert_eq!(client.control_url(), server.url("/ctl/ConnectionManager"));

        let sink = client.get_sink_protocol_info().await.expect("sink list");
        assert_eq!(
            sink,
            vec![
                "http-get:*:video/mp4:*",
                "http-get:*:video/x-matroska:DLNA.ORG_PN=MKV;DLNA.ORG_OP=01;DLNA.ORG_CI=0",
                "http-get:*:audio/mpeg:*",
            ]
        );

        // SOAP request shape: ConnectionManager namespace + action, quoted
        // SOAPACTION header and XML content type (Dart parity).
        let post = server
            .captured()
            .into_iter()
            .find(|r| r.starts_with("POST "))
            .expect("GetProtocolInfo POST");
        assert!(post.contains(
            "<u:GetProtocolInfo xmlns:u=\"urn:schemas-upnp-org:service:ConnectionManager:1\">"
        ));
        assert!(post
            .to_ascii_lowercase()
            .contains("soapaction: \"urn:schemas-upnp-org:service:connectionmanager:1#getprotocolinfo\""));
        assert!(post
            .to_ascii_lowercase()
            .contains("content-type: text/xml; charset=utf-8"));
    }

    #[tokio::test]
    async fn from_renderer_errors_without_connection_manager_service() {
        let server = spawn_fake_http(|req| {
            if req.starts_with("GET ") {
                FakeResponse::ok(DESC_WITHOUT_CM)
            } else {
                FakeResponse::not_found()
            }
        })
        .await;
        let renderer = renderer_for(&server).await;
        let err = ConnectionManagerClient::from_renderer(&renderer)
            .await
            .err()
            .expect("missing ConnectionManager must fail");
        assert!(matches!(err, DlnaError::Cast(_)));
    }

    #[tokio::test]
    async fn from_renderer_errors_on_bad_description_status() {
        let server = spawn_fake_http(|_| FakeResponse::not_found()).await;
        let renderer = renderer_for(&server).await;
        assert!(ConnectionManagerClient::from_renderer(&renderer).await.is_err());
    }

    // ── negotiate (Dart parity) ───────────────────────────────────────────────

    fn negotiate(mime: &str, sink: &[&str]) -> String {
        let client = ConnectionManagerClient::new("http://unused/ctl");
        let sink: Vec<String> = sink.iter().map(|s| s.to_string()).collect();
        client.negotiate(mime, "MKV", &sink)
    }

    #[test]
    fn negotiate_prefers_exact_content_format() {
        let found = negotiate(
            "video/x-matroska",
            &[
                "http-get:*:video/*:*",
                "http-get:*:video/x-matroska:DLNA.ORG_PN=MKV;DLNA.ORG_OP=01;DLNA.ORG_CI=0",
            ],
        );
        assert_eq!(
            found,
            "http-get:*:video/x-matroska:DLNA.ORG_PN=MKV;DLNA.ORG_OP=01;DLNA.ORG_CI=0"
        );
    }

    #[test]
    fn negotiate_rewrites_wildcard_with_sink_fourth_field() {
        let found = negotiate(
            "video/x-matroska",
            &["http-get:*:video/*:DLNA.ORG_PN=MPEG_PS_PAL"],
        );
        assert_eq!(
            found,
            "http-get:*:video/x-matroska:DLNA.ORG_PN=MPEG_PS_PAL"
        );
    }

    #[test]
    fn negotiate_wildcard_without_fourth_field_adds_op01() {
        let found = negotiate("video/x-matroska", &["http-get:*:video/*"]);
        assert_eq!(found, "http-get:*:video/x-matroska:DLNA.ORG_OP=01");
        let found = negotiate("image/jpeg", &["http-get:*:*/*"]);
        assert_eq!(found, "http-get:*:image/jpeg:DLNA.ORG_OP=01");
    }

    #[test]
    fn negotiate_falls_back_to_op01() {
        assert_eq!(
            negotiate("video/x-matroska", &[]),
            "http-get:*:video/x-matroska:DLNA.ORG_OP=01"
        );
        assert_eq!(
            negotiate(
                "video/x-matroska",
                &["http-get:*:video/mp4:*", "garbage", "http-get:*"]
            ),
            "http-get:*:video/x-matroska:DLNA.ORG_OP=01"
        );
    }

    #[test]
    fn negotiate_matches_main_type_wildcard() {
        let found = negotiate("video/mp4", &["http-get:*:video/*:DLNA.ORG_PN=MPEG_PS_PAL"]);
        assert_eq!(found, "http-get:*:video/mp4:DLNA.ORG_PN=MPEG_PS_PAL");
    }

    #[test]
    fn split_sink_trims_and_skips_empty_entries() {
        assert_eq!(
            split_sink_protocol_info(" a , ,b, "),
            vec!["a".to_string(), "b".to_string()]
        );
        assert!(split_sink_protocol_info("").is_empty());
    }
}
