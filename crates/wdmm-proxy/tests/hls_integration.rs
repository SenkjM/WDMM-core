// Integration test: real libav HLS job writing into a caller-owned segment directory.
//
// This exercises the pull-based VOD model: the job writes `seg<k>.ts` numbered from
// `start_number`, into a persistent `out_dir` that survives `JobHandle::drop` (the
// transcode cache owns its lifetime). The playlist is NOT read from the muxer — the
// proxy synthesizes it — so only segment production is asserted here.

use std::path::PathBuf;
use std::time::Duration;
use tokio::time::sleep;
use wdmm_ffmpeg::preset;
use wdmm_ffmpeg::{open_job, JobOpts};

/// Generate a short test video using ffmpeg. Returns path to the video file.
async fn create_test_video() -> PathBuf {
    let path = std::env::temp_dir().join("wdmm-test-video.mp4");
    if path.exists() {
        return path;
    }

    // Generate a 5-second test video with color bars and tone
    let output = tokio::process::Command::new("ffmpeg")
        .args([
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=5:size=640x480:rate=30",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000:duration=5",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            // Video HLS segments now cut on keyframes only (the muxer's
            // `hls_flags=split_by_time` is deliberately not set for video
            // copies), so the fixture needs a short GOP to produce more than
            // one segment with the preset's 4 s target.
            "-g",
            "30",
            "-c:a",
            "aac",
            "-y",
        ])
        .arg(&path)
        .output()
        .await
        .expect("failed to spawn ffmpeg");

    assert!(
        output.status.success(),
        "ffmpeg test video generation failed"
    );
    assert!(path.exists(), "test video not created");
    path
}

fn unique_out_dir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "wdmm-hls-job-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

#[tokio::test]
async fn hls_job_produces_numbered_segments_in_out_dir() {
    if !wdmm_ffmpeg::is_available() {
        // Nothing to transcode with: a build without the `ffmpeg` feature can
        // never open a job, so this is a skip, not a failure.
        eprintln!("skip: wdmm-ffmpeg was built without the `ffmpeg` feature");
        return;
    }
    // Point `WDMM_HLS_TEST_MEDIA` at any real media file to avoid needing a
    // system `ffmpeg` on PATH just to synthesise one.
    let video = match std::env::var_os("WDMM_HLS_TEST_MEDIA") {
        Some(p) if PathBuf::from(&p).is_file() => PathBuf::from(p),
        _ => create_test_video().await,
    };

    let out_dir = unique_out_dir();
    let handle = open_job(JobOpts {
        input_url: video.to_string_lossy().into_owned(),
        preset: preset::preset_balanced(),
        start_offset_secs: 0.0,
        segment_dir: PathBuf::new(),
        start_number: 0,
        out_dir: Some(out_dir.clone()),
        hls_time_override_ms: None,
    })
    .expect("failed to open job");

    // The pull-based model restarts jobs at arbitrary `k`, so segment numbering
    // must come from `start_number` and the first segments must appear promptly.
    let mut produced = 0usize;
    for _ in 0..300 {
        produced = handle.ready_segments();
        if produced >= 2 {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }

    assert!(
        produced >= 2,
        "expected at least two segments in {}, got {produced}",
        out_dir.display()
    );
    assert!(
        out_dir.join("seg0.ts").is_file(),
        "first segment must be numbered seg0.ts"
    );
    assert!(
        out_dir.join("seg1.ts").is_file(),
        "second segment must be numbered seg1.ts"
    );
    let durations = handle.segment_durations();
    assert!(
        durations.len() >= 2 && durations.iter().all(|d| *d > 0),
        "segment durations must be reported for calibration, got {durations:?}"
    );

    drop(handle);
    assert!(
        out_dir.exists(),
        "out_dir belongs to the transcode cache and must survive JobHandle::drop"
    );
    assert!(
        out_dir.join("seg0.ts").is_file(),
        "segments must survive JobHandle::drop"
    );
    let _ = std::fs::remove_dir_all(&out_dir);
}
