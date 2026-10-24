# FFmpeg Third-Party Attribution

WDMM-core is distributed under the GNU Affero General Public License v3.0
(AGPL-3.0). The `wdmm-ffmpeg` crate links **FFmpeg** libraries
(`libavcodec`, `libavformat`, `libavutil`, `libswresample`, `libswscale`),
which are Copyright © the FFmpeg developers and licensed under:

- **GNU Lesser General Public License (LGPL) v2.1 or later** — the default
  linking configuration.
- **GNU General Public License (GPL) v2 or later** — applies when components
  that require it are enabled (e.g. `libx264`/GPL build, `libfdk-aac`).

## Source

- Project home: <https://ffmpeg.org/>
- Source tarballs: <https://ffmpeg.org/download.html>
- Git mirror: <https://git.ffmpeg.org/ffmpeg.git>

## Vendored binaries

The `wdmm-ffmpeg/vendor/<target>/` directories ship prebuilt static libraries
for each supported target:

| Target | Directory |
|--------|-----------|
| Android `aarch64` | `vendor/android-arm64/` |
| Android `armv7` | `vendor/android-arm/` |
| Android `x86_64` | `vendor/android-x86_64/` |
| Android `x86` | `vendor/android-x86/` |
| Windows `x86_64` | `vendor/windows-x86_64/` |

> **Round 11**: these directories are not yet populated. See `build.rs` for the
> link configuration and the TODO for the download/unpack step.

## Configuration

The vendored FFmpeg is built with a configuration compatible with the LGPL
unless a GPL variant is explicitly selected. The build flags used to produce
each vendored binary are recorded in `vendor/<target>/BUILD-FLAGS.txt`
alongside the libraries (to be added when binaries are vendored).

## Corresponding source

Per LGPL v2.1 §3 / §6, the complete corresponding source of FFmpeg (including
all modifications, if any) used to build the vendored libraries is available
on request and from the upstream FFmpeg git mirror above.

## License compatibility note

AGPL-3.0 is one-way compatible with LGPL v2.1+ (you may incorporate
LGPL-licensed code into an AGPL-licensed work). The FFmpeg libraries are
linked as separate works; this notice fulfils the attribution requirement.
This is not legal advice — consult counsel for distribution decisions.
