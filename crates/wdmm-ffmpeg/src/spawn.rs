// Real FFmpeg process spawn implementation — vendor-only, no PATH fallback.
//
// All supported targets resolve their binary from
// `CARGO_MANIFEST_DIR/vendor/<triple>/{ffmpeg,ffprobe}[.exe]`.
// There is intentionally no `which` / PATH lookup.  If the vendor binary is
// missing, the error message directs the developer to run
// `scripts/fetch-ffmpeg-vendor.sh`.

use crate::{FfmpegError, FfmpegResult, HlsMode, JobOpts, SubtitleMode};
use std::path::PathBuf;

/// Return the vendored `ffmpeg` binary path for the current target triple.
///
/// Fails with [`FfmpegError::Unavailable`] if the vendor directory is absent
/// or if no confirmed binary source exists for the target ABI.
pub fn ffmpeg_bin() -> FfmpegResult<PathBuf> {
    resolve_bin("ffmpeg")
}

/// Return the vendored `ffprobe` binary path for the current target triple.
pub fn ffprobe_bin() -> FfmpegResult<PathBuf> {
    resolve_bin("ffprobe")
}

fn resolve_bin(name: &str) -> FfmpegResult<PathBuf> {
    let os   = std::env::consts::OS;
    let arch = std::env::consts::ARCH;

    // Targets with confirmed vendor binaries.
    let vendor_subdir = match (os, arch) {
        ("linux",   "x86_64")  => "linux-x86_64",
        ("android", "aarch64") => "android-arm64",
        ("windows", "x86_64")  => "windows-x86_64",
        // android-arm, android-x86_64, android-x86: no confirmed ffprobe+ffmpeg
        // CLI binary source found after checking Tyrrrz (ffmpeg-only),
        // yearsyan (static .a libs only), Khang-NT (disables ffprobe).
        // Fail loudly so callers know why.
        ("android", abi @ ("arm" | "x86_64" | "x86")) => {
            return Err(FfmpegError::Unavailable(format!(
                "android-{abi}: no confirmed ffprobe+ffmpeg CLI binary source found. \
                 Tried: Tyrrrz/FFmpegBinaries (ffmpeg-only), \
                 yearsyan/ffmpeg-android-build (static libs only), \
                 Khang-NT/android-ffmpeg-toolkit (disables ffprobe). \
                 See crates/wdmm-ffmpeg/docs/FFMPEG-ATTRIBUTION.md §Android gaps."
            )));
        }
        _ => {
            return Err(FfmpegError::Unavailable(format!(
                "unsupported target: {os}-{arch}"
            )));
        }
    };

    let exe_name = if os == "windows" {
        format!("{name}.exe")
    } else {
        name.to_string()
    };

    // CARGO_MANIFEST_DIR is set at compile time via std::env! so the path is
    // always the *source* tree; vendor binaries are populated by
    // scripts/fetch-ffmpeg-vendor.sh before building.
    let manifest_dir = std::env!("CARGO_MANIFEST_DIR");
    let vendor_path = PathBuf::from(manifest_dir)
        .join("vendor")
        .join(vendor_subdir)
        .join(&exe_name);

    if vendor_path.exists() {
        Ok(vendor_path)
    } else {
        Err(FfmpegError::Unavailable(format!(
            "vendored {name} not found at {path}; \
             run: scripts/fetch-ffmpeg-vendor.sh",
            path = vendor_path.display()
        )))
    }
}

/// Spawn the ffmpeg process for an HLS job.
pub fn spawn_hls_job(
    opts: &JobOpts,
    segment_dir: &std::path::Path,
) -> FfmpegResult<tokio::process::Child> {
    let ffmpeg = ffmpeg_bin()?;
    let mut cmd = tokio::process::Command::new(&ffmpeg);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    // Seek
    if opts.start_offset_secs > 0.0 {
        cmd.args(["-ss", &opts.start_offset_secs.to_string()]);
    }

    // Input
    cmd.arg("-i").arg(&opts.input);

    // Codec flags per mode
    match &opts.mode {
        HlsMode::DirectCopy => {
            cmd.args(["-c", "copy"]);
        }
        HlsMode::Compat { video_codec, audio_codec } => {
            let vcodec = video_codec.as_deref().unwrap_or("libx264");
            let acodec = audio_codec.as_deref().unwrap_or("aac");
            cmd.args(["-c:v", vcodec, "-c:a", acodec]);
            // Subtitle burn-in
            match &opts.subtitles {
                SubtitleMode::BurnIn { stream_index } => {
                    cmd.args([
                        "-vf",
                        &format!("subtitles={}:si={}", opts.input.display(), stream_index),
                    ]);
                }
                SubtitleMode::SidecarBurnIn { path } => {
                    cmd.args(["-vf", &format!("subtitles={}", path.display())]);
                }
                SubtitleMode::Off => {}
            }
        }
    }

    // HLS muxer
    cmd.args(["-f", "hls"]);
    cmd.args(["-hls_time", &opts.segment_duration_secs.to_string()]);
    cmd.args(["-hls_list_size", "0"]);
    cmd.args(["-hls_flags", "delete_segments"]);

    let playlist = segment_dir.join("index.m3u8");
    cmd.arg(&playlist);

    tracing::debug!(
        "spawning {} mode={:?} start={:.1}s",
        ffmpeg.display(),
        opts.mode,
        opts.start_offset_secs
    );

    cmd.spawn().map_err(Into::into)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffmpeg_bin_resolves_to_vendor_path() {
        let path = ffmpeg_bin().expect("ffmpeg_bin() failed on host linux-x86_64");
        let path_str = path.to_string_lossy();
        assert!(
            path_str.contains("vendor/linux-x86_64"),
            "expected vendor/linux-x86_64 in path, got: {path_str}"
        );
        assert!(path.exists(), "vendor ffmpeg binary does not exist at {path_str}");
    }

    #[test]
    fn ffprobe_bin_resolves_to_vendor_path() {
        let path = ffprobe_bin().expect("ffprobe_bin() failed on host linux-x86_64");
        let path_str = path.to_string_lossy();
        assert!(
            path_str.contains("vendor/linux-x86_64"),
            "expected vendor/linux-x86_64 in path, got: {path_str}"
        );
        assert!(path.exists(), "vendor ffprobe binary does not exist at {path_str}");
    }

    #[test]
    fn ffmpeg_bin_is_not_usr_bin() {
        let path = ffmpeg_bin().expect("ffmpeg_bin() must succeed on host");
        let path_str = path.to_string_lossy();
        assert!(
            !path_str.starts_with("/usr/bin"),
            "PATH fallback active — must use vendor binary, got: {path_str}"
        );
        assert!(
            !path_str.starts_with("/usr/local"),
            "PATH fallback active — must use vendor binary, got: {path_str}"
        );
    }
}
