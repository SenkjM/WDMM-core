/// build.rs — wdmm-ffmpeg
///
/// Checks that vendored FFmpeg binaries exist for the Cargo target triple.
/// Fails the build with a clear error if they are missing.
///
/// ## Vendor directory layout
///
/// ```text
/// crates/wdmm-ffmpeg/vendor/
///     linux-x86_64/    ffmpeg ffprobe          (BtbN LGPL master linux64)
///     android-arm64/   ffmpeg ffprobe          (hzw1199 LGPL arm64 CLI)
///     windows-x86_64/  ffmpeg.exe ffprobe.exe  (BtbN LGPL master win64)
/// ```
///
/// ## Acquisition
///
/// Run `scripts/fetch-ffmpeg-vendor.sh` from the workspace root.
/// Set `WDMM_FFMPEG_CACHE=<dir>` to point at an already-downloaded cache.
///
/// ## Android gaps
///
/// `android-arm`, `android-x86_64`, and `android-x86` have NO confirmed
/// ffprobe+ffmpeg CLI binary source (see FFMPEG-ATTRIBUTION.md §Android gaps).
/// The build fails for those targets until a dual CLI package is confirmed.
fn main() {
    let target_os   = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();

    let vendor_subdir = match (target_os.as_str(), target_arch.as_str()) {
        ("linux",   "x86_64")  => "linux-x86_64",
        ("android", "aarch64") => "android-arm64",
        ("windows", "x86_64")  => "windows-x86_64",

        // Android ABIs with no confirmed ffprobe+ffmpeg CLI source.
        ("android", abi @ ("arm" | "x86_64" | "x86")) => {
            panic!(
                "wdmm-ffmpeg: android-{abi} has no confirmed ffprobe+ffmpeg CLI binary source.\n\
                 Tried: Tyrrrz/FFmpegBinaries (ffmpeg-only), \
                 yearsyan/ffmpeg-android-build (static .a libs only), \
                 Khang-NT/android-ffmpeg-toolkit (disables ffprobe).\n\
                 Until a package shipping both ffmpeg and ffprobe CLI binaries for \
                 android-{abi} is confirmed, this ABI is unsupported.\n\
                 See crates/wdmm-ffmpeg/docs/FFMPEG-ATTRIBUTION.md §Android gaps."
            );
        }

        _ => {
            // Unsupported / unknown target: not a hard error — let Rust decide
            // what to do (e.g. wasm, riscv). The binary just won't be usable.
            println!("cargo:rerun-if-changed=build.rs");
            return;
        }
    };

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let vendor_path = std::path::Path::new(&manifest_dir)
        .join("vendor")
        .join(vendor_subdir);

    let (ffmpeg_name, ffprobe_name) = if target_os == "windows" {
        ("ffmpeg.exe", "ffprobe.exe")
    } else {
        ("ffmpeg", "ffprobe")
    };

    let ffmpeg_bin  = vendor_path.join(ffmpeg_name);
    let ffprobe_bin = vendor_path.join(ffprobe_name);

    if !ffmpeg_bin.exists() || !ffprobe_bin.exists() {
        panic!(
            "wdmm-ffmpeg: vendored binaries not found for {target_os}-{target_arch}.\n\
             Expected:\n  {ffmpeg}\n  {ffprobe}\n\
             Run:  scripts/fetch-ffmpeg-vendor.sh\n\
             Or set WDMM_FFMPEG_CACHE=/path/to/cache and re-run.",
            ffmpeg  = ffmpeg_bin.display(),
            ffprobe = ffprobe_bin.display(),
        );
    }

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=vendor/{vendor_subdir}");
}
