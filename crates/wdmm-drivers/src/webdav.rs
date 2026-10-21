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
    fn parse_propfind_response(&self, xml: &str, req_path: &str) -> Result<Vec<CloudFileItem>> {
        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(true);

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
                    b"d:response" | b"response" => {
                        in_response = true;
                        current_href = None;
                        current_is_dir = false;
                        current_size = 0;
                        current_modified = None;
                    }
                    b"d:href" | b"href" if in_response => {
                        if let Ok(Event::Text(t)) = reader.read_event_into(&mut buf) {
                            current_href = t.unescape().ok().map(|s| s.to_string());
                        }
                    }
                    b"d:collection" | b"collection" => {
                        current_is_dir = true;
                    }
                    b"d:getcontentlength" | b"getcontentlength" if in_response => {
                        if let Ok(Event::Text(t)) = reader.read_event_into(&mut buf) {
                            if let Ok(s) = t.unescape() {
                                current_size = s.parse().unwrap_or(0);
                            }
                        }
                    }
                    b"d:getlastmodified" | b"getlastmodified" if in_response => {
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
                Ok(Event::End(e)) => {
                    if matches!(e.name().as_ref(), b"d:response" | b"response") {
                        in_response = false;
                        if let Some(href) = current_href.take() {
                            let decoded_href = urlencoding::decode(&href).unwrap_or_default().to_string();
                            let name = decoded_href.trim_end_matches('/').split('/').last().unwrap_or("").to_string();

                            // skip the collection itself (href == req_path)
                            if !name.is_empty() && decoded_href.trim_end_matches('/') != req_path.trim_end_matches('/') {
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
        self.parse_propfind_response(&body, path)
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

    async fn rename(&self, _old: &str, _new: &str) -> Result<()> {
        Err(DriverError::Other("WebDAV MOVE not yet implemented".into()))
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_joining() {
        let driver = WebDavDriver::new("https://example.com/dav/", "", "").unwrap();
        assert_eq!(driver.join_path("/Music/track.mp3"), "https://example.com/dav/Music/track.mp3");
    }
}
