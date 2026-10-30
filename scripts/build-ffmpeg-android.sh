#!/usr/bin/env bash
# scripts/build-ffmpeg-android.sh
#
# Build FFmpeg 7.1 shared libraries for a single Android ABI, with libx264
# (GPL) as the software H.264 encode fallback.  Produces the six `.so` files
# that the in-process libav transcoder (`crates/wdmm-ffmpeg`) dynamically
# links against, and which ship inside the APK under `jniLibs/<abi>/`.
#
# Required env:
#   ABI                — arm64-v8a | x86_64
#   ANDROID_NDK_HOME   — path to Android NDK r28.x
#   OUTPUT_DIR         — destination directory for the final `.so` files
#
# Optional env:
#   MIN_API            — Android min API level (default: 24, matches minSdk)
#   FFMPEG_VERSION     — FFmpeg git tag (default: n7.1)
#   X264_BRANCH        — x264 branch (default: stable)
#   BUILD_DIR          — scratch dir (default: /tmp/wdmm-ffmpeg-build-<abi>)
#   JOBS               — make parallelism (default: nproc)
#
# License posture (PLAN B3.5 / m01480):
#   --disable-nonfree  is mandatory — the project is AGPL and may not ship
#                      nonfree components.
#   --enable-gpl + --enable-libx264 are allowed (GPL v2+, not nonfree).
#   This script refuses to run if --enable-nonfree is present in the
#   generated `ffbuild/config.log`.
#
# Exit codes: 0 success, 1 build failure or license violation.

set -euo pipefail

ABI="${ABI:?ABI must be set (arm64-v8a | x86_64)}"
NDK="${ANDROID_NDK_HOME:?ANDROID_NDK_HOME must be set (NDK r28.x)}"
OUTPUT_DIR="${OUTPUT_DIR:?OUTPUT_DIR must be set}"
MIN_API="${MIN_API:-24}"
FFMPEG_VERSION="${FFMPEG_VERSION:-n7.1}"
X264_BRANCH="${X264_BRANCH:-stable}"
BUILD_DIR="${BUILD_DIR:-/tmp/wdmm-ffmpeg-build-${ABI}}"
JOBS="${JOBS:-$(nproc 2>/dev/null || echo 4)}"

log()  { echo "[build-ffmpeg:${ABI}] $*"; }
err()  { echo "[build-ffmpeg:${ABI}] ERROR: $*" >&2; }

# ── Map ABI to NDK cross-compile triple ────────────────────────────────────
case "$ABI" in
  arm64-v8a)
    TRIPLE="aarch64-linux-android"
    ARCH="aarch64"
    CPU="armv8-a"
    X264_HOST="aarch64-linux"
    ;;
  x86_64)
    TRIPLE="x86_64-linux-android"
    ARCH="x86_64"
    CPU="x86-64"
    X264_HOST="x86_64-linux"
    ;;
  *)
    err "unsupported ABI '$ABI' (supported: arm64-v8a, x86_64). armeabi-v7a is"
    err "excluded — FFmpeg 7.1 LP64 struct offsets are not valid on ILP32."
    exit 1
    ;;
esac

NDK_HOST="linux-x86_64"
TOOLCHAIN="${NDK}/toolchains/llvm/prebuilt/${NDK_HOST}/bin"
SYSROOT="${NDK}/toolchains/llvm/prebuilt/${NDK_HOST}/sysroot"
CC="${TOOLCHAIN}/${TRIPLE}${MIN_API}-clang"
CXX="${TOOLCHAIN}/${TRIPLE}${MIN_API}-clang++"
AR="${TOOLCHAIN}/llvm-ar"
NM="${TOOLCHAIN}/llvm-nm"
RANLIB="${TOOLCHAIN}/llvm-ranlib"
STRIP="${TOOLCHAIN}/llvm-strip"

SRC_DIR="${BUILD_DIR}/src"
X264_SRC="${SRC_DIR}/x264"
X264_INSTALL="${BUILD_DIR}/x264-install"
FFMPEG_SRC="${SRC_DIR}/ffmpeg"
FFMPEG_INSTALL="${BUILD_DIR}/ffmpeg-install"

mkdir -p "$SRC_DIR" "$X264_INSTALL" "$FFMPEG_INSTALL" "$OUTPUT_DIR"

log "FFmpeg ${FFMPEG_VERSION} | x264 ${X264_BRANCH} | min API ${MIN_API} | -j${JOBS}"
log "NDK:        ${NDK}"
log "CC:         ${CC}"
log "OUTPUT_DIR: ${OUTPUT_DIR}"

# ── Build x264 (static, no CLI) ────────────────────────────────────────────
log "--- x264: fetch ${X264_BRANCH} ---"
if [ ! -d "$X264_SRC" ]; then
  git clone --depth=1 --branch="$X264_BRANCH" \
    https://code.videolan.org/videolan/x264.git "$X264_SRC"
fi

log "--- x264: configure (${ABI}) ---"
cd "$X264_SRC"
# x264's configure does not understand --sysroot; it relies on the
# cross-prefix to point at the NDK clang driver, which honours its own
# sysroot.  -fPIC is required because FFmpeg will link x264 into a .so.
./configure \
  --host="$X264_HOST" \
  --cross-prefix="${TOOLCHAIN}/${TRIPLE}${MIN_API}-" \
  --enable-static \
  --disable-shared \
  --disable-opencl \
  --disable-cli \
  --extra-cflags="-fPIC -O2" \
  --extra-ldflags="-L${SYSROOT}/usr/lib"

log "--- x264: build ---"
make -j"$JOBS"
make install prefix="$X264_INSTALL"

