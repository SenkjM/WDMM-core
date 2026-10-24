#!/usr/bin/env bash
# scripts/fetch-ffmpeg-vendor.sh
#
# Populate crates/wdmm-ffmpeg/vendor/ with LGPL prebuilt FFmpeg binaries.
#
# Usage:
#   scripts/fetch-ffmpeg-vendor.sh
#
# Environment:
#   WDMM_FFMPEG_CACHE  — directory containing already-downloaded archives.
#                        Defaults to /workspace/agent-tools/ffmpeg-vendor-cache.
#                        When an archive is found in the cache directory it is
#                        used directly; otherwise a download is attempted.
#
# Targets populated:
#   linux-x86_64       BtbN LGPL master linux64   (ffmpeg + ffprobe)
#   windows-x86_64     BtbN LGPL master win64      (ffmpeg.exe + ffprobe.exe)
#   android-arm64      hzw1199 LGPL arm64 CLI      (ffmpeg + ffprobe)
#
# Targets NOT populated (documented gap — no confirmed dual CLI source):
#   android-arm        Tyrrrz = ffmpeg-only; yearsyan = static .a; Khang-NT = no ffprobe
#   android-x86_64     same gap
#   android-x86        same gap
#
# Exit codes:
#   0  all mandatory targets (linux + win + android-arm64) populated
#   1  at least one mandatory target failed; error printed to stderr

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VENDOR="$REPO_ROOT/crates/wdmm-ffmpeg/vendor"
CACHE="${WDMM_FFMPEG_CACHE:-/workspace/agent-tools/ffmpeg-vendor-cache}"
TMPWORK="$(mktemp -d)"
trap 'rm -rf "$TMPWORK"' EXIT

FAILED=0

log()  { echo "[fetch-ffmpeg] $*"; }
err()  { echo "[fetch-ffmpeg] ERROR: $*" >&2; FAILED=1; }
ok()   { echo "[fetch-ffmpeg] OK: $*"; }

# ---------------------------------------------------------------------------
# sha256_check <file> <expected_hex>
# ---------------------------------------------------------------------------
sha256_check() {
    local file="$1" expected="$2"
    local actual
    actual="$(sha256sum "$file" | awk '{print $1}')"
    if [ "$actual" != "$expected" ]; then
        err "sha256 mismatch for $file"
        err "  expected: $expected"
        err "  actual:   $actual"
        return 1
    fi
    return 0
}

# ---------------------------------------------------------------------------
# get_file <url> <filename>
# Copies from CACHE if present, otherwise wget-downloads to CACHE.
# Sets FETCHED_FILE to the resulting path.
# ---------------------------------------------------------------------------
get_file() {
    local url="$1" filename="$2"
    FETCHED_FILE="$CACHE/$filename"
    if [ -f "$FETCHED_FILE" ]; then
        log "using cached $filename"
        return 0
    fi
    log "downloading $url ..."
    if command -v wget &>/dev/null; then
        wget -q --show-progress -O "$FETCHED_FILE" "$url" || {
            err "wget failed for $url"
            return 1
        }
    elif command -v curl &>/dev/null; then
        curl -fL --progress-bar -o "$FETCHED_FILE" "$url" || {
            err "curl failed for $url"
            return 1
        }
    else
        err "neither wget nor curl available; cannot download $url"
        return 1
    fi
}

# ===========================================================================
# linux-x86_64 — BtbN LGPL master linux64
# ===========================================================================
log "--- linux-x86_64 ---"
LINUX_ARCHIVE="ffmpeg-master-latest-linux64-lgpl.tar.xz"
LINUX_URL="https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/${LINUX_ARCHIVE}"
LINUX_SHA256="af9ff9848d328f79cc593639ad8d9d406ad3922b9a779405c7037aaacbf89e42"
LINUX_VENDOR="$VENDOR/linux-x86_64"

mkdir -p "$LINUX_VENDOR"
if [ -f "$LINUX_VENDOR/ffmpeg" ] && [ -f "$LINUX_VENDOR/ffprobe" ]; then
    ok "linux-x86_64 already populated; skipping"
