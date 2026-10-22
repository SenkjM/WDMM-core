//! WebDAV driver — working `list()` referencing WDMM webdav_source.dart.

use async_trait::async_trait;
use base64::prelude::*;
use chrono::{DateTime, Utc};
use quick_xml::events::Event;
use quick_xml::Reader;
use reqwest::Client;
use url::Url;

use crate::driver::{CloudDriver, CloudFileItem, DriverError, Result};

pub struct WebDavDriver {
    base_url: Url,
    client: Client,
}

impl WebDavDriver {
    pub fn new(url: &str, username: &str, password: &str) -> Result<Self> {
        let base = Url::parse(url).map_err(|e| DriverError::Config(e.to_string()))?;

        let mut client_builder = Client::builder();
        if !username.is_empty() || !password.is_empty() {
            // Basic auth injected into default headers (reqwest pattern)
            let auth = format!("{}:{}", username, password);
            let encoded = base64::prelude::BASE64_STANDARD.encode(auth.as_bytes());
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::AUTHORIZATION,
                format!("Basic {}", encoded).parse().unwrap(),
            );
            client_builder = client_builder.default_headers(headers);
        }

        Ok(Self {
            base_url: base,
            client: client_builder.build().map_err(|e| DriverError::Config(e.to_string()))?,
        })
    }

    fn join_path(&self, path: &str) -> String {
        let trimmed = path.trim_start_matches('/');
        self.base_url.join(trimmed).unwrap().to_string()
    }

    /// Parse PROPFIND XML and extract items.
    /// `propfind_url` is the full URL that was requested (for filtering the collection itself).
    fn parse_propfind_response(&self, xml: &str, propfind_url: &str) -> Result<Vec<CloudFileItem>> {
        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(true);

        // Extract the path from the PROPFIND URL for filtering.
        let propfind_path = Url::parse(propfind_url)
            .ok()
            .and_then(|u| u.path().strip_prefix('/').map(|s| format!("/{}", s)))
            .unwrap_or_else(|| propfind_url.to_string());

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
                            let name = decoded_href.trim_end_matches('/').split('/').last().unwrap_or("").to_string();

                            // skip the collection itself (href matches propfind_path)
                            if !name.is_empty() && decoded_href.trim_end_matches('/') != propfind_path.trim_end_matches('/') {
                                items.push(CloudFileItem {
                                    name,
                                    is_dir: current_is_dir,
                                    size: current_size,
                                    modified: current_modified,
                                    raw_url: Some(self.join_path(&decoded_href)),
                                    raw_headers: vec![],
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
}

#[async_trait]
impl CloudDriver for WebDavDriver {
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
            return Err(DriverError::Network(format!("PROPFIND failed: {}", resp.status())));
        }

        let body = resp.text().await?;
        self.parse_propfind_response(&body, &url)
    }

    async fn get(&self, path: &str) -> Result<CloudFileItem> {
        let items = self.list(&format!("{}/..", path)).await?;
        let name = path.trim_end_matches('/').split('/').last().unwrap_or("");
        items.into_iter().find(|i| i.name == name).ok_or_else(|| DriverError::NotFound(path.to_string()))
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_joining() {
        let driver = WebDavDriver::new("https://example.com/dav/", "", "").unwrap();
        assert_eq!(driver.join_path("/Music/track.mp3"), "https://example.com/dav/Music/track.mp3");
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
            .parse_propfind_response(PROPFIND_FIXTURE, "/dav/Music")
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
        let items = driver.parse_propfind_response(UNPREFIXED, "/dav").unwrap();
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
        let items = driver.parse_propfind_response(EMPTY, "/dav").unwrap();
        assert!(items.is_empty());
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
    // Verifies both new CloudDriver methods work end-to-end.

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
}

