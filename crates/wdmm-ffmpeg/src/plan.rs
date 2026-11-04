//! Pure remux / copy / encode decision logic for the three-layer preset model.
//!
//! These helpers involve no libav calls, so they compile (and are unit-tested)
//! on a host build without the `ffmpeg` feature — the Windows host has no
//! libav, which is exactly where the decision rules must be provable.
//!
//! Each stream has its own remux mode (`off` / `strict` / `on`, contract
//! `remux-modes` §3):
//!
//! - `off` re-encodes the stream;
//! - `on` copies it whenever MPEG-TS can carry the source codec;
//! - `strict` copies it only when the source codec is both MPEG-TS legal and
//!   identical to the codec the selected encoder would produce.
//!
//! [`decide_video_action`] / [`decide_audio_action`] implement that rule for
//! one stream. The caller decides each stream once per session and caches the
//! result — reopen/seek restarts must reuse the same decision.
//!
//! The target codec is a *runtime* value: the hardware encoder's output when it
//! is usable, otherwise the software encoder's. [`crate::SessionDecision::decide`]
//! computes it once and passes it down; audio has no hardware layer, so its
//! target is simply the preset's `audio_codec`.
//!
//! [`encoder_av_options`] then turns the preset's structured rate-control
//! fields into the exact `AVDictionary` handed to `avcodec_open2`. The preset
//! editor no longer offers free-form advanced options (contract
//! `preset-editor-slimming` §2); what remains is the closed rate-control
//! mapping plus the invariants that keep the segment grid intact.
//! [`video_encoder_catalog`] is the closed encoder list the editor renders.

use wdmm_core::models::TranscodePreset;

use crate::sys::AVRational;

/// What a remux mode does with one stream, one of
/// `wdmm_core::models::REMUX_MODES` (contract `remux-modes` §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemuxMode {
    /// Always re-encode the stream.
    Off,
    /// Copy only when the source codec is MPEG-TS legal *and* already the codec
    /// the selected encoder would produce.
    Strict,
    /// Copy whenever the source codec is MPEG-TS legal.
    On,
}

impl RemuxMode {
    /// Parse the preset value, with the exact user-facing error message used by
    /// `TranscodePreset::validate`.
    pub fn parse(mode: &str) -> Result<Self, String> {
        match mode {
            "off" => Ok(RemuxMode::Off),
            "strict" => Ok(RemuxMode::Strict),
            "on" => Ok(RemuxMode::On),
            other => Err(format!("unknown remux mode: {other}")),
        }
    }

    /// Stable preset token: `"off"`/`"strict"`/`"on"`.
    pub fn as_str(self) -> &'static str {
        match self {
            RemuxMode::Off => "off",
            RemuxMode::Strict => "strict",
            RemuxMode::On => "on",
        }
    }

    /// Whether a legal source stream may be copied, given the codec the
    /// selected encoder would produce (`0` = no encoder to run).
    fn copies(self, source_codec_id: u32, target_codec_id: u32) -> bool {
        if source_codec_id == 0 {
            return false;
        }
        match self {
            RemuxMode::Off => false,
            RemuxMode::Strict => target_codec_id != 0 && source_codec_id == target_codec_id,
            RemuxMode::On => true,
        }
    }
}

/// Streams kept in the HLS output: the best video stream and the best audio
/// stream (`-1` = not found). Everything else is dropped.
pub fn remux_stream_selection(best_video: i32, best_audio: i32) -> (Option<usize>, Option<usize>) {
    fn idx(i: i32) -> Option<usize> {
        (i >= 0).then_some(i as usize)
    }
    (idx(best_video), idx(best_audio))
}

/// Input indices that will NOT be muxed, so the diag log can explain drops.
pub fn remux_dropped_streams(best_video: i32, best_audio: i32, nb_streams: usize) -> Vec<usize> {
    let (keep_v, keep_a) = remux_stream_selection(best_video, best_audio);
    (0..nb_streams)
        .filter(|i| Some(*i) != keep_v && Some(*i) != keep_a)
        .collect()
}

// ── Three-layer stream decisions (contract §2) ───────────────────────────────

/// Decision for one video stream, fixed once per session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamAction<'a> {
    /// Pass the source bitstream through unchanged (MPEG-TS compatible codec).
    Copy,
    /// Re-encode with `encoder`; `hardware` selects the MediaCodec layer.
    Encode {
        /// `true` for a MediaCodec (hardware) encoder, `false` for software.
        hardware: bool,
        /// Encoder name from the preset (`""` never appears here).
        encoder: &'a str,
    },
}

impl<'a> StreamAction<'a> {
    /// `true` for [`StreamAction::Copy`].
    pub fn is_copy(&self) -> bool {
        matches!(self, StreamAction::Copy)
    }

    /// Encoder name when encoding, `None` for copy.
    pub fn encoder_name(&self) -> Option<&'a str> {
        match self {
            StreamAction::Copy => None,
            StreamAction::Encode { encoder, .. } => Some(encoder),
        }
    }

    /// `true` when the decision encodes with a MediaCodec encoder.
    pub fn uses_hardware(&self) -> bool {
        matches!(self, StreamAction::Encode { hardware: true, .. })
    }
}

/// Decision for one audio stream, fixed once per session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioAction<'a> {
    /// Pass the source audio through unchanged (MPEG-TS compatible codec).
    Copy,
    /// Re-encode with the preset's `audio_codec` (`aac`/`ac3`/`eac3`/`mp2`).
    Encode { encoder: &'a str },
}

impl<'a> AudioAction<'a> {
    /// `true` for [`AudioAction::Copy`].
    pub fn is_copy(&self) -> bool {
        matches!(self, AudioAction::Copy)
    }

    /// Encoder name when encoding, `None` for copy.
    pub fn encoder_name(&self) -> Option<&'a str> {
        match self {
            AudioAction::Copy => None,
            AudioAction::Encode { encoder } => Some(encoder),
        }
    }
}

