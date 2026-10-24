# FFmpeg Third-Party Attribution

WDMM-core is distributed under the GNU Affero General Public License v3.0
(AGPL-3.0). The `wdmm-ffmpeg` crate uses **FFmpeg** command-line binaries
(`ffmpeg`, `ffprobe`), which are Copyright © the FFmpeg developers and
licensed under:

- **GNU Lesser General Public License (LGPL) v2.1 or later** — the default
  linking configuration.
- **GNU General Public License (GPL) v2 or later** — applies when components
  that require it are enabled (e.g. `libx264`/GPL build, `libfdk-aac`).

## Source

- Project home: <https://ffmpeg.org/>
- Source tarballs: <https://ffmpeg.org/download.html>
- Git mirror: <https://git.ffmpeg.org/ffmpeg.git>

## Vendored binaries

The `wdmm-ffmpeg/vendor/<target>/` directories contain prebuilt FFmpeg
command-line binaries for each supported target:

| Target | Directory | Download Source |
|--------|-----------|-----------------|
| Host Linux x86_64 | System PATH | System package (`apt install ffmpeg`) or https://ffmpeg.org/download.html |
| Android `aarch64` | `vendor/android-arm64/` | https://github.com/Javernaut/ffmpeg-android-maker or build from source |
| Android `armv7` | `vendor/android-arm/` | https://github.com/Javernaut/ffmpeg-android-maker |
| Android `x86_64` | `vendor/android-x86_64/` | https://github.com/Javernaut/ffmpeg-android-maker |
| Windows `x86_64` | `vendor/windows-x86_64/` | https://www.gyan.dev/ffmpeg/builds/ or https://github.com/BtbN/FFmpeg-Builds |

### Recommended builds

- **Android**: LGPL shared builds from `ffmpeg-android-maker` with `--enable-gpl=false`.
- **Windows**: LGPL shared builds from https://www.gyan.dev/ffmpeg/builds/ (essentials or full).

Binaries are **gitignored** (see `.gitignore`) to avoid bloating the repository.
Place the downloaded `ffmpeg` / `ffprobe` (or `.exe`) into the appropriate
`vendor/<target>/` directory before building for that target.

## Configuration

The vendored FFmpeg is built with a configuration compatible with the LGPL
unless a GPL variant is explicitly selected. The build flags used to produce
each vendored binary are recorded in `vendor/<target>/BUILD-FLAGS.txt`
alongside the binaries (optional; for audit purposes).

## Corresponding source

Per LGPL v2.1 §3 / §6, the complete corresponding source of FFmpeg (including
all modifications, if any) used to build the vendored binaries is available
on request and from the upstream FFmpeg git mirror above.

## License compatibility note

AGPL-3.0 is one-way compatible with LGPL v2.1+ (you may incorporate
LGPL-licensed code into an AGPL-licensed work). The FFmpeg binaries are
executed as separate processes; this notice fulfils the attribution requirement.
This is not legal advice — consult counsel for distribution decisions.
