//! Pure remux / copy / encode decision logic for the three-layer preset model.
//!
//! These helpers involve no libav calls, so they compile (and are unit-tested)
//! on a host build without the `ffmpeg` feature — the Windows host has no
//! libav, which is exactly where the decision rules must be provable.
//!
//! The model is (see `preset-rate-control-and-codec-probe` contract §1–§4):
//!
//! 1. copy the source stream when MPEG-TS can carry it and the preset allows it;
//! 2. otherwise encode with the preset's MediaCodec (hardware) encoder;
//! 3. otherwise encode with the preset's software encoder.
//!
//! [`decide_video_action`] / [`decide_audio_action`] implement that order for
//! one stream. The caller decides each stream once per session and caches the
//! result — reopen/seek restarts must reuse the same decision.
//!
//! [`video_encoder_options`] then turns the preset's structured rate-control
//! fields plus its advanced option maps into the exact `AVDictionary` handed to
//! `avcodec_open2`, with the contract §13.5 priority (invariants > advanced
//! options > structured fields). [`encoder_options_schema`] is the same
//! whitelist rendered for the preset editor, so UI and transcoder cannot drift.

use wdmm_core::models::TranscodePreset;

/// What the cheap remux stage should do for the selected best streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemuxPlan {
    /// Video and audio can both be copied verbatim (`-c copy`).
    Remux,
    /// Video is copied verbatim, audio is decoded/re-encoded (e.g. HEVC+Opus).
    Hybrid,
    /// Video cannot be copied; the caller falls back to the full transcode path.
    Transcode,
}

/// Decide the remux-stage plan from the best video/audio codec ids.
///
/// Audio-only sources (music, no video stream) keep the cheap path too: the
/// audio track is copied when MPEG-TS can carry it, and otherwise only the
/// audio is re-encoded — the hybrid machinery already does exactly that and
/// simply has no video stream to copy alongside.
pub fn remux_plan(video_codec_id: Option<u32>, audio_codec_id: Option<u32>) -> RemuxPlan {
    let Some(video_id) = video_codec_id else {
        return match audio_codec_id {
            Some(audio_id) if crate::preset::is_mpegts_audio_copy_compatible(audio_id) => {
                RemuxPlan::Remux
            }
            Some(_) => RemuxPlan::Hybrid,
            None => RemuxPlan::Transcode,
        };
    };
    if !crate::preset::is_mpegts_video_copy_compatible(video_id) {
        return RemuxPlan::Transcode;
    }
    match audio_codec_id {
        None => RemuxPlan::Remux,
        Some(audio_id) if crate::preset::is_mpegts_audio_copy_compatible(audio_id) => {
            RemuxPlan::Remux
        }
        Some(_) => RemuxPlan::Hybrid,
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

/// Decide the video action for a source stream (contract §2).
///
/// Order: copy (when allowed and MPEG-TS compatible) → hardware encode (when
/// configured *and* the device encoder is available) → software encode →
/// error. `hw_encoder_available` is the caller's `avcodec_find_encoder_by_name`
/// probe plus a successful `avcodec_open2` attempt; presence alone is not proof
/// that the device codec works.
pub fn decide_video_action<'a>(
    preset: &'a TranscodePreset,
    source_codec_id: u32,
    hw_encoder_available: bool,
) -> Result<StreamAction<'a>, String> {
    if preset.video_copy && crate::preset::is_mpegts_video_copy_compatible(source_codec_id) {
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

/// Decide the audio action for a source stream (contract §2).
///
/// Audio has no hardware encoder: copy when allowed and MPEG-TS compatible,
/// otherwise encode with the preset's `audio_codec`. Audio-only sources (no
/// video stream) still go through this same decision.
pub fn decide_audio_action<'a>(
    preset: &'a TranscodePreset,
    source_codec_id: u32,
) -> AudioAction<'a> {
    if preset.audio_copy && crate::preset::is_mpegts_audio_copy_compatible(source_codec_id) {
        return AudioAction::Copy;
    }
    AudioAction::Encode {
        encoder: &preset.audio_codec,
    }
}

/// `true` when a software fallback downgrades the intended HEVC output to
/// H.264 (software HEVC encoding is not shipped, contract §2).
///
/// Callers must log `video: sw fallback downgrades hevc to h264` when this is
/// true and the software branch actually runs.
pub fn sw_fallback_downgrades_hevc(preset: &TranscodePreset) -> bool {
    preset.hw_video_encoder == "hevc_mediacodec" && preset.sw_video_encoder == "libx264"
}

// ── Video rate control (contract §4) ─────────────────────────────────────────