/// Decide the video action for a source stream (contract `remux-modes` §3).
///
/// `target_codec_id` is the codec the encoder layer would produce right now:
/// the usable hardware encoder's output when there is one, otherwise the
/// software encoder's (`0` when neither is configured). The remux mode decides
/// whether that makes the source copyable; a codec MPEG-TS cannot carry is
/// always re-encoded, whatever the mode says.
///
/// `hw_encoder_available` is the caller's `avcodec_find_encoder_by_name` probe
/// plus a successful `avcodec_open2` attempt; presence alone is not proof that
/// the device codec works.
pub fn decide_video_action<'a>(
    preset: &'a TranscodePreset,
    source_codec_id: u32,
    target_codec_id: u32,
    hw_encoder_available: bool,
) -> Result<StreamAction<'a>, String> {
    let mode = RemuxMode::parse(&preset.video_remux)?;
    if crate::preset::is_ts_legal_video(source_codec_id)
        && mode.copies(source_codec_id, target_codec_id)
    {
        return Ok(StreamAction::Copy);
    }
    if !preset.hw_video_encoder.is_empty() && hw_encoder_available {
        return Ok(StreamAction::Encode {
            hardware: true,
            encoder: &preset.hw_video_encoder,
        });
    }
    if !preset.sw_video_encoder.is_empty() {
        return Ok(StreamAction::Encode {
            hardware: false,
            encoder: &preset.sw_video_encoder,
        });
    }
    Err(format!(
        "no video encoder available for preset {}",
        preset.id
    ))
}

/// Decide the audio action for a source stream (contract `remux-modes` §3).
///
/// Audio has no hardware layer, so `target_codec_id` is the codec
/// `preset.audio_codec` encodes to. Audio-only sources (no video stream) still
/// go through this same decision.
pub fn decide_audio_action<'a>(
    preset: &'a TranscodePreset,
    source_codec_id: u32,
    target_codec_id: u32,
) -> Result<AudioAction<'a>, String> {
    let mode = RemuxMode::parse(&preset.audio_remux)?;
    if crate::preset::is_ts_legal_audio(source_codec_id)
        && mode.copies(source_codec_id, target_codec_id)
    {
        return Ok(AudioAction::Copy);
    }
    Ok(AudioAction::Encode {
        encoder: &preset.audio_codec,
    })
}

/// `true` when a software fallback changes the encoder's output codec (the
/// shipped HEVC preset: MediaCodec H.265 → libx264 H.264, contract §2.1).
///
/// Callers must log `video: sw fallback downgrades hevc to h264` when this is
/// true and the software branch actually runs.
pub fn sw_fallback_downgrades_hevc(preset: &TranscodePreset) -> bool {
    match (
        crate::preset::video_encoder_output_codec_id(&preset.hw_video_encoder),
        crate::preset::video_encoder_output_codec_id(&preset.sw_video_encoder),
    ) {
        (Some(hw), Some(sw)) => hw != sw,
        _ => false,
    }
}

/// Whether the caller must probe the preset's hardware encoder before deciding.
///
/// `off` always has to choose an encoder layer, and `strict` compares the
/// source against that layer's output — both therefore depend on whether the
/// hardware encoder actually opens. `on` copies a legal source without looking
/// at the encoders, so it only probes when the source cannot be copied anyway.
pub fn hw_probe_needed(
    preset: &TranscodePreset,
    source_video_codec_id: u32,
) -> Result<bool, String> {
    if source_video_codec_id == 0 || preset.hw_video_encoder.is_empty() {
        return Ok(false);
    }
    Ok(match RemuxMode::parse(&preset.video_remux)? {
        RemuxMode::Off | RemuxMode::Strict => true,
        RemuxMode::On => !crate::preset::is_ts_legal_video(source_video_codec_id),
    })
}

// ── Video rate control (contract §4) ─────────────────────────────────────────

/// Video rate-control mode, one of `wdmm_core::models::RATE_CONTROL_MODES`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RateControl {
    /// Leave the encoder defaults alone: the bitrate is a plain ABR target and
    /// no `bitrate_mode`/quality option is injected.
    Auto,
    /// Variable bitrate: the bitrate is the average target.
    Vbr,
    /// Constant bitrate: the encoder holds the target continuously.
    Cbr,
    /// Constant quality: the quality value drives the encoder and the bitrate
    /// becomes a VBV upper bound.
    Cq,
}

impl RateControl {
    /// Parse the preset value, with the exact user-facing error message.
    pub fn parse(mode: &str) -> Result<Self, String> {
        match mode {
            "auto" => Ok(RateControl::Auto),
            "vbr" => Ok(RateControl::Vbr),
            "cbr" => Ok(RateControl::Cbr),
            "cq" => Ok(RateControl::Cq),
            other => Err(format!("unknown rate control: {other}")),
        }
    }

    /// Preset string form (round-trips through [`RateControl::parse`]).
    pub const fn as_str(self) -> &'static str {
        match self {
            RateControl::Auto => "auto",
            RateControl::Vbr => "vbr",
            RateControl::Cbr => "cbr",
            RateControl::Cq => "cq",
        }
    }
}

/// Encoder families whose `AVDictionary` option whitelist differs
/// (contract §3 and §8bis).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderFamily {
    /// MediaCodec encoders (`*_mediacodec`): Android-defined AVOptions.
    Hardware,
    /// libx264: the x264 preset ladder, CRF and `x264-params`.
    LibX264,
    /// MPEG-1/2/4 video encoders: MPEG quantiser and VBV knobs.
    MpegVideo,
}

impl EncoderFamily {
    /// Family for a named encoder; `None` when the name is not shipped.
    pub fn of(encoder: &str) -> Option<Self> {
        if encoder.ends_with("_mediacodec") {
            Some(EncoderFamily::Hardware)
        } else if encoder == "libx264" {
            Some(EncoderFamily::LibX264)
        } else if matches!(encoder, "mpeg1video" | "mpeg2video" | "mpeg4") {
            Some(EncoderFamily::MpegVideo)
        } else {
            None
        }
    }
}

/// `x264-params` keys the transcode plan owns. The plan injects them itself, so
/// the segment grid can never drift off the HLS plan (contract §13.5).
pub const INVARIANT_X264_PARAMS: &[&str] = &["keyint", "min-keyint", "scenecut"];

