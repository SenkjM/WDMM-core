// Integration test: Real FFmpeg HLS session

use std::path::PathBuf;
use std::time::Duration;
use tokio::time::sleep;
use wdmm_ffmpeg::{open_job, JobOpts, HlsMode, SubtitleMode};

/// Generate a short test video using ffmpeg. Returns path to the video file.
async fn create_test_video() -> PathBuf {
    let path = std::env::temp_dir().join("wdmm-test-video.mp4");
    if path.exists() {
        return path;
    }

    // Generate a 5-second test video with color bars and tone
    let output = tokio::process::Command::new("ffmpeg")
        .args([
            "-f", "lavfi",
            "-i", "testsrc=duration=5:size=640x480:rate=30",
            "-f", "lavfi",
            "-i", "sine=frequency=1000:duration=5",
            "-c:v", "libx264",
            "-preset", "ultrafast",
            "-c:a", "aac",
            "-y",
        ])
        .arg(&path)
        .output()
        .await
        .expect("failed to spawn ffmpeg");

    assert!(output.status.success(), "ffmpeg test video generation failed");
    assert!(path.exists(), "test video not created");
    path
}

#[tokio::test]
async fn hls_job_produces_segments() {
    let video = create_test_video().await;

    let handle = open_job(JobOpts {
        input: video,
        mode: HlsMode::DirectCopy,
        subtitles: SubtitleMode::Off,
        start_offset_secs: 0.0,
        segment_duration_secs: 2,
    })
    .expect("failed to open job");

    let segment_dir = handle.segment_dir().to_path_buf();
    let playlist = segment_dir.join("index.m3u8");

    // Wait for FFmpeg to write at least the playlist
    for _ in 0..30 {
        if playlist.exists() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }

    assert!(playlist.exists(), "playlist was not created");

    // Wait a bit longer for at least one segment
    for _ in 0..30 {
        let segments: Vec<_> = std::fs::read_dir(&segment_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map_or(false, |ext| ext == "ts"))
            .collect();
        if !segments.is_empty() {
            // Success
            drop(handle);
            assert!(!segment_dir.exists(), "segment dir should be cleaned up on drop");
            return;
        }
        sleep(Duration::from_millis(100)).await;
    }

    panic!("no segments were produced after 3 seconds");
}