/// Video rate-control mode, one of `wdmm_core::models::RATE_CONTROL_MODES`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

    /// Advanced option keys a preset may set for this family.
    pub fn allowed_keys(self) -> &'static [&'static str] {
        match self {
            EncoderFamily::Hardware => HW_VIDEO_OPTION_KEYS,
            EncoderFamily::LibX264 => SW_X264_OPTION_KEYS,
            EncoderFamily::MpegVideo => SW_MPEG_OPTION_KEYS,
        }
    }
}

/// Whitelisted `hw_video_options` keys (contract §3).
pub const HW_VIDEO_OPTION_KEYS: &[&str] = &[
    "bitrate_mode",
    "global_quality",
    "qmin",
    "qmax",
    "profile",
    "level",
    "operating_rate",
    "codec_name",
    "pts_as_dts",
    "max_b_frames",
];

/// Whitelisted `sw_video_options` keys for `libx264` (contract §3).
pub const SW_X264_OPTION_KEYS: &[&str] = &[
    "preset",
    "crf",
    "tune",
    "profile",
    "level",
    "max_b_frames",
    "x264-params",
];

/// Whitelisted `sw_video_options` keys for the MPEG-1/2/4 encoders
/// (contract §8bis — the equivalent knobs of the x264 ladder).
pub const SW_MPEG_OPTION_KEYS: &[&str] = &[
    "mpv_flags",
    "lmin",
    "lmax",
    "noise_reduction",
    "luma_elim_threshold",
    "chroma_elim_threshold",
    "mpeg_quant",
    "data_partitioning",
    "b_strategy",
    "me_method",
    "me_range",
    "mb_decision",
    "qmin",
    "qmax",
    "max_b_frames",
    "rc_min_rate",
    "rc_max_rate",
    "rc_buffer_size",
];

/// `x264-params` keys the transcode plan owns. A preset that sets one of them
/// is overridden (and the diag names the key), so the segment grid can never
/// drift off the HLS plan (contract §13.5).
pub const INVARIANT_X264_PARAMS: &[&str] = &["keyint", "min-keyint", "scenecut"];

/// The `AVDictionary` a video encoder is opened with, plus its provenance.
///
/// `entries` is applied in order and the last writer wins (`av_dict_set`
/// overwrites), so that order *is* the contract §13.5 priority:
/// structured fields → preset advanced options → invariants.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VideoEncoderOptions {
    /// Key/value pairs handed to `avcodec_open2`, in application order.
    pub entries: Vec<(String, String)>,
    /// Preset option keys an invariant took over (reported in the diag).
    pub overridden: Vec<String>,
    /// How many entries came from the preset's advanced options; a failed
    /// `avcodec_open2` is only worth retrying when this is non-zero.
    pub user_option_count: usize,
}

