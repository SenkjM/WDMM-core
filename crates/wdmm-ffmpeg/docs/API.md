# wdmm-ffmpeg — Public API

In-process FFmpeg (libav*) bindings: probing, HLS segment jobs, resumable
segment plans, remux/encoder planning and subtitle conversion. The prebuilt
`libav*.so` are linked in-process by `libwdmm_ffi.so` (built with
`--features ffmpeg`); no `ffmpeg`/`ffprobe` subprocess is ever spawned. Without
the feature the API still compiles, and job entry points return
`FfmpegError::Unavailable`.
FFmpeg is LGPL/GPL — see `docs/FFMPEG-ATTRIBUTION.md`. WDMM is AGPL-3.0.

## Availability and errors
### `pub const fn is_available() -> bool`
`true` when built with the `ffmpeg` feature; lets callers disable cast UI.
### `pub type FfmpegResult<T> = std::result::Result<T, FfmpegError>;`
### `pub enum FfmpegError {`
- `Unavailable(String)` — built without the `ffmpeg` feature.
- `Probe(String)` — libavformat could not open or describe the input.
- `Io(#[from] std::io::Error)` — segment/cache directory I/O.

## Probe
### `pub struct ProbeResult {`
- `duration_secs: f64`
- `video: Vec<VideoStream>` (`index`, `codec`, `width`, `height`, `fps`)
- `audio: Vec<AudioStream>` (`index`, `codec`, `channels`, `sample_rate`, `language`)
- `subtitles: Vec<SubtitleStream>` (`index`, `codec`, `language`, `title`)
- `format_name: String`
### `pub fn probe(url: &str) -> FfmpegResult<ProbeResult>`
Accepts a local path or an `http(s)://` URL (the proxy passes its loopback URL
for encrypted sources).

## Subtitle extraction
### `pub fn embedded_subtitle_tracks(url: &str) -> FfmpegResult<Vec<SubtitleStream>>`
Lists the embedded subtitle tracks of a source (one probe, no second scan),
including the per-track `language` and `title` metadata the FFI exposes as cast
subtitle candidates.
### `pub fn extract_subtitle_track(url: &str, stream_index: u32) -> FfmpegResult<String>`
Decodes one text subtitle stream to WebVTT. Bitmap codecs (PGS, DVD, DVB) are
rejected — classify with `classify_codec` first. Requires the `ffmpeg` feature.

## HLS job
### `pub struct JobOpts {`
- `input_url: String`, `preset: TranscodePreset` (per-stream remux-mode/encoder
  config, rate control and `hls_segment_secs`)
- `start_offset_secs: f64` — media time to start from (`0.0` = head)
- `segment_dir: PathBuf` — caller-provided; ignored when `out_dir` is set
- `start_number: u64` — index of the first segment, so a pull job can restart
  mid-film and cover the requested `k`
- `out_dir: Option<PathBuf>` — persistent VOD cache; created here and **not**
  deleted on drop. `None` means a temp dir owned (and removed) by the job.
- `decision: Option<SessionDecision>` — the per-session copy/encode decision
  (see *Stream decision*). `None` lets the job decide on its own; a session that
  reopens for another segment range passes the decision it cached first so the
  grid never drifts.
- `hls_time_override_ms: Option<u32>` — `hls_time` in ms. `Some(1)` makes the
  HLS muxer cut on every video keyframe (remux mode) or every forced IDR
  (encode path), so the plan grid is anchor-independent. Audio-only jobs keep
  `hls_segment_secs` with `split_by_time`.
### `pub fn open_job(opts: JobOpts) -> FfmpegResult<JobHandle>`
Starts the job on a background OS thread and returns immediately. Admission is
the caller's job — `wdmm-proxy` holds the process-wide `JobGate` permit.
### `pub struct JobHandle {`
- `pub segment_dir: PathBuf`; `segment_dir()` / `out_dir()` return the same dir.
- `ready_segments()` — non-blocking directory snapshot.
- `segment_durations()` — `EXTINF` values; empty until the first segment.
- `cancel()`, `is_running()`, `take_result()` (`None` while running), `join()`.
- `pause()` / `resume()` / `is_paused()` — cooperative throttling: the read loops
  block at packet boundaries while paused (no busy wait); `cancel()` and `Drop`
  wake the latch, so a paused job never deadlocks. `is_running()` stays `true`
  while paused.