# Rewrite the pkg-config file so FFmpeg's configure finds x264 by absolute
# prefix (the NDK sysroot does not expose it).
if [ -f "${X264_INSTALL}/lib/pkgconfig/x264.pc" ]; then
  sed -i "s|^prefix=.*|prefix=${X264_INSTALL}|" "${X264_INSTALL}/lib/pkgconfig/x264.pc"
fi

# ── Build FFmpeg (shared, no CLI) ──────────────────────────────────────────
log "--- FFmpeg: fetch ${FFMPEG_VERSION} ---"
if [ ! -d "$FFMPEG_SRC" ]; then
  git clone --depth=1 --branch="$FFMPEG_VERSION" \
    https://github.com/FFmpeg/FFmpeg.git "$FFMPEG_SRC"
fi

log "--- FFmpeg: configure (${ABI}) ---"
cd "$FFMPEG_SRC"

# --disable-symver: produce flat `libXXX.so` names (no `.61` version suffix)
#   so Android's dlopen finds them by SONAME without version gymnastics.
# --enable-pic:     position-independent code (we link into a .so).
# --max-page-size=16384: 16 KB page alignment for Android 15+ (Pixel 8a/9).
EXTRA_CFLAGS="-fPIC -O2 -ffunction-sections -fdata-sections -I${X264_INSTALL}/include"
EXTRA_LDFLAGS="-L${X264_INSTALL}/lib -Wl,--max-page-size=16384 -Wl,-z,max-page-size=16384"

./configure \
  --enable-shared \
  --disable-static \
  --disable-programs \
  --disable-doc \
  --disable-avdevice \
  --disable-postproc \
  --disable-symver \
  --enable-pic \
  --disable-nonfree \
  --enable-gpl \
  --enable-libx264 \
  --enable-encoder=libx264 \
  --enable-decoder=h264 \
  --enable-decoder=hevc \
  --enable-decoder=aac \
  --enable-decoder=mp3 \
  --enable-decoder=ac3 \
  --enable-decoder=eac3 \
  --enable-muxer=hls \
  --enable-muxer=mpegts \
  --enable-muxer=mp4 \
  --enable-demuxer=hls \
  --enable-demuxer=mpegts \
  --enable-demuxer=matroska \
  --enable-demuxer=mov \
  --enable-demuxer=avi \
  --enable-protocol=file \
  --enable-protocol=http \
  --enable-cross-compile \
  --target-os=android \
  --arch="$ARCH" \
  --cpu="$CPU" \
  --cc="$CC" \
  --cxx="$CXX" \
  --nm="$NM" \
  --ar="$AR" \
  --ranlib="$RANLIB" \
  --strip="$STRIP" \
  --sysroot="$SYSROOT" \
  --extra-cflags="$EXTRA_CFLAGS" \
  --extra-ldflags="$EXTRA_LDFLAGS" \
  --pkg-config-flags="--static" \
  --prefix="$FFMPEG_INSTALL"

# ── License guard ───────────────────────────────────────────────────────────
# The PLAN (m01480) forbids --enable-nonfree in an AGPL project.  FFmpeg's
# configure writes every flag it processed into ffbuild/config.log; refuse
# to proceed if nonfree was activated, regardless of how it got there.
if grep -q -- "--enable-nonfree" "${FFMPEG_SRC}/ffbuild/config.log" 2>/dev/null; then
  err "LICENSE VIOLATION: --enable-nonfree present in ffbuild/config.log — aborting."
  exit 1
fi

log "--- FFmpeg: build (-j${JOBS}) ---"
make -j"$JOBS"
make install

# ── Install shared libs to OUTPUT_DIR ──────────────────────────────────────
log "--- install .so → ${OUTPUT_DIR} ---"
for lib in avcodec avformat avutil swresample swscale avfilter; do
  # --disable-symver yields a flat libXXX.so; fall back to a versioned name
  # just in case a future FFmpeg tag re-introduces suffixes.
  so="${FFMPEG_INSTALL}/lib/lib${lib}.so"
  if [ ! -f "$so" ]; then
    so=$(find "${FFMPEG_INSTALL}/lib" -name "lib${lib}.so.*" -type f 2>/dev/null | head -1 || true)
  fi
  if [ -z "${so:-}" ] || [ ! -f "$so" ]; then
    err "lib${lib}.so not produced in ${FFMPEG_INSTALL}/lib"
    exit 1
  fi
  install -m 755 "$so" "${OUTPUT_DIR}/lib${lib}.so"
  "${STRIP}" --strip-unneeded "${OUTPUT_DIR}/lib${lib}.so" 2>/dev/null || true
  log "  lib${lib}.so"
done

# ── 16 KB page alignment audit ─────────────────────────────────────────────
log "--- 16 KB page alignment audit ---"
bad=0
for so in "${OUTPUT_DIR}"/lib*.so; do
  # LOAD segment alignment must be >= 0x4000 (16384) for Android 15 16 KB pages.
  align=$(readelf -lW "$so" 2>/dev/null \
    | awk '/^LOAD/ {print $NF; exit}' \
    | sed 's/^0*//' || true)
  if [ -z "$align" ] || [ "$((align))" -lt 16384 ]; then
    err "  ${so##*/}: LOAD alignment < 16384 (got ${align:-?})"
    bad=1
  fi
done
if [ "$bad" -ne 0 ]; then
  err "one or more libs are not 16 KB-aligned; rerun with a fixed --extra-ldflags."
  exit 1
fi

log "=== done: ${ABI} ==="
log "license: GPL v2+ (gpl=on, nonfree=off, libx264 bundled static)"
ls -lh "${OUTPUT_DIR}"/lib*.so
