// Temporary diagnostic probe (Lead): does the warm-up path ever report
// `playlist_ready` without a renderer asking for a segment?
//
// Mirrors what the app does: register_hls_nowait() then poll hls_progress()
// every 200 ms, printing the fields the UI depends on.
//
// Run:
//   $env:WDMM_HLS_TEST_MEDIA='E:\test\video\1.m4v'
//   cargo test -p wdmm-proxy --features wdmm-ffmpeg/ffmpeg --test warmup_probe -- --ignored --nocapture

use std::path::PathBuf;
use std::time::{Duration, Instant};
use wdmm_ffmpeg::preset;
use wdmm_proxy::{BindMode, HlsCacheOptions, HlsSessionOpts, ProxyHandle};

fn unique_dir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "wdmm-warmup-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn warm_up_starts_eagerly_and_reports_ready() {
    assert!(
        wdmm_ffmpeg::is_available(),
        "needs --features wdmm-ffmpeg/ffmpeg"
    );
    let media = std::env::var("WDMM_HLS_TEST_MEDIA").expect("WDMM_HLS_TEST_MEDIA");
    let media = PathBuf::from(media);
    assert!(media.is_file(), "missing media {}", media.display());
    let size = std::fs::metadata(&media).unwrap().len();

    let handle = ProxyHandle::new(BindMode::Loopback, 0);
    let cache_root = unique_dir();
    let opts = HlsSessionOpts {
        device_id: "probe-device".into(),
        input_url: media.to_string_lossy().into_owned(),
        preset: preset::preset_h264(),
        ttl_secs: None,
        allowed_peers: None,
        cache: HlsCacheOptions {
            root: Some(cache_root.clone()),
            budget_bytes: Some(512 * 1024 * 1024),
            account_id: "probe-account".into(),
            remote_path: media.to_string_lossy().into_owned(),
            preset_id: "balanced".into(),
            size,
            ident: "probe-ident".into(),
        },
    };

    let started = Instant::now();
    let reg = handle
        .register_hls_nowait(opts)
        .await
        .expect("register_hls_nowait");
    eprintln!(
        "registered in {:?}: {}",
        started.elapsed(),
        reg.playlist_template
    );

    let mut last_print = Instant::now();
    let mut ready_at = None;
    for i in 0..300 {
        let progress = handle.hls_progress(&reg.token).expect("progress");
        if i % 5 == 0 || progress.playlist_ready {
            eprintln!(
                "[{:>6.2}s] ready={} segments={} seg_count={} alignment={} speed={:.2} content_ms={} run_state={} error={:?}",
                started.elapsed().as_secs_f64(),
                progress.playlist_ready,
                progress.segments,
                progress.segment_count,
                progress.alignment.unwrap_or("none"),
                progress.speed,
                progress.content_ms,
                progress.run_state,
                progress.error,
            );
            last_print = Instant::now();
        }
        if progress.playlist_ready && ready_at.is_none() {
            ready_at = Some(started.elapsed());
            eprintln!(">>> playlist_ready after {:?}", started.elapsed());
            break;
        }
        if progress.error.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let _ = last_print;

    // Dump what the job left on disk.
    let dirs: Vec<_> = std::fs::read_dir(&cache_root)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    for dir in &dirs {
        let mut segs = 0;
        let mut playlist = None;
        if let Ok(rd) = std::fs::read_dir(dir) {
            for entry in rd.filter_map(|e| e.ok()) {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name == "index.m3u8" {
                    playlist = std::fs::read_to_string(entry.path()).ok();
                } else if name.ends_with(".ts") {
                    segs += 1;
                }
            }
        }
        eprintln!(
            "dir {} -> {segs} .ts files, playlist {}",
            dir.display(),
            if playlist.is_some() {
                "present"
            } else {
                "missing"
            }
        );
        if let Some(text) = playlist {
            let extinf: Vec<&str> = text.lines().filter(|l| l.starts_with("#EXTINF")).collect();
            eprintln!(
                "  #EXTINF count={} first={:?}",
                extinf.len(),
                extinf.first()
            );
        }
    }

    handle.unregister_hls(&reg.token);
    let _ = std::fs::remove_dir_all(&cache_root);
    assert!(
        ready_at.is_some(),
        "warm-up never reported playlist_ready — this is the device symptom"
    );
}
