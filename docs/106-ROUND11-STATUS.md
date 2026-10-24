# Round 11 Status — wdmm-ffmpeg + proxy HLS skeleton

## SHAs at round start

| Repo | Branch | SHA |
|------|--------|-----|
| WDMM-core | `feat/proxy-hls-ffmpeg` | `d8b2217` (Round 10 tip) |

## What landed

| # | Item | File(s) | Notes |
|---|------|---------|-------|
| 1 | **New crate `wdmm-ffmpeg`** — API stubs | `crates/wdmm-ffmpeg/Cargo.toml` `src/lib.rs` | `probe`, `open_job`, `JobHandle` (kill + drop cleanup), `HlsMode`, `SubtitleMode`, `JobOpts`, `ProbeResult`. All stubs — no vendored binary. |
| 2 | **build.rs multi-target** | `crates/wdmm-ffmpeg/build.rs` | Detects Android ABIs + Windows x86_64; has commented-out `rustc-link-*` lines ready for when vendored libs land. |
| 3 | **wdmm-proxy HLS session API** | `crates/wdmm-proxy/src/lib.rs` | `HlsSession`, `HlsRegistration`, `register_hls`, `seek_hls`, `unregister_hls`; `Ffmpeg` error variant; `Shared.hls_sessions`; routes `/h/:token/*seg`; `serve_hls` handler. |
| 4 | **wdmm-ffi HLS exports** | `crates/wdmm-ffi/src/lib.rs` | `wdmm_proxy_register_hls`, `wdmm_proxy_hls_seek`, `wdmm_proxy_unregister_hls`; `parse_hls_mode` JSON helper. |
| 5 | **Workspace Cargo.toml** | `Cargo.toml` | `wdmm-ffmpeg` added to `members` + `[workspace.dependencies]`. |
| 6 | **wdmm-ffmpeg docs** | `crates/wdmm-ffmpeg/docs/API.md` `docs/FFMPEG-ATTRIBUTION.md` | API signature sheet + AGPL + FFmpeg attribution note (LGPL v2.1+). |
| 7 | **wdmm-proxy API.md updated** | `crates/wdmm-proxy/docs/API.md` | HLS session API, `HlsRegistration`, HLS route table. |
| 8 | **wdmm-ffi API.md updated** | `crates/wdmm-ffi/docs/API.md` | Proxy HLS section: register, seek, unregister. |
| 9 | **This status doc** | `docs/106-ROUND11-STATUS.md` | — |

## Test counts

| Crate | Round 10 | Round 11 | Δ |
|-------|---------|---------|---|
| wdmm-ffmpeg | — (new) | 4 | +4 |
| wdmm-proxy | 9 | 11 | +2 |
| wdmm-drivers | 90 | 90 | — |
| wdmm-dlna | 9 | 9 | — |
| wdmm-storage | 5 | 5 | — |
| wdmm-sync | 10 | 10 | — |
| wdmm-openapi | 15 | 15 | — |
| wdmm-queue | 9 | 9 | — |
| wdmm-ffi | 4 | 4 | — |
| wdmm-platform | 8 | 8 | — |
| wdmm-core | 4 | 4 | — |
| wdmm-crypto | 5 | 5 | — |
| **workspace total** | **163** | **169** | **+6** |

All 169 tests green, 0 failed, 1 ignored (pre-existing).

## Architecture decisions

| Decision | Rationale |
|----------|-----------|
| `open_job` is sync | Process spawn is a fast system call; actual transcoding is background. Async wrapper deferred to round 12 when real process management lands. |
| `HlsMode` lives in `wdmm-ffmpeg` | Avoids two representations; `wdmm-proxy` and `wdmm-ffi` import it directly. |
| `JobHandle::drop` removes segment dir | Ensures cleanup even on panic or early return; mirrors the token TTL in the proxy. |
| Stub `open_job` returns `Ok(...)` | Lets the proxy skeleton run the full register/seek/unregister lifecycle without a real binary; tests can assert 404 on segments. |
| `parse_hls_mode` in wdmm-ffi | JSON → `HlsMode` conversion is a thin FFI concern, not a proxy or ffmpeg concern. |

## Blockers / TODOs

| Item | Priority | Notes |
|------|----------|-------|
| Download / vendor FFmpeg prebuilts | P1 | `build.rs` has the link logic commented out; needs `vendor/<target>/` populated. |
| Real `ffmpeg` process spawn in `open_job` | P1 | Blocked on vendored binaries. Use `tokio::process::Command`. |
| `probe` implementation | P1 | Blocked on vendored `ffprobe`. |
| One-transcode-per-device/media guard | P2 | Caller (DLNA cast path) must check before calling `register_hls`; no lock inside proxy yet. |
| Per-device DMR settings + USN keying | P2 | Spec §3: friendly error, no auto fallback; deferred to DLNA integration round. |
| HLS `EXT-X-DISCONTINUITY` tag on seek | P3 | Needed for smooth renderer handling; add to playlist writer when real ffmpeg lands. |
