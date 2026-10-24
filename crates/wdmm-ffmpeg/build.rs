/// build.rs — wdmm-ffmpeg
///
/// Detects the Cargo target triple and ensures vendored FFmpeg binaries are
/// available. For cross-compilation targets (Android, Windows), looks for
/// prebuilt binaries in vendor/<target>/ and fails the build if missing.
///
/// ## Vendor directory layout
///
/// ```text
/// crates/wdmm-ffmpeg/vendor/
///     linux-x86_64/    ffmpeg ffprobe                 # host, uses system PATH
///     android-arm64/   ffmpeg ffprobe                 # must be manually placed
///     android-arm/     ffmpeg ffprobe
///     android-x86_64/  ffmpeg ffprobe
///     windows-x86_64/  ffmpeg.exe ffprobe.exe
/// ```
///
/// ## Acquisition
///
/// - **Linux x86_64 (host)** — uses system ffmpeg/ffprobe from PATH. No vendored
///   binary required for box tests.
/// - **Android** — download LGPL static builds from https://github.com/Javernaut/ffmpeg-android-maker
///   (or build from source). Place binaries in vendor/<abi>/.
/// - **Windows x86_64** — download LGPL shared builds from https://www.gyan.dev/ffmpeg/builds/
///   or https://github.com/BtbN/FFmpeg-Builds. Place .exe files in vendor/windows-x86_64/.
///
/// Build fails with a clear error message if binaries are missing for the target.
fn main() {
    let target_os   = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();

    let vendor_subdir = match (target_os.as_str(), target_arch.as_str()) {
        ("linux", "x86_64")    => {
            // Host build: use system ffmpeg from PATH; no vendor required.
            println!("cargo:rerun-if-changed=build.rs");
            return;
        }
        ("android", "aarch64") => "android-arm64",
        ("android", "arm")     => "android-arm",
        ("android", "x86_64")  => "android-x86_64",
        ("android", "x86")     => "android-x86",
        ("windows", "x86_64")  => "windows-x86_64",
        _ => {
            // Unsupported target: no FFmpeg, stub mode only.
            println!("cargo:rerun-if-changed=build.rs");
            return;
        }
    };

    // Check that vendor directory exists with binaries.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let vendor_path = std::path::Path::new(&manifest_dir)
        .join("vendor")
        .join(vendor_subdir);

    let (ffmpeg_name, ffprobe_name) = if target_os == "windows" {
        ("ffmpeg.exe", "ffprobe.exe")
    } else {
        ("ffmpeg", "ffprobe")
    };

    if !vendor_path.join(ffmpeg_name).exists() || !vendor_path.join(ffprobe_name).exists() {
        panic!(
            "wdmm-ffmpeg: vendored binaries not found for target {}-{}\n\
             Expected: {}/{{{},{}}}\n\
             See crates/wdmm-ffmpeg/build.rs for download sources.",
            target_os, target_arch, vendor_path.display(), ffmpeg_name, ffprobe_name
        );
    }

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=vendor/");
}
