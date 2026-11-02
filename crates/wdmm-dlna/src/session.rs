//! DLNA cast session — pure protocol state machine over AVTransport.
//!
//! No knowledge of drivers, the local proxy, transcoding or the FFI layer: the
//! caller supplies the (already proxied) media URL plus DIDL-Lite metadata and
//! drives playback with [`DlnaSession::tick`].
//!
//! ## End-of-media detection
//!
//! `tick` reports `ended = true` only on the transition into `STOPPED` from a
//! non-`STOPPED` transport state while the stop was **not** initiated locally
//! (`stop()`). This mirrors the Dart `_poll` fix that distinguishes "track
//! finished" from "user/device stopped the renderer".
//!
//! `local_stop` is **consumed once**: the first `tick` that observes `STOPPED`
//! clears it, so a user-initiated stop suppresses exactly one `ended` report.
//! Subsequent `STOPPED` ticks are quiet anyway because `last_transport` is
//! already `STOPPED`; the flag is cleared only on the `STOPPED` observation
//! (ticks that still see `PLAYING`/`TRANSITIONING` keep it armed).
//!
//! ## End reason
//!
//! A renderer has no authoritative "playback finished" signal, so when
//! [`TickResult::ended`] fires, [`TickResult::ended_reason`] is derived from
//! the last position observed while still playing: `"eof"` when that position
//! is within [`EOF_TOLERANCE_MS`] of the track duration, `"stopped_by_device"`
//! otherwise (including when the duration is unknown). Positions reported after
//! the renderer already flipped to `STOPPED` are ignored because most DMRs zero
//! them out.
//!
//! `last_transport` starts as `"STOPPED"`: a `STOPPED` observation before
//! [`DlnaSession::start`] is not a playback transition and must not report
//! `ended`.

use crate::avtransport::AvTransportClient;
use crate::{DlnaRenderer, DlnaResult};

/// How close the last playing position must be to the track duration for an
/// end to be classified as `"eof"` rather than `"stopped_by_device"`.
///
/// A DLNA renderer gives no authoritative end-of-media signal; this heuristic
/// compares the last position observed while still playing with the reported
/// duration. 2 seconds absorbs the poll interval plus renderer position drift.
pub const EOF_TOLERANCE_MS: i64 = 2_000;

/// One poll result from [`DlnaSession::tick`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickResult {
    /// Canonical UPnP transport state: `PLAYING` / `PAUSED_PLAYBACK` /
    /// `STOPPED` / `TRANSITIONING` / `NO_MEDIA_PRESENT` (or a vendor string).
    pub transport: String,
    /// Playback position in milliseconds (`RelTime`).
    pub position_ms: i64,
    /// Track duration in milliseconds (`TrackDuration`); `0` when the renderer
    /// reports `NOT_IMPLEMENTED` or a malformed value.
    pub duration_ms: i64,
    /// The renderer reached `STOPPED` on its own (natural end of media).
    pub ended: bool,
    /// Why playback ended: `Some("eof")` (played to the end),
    /// `Some("stopped_by_device")` (renderer stopped away from the end), or
    /// `None` when `ended` is `false`.
    pub ended_reason: Option<&'static str>,
}

/// A cast session bound to one renderer.
///
/// Construct with [`DlnaSession::open`], then [`DlnaSession::start`] (or
/// [`DlnaSession::switch_to`] for the next episode) and poll with
/// [`DlnaSession::tick`].
pub struct DlnaSession {
    renderer: DlnaRenderer,
    av: AvTransportClient,
    media_url: String,
    didl: String,
    /// Set by local [`DlnaSession::stop`], consumed once by `tick`.
    local_stop: bool,
    /// Transport string observed by the previous `tick` (or implied by the
    /// last command). Starts as `"STOPPED"`: nothing has played yet.
    last_transport: String,
    /// Last position observed while the transport was not `STOPPED`; used for
    /// the [`TickResult::ended_reason`] heuristic.
    last_playing_position_ms: Option<i64>,
    /// Last track duration observed while the transport was not `STOPPED`.
    duration_ms: i64,
}

