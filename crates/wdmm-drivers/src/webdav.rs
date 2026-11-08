//! WebDAV driver — working `list()` referencing WDMM webdav_source.dart.

use async_trait::async_trait;
use base64::prelude::*;
use chrono::{DateTime, Utc};
use quick_xml::events::Event;
use quick_xml::Reader;
use reqwest::Client;
use url::Url;

use crate::driver::{WdmmVfs, CloudFileItem, DriverError, Result};

pub struct WebDavDriver {
    base_url: Url,
    client: Client,
    /// Basic auth header value (`"Basic <base64>"`) for items' `raw_headers`.
    /// Empty string when no credentials were provided.
    auth_header: String,
}

impl WebDavDriver {
    pub fn new(url: &str, username: &str, password: &str) -> Result<Self> {
        // Ensure the base URL ends with '/' so that Url::join treats every
        // relative segment as a child rather than replacing the last path
        // component.  e.g. "https://host/dav" → "https://host/dav/" so that
        // join("Music/sub") produces "https://host/dav/Music/sub" instead of
        // the incorrect "https://host/Music/sub".
        let normalised = if url.ends_with('/') {
            url.to_string()
        } else {
            format!("{}/", url)
        };
        let base = Url::parse(&normalised).map_err(|e| DriverError::Config(e.to_string()))?;

        let auth_header;
        let mut client_builder = Client::builder();
        if !username.is_empty() || !password.is_empty() {
            // Basic auth injected into default headers (reqwest pattern)
            let auth = format!("{}:{}", username, password);
            let encoded = base64::prelude::BASE64_STANDARD.encode(auth.as_bytes());
            let header_value = format!("Basic {}", encoded);
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::AUTHORIZATION,
                header_value.parse().unwrap(),
            );
            client_builder = client_builder.default_headers(headers);
            auth_header = header_value;
        } else {
            auth_header = String::new();
        }

