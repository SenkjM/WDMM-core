//! DIDL-Lite metadata builder.
//!
//! Mirrors the Dart `_didlLite` / `_xmlEscape` implementation used by
//! `lib/services/dlna_cast_service.dart`: the produced string is passed as
//! `CurrentURIMetaData` to `SetAVTransportURI` (SOAP-escaped once more by the
//! AVTransport envelope builder).
//!
//! The subtitle resource convention matches the legacy Dart path: a second
//! `<res>` element whose `protocolInfo` is [`SUBTITLE_PROTOCOL_INFO`] lets
//! DLNA renderers with caption support pick up sidecar subtitles.

/// `protocolInfo` used for the subtitle (sidecar) `<res>` element.
pub const SUBTITLE_PROTOCOL_INFO: &str = "http-get:*:text/plain:DLNA.ORG_PN=*";

/// One media resource of a DIDL-Lite item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DidlRes {
    /// Absolute URL the renderer should fetch.
    pub url: String,
    /// UPnP `protocolInfo` four-tuple (from
    /// [`crate::ConnectionManagerClient::negotiate`]).
    pub protocol_info: String,
}

impl DidlRes {
    /// Convenience constructor.
    pub fn new(url: impl Into<String>, protocol_info: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            protocol_info: protocol_info.into(),
        }
    }
}

/// Build a DIDL-Lite document with a title and one `<res>` per resource.
///
/// The first resource is the media stream; a second resource should use
/// [`SUBTITLE_PROTOCOL_INFO`] (see [`DidlRes`]). Output is byte-identical to
/// the Dart `_didlLite` for the same inputs (no XML declaration, no
/// whitespace).
pub fn build_didl_lite(title: &str, resources: &[DidlRes]) -> String {
    let mut s = String::new();
    s.push_str(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
         xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
         xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">",
    );
    s.push_str("<item id=\"0\" parentID=\"-1\" restricted=\"1\">");
    s.push_str(&format!("<dc:title>{}</dc:title>", xml_escape(title)));
    s.push_str("<upnp:class>object.item.videoItem</upnp:class>");
    for res in resources {
        s.push_str(&format!(
            "<res protocolInfo=\"{}\">{}</res>",
            xml_escape(&res.protocol_info),
            xml_escape(&res.url)
        ));
    }
    s.push_str("</item>");
    s.push_str("</DIDL-Lite>");
    s
}

/// Escape `&`, `<`, `>`, `"`, `'` in that order (`&` first, so already-escaped
/// sequences are not double-escaped by later replacements).
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROTO: &str = "http-get:*:video/x-matroska:DLNA.ORG_PN=MKV;DLNA.ORG_OP=01;DLNA.ORG_CI=0";

    #[test]
    fn builds_single_resource_didl() {
        let didl = build_didl_lite(
            "Movie",
            &[DidlRes::new("http://media/movie.mkv", PROTO)],
        );
        assert_eq!(
            didl,
            "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
             xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
             xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">\
             <item id=\"0\" parentID=\"-1\" restricted=\"1\">\
             <dc:title>Movie</dc:title>\
             <upnp:class>object.item.videoItem</upnp:class>\
             <res protocolInfo=\"http-get:*:video/x-matroska:DLNA.ORG_PN=MKV;DLNA.ORG_OP=01;DLNA.ORG_CI=0\">\
             http://media/movie.mkv</res>\
             </item></DIDL-Lite>"
        );
    }

    #[test]
    fn escapes_title_and_url() {
        let didl = build_didl_lite(
            "Tom & Jerry <\"quoted\">",
            &[DidlRes::new("http://media/a?b=1&c=\"2\"", PROTO)],
        );
        assert!(didl.contains("<dc:title>Tom &amp; Jerry &lt;&quot;quoted&quot;&gt;</dc:title>"));
        assert!(didl.contains("http://media/a?b=1&amp;c=&quot;2&quot;"));
        assert!(!didl.contains("Tom & Jerry"));
    }

    #[test]
    fn builds_single_res_only() {
        let didl = build_didl_lite("T", &[DidlRes::new("http://m/a.mkv", PROTO)]);
        assert_eq!(didl.matches("<res ").count(), 1);
    }

    #[test]
    fn builds_dual_res_with_subtitle_protocol() {
        let didl = build_didl_lite(
            "Episode 1",
            &[
                DidlRes::new("http://media/ep1.mkv", PROTO),
                DidlRes::new("http://media/ep1.srt", SUBTITLE_PROTOCOL_INFO),
            ],
        );
        assert_eq!(didl.matches("<res ").count(), 2);
        assert!(didl.contains(&format!(
            "<res protocolInfo=\"{SUBTITLE_PROTOCOL_INFO}\">http://media/ep1.srt</res>"
        )));
    }

    #[test]
    fn escapes_protocol_info_special_chars() {
        let didl = build_didl_lite(
            "X",
            &[DidlRes::new("http://m/a", "http-get:*:video/mp4:DLNA.ORG_PN=A&B")],
        );
        assert!(didl.contains("DLNA.ORG_PN=A&amp;B"));
    }
}
