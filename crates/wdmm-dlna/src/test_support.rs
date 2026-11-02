//! Test-only helpers: a tiny fake UPnP HTTP server on loopback.
//!
//! No real renderer or network access is involved — tests bind
//! `127.0.0.1:0`, the client under test talks plain HTTP to it, and the
//! handler can be driven by internal mock state (e.g. a transport-state
//! counter) to emulate a DMR.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// One canned HTTP response from the fake server.
pub(crate) struct FakeResponse {
    /// Full status line payload, e.g. `"200 OK"`.
    pub status: &'static str,
    /// Response body (sent with `Content-Type: text/xml; charset=utf-8`).
    pub body: String,
}

impl FakeResponse {
    /// `200 OK` with an XML body.
    pub fn ok(body: impl Into<String>) -> Self {
        Self {
            status: "200 OK",
            body: body.into(),
        }
    }

    /// Empty `200 OK` body (enough for SOAP actions whose response is ignored).
    pub fn empty() -> Self {
        Self::ok("")
    }

    /// `404 Not Found`.
    pub fn not_found() -> Self {
        Self {
            status: "404 Not Found",
            body: String::new(),
        }
    }
}

/// Handle to a spawned fake HTTP server.
pub(crate) struct FakeHttpServer {
    /// Port the server listens on (`http://127.0.0.1:<port>`).
    pub port: u16,
    /// Raw request texts (head + body), in arrival order.
    pub requests: Arc<Mutex<Vec<String>>>,
}

impl FakeHttpServer {
    /// Absolute URL of a path served by this fake server.
    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// Snapshot of the captured raw requests.
    pub fn captured(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

/// Spawn a fake HTTP server on a random loopback port.
///
/// The listener task is detached and dies with the tokio runtime; tests are
/// expected to short-lived. `handler` receives the raw request text and
/// returns the canned response.
pub(crate) async fn spawn_fake_http<F>(handler: F) -> FakeHttpServer
where
    F: Fn(&str) -> FakeResponse + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind fake server");
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    let handler = Arc::new(handler);

    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let handler = Arc::clone(&handler);
            let captured = Arc::clone(&captured);
            tokio::spawn(async move {
                let Some(request) = read_request(&mut sock).await else {
                    return;
                };
                captured.lock().unwrap().push(request.clone());
                let response = handler(&request);
                let raw = format!(
                    "HTTP/1.1 {}\r\nContent-Type: text/xml; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.status,
                    response.body.len(),
                    response.body
                );
                let _ = sock.write_all(raw.as_bytes()).await;
                let _ = sock.flush().await;
                let _ = sock.shutdown().await;
            });
        }
    });

    FakeHttpServer { port, requests }
}

/// Read one HTTP/1.1 request (head + `Content-Length` body) as text.
async fn read_request(sock: &mut TcpStream) -> Option<String> {
    let mut data: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = sock.read(&mut buf).await.ok()?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
        if let Some(head_end) = find_subslice(&data, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&data[..head_end]).to_ascii_lowercase();
            let body_len = head
                .lines()
                .find_map(|line| {
                    line.strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                })
                .unwrap_or(0);
            if data.len() >= head_end + 4 + body_len {
                break;
            }
        }
    }
    Some(String::from_utf8_lossy(&data).to_string())
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
