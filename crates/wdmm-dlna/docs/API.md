# wdmm-dlna — Public API

DLNA/UPnP SSDP discovery and AVTransport SOAP control.

The unified local HTTP range proxy has been extracted to its own crate,
**`wdmm-proxy`** (see `crates/wdmm-proxy/docs/API.md`). For backward
convenience, `wdmm_dlna::ProxyHandle` is re-exported from `wdmm_proxy`; new
code should depend on `wdmm-proxy` directly.

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

### `struct RendererDescription`
Display fields extracted from a device description XML, plus whether the device
advertises AVTransport (i.e. can be cast to).
Fields: `friendly_name`, `model_name`, `manufacturer`, `model_number`
(all `Option<String>`) and `has_avtransport: bool`.

### `async fn describe_renderer(location: &str) -> DlnaResult<RendererDescription>`
Fetch the description at `location` and parse it. Only fetch/HTTP failures are
errors; malformed XML yields default fields so one broken renderer cannot break
the discovery list.

### `fn parse_renderer_description(xml: &str) -> RendererDescription`
Pure parser behind [`describe_renderer`]: takes the first non-empty occurrence
of each tag inside `<device>`, and sets `has_avtransport` when the description
contains `urn:schemas-upnp-org:service:AVTransport:1` with a `controlURL`.
Malformed XML never panics.

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

### `async fn AvTransportClient::set_uri_with_metadata(&self, instance_id: u32, uri: &str, metadata_xml: &str) -> DlnaResult<()>`
`SetAVTransportURI` with a DIDL-Lite `CurrentURIMetaData` payload. Build the
metadata with [`build_didl_lite`].

### `async fn AvTransportClient::play(&self, instance_id: u32, speed: u8) -> DlnaResult<()>`
`Play` SOAP action.

### `async fn AvTransportClient::pause(&self, instance_id: u32) -> DlnaResult<()>`

### `async fn AvTransportClient::stop(&self, instance_id: u32) -> DlnaResult<()>`

### `async fn AvTransportClient::seek(&self, instance_id: u32, unit: &str, target: &str) -> DlnaResult<()>`
`unit` is e.g. `"REL_TIME"` / `"TRACK_NR"`.

### `async fn AvTransportClient::get_position_info(&self, instance_id: u32) -> DlnaResult<PositionInfo>`
`GetPositionInfo` — `track`, `track_duration`, `track_uri`, `rel_time`, `abs_time`
(all `HH:MM:SS` strings except `track`).

### `async fn AvTransportClient::get_transport_info(&self, instance_id: u32) -> DlnaResult<TransportInfo>`
`GetTransportInfo` — `state: TransportState`, `status: String`, `speed: String`.
`TransportState::as_str()` yields the canonical UPnP string.

---

## ConnectionManager client (`wdmm_dlna::ConnectionManagerClient`)

Ports Dart `_connMgrControlUrl` / `_soapConnMgr` / `_getSinkProtocolInfo` /
`_negotiateProtocolInfo`.

### `fn ConnectionManagerClient::new(control_url: impl Into<String>) -> Self`
Construct from an already-resolved control URL (tests / pre-resolved callers).

### `async fn ConnectionManagerClient::from_renderer(renderer: &DlnaRenderer) -> DlnaResult<Self>`
Fetch device description, parse the
`urn:schemas-upnp-org:service:ConnectionManager:1` `controlURL`. Returns
`Cast` when the description is unreachable or has no ConnectionManager service
(Dart treats that case as an empty sink list).

### `fn ConnectionManagerClient::control_url(&self) -> &str`

### `async fn ConnectionManagerClient::get_sink_protocol_info(&self) -> DlnaResult<Vec<String>>`
`GetProtocolInfo` → the `<Sink>` list split on `,`, trimmed, empty entries
dropped. SOAP/transport failures are `Cast`; use `unwrap_or_default()` for
Dart's "empty list on failure" behaviour.

### `fn ConnectionManagerClient::negotiate(&self, mime: &str, mime_ext: &str, sink: &[String]) -> String`
Pick the `protocolInfo` to embed in DIDL-Lite: (1) first sink entry whose third
field equals `mime` verbatim, (2) first `video/*` / `*/*` entry rewritten to
`mime` (keeping its fourth field, or appending `DLNA.ORG_OP=01` when absent),
(3) fallback `http-get:*:<mime>:DLNA.ORG_OP=01`. `mime_ext` is accepted for FFI
compatibility but does not affect the decision (Dart parity).

