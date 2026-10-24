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
#   WDMM_FFMPEG_ONLY   — optional: linux-x86_64 | windows-x86_64 | android-arm64
#                        When set, only that target is fetched/required (CI jobs).
#
# Targets populated:
#   linux-x86_64       BtbN LGPL master linux64   (ffmpeg + ffprobe)
#   windows-x86_64     BtbN LGPL master win64      (ffmpeg.exe + ffprobe.exe)
#   android-arm64      hzw1199 LGPL arm64 CLI      (seeded on WDMM-core release)
#
# Targets NOT populated (documented gap — no confirmed dual CLI source):
#   android-arm / android-x86_64 / android-x86
#
# Exit codes:
#   0  all required targets populated
#   1  at least one required target failed

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

# Portable copy+chmod (Git Bash Windows may lack GNU install).
install_bin() {
    local src="$1" dst="$2"
    mkdir -p "$(dirname "$dst")"
    cp "$src" "$dst"
    chmod 755 "$dst" 2>/dev/null || true
}

need_target() {
    local t="$1"
    if [ -z "${WDMM_FFMPEG_ONLY:-}" ]; then
        return 0
    fi
    [ "$WDMM_FFMPEG_ONLY" = "$t" ]
}

sha256_check() {
    local file="$1" expected="$2"
    local actual
    # Git Bash on Windows may prefix a backslash; strip CR too.
    actual="$(sha256sum "$file" | awk '{print $1}' | tr -d '\r' | sed 's/^\\//')"
    if [ "$actual" != "$expected" ]; then
        log "WARN: sha256 mismatch for $file"
        log "  expected: $expected"
        log "  actual:   $actual"
        return 1
    fi
    return 0
}

get_file() {
    local url="$1" filename="$2"
    mkdir -p "$CACHE"
    FETCHED_FILE="$CACHE/$filename"
    if [ -f "$FETCHED_FILE" ] && [ -s "$FETCHED_FILE" ]; then
        log "using cached $filename"
        return 0
    fi
    rm -f "$FETCHED_FILE"
    log "downloading $url ..."
    if command -v wget &>/dev/null; then
        wget -q --show-progress -O "$FETCHED_FILE" "$url" || {
            rm -f "$FETCHED_FILE"
            err "wget failed for $url"
            return 1
        }
    elif command -v curl &>/dev/null; then
        curl -fL --progress-bar -o "$FETCHED_FILE" "$url" || {
            rm -f "$FETCHED_FILE"
            err "curl failed for $url"
            return 1
        }
    else
        err "neither wget nor curl available; cannot download $url"
        return 1
    fi
    if [ ! -s "$FETCHED_FILE" ]; then
        rm -f "$FETCHED_FILE"
        err "download empty for $url"
        return 1
    fi
}

# ===========================================================================
# linux-x86_64
# ===========================================================================
if ! need_target linux-x86_64; then
    log "--- linux-x86_64 --- SKIPPED (WDMM_FFMPEG_ONLY=${WDMM_FFMPEG_ONLY:-})"
else
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
        sha256_check "$FETCHED_FILE" "$LINUX_SHA256" || \
            log "WARN: linux-x86_64 sha256 mismatch (BtbN latest drifts) — extracting anyway"
        EXTRACT="$TMPWORK/linux"
        mkdir -p "$EXTRACT"
        tar -xJf "$FETCHED_FILE" -C "$EXTRACT" --strip-components=2 \
            --wildcards "*/bin/ffmpeg" "*/bin/ffprobe" 2>/dev/null || \
        tar -xJf "$FETCHED_FILE" -C "$EXTRACT" 2>/dev/null || true
        FF=$(find "$EXTRACT" -type f -name "ffmpeg" | head -1)
        FP=$(find "$EXTRACT" -type f -name "ffprobe" | head -1)
        if [ -n "$FF" ] && [ -n "$FP" ]; then
            install_bin "$FF" "$LINUX_VENDOR/ffmpeg"
            install_bin "$FP" "$LINUX_VENDOR/ffprobe"
            ok "linux-x86_64 installed"
        else
            err "linux-x86_64: ffmpeg/ffprobe not found in archive"
        fi
    fi