impl VideoEncoderOptions {
    /// Get one final entry by key (last writer wins).
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// `key=value` list for the diag log; `none` when the encoder keeps every
    /// default.
    pub fn dict_line(&self) -> String {
        if self.entries.is_empty() {
            return "none".to_string();
        }
        self.entries
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// Build the complete `AVDictionary` for one video encoder.
///
/// Applies the contract §13.5 priority — invariants > the preset's advanced
/// options > the structured fields — by ordering the entries, and returns a
/// loud error for an unknown key or an encoder/family mismatch instead of
/// silently dropping the option.
pub fn video_encoder_options(
    preset: &TranscodePreset,
    encoder: &str,
    hardware: bool,
    bitrate: u32,
    gop: i32,
) -> Result<VideoEncoderOptions, String> {
    build_video_encoder_options(preset, encoder, hardware, bitrate, gop, true)
}

/// Same options without the preset's advanced entries: the retry pass after
/// `avcodec_open2` rejected the user's options (contract §13.5).
pub fn video_encoder_options_without_user(
    preset: &TranscodePreset,
    encoder: &str,
    hardware: bool,
    bitrate: u32,
    gop: i32,
) -> Result<VideoEncoderOptions, String> {
    build_video_encoder_options(preset, encoder, hardware, bitrate, gop, false)
}

fn build_video_encoder_options(
    preset: &TranscodePreset,
    encoder: &str,
    hardware: bool,
    bitrate: u32,
    gop: i32,
    include_user_options: bool,
) -> Result<VideoEncoderOptions, String> {
    let family = EncoderFamily::of(encoder).ok_or_else(|| format!("unknown encoder: {encoder}"))?;
    if hardware != (family == EncoderFamily::Hardware) {
        return Err(format!(
            "encoder {encoder} is not a {} encoder",
            if hardware { "hardware" } else { "software" }
        ));
    }
    let rate_control = RateControl::parse(&preset.video_rate_control)?;
    let quality = preset.video_quality;

    // ── Structured fields (rate control / preset ladder) ────────────────────
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

    // ── Preset advanced options (middle priority) ───────────────────────────
    let mut user: Vec<(String, String)> = Vec::new();
    let mut x264_user: Vec<String> = Vec::new();
    let mut overridden: Vec<String> = Vec::new();
    if include_user_options {
        let source = if hardware {
            &preset.hw_video_options
        } else {
            &preset.sw_video_options
        };
        for (key, value) in source {
            if !family.allowed_keys().contains(&key.as_str()) {
                return Err(format!("unknown option: {key}"));
            }
            if family == EncoderFamily::Hardware && key == "max_b_frames" {
                // Invariant: MediaCodec must not reorder frames (B-frames would
                // delay every packet past its segment boundary).
                overridden.push(key.clone());
                continue;
            }
            if family == EncoderFamily::LibX264 && key == "x264-params" {
                for token in value.split(':') {
                    let token = token.trim();
                    if token.is_empty() {
                        continue;
                    }
                    let name = token.split('=').next().unwrap_or(token);
                    if INVARIANT_X264_PARAMS.contains(&name) {
                        overridden.push(format!("x264-params:{name}"));
                    } else {
                        x264_user.push(token.to_string());
                    }
                }
                continue;
            }
            user.push((key.clone(), value.clone()));
        }
    }
    let user_option_count = user.len() + x264_user.len();

    // ── Invariants (highest priority) ───────────────────────────────────────
    let mut invariants: Vec<(String, String)> = Vec::new();
    match family {
        EncoderFamily::Hardware => invariants.push(("max_b_frames".into(), "0".into())),
        EncoderFamily::LibX264 => {
            let mut tokens = x264_structured;
            tokens.extend(x264_user);
            tokens.push(format!("keyint={gop}:min-keyint={gop}:scenecut=0"));
            invariants.push(("x264-params".into(), tokens.join(":")));
        }
        EncoderFamily::MpegVideo => {
            invariants.push(("max_b_frames".into(), "0".into()));
            // libavcodec refuses CLOSED_GOP while scene-change detection is on
            // ("set threshold to 1000000000").
            invariants.push(("sc_threshold".into(), "1000000000".into()));
        }
    }

    let mut entries = Vec::with_capacity(structured.len() + user.len() + invariants.len());
    entries.extend(structured);
    entries.extend(user);
    entries.extend(invariants);
    Ok(VideoEncoderOptions {
        entries,
        overridden,
        user_option_count,
    })
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

/// One-line diag for the decided video layer (contract §3/§13.5): emitted once
/// per plan and carrying the *actual* options that reach `avcodec_open2`.
pub fn video_plan_diag(
    action: StreamAction<'_>,
    bitrate: u32,
    rate_control: RateControl,
    options: &VideoEncoderOptions,
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
            options.dict_line()
        ),
    }
}

// ── Encoder option schema for the preset editor (contract §13.4) ───────────

/// Value type the preset editor renders for one advanced option (§13.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OptionType {
    /// Fixed set in [`EncoderOptionSpec::values`].
    Enum,
    /// Integer with optional [`EncoderOptionSpec::min`] / [`EncoderOptionSpec::max`].
    Int,
    /// Decimal number with optional bounds.
    Float,
    /// Boolean switch.
    Bool,
}

/// One advanced option the preset editor may render.
///
/// `key` is exactly the `AVDictionary` key handed to `avcodec_open2`, so the
/// editor and the transcoder cannot drift apart (§13.4).
#[derive(Debug, Clone, serde::Serialize)]
pub struct EncoderOptionSpec {
    /// `AVDictionary` key.
    pub key: &'static str,
    /// Human label for the editor.
    pub label: &'static str,
    /// Value type.
    #[serde(rename = "type")]
    pub kind: OptionType,
    /// Allowed values for [`OptionType::Enum`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub values: Option<&'static [&'static str]>,
    /// Lower bound for numeric options.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    /// Upper bound for numeric options.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    /// `None` = keep the encoder default.
    pub default: Option<&'static str>,
    /// `true` for options hidden behind the "advanced" disclosure.
    pub advanced: bool,
}

/// Build an [`EncoderOptionSpec`] for an enum option.
const fn enum_option(
    key: &'static str,
    label: &'static str,
    values: &'static [&'static str],
) -> EncoderOptionSpec {
    EncoderOptionSpec {
        key,
        label,
        kind: OptionType::Enum,
        values: Some(values),
        min: None,
        max: None,
        default: None,
        advanced: true,
    }
}

