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
- `subtitles: Vec<SubtitleStream>` (`index`, `codec`, `language`)
- `format_name: String`
### `pub fn probe(url: &str) -> FfmpegResult<ProbeResult>`
Accepts a local path or an `http(s)://` URL (the proxy passes its loopback URL
for encrypted sources).

## Subtitle extraction
### `pub fn extract_subtitle_track(url: &str, stream_index: u32) -> FfmpegResult<String>`
Decodes one text subtitle stream to WebVTT. Bitmap codecs (PGS, DVD, DVB) are
rejected — classify with `classify_codec` first. Requires the `ffmpeg` feature.

## HLS job
### `pub struct JobOpts {`
- `input_url: String`, `preset: TranscodePreset` (codec/bitrate/`hls_segment_secs`)
- `start_offset_secs: f64` — media time to start from (`0.0` = head)
- `segment_dir: PathBuf` — caller-provided; ignored when `out_dir` is set
- `start_number: u64` — index of the first segment, so a pull job can restart
  mid-film and cover the requested `k`
- `out_dir: Option<PathBuf>` — persistent VOD cache; created here and **not**
  deleted on drop. `None` means a temp dir owned (and removed) by the job.
- `hls_time_override_ms: Option<u32>` — `hls_time` in ms, aligned to the
  measured GOP after calibration so cuts stop drifting
### `pub fn open_job(opts: JobOpts) -> FfmpegResult<JobHandle>`
Starts the job on a background OS thread and returns immediately. Admission is
the caller's job — `wdmm-proxy` holds the process-wide `JobGate` permit.
### `pub struct JobHandle {`
- `pub segment_dir: PathBuf`; `segment_dir()` / `out_dir()` return the same dir.
- `ready_segments()` — non-blocking directory snapshot.
- `segment_durations()` — `EXTINF` values; empty until the first segment.
- `cancel()`, `is_running()`, `take_result()` (`None` while running), `join()`.
- `progress()` (`content_ms`, `wall_ms`, `packets`), `encoder_info()`
  (`mode` = `remux`/`hybrid`/`hw`/`sw`), `warnings()` (deduplicated fallbacks).
Dropping the handle cancels and joins the job.

## Segment plan
The proxy synthesises a VOD playlist from a `SegmentPlan` before playback, so
the renderer can seek without reloading it:
- `Exact` — strict `k * seg_secs`; the job seeks and decodes per segment, so
  cuts land on the grid regardless of keyframes.
- `Keyframe` — copy/hybrid with a container index (mp4 `stss`, mkv `cues`);
  `EXTINF` matches the real output exactly.
- `Calibrated` — copy/hybrid without an index; warm-up measures real segment
  durations and the plan steps on the measured GOP multiple.
- `Approx` — VFR/no-index fallback on a nominal grid. Only this alignment
  implies user-visible drift; the caller reports it (`drift_ms`).
### `pub enum PlanAlignment {`
### `pub struct SegmentPlan {`
- `duration_ms: i64`, `seg_secs: f64`, `alignment: PlanAlignment`,
  `gop_ms: Option<i64>`, `entries: Vec<SegEntry>`
- `segment_count()`, `start_ms(k) -> Option<i64>`, `duration_of(k) -> Option<i64>`
### `pub struct SegEntry {`
- `start_ms: i64`, `duration_ms: i64`
### `pub struct KeyframeInfo {`
- `pts_ms: i64`
### `pub fn exact_plan(duration_ms: i64, seg_secs: f64) -> SegmentPlan`
Never drifts because the transcode job seeks per segment.
### `pub fn approximate_plan(duration_ms: i64, seg_secs: f64) -> SegmentPlan`
Nominal grid; the caller reports the drift instead of modelling it.
### `pub fn calibration_plan(duration_ms: i64, seg_secs: f64, gop_ms: i64) -> SegmentPlan`
Step is `ceil(seg_secs*1000 / gop_ms) * gop_ms`; an implausible GOP
(< 250 ms or > 30 s) degrades to `approximate_plan`.
### `pub fn keyframe_plan(duration_ms: i64, seg_secs: f64, keyframes: &[KeyframeInfo]) -> SegmentPlan`
Uses the HLS muxer cut rule ("first keyframe at/after `seg_secs` since the
previous cut"); without usable keyframes it degrades to `approximate_plan`.
### `pub fn measure_gop_from_durations(durations_ms: &[i64]) -> Option<i64>`
Infers one GOP unit; `None` when the sample is too small or implausible.
### `pub fn keyframe_index(url: &str) -> FfmpegResult<Vec<KeyframeInfo>>`
Reads a container index only, never the whole file; empty when the feature is
off or the container has no index.
### `pub fn keyframe_index_available() -> bool`
Whether this build can read a real keyframe index (path: `wdmm_ffmpeg::vod::keyframe_index_available`).
### `pub fn measure_segment_durations(dir: &Path) -> Vec<i64>`
Parses `EXTINF` values from `<dir>/index.m3u8`.

## Diagnostics
### `pub fn log(msg: impl AsRef<str>)`
### `pub fn tail(max_lines: usize) -> Vec<String>`
### `pub fn set_file(path: Option<PathBuf>)`
### `pub fn clear()`
Process-global ring buffer (512 lines, elapsed-ms prefix). `set_file` mirrors
new lines to a file for `adb exec-out run-as`; works without libav.

## Presets and MPEG-TS copy checks
### `pub fn preset_passthrough() -> TranscodePreset`
### `pub fn preset_dlna_compat() -> TranscodePreset`
### `pub fn preset_balanced() -> TranscodePreset`
### `pub fn preset_high() -> TranscodePreset`
### `pub fn preset_audio_only() -> TranscodePreset`
### `pub fn all_builtin_presets() -> [TranscodePreset; 5]`
In order: `passthrough`, `dlna-compat`, `balanced`, `high`, `audio-only`.
### `pub fn is_mpegts_video_copy_compatible(codec_id: u32) -> bool`
H.264, HEVC and MPEG-2 can be copied into MPEG-TS.
### `pub fn is_mpegts_audio_copy_compatible(codec_id: u32) -> bool`
AAC, MP3, AC-3 and E-AC-3 can be copied into MPEG-TS.