fi

LINUX_EXTRACT="$CACHE/linux-extract"
if [ -d "$LINUX_EXTRACT" ]; then
    FF=$(find "$LINUX_EXTRACT" -type f -name "ffmpeg" | head -1)
    FP=$(find "$LINUX_EXTRACT" -type f -name "ffprobe" | head -1)
    if [ -n "$FF" ] && [ -n "$FP" ] && [ ! -f "$LINUX_VENDOR/ffmpeg" ]; then
        install_bin "$FF" "$LINUX_VENDOR/ffmpeg"
        install_bin "$FP" "$LINUX_VENDOR/ffprobe"
        ok "linux-x86_64 installed from pre-extracted cache"
    fi
fi

if [ ! -s "$LINUX_VENDOR/ffmpeg" ] || [ ! -s "$LINUX_VENDOR/ffprobe" ]; then
    err "linux-x86_64: FAILED — binaries not installed"
    err "  Archive URL: $LINUX_URL"
    err "  Cache dir:   $CACHE"
fi
fi  # need_target linux-x86_64

# ===========================================================================
# windows-x86_64
# ===========================================================================
if ! need_target windows-x86_64; then
    log "--- windows-x86_64 --- SKIPPED (WDMM_FFMPEG_ONLY=${WDMM_FFMPEG_ONLY:-})"
else
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
        sha256_check "$FETCHED_FILE" "$WIN_SHA256" || \
            log "WARN: windows-x86_64 sha256 mismatch — extracting anyway"
        WIN_EXTRACT="$TMPWORK/win"
        mkdir -p "$WIN_EXTRACT"
        if command -v unzip &>/dev/null; then
            unzip -q "$FETCHED_FILE" -d "$WIN_EXTRACT" 2>/dev/null || true
        fi
        FF=$(find "$WIN_EXTRACT" -type f -name "ffmpeg.exe" | head -1)
        FP=$(find "$WIN_EXTRACT" -type f -name "ffprobe.exe" | head -1)
        if [ -n "$FF" ] && [ -n "$FP" ]; then
            install_bin "$FF" "$WIN_VENDOR/ffmpeg.exe"
            install_bin "$FP" "$WIN_VENDOR/ffprobe.exe"
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
        install_bin "$FF" "$WIN_VENDOR/ffmpeg.exe"
        install_bin "$FP" "$WIN_VENDOR/ffprobe.exe"
        ok "windows-x86_64 installed from pre-extracted cache"
    fi
fi

if [ ! -s "$WIN_VENDOR/ffmpeg.exe" ] || [ ! -s "$WIN_VENDOR/ffprobe.exe" ]; then
    err "windows-x86_64: FAILED — binaries not installed"
    err "  Archive URL: $WIN_URL"
    err "  Cache dir:   $CACHE"
fi
fi  # need_target windows-x86_64

# ===========================================================================
# android-arm64
# ===========================================================================
if ! need_target android-arm64; then
    log "--- android-arm64 --- SKIPPED (WDMM_FFMPEG_ONLY=${WDMM_FFMPEG_ONLY:-})"
else
log "--- android-arm64 ---"
HZW_FF_FILE="hzw-ffmpeg-arm64"
HZW_FP_FILE="hzw-ffprobe-arm64"
HZW_FF_SHA256="480defd776199b5434008565974de5bf99822371c3cd9c1f130fda2c57de6439"
HZW_FP_SHA256="e66d481eb55af07282ea4efcdba5d18b564f809e9e40441dede7cc6ee9f88224"
# Upstream hzw1199 latest/ 404 as of 2026-10-25; seed on WDMM-core release.
HZW_FF_URL="https://github.com/SenkjM/WDMM-core/releases/download/ffmpeg-vendor-seed-20261025/hzw-ffmpeg-arm64"
HZW_FP_URL="https://github.com/SenkjM/WDMM-core/releases/download/ffmpeg-vendor-seed-20261025/hzw-ffprobe-arm64"
ARM64_VENDOR="$VENDOR/android-arm64"