### `fn wdmm_dlna::connection_manager::negotiate_protocol_info(mime: &str, sink: &[String]) -> String`
Pure version of the same negotiation, for callers with an already-fetched sink
list.

---

## DIDL-Lite builder (`wdmm_dlna::didl`)

### `struct DidlRes { url: String, protocol_info: String }`
One `<res>` of the DIDL item; `DidlRes::new(url, protocol_info)` is a
convenience constructor.

### `const SUBTITLE_PROTOCOL_INFO: &str = "http-get:*:text/plain:DLNA.ORG_PN=*";`
`protocolInfo` for the sidecar-subtitle `<res>` (second resource).

### `fn build_didl_lite(title: &str, resources: &[DidlRes]) -> String`
Byte-identical to Dart `_didlLite`: `<DIDL-Lite>` wrapper, one video `item`
(`object.item.videoItem`), one `<res>` per resource. Title, URLs and
`protocolInfo` are XML-escaped (`& < > " '`).

---

## Cast session (`wdmm_dlna::DlnaSession`)

Pure protocol state machine over AVTransport — no driver, proxy or transcoding
dependencies. One session per renderer.

### `async fn DlnaSession::open(renderer: &DlnaRenderer) -> DlnaResult<Self>`
Fetch description → resolve AVTransport control URL → idle session.

### `async fn DlnaSession::start(&mut self, media_url: &str, didl: &str) -> DlnaResult<()>`
`SetAVTransportURI` (with DIDL metadata) + `Play(Speed=1)`.

### `async fn DlnaSession::play(&mut self) -> DlnaResult<()>`
### `async fn DlnaSession::pause(&mut self) -> DlnaResult<()>`
### `async fn DlnaSession::stop(&mut self) -> DlnaResult<()>`
Arms the `local_stop` guard so the following `STOPPED` observation is not
reported as `ended`.

### `async fn DlnaSession::seek(&mut self, position_ms: i64) -> DlnaResult<()>`
`Seek REL_TIME` with an `HH:MM:SS` target (negative inputs clamp to zero).

### `async fn DlnaSession::switch_to(&mut self, media_url: &str, didl: &str) -> DlnaResult<()>`
Next-episode switch: same `SetAVTransportURI` + `Play` sequence, no explicit
`Stop` (Dart parity).

### `async fn DlnaSession::tick(&mut self) -> DlnaResult<TickResult>`
One poll (`GetTransportInfo`, plus `GetPositionInfo` while the transport is not
`STOPPED`). Nothing is polled before `open`; `last_transport` starts as
`"STOPPED"`, so an idle renderer reporting `STOPPED` is not an `ended` event.

### `struct TickResult`
`transport: String` (canonical UPnP state), `position_ms: i64`,
`duration_ms: i64`, `ended: bool`, `ended_reason: Option<&'static str>`.

`ended` is `true` on the first `STOPPED` observation after a non-`STOPPED` one,
when the stop was not locally initiated. `local_stop` is *consumed once*: the
tick that observes `STOPPED` clears it, so one `stop()` suppresses exactly one
`ended`; ticks that still see `PLAYING`/`TRANSITIONING` keep it armed.

`ended_reason` is `None` when `ended` is `false`, otherwise:

- `"eof"` — the last position observed while still playing is within
  `EOF_TOLERANCE_MS` (2000 ms) of the track duration;
- `"stopped_by_device"` — anything else, including an unknown/zero duration.

`position_ms`/`duration_ms` on a `STOPPED` tick are the last values observed
while playing: renderers usually zero the position once stopped, which is why
the heuristic uses the stored pre-stop position and why `GetPositionInfo` is
skipped on `STOPPED`.

### Accessors
`renderer() -> &DlnaRenderer`, `media_url() -> &str`, `didl_metadata() -> &str`.


---

## Local HTTP proxy

`ProxyHandle` is re-exported from `wdmm_proxy` — see `crates/wdmm-proxy/docs/API.md` for the full API.
