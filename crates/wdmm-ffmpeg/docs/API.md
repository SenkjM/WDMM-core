# wdmm-ffmpeg — Public API

In-process FFmpeg (libav*) bindings: probing, HLS segment jobs, resumable
segment plans, remux/encoder planning and subtitle conversion. The crate links
the `libav*.so` shared libraries that are built in-process with `libwdmm_ffi.so`
(`--features ffmpeg`); it never spawns an `ffmpeg`/`ffprobe` subprocess. Without
the feature the same API compiles, and job entry points return
`FfmpegError::Unavailable`.

FFmpeg is LGPL/GPL — see `docs/FFMPEG-ATTRIBUTION.md`. WDMM is AGPL-3.0.

---

## Availability and errors

### `pub const fn is_available() -> bool`

`true` when built with the `ffmpeg` feature; callers use it to disable cast UI.

### `pub type FfmpegResult<T> = std::result::Result<T, FfmpegError>;`

### `pub enum FfmpegError {`

- `Unavailable(String)` — the crate was built without the `ffmpeg` feature.
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

Accepts a local path or an `http(s)://` URL (the DLNA session passes a loopback
proxy URL for encrypted sources).

## Subtitle extraction

### `pub fn extract_subtitle_track(url: &str, stream_index: u32) -> FfmpegResult<String>`

Decodes one text subtitle stream and returns WebVTT. Bitmap codecs (PGS, DVD,
DVB) are rejected — classify with `classify_codec` first. Requires the
`ffmpeg` feature.

## HLS job

### `pub struct JobOpts {`

- `input_url: String` — local path or `http(s)://` URL.
- `preset: TranscodePreset` — codec/bitrate/`hls_segment_secs` selection.
- `start_offset_secs: f64` — media time to start producing from (`0.0` = head).
- `segment_dir: PathBuf` — caller-provided directory; ignored when `out_dir` is set.
- `start_number: u64` — index of the first segment (`seg<k>.ts`), so a pull job
  can restart mid-film and cover the requested `k`.
- `out_dir: Option<PathBuf>` — persistent VOD cache directory. When set the job
  creates it and does **not** delete it on drop; when `None` the job owns a
  temp directory and removes it.
- `hls_time_override_ms: Option<u32>` — `hls_time` in milliseconds, aligned to
  a multiple of the measured GOP after calibration so cuts stop drifting.

### `pub fn open_job(opts: JobOpts) -> FfmpegResult<JobHandle>`

Starts the job on a background OS thread and returns immediately; the caller
polls progress. Admission is the caller's responsibility — `wdmm-proxy` holds a
single process-wide `JobGate` permit per run.

### `pub struct JobHandle {`

- `pub segment_dir: PathBuf`
- `pub fn segment_dir(&self) -> &Path`
- `pub fn out_dir(&self) -> &Path` — same directory as `segment_dir`; the VOD
  cache speaks in out dirs, the legacy proxy path in segment dirs.
- `pub fn ready_segments(&self) -> usize` — non-blocking directory snapshot.
- `pub fn segment_durations(&self) -> Vec<i64>` — `EXTINF` values; empty until
  the first segment is complete.
- `pub fn cancel(&self)`, `pub fn is_running(&self) -> bool`
- `pub fn take_result(&mut self) -> Option<FfmpegResult<()>>` — `None` while running.
- `pub fn join(mut self) -> FfmpegResult<()>` — blocks until the thread exits.
- `pub fn progress(&self) -> ProgressSnapshot` (`content_ms`, `wall_ms`, `packets`)
- `pub fn encoder_info(&self) -> EncoderInfo` (`mode`, `video`, `audio`,
  `hw_requested`, `hw_available`; mode is `remux`/`hybrid`/`hw`/`sw`)
- `pub fn warnings(&self) -> Vec<String>` — deduplicated encoder fallbacks.

Dropping a handle cancels and joins the job.

## Segment plan

The proxy synthesises a VOD playlist from a `SegmentPlan` before playback, so
the renderer can seek without reloading it. Four alignments describe how the
segment boundaries relate to the media:

- `PlanAlignment::Exact` — transcode grid, strict `k * seg_secs`; the job seeks
  and decodes per segment, so cuts land on the grid regardless of keyframes.
- `PlanAlignment::Keyframe` — copy/hybrid with a container keyframe index
  (mp4 `stss`, mkv `cues`); `EXTINF` matches the real output.
- `PlanAlignment::Calibrated` — copy/hybrid without an index; warm-up measures
  real durations and the plan steps on the measured GOP multiple.
- `PlanAlignment::Approx` — VFR/no-index fallback on a nominal grid. Only this
  alignment implies user-visible drift; the caller reports it (`drift_ms`).

### `pub enum PlanAlignment {`

### `pub struct SegmentPlan {`

- `duration_ms: i64`, `seg_secs: f64`, `alignment: PlanAlignment`,
  `gop_ms: Option<i64>`, `entries: Vec<SegEntry>`
- `pub fn segment_count(&self) -> usize`
- `pub fn start_ms(&self, k: u64) -> Option<i64>`
- `pub fn duration_of(&self, k: u64) -> Option<i64>`

### `pub struct SegEntry {`

- `start_ms: i64`, `duration_ms: i64`

### `pub struct KeyframeInfo {`

- `pts_ms: i64`

### `pub fn exact_plan(duration_ms: i64, seg_secs: f64) -> SegmentPlan`

Never drifts because the transcode job seeks per segment.

### `pub fn approximate_plan(duration_ms: i64, seg_secs: f64) -> SegmentPlan`

Nominal grid; the caller reports the drift instead of modelling it.

### `pub fn calibration_plan(duration_ms: i64, seg_secs: f64, gop_ms: i64) -> SegmentPlan`

Step is `ceil(seg_secs*1000 / gop_ms) * gop_ms`. An implausible `gop_ms`
(< 250 ms or > 30 s) degrades to `approximate_plan`.

### `pub fn keyframe_plan(duration_ms: i64, seg_secs: f64, keyframes: &[KeyframeInfo]) -> SegmentPlan`

Uses the HLS muxer cut rule ("first keyframe at/after `seg_secs` since the
previous cut"); without usable keyframes it degrades to `approximate_plan`.

### `pub fn measure_gop_from_durations(durations_ms: &[i64]) -> Option<i64>`

Infers one GOP unit from measured segment durations; returns `None` when the
sample is too small or not plausible (250 ms–30 s).

### `pub fn keyframe_index(url: &str) -> FfmpegResult<Vec<KeyframeInfo>>`

Reads a container index only (never scans the whole file). Returns an empty
vector when the feature is off or the container has no index.

### `pub fn keyframe_index_available() -> bool`

Whether this build can read a real keyframe index.

### `pub fn measure_segment_durations(dir: &Path) -> Vec<i64>`

Parses `EXTINF` values from `<dir>/index.m3u8`.

## Diagnostics

### `pub fn log(msg: impl AsRef<str>)`

### `pub fn tail(max_lines: usize) -> Vec<String>`

### `pub fn set_file(path: Option<PathBuf>)`

### `pub fn clear()`

Process-global ring buffer (512 lines, elapsed-ms prefix). `set_file` mirrors
new lines to a file for `adb exec-out run-as` inspection; works without libav.

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