/// Build an [`EncoderOptionSpec`] for a free-form string option.
const fn text_option(key: &'static str, label: &'static str) -> EncoderOptionSpec {
    EncoderOptionSpec {
        key,
        label,
        kind: OptionType::Enum,
        values: None,
        min: None,
        max: None,
        default: None,
        advanced: true,
    }
}

/// Build an [`EncoderOptionSpec`] for an integer option with bounds.
const fn int_option(
    key: &'static str,
    label: &'static str,
    min: f64,
    max: f64,
) -> EncoderOptionSpec {
    EncoderOptionSpec {
        key,
        label,
        kind: OptionType::Int,
        values: None,
        min: Some(min),
        max: Some(max),
        default: None,
        advanced: true,
    }
}

/// Build an [`EncoderOptionSpec`] for a boolean option.
const fn bool_option(key: &'static str, label: &'static str) -> EncoderOptionSpec {
    EncoderOptionSpec {
        key,
        label,
        kind: OptionType::Bool,
        values: None,
        min: None,
        max: None,
        default: None,
        advanced: true,
    }
}

/// One encoder the preset editor can select (§13.4).
#[derive(Debug, Clone, serde::Serialize)]
pub struct CodecSchema {
    /// Encoder name (`hw_video_encoder`/`sw_video_encoder` value).
    pub id: &'static str,
    /// `"hw"` for MediaCodec, `"sw"` for software.
    pub kind: &'static str,
    /// Human label for the editor.
    pub label: &'static str,
    /// Codec the encoder produces (`h264`/`hevc`/`mpeg4`/`mpeg2video`/…).
    pub output_codec: &'static str,
    /// Rate-control modes this encoder supports.
    pub rate_control: &'static [&'static str],
    /// Whitelisted advanced options.
    pub options: &'static [EncoderOptionSpec],
}

/// Complete schema returned by `wdmm_encoder_options_schema_json`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EncoderOptionsSchema {
    /// Every selectable encoder, hardware first.
    pub codecs: Vec<CodecSchema>,
}

/// Rate-control modes exposed in the schema (same order as the model).
pub const SCHEMA_RATE_CONTROL_MODES: [&str; 4] = ["auto", "vbr", "cbr", "cq"];

/// MediaCodec advanced options (§3). The platform codec itself rejects an
/// unsupported combination at `avcodec_open2`, which the transcode path turns
/// into a loud fallback instead of a silent no-op.
static HW_OPTION_SPECS: [EncoderOptionSpec; 10] = [
    enum_option("bitrate_mode", "码率模式", &["vbr", "cbr", "cq"]),
    int_option("global_quality", "固定质量（0-100）", 0.0, 100.0),
    int_option("qmin", "最小量化器", 0.0, 51.0),
    int_option("qmax", "最大量化器", 0.0, 51.0),
    int_option("profile", "Profile", 0.0, 22.0),
    int_option("level", "Level", 0.0, 62.0),
    int_option("operating_rate", "期望帧率", 1.0, 240.0),
    text_option("codec_name", "平台编码器名"),
    bool_option("pts_as_dts", "用 PTS 作为 DTS"),
    int_option("max_b_frames", "B 帧数（固定 0）", 0.0, 0.0),
];

/// libx264 advanced options (§3): the x264 speed ladder, CRF and tuning.
static X264_OPTION_SPECS: [EncoderOptionSpec; 7] = [
    enum_option(
        "preset",
        "x264 速度档位",
        &[
            "ultrafast",
            "superfast",
            "veryfast",
            "faster",
            "fast",
            "medium",
            "slow",
            "slower",
            "veryslow",
            "placebo",
        ],
    ),
    int_option("crf", "CRF（0-51）", 0.0, 51.0),
    enum_option(
        "tune",
        "画面调优",
        &[
            "film",
            "animation",
            "grain",
            "stillimage",
            "fastdecode",
            "zerolatency",
        ],
    ),
    enum_option(
        "profile",
        "Profile",
        &["baseline", "main", "high", "high10", "high422", "high444"],
    ),
    enum_option(
        "level",
        "Level",
        &[
            "3.0", "3.1", "3.2", "4.0", "4.1", "4.2", "5.0", "5.1", "5.2", "6.0",
        ],
    ),
    int_option("max_b_frames", "B 帧数", 0.0, 16.0),
    text_option("x264-params", "x264 参数串"),
];

