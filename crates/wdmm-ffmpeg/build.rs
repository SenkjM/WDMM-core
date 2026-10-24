/// build.rs — wdmm-ffmpeg
///
/// Detects the Cargo target triple and links the appropriate vendored FFmpeg
/// prebuilt library set.
///
/// ## Vendor directory layout (not yet populated)
///
/// ```text
/// crates/wdmm-ffmpeg/vendor/
///     android-arm64/   libavcodec.a libavformat.a libavutil.a libswresample.a libswscale.a
///     android-arm/     …
///     android-x86_64/  …
///     android-x86/     …
///     windows-x86_64/  avcodec.lib avformat.lib avutil.lib swresample.lib swscale.lib
/// ```
///
/// TODO(round-12+): download / unpack vendored prebuilt tarballs here (or via a
///   separate `scripts/fetch-ffmpeg-libs.sh`) and uncomment the
///   `cargo:rustc-link-*` lines below.
fn main() {
    let target_os   = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();

    let vendor_subdir = match (target_os.as_str(), target_arch.as_str()) {
        ("android", "aarch64") => "android-arm64",
        ("android", "arm")     => "android-arm",
        ("android", "x86_64")  => "android-x86_64",
        ("android", "x86")     => "android-x86",
        ("windows", "x86_64")  => "windows-x86_64",
        // Host (Linux / macOS) builds: stub mode only — no link.
        _ => {
            println!("cargo:rerun-if-changed=build.rs");
            return;
        }
    };

    // Uncomment once vendored libs are present:
    // let vendor_dir = std::path::Path::new("vendor").join(vendor_subdir);
    // println!("cargo:rustc-link-search=native={}", vendor_dir.display());
    // for lib in &["avcodec", "avformat", "avutil", "swresample", "swscale"] {
    //     println!("cargo:rustc-link-lib=static={lib}");
    // }

    let _ = vendor_subdir; // suppress unused-variable warning until TODO resolved
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=vendor/");
}
