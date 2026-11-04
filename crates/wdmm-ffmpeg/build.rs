//! Build script for `wdmm-ffmpeg` (B3.5).
//!
//! Links against the FFmpeg shared libraries when the `ffmpeg` Cargo feature
//! is enabled. The FFI surface in `src/sys.rs` is hand-rolled against the
//! FFmpeg 9.0 ABI (struct offsets verified with clang
//! `-fdump-record-layouts` against the 9.0.2 headers on arm64 and x86_64;
//! both are LP64 so the layouts agree). No headers or pkg-config needed at
//! build time.
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

    // No `avfilter`: we never build a filter graph (every pixel-level step is
    // our own FFI call), so the Android build is configured with
    // `--disable-avfilter` and the library is not shipped at all. Linking it
    // here would fail on those trimmed libraries, and the linker would drop it
    // anyway (zero undefined avfilter_* symbols in the whole crate).
    for lib in ["avformat", "avcodec", "swresample", "swscale", "avutil"] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
}
