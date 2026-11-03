//! Host measurements for the contract §3.6 priority rule: a cast's transcode
//! must never queue behind a background libav job (the FFI embedded-subtitle
//! probe), while background work may only run when no cast job is active.
//!
//! The device symptom was: on a source with 33 embedded subtitle tracks the
//! probe repeatedly took the single gate slot, so HLS warm-up / seek re-open
//! waited out `JOB_ACQUIRE_TIMEOUT` (15 s) and surfaced `transcoding busy`.
//!
//! Run the libav measurement with:
//!   cargo test -p wdmm-proxy --features wdmm-ffmpeg/ffmpeg --test gate_priority -- --nocapture
//! (set `WDMM_HLS_TEST_MEDIA` to pick a single file; otherwise every media file
//! in `E:\test\video` is measured).

use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wdmm_proxy::{BindMode, JobGate, JOB_ACQUIRE_TIMEOUT};

/// Background hold, in ms, used to model a subtitle probe that is already
/// running when the cast wants the gate.
const BACKGROUND_HOLD_MS: u64 = 300;

fn cast(gate: &JobGate, timeout: Duration) -> Result<wdmm_proxy::JobPermit, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(gate.acquire_async(timeout))
}

#[test]
fn cast_acquire_does_not_wait_behind_a_background_holder() {
    let gate = JobGate::new(1);
    let (holding_tx, holding_rx) = mpsc::channel();

    let holder_gate = Arc::clone(&gate);
    let holder = std::thread::spawn(move || {
        let _permit = holder_gate
            .acquire(Duration::from_millis(100))
            .expect("background slot");
        holding_tx.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(BACKGROUND_HOLD_MS));
    });
    holding_rx.recv().expect("background job started");

    let started = Instant::now();
    let permit = cast(&gate, JOB_ACQUIRE_TIMEOUT).expect("cast preempts the probe");
    let waited_ms = started.elapsed().as_secs_f64() * 1000.0;
    eprintln!(
        "cast acquire behind a background holder: {waited_ms:.2} ms (was up to {} ms before the priority rule)",
        JOB_ACQUIRE_TIMEOUT.as_millis()
    );
    assert!(
        waited_ms < 20.0,
        "cast acquire waited {waited_ms:.2} ms behind background work"
    );
    assert!(permit.preempted());
    assert_eq!(gate.preemptions(), 1);

    drop(permit);
    holder.join().unwrap();
}

#[test]
fn background_wait_shows_the_starvation_the_cast_used_to_suffer() {
    // A device-class probe hold: the host probe over loopback takes tens of ms,
    // while the 4.7 GB / 33-subtitle-track source on the phone took long enough
    // that the cast waited out JOB_ACQUIRE_TIMEOUT. Both extremes are measured
    // through the same background path the cast used to share.
    for hold_ms in [BACKGROUND_HOLD_MS, 1_200] {
        let gate = JobGate::new(1);
        let (holding_tx, holding_rx) = mpsc::channel();

        let holder_gate = Arc::clone(&gate);
        let holder = std::thread::spawn(move || {
            let _permit = holder_gate
                .acquire(Duration::from_millis(100))
                .expect("background slot");
            holding_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(hold_ms));
        });
        holding_rx.recv().expect("background job started");

        let waiter_gate = Arc::clone(&gate);
        let started = Instant::now();
        let waiter = std::thread::spawn(move || waiter_gate.acquire(Duration::from_secs(5)));
        let permit = waiter.join().unwrap().expect("second background job");
        let waited_ms = started.elapsed().as_secs_f64() * 1000.0;
        eprintln!(
            "probe hold {hold_ms} ms => a job queued behind it waits {waited_ms:.2} ms (the wait a \
             cast job paid before it preempted the probe)"
        );
        assert!(
            waited_ms >= (hold_ms as f64) * 0.8,
            "background wait should cover the holder's remaining time ({waited_ms:.2} ms)"
        );
        assert!(!permit.preempted());
        drop(permit);
        holder.join().unwrap();
    }
}

/// Measures a real libav probe over the local proxy (`/s/<token>`), which is the
/// exact path the FFI subtitle probe takes, and the cast's acquire latency while
/// it is in flight.
#[test]
fn real_probe_hold_is_measured_and_cast_preempts_it() {
    if !wdmm_ffmpeg::is_available() {
        println!("skipped: wdmm-ffmpeg/ffmpeg feature not enabled");
        return;
    }
    let media_files = test_media();
    if media_files.is_empty() {
        println!("skipped: no media file (set WDMM_HLS_TEST_MEDIA)");
        return;
    }

    let handle = wdmm_proxy::ProxyHandle::new(BindMode::Loopback, 0);
    // Multi-threaded so the proxy server keeps serving after `block_on` returns.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let gate = JobGate::new(1);

    for media in media_files {
        let registration = rt
            .block_on(handle.register_file(
                media.clone(),
                Some("probe-media".to_string()),
                None,
                None,
            ))
            .expect("register media on the loopback proxy");
        let port = handle.addr().expect("proxy bound").port();
        let url = format!("http://127.0.0.1:{port}{}", registration.path_template);

        let (probe_done_tx, probe_done_rx) = mpsc::channel();
        let probe_gate = Arc::clone(&gate);
        let probe = std::thread::spawn(move || {
            let permit = probe_gate.acquire(JOB_ACQUIRE_TIMEOUT).expect("probe slot");
            let started = Instant::now();
            let result = wdmm_ffmpeg::probe(&url);
            let took = started.elapsed();
            drop(permit);
            probe_done_tx.send((took, result.is_ok())).unwrap();
        });

        // Wait until the probe owns the slot, then time a cast acquire.
        let mut waited = Duration::ZERO;
        while gate.in_use() == 0 && waited < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(2));
            waited += Duration::from_millis(2);
        }
        assert!(gate.in_use() > 0, "probe never reached the gate");

        let started = Instant::now();
        let cast_permit = cast(&gate, JOB_ACQUIRE_TIMEOUT).expect("cast preempts the real probe");
        let cast_latency_ms = started.elapsed().as_secs_f64() * 1000.0;
        assert!(
            cast_permit.preempted(),
            "cast permit must report preemption"
        );
        let (probe_took, probe_ok) = probe_done_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("probe finished");
        let size = std::fs::metadata(&media).map(|m| m.len()).unwrap_or(0);
        eprintln!(
            "probe via loopback proxy on {} ({} B): {:.2} ms (ok={probe_ok}); cast acquire during \
             it: {cast_latency_ms:.2} ms",
            media.file_name().unwrap_or_default().to_string_lossy(),
            size,
            probe_took.as_secs_f64() * 1000.0
        );
        assert!(probe_ok, "probe should succeed over the proxy");
        assert!(
            cast_latency_ms < 20.0,
            "cast waited {cast_latency_ms:.2} ms behind a real probe"
        );

        drop(cast_permit);
        probe.join().unwrap();
        handle.unregister(&registration.token);
    }
}

fn test_media() -> Vec<std::path::PathBuf> {
    if let Ok(path) = std::env::var("WDMM_HLS_TEST_MEDIA") {
        let path = std::path::PathBuf::from(path);
        return path.is_file().then_some(path).into_iter().collect();
    }
    let mut found: Vec<std::path::PathBuf> = std::fs::read_dir(r"E:\test\video")
        .map(|rd| {
            rd.filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|path| {
                    path.is_file()
                        && matches!(
                            path.extension().and_then(|ext| ext.to_str()),
                            Some("m4v" | "mp4" | "mkv")
                        )
                })
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found
}