mkdir -p "$ARM64_VENDOR"
if [ -f "$ARM64_VENDOR/ffmpeg" ] && [ -f "$ARM64_VENDOR/ffprobe" ]; then
    ok "android-arm64 already populated; skipping"
else
    HZW_FF_SRC="$CACHE/$HZW_FF_FILE"
    HZW_FP_SRC="$CACHE/$HZW_FP_FILE"

    if [ ! -f "$HZW_FF_SRC" ] || [ ! -s "$HZW_FF_SRC" ]; then
        get_file "$HZW_FF_URL" "$HZW_FF_FILE" || true
    fi
    if [ ! -f "$HZW_FP_SRC" ] || [ ! -s "$HZW_FP_SRC" ]; then
        get_file "$HZW_FP_URL" "$HZW_FP_FILE" || true
    fi

    if [ -s "$HZW_FF_SRC" ] && [ -s "$HZW_FP_SRC" ]; then
        sha256_check "$HZW_FF_SRC" "$HZW_FF_SHA256" || true
        sha256_check "$HZW_FP_SRC" "$HZW_FP_SHA256" || true
        install_bin "$HZW_FF_SRC" "$ARM64_VENDOR/ffmpeg"
        install_bin "$HZW_FP_SRC" "$ARM64_VENDOR/ffprobe"
        ok "android-arm64 installed"
    else
        err "android-arm64: FAILED — source files not found"
        err "  Tried cache: $HZW_FF_SRC"
        err "  Tried cache: $HZW_FP_SRC"
        err "  URL: $HZW_FF_URL"
        err "  URL: $HZW_FP_URL"
    fi
fi

if [ ! -s "$ARM64_VENDOR/ffmpeg" ] || [ ! -s "$ARM64_VENDOR/ffprobe" ]; then
    err "android-arm64: FAILED — binaries not installed"
fi
fi  # need_target android-arm64

# ===========================================================================
# skipped ABIs
# ===========================================================================
log "--- android-arm / x86_64 / x86 (SKIPPED — no confirmed ffprobe+ffmpeg CLI) ---"

# ===========================================================================
# Summary — recompute FAILED from required target presence
# ===========================================================================
log ""
log "=== Vendor population summary ==="
for target in linux-x86_64 windows-x86_64 android-arm64; do
    dir="$VENDOR/$target"
    if need_target "$target"; then
        if [ -d "$dir" ] && ls "$dir" 2>/dev/null | grep -qE "^ffmpeg"; then
            log "  $target: OK ($(ls "$dir" | tr '\n' ' '))"
        else
            log "  $target: MISSING"
        fi
    else
        log "  $target: SKIPPED (WDMM_FFMPEG_ONLY)"
    fi
done
for target in android-arm android-x86_64 android-x86; do
    log "  $target: SKIPPED (no confirmed dual CLI source)"
done

FAILED=0
require_ok() {
    local t="$1"
    if ! need_target "$t"; then
        return 0
    fi
    local dir="$VENDOR/$t"
    if [ "$t" = "windows-x86_64" ]; then
        if [ ! -s "$dir/ffmpeg.exe" ] || [ ! -s "$dir/ffprobe.exe" ]; then
            err "$t: MISSING after fetch"
            return 1
        fi
    else
        if [ ! -s "$dir/ffmpeg" ] || [ ! -s "$dir/ffprobe" ]; then
            err "$t: MISSING after fetch"
            return 1
        fi
    fi
    return 0
}
require_ok linux-x86_64 || true
require_ok windows-x86_64 || true
require_ok android-arm64 || true

if [ "$FAILED" -ne 0 ]; then
    echo "[fetch-ffmpeg] FAILED — one or more required targets not populated." >&2
    exit 1
fi

log "Done."
exit 0