/// Build the `AVDictionary` for one video encoder.
///
/// Only two sources remain (contract `preset-editor-slimming` §2.1):
///
/// 1. the preset's structured rate-control fields (`video_rate_control`,
///    `video_quality`, the libx264 speed preset and the bitrate target);
/// 2. the invariants the HLS segment grid depends on.
///
/// The dictionary also pins the encoder's sample aspect ratio to 1/1: the plan
/// scales the *display* geometry into the envelope, so the encoded pixels have
/// to describe square pixels (contract §3.2).
///
/// Returns a loud error for an unknown encoder or a family mismatch instead of
/// silently dropping the rate control.
pub fn encoder_av_options(
    preset: &TranscodePreset,
    encoder: &str,
    hardware: bool,
    bitrate: u32,
    gop: i32,
) -> Result<Vec<(String, String)>, String> {
    let family = EncoderFamily::of(encoder).ok_or_else(|| format!("unknown encoder: {encoder}"))?;
    if hardware != (family == EncoderFamily::Hardware) {
        return Err(format!(
            "encoder {encoder} is not a {} encoder",
            if hardware { "hardware" } else { "software" }
        ));
    }
    let rate_control = RateControl::parse(&preset.video_rate_control)?;
    let quality = preset.video_quality;

    // ── Structured fields (rate control / preset ladder, contract §13.4) ────
    let mut structured: Vec<(String, String)> = Vec::new();
    let mut x264_structured: Vec<String> = Vec::new();
    match family {
        EncoderFamily::Hardware => match rate_control {
            RateControl::Auto => {}
            RateControl::Vbr => structured.push(("bitrate_mode".into(), "vbr".into())),
            RateControl::Cbr => structured.push(("bitrate_mode".into(), "cbr".into())),
            RateControl::Cq => {
                structured.push(("bitrate_mode".into(), "cq".into()));
                // MediaCodec quality is 0–100; the contract's 23 mirrors the
                // libx264 CRF default so one value means the same thing.
                structured.push(("global_quality".into(), quality.unwrap_or(23).to_string()));
            }
        },
        EncoderFamily::LibX264 => {
            if !preset.sw_video_preset.is_empty() {
                structured.push(("preset".into(), preset.sw_video_preset.clone()));
            }
            let kbps = (bitrate / 1000).max(1);
            match rate_control {
                RateControl::Auto | RateControl::Vbr => {}
                RateControl::Cbr => x264_structured.push(format!(
                    "nal-hrd=cbr:force-cfr=1:vbv-maxrate={kbps}:vbv-bufsize={}",
                    kbps.saturating_mul(2)
                )),
                RateControl::Cq => {
                    structured.push(("crf".into(), quality.unwrap_or(23).to_string()));
                    x264_structured.push(format!(
                        "vbv-maxrate={kbps}:vbv-bufsize={}",
                        kbps.saturating_mul(2)
                    ));
                }
            }
        }
        EncoderFamily::MpegVideo => match rate_control {
            RateControl::Auto | RateControl::Vbr => {}
            RateControl::Cbr => {
                structured.push(("minrate".into(), bitrate.to_string()));
                structured.push(("maxrate".into(), bitrate.to_string()));
                structured.push(("bufsize".into(), bitrate.saturating_mul(2).to_string()));
            }
            RateControl::Cq => {
                let quantiser = quality.unwrap_or(23);
                structured.push(("qmin".into(), quantiser.to_string()));
                structured.push(("qmax".into(), quantiser.to_string()));
            }
        },
    }

    // ── Invariants (highest priority) ───────────────────────────────────────
    let mut invariants: Vec<(String, String)> = Vec::new();
    match family {
        EncoderFamily::Hardware => invariants.push(("max_b_frames".into(), "0".into())),
        EncoderFamily::LibX264 => {
            let mut tokens = x264_structured;
            tokens.push(format!(
                "{}={gop}:{}={gop}:scenecut=0",
                INVARIANT_X264_PARAMS[0], INVARIANT_X264_PARAMS[1]
            ));
            invariants.push(("x264-params".into(), tokens.join(":")));
        }
        EncoderFamily::MpegVideo => {
            invariants.push(("max_b_frames".into(), "0".into()));
            // libavcodec refuses CLOSED_GOP while scene-change detection is on
            // ("set threshold to 1000000000").
            invariants.push(("sc_threshold".into(), "1000000000".into()));
        }
    }
    // The plan scales the display geometry itself, so the encoder emits square
    // pixels (contract §3.2).
    invariants.push(("aspect".into(), "1/1".into()));

    let mut entries = Vec::with_capacity(structured.len() + invariants.len());
    entries.extend(structured);
    entries.extend(invariants);
    Ok(entries)
}

