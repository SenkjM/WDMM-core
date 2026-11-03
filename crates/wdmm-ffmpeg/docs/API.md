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
- `input_url: String`, `preset: TranscodePreset` (codec/bitrate/`hls_segment_secs`)
- `start_offset_secs: f64` — media time to start from (`0.0` = head)
- `segment_dir: PathBuf` — caller-provided; ignored when `out_dir` is set
- `start_number: u64` — index of the first segment, so a pull job can restart
  mid-film and cover the requested `k`
- `out_dir: Option<PathBuf>` — persistent VOD cache; created here and **not**
  deleted on drop. `None` means a temp dir owned (and removed) by the job.
- `hls_time_override_ms: Option<u32>` — `hls_time` in ms. `Some(1)` makes the
  HLS muxer cut on every video keyframe (copy/hybrid) or every forced IDR
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
- `encoder_info()` (`mode` = `remux`/`hybrid`/`hw`/`sw`), `warnings()`
  (deduplicated fallbacks).
Dropping the handle cancels and joins the job.

## Segment plan
The proxy synthesises a VOD playlist from a `SegmentPlan` before playback, so
the renderer can seek without reloading it. There are exactly two alignments —
by design there is **no approximate/calibrated grid**, because a plan whose
boundaries are not real cut points makes seeking land on the wrong segment:
- `Exact` — forced re-encode: strict `k * seg_secs`. The encoder emits one IDR
  per step (closed GOP) and `hls_time` is tiny, so the muxer cuts on the grid.
- `Keyframe` — copy/hybrid with a usable container keyframe index (mp4 `stss`,
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
- `plan: SegmentPlan`, `video_copied: bool`, `keyframe_count: usize`
### `pub fn source_plan(url: &str, preset: &TranscodePreset) -> FfmpegResult<Option<SourcePlan>>`
Opens the source once, decides the alignment up front (video copy ⇒ keyframe
grid; re-encode or unusable index ⇒ exact grid) and returns `None` when the plan
cannot be decided (audio-only source, unknown duration).
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