impl DlnaSession {
    /// Fetch the renderer description, resolve its AVTransport control URL and
    /// construct an idle session.
    pub async fn open(renderer: &DlnaRenderer) -> DlnaResult<Self> {
        let av = AvTransportClient::from_renderer(renderer).await?;
        Ok(Self {
            renderer: renderer.clone(),
            av,
            media_url: String::new(),
            didl: String::new(),
            local_stop: false,
            last_transport: "STOPPED".to_string(),
            last_playing_position_ms: None,
            duration_ms: 0,
        })
    }

    /// The renderer this session is bound to.
    pub fn renderer(&self) -> &DlnaRenderer {
        &self.renderer
    }

    /// Media URL currently loaded on the renderer (empty before `start`).
    pub fn media_url(&self) -> &str {
        &self.media_url
    }

    /// DIDL-Lite metadata last sent to the renderer.
    pub fn didl_metadata(&self) -> &str {
        &self.didl
    }

    /// `SetAVTransportURI` (with DIDL metadata) followed by `Play(Speed=1)`.
    pub async fn start(&mut self, media_url: &str, didl: &str) -> DlnaResult<()> {
        self.av.set_uri_with_metadata(0, media_url, didl).await?;
        self.av.play(0, 1).await?;
        self.media_url = media_url.to_string();
        self.didl = didl.to_string();
        self.local_stop = false;
        self.last_playing_position_ms = None;
        self.duration_ms = 0;
        // We just asked for playback; a subsequent STOPPED is a natural end.
        self.last_transport = "PLAYING".to_string();
        Ok(())
    }

    /// Resume playback (after [`DlnaSession::pause`]).
    pub async fn play(&mut self) -> DlnaResult<()> {
        self.av.play(0, 1).await?;
        self.local_stop = false;
        // Keep the last position/duration: it is still the same track.
        self.last_transport = "PLAYING".to_string();
        Ok(())
    }

    /// Pause playback.
    pub async fn pause(&mut self) -> DlnaResult<()> {
        self.av.pause(0).await?;
        self.last_transport = "PAUSED_PLAYBACK".to_string();
        Ok(())
    }

    /// Stop playback. Arms the `local_stop` guard so the matching `STOPPED`
    /// observation is not reported as `ended`; see the module docs.
    pub async fn stop(&mut self) -> DlnaResult<()> {
        self.av.stop(0).await?;
        self.local_stop = true;
        Ok(())
    }

    /// Seek to an absolute position inside the current track.
    pub async fn seek(&mut self, position_ms: i64) -> DlnaResult<()> {
        let target = format_hms(position_ms);
        self.av.seek(0, "REL_TIME", &target).await
    }

    /// Switch to another item (next episode) — `SetAVTransportURI` + `Play`.
    ///
    /// Follows the Dart `playNext` path: no explicit `Stop` before the switch.
    pub async fn switch_to(&mut self, media_url: &str, didl: &str) -> DlnaResult<()> {
        self.start(media_url, didl).await
    }

    /// Poll the renderer once and classify end-of-media.
    ///
    /// Performs `GetTransportInfo` + `GetPositionInfo`. `ended` is `true` on the
    /// first `STOPPED` observation after a non-`STOPPED` one when the stop was
    /// not locally initiated (see the module docs); `ended_reason` then
    /// distinguishes `"eof"` from `"stopped_by_device"` using the last position
    /// observed while still playing.
    ///
    /// Position and duration are only refreshed while the transport is not
    /// `STOPPED`: after a renderer-side stop most DMRs report zeroed positions,
    /// which would defeat the end-reason heuristic. `GetPositionInfo` is
    /// skipped entirely on a `STOPPED` observation.
    pub async fn tick(&mut self) -> DlnaResult<TickResult> {
        let info = self.av.get_transport_info(0).await?;
        let transport = info.state.as_str().to_string();

        if transport != "STOPPED" {
            let position = self.av.get_position_info(0).await?;
            self.last_playing_position_ms = Some(parse_duration_ms(&position.rel_time));
            self.duration_ms = parse_duration_ms(&position.track_duration);
        }

        let ended =
            transport == "STOPPED" && self.last_transport != "STOPPED" && !self.local_stop;
        let ended_reason = if ended {
            let position = self.last_playing_position_ms.unwrap_or(0);
            if self.duration_ms > 0 && position + EOF_TOLERANCE_MS >= self.duration_ms {
                Some("eof")
            } else {
                Some("stopped_by_device")
            }
        } else {
            None
        };
        if transport == "STOPPED" {
            // Consumed once: suppress only the observation that matches stop().
            self.local_stop = false;
        }
        self.last_transport = transport.clone();

        Ok(TickResult {
            transport,
            position_ms: self.last_playing_position_ms.unwrap_or(0),
            duration_ms: self.duration_ms,
            ended,
            ended_reason,
        })
    }
}

