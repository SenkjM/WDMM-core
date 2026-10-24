# wdmm-ffmpeg — Public API

Vendored FFmpeg bindings. Provides media probe and HLS transcode job
management. All I/O is routed through the vendor library — no system FFmpeg,
no git submodule.

> **Round 11 skeleton** — stubs only; no vendored binaries are linked yet.
> See `build.rs` TODO and `vendor/` layout comments.
>
> **License** — This crate links FFmpeg (LGPL v2.1+; optionally GPL v2+ for
> certain components). See `docs/FFMPEG-ATTRIBUTION.md` for the full
> third-party attribution note. WDMM is distributed under AGPL-3.0.

---

## Error types

### `enum FfmpegError`
- `Unavailable(String)` — vendored binary not yet linked.
- `Probe(String)` — probe subprocess failed.
- `Io(#[from] std::io::Error)` — file-system I/O.

### `type FfmpegResult<T> = Result<T, FfmpegError>`

---

## Probe

### `struct ProbeResult`
Fields: `duration_secs: f64`, `video: Vec<VideoStream>`,
`audio: Vec<AudioStream>`, `subtitles: Vec<SubtitleStream>`,
`format_name: String`.

#### `struct VideoStream`
`index`, `codec`, `width`, `height`, `fps`.

#### `struct AudioStream`
`index`, `codec`, `channels`, `sample_rate`, `language: Option<String>`.

#### `struct SubtitleStream`
`index`, `codec`, `language: Option<String>`.

### `fn probe(path: &Path) -> FfmpegResult<ProbeResult>`
Probe a local file via `ffprobe` (JSON output). **Stub** — always returns
`Err(FfmpegError::Unavailable)` until vendored binaries are linked.

---

## HLS transcode job

### `enum HlsMode`
```rust
HlsMode::DirectCopy                          // -c copy (default)
HlsMode::Compat { video_codec, audio_codec } // transcode (H.264 + AAC default)
```
`DirectCopy` remuxes compatible media into HLS segments without transcoding.
`Compat` transcodes; seek is implemented by killing the job and restarting
with `-ss`. No Range on transcode stdout (brief §3).

### `enum SubtitleMode`
```rust
SubtitleMode::Off                        // default; no burn-in, no HDR tonemap
SubtitleMode::BurnIn { stream_index }    // embedded stream, Compat only
SubtitleMode::SidecarBurnIn { path }     // .srt etc., Compat only
```

### `struct JobOpts`
Fields: `input: PathBuf`, `mode: HlsMode`, `subtitles: SubtitleMode`,
`start_offset_secs: f64`, `segment_duration_secs: u32` (default `6`).

### `struct JobHandle`
Handle to an active or completed HLS transcode job.

Fields (pub):
- `segment_dir: PathBuf` — directory where `index.m3u8` + numbered
  segments (`0.ts`, `1.ts`, …) are written.

Methods:
- `fn segment_dir(&self) -> &Path` — accessor.
- `fn kill(&mut self)` — kill the process (no-op in stub mode).

`Drop` calls `kill()` and removes `segment_dir` (best-effort).

### `fn open_job(opts: JobOpts) -> FfmpegResult<JobHandle>`
Open a new HLS transcode job.

Creates `segment_dir` in `$TMPDIR` and (once vendored binaries are linked)
spawns the FFmpeg process writing:
```
ffmpeg -ss <start_offset_secs> -i <input> [codec flags]
       -hls_time <segment_duration_secs> -hls_list_size 0 -f hls
       <segment_dir>/index.m3u8
```
**Stub** — directory is created but no process is spawned. Returns
`Ok(JobHandle)` so the proxy skeleton can exercise the full session
lifecycle without FFmpeg.

---

## Seek pattern (proxy side)

Seek is implemented by the proxy:
1. Call `handle.seek_hls(token, offset_secs)`.
2. Proxy calls `open_job` with the same input + mode but
   `start_offset_secs = offset_secs`.
3. Old `JobHandle` drops → process killed, old segment_dir removed.
4. Renderer continues from the new `index.m3u8`.

---

## Segment session config (brief §3)

| Setting | Value |
|---------|-------|
| Segment duration | ~6 s |
| Session token dir | `$TMPDIR/wdmm-hls-{id}` |
| Idle TTL | ~10 min (enforced by wdmm-proxy) |
| Max concurrent jobs | 1 per device/media (enforced by caller) |
| HDR tone mapping | disabled |
| Subtitle burn-in | off by default |