- `progress()` — `JobProgress { content_ms, start_ms, wall_ms, packets }`:
  `content_ms` is media produced since the run's seek anchor, `start_ms` is that
  anchor (absolute media ms = `start_offset_secs * 1000`), `wall_ms` the elapsed
  job time.
- `encoder_info()` (`mode` = `remux`/`transcode`; `video_action` /
  `audio_action` = `copy`/`hw`/`sw`/`none`, straight from the per-stream
  decision — `mode` alone cannot express encode+copy), `warnings()`
  (deduplicated fallbacks).
Dropping the handle cancels and joins the job.

## Stream decision
One decision per stream, taken once per session from the preset plus the probed
source codecs, then reused by every job of that session (reopen/seek must not
change the grid):
### `pub enum StreamMode {`
- `None` — the source has no such stream, nothing is muxed.
- `Copy` — remux the source packets into MPEG-TS unchanged.
- `Hardware` — encode with `preset.hw_video_encoder` (MediaCodec).
- `Software` — encode with `preset.sw_video_encoder`, tuned by
  `preset.sw_video_preset` and `video_rate_control` (see *Encoder catalog*).
`StreamMode::as_str()` yields `none`/`copy`/`hw`/`sw` (the session-status
`video_action`/`audio_action` values).
### `pub struct SessionDecision {`
- `video: StreamMode`, `video_encoder: Option<String>`
- `audio: StreamMode`, `audio_encoder: Option<String>`
### `pub enum RemuxMode { Off, Strict, On }`
`preset.video_remux` / `preset.audio_remux` are parsed by `RemuxMode::parse(&str)`
(`unknown remux mode: <mode>` error) and round-trip through `as_str()` (contract
`remux-modes` §3). `Off` always encodes; `Strict` copies only when the source
codec equals the codec the encoder layer would produce *right now*; `On` copies
every source MPEG-TS can carry.
### `pub fn SessionDecision::decide(preset, video_codec_id, audio_codec_id, hw_encoder_available) -> Result<SessionDecision, String>`
Applies `video_remux`/`audio_remux` against MPEG-TS legality and the runtime
target codec (the hardware encoder's output when it is available, otherwise the
software encoder's), picks the encoder path from the preset, and errors when the
preset has no usable video path. `hw_encoder_available` must mean *found and
opened*: the job probes the hardware encoder once and falls back to the software
encoder (logging `video: sw fallback downgrades hevc to h264` when the fallback
changes the codec).
### `pub fn decide_video_action(preset, source_codec_id, target_codec_id, hw_encoder_available) -> Result<StreamAction, String>`
### `pub fn decide_audio_action(preset, source_codec_id, target_codec_id) -> Result<AudioAction, String>`
### `pub fn hw_probe_needed(preset, source_video_codec_id) -> Result<bool, String>`
True when the remux decision depends on the encoder layer: always for `off`/
`strict`, and for `on` only when the source video codec is not legal in MPEG-TS.
Without video (or a hardware encoder) it is false.
### `pub enum StreamAction<'a> { Copy, Encode { hardware, encoder } }`
### `pub enum AudioAction<'a> { Copy, Encode { encoder } }`
The primitives `SessionDecision` is built from; `encoder_name()`, `uses_hardware()`,
`is_copy()` describe the chosen path.
### `pub fn sw_fallback_downgrades_hevc(preset) -> bool`
True when the preset promises HEVC through its hardware encoder but the software
fallback produces another codec.

## Encoder catalog and rate control
The preset editor renders a closed encoder list; free-form advanced options are
gone (contract `preset-editor-slimming` §2). The structured fields
(`video_rate_control`, `video_quality`, `sw_video_preset`, the bitrate envelope)
plus the invariants are turned into the exact `AVDictionary` handed to
`avcodec_open2`.
### `pub enum RateControl { Auto, Vbr, Cbr, Cq }`
`RateControl::parse(&preset.video_rate_control)` returns the user-facing
`unknown rate control: <mode>` error; `as_str()` round-trips and the enum
serialises lowercase for the catalog JSON.
### `pub enum EncoderFamily { Hardware, LibX264, MpegVideo }`
`EncoderFamily::of(encoder)` classifies a name; used to validate a requested
encoder against the chosen path.
### `pub fn encoder_av_options(preset, encoder, hardware, bitrate, gop) -> Result<Vec<(String, String)>, String>`
The complete, closed dictionary for one encoder: the structured rate-control
mapping plus the invariants. Unknown names and family mismatches are loud
(`unknown encoder: <name>`, `encoder <name> is not a hardware/software encoder`,
`unknown rate control: <mode>`). Invariants (a preset cannot override them):
`AV_CODEC_FLAG_GLOBAL_HEADER`, `max_b_frames=0` for the hardware path,
`keyint=min-keyint=<segment frames>` + `scenecut=0` for libx264 and the
closed-GOP threshold for the MPEG encoders. The dictionary also pins
`aspect=1/1`: the plan scales the *display* geometry, so the encoded pixels are
square (§3.2).
### `pub fn options_line(entries) -> String`
`k=v,…` list for the plan diagnostics (`none` when the encoder keeps defaults).
### `pub fn target_video_bitrate(preset, width, height, fps) -> u32`
`preset.video_max_bitrate`, or the resolution/fps-derived envelope
(`w*h*fps*0.1`, floored at 800 kbit/s). No tier factor.
### `pub fn video_plan_diag(action, bitrate, rate_control, options) -> String`
The diagnostic line naming the effective encoder, rate control, bitrate and
options dictionary (not the requested ones).
### `pub fn display_width(width, sar) -> i32`
### `pub fn fit_display_dimensions(src_w, src_h, sar, max_w, max_h) -> (i32, i32)`
Anamorphic sources are scaled by their display width (`width * sar`), never
squeezed to the coded width; the output is even-sized and never upscaled.
### `pub struct VideoEncoderInfo { id, label, hardware, output_codec, rate_control }`
### `pub fn video_encoder_catalog() -> &'static [VideoEncoderInfo]`
### `pub fn video_encoder_catalog_json() -> Result<String, serde_json::Error>`
The six editor-selectable encoders (`h264_mediacodec`, `hevc_mediacodec`,
`mpeg4_mediacodec`, `libx264`, `mpeg2video`, `mpeg4`) with their user-facing
label, hardware flag, output codec and supported rate-control modes. The JSON
shape is `{"codecs":[{"id","label","hardware","output_codec","rate_control":[…]}]}`;
the editor renders it directly instead of hard-coding names.
### `pub const SHIPPED_VIDEO_ENCODERS: [&str; N]`
### `pub const SHIPPED_AUDIO_ENCODERS: [&str; N]`
The explicit `--enable-encoder` list of the trimmed Android build; §13.6 gate
tests assert every one resolves.
### `pub fn encoder_available(name: &str) -> bool`
Whether libavcodec resolves that encoder name (false without the `ffmpeg`
feature).

## Segment plan
The proxy synthesises a VOD playlist from a `SegmentPlan` before playback, so
the renderer can seek without reloading it. There are exactly two alignments —
by design there is **no approximate/calibrated grid**, because a plan whose
boundaries are not real cut points makes seeking land on the wrong segment:
- `Exact` — forced re-encode: strict `k * seg_secs`. The encoder emits one IDR
  per step (closed GOP) and `hls_time` is tiny, so the muxer cuts on the grid.
- `Keyframe` — remux mode with a usable container keyframe index (mp4 `stss`,
  mkv `cues`): one segment per keyframe interval, so the synthesized `EXTINF`
  values match the segment files exactly. A source without a usable index is
  not planned approximately: it takes the `Exact` re-encode path instead.
### `pub enum PlanAlignment {`
### `pub struct SegmentPlan {`
- `duration_ms: i64`, `seg_secs: f64`, `alignment: PlanAlignment`,
  `gop_ms: Option<i64>` (median keyframe interval for `Keyframe`, `None` for
  `Exact`), `entries: Vec<SegEntry>`
- `segment_count()`, `start_ms(k) -> Option<i64>`, `duration_of(k) -> Option<i64>`
### `pub struct SegEntry {`
- `start_ms: i64`, `duration_ms: i64`
### `pub fn exact_plan(duration_ms: i64, seg_secs: f64) -> SegmentPlan`
Uniform grid; never drifts because the encoder controls every cut.
### `pub fn keyframe_plan(duration_ms: i64, seg_secs: f64, keyframes: &[i64]) -> Option<SegmentPlan>`
One segment per keyframe interval (the muxer cuts on every keyframe). `None`
when the index is too short (`MIN_KEYFRAMES_FOR_PLAN`) or truncated (ends more
than two GOPs before the media end) — the caller then re-encodes.
### `pub const MIN_KEYFRAMES_FOR_PLAN: usize`
### `pub struct SourcePlan {`
- `plan: SegmentPlan`, `video_copied: bool`, `keyframe_count: usize`,
  `decision: SessionDecision` — reuse it for every `JobOpts::decision` of the
  session
### `pub fn source_plan(url: &str, preset: &TranscodePreset) -> FfmpegResult<Option<SourcePlan>>`
Opens the source once, decides the stream actions and the alignment up front
(video copy ⇒ keyframe grid; re-encode or unusable index ⇒ exact grid) and
returns `None` when the plan cannot be decided (audio-only source, unknown
duration).
### `pub fn measure_segment_durations(dir: &Path) -> Vec<i64>`
Parses `EXTINF` values from `<dir>/index.m3u8` (diagnostics; plans are decided
up front, never calibrated from measurements).

## Diagnostics
### `pub fn log(msg: impl AsRef<str>)`
### `pub fn tail(max_lines: usize) -> Vec<String>`
### `pub fn set_file(path: Option<PathBuf>)`
### `pub fn clear()`
Process-global ring buffer (512 lines, elapsed-ms prefix). `set_file` mirrors
new lines to a file for `adb exec-out run-as`; works without libav.

## Presets and MPEG-TS copy checks
### `pub fn preset_h264() -> TranscodePreset`
### `pub fn preset_hevc() -> TranscodePreset`
### `pub fn all_builtin_presets() -> [TranscodePreset; 2]`
In order: `h264`, `hevc`. Both default to `video_remux`/`audio_remux` = `on` (video/audio copied when the
source is already MPEG-TS legal), with `h264_mediacodec`/`hevc_mediacodec`
as the hardware encoders and `libx264` as the software fallback.
### `pub fn builtin_preset(id: &str) -> Option<TranscodePreset>`
### `pub mod codec_ids`
`MPEG1VIDEO`, `MPEG2VIDEO`, `MPEG4`, `H264`, `HEVC`, `AAC`, `MP2`, `MP3`, `AC3`, `EAC3`.
### `pub fn video_encoder_output_codec_id(encoder: &str) -> Option<u32>`
The output codec of an encoder name — never reverse-derived from a codec name.
### `pub fn audio_encoder_name(codec: &str) -> &'static str`
### `pub fn audio_encoder_codec_id(codec: &str) -> u32`
`aac`/`ac3`/`eac3`/`mp2`; unknown values fall back to AAC.
### `pub fn is_ts_legal_video(codec_id: u32) -> bool`
MPEG-1, MPEG-2 and MPEG-4 video, H.264 and HEVC are legal in MPEG-TS. This is the single legality check the remux modes consult.
### `pub fn is_ts_legal_audio(codec_id: u32) -> bool`
MP2, MP3, AAC, AC-3 and E-AC-3 are legal in MPEG-TS.
