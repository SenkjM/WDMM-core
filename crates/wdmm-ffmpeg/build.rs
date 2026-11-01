//! Build script for `wdmm-ffmpeg` (B3.5).
//!
//! Links against the FFmpeg shared libraries when the `ffmpeg` Cargo feature
//! is enabled. The FFI surface in `src/sys.rs` is hand-rolled against the
//! FFmpeg 7.1 ABI (struct offsets verified via a compile-time offset probe
//! against the 7.1 headers on x86_64; arm64 is identical because both are
//! LP64). No headers or pkg-config needed at build time.
//!
//! `WDMM_FFMPEG_LIB_DIR` adds an extra library search path (Android NDK
//! cross-build uses this to point at the per-ABI jniLibs staging dir).
//!
//! Without `--features ffmpeg` (e.g. Windows CI) the build produces stubs
//! only — no libav* symbols are needed at link time.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var_os("CARGO_FEATURE_FFMPEG").is_none() {
        // No FFmpeg feature — skip all link directives (stub build).
        return;
    }

    if let Ok(dir) = std::env::var("WDMM_FFMPEG_LIB_DIR") {
        if !dir.is_empty() {
            println!("cargo:rustc-link-search=native={dir}");
        }
    }

    for lib in ["avformat", "avcodec", "avfilter", "swresample", "swscale", "avutil"] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
}
