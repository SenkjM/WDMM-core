// Real FFmpeg process spawn implementation

use crate::{FfmpegError, FfmpegResult, HlsMode, JobOpts, SubtitleMode};
use std::path::PathBuf;

/// Resolve the vendored ffmpeg binary path for the current target.
pub fn ffmpeg_bin() -> FfmpegResult<PathBuf> {
    resolve_bin("ffmpeg")
}

pub fn ffprobe_bin() -> FfmpegResult<PathBuf> {
    resolve_bin("ffprobe")
}

fn resolve_bin(name: &str) -> FfmpegResult<PathBuf> {
    let target_os = std::env::consts::OS;
    let target_arch = std::env::consts::ARCH;

    // Host Linux x86_64: use system PATH.
    if target_os == "linux" && target_arch == "x86_64" {
        return which::which(name).map_err(|_| {
            FfmpegError::Unavailable(format!(
                "{} not found in PATH; install via apt", name
            ))
        });
    }

    // Cross-compilation targets
    let vendor_subdir = match (target_os, target_arch) {
        ("android", "aarch64") => "android-arm64",
        ("android", "arm")     => "android-arm",
        ("android", "x86_64")  => "android-x86_64",
        ("android", "x86")     => "android-x86",
        ("windows", "x86_64")  => "windows-x86_64",
        _ => {
            return Err(FfmpegError::Unavailable(format!(
                "unsupported target: {}-{}", target_os, target_arch
            )))
        }
    };

    let exe_name = if target_os == "windows" {
        format!("{}.exe", name)
    } else {
        name.to_string()
    };

    let manifest_dir = std::env!("CARGO_MANIFEST_DIR");
    let vendor_path = PathBuf::from(manifest_dir)
        .join("vendor")
        .join(vendor_subdir)
        .join(&exe_name);

    if vendor_path.exists() {
        Ok(vendor_path)
    } else {
        Err(FfmpegError::Unavailable(format!(
            "vendored {} not found at {}", name, vendor_path.display()
        )))
    }
}

/// Spawn the ffmpeg process for an HLS job
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
                    cmd.args(["-vf", &format!("subtitles={}:si={}", opts.input.display(), stream_index)]);
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