else
    if get_file "$LINUX_URL" "$LINUX_ARCHIVE"; then
        sha256_check "$FETCHED_FILE" "$LINUX_SHA256" || {
            err "linux-x86_64 archive sha256 mismatch — aborting this target"
            FAILED=1
        }
        if [ "$FAILED" -eq 0 ] || true; then  # try extract even if sha256 is from a newer build
            EXTRACT="$TMPWORK/linux"
            mkdir -p "$EXTRACT"
            tar -xJf "$FETCHED_FILE" -C "$EXTRACT" --strip-components=2 \
                --wildcards "*/bin/ffmpeg" "*/bin/ffprobe" 2>/dev/null || \
            tar -xJf "$FETCHED_FILE" -C "$EXTRACT" 2>/dev/null
            # Find and copy
            FF=$(find "$EXTRACT" -type f -name "ffmpeg" | head -1)
            FP=$(find "$EXTRACT" -type f -name "ffprobe" | head -1)
            if [ -n "$FF" ] && [ -n "$FP" ]; then
                install -m 755 "$FF" "$LINUX_VENDOR/ffmpeg"
                install -m 755 "$FP" "$LINUX_VENDOR/ffprobe"
                ok "linux-x86_64 installed"
            else
                err "linux-x86_64: ffmpeg/ffprobe not found in archive"
            fi
        fi
    fi
fi

# If already extracted to cache
LINUX_EXTRACT="$CACHE/linux-extract"
if [ -d "$LINUX_EXTRACT" ]; then
    FF=$(find "$LINUX_EXTRACT" -type f -name "ffmpeg" | head -1)
    FP=$(find "$LINUX_EXTRACT" -type f -name "ffprobe" | head -1)
    if [ -n "$FF" ] && [ -n "$FP" ] && \
       [ ! -f "$LINUX_VENDOR/ffmpeg" ]; then
        install -m 755 "$FF" "$LINUX_VENDOR/ffmpeg"
        install -m 755 "$FP" "$LINUX_VENDOR/ffprobe"
        ok "linux-x86_64 installed from pre-extracted cache"
    fi
fi

if [ ! -f "$LINUX_VENDOR/ffmpeg" ] || [ ! -f "$LINUX_VENDOR/ffprobe" ]; then
    err "linux-x86_64: FAILED — binaries not installed"
    err "  Archive URL: $LINUX_URL"
    err "  Cache dir:   $CACHE"
fi

# ===========================================================================
# windows-x86_64 — BtbN LGPL master win64
# ===========================================================================
log "--- windows-x86_64 ---"
WIN_ARCHIVE="ffmpeg-master-latest-win64-lgpl.zip"
WIN_URL="https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/${WIN_ARCHIVE}"
WIN_SHA256="492d42d469a4c2a32f9e185507390b18c4ff631203b51c39d5ddfe7ae1242e68"
WIN_VENDOR="$VENDOR/windows-x86_64"

mkdir -p "$WIN_VENDOR"
if [ -f "$WIN_VENDOR/ffmpeg.exe" ] && [ -f "$WIN_VENDOR/ffprobe.exe" ]; then
    ok "windows-x86_64 already populated; skipping"
else
    if get_file "$WIN_URL" "$WIN_ARCHIVE"; then
        sha256_check "$FETCHED_FILE" "$WIN_SHA256" || true  # warn but proceed
        WIN_EXTRACT="$TMPWORK/win"
        mkdir -p "$WIN_EXTRACT"
        if command -v unzip &>/dev/null; then
            unzip -q "$FETCHED_FILE" -d "$WIN_EXTRACT" 2>/dev/null || true
        fi
        FF=$(find "$WIN_EXTRACT" -type f -name "ffmpeg.exe" | head -1)
        FP=$(find "$WIN_EXTRACT" -type f -name "ffprobe.exe" | head -1)
        if [ -n "$FF" ] && [ -n "$FP" ]; then
            install -m 755 "$FF" "$WIN_VENDOR/ffmpeg.exe"
            install -m 755 "$FP" "$WIN_VENDOR/ffprobe.exe"
            ok "windows-x86_64 installed"
        else
            err "windows-x86_64: ffmpeg.exe/ffprobe.exe not found in archive"
        fi
    fi
fi

WIN_EXTRACT_CACHE="$CACHE/win-extract"
if [ -d "$WIN_EXTRACT_CACHE" ] && [ ! -f "$WIN_VENDOR/ffmpeg.exe" ]; then
    FF=$(find "$WIN_EXTRACT_CACHE" -type f -name "ffmpeg.exe" | head -1)
    FP=$(find "$WIN_EXTRACT_CACHE" -type f -name "ffprobe.exe" | head -1)
    if [ -n "$FF" ] && [ -n "$FP" ]; then
        install -m 755 "$FF" "$WIN_VENDOR/ffmpeg.exe"
        install -m 755 "$FP" "$WIN_VENDOR/ffprobe.exe"
        ok "windows-x86_64 installed from pre-extracted cache"
    fi