/// MPEG-1/2/4 advanced options (§8bis): the equivalent knobs of the x264
/// ladder, plus the MPEG quantiser and VBV controls.
static MPEG_OPTION_SPECS: [EncoderOptionSpec; 18] = [
    enum_option(
        "mpv_flags",
        "运动估计加速",
        &["qp_rd", "cbp_rd", "skip_rd", "naq", "strict_gop"],
    ),
    int_option("lmin", "最小拉格朗日乘子", 0.0, 63.0),
    int_option("lmax", "最大拉格朗日乘子", 0.0, 63.0),
    int_option("noise_reduction", "降噪强度", 0.0, 1000.0),
    int_option("luma_elim_threshold", "亮度零系数阈值", 0.0, 1_000_000.0),
    int_option("chroma_elim_threshold", "色度零系数阈值", 0.0, 1_000_000.0),
    bool_option("mpeg_quant", "MPEG 量化"),
    bool_option("data_partitioning", "数据分区"),
    int_option("b_strategy", "B 帧策略", 0.0, 2.0),
    enum_option(
        "me_method",
        "运动估计方法",
        &[
            "zero", "full", "log", "phods", "epzs", "x1", "hex", "umh", "tesa",
        ],
    ),
    int_option("me_range", "运动估计范围", 0.0, 4096.0),
    int_option("mb_decision", "宏块决策", 0.0, 2.0),
    int_option("qmin", "最小量化器", 0.0, 51.0),
    int_option("qmax", "最大量化器", 0.0, 51.0),
    int_option("max_b_frames", "B 帧数（固定 0）", 0.0, 0.0),
    int_option("rc_min_rate", "最小码率", 0.0, 100_000_000.0),
    int_option("rc_max_rate", "最大码率", 0.0, 100_000_000.0),
    int_option("rc_buffer_size", "VBV 缓冲", 0.0, 100_000_000.0),
];

/// Editor metadata for one encoder name: `(label, output_codec)`.
fn encoder_meta(name: &str) -> (&'static str, &'static str) {
    match name {
        "h264_mediacodec" => ("MediaCodec H.264（硬件）", "h264"),
        "hevc_mediacodec" => ("MediaCodec H.265（硬件）", "hevc"),
        "mpeg4_mediacodec" => ("MediaCodec MPEG-4（硬件）", "mpeg4"),
        "libx264" => ("libx264（软件）", "h264"),
        "mpeg2video" => ("MPEG-2（软件）", "mpeg2video"),
        "mpeg4" => ("MPEG-4（软件）", "mpeg4"),
        _ => ("未知编码器", "unknown"),
    }
}