/// `key=value` list for the plan diagnostics; `none` when the encoder keeps
/// every default.
pub fn options_line(entries: &[(String, String)]) -> String {
    if entries.is_empty() {
        return "none".to_string();
    }
    entries
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Default video bitrate used when the preset does not set one:
/// `width * height * fps * 0.10`, floored at 800 kbit/s.
pub fn default_video_bitrate(width: u32, height: u32, fps: f64) -> u32 {
    let fps = if fps.is_finite() && fps > 0.0 {
        fps
    } else {
        30.0
    };
    let derived = (f64::from(width) * f64::from(height) * fps * 0.10).round();
    (derived as u64).max(800_000) as u32
}

/// Effective video bitrate: the preset's hand-set ceiling, or the envelope
/// default derived from the output geometry. No tier factor is applied
/// (contract §1).
pub fn target_video_bitrate(preset: &TranscodePreset, width: u32, height: u32, fps: f64) -> u32 {
    preset
        .video_max_bitrate
        .unwrap_or_else(|| default_video_bitrate(width, height, fps))
        .max(1)
}

/// One-line diag for the decided video layer (contract §13.4): emitted once per
/// plan and carrying the *actual* rate-control options that reach
/// `avcodec_open2`.
pub fn video_plan_diag(
    action: StreamAction<'_>,
    bitrate: u32,
    rate_control: RateControl,
    options: &[(String, String)],
) -> String {
    match action {
        StreamAction::Copy => format!(
            "video plan: action=copy encoder=none rate_control={} bitrate={bitrate} options=none",
            rate_control.as_str()
        ),
        StreamAction::Encode { hardware, encoder } => format!(
            "video plan: action={} encoder={encoder} rate_control={} bitrate={bitrate} options={}",
            if hardware { "hw" } else { "sw" },
            rate_control.as_str(),
            options_line(options)
        ),
    }
}

// ── Display geometry (contract `preset-editor-slimming` §3.2) ───────────────

/// Pixel width a sample aspect ratio expands the stored width to.
///
/// An anamorphic source (DVD 720×576 SAR 64:45) stores 720 pixels that are
/// meant to be shown 1024 wide. Returns `width` unchanged for a missing or
/// invalid SAR so a plain square-pixel source cannot regress.
pub fn display_width(width: i32, sar: AVRational) -> i32 {
    if width <= 0 || sar.num <= 0 || sar.den <= 0 {
        return width;
    }
    let scaled = i64::from(width) * i64::from(sar.num);
    let width = (scaled + i64::from(sar.den) / 2) / i64::from(sar.den);
    width.clamp(0, i64::from(i32::MAX)) as i32
}

/// Output pixel size for a source with `sar`, capped by the preset envelope.
///
/// The *display* geometry (display width × stored height) is fitted into the
/// envelope first, so a 720×576 SAR 64:45 source (1024×576 displayed) never
/// overflows a 640×480 cap. When the display geometry already fits, the stored
/// pixels are kept as they are — the cheap fix avoids upscaling and keeps the
/// pixel count of ordinary sources untouched (contract §3.2).
pub fn fit_display_dimensions(
    src_w: i32,
    src_h: i32,
    sar: AVRational,
    max_w: Option<i32>,
    max_h: Option<i32>,
) -> (i32, i32) {
    if src_w <= 0 || src_h <= 0 {
        return (src_w, src_h);
    }
    let display_w = display_width(src_w, sar).max(1);
    let fits = max_w.is_none_or(|m| display_w <= m) && max_h.is_none_or(|m| src_h <= m);
    if fits {
        return (even(src_w), even(src_h));
    }
    let scale_w = max_w.map_or(f64::INFINITY, |m| f64::from(m) / f64::from(display_w));
    let scale_h = max_h.map_or(f64::INFINITY, |m| f64::from(m) / f64::from(src_h));
    let scale = scale_w.min(scale_h).clamp(0.0, 1.0);
    let fitted_w = ((f64::from(display_w) * scale).round() as i32).max(2);
    let fitted_h = ((f64::from(src_h) * scale).round() as i32).max(2);
    if fitted_w <= src_w && fitted_h <= src_h {
        return (even(fitted_w), even(fitted_h));
    }
    // The envelope wants fewer display pixels than the source stores (e.g. a
    // 720-wide source displayed 1024 wide into a 640-wide cap): clamp the
    // output to the source width and keep the display aspect.
    let out_w = even(src_w.min(fitted_w).max(2));
    let out_h =
        ((f64::from(src_h) * f64::from(out_w) / f64::from(display_w)).round() as i32).max(2);
    (out_w, even(out_h))
}

/// Round up to the nearest even pixel count (4:2:0-safe encoder sizes).
fn even(v: i32) -> i32 {
    let v = v.max(2);
    v + (v & 1)
}

// ── Encoder catalog for the preset editor (contract §2) ─────────────────────

/// One selectable video encoder the preset editor renders (contract §2).
///
/// This is the single source of truth for encoder ids, labels, output codecs
/// and the rate-control modes each one accepts; Dart renders it and must not
/// hard-code encoder names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct VideoEncoderInfo {
    /// Encoder name stored in `hw_video_encoder`/`sw_video_encoder`.
    pub id: &'static str,
    /// Chinese label rendered in the editor.
    pub label: &'static str,
    /// `true` for MediaCodec (`hw_video_encoder`), `false` for software.
    pub hardware: bool,
    /// Codec the encoder produces (`h264`/`hevc`/`mpeg4`/`mpeg2video`).
    pub output_codec: &'static str,
    /// Rate-control modes this encoder accepts, in menu order.
    pub rate_control: &'static [RateControl],
}

/// Rate-control modes every catalog encoder accepts (contract §2).
const CATALOG_RATE_CONTROL: &[RateControl] = &[RateControl::Cq, RateControl::Vbr, RateControl::Cbr];

/// Every video encoder the preset editor may select (contract §2).
///
/// Orders hardware first, then software, matching the editor's grouping.
pub fn video_encoder_catalog() -> &'static [VideoEncoderInfo] {
    static CATALOG: [VideoEncoderInfo; 6] = [
        VideoEncoderInfo {
            id: "h264_mediacodec",
            label: "H.264 硬编",
            hardware: true,
            output_codec: "h264",
            rate_control: CATALOG_RATE_CONTROL,
        },
        VideoEncoderInfo {
            id: "hevc_mediacodec",
            label: "H.265 硬编",
            hardware: true,
            output_codec: "hevc",
            rate_control: CATALOG_RATE_CONTROL,
        },
        VideoEncoderInfo {
            id: "mpeg4_mediacodec",
            label: "MPEG-4 硬编",
            hardware: true,
            output_codec: "mpeg4",
            rate_control: CATALOG_RATE_CONTROL,
        },
        VideoEncoderInfo {
            id: "libx264",
            label: "x264（软件）",
            hardware: false,
            output_codec: "h264",
            rate_control: CATALOG_RATE_CONTROL,
        },
        VideoEncoderInfo {
            id: "mpeg2video",
            label: "MPEG-2（软件）",
            hardware: false,
            output_codec: "mpeg2video",
            rate_control: CATALOG_RATE_CONTROL,
        },
        VideoEncoderInfo {
            id: "mpeg4",
            label: "MPEG-4（软件）",
            hardware: false,
            output_codec: "mpeg4",
            rate_control: CATALOG_RATE_CONTROL,
        },
    ];
    &CATALOG
}
/// Video encoders the Android FFmpeg build must ship: the explicit
/// `--enable-encoder` list of `scripts/build-ffmpeg-android.sh` (contract
/// §13.1/§13.6).
///
/// `mpeg1video` is deliberately in the list: the software encoder set is
/// `libx264`, `mpeg1video`, `mpeg2video`, `mpeg4` (§8bis), so a trim build that
/// drops one of them must fail the host gate test instead of greying out an
/// editor option on a phone.
pub const SHIPPED_VIDEO_ENCODERS: &[&str] = &[
    "libx264",
    "h264_mediacodec",
    "hevc_mediacodec",
    "mpeg4_mediacodec",
    "mpeg1video",
    "mpeg2video",
    "mpeg4",
];

/// Audio encoders the Android FFmpeg build must ship (contract §13.1/§13.6).
pub const SHIPPED_AUDIO_ENCODERS: &[&str] = &["aac", "ac3", "eac3", "mp2"];

/// Frame rate to configure on the video encoder.
///
/// Uses the source stream rate when it is sane and not above the preset cap;
/// otherwise falls back to the cap (or 30 fps). Returns `(num, den)`.
pub fn encoder_frame_rate(input_num: i32, input_den: i32, fps_cap: Option<u32>) -> (i32, i32) {
    let sane_source = (input_num > 0 && input_den > 0).then(|| input_num as f64 / input_den as f64);
    let cap = fps_cap.filter(|c| *c > 0);
    if let Some(fps) = sane_source {
        if (1.0..=240.0).contains(&fps) && cap.is_none_or(|c| fps <= c as f64) {
            return (input_num, input_den);
        }
    }
    let base = match sane_source {
        Some(fps) if (1.0..=240.0).contains(&fps) => fps,
        _ => 30.0,
    };
    let capped = cap.map_or(base, |c| base.min(c as f64)).max(1.0);
    (capped.round() as i32, 1)
}