/// Parse a UPnP `HH:MM:SS[.mmm]` duration into milliseconds.
///
/// Dart-parity (`_parseDuration`): exactly three colon-separated parts are
/// required, the fractional part is right-padded/truncated to three digits,
/// and any malformed input yields `0`.
pub(crate) fn parse_duration_ms(hms: &str) -> i64 {
    let parts: Vec<&str> = hms.split(':').collect();
    if parts.len() != 3 {
        return 0;
    }
    let Ok(hours) = parts[0].parse::<i64>() else {
        return 0;
    };
    let Ok(minutes) = parts[1].parse::<i64>() else {
        return 0;
    };
    let mut sec_parts = parts[2].split('.');
    let Ok(seconds) = sec_parts.next().unwrap_or("").parse::<i64>() else {
        return 0;
    };
    let millis = match sec_parts.next() {
        Some(frac) => {
            let mut digits: String = frac.chars().take(3).collect();
            while digits.len() < 3 {
                digits.push('0');
            }
            digits.parse::<i64>().unwrap_or(0)
        }
        None => 0,
    };
    hours
        .saturating_mul(3_600_000)
        .saturating_add(minutes.saturating_mul(60_000))
        .saturating_add(seconds.saturating_mul(1_000))
        .saturating_add(millis)
}