        Ok(Self {
            base_url: base,
            client: client_builder.build().map_err(|e| DriverError::Config(e.to_string()))?,
            auth_header,
        })
    }

    fn join_path(&self, path: &str) -> String {
        // If `path` already starts with the base URL's path prefix (i.e. the
        // caller passed an absolute server path that includes the base path),
        // strip that prefix first so we never double-append it.
        //
        // Example: base = "http://host/dav/", path = "/dav/Local"
        //   → strip "/dav" prefix → relative = "Local"
        //   → base.join("Local") = "http://host/dav/Local"  ✓
        //
        // Example: base = "http://host/dav/", path = "/Local"
        //   → no prefix match → relative = "Local"
        //   → base.join("Local") = "http://host/dav/Local"  ✓
        //
        // Example: base = "http://host/", path = "/Local"
        //   → no prefix (base path is "/") → relative = "Local"
        //   → base.join("Local") = "http://host/Local"  ✓
        let base_path = self.base_url.path(); // e.g. "/dav/"
        // Canonical prefix to strip: base path without trailing slash.
        let prefix = base_path.trim_end_matches('/'); // e.g. "/dav"

        let relative = if !prefix.is_empty() && path.starts_with(prefix) {
            // Strip prefix, then remove any leading slash so Url::join treats
            // it as a relative path (not an absolute one that replaces the path).
            path[prefix.len()..].trim_start_matches('/')
        } else {
            path.trim_start_matches('/')
        };

        self.base_url.join(relative).unwrap().to_string()
    }

    /// Convert a server-supplied absolute href (e.g. `/dav/Local/file.mp3`)
    /// into a full URL by reusing the base URL's scheme + host, **not** by
    /// appending to the base path.  This avoids the double-segment bug where
    /// `join_path("/dav/Local/file.mp3")` would produce `.../dav/dav/Local/…`.
    fn href_to_url(&self, href: &str) -> String {
        // If the server already returned a full URL (some proxy servers do),
        // use it directly.
        if href.starts_with("http://") || href.starts_with("https://") {
            return href.to_string();
        }
        // Otherwise reconstruct: scheme://host[:port] + absolute_path
        let origin = format!(
            "{}://{}",
            self.base_url.scheme(),
            self.base_url.host_str().unwrap_or("")
        );
        let origin = if let Some(port) = self.base_url.port() {
            format!("{}:{}", origin, port)
        } else {
            origin
        };
        format!("{}{}", origin, href)
    }

    /// Parse PROPFIND XML and extract items.
    /// `propfind_url` is the full URL that was requested (for filtering the collection itself).
    fn parse_propfind_response(
        &self,
        xml: &str,
        propfind_url: &str,
        include_self: bool,
    ) -> Result<Vec<CloudFileItem>> {
        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(true);

        // Extract the path from the PROPFIND URL for filtering.
        // Decode percent-encoding so it can be compared with decoded_href below
        // (e.g. "/dav/My%20Music" == "/dav/My Music" after decoding both sides).
        let propfind_path = Url::parse(propfind_url)
            .ok()
            .and_then(|u| u.path().strip_prefix('/').map(|s| format!("/{}", s)))
            .unwrap_or_else(|| propfind_url.to_string());
        let propfind_path = urlencoding::decode(&propfind_path)
            .unwrap_or_else(|_| std::borrow::Cow::Borrowed(&propfind_path))
            .into_owned();

        let mut items = Vec::new();
        let mut current_href: Option<String> = None;
        let mut current_is_dir = false;
        let mut current_size = 0u64;
        let mut current_modified: Option<DateTime<Utc>> = None;

        let mut in_response = false;
        let mut buf = Vec::new();

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(e)) => match e.name().as_ref() {
                    b"d:response" | b"D:response" | b"response" => {
                        in_response = true;
                        current_href = None;
                        current_is_dir = false;
                        current_size = 0;
                        current_modified = None;
                    }
                    b"d:href" | b"D:href" | b"href" if in_response => {
                        if let Ok(Event::Text(t)) = reader.read_event_into(&mut buf) {
                            current_href = t.unescape().ok().map(|s| s.to_string());
                        }
                    }
                    b"d:collection" | b"D:collection" | b"collection" => {
                        current_is_dir = true;
                    }
                    b"d:getcontentlength" | b"D:getcontentlength" | b"getcontentlength" if in_response => {
                        if let Ok(Event::Text(t)) = reader.read_event_into(&mut buf) {
                            if let Ok(s) = t.unescape() {
                                current_size = s.parse().unwrap_or(0);
                            }
                        }
                    }
                    b"d:getlastmodified" | b"D:getlastmodified" | b"getlastmodified" if in_response => {
                        if let Ok(Event::Text(t)) = reader.read_event_into(&mut buf) {
                            if let Ok(s) = t.unescape() {
                                current_modified = DateTime::parse_from_rfc2822(&s)
                                    .ok()
                                    .map(|dt| dt.with_timezone(&Utc));
                            }
                        }
                    }
                    _ => {}
                },
                // Self-closing elements (e.g. `<D:collection/>`, `<D:resourcetype/>`)
                // arrive as Empty events — handle the directory marker here.
                Ok(Event::Empty(e)) => match e.name().as_ref() {
                    b"d:collection" | b"D:collection" | b"collection" if in_response => {
                        current_is_dir = true;
                    }
                    _ => {}
                },
                Ok(Event::End(e)) => {
                    if matches!(e.name().as_ref(), b"d:response" | b"D:response" | b"response") {
                        in_response = false;
                        if let Some(href) = current_href.take() {
                            let decoded_href = urlencoding::decode(&href).unwrap_or_default().to_string();
                            let name = decoded_href.trim_end_matches('/').split('/').next_back().unwrap_or("").to_string();

                            // skip the collection itself (href matches propfind_path)
                            // unless the caller asked for a depth-0 stat of this path.
                            if !name.is_empty() && (include_self
                                || decoded_href.trim_end_matches('/') != propfind_path.trim_end_matches('/'))
                            {
                                // B1 step 4: carry the Basic auth header on each
                                // item's raw_headers so downstream players/DLNA/B4.5
                                // direct-mode can fetch the URL without a second
                                // auth store. Empty when the server is anonymous.
                                let raw_headers = if self.auth_header.is_empty() {
                                    Vec::new()
                                } else {
                                    vec![("Authorization".to_string(), self.auth_header.clone())]
                                };
                                items.push(CloudFileItem {
                                    name,
                                    is_dir: current_is_dir,
                                    size: current_size,
                                    modified: current_modified,
                                    raw_url: Some(self.href_to_url(&decoded_href)),
                                    raw_headers,
                                    delivery: crate::driver::ContentDeliveryKind::NetworkDirect,
                                    local_path: None,
                                });
                            }
                        }
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => {
                    return Err(DriverError::Other(format!("XML parse error: {}", e)));
                }
                _ => {}
            }
            buf.clear();
        }

        Ok(items)
    }

    /// Depth-0 PROPFIND stat of a single path — returns the resource itself.
    /// More robust than listing the parent and finding by name: works at the
    /// server root and avoids an extra round-trip. B1 step 4.
    async fn stat(&self, path: &str) -> Result<CloudFileItem> {
        let url = self.join_path(path);

        let propfind_body = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:">
  <d:prop>
    <d:resourcetype/>
    <d:getcontentlength/>
    <d:getlastmodified/>
  </d:prop>
</d:propfind>"#;

        let resp = self
            .client
            .request(reqwest::Method::from_bytes(b"PROPFIND").unwrap(), &url)
            .header("Depth", "0")
            .header("Content-Type", "application/xml; charset=utf-8")
            .body(propfind_body)
            .send()
            .await?;

        if !resp.status().is_success() {
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                return Err(DriverError::NotFound(path.to_string()));
            }
            return Err(DriverError::Network(format!("PROPFIND depth-0 failed: {}", resp.status())));
        }

        let body = resp.text().await?;
        let items = self.parse_propfind_response(&body, &url, true)?;
        // depth-0 returns exactly one response (the resource itself).
        items
            .into_iter()
            .next()
            .ok_or_else(|| DriverError::NotFound(path.to_string()))
    }
}