/// Schema for every encoder a preset may select.
///
/// The editor renders exactly this list and these keys; it must not hard-code
/// option names or value ranges (§13.4). The list matches
/// `HW_VIDEO_ENCODERS ∪ SW_VIDEO_ENCODERS` — the encoders
/// [`decide_video_action`] can return.
pub fn encoder_options_schema() -> EncoderOptionsSchema {
    let rate_control: &'static [&'static str] = &SCHEMA_RATE_CONTROL_MODES;
    let mut codecs = Vec::new();
    for &name in wdmm_core::models::HW_VIDEO_ENCODERS.iter() {
        let (label, output_codec) = encoder_meta(name);
        codecs.push(CodecSchema {
            id: name,
            kind: "hw",
            label,
            output_codec,
            rate_control,
            options: &HW_OPTION_SPECS,
        });
    }
    for &name in wdmm_core::models::SW_VIDEO_ENCODERS.iter() {
        let (label, output_codec) = encoder_meta(name);
        codecs.push(CodecSchema {
            id: name,
            kind: "sw",
            label,
            output_codec,
            rate_control,
            options: if name == "libx264" {
                &X264_OPTION_SPECS
            } else {
                &MPEG_OPTION_SPECS
            },
        });
    }
    EncoderOptionsSchema { codecs }
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
    fn remux_plan_matrix_matches_stream_compatibility() {
        use codec_ids::*;
        // Both copyable → pure remux.
        assert_eq!(remux_plan(Some(H264), Some(AAC)), RemuxPlan::Remux);
        assert_eq!(remux_plan(Some(HEVC), Some(AC3)), RemuxPlan::Remux);
        assert_eq!(remux_plan(Some(MPEG2VIDEO), Some(EAC3)), RemuxPlan::Remux);
        assert_eq!(remux_plan(Some(H264), Some(MP3)), RemuxPlan::Remux);
        // Video copyable, audio not → hybrid (the HEVC+Opus case).
        assert_eq!(remux_plan(Some(H264), Some(OPUS_ID)), RemuxPlan::Hybrid);
        assert_eq!(remux_plan(Some(HEVC), Some(FLAC_ID)), RemuxPlan::Hybrid);
        // Video not copyable → full transcode.
        assert_eq!(
            remux_plan(Some(VP9_ID), Some(OPUS_ID)),
            RemuxPlan::Transcode
        );
        assert_eq!(remux_plan(Some(AV1_ID), Some(AAC)), RemuxPlan::Transcode);
        // No audio is fine: video-only remux.
        assert_eq!(remux_plan(Some(H264), None), RemuxPlan::Remux);
        assert_eq!(remux_plan(None, None), RemuxPlan::Transcode);
    }

    // Codecs not modelled by the preset layer; values are only used by these
    // tests (FFmpeg 9.0 numbering, see `preset::codec_ids`).
    const VP9_ID: u32 = 166;
    const AV1_ID: u32 = 222;
    const OPUS_ID: u32 = 86076;
    const FLAC_ID: u32 = 86028;

    #[test]
    fn audio_only_sources_stay_on_the_cheap_path() {
        use codec_ids::*;
        // Audio-only source (no video stream): copy the audio when MPEG-TS can
        // carry it, otherwise hybrid re-encodes only the audio.
        assert_eq!(remux_plan(None, Some(AAC)), RemuxPlan::Remux);
        assert_eq!(remux_plan(None, Some(MP3)), RemuxPlan::Remux);
        assert_eq!(remux_plan(None, Some(AC3)), RemuxPlan::Remux);
        assert_eq!(remux_plan(None, Some(EAC3)), RemuxPlan::Remux);
        assert_eq!(remux_plan(None, Some(OPUS_ID)), RemuxPlan::Hybrid);
        assert_eq!(remux_plan(None, Some(FLAC_ID)), RemuxPlan::Hybrid);
        // Nothing playable at all still falls back to the transcode path.
        assert_eq!(remux_plan(None, None), RemuxPlan::Transcode);
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
        // The built-ins allow copy, so a compatible source copies both streams.
        let preset = preset_h264();
        assert_eq!(
            decide_video_action(&preset, codec_ids::H264, true),
            Ok(StreamAction::Copy)
        );
        assert_eq!(
            decide_audio_action(&preset, codec_ids::AAC),
            AudioAction::Copy
        );

        // copy + encode: video copies, Opus audio must be re-encoded.
        assert_eq!(
            decide_video_action(&preset, codec_ids::H264, true),
            Ok(StreamAction::Copy)
        );
        assert_eq!(
            decide_audio_action(&preset, OPUS_ID),
            AudioAction::Encode { encoder: "aac" }
        );

        // encode + copy: VP9 video cannot be copied, AAC audio can.
        assert_eq!(
            decide_video_action(&preset, VP9_ID, true),
            Ok(StreamAction::Encode {
                hardware: true,
                encoder: "h264_mediacodec",
            })
        );
        assert_eq!(
            decide_audio_action(&preset, codec_ids::AAC),
            AudioAction::Copy
        );

        // encode + encode: VP9 video + Opus audio both re-encoded.
        assert_eq!(
            decide_video_action(&preset, VP9_ID, false),
            Ok(StreamAction::Encode {
                hardware: false,
                encoder: "libx264",
            })
        );
        assert_eq!(
            decide_audio_action(&preset, OPUS_ID),
            AudioAction::Encode { encoder: "aac" }
        );
    }

    #[test]
    fn copy_off_and_hardware_unavailable_fall_through_to_software() {
        let mut preset = preset_h264();
        preset.video_copy = false;
        preset.audio_copy = false;
        assert_eq!(
            decide_video_action(&preset, codec_ids::H264, true),
            Ok(StreamAction::Encode {
                hardware: true,
                encoder: "h264_mediacodec",
            })
        );
        assert_eq!(
            decide_video_action(&preset, codec_ids::H264, false),
            Ok(StreamAction::Encode {
                hardware: false,
                encoder: "libx264",
            })
        );
        assert_eq!(
            decide_audio_action(&preset, codec_ids::AAC),
            AudioAction::Encode { encoder: "aac" }
        );
    }

    #[test]
    fn no_usable_layer_is_a_loud_error() {
        let mut preset = preset_h264();
        preset.video_copy = false;
        preset.hw_video_encoder = String::new();
        preset.sw_video_encoder = String::new();
        let err = decide_video_action(&preset, VP9_ID, true).unwrap_err();
        assert_eq!(err, "no video encoder available for preset h264");
    }

    #[test]
    fn sw_fallback_downgrades_hevc_only_for_the_hevc_builtin() {
        assert!(sw_fallback_downgrades_hevc(&preset_hevc()));
        assert!(!sw_fallback_downgrades_hevc(&preset_h264()));
    }

    #[test]
    fn rate_control_parses_the_four_modes() {
        for mode in SCHEMA_RATE_CONTROL_MODES {
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

    #[test]
    fn libx264_options_carry_the_speed_preset_and_the_gop_invariant() {
        let preset = preset_h264();
        let opts = video_encoder_options(&preset, "libx264", false, 4_000_000, 120).unwrap();
        assert_eq!(opts.get("preset"), Some("veryfast"));
        // vbr leaves the encoder's own rate control alone: no crf, no vbv.
        assert_eq!(opts.get("crf"), None);
        assert_eq!(
            opts.get("x264-params"),
            Some("keyint=120:min-keyint=120:scenecut=0")
        );
        assert_eq!(opts.user_option_count, 0);
        assert!(opts.overridden.is_empty());
        assert_eq!(
            opts.dict_line(),
            "preset=veryfast,x264-params=keyint=120:min-keyint=120:scenecut=0"
        );
    }

    #[test]
    fn cbr_and_cq_map_to_encoder_specific_knobs() {
        let mut preset = preset_h264();
        preset.video_rate_control = "cbr".into();
        let opts = video_encoder_options(&preset, "libx264", false, 4_000_000, 120).unwrap();
        let params = opts.get("x264-params").unwrap();
        assert!(params.contains("nal-hrd=cbr:force-cfr=1"));
        // 4 Mbps target → 4000 kbit/s HRD at a 2 s buffer.
        assert!(params.contains("vbv-maxrate=4000:vbv-bufsize=8000"));
        assert!(params.ends_with("keyint=120:min-keyint=120:scenecut=0"));

        // cq → CRF (default 23) with the bitrate as a VBV bound.
        preset.video_rate_control = "cq".into();
        let opts = video_encoder_options(&preset, "libx264", false, 4_000_000, 120).unwrap();
        assert_eq!(opts.get("crf"), Some("23"));
        assert!(opts
            .get("x264-params")
            .unwrap()
            .contains("vbv-maxrate=4000"));
        // The preset's quality value replaces the default CRF.
        preset.video_quality = Some(18);
        let opts = video_encoder_options(&preset, "libx264", false, 4_000_000, 120).unwrap();
        assert_eq!(opts.get("crf"), Some("18"));

        // MPEG-2 cbr → explicit RC window instead of a bitrate_mode option.
        preset.video_rate_control = "cbr".into();
        let opts = video_encoder_options(&preset, "mpeg2video", false, 3_000_000, 60).unwrap();
        assert_eq!(opts.get("minrate"), Some("3000000"));
        assert_eq!(opts.get("maxrate"), Some("3000000"));
        assert_eq!(opts.get("bufsize"), Some("6000000"));
        assert_eq!(opts.get("max_b_frames"), Some("0"));
        assert_eq!(opts.get("sc_threshold"), Some("1000000000"));

        // MPEG-2 cq → a fixed quantiser, still pinned to a constant GOP.
        preset.video_rate_control = "cq".into();
        preset.video_quality = Some(4);
        let opts = video_encoder_options(&preset, "mpeg2video", false, 3_000_000, 60).unwrap();
        assert_eq!(opts.get("qmin"), Some("4"));
        assert_eq!(opts.get("qmax"), Some("4"));
        assert_eq!(opts.get("sc_threshold"), Some("1000000000"));
    }

    #[test]
    fn hardware_options_inject_the_rate_mode_and_pin_b_frames() {
        let mut preset = preset_h264();
        preset.video_rate_control = "cq".into();
        preset
            .hw_video_options
            .insert("max_b_frames".into(), "3".into());
        let opts = video_encoder_options(&preset, "h264_mediacodec", true, 4_000_000, 60).unwrap();
        assert_eq!(opts.get("bitrate_mode"), Some("cq"));
        assert_eq!(opts.get("global_quality"), Some("23"));
        // The invariant wins: MediaCodec must never reorder frames.
        assert_eq!(opts.get("max_b_frames"), Some("0"));
        assert_eq!(opts.overridden, vec!["max_b_frames".to_string()]);
        assert_eq!(opts.user_option_count, 0);
    }

    #[test]
    fn preset_options_cannot_override_the_gop_invariant() {
        let mut preset = preset_h264();
        preset
            .sw_video_options
            .insert("x264-params".into(), "ref=3:keyint=999".into());
        preset
            .sw_video_options
            .insert("tune".into(), "animation".into());
        let opts = video_encoder_options(&preset, "libx264", false, 1_000_000, 60).unwrap();
        let params = opts.get("x264-params").unwrap();
        assert!(params.starts_with("ref=3"));
        assert!(params.ends_with("keyint=60:min-keyint=60:scenecut=0"));
        assert_eq!(opts.get("tune"), Some("animation"));
        assert!(opts.overridden.iter().any(|k| k == "x264-params:keyint"));
        assert_eq!(opts.user_option_count, 2);
    }

    #[test]
    fn unknown_options_and_family_mismatches_are_loud() {
        let mut preset = preset_h264();
        preset.sw_video_options.insert("bogus".into(), "1".into());
        assert_eq!(
            video_encoder_options(&preset, "libx264", false, 1_000_000, 60).unwrap_err(),
            "unknown option: bogus"
        );
        let clean = preset_h264();
        assert_eq!(
            video_encoder_options(&clean, "libx264", true, 1_000_000, 60).unwrap_err(),
            "encoder libx264 is not a hardware encoder"
        );
        assert_eq!(
            video_encoder_options(&clean, "h264_mediacodec", false, 1_000_000, 60).unwrap_err(),
            "encoder h264_mediacodec is not a software encoder"
        );
        assert_eq!(
            video_encoder_options(&clean, "x265", false, 1_000_000, 60).unwrap_err(),
            "unknown encoder: x265"
        );
    }

    #[test]
    fn dropping_the_user_options_keeps_the_structured_fields() {
        let mut preset = preset_h264();
        preset.video_rate_control = "cq".into();
        preset.video_quality = Some(20);
        preset
            .sw_video_options
            .insert("x264-params".into(), "ref=3".into());
        let full = video_encoder_options(&preset, "libx264", false, 2_000_000, 60).unwrap();
        let retry =
            video_encoder_options_without_user(&preset, "libx264", false, 2_000_000, 60).unwrap();
        assert_eq!(full.get("crf"), retry.get("crf"));
        assert_eq!(full.get("preset"), retry.get("preset"));
        assert_eq!(full.user_option_count, 1);
        assert_eq!(retry.user_option_count, 0);
        assert!(!retry.get("x264-params").unwrap().contains("ref=3"));
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
        let options = VideoEncoderOptions::default();
        let copy = video_plan_diag(StreamAction::Copy, 4_000_000, RateControl::Vbr, &options);
        assert_eq!(
            copy,
            "video plan: action=copy encoder=none rate_control=vbr bitrate=4000000 options=none"
        );
        let hw_options =
            video_encoder_options(&preset_hevc(), "hevc_mediacodec", true, 3_000_000, 60).unwrap();
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
            "video plan: action=hw encoder=hevc_mediacodec rate_control=vbr bitrate=3000000 options=bitrate_mode=vbr,max_b_frames=0"
        );
        let sw_options =
            video_encoder_options(&preset_h264(), "libx264", false, 1_000_000, 60).unwrap();
        let sw = video_plan_diag(
            StreamAction::Encode {
                hardware: false,
                encoder: "libx264",
            },
            1_000_000,
            RateControl::Vbr,
            &sw_options,
        );
        assert!(sw.starts_with("video plan: action=sw encoder=libx264 rate_control=vbr bitrate=1000000 options=preset=veryfast,x264-params="));
    }

    #[test]
    fn schema_lists_every_shippable_encoder_with_whitelisted_keys() {
        let schema = encoder_options_schema();
        assert_eq!(
            schema.codecs.len(),
            wdmm_core::models::HW_VIDEO_ENCODERS.len() + wdmm_core::models::SW_VIDEO_ENCODERS.len()
        );
        for codec in &schema.codecs {
            let family = EncoderFamily::of(codec.id).expect("schema names a known encoder");
            assert_eq!(
                codec.kind,
                if family == EncoderFamily::Hardware {
                    "hw"
                } else {
                    "sw"
                }
            );
            assert_eq!(codec.rate_control, &SCHEMA_RATE_CONTROL_MODES[..]);
            for spec in codec.options {
                assert!(
                    family.allowed_keys().contains(&spec.key),
                    "{} is not whitelisted for {}",
                    spec.key,
                    codec.id
                );
            }
            assert!(SHIPPED_VIDEO_ENCODERS.contains(&codec.id));
        }
        // Hardware encoders first, mirroring the preset editor order.
        let k = wdmm_core::models::HW_VIDEO_ENCODERS.len();
        assert!(schema.codecs[..k].iter().all(|c| c.kind == "hw"));
        assert!(schema.codecs[k..].iter().all(|c| c.kind == "sw"));
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