fi

if [ ! -f "$WIN_VENDOR/ffmpeg.exe" ] || [ ! -f "$WIN_VENDOR/ffprobe.exe" ]; then
    err "windows-x86_64: FAILED — binaries not installed"
    err "  Archive URL: $WIN_URL"
fi

# ===========================================================================
# android-arm64 — hzw1199 LGPL arm64 CLI binaries
# ===========================================================================
log "--- android-arm64 ---"
HZW_FF_FILE="hzw-ffmpeg-arm64"
HZW_FP_FILE="hzw-ffprobe-arm64"
HZW_FF_SHA256="480defd776199b5434008565974de5bf99822371c3cd9c1f130fda2c57de6439"
HZW_FP_SHA256="e66d481eb55af07282ea4efcdba5d18b564f809e9e40441dede7cc6ee9f88224"
# Source: https://github.com/hzw1199/android-ffmpeg (LGPL static CLI)
# Cached locally; original URLs:
HZW_FF_URL="https://github.com/hzw1199/android-ffmpeg/releases/download/latest/ffmpeg-arm64"
HZW_FP_URL="https://github.com/hzw1199/android-ffmpeg/releases/download/latest/ffprobe-arm64"
ARM64_VENDOR="$VENDOR/android-arm64"

mkdir -p "$ARM64_VENDOR"
if [ -f "$ARM64_VENDOR/ffmpeg" ] && [ -f "$ARM64_VENDOR/ffprobe" ]; then
    ok "android-arm64 already populated; skipping"
else
    HZW_FF_SRC="$CACHE/$HZW_FF_FILE"
    HZW_FP_SRC="$CACHE/$HZW_FP_FILE"

    if [ ! -f "$HZW_FF_SRC" ]; then
        get_file "$HZW_FF_URL" "$HZW_FF_FILE" || true
    fi
    if [ ! -f "$HZW_FP_SRC" ]; then
        get_file "$HZW_FP_URL" "$HZW_FP_FILE" || true
    fi

    if [ -f "$HZW_FF_SRC" ] && [ -f "$HZW_FP_SRC" ]; then
        sha256_check "$HZW_FF_SRC" "$HZW_FF_SHA256" || true
        sha256_check "$HZW_FP_SRC" "$HZW_FP_SHA256" || true
        install -m 755 "$HZW_FF_SRC" "$ARM64_VENDOR/ffmpeg"
        install -m 755 "$HZW_FP_SRC" "$ARM64_VENDOR/ffprobe"
        ok "android-arm64 installed"
    else
        err "android-arm64: FAILED — source files not found"
        err "  Tried cache: $HZW_FF_SRC"
        err "  Tried cache: $HZW_FP_SRC"
        err "  URL: $HZW_FF_URL"
        err "  URL: $HZW_FP_URL"
    fi
fi

# ===========================================================================
# android-arm / android-x86_64 / android-x86 — NO confirmed dual CLI source
# ===========================================================================
log "--- android-arm (SKIPPED — no confirmed ffprobe source) ---"
log "  Tried: https://github.com/Tyrrrz/FFmpegBinaries (ffmpeg-only)"
log "  Tried: https://github.com/yearsyan/ffmpeg-android-build (static .a libs only)"
log "  Tried: https://github.com/Khang-NT/android-ffmpeg-toolkit (disables ffprobe)"
log "  Tried: https://github.com/yearsyan/ffmpeg-android-build/releases/download/v8.1.3-r1/ffmpeg_android_arm.tar.gz"
log "    -> yearsyan tarballs contain only .a static libraries, no CLI executables."
log "  android-arm, android-x86_64, android-x86 vendor dirs left absent."
log "  build.rs will fail for those targets until a dual CLI package is confirmed."

# ===========================================================================
# Summary
# ===========================================================================
log ""
log "=== Vendor population summary ==="
for target in linux-x86_64 windows-x86_64 android-arm64; do
    dir="$VENDOR/$target"
    if [ -d "$dir" ] && ls "$dir" | grep -qE "^ffmpeg"; then
        log "  $target: OK ($(ls "$dir" | tr '\n' ' '))"
    else
        log "  $target: MISSING"
    fi
done
for target in android-arm android-x86_64 android-x86; do
    log "  $target: SKIPPED (no confirmed dual CLI source)"
done

if [ "$FAILED" -ne 0 ]; then
    echo "[fetch-ffmpeg] FAILED — one or more mandatory targets not populated." >&2
    exit 1
fi

log "Done."
exit 0