/// Format milliseconds as `HH:MM:SS` for AVTransport `REL_TIME` seeks.
///
/// Dart-parity (`_formatHms`); negative inputs clamp to zero.
pub(crate) fn format_hms(position_ms: i64) -> String {
    let ms = position_ms.max(0);
    let total_seconds = ms / 1_000;
    let hours = total_seconds / 3_600;
    let minutes = (total_seconds / 60) % 60;
    let seconds = total_seconds % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::didl::{build_didl_lite, DidlRes};
    use crate::test_support::{spawn_fake_http, FakeHttpServer, FakeResponse};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    const PROTO: &str = "http-get:*:video/x-matroska:DLNA.ORG_PN=MKV;DLNA.ORG_OP=01";

    const DESC_XML: &str = r#"<?xml version="1.0"?>
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

    fn transport_resp(state: &str) -> String {
        format!(
            "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:GetTransportInfoResponse xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\"><CurrentTransportState>{state}</CurrentTransportState><CurrentTransportStatus>OK</CurrentTransportStatus><CurrentSpeed>1</CurrentSpeed></u:GetTransportInfoResponse></s:Body></s:Envelope>"
        )
    }

    fn pos_resp(rel_time: &str, track_duration: &str) -> String {
        format!(
            "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:GetPositionInfoResponse xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\"><Track>1</Track><TrackDuration>{track_duration}</TrackDuration><TrackURI>http://media/a.mkv</TrackURI><RelTime>{rel_time}</RelTime><AbsTime>{rel_time}</AbsTime></u:GetPositionInfoResponse></s:Body></s:Envelope>"
        )
    }

    fn renderer(server: &FakeHttpServer) -> DlnaRenderer {
        DlnaRenderer {
            usn: "uuid:mock-tv".into(),
            location: server.url("/desc.xml"),
            server: Some("Mock".into()),
        }
    }

    /// Fake DMR: description GET, AVTransport SOAP actions, a transport state
    /// script advanced once per `GetTransportInfo` call, and a position script
    /// (`(RelTime, TrackDuration)`) advanced once per `GetPositionInfo` call.
    /// Position defaults to `("0:00:10", "0:01:00")` when the script runs out.
    async fn spawn_mock_renderer_with_positions(
        states: Vec<&'static str>,
        positions: Vec<(&'static str, &'static str)>,
    ) -> FakeHttpServer {
        let states = Arc::new(states);
        let positions = Arc::new(positions);
        let query_count = Arc::new(AtomicUsize::new(0));
        let position_count = Arc::new(AtomicUsize::new(0));
        spawn_fake_http(move |req| {
            if req.starts_with("GET ") {
                FakeResponse::ok(DESC_XML)
            } else if req.contains("GetTransportInfo") {
                let idx = query_count.fetch_add(1, Ordering::SeqCst);
                let state = states.get(idx).copied().unwrap_or("STOPPED");
                FakeResponse::ok(transport_resp(state))
            } else if req.contains("GetPositionInfo") {
                let idx = position_count.fetch_add(1, Ordering::SeqCst);
                let (rel, dur) = positions
                    .get(idx)
                    .copied()
                    .unwrap_or(("0:00:10", "0:01:00"));
                FakeResponse::ok(pos_resp(rel, dur))
            } else if req.contains("u:SetAVTransportURI")
                || req.contains("u:Play")
                || req.contains("u:Pause")
                || req.contains("u:Stop")
                || req.contains("u:Seek")
            {
                FakeResponse::empty()
            } else {
                FakeResponse::not_found()
            }
        })
        .await
    }

    /// Fake DMR with the default position script (`0:00:10` / `0:01:00`).
    async fn spawn_mock_renderer(states: Vec<&'static str>) -> FakeHttpServer {
        spawn_mock_renderer_with_positions(states, Vec::new()).await
    }

    fn simple_didl(url: &str) -> String {
        build_didl_lite("Episode", &[DidlRes::new(url, PROTO)])
    }

    #[tokio::test]
    async fn open_resolves_avtransport_control_url() {
        let server = spawn_mock_renderer(vec!["PLAYING"]).await;
        let session = DlnaSession::open(&renderer(&server))
            .await
            .expect("session open");
        assert_eq!(session.renderer().usn, "uuid:mock-tv");
        assert!(session.media_url().is_empty());
        // Opening only fetches the description (no SOAP yet).
        assert_eq!(server.captured().len(), 1);
    }

    #[tokio::test]
    async fn start_sends_didl_metadata_then_play() {
        let server = spawn_mock_renderer(vec!["PLAYING"]).await;
        let mut session = DlnaSession::open(&renderer(&server)).await.unwrap();

        let didl = simple_didl("http://media/a.mkv");
        session.start("http://media/a.mkv", &didl).await.unwrap();

        let requests = server.captured();
        assert_eq!(requests.len(), 3, "GET desc + SetAVTransportURI + Play");
        let set_uri = &requests[1];
        assert!(set_uri.contains("<u:SetAVTransportURI"));
        assert!(set_uri.contains("<CurrentURI>http://media/a.mkv</CurrentURI>"));
        // DIDL is nested as text → escaped inside the SOAP envelope.
        assert!(set_uri.contains("&lt;DIDL-Lite"));
        assert!(set_uri.contains("&lt;dc:title&gt;Episode&lt;/dc:title&gt;"));
        let play = &requests[2];
        assert!(play.contains("<u:Play"));
        assert!(play.contains("<Speed>1</Speed>"));

        assert_eq!(session.media_url(), "http://media/a.mkv");
        assert_eq!(session.didl_metadata(), didl);
    }

    #[tokio::test]
    async fn tick_reports_playing_then_natural_end() {
        let server = spawn_mock_renderer(vec!["PLAYING", "STOPPED", "STOPPED"]).await;
        let mut session = DlnaSession::open(&renderer(&server)).await.unwrap();
        session
            .start("http://media/a.mkv", &simple_didl("http://media/a.mkv"))
            .await
            .unwrap();

        let first = session.tick().await.unwrap();
        assert_eq!(first.transport, "PLAYING");
        assert!(!first.ended);
        assert_eq!(first.ended_reason, None);
        assert_eq!(first.position_ms, 10_000);
        assert_eq!(first.duration_ms, 60_000);

        // STOPPED after PLAYING without local stop → natural end.
        let second = session.tick().await.unwrap();
        assert_eq!(second.transport, "STOPPED");
        assert!(second.ended);
        // Stopped at 10s of a 60s track → renderer-side stop, not eof.
        assert_eq!(second.ended_reason, Some("stopped_by_device"));

        // Consecutive STOPPED is no longer "ended".
        let third = session.tick().await.unwrap();
        assert!(!third.ended);
        assert_eq!(third.ended_reason, None);
    }

    #[tokio::test]
    async fn ended_reason_eof_when_last_playing_position_near_duration() {
        let server = spawn_mock_renderer_with_positions(
            vec!["PLAYING", "PLAYING", "STOPPED"],
            vec![("0:00:01", "0:00:05"), ("0:00:04.800", "0:00:05")],
        )
        .await;
        let mut session = DlnaSession::open(&renderer(&server)).await.unwrap();
        session
            .start("http://media/a.mkv", &simple_didl("http://media/a.mkv"))
            .await
            .unwrap();

        let first = session.tick().await.unwrap();
        assert_eq!(first.transport, "PLAYING");
        assert_eq!(first.position_ms, 1_000);
        assert_eq!(first.duration_ms, 5_000);
        assert_eq!(first.ended_reason, None);

        let second = session.tick().await.unwrap();
        assert_eq!(second.position_ms, 4_800);
        assert_eq!(second.ended_reason, None);

        // STOPPED zeroes positions on most DMRs; the stored 4800 ms is used.
        let third = session.tick().await.unwrap();
        assert_eq!(third.transport, "STOPPED");
        assert!(third.ended);
        assert_eq!(third.ended_reason, Some("eof"));
        assert_eq!(third.position_ms, 4_800);
        assert_eq!(third.duration_ms, 5_000);
    }

    #[tokio::test]
    async fn ended_reason_stopped_by_device_when_position_far_from_end() {
        let server = spawn_mock_renderer_with_positions(
            vec!["PLAYING", "STOPPED"],
            vec![("0:00:01", "0:00:05")],
        )
        .await;
        let mut session = DlnaSession::open(&renderer(&server)).await.unwrap();
        session
            .start("http://media/a.mkv", &simple_didl("http://media/a.mkv"))
            .await
            .unwrap();

        assert_eq!(session.tick().await.unwrap().position_ms, 1_000);
        let stopped = session.tick().await.unwrap();
        assert!(stopped.ended);
        assert_eq!(stopped.ended_reason, Some("stopped_by_device"));
    }

    #[tokio::test]
    async fn ended_reason_stopped_by_device_when_duration_unknown() {
        let server = spawn_mock_renderer_with_positions(
            vec!["PLAYING", "STOPPED"],
            vec![("0:00:01", "NOT_IMPLEMENTED")],
        )
        .await;
        let mut session = DlnaSession::open(&renderer(&server)).await.unwrap();
        session
            .start("http://media/a.mkv", &simple_didl("http://media/a.mkv"))
            .await
            .unwrap();

        let playing = session.tick().await.unwrap();
        assert_eq!(playing.duration_ms, 0);
        let stopped = session.tick().await.unwrap();
        assert!(stopped.ended);
        assert_eq!(stopped.ended_reason, Some("stopped_by_device"));
    }

    #[tokio::test]
    async fn stopped_before_start_is_not_ended() {
        // `open` initialises last_transport to "STOPPED": an idle renderer that
        // already reports STOPPED is not a playback transition.
        let server = spawn_mock_renderer(vec!["STOPPED"]).await;
        let mut session = DlnaSession::open(&renderer(&server)).await.unwrap();
        let tick = session.tick().await.unwrap();
        assert_eq!(tick.transport, "STOPPED");
        assert!(!tick.ended);
        assert_eq!(tick.ended_reason, None);
        assert_eq!(tick.position_ms, 0);
    }

    #[tokio::test]
    async fn local_stop_suppresses_ended_once() {
        let server = spawn_mock_renderer(vec!["STOPPED", "STOPPED", "STOPPED"]).await;
        let mut session = DlnaSession::open(&renderer(&server)).await.unwrap();
        session
            .start("http://media/a.mkv", &simple_didl("http://media/a.mkv"))
            .await
            .unwrap();

        session.stop().await.unwrap();
        let stopped = session.tick().await.unwrap();
        assert_eq!(stopped.transport, "STOPPED");
        assert!(!stopped.ended, "local stop must not be reported as ended");

        // local_stop was consumed; last_transport is STOPPED so still quiet.
        assert!(!session.tick().await.unwrap().ended);
    }

    #[tokio::test]
    async fn local_stop_survives_non_stopped_ticks() {
        // Device still reports PLAYING while the user stop is in flight.
        let server = spawn_mock_renderer(vec!["PLAYING", "STOPPED"]).await;
        let mut session = DlnaSession::open(&renderer(&server)).await.unwrap();
        session
            .start("http://media/a.mkv", &simple_didl("http://media/a.mkv"))
            .await
            .unwrap();

        session.stop().await.unwrap();
        let still_playing = session.tick().await.unwrap();
        assert_eq!(still_playing.transport, "PLAYING");
        assert!(!still_playing.ended);

        // The flag must still be armed when STOPPED finally shows up.
        let stopped = session.tick().await.unwrap();
        assert_eq!(stopped.transport, "STOPPED");
        assert!(!stopped.ended);
    }

    #[tokio::test]
    async fn switch_to_and_seek_send_expected_soap() {
        let server = spawn_mock_renderer(vec!["PLAYING"]).await;
        let mut session = DlnaSession::open(&renderer(&server)).await.unwrap();
        session
            .start("http://media/a.mkv", &simple_didl("http://media/a.mkv"))
            .await
            .unwrap();

        session
            .switch_to("http://media/b.mkv", &simple_didl("http://media/b.mkv"))
            .await
            .unwrap();
        session.seek(3_723_000).await.unwrap();

        let requests = server.captured();
        let after_start = &requests[3..];
        assert!(
            after_start
                .iter()
                .filter(|r| r.contains("<u:SetAVTransportURI"))
                .any(|r| r.contains("http://media/b.mkv")),
            "switch_to must send the new URI: {after_start:?}"
        );
        assert!(
            after_start
                .iter()
                .any(|r| r.contains("<CurrentURIMetaData>&lt;DIDL-Lite")),
            "switch_to must carry the new DIDL metadata"
        );
        let seek = after_start
            .iter()
            .find(|r| r.contains("<u:Seek"))
            .expect("seek request");
        assert!(seek.contains("<Unit>REL_TIME</Unit>"));
        assert!(seek.contains("<Target>01:02:03</Target>"));
        assert_eq!(session.media_url(), "http://media/b.mkv");
    }

    #[tokio::test]
    async fn tick_surfaces_soap_failures() {
        let server = spawn_fake_http(|req| {
            if req.starts_with("GET ") {
                FakeResponse::ok(DESC_XML)
            } else {
                FakeResponse::not_found()
            }
        })
        .await;
        let mut session = DlnaSession::open(&renderer(&server)).await.unwrap();
        assert!(session.tick().await.is_err());
    }

    // ── duration parsing / formatting (Dart parity) ──────────────────────────

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration_ms("0:01:12"), 72_000);
        assert_eq!(parse_duration_ms("0:03:42"), 222_000);
        assert_eq!(parse_duration_ms("1:02:03"), 3_723_000);
        assert_eq!(parse_duration_ms("0:00:01.5"), 1_500);
        assert_eq!(parse_duration_ms("0:00:01.2345"), 1_234);
        assert_eq!(parse_duration_ms("0:00:01."), 1_000);
        assert_eq!(parse_duration_ms("NOT_IMPLEMENTED"), 0);
        assert_eq!(parse_duration_ms("1:2"), 0);
        assert_eq!(parse_duration_ms(""), 0);
    }

    #[test]
    fn formats_hms() {
        assert_eq!(format_hms(0), "00:00:00");
        assert_eq!(format_hms(1_000), "00:00:01");
        assert_eq!(format_hms(3_723_000), "01:02:03");
        assert_eq!(format_hms(-5_000), "00:00:00");
    }
}