/// GOP size (frames) for a hardware encoder.
///
/// `mediacodecenc.c` derives the I-frame interval from
/// `round(gop_size / fps)` and warns when it rounds to 0, so keep at least two
/// seconds between keyframes (MediaCodec default behaviour would be one).
pub fn hw_gop_size(fps_num: i32, fps_den: i32) -> i32 {
    if fps_num <= 0 || fps_den <= 0 {
        return 60;
    }
    let fps = fps_num as f64 / fps_den as f64;
    ((2.0 * fps).round() as i32).max(1)
}

/// Encoder GOP size (frames) for a segment boundary: `round(seg_secs * fps)`.
///
/// The exact transcode grid assumes a keyframe on every `k * seg_secs` boundary,
/// and the HLS muxer only cuts encoded streams on keyframes. Setting the
/// encoder's GOP to exactly one segment worth of frames — with B-frames off and
/// scene-cut keyframes disabled — makes the produced `EXTINF` values match
/// [`exact_plan`](crate::vod::exact_plan).
///
/// Returns at least one frame; a non-positive frame rate falls back to 30 fps and
/// pathological inputs are capped at one hour of frames (no 32-bit overflow).
pub fn segment_gop_size(fps_num: i32, fps_den: i32, seg_secs: u32) -> i32 {
    let fps = if fps_num > 0 && fps_den > 0 {
        fps_num as f64 / fps_den as f64
    } else {
        30.0
    };
    let frames = (f64::from(seg_secs.max(1)) * fps).round();
    (frames as i64).clamp(1, 60 * 60) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preset::{codec_ids, preset_h264, preset_hevc};

    #[test]
    fn remux_modes_parse_with_the_models_tokens() {
        for mode in wdmm_core::models::REMUX_MODES {
            assert_eq!(RemuxMode::parse(mode).unwrap().as_str(), mode);
        }
        assert_eq!(RemuxMode::parse("off"), Ok(RemuxMode::Off));
        assert_eq!(RemuxMode::parse("strict"), Ok(RemuxMode::Strict));
        assert_eq!(RemuxMode::parse("on"), Ok(RemuxMode::On));
        assert_eq!(
            RemuxMode::parse("hybrid").unwrap_err(),
            "unknown remux mode: hybrid"
        );
    }

    #[test]
    fn video_remux_modes_cover_every_source_class() {
        // (mode, source codec, runtime target codec, must copy)
        let cases = [
            ("off", codec_ids::H264, codec_ids::H264, false),
            ("off", codec_ids::H264, codec_ids::MPEG4, false),
            ("strict", codec_ids::H264, codec_ids::H264, true),
            ("strict", codec_ids::H264, codec_ids::HEVC, false),
            ("strict", codec_ids::H264, 0, false),
            ("on", codec_ids::H264, codec_ids::HEVC, true),
            // A codec MPEG-TS cannot carry is never copied, not even with `on`.
            ("on", VP9_ID, codec_ids::H264, false),
        ];
        for (mode, source, target, copy) in cases {
            let mut preset = preset_h264();
            preset.video_remux = mode.to_string();
            let action = decide_video_action(&preset, source, target, false).unwrap();
            assert_eq!(
                action.is_copy(),
                copy,
                "mode={mode} source={source} target={target}"
            );
        }
    }

    #[test]
    fn audio_remux_modes_cover_every_source_class() {
        // (mode, source codec, runtime target codec, must copy)
        let cases = [
            ("off", codec_ids::AAC, codec_ids::AAC, false),
            ("strict", codec_ids::AAC, codec_ids::AAC, true),
            ("strict", codec_ids::AAC, codec_ids::AC3, false),
            ("strict", codec_ids::AAC, 0, false),
            ("on", codec_ids::MP3, codec_ids::AAC, true),
            // Opus is not legal in MPEG-TS, so it is re-encoded even with `on`.
            ("on", OPUS_ID, codec_ids::AAC, false),
        ];
        for (mode, source, target, copy) in cases {
            let mut preset = preset_h264();
            preset.audio_remux = mode.to_string();
            let action = decide_audio_action(&preset, source, target).unwrap();
            assert_eq!(
                action.is_copy(),
                copy,
                "mode={mode} source={source} target={target}"
            );
        }
    }

    #[test]
    fn invalid_remux_modes_are_loud() {
        let mut preset = preset_h264();
        preset.video_remux = "hybrid".to_string();
        assert_eq!(
            decide_video_action(&preset, codec_ids::H264, codec_ids::H264, false).unwrap_err(),
            "unknown remux mode: hybrid"
        );
        preset.video_remux = "on".to_string();
        preset.audio_remux = "sometimes".to_string();
        assert_eq!(
            decide_audio_action(&preset, codec_ids::AAC, codec_ids::AAC).unwrap_err(),
            "unknown remux mode: sometimes"
        );
    }

    // Codec not modelled by the preset layer; the value is only used by these
    // tests (FFmpeg 9.0 numbering, see `preset::codec_ids`).
    const VP9_ID: u32 = 166;
    const OPUS_ID: u32 = 86076;

    #[test]
    fn hw_probe_is_needed_only_when_the_mode_depends_on_the_encoder_layer() {
        let mut preset = preset_h264();
        // No video stream: there is nothing to encode or probe.
        assert!(!hw_probe_needed(&preset, 0).unwrap());
        // No hardware encoder configured: the software layer is the only choice.
        let mut no_hw = preset_h264();
        no_hw.hw_video_encoder = String::new();
        assert!(!hw_probe_needed(&no_hw, VP9_ID).unwrap());
        // `off` always picks an encoder layer and `strict` compares against that
        // layer's output, so both probe even for a copyable source.
        preset.video_remux = "off".to_string();
        assert!(hw_probe_needed(&preset, codec_ids::H264).unwrap());
        assert!(hw_probe_needed(&preset, VP9_ID).unwrap());
        preset.video_remux = "strict".to_string();
        assert!(hw_probe_needed(&preset, codec_ids::H264).unwrap());
        // `on` copies a legal source without looking at the encoders and only
        // probes when the source has to be re-encoded anyway.
        preset.video_remux = "on".to_string();
        assert!(!hw_probe_needed(&preset, codec_ids::H264).unwrap());
        assert!(hw_probe_needed(&preset, VP9_ID).unwrap());
        // An invalid mode is reported instead of silently choosing a layer.
        preset.video_remux = "hybrid".to_string();
        assert_eq!(
            hw_probe_needed(&preset, codec_ids::H264).unwrap_err(),
            "unknown remux mode: hybrid"
        );
    }

    #[test]
    fn stream_selection_maps_best_streams() {
        assert_eq!(remux_stream_selection(0, 1), (Some(0), Some(1)));
        assert_eq!(remux_stream_selection(-1, 1), (None, Some(1)));
        assert_eq!(remux_stream_selection(2, -1), (Some(2), None));
        assert_eq!(remux_stream_selection(-1, -1), (None, None));
    }

    #[test]
    fn stream_selection_drops_everything_else() {
        assert_eq!(remux_dropped_streams(0, 1, 5), vec![2, 3, 4]);
        assert_eq!(remux_dropped_streams(-1, 1, 3), vec![0, 2]);
        assert_eq!(remux_dropped_streams(0, -1, 2), vec![1]);
        assert_eq!(remux_dropped_streams(-1, -1, 2), vec![0, 1]);
        assert!(remux_dropped_streams(0, 1, 2).is_empty());
    }

    #[test]
    fn four_quadrants_are_decidable() {
        // The built-ins default to remux `on`, so a legal source copies both
        // streams; the runtime target only matters for `strict`.
        let preset = preset_h264();
        assert_eq!(
            decide_video_action(&preset, codec_ids::H264, codec_ids::H264, true),
            Ok(StreamAction::Copy)
        );
        assert_eq!(
            decide_audio_action(&preset, codec_ids::AAC, codec_ids::AAC),
            Ok(AudioAction::Copy)
        );

        // copy + encode: video copies, Opus audio must be re-encoded.
        assert_eq!(
            decide_video_action(&preset, codec_ids::H264, codec_ids::H264, true),
            Ok(StreamAction::Copy)
        );
        assert_eq!(
            decide_audio_action(&preset, OPUS_ID, codec_ids::AAC),
            Ok(AudioAction::Encode { encoder: "aac" })
        );

        // encode + copy: VP9 video cannot be copied, AAC audio can.
        assert_eq!(
            decide_video_action(&preset, VP9_ID, codec_ids::H264, true),
            Ok(StreamAction::Encode {
                hardware: true,
                encoder: "h264_mediacodec",
            })
        );
        assert_eq!(
            decide_audio_action(&preset, codec_ids::AAC, codec_ids::AAC),
            Ok(AudioAction::Copy)
        );

        // encode + encode: VP9 video + Opus audio both re-encoded.
        assert_eq!(
            decide_video_action(&preset, VP9_ID, codec_ids::H264, false),
            Ok(StreamAction::Encode {
                hardware: false,
                encoder: "libx264",
            })
        );
        assert_eq!(
            decide_audio_action(&preset, OPUS_ID, codec_ids::AAC),
            Ok(AudioAction::Encode { encoder: "aac" })
        );
    }

    #[test]
    fn remux_off_and_hardware_unavailable_fall_through_to_software() {
        let mut preset = preset_h264();
        preset.video_remux = "off".to_string();
        preset.audio_remux = "off".to_string();
        assert_eq!(
            decide_video_action(&preset, codec_ids::H264, codec_ids::H264, true),
            Ok(StreamAction::Encode {
                hardware: true,
                encoder: "h264_mediacodec",
            })
        );
        assert_eq!(
            decide_video_action(&preset, codec_ids::H264, codec_ids::H264, false),
            Ok(StreamAction::Encode {
                hardware: false,
                encoder: "libx264",
            })
        );
        assert_eq!(
            decide_audio_action(&preset, codec_ids::AAC, codec_ids::AAC),
            Ok(AudioAction::Encode { encoder: "aac" })
        );
    }

    #[test]
    fn no_usable_layer_is_a_loud_error() {
        let mut preset = preset_h264();
        preset.video_remux = "off".to_string();
        preset.hw_video_encoder = String::new();
        preset.sw_video_encoder = String::new();
        let err = decide_video_action(&preset, VP9_ID, codec_ids::H264, true).unwrap_err();
        assert_eq!(err, "no video encoder available for preset h264");
    }

    #[test]
    fn sw_fallback_downgrades_hevc_only_when_the_output_codec_changes() {
        // hevc preset: MediaCodec H.265 vs libx264 H.264.
        assert!(sw_fallback_downgrades_hevc(&preset_hevc()));
        // h264 preset: both layers produce H.264, so falling back is invisible.
        assert!(!sw_fallback_downgrades_hevc(&preset_h264()));
        // No hardware encoder configured: there is no promised codec to lose.
        let mut no_hw = preset_hevc();
        no_hw.hw_video_encoder = String::new();
        assert!(!sw_fallback_downgrades_hevc(&no_hw));
    }
    #[test]
    fn rate_control_parses_the_four_modes() {
        for mode in wdmm_core::models::RATE_CONTROL_MODES {
            assert_eq!(RateControl::parse(mode).unwrap().as_str(), mode);
        }
        assert_eq!(RateControl::parse("auto"), Ok(RateControl::Auto));
        assert_eq!(RateControl::parse("vbr"), Ok(RateControl::Vbr));
        assert_eq!(RateControl::parse("cbr"), Ok(RateControl::Cbr));
        assert_eq!(RateControl::parse("cq"), Ok(RateControl::Cq));
        assert_eq!(
            RateControl::parse("turbo").unwrap_err(),
            "unknown rate control: turbo"
        );
    }

    /// Final value of one option key (last writer wins).
    fn entry<'a>(entries: &'a [(String, String)], key: &str) -> Option<&'a str> {
        entries
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn libx264_options_carry_the_speed_preset_and_the_gop_invariant() {
        let preset = preset_h264();
        let options = encoder_av_options(&preset, "libx264", false, 4_000_000, 120).unwrap();
        assert_eq!(entry(&options, "preset"), Some("veryfast"));
        // vbr leaves the encoder's own rate control alone: no crf, no vbv.
        assert_eq!(entry(&options, "crf"), None);
        assert_eq!(
            entry(&options, "x264-params"),
            Some("keyint=120:min-keyint=120:scenecut=0")
        );
        // The plan re-scales the display geometry itself, so the pixels are
        // square and the segment grid stays untouched.
        assert_eq!(entry(&options, "aspect"), Some("1/1"));
    }

    #[test]
    fn cbr_and_cq_map_to_encoder_specific_knobs() {
        let mut preset = preset_h264();
        preset.video_rate_control = "cbr".into();
        let options = encoder_av_options(&preset, "libx264", false, 4_000_000, 120).unwrap();
        let params = entry(&options, "x264-params").unwrap();
        assert!(params.contains("nal-hrd=cbr:force-cfr=1"));
        // 4 Mbps target → 4000 kbit/s HRD at a 2 s buffer.
        assert!(params.contains("vbv-maxrate=4000:vbv-bufsize=8000"));
        assert!(params.ends_with("keyint=120:min-keyint=120:scenecut=0"));

        // cq → CRF (default 23) with the bitrate as a VBV bound.
        preset.video_rate_control = "cq".into();
        let options = encoder_av_options(&preset, "libx264", false, 4_000_000, 120).unwrap();
        assert_eq!(entry(&options, "crf"), Some("23"));
        assert!(entry(&options, "x264-params")
            .unwrap()
            .contains("vbv-maxrate=4000"));
        // The preset's quality value replaces the default CRF.
        preset.video_quality = Some(18);
        let options = encoder_av_options(&preset, "libx264", false, 4_000_000, 120).unwrap();
        assert_eq!(entry(&options, "crf"), Some("18"));

        // MPEG-2 cbr → explicit RC window instead of a bitrate_mode option.
        preset.video_rate_control = "cbr".into();
        let options = encoder_av_options(&preset, "mpeg2video", false, 3_000_000, 60).unwrap();
        assert_eq!(entry(&options, "minrate"), Some("3000000"));
        assert_eq!(entry(&options, "maxrate"), Some("3000000"));
        assert_eq!(entry(&options, "bufsize"), Some("6000000"));
        assert_eq!(entry(&options, "max_b_frames"), Some("0"));
        assert_eq!(entry(&options, "sc_threshold"), Some("1000000000"));

        // MPEG-2 cq → a fixed quantiser, still pinned to a constant GOP.
        preset.video_rate_control = "cq".into();
        preset.video_quality = Some(4);
        let options = encoder_av_options(&preset, "mpeg2video", false, 3_000_000, 60).unwrap();
        assert_eq!(entry(&options, "qmin"), Some("4"));
        assert_eq!(entry(&options, "qmax"), Some("4"));
        assert_eq!(entry(&options, "sc_threshold"), Some("1000000000"));
    }

    #[test]
    fn hardware_options_inject_the_rate_mode_and_pin_b_frames() {
        let mut preset = preset_h264();
        preset.video_rate_control = "cq".into();
        let options = encoder_av_options(&preset, "h264_mediacodec", true, 4_000_000, 60).unwrap();
        assert_eq!(entry(&options, "bitrate_mode"), Some("cq"));
        assert_eq!(entry(&options, "global_quality"), Some("23"));
        // The invariant wins: MediaCodec must never reorder frames.
        assert_eq!(entry(&options, "max_b_frames"), Some("0"));
        assert_eq!(entry(&options, "aspect"), Some("1/1"));
    }

    #[test]
    fn unknown_encoders_and_family_mismatches_are_loud() {
        let clean = preset_h264();
        assert_eq!(
            encoder_av_options(&clean, "libx264", true, 1_000_000, 60).unwrap_err(),
            "encoder libx264 is not a hardware encoder"
        );
        assert_eq!(
            encoder_av_options(&clean, "h264_mediacodec", false, 1_000_000, 60).unwrap_err(),
            "encoder h264_mediacodec is not a software encoder"
        );
        assert_eq!(
            encoder_av_options(&clean, "x265", false, 1_000_000, 60).unwrap_err(),
            "unknown encoder: x265"
        );
    }

    #[test]
    fn target_bitrate_uses_envelope_or_derives_with_a_floor() {
        let preset = preset_h264();
        // Explicit envelope (4 Mbps) passes through untouched: no tier factor.
        assert_eq!(target_video_bitrate(&preset, 1920, 1080, 30.0), 4_000_000);
        // No envelope: 320x180@15 derives 86_400 → floored to 800_000.
        let mut bare = preset.clone();
        bare.video_max_bitrate = None;
        assert_eq!(target_video_bitrate(&bare, 320, 180, 15.0), 800_000);
        assert_eq!(default_video_bitrate(320, 180, 15.0), 800_000);
        // 1920x1080@30 derives 6_220_800.
        assert_eq!(default_video_bitrate(1920, 1080, 30.0), 6_220_800);
        assert_eq!(target_video_bitrate(&bare, 1920, 1080, 30.0), 6_220_800);
    }

    #[test]
    fn video_plan_diag_spells_out_the_effective_options() {
        let copy = video_plan_diag(StreamAction::Copy, 4_000_000, RateControl::Vbr, &[]);
        assert_eq!(
            copy,
            "video plan: action=copy encoder=none rate_control=vbr bitrate=4000000 options=none"
        );
        let hw_options =
            encoder_av_options(&preset_hevc(), "hevc_mediacodec", true, 3_000_000, 60).unwrap();
        let hw = video_plan_diag(
            StreamAction::Encode {
                hardware: true,
                encoder: "hevc_mediacodec",
            },
            3_000_000,
            RateControl::Vbr,
            &hw_options,
        );
        assert_eq!(
            hw,
            "video plan: action=hw encoder=hevc_mediacodec rate_control=vbr bitrate=3000000 \
             options=bitrate_mode=vbr,max_b_frames=0,aspect=1/1"
        );
        let sw_options =
            encoder_av_options(&preset_h264(), "libx264", false, 1_000_000, 60).unwrap();
        let sw = video_plan_diag(
            StreamAction::Encode {
                hardware: false,
                encoder: "libx264",
            },
            1_000_000,
            RateControl::Vbr,
            &sw_options,
        );
        assert!(sw.starts_with(
            "video plan: action=sw encoder=libx264 rate_control=vbr bitrate=1000000 \
             options=preset=veryfast,x264-params="
        ));
    }

    #[test]
    fn catalog_lists_the_six_editor_encoders() {
        let catalog = video_encoder_catalog();
        assert_eq!(catalog.len(), 6);
        let expected = [
            ("h264_mediacodec", "H.264 硬编", true, "h264"),
            ("hevc_mediacodec", "H.265 硬编", true, "hevc"),
            ("mpeg4_mediacodec", "MPEG-4 硬编", true, "mpeg4"),
            ("libx264", "x264（软件）", false, "h264"),
            ("mpeg2video", "MPEG-2（软件）", false, "mpeg2video"),
            ("mpeg4", "MPEG-4（软件）", false, "mpeg4"),
        ];
        for (codec, (id, label, hardware, output_codec)) in catalog.iter().zip(expected) {
            assert_eq!(codec.id, id);
            assert_eq!(codec.label, label);
            assert_eq!(codec.hardware, hardware);
            assert_eq!(codec.output_codec, output_codec);
            assert_eq!(
                codec.rate_control,
                &[RateControl::Cq, RateControl::Vbr, RateControl::Cbr]
            );
            // The catalog and the transcode path must agree on the family.
            assert_eq!(
                EncoderFamily::of(codec.id),
                Some(if hardware {
                    EncoderFamily::Hardware
                } else if id == "libx264" {
                    EncoderFamily::LibX264
                } else {
                    EncoderFamily::MpegVideo
                })
            );
            assert!(SHIPPED_VIDEO_ENCODERS.contains(&codec.id));
        }
        // Hardware first, then software: the editor groups them in this order.
        assert!(catalog[..3].iter().all(|c| c.hardware));
        assert!(catalog[3..].iter().all(|c| !c.hardware));
    }

    #[test]
    fn anamorphic_source_is_scaled_by_display_width() {
        let anamorphic = AVRational::new(64, 45);
        // DVD 720x576 SAR 64:45 is shown 1024 wide.
        assert_eq!(display_width(720, anamorphic), 1024);
        // Square pixels or a missing SAR never change the width.
        assert_eq!(display_width(720, AVRational::new(1, 1)), 720);
        assert_eq!(display_width(720, AVRational::new(0, 1)), 720);
        assert_eq!(display_width(720, AVRational::new(1, 90)), 8);
        // A 1920x1080 envelope is wider than the display geometry: the stored
        // pixels are kept (no upscale, no needless re-scale).
        assert_eq!(
            fit_display_dimensions(720, 576, anamorphic, Some(1920), Some(1080)),
            (720, 576)
        );
        // A 640x480 envelope is narrower than the 1024-pixel display width:
        // scale by the display width, not by the stored 720 pixels.
        assert_eq!(
            fit_display_dimensions(720, 576, anamorphic, Some(640), Some(480)),
            (640, 360)
        );
        // An intermediate cap keeps the display aspect (1024:576 → 16:9) and
        // the output stays 4:2:0-safe (even height).
        assert_eq!(
            fit_display_dimensions(720, 576, anamorphic, Some(800), Some(600)),
            (720, 406)
        );
        // Square-pixel sources keep the plain envelope behaviour.
        assert_eq!(
            fit_display_dimensions(1920, 1080, AVRational::new(1, 1), Some(1280), Some(720)),
            (1280, 720)
        );
        assert_eq!(
            fit_display_dimensions(320, 240, AVRational::new(0, 0), None, None),
            (320, 240)
        );
    }

    #[test]
    fn shipped_lists_cover_the_builtin_presets_and_the_android_manifest() {
        // Every encoder a built-in preset can pick must be in the trim build.
        for preset in crate::preset::all_builtin_presets() {
            assert!(
                SHIPPED_VIDEO_ENCODERS.contains(&preset.hw_video_encoder.as_str()),
                "{} missing from the shipped manifest",
                preset.hw_video_encoder
            );
            assert!(
                SHIPPED_VIDEO_ENCODERS.contains(&preset.sw_video_encoder.as_str()),
                "{} missing from the shipped manifest",
                preset.sw_video_encoder
            );
            assert!(
                SHIPPED_AUDIO_ENCODERS.contains(&preset.audio_codec.as_str()),
                "{} missing from the shipped manifest",
                preset.audio_codec
            );
        }
        // The software set of §8bis: mpeg1video is shipped even though no
        // preset selects it yet, because the preset editor may.
        for encoder in ["libx264", "mpeg1video", "mpeg2video", "mpeg4"] {
            assert!(SHIPPED_VIDEO_ENCODERS.contains(&encoder));
            assert!(EncoderFamily::of(encoder).is_some());
        }
        for encoder in ["aac", "ac3", "eac3", "mp2"] {
            assert!(SHIPPED_AUDIO_ENCODERS.contains(&encoder));
        }
    }

    #[test]
    fn encoder_frame_rate_keeps_sane_source_and_respects_cap() {
        // 29.97 fps source, no cap → exact source rational.
        assert_eq!(encoder_frame_rate(30000, 1001, None), (30000, 1001));
        // 60 fps source with a 30 fps cap → 30/1.
        assert_eq!(encoder_frame_rate(60, 1, Some(30)), (30, 1));
        // Unknown/missing source rate → 30 fps default, cap still honoured.
        assert_eq!(encoder_frame_rate(0, 1, None), (30, 1));
        assert_eq!(encoder_frame_rate(0, 0, Some(24)), (24, 1));
        // Nonsense source (e.g. 1000 fps) falls back to the default.
        assert_eq!(encoder_frame_rate(1000, 1, None), (30, 1));
    }

    #[test]
    fn hardware_gop_is_two_seconds() {
        assert_eq!(hw_gop_size(30, 1), 60);
        assert_eq!(hw_gop_size(30000, 1001), 60);
        assert_eq!(hw_gop_size(60, 1), 120);
        // Degenerate input rate keeps a usable 2 s GOP at 30 fps.
        assert_eq!(hw_gop_size(0, 0), 60);
    }

    #[test]
    fn segment_gop_is_one_segment_worth_of_frames() {
        // 30 fps × 4 s segments → a keyframe every 120 frames.
        assert_eq!(segment_gop_size(30, 1, 4), 120);
        // 23.976 fps × 4 s → 96 frames (rounded).
        assert_eq!(segment_gop_size(24000, 1001, 4), 96);
        // 25 fps × 2 s → 50 frames.
        assert_eq!(segment_gop_size(25, 1, 2), 50);
        // Degenerate input rate defaults to 30 fps; zero seconds means one frame.
        assert_eq!(segment_gop_size(0, 0, 4), 120);
        assert_eq!(segment_gop_size(30, 1, 0), 30);
    }
}
