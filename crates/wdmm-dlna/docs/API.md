# wdmm-dlna — Public API

DLNA/UPnP SSDP discovery, unified local HTTP range proxy, and AVTransport SOAP control.

---

## Error types

### `enum DlnaError`
`Discovery(String)`, `Cast(String)`, `Io(#[from] std::io::Error)`, `Unsupported`.

### `type DlnaResult<T> = std::result::Result<T, DlnaError>`

---

## Discovery types

### `struct DlnaRenderer`
A DLNA renderer discovered via SSDP.
Fields: `usn: String` (stable unique id), `location: String` (description XML URL),
`server: Option<String>` (display hint).

---

## SSDP discovery (`wdmm_dlna::SsdpDiscovery`)

### `fn SsdpDiscovery::new() -> Self`
Target the standard SSDP multicast address `239.255.255.250:1900`.

### `fn SsdpDiscovery::with_target(target: SocketAddr) -> Self`
Override the M-SEARCH target. Used in tests against a fake responder.

### `async fn SsdpDiscovery::discover(&self, timeout: Duration) -> DlnaResult<Vec<DlnaRenderer>>`
Send an M-SEARCH and collect `MediaRenderer:1` responses until `timeout` elapses.
Responses are de-duplicated by USN.

---

## Cast service

### `struct DlnaCastService`
Combines discovery and AVTransport casting.

### `fn DlnaCastService::new() -> Self`

### `async fn DlnaCastService::discover(&self, timeout: Duration) -> DlnaResult<Vec<DlnaRenderer>>`
Delegates to `SsdpDiscovery::discover`.

### `async fn DlnaCastService::cast(&self, renderer: &DlnaRenderer, url: &str) -> DlnaResult<()>`
Full cast flow: fetch device description → resolve AVTransport control URL →
`SetAVTransportURI(url)` → `Play(Speed=1)`.

Errors: `Cast` on network or SOAP failure; `Discovery` if the renderer description
is unreachable.

---

## AVTransport client (`wdmm_dlna::AvTransportClient`)

### `fn AvTransportClient::new(control_url: String) -> Self`
Construct from an already-resolved control URL.

### `async fn AvTransportClient::from_renderer(renderer: &DlnaRenderer) -> DlnaResult<Self>`
Fetch device description, parse AVTransport `controlURL`, build client.

### `async fn AvTransportClient::set_uri(&self, instance_id: u32, uri: &str) -> DlnaResult<()>`
`SetAVTransportURI` SOAP action. Accepts an empty `CurrentURIMetaData`.

### `async fn AvTransportClient::play(&self, instance_id: u32, speed: u8) -> DlnaResult<()>`
`Play` SOAP action.

### `async fn AvTransportClient::pause(&self, instance_id: u32) -> DlnaResult<()>`

### `async fn AvTransportClient::stop(&self, instance_id: u32) -> DlnaResult<()>`

### `async fn AvTransportClient::seek(&self, instance_id: u32, unit: &str, target: &str) -> DlnaResult<()>`
`unit` is e.g. `"REL_TIME"` / `"TRACK_NR"`.

---

## Local HTTP proxy (`wdmm_dlna::ProxyHandle`)

Serves registered local files over HTTP with `Range` support.
Only token-addressed URLs are served; raw filesystem paths are never exposed.

### `async fn ProxyHandle::start(bind: SocketAddr) -> DlnaResult<Self>`
Start the proxy. Use port 0 for an ephemeral port.

### `fn ProxyHandle::addr(&self) -> SocketAddr`
The bound address (useful when port was 0).

### `fn ProxyHandle::register(&self, path: impl Into<PathBuf>) -> String`
Register a local file; returns an unguessable `http://<addr>/stream/<token>` URL.

### `fn ProxyHandle::unregister(&self, url: &str)`
Revoke a previously registered URL. The token is extracted from the URL path.

### `fn ProxyHandle::stop(self)`
Send graceful shutdown signal. Also triggered on `Drop`.
