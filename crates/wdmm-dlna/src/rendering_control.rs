//! UPnP RenderingControl:1 client — set/get volume and mute.
//!
//! Follows the same SOAP skeleton pattern as `avtransport.rs`.
//! Resolve the control URL from the device description with
//! [`resolve_rendering_control_url`] before constructing the client.

use quick_xml::events::Event;
use quick_xml::Reader;
use reqwest::Client;

use crate::{DlnaError, DlnaRenderer, DlnaResult};

const RENDERING_CONTROL_SERVICE_TYPE: &str =
    "urn:schemas-upnp-org:service:RenderingControl:1";
const SOAP_ENVELOPE_NS: &str = "http://schemas.xmlsoap.org/soap/envelope/";
const SOAP_ENCODING_NS: &str = "http://schemas.xmlsoap.org/soap/encoding/";

/// RenderingControl SOAP control client. One instance per renderer.
pub struct RenderingControlClient {
    client: Client,
    control_url: String,
}

impl RenderingControlClient {
    /// Create a new client for the given absolute `control_url`.
    pub fn new(control_url: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            control_url: control_url.into(),
        }
    }

    /// Set the master volume (0–100) on the given `instance_id` (typically 0).
    pub async fn set_volume(&self, instance_id: u32, volume: u32) -> DlnaResult<()> {
        let body = soap_action(
            "SetVolume",
            &[
                ("<InstanceID>", &instance_id.to_string()),
                ("<Channel>", "Master"),
                ("<DesiredVolume>", &volume.to_string()),
            ],
        );
        self.post_action("SetVolume", &body).await
    }

    /// Get the master volume (0–100).  Returns `None` if the renderer does
    /// not return a parseable value.
    pub async fn get_volume(&self, instance_id: u32) -> DlnaResult<Option<u32>> {
        let body = soap_action(
            "GetVolume",
            &[
                ("<InstanceID>", &instance_id.to_string()),
                ("<Channel>", "Master"),
            ],
        );
        let resp = self.post_action_with_response("GetVolume", &body).await?;
        let val = extract_value(&resp, "CurrentVolume");
        Ok(val.and_then(|s| s.parse().ok()))
    }

    /// Set mute (`true` = muted) on the master channel.
    pub async fn set_mute(&self, instance_id: u32, muted: bool) -> DlnaResult<()> {
        let body = soap_action(
            "SetMute",
            &[
                ("<InstanceID>", &instance_id.to_string()),
                ("<Channel>", "Master"),
                ("<DesiredMute>", if muted { "1" } else { "0" }),
            ],
        );
        self.post_action("SetMute", &body).await
    }

    /// Get the mute state of the master channel.
    pub async fn get_mute(&self, instance_id: u32) -> DlnaResult<Option<bool>> {
        let body = soap_action(
            "GetMute",
            &[
                ("<InstanceID>", &instance_id.to_string()),
                ("<Channel>", "Master"),
            ],
        );
        let resp = self.post_action_with_response("GetMute", &body).await?;
        let val = extract_value(&resp, "CurrentMute");
        Ok(val.map(|s| s.trim() == "1" || s.trim().eq_ignore_ascii_case("true")))
    }

    async fn post_action(&self, action: &str, body: &str) -> DlnaResult<()> {
        let soap_action_hdr =
            format!("\"{RENDERING_CONTROL_SERVICE_TYPE}#{action}\"");
        let resp = self
            .client
            .post(&self.control_url)
            .header("Content-Type", "text/xml; charset=utf-8")
            .header("SOAPACTION", &soap_action_hdr)
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

    async fn post_action_with_response(
        &self,
        action: &str,
        body: &str,
    ) -> DlnaResult<String> {
        let soap_action_hdr =
            format!("\"{RENDERING_CONTROL_SERVICE_TYPE}#{action}\"");
        let resp = self
            .client
            .post(&self.control_url)
            .header("Content-Type", "text/xml; charset=utf-8")
            .header("SOAPACTION", &soap_action_hdr)
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
        resp.text()
            .await
            .map_err(|e| DlnaError::Cast(format!("body read failed: {e}")))
    }
}

/// Build a SOAP envelope for a RenderingControl action.
fn soap_action(action: &str, args: &[(&str, &str)]) -> String {
    let mut s = String::new();
    s.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>");
    s.push_str(&format!(
        "<s:Envelope xmlns:s=\"{SOAP_ENVELOPE_NS}\" s:encodingStyle=\"{SOAP_ENCODING_NS}\">"
    ));
    s.push_str("<s:Body>");
    s.push_str(&format!(
        "<u:{action} xmlns:u=\"{RENDERING_CONTROL_SERVICE_TYPE}\">"
    ));
    for (tag, val) in args {
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

/// Extract the text content of the first element with the given local name.
fn extract_value(xml: &str, tag: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut in_tag = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                in_tag = e.name().local_name().as_ref() == tag.as_bytes();
            }
            Ok(Event::Text(t)) if in_tag => {
                return t.unescape().ok().map(|s| s.to_string());
            }
            Ok(Event::End(_)) => in_tag = false,
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    None
}

/// Fetch the device description at `renderer.location` and return the
/// RenderingControl service control URL (absolute).
pub async fn resolve_rendering_control_url(
    renderer: &DlnaRenderer,
) -> DlnaResult<String> {
    let xml = reqwest::get(&renderer.location)
        .await
        .map_err(|e| DlnaError::Cast(format!("fetch device description: {e}")))?
        .text()
        .await
        .map_err(|e| DlnaError::Cast(format!("read device description: {e}")))?;

    let rel = crate::parse_service_url(&xml, RENDERING_CONTROL_SERVICE_TYPE, "controlURL")
        .ok_or_else(|| {
            DlnaError::Cast("RenderingControl controlURL not found in device description".into())
        })?;

    let base = url::Url::parse(&renderer.location)
        .map_err(|e| DlnaError::Cast(format!("invalid location URL: {e}")))?;
    base.join(&rel)
        .map(|u| u.to_string())
        .map_err(|e| DlnaError::Cast(format!("joining control URL: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soap_envelope_set_volume() {
        let body = soap_action("SetVolume", &[
            ("<InstanceID>", "0"),
            ("<Channel>", "Master"),
            ("<DesiredVolume>", "75"),
        ]);
        assert!(body.contains("<u:SetVolume xmlns:u=\"urn:schemas-upnp-org:service:RenderingControl:1\">"));
        assert!(body.contains("<InstanceID>0</InstanceID>"));
        assert!(body.contains("<DesiredVolume>75</DesiredVolume>"));
    }

    #[test]
    fn extract_volume_value() {
        let xml = r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body>
    <u:GetVolumeResponse xmlns:u="urn:schemas-upnp-org:service:RenderingControl:1">
      <CurrentVolume>42</CurrentVolume>
    </u:GetVolumeResponse>
  </s:Body>
</s:Envelope>"#;
        assert_eq!(extract_value(xml, "CurrentVolume").as_deref(), Some("42"));
    }

    #[test]
    fn extract_mute_value_true() {
        let xml = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:GetMuteResponse xmlns:u="urn:schemas-upnp-org:service:RenderingControl:1"><CurrentMute>1</CurrentMute></u:GetMuteResponse></s:Body></s:Envelope>"#;
        assert_eq!(extract_value(xml, "CurrentMute").as_deref(), Some("1"));
    }

    #[test]
    fn extract_value_missing_returns_none() {
        let xml = r#"<root><Other>hi</Other></root>"#;
        assert!(extract_value(xml, "CurrentVolume").is_none());
    }
}
