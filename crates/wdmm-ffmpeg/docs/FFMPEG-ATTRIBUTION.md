# FFmpeg Third-Party Attribution

WDMM-core is distributed under the GNU Affero General Public License v3.0
(AGPL-3.0). The `wdmm-ffmpeg` crate uses **FFmpeg** command-line binaries
(`ffmpeg`, `ffprobe`), which are Copyright © the FFmpeg developers and
licensed under the **GNU Lesser General Public License (LGPL) v2.1 or later**.

- Project home: <https://ffmpeg.org/>
- Source: <https://git.ffmpeg.org/ffmpeg.git>

---

## Vendored binaries

Populated by `scripts/fetch-ffmpeg-vendor.sh`. Binaries are gitignored
(`.gitignore`); run the script after cloning to restore them.
There is **no PATH / `which` fallback** — the vendor directory is the only
source. See `crates/wdmm-ffmpeg/build.rs` for the build-time check.

### linux-x86_64

| Field | Value |
|-------|-------|
| Source | BtbN/FFmpeg-Builds (GitHub) |
| Archive | `ffmpeg-master-latest-linux64-lgpl.tar.xz` |
| URL | https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-linux64-lgpl.tar.xz |
| SHA-256 | `af9ff9848d328f79cc593639ad8d9d406ad3922b9a779405c7037aaacbf89e42` |
| License | LGPL v2.1+ |
| Build | Master branch, no GPL components |

### windows-x86_64

| Field | Value |
|-------|-------|
| Source | BtbN/FFmpeg-Builds (GitHub) |
| Archive | `ffmpeg-master-latest-win64-lgpl.zip` |
| URL | https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-lgpl.zip |
| SHA-256 | `492d42d469a4c2a32f9e185507390b18c4ff631203b51c39d5ddfe7ae1242e68` |
| License | LGPL v2.1+ |
| Build | Master branch, no GPL components |

### android-arm64 (aarch64)

| Field | Value |
|-------|-------|
| Source | hzw1199/android-ffmpeg (GitHub) |
| Files | `hzw-ffmpeg-arm64`, `hzw-ffprobe-arm64` |
| URL | https://github.com/hzw1199/android-ffmpeg |
| SHA-256 (ffmpeg) | `480defd776199b5434008565974de5bf99822371c3cd9c1f130fda2c57de6439` |
| SHA-256 (ffprobe) | `e66d481eb55af07282ea4efcdba5d18b564f809e9e40441dede7cc6ee9f88224` |
| License | LGPL v2.1+ |
| Build | Static CLI, LGPL-only components |

---

## Android gaps — arm / x86\_64 / x86

No confirmed dual (`ffmpeg` + `ffprobe`) CLI binary package exists for these
ABIs after exhaustive search (2026-10-25):

| Source | URL tried | Result |
|--------|-----------|--------|
| Tyrrrz/FFmpegBinaries | https://github.com/Tyrrrz/FFmpegBinaries | Ships `ffmpeg` CLI only; no `ffprobe` |
| yearsyan/ffmpeg-android-build v8.1.3-r1 | https://github.com/yearsyan/ffmpeg-android-build/releases/download/v8.1.3-r1/ffmpeg_android_arm.tar.gz | Tarballs contain only `.a` static libraries; no CLI executables |
| Khang-NT/android-ffmpeg-toolkit | https://github.com/Khang-NT/android-ffmpeg-toolkit | Build system explicitly disables ffprobe |

**Policy**: `vendor/android-arm`, `vendor/android-x86_64`, and
`vendor/android-x86` directories are intentionally absent.
`build.rs` panics hard for those targets. `spawn.rs` returns
`FfmpegError::Unavailable` at runtime on those ABIs.

If you find a confirmed dual CLI source for any of these ABIs:
1. Add it to this table with URL + sha256.
2. Update `scripts/fetch-ffmpeg-vendor.sh`.
3. Remove the matching `panic!` arm from `build.rs`.
4. Remove the matching `Err` arm from `spawn.rs`.

---

## LGPL compliance notes

The binaries above use only LGPL v2.1+ licensed FFmpeg components
(no `libx264`/GPL, no `libfdk-aac`/non-free). LGPL requires that:

1. The license text is distributed alongside the binary.
2. The user can re-link with a modified version of FFmpeg.

WDMM invokes these as external CLI tools via `Command::spawn`
(separate processes, not statically linked into the Rust binary).
This satisfies the LGPL dynamic separation requirement.

The FFmpeg LGPL license text: <https://www.gnu.org/licenses/lgpl-2.1.html>

## Corresponding source

Per LGPL v2.1 §3/§6, the complete corresponding FFmpeg source is available
from the upstream git mirror: <https://git.ffmpeg.org/ffmpeg.git>