#[async_trait]
impl WdmmVfs for WebDavDriver {
    async fn list(&self, path: &str) -> Result<Vec<CloudFileItem>> {
        let url = self.join_path(path);

        let propfind_body = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:">
  <d:prop>
    <d:resourcetype/>
    <d:getcontentlength/>
    <d:getlastmodified/>
  </d:prop>
</d:propfind>"#;

        let resp = self
            .client
            .request(reqwest::Method::from_bytes(b"PROPFIND").unwrap(), &url)
            .header("Depth", "1")
            .header("Content-Type", "application/xml; charset=utf-8")
            .body(propfind_body)
            .send()
            .await?;

        if !resp.status().is_success() {
            // A missing collection has to look like a missing resource: the
            // sync protocol treats `NotFound` as "empty directory" and would
            // otherwise abort a first sync to a fresh remote root.
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                return Err(DriverError::NotFound(path.to_string()));
            }
            return Err(DriverError::Network(format!("PROPFIND failed: {}", resp.status())));
        }

        let body = resp.text().await?;
        self.parse_propfind_response(&body, &url, false)
    }

    /// Depth-0 PROPFIND stat of a single path — returns the resource itself
    /// (the collection href). More robust than listing the parent and finding
    /// by name: works at the server root and avoids an extra round-trip to
    /// resolve "..". B1 step 4.
    async fn get(&self, path: &str) -> Result<CloudFileItem> {
        // depth-0 PROPFIND. Replaces the previous "list the parent and find by
        // name" approach, which failed at the server root and cost an extra
        // request. The actual PROPFIND lives in `impl WebDavDriver::stat`.
        self.stat(path).await
    }

    async fn mkdir(&self, path: &str) -> Result<()> {
        let url = self.join_path(path);
        let resp = self.client.request(reqwest::Method::from_bytes(b"MKCOL").unwrap(), &url).send().await?;
        if resp.status().is_success() || resp.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            Ok(())
        } else {
            Err(DriverError::Network(format!("MKCOL failed: {}", resp.status())))
        }
    }

    async fn rename(&self, old: &str, new_path: &str) -> Result<()> {
        let old_url = self.join_path(old);
        let new_url = self.join_path(new_path);
        let resp = self
            .client
            .request(reqwest::Method::from_bytes(b"MOVE").unwrap(), &old_url)
            .header("Destination", &new_url)
            .header("Overwrite", "T")
            .send()
            .await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(DriverError::Network(format!("MOVE failed: {}", resp.status())))
        }
    }

    async fn remove(&self, path: &str) -> Result<()> {
        let url = self.join_path(path);
        let resp = self.client.delete(&url).send().await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(DriverError::Network(format!("DELETE failed: {}", resp.status())))
        }
    }

    async fn put_content(&self, path: &str, data: &[u8]) -> Result<()> {
        let url = self.join_path(path);
        let resp = self
            .client
            .put(&url)
            .header("Content-Type", "application/octet-stream")
            .body(data.to_vec())
            .send()
            .await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(DriverError::Network(format!("PUT failed: {}", resp.status())))
        }
    }

    async fn get_content(&self, path: &str) -> Result<Vec<u8>> {
        let url = self.join_path(path);
        let resp = self.client.get(&url).send().await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(DriverError::NotFound(path.to_string()));
        }
        if !resp.status().is_success() {
            return Err(DriverError::Network(format!(
                "GET failed: {}",
                resp.status()
            )));
        }
        Ok(resp.bytes().await?.to_vec())
    }

    async fn get_content_range(&self, path: &str, start: u64, end: Option<u64>) -> Result<Vec<u8>> {
        let url = self.join_path(path);
        let range_value = match end {
            Some(e) => format!("bytes={}-{}", start, e),
            None => format!("bytes={}-", start),
        };
        let resp = self
            .client
            .get(&url)
            .header(reqwest::header::RANGE, &range_value)
            .send()
            .await?;
        // 206 Partial Content or 200 OK (server that ignores Range)
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(DriverError::NotFound(path.to_string()));
        }
        if !resp.status().is_success() {
            return Err(DriverError::Network(format!(
                "GET range failed: {}",
                resp.status()
            )));
        }
        Ok(resp.bytes().await?.to_vec())
    }

    async fn copy(&self, src: &str, dst: &str) -> Result<()> {
        let src_url = self.join_path(src);
        let dst_url = self.join_path(dst);
        let resp = self
            .client
            .request(reqwest::Method::from_bytes(b"COPY").unwrap(), &src_url)
            .header("Destination", &dst_url)
            .header("Overwrite", "T")
            .send()
            .await?;
        // 201 Created (new resource) or 204 No Content (overwrote existing) are success.
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(DriverError::Network(format!("COPY failed: {}", resp.status())))
        }
    }

    async fn ensure_dir(&self, path: &str) -> Result<()> {
        let url = self.join_path(path);
        let resp = self
            .client
            .request(reqwest::Method::from_bytes(b"MKCOL").unwrap(), &url)
            .send()
            .await?;
        // 201 Created, 405 Method Not Allowed (collection already exists),
        // and 409 Conflict (intermediate created on some servers) all mean the
        // directory is effectively present.
        if resp.status().is_success()
            || resp.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED
            || resp.status() == reqwest::StatusCode::CONFLICT
        {
            Ok(())
        } else {
            Err(DriverError::Network(format!("MKCOL failed: {}", resp.status())))
        }
    }

    async fn status_tags(&self) -> Result<Vec<crate::driver::StatusTag>> {
        // WebDAV exposes no account-status API.
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_joining() {
        let driver = WebDavDriver::new("https://example.com/dav/", "", "").unwrap();
        assert_eq!(driver.join_path("/Music/track.mp3"), "https://example.com/dav/Music/track.mp3");
    }

    /// Base URL without trailing slash must not drop the last path component.
    /// Regression test for: Url::join("Music/sub") on "https://host/dav" (no
    /// trailing slash) was producing "https://host/Music/sub" instead of
    /// "https://host/dav/Music/sub".
    #[test]
    fn url_joining_no_trailing_slash() {
        let driver = WebDavDriver::new("https://example.com/dav", "", "").unwrap();
        // Single segment
        assert_eq!(
            driver.join_path("/Music"),
            "https://example.com/dav/Music",
            "single segment must preserve base path"
        );
        // Multi-segment subdirectory
        assert_eq!(
            driver.join_path("/Music/sub"),
            "https://example.com/dav/Music/sub",
            "multi-segment must preserve base path"
        );
    }

    /// Nextcloud-style base URL (no trailing slash on deep path).
    #[test]
    fn url_joining_nextcloud_style() {
        let driver = WebDavDriver::new("https://cloud.example.com/remote.php/webdav", "", "").unwrap();
        assert_eq!(
            driver.join_path("/Photos/holiday"),
            "https://cloud.example.com/remote.php/webdav/Photos/holiday"
        );
    }

    /// join_path must be idempotent: if the caller passes an absolute path
    /// that already includes the base URL's path prefix, the prefix must be
    /// stripped to avoid double-appending.
    ///
    /// Regression test for: Dart's `joinRemotePath` produces "/dav/Local"
    /// (account.remotePath="/dav" + path="/Local"), but Rust's WebDavDriver
    /// already has base_url="http://host/dav/".  Without prefix stripping,
    /// `join_path("/dav/Local")` produced "http://host/dav/dav/Local".
    #[test]
    fn url_joining_strips_base_path_prefix() {
        // With trailing slash on base
        let driver = WebDavDriver::new("https://example.com/dav/", "", "").unwrap();
        assert_eq!(
            driver.join_path("/dav/Local"),
            "https://example.com/dav/Local",
            "absolute path with base prefix must be idempotent"
        );
        assert_eq!(
            driver.join_path("/dav/Local/Music"),
            "https://example.com/dav/Local/Music",
            "deep absolute path with base prefix must be idempotent"
        );
        // Without trailing slash on base
        let driver2 = WebDavDriver::new("https://example.com/dav", "", "").unwrap();
        assert_eq!(
            driver2.join_path("/dav/Local"),
            "https://example.com/dav/Local",
            "idempotent even when base has no trailing slash"
        );
    }

    /// Idempotent join must also work for Nextcloud-style deep base paths.
    #[test]
    fn url_joining_nextcloud_idempotent() {
        let driver = WebDavDriver::new("https://cloud.example.com/remote.php/webdav", "", "").unwrap();
        assert_eq!(
            driver.join_path("/remote.php/webdav/Photos"),
            "https://cloud.example.com/remote.php/webdav/Photos",
            "nextcloud deep path must be idempotent"
        );
    }

    /// Root base URL (path = "/") must not over-strip.
    #[test]
    fn url_joining_root_base_no_strip() {
        let driver = WebDavDriver::new("https://example.com/", "", "").unwrap();
        assert_eq!(
            driver.join_path("/Local"),
            "https://example.com/Local",
            "root base must not strip anything"
        );
        assert_eq!(
            driver.join_path("/dav/Local"),
            "https://example.com/dav/Local",
            "root base preserves full path"
        );
    }

    // ── Fixture-based PROPFIND parser tests ─────────────────────────────────────
    //
    // These exercise parse_propfind_response against a realistic WebDAV
    // PROPFIND multistatus body. No network needed — the fixture is inlined.

    /// A realistic PROPFIND multistatus response for `/dav/Music` containing:
    /// - the collection itself (must be filtered out)
    /// - one file (song.mp3)
    /// - one subdirectory (Album/)
    const PROPFIND_FIXTURE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>/dav/Music</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype><D:collection/></D:resourcetype>
        <D:getlastmodified>Wed, 21 Oct 2026 10:00:00 GMT</D:getlastmodified>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/dav/Music/song.mp3</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype/>
        <D:getcontentlength>12345</D:getcontentlength>
        <D:getlastmodified>Wed, 21 Oct 2026 09:00:00 GMT</D:getlastmodified>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/dav/Music/Album/</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype><D:collection/></D:resourcetype>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
</D:multistatus>"#;

    #[test]
    fn parse_propfind_extracts_file_and_dir() {
        let driver = WebDavDriver::new("https://example.com/dav/", "", "").unwrap();
        // req_path matches the collection href so the collection itself is filtered.
        let items = driver
            .parse_propfind_response(PROPFIND_FIXTURE, "/dav/Music", false)
            .unwrap();

        // Collection itself must be filtered out; 2 real children remain.
        assert_eq!(items.len(), 2);

        let by_name: std::collections::HashMap<&str, &CloudFileItem> =
            items.iter().map(|i| (i.name.as_str(), i)).collect();

        let song = by_name.get("song.mp3").expect("song.mp3 present");
        assert!(!song.is_dir);
        assert_eq!(song.size, 12345);
        assert!(song.modified.is_some());
        assert!(song.raw_url.as_ref().unwrap().ends_with("/dav/Music/song.mp3"));

        let album = by_name.get("Album").expect("Album dir present");
        assert!(album.is_dir);
        assert_eq!(album.size, 0);
    }

    #[test]
    fn parse_propfind_unprefixed_namespace() {
        // Some servers omit the namespace prefix entirely.
        const UNPREFIXED: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<multistatus xmlns="DAV:">
  <response>
    <href>/dav/file.txt</href>
    <propstat>
      <prop>
        <resourcetype/>
        <getcontentlength>42</getcontentlength>
      </prop>
      <status>HTTP/1.1 200 OK</status>
    </propstat>
  </response>
</multistatus>"#;
        let driver = WebDavDriver::new("https://example.com/dav/", "", "").unwrap();
        let items = driver.parse_propfind_response(UNPREFIXED, "/dav", false).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "file.txt");
        assert_eq!(items[0].size, 42);
        assert!(!items[0].is_dir);
    }

    #[test]
    fn parse_propfind_empty_response() {
        let driver = WebDavDriver::new("https://example.com/dav/", "", "").unwrap();
        const EMPTY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:"></D:multistatus>"#;
        let items = driver.parse_propfind_response(EMPTY, "/dav", false).unwrap();
        assert!(items.is_empty());
    }

    /// Regression: folder names with spaces caused the parent-directory filter to
    /// miss the collection entry because `propfind_path` was percent-encoded
    /// ("/dav/My%20Music") while `decoded_href` was plain text ("/dav/My Music").
    /// After the fix both sides are decoded before comparison.
    #[test]
    fn parse_propfind_space_in_folder_name_filters_collection() {
        const SPACE_FIXTURE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>/dav/My%20Music</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype><D:collection/></D:resourcetype>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/dav/My%20Music/song.mp3</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype/>
        <D:getcontentlength>9999</D:getcontentlength>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
</D:multistatus>"#;
        let driver = WebDavDriver::new("https://example.com/dav/", "", "").unwrap();
        // The PROPFIND was issued against the percent-encoded URL.
        let items = driver
            .parse_propfind_response(
                SPACE_FIXTURE,
                "https://example.com/dav/My%20Music",
                false,
            )
            .unwrap();
        // The collection itself (/dav/My%20Music) must be filtered out.
        assert_eq!(items.len(), 1, "collection with space in name must be filtered out");
        assert_eq!(items[0].name, "song.mp3");
    }

    // ── Async HTTP integration test (mock server, no external deps) ────────────
    //
    // Spins up a raw TCP listener that speaks just enough HTTP/1.1 to answer a
    // single PROPFIND, then points a real WebDavDriver at it. This exercises the
    // full list() round-trip including request headers and response parsing.

    async fn start_mock_dav_server() -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let body = PROPFIND_FIXTURE.to_string();
        tokio::spawn(async move {
            // Accept a single connection; respond to its PROPFIND and close.
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 8192];
                // Read the full request (headers + small PROPFIND body). The body
                // is <300 bytes so one read captures everything.
                let _ = sock.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 207 Multi-Status\r\n\
                     Content-Type: application/xml; charset=utf-8\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\
                     \r\n\
                     {}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        port
    }

    #[tokio::test]
    async fn list_over_mock_http() {
        let port = start_mock_dav_server().await;
        // Brief yield so the spawned task can install the listener.
        tokio::task::yield_now().await;

        // Base URL with the same path namespace the fixture uses.
        let base = format!("http://127.0.0.1:{}/dav/", port);
        let driver = WebDavDriver::new(&base, "", "").unwrap();

        // list("/Music") → PROPFIND to /dav/Music → 2 children (collection filtered).
        let items = driver.list("/Music").await.expect("list should succeed");
        assert_eq!(items.len(), 2, "expected file + dir, got {:#?}", items);

        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
        assert!(names.contains(&"song.mp3"), "names: {:?}", names);
        assert!(names.contains(&"Album"), "names: {:?}", names);

        let song = items.iter().find(|i| i.name == "song.mp3").unwrap();
        assert_eq!(song.size, 12345);
        assert!(!song.is_dir);
        assert!(song.raw_url.as_ref().unwrap().contains("/dav/Music/song.mp3"));
    }

    // ── put_content / get_content round-trip over mock HTTP ───────────────────
    //
    // Spins up a two-request mock: first PUT → 201, then GET → 200 + body.
    // Verifies both new WdmmVfs methods work end-to-end.

    async fn start_mock_put_get_server(payload: Vec<u8>) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            // Serve exactly two requests (PUT then GET) then exit.
            for _ in 0..2u8 {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let is_put = req.starts_with("PUT ");
                let resp = if is_put {
                    "HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                } else {
                    // GET — reply with the stored payload
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        payload.len()
                    )
                };
                let _ = sock.write_all(resp.as_bytes()).await;
                if !is_put {
                    let _ = sock.write_all(&payload).await;
                }
                let _ = sock.flush().await;
            }
        });
        port
    }

    #[tokio::test]
    async fn put_content_and_get_content_over_mock_http() {
        let data = b"hello from wdmm sync".to_vec();
        let port = start_mock_put_get_server(data.clone()).await;
        tokio::task::yield_now().await;

        let base = format!("http://127.0.0.1:{}/dav/", port);
        let driver = WebDavDriver::new(&base, "", "").unwrap();

        // PUT → should succeed (201)
        driver
            .put_content("/sync/meta.json", &data)
            .await
            .expect("put_content should succeed");

        // GET → should return the same bytes
        let fetched = driver
            .get_content("/sync/meta.json")
            .await
            .expect("get_content should succeed");
        assert_eq!(fetched, data);
    }

    // ── rename via MOVE mock ──────────────────────────────────────────────────

    #[tokio::test]
    async fn rename_via_move_over_mock_http() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                assert!(req.starts_with("MOVE "), "expected MOVE, got: {req}");
                assert!(
                    req.to_ascii_lowercase().contains("destination:"),
                    "missing Destination header"
                );
                let _ = sock
                    .write_all(b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
            }
        });
        tokio::task::yield_now().await;

        let base = format!("http://127.0.0.1:{}/dav/", port);
        let driver = WebDavDriver::new(&base, "", "").unwrap();
        driver
            .rename("/old.mp3", "/new.mp3")
            .await
            .expect("rename should succeed");
    }

    // ── Live integration tests against a configurable WebDAV server ──────────
    //
    // Credentials and endpoint come from environment variables — never hardcoded:
    //
    //   WDMM_TEST_WEBDAV_URL       base URL without trailing slash, e.g.
    //                              "http://192.168.0.107:5244/dav"
    //   WDMM_TEST_WEBDAV_USER      username
    //   WDMM_TEST_WEBDAV_PASS      password
    //   WDMM_TEST_WEBDAV_SUBDIR    a directory that exists under the root, e.g. "Local"
    //
    // Tests are skipped when any variable is absent (env var not set = no live
    // server available).  They are also marked #[ignore] so they never run in
    // CI or plain `cargo test`; use `cargo test -- --ignored` to opt in.
    //
    // These tests verify the no-trailing-slash URL normalisation fix: a base URL
    // without a trailing slash must produce correct sub-path URLs so that
    // PROPFIND on "/subdir" hits <base>/subdir, not /<subdir>.

    /// Returns (url, user, pass, subdir) from env, or None to skip.
    fn live_webdav_config() -> Option<(String, String, String, String)> {
        let url = std::env::var("WDMM_TEST_WEBDAV_URL").ok()?;
        let user = std::env::var("WDMM_TEST_WEBDAV_USER").unwrap_or_default();
        let pass = std::env::var("WDMM_TEST_WEBDAV_PASS").unwrap_or_default();
        let sub = std::env::var("WDMM_TEST_WEBDAV_SUBDIR").unwrap_or_else(|_| "Local".into());
        Some((url, user, pass, sub))
    }

    #[tokio::test]
    #[ignore = "requires WDMM_TEST_WEBDAV_URL env var (run with --ignored)"]
    async fn live_list_root_no_trailing_slash() {
        let Some((url, user, pass, _)) = live_webdav_config() else {
            eprintln!("WDMM_TEST_WEBDAV_URL not set — skipping");
            return;
        };
        // Base URL intentionally omits trailing slash (the regression scenario).
        let base = url.trim_end_matches('/').to_string();
        let driver = WebDavDriver::new(&base, &user, &pass).unwrap();

        let items = driver.list("/").await.expect("list root should succeed");
        assert!(!items.is_empty(), "root listing must not be empty");

        // Every raw_url must start with the correct base prefix.
        let expected_prefix = format!("{}/", base);
        for item in &items {
            if let Some(raw) = &item.raw_url {
                assert!(
                    raw.starts_with(&expected_prefix),
                    "raw_url has wrong base (base path was dropped): {raw}\nexpected prefix: {expected_prefix}"
                );
            }
        }
    }

    #[tokio::test]
    #[ignore = "requires WDMM_TEST_WEBDAV_URL + WDMM_TEST_WEBDAV_SUBDIR env vars (run with --ignored)"]
    async fn live_list_subdirectory_no_trailing_slash() {
        let Some((url, user, pass, subdir)) = live_webdav_config() else {
            eprintln!("WDMM_TEST_WEBDAV_URL not set — skipping");
            return;
        };
        // Base URL intentionally omits trailing slash (the regression scenario).
        let base = url.trim_end_matches('/').to_string();
        let driver = WebDavDriver::new(&base, &user, &pass).unwrap();

        let path = format!("/{}", subdir.trim_matches('/'));
        let items = driver
            .list(&path)
            .await
            .unwrap_or_else(|e| panic!("list {path} failed: {e}"));

        assert!(
            !items.is_empty(),
            "subdirectory {path} listing must not be empty"
        );

        // All raw_urls must include the subdirectory component.
        let expected_fragment = format!("{}{}/", base, path);
        for item in &items {
            if let Some(raw) = &item.raw_url {
                assert!(
                    raw.starts_with(&expected_fragment) || raw.starts_with(&format!("{}{}", base, path)),
                    "raw_url missing expected base path fragment.\nexpected prefix: {expected_fragment}\ngot: {raw}"
                );
            }
        }
    }

    // ── Full CRUD live tests ─────────────────────────────────────────────────
    //
    // These exercise put_content / get_content / get_content_range / get /
    // mkdir / rename / copy / remove / ensure_dir against a real server.
    //
    // Servers (e.g. OpenList/Alist) may not implement the full WebDAV spec.
    // Tests that hit an unsupported operation print a clear note and skip
    // rather than failing — the important thing is that we know what works.
    //
    // All writes go under a test-scoped subdirectory and are cleaned up at
    // the end (best-effort).  Use a writable account; the guest account may
    // be read-only on some servers.

    /// Returns true if the error looks like "server doesn't support this method".
    fn is_unsupported(e: &DriverError) -> bool {
        match e {
            DriverError::Network(msg) => {
                let m = msg.to_ascii_lowercase();
                m.contains("405") || m.contains("501") || m.contains("not allowed")
                    || m.contains("not implemented") || m.contains("forbidden")
                    || m.contains("403") || m.contains("500")
            }
            _ => false,
        }
    }

    #[tokio::test]
    #[ignore = "requires WDMM_TEST_WEBDAV_URL env var (run with --ignored); account needs write access"]
    async fn live_crud_put_get_remove() {
        let Some((url, user, pass, subdir)) = live_webdav_config() else {
            eprintln!("WDMM_TEST_WEBDAV_URL not set — skipping");
            return;
        };
        let base = url.trim_end_matches('/').to_string();
        let driver = WebDavDriver::new(&base, &user, &pass).unwrap();

        let test_dir = format!("/{}/wdmm_test_crud", subdir.trim_matches('/'));
        let test_file = format!("{}/hello.txt", test_dir);
        let payload = b"wdmm-live-test-payload-v1";

        // ── ensure_dir ──────────────────────────────────────────────────────
        match driver.ensure_dir(&test_dir).await {
            Ok(()) => eprintln!("ensure_dir({test_dir}): ok"),
            Err(e) if is_unsupported(&e) => {
                eprintln!("NOTE: server does not support MKCOL (ensure_dir): {e}");
                return;
            }
            Err(e) => panic!("ensure_dir failed unexpectedly: {e}"),
        }

        // ── put_content ─────────────────────────────────────────────────────
        match driver.put_content(&test_file, payload).await {
            Ok(()) => eprintln!("put_content({test_file}): ok"),
            Err(e) if is_unsupported(&e) => {
                eprintln!("NOTE: server does not support PUT (put_content): {e}");
                let _ = driver.remove(&test_dir).await;
                return;
            }
            Err(e) => {
                let _ = driver.remove(&test_dir).await;
                panic!("put_content failed: {e}");
            }
        }

        // ── get_content ─────────────────────────────────────────────────────
        match driver.get_content(&test_file).await {
            Ok(data) => {
                assert_eq!(data.as_slice(), payload, "get_content round-trip mismatch");
                eprintln!("get_content({test_file}): ok ({} bytes)", data.len());
            }
            Err(e) if is_unsupported(&e) => {
                eprintln!("NOTE: server does not support GET (get_content): {e}");
            }
            Err(e) => {
                let _ = driver.remove(&test_file).await;
                let _ = driver.remove(&test_dir).await;
                panic!("get_content failed: {e}");
            }
        }

        // ── get_content_range ───────────────────────────────────────────────
        match driver.get_content_range(&test_file, 5, Some(9)).await {
            Ok(data) => {
                // bytes 5..=9 of "wdmm-live-test-payload-v1" = "live-"
                assert_eq!(&data, b"live-", "range [5,9] mismatch");
                eprintln!("get_content_range({test_file}, 5..9): ok");
            }
            Err(e) if is_unsupported(&e) => {
                eprintln!("NOTE: server does not support Range requests: {e}");
            }
            Err(e) => {
                let _ = driver.remove(&test_file).await;
                let _ = driver.remove(&test_dir).await;
                panic!("get_content_range failed: {e}");
            }
        }

        // ── get (depth-0 stat) ───────────────────────────────────────────────
        match driver.get(&test_file).await {
            Ok(item) => {
                assert_eq!(item.name, "hello.txt");
                assert!(!item.is_dir);
                assert_eq!(item.size, payload.len() as u64, "stat size mismatch");
                eprintln!("get({test_file}): ok (size={})", item.size);
            }
            Err(e) if is_unsupported(&e) => {
                eprintln!("NOTE: server does not support depth-0 PROPFIND (get): {e}");
            }
            Err(e) => {
                let _ = driver.remove(&test_file).await;
                let _ = driver.remove(&test_dir).await;
                panic!("get failed: {e}");
            }
        }

        // ── rename (MOVE) ────────────────────────────────────────────────────
        let renamed = format!("{}/hello_renamed.txt", test_dir);
        match driver.rename(&test_file, &renamed).await {
            Ok(()) => eprintln!("rename({test_file} → {renamed}): ok"),
            Err(e) if is_unsupported(&e) => {
                eprintln!("NOTE: server does not support MOVE (rename): {e}");
                // still present as test_file — remove it
                let _ = driver.remove(&test_file).await;
                let _ = driver.remove(&test_dir).await;
                return;
            }
            Err(e) => {
                let _ = driver.remove(&test_file).await;
                let _ = driver.remove(&test_dir).await;
                panic!("rename failed: {e}");
            }
        }

        // ── copy ─────────────────────────────────────────────────────────────
        let copied = format!("{}/hello_copy.txt", test_dir);
        match driver.copy(&renamed, &copied).await {
            Ok(()) => eprintln!("copy({renamed} → {copied}): ok"),
            Err(e) if is_unsupported(&e) => {
                eprintln!("NOTE: server does not support COPY: {e}");
            }
            Err(e) => {
                let _ = driver.remove(&renamed).await;
                let _ = driver.remove(&test_dir).await;
                panic!("copy failed: {e}");
            }
        }

        // ── remove (cleanup) ─────────────────────────────────────────────────
        for path in &[&renamed, &copied, &test_dir] {
            match driver.remove(path).await {
                Ok(()) => eprintln!("remove({path}): ok"),
                Err(e) if is_unsupported(&e) => {
                    eprintln!("NOTE: server does not support DELETE (remove): {e}");
                }
                Err(e) => eprintln!("WARN: cleanup remove({path}) failed: {e}"),
            }
        }
    }

    #[tokio::test]
    #[ignore = "requires WDMM_TEST_WEBDAV_URL env var (run with --ignored); account needs write access"]
    async fn live_mkdir_and_remove() {
        let Some((url, user, pass, subdir)) = live_webdav_config() else {
            eprintln!("WDMM_TEST_WEBDAV_URL not set — skipping");
            return;
        };
        let base = url.trim_end_matches('/').to_string();
        let driver = WebDavDriver::new(&base, &user, &pass).unwrap();

        let test_dir = format!("/{}/wdmm_test_mkdir", subdir.trim_matches('/'));

        // ── mkdir ────────────────────────────────────────────────────────────
        match driver.mkdir(&test_dir).await {
            Ok(()) => eprintln!("mkdir({test_dir}): ok"),
            Err(e) if is_unsupported(&e) => {
                eprintln!("NOTE: server does not support MKCOL (mkdir): {e}");
                return;
            }
            Err(e) => panic!("mkdir failed: {e}"),
        }

        // Verify directory exists via list of parent
        let parent = format!("/{}", subdir.trim_matches('/'));
        match driver.list(&parent).await {
            Ok(items) => {
                let found = items.iter().any(|i| i.name == "wdmm_test_mkdir" && i.is_dir);
                assert!(found, "newly created dir not found in parent listing");
                eprintln!("verified wdmm_test_mkdir in parent listing");
            }
            Err(e) => eprintln!("WARN: could not verify mkdir via list: {e}"),
        }

        // ── cleanup ──────────────────────────────────────────────────────────
        match driver.remove(&test_dir).await {
            Ok(()) => eprintln!("remove({test_dir}): ok"),
            Err(e) if is_unsupported(&e) => {
                eprintln!("NOTE: server does not support DELETE: {e}");
            }
            Err(e) => eprintln!("WARN: cleanup failed: {e}"),
        }
    }
}

