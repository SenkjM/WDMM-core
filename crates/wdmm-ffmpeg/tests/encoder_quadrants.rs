//! Host-only regression tests for the three-layer stream decision
//! (`.tmp/contract/preset-encoder-tiers.md` §2/§3).
//!
//! These tests drive a real libav job, so they are `#[ignore]`d and only run on a
//! host that links FFmpeg and has the `ffmpeg`/`ffprobe` CLI on `PATH`:
//!
//! ```powershell
//! $env:FF='E:\test\video\_env\ffmpeg-n9.0.2-win64-gpl-shared'
//! $env:WDMM_FFMPEG_LIB_DIR="$env:FF\lib"
//! $env:RUSTFLAGS="-L native=$env:FF\lib"
//! $env:PATH="$env:FF\bin;$env:PATH"
//! cargo test -p wdmm-ffmpeg --features ffmpeg --test encoder_quadrants -- --ignored --nocapture
//! ```
//!
//! Each test skips cleanly (prints `skip: …` and returns) when libav is not
//! linked, the CLI is missing, or fixture synthesis fails.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use wdmm_ffmpeg::{diag, open_job, preset, EncoderInfo, JobOpts, TranscodePreset};

/// Length of the synthesised fixture clips.
const CLIP_SECS: u32 = 6;
/// How long one job may take before the test fails.
const JOB_TIMEOUT: Duration = Duration::from_secs(120);

/// The diagnostics ring buffer and the temp-dir fixtures are global, so these
/// tests must not run concurrently with each other.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn libav_ready() -> bool {
    if !wdmm_ffmpeg::is_available() {
        eprintln!("skip: libav is not linked (run with --features ffmpeg)");
        return false;
    }
    true
}

/// Whether the `ffmpeg` CLI is available to synthesise the fixtures.
fn ffmpeg_cli_ready() -> bool {
    match Command::new("ffmpeg").arg("-version").output() {
        Ok(out) if out.status.success() => true,
        _ => {
            eprintln!("skip: the `ffmpeg` CLI is not on PATH");
            false
        }
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wdmm-quadrants-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// A small H.264 + AAC MP4: both streams are MPEG-TS copy compatible, so a
/// single fixture can reach every quadrant (copy/copy, encode/copy,
/// copy/encode, encode/encode).
fn h264_aac_fixture(dir: &Path) -> PathBuf {
    let path = dir.join("h264-aac.mp4");
    if path.is_file() {
        return path;
    }
    let status = Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error"])
        .args(["-f", "lavfi", "-i"])
        .arg(format!(
            "testsrc2=size=320x240:rate=25:duration={CLIP_SECS}"
        ))
        .args(["-f", "lavfi", "-i"])
        .arg(format!(
            "sine=frequency=440:sample_rate=48000:duration={CLIP_SECS}"
        ))
        // A one second GOP lets the copy paths split the clip into 2 s segments
        // (a stream copy can only cut on a source keyframe).
        .args([
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
        ])
        .args(["-g", "25", "-keyint_min", "25", "-sc_threshold", "0"])
        .args(["-c:a", "aac", "-b:a", "96k", "-shortest"])
        .arg(&path)
        .status()
        .expect("spawn ffmpeg");
    assert!(
        status.success(),
        "fixture synthesis failed: {}",
        path.display()
    );
    path
}

/// A portrait 720x1280 clip at `97/6` fps: non-integer, non-standard frame
/// rate plus a portrait aspect. MPEG-1/2/4 read the encoder time base as the
/// frame rate, so a bogus `1/90000` time base makes `avcodec_send_frame`
/// return -22 on exactly this kind of media.
fn portrait_non_integer_fps_fixture(dir: &Path) -> PathBuf {
    let path = dir.join("portrait-97-6.mp4");
    if path.is_file() {
        return path;
    }
    let status = Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error"])
        .args(["-f", "lavfi", "-i"])
        .arg(format!(
            "testsrc2=size=720x1280:rate=97/6:duration={CLIP_SECS}"
        ))
        .args(["-f", "lavfi", "-i"])
        .arg(format!(
            "sine=frequency=440:sample_rate=48000:duration={CLIP_SECS}"
        ))
        .args([
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
        ])
        .args(["-g", "16", "-keyint_min", "16", "-sc_threshold", "0"])
        .args(["-c:a", "aac", "-b:a", "96k", "-shortest"])
        .arg(&path)
        .status()
        .expect("spawn ffmpeg");
    assert!(
        status.success(),
        "portrait fixture synthesis failed: {}",
        path.display()
    );
    path
}

/// Run one job until at least `min_segments` segments are ready, then return the
/// telemetry, the diagnostics captured during the run, and the segment dir.
fn run_job(
    input: &Path,
    preset: &TranscodePreset,
    tag: &str,
    min_segments: usize,
) -> (EncoderInfo, String, PathBuf) {
    let out_dir =
        std::env::temp_dir().join(format!("wdmm-quadrants-out-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out_dir);
    diag::clear();
    let opts = JobOpts {
        input_url: input.to_string_lossy().into_owned(),
        preset: preset.clone(),
        out_dir: Some(out_dir.clone()),
        ..JobOpts::default()
    };
    let mut job = open_job(opts).expect("open_job");
    let deadline = Instant::now() + JOB_TIMEOUT;
    while job.is_running() && job.ready_segments() < min_segments && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let info = job.encoder_info();
    let log = diag::tail(400).join("\n");
    let result = job.take_result();
    drop(job);
    if let Some(Err(err)) = result {
        panic!("{tag}: job failed: {err}\n{log}");
    }
    assert!(
        job_ready(&out_dir) >= min_segments,
        "{tag}: expected >= {min_segments} segments in {}\n{log}",
        out_dir.display()
    );
    (info, log, out_dir)
}

fn job_ready(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("seg")
                && name.ends_with(".ts")
                && entry.metadata().map(|m| m.len() > 0).unwrap_or(false)
        })
        .count()
}

/// Base preset for the quadrant matrix: deterministic software video path, a
/// small/fast grid so the matrix runs in seconds.
fn matrix_base() -> TranscodePreset {
    let mut preset = preset::preset_h264();
    preset.hw_video_encoder = String::new(); // an empty hw name forces the software encoder
    preset.hls_segment_secs = 2;
    preset.video_max_w = Some(320);
    preset.video_max_h = Some(240);
    preset.video_fps_cap = Some(25);
    preset
}

fn first_audio_codec(segment: &Path) -> String {
    let probe = wdmm_ffmpeg::probe(&segment.to_string_lossy()).expect("probe segment");
    probe
        .audio
        .first()
        .map(|stream| stream.codec.to_ascii_lowercase())
        .unwrap_or_default()
}

#[test]
#[ignore = "host only: needs linked FFmpeg + the ffmpeg CLI"]
fn four_quadrants_are_reachable_from_one_source() {
    let _serial = serial();
    if !libav_ready() || !ffmpeg_cli_ready() {
        return;
    }
    let dir = temp_dir("quadrants");
    let input = h264_aac_fixture(&dir);

    // (video_copy, audio_copy, expected mode, expected video action, expected audio action)
    let quadrants = [
        (true, true, "remux", "copy", "copy"),
        (false, true, "sw", "sw", "copy"),
        (true, false, "hybrid", "copy", "sw"),
        (false, false, "sw", "sw", "sw"),
    ];

    for (index, (video_copy, audio_copy, mode, video_action, audio_action)) in
        quadrants.into_iter().enumerate()
    {
        let mut preset = matrix_base();
        preset.video_copy = video_copy;
        preset.audio_copy = audio_copy;
        let tag = format!("q{index}");
        let (info, log, out_dir) = run_job(&input, &preset, &tag, 2);
        eprintln!(
            "quadrant {tag}: video_copy={video_copy} audio_copy={audio_copy} -> mode={} video={:?} audio={:?}",
            info.mode, info.video_action, info.audio_action
        );
        assert_eq!(info.mode, mode, "{tag} mode\n{log}");
        assert_eq!(
            info.video_action.as_deref(),
            Some(video_action),
            "{tag} video_action\n{log}"
        );
        assert_eq!(
            info.audio_action.as_deref(),
            Some(audio_action),
            "{tag} audio_action\n{log}"
        );

        // encode (video) + copy (audio) must write the source audio packets
        // straight to the muxer.
        if !video_copy && audio_copy {
            assert!(
                log.contains("transcode audio copy: keeping source codec aac untouched"),
                "{tag} must copy audio without a decoder/resampler\n{log}"
            );
            assert_eq!(
                first_audio_codec(&out_dir.join("seg0.ts")),
                "aac",
                "{tag}: copied audio keeps the source codec"
            );
        }
        let _ = std::fs::remove_dir_all(&out_dir);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "host only: needs linked FFmpeg + the ffmpeg CLI"]
fn encoder_options_carry_the_speed_preset_and_rate_control() {
    let _serial = serial();
    if !libav_ready() || !ffmpeg_cli_ready() {
        return;
    }
    let dir = temp_dir("options");
    let input = h264_aac_fixture(&dir);
    let target_bitrate = matrix_base()
        .video_max_bitrate
        .expect("the matrix preset carries a bitrate envelope");

    // (libx264 preset, rate control): the two structured knobs that reach
    // avcodec_open2 through the option dictionary (§4, §13.3).
    for (x264_preset, rate_control) in [("ultrafast", "vbr"), ("medium", "cq"), ("slow", "cbr")] {
        let mut preset = matrix_base();
        preset.video_copy = false;
        preset.audio_copy = false;
        preset.sw_video_encoder = "libx264".into();
        preset.sw_video_preset = x264_preset.into();
        preset.video_rate_control = rate_control.into();
        if rate_control == "cq" {
            preset.video_quality = Some(23);
        }
        let tag = format!("options-{rate_control}");
        let (info, log, out_dir) = run_job(&input, &preset, &tag, 2);
        assert_eq!(info.video_action.as_deref(), Some("sw"), "{tag}\n{log}");
        let plan_line = log
            .lines()
            .find(|line| line.contains("video plan: action=sw") && line.contains("rate_control="))
            .unwrap_or_else(|| panic!("{tag}: no video plan diagnostic\n{log}"));
        assert!(
            plan_line.contains(&format!("rate_control={rate_control}")),
            "{tag}: the plan must name the requested rate control\n{plan_line}"
        );
        assert!(
            plan_line.contains(&format!("bitrate={target_bitrate} ")),
            "{tag}: the envelope bitrate must reach the encoder\n{plan_line}"
        );
        assert!(
            plan_line.contains(&format!("options=preset={x264_preset}")),
            "{tag}: the configured x264 preset must be handed to avcodec_open2\n{plan_line}"
        );
        match rate_control {
            "cbr" => assert!(
                plan_line.contains("nal-hrd=cbr"),
                "{tag}: CBR must map to libx264's nal-hrd= cbr mode\n{plan_line}"
            ),
            "cq" => assert!(
                plan_line.contains("crf=23"),
                "{tag}: CQ must map to libx264's crf\n{plan_line}"
            ),
            _ => {}
        }
        assert!(
            !log.contains("encoder options rejected by the codec"),
            "{tag}: the structured options must be accepted\n{log}"
        );
        eprintln!("encoder options {rate_control}: {plan_line}");
        let _ = std::fs::remove_dir_all(&out_dir);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "host only: needs linked FFmpeg + the ffmpeg CLI"]
fn audio_encoders_produce_aac_ac3_and_mp2() {
    let _serial = serial();
    if !libav_ready() || !ffmpeg_cli_ready() {
        return;
    }
    let dir = temp_dir("audio");
    let input = h264_aac_fixture(&dir);

    for codec in ["aac", "ac3", "mp2"] {
        let mut preset = matrix_base();
        // Copy the video, re-encode the audio: this is the copy+encode quadrant.
        preset.video_copy = true;
        preset.audio_copy = false;
        preset.audio_codec = codec.into();
        let (info, log, out_dir) = run_job(&input, &preset, &format!("audio-{codec}"), 2);
        assert_eq!(
            info.audio.as_deref(),
            Some(codec),
            "{codec}: telemetry must name the encoder\n{log}"
        );
        let produced = first_audio_codec(&out_dir.join("seg0.ts"));
        assert!(
            produced.contains(codec),
            "{codec}: segments must carry that audio codec, got {produced}\n{log}"
        );
        eprintln!(
            "audio {codec}: encoder={:?} produced={produced}",
            info.audio
        );
        if codec == "mp2" {
            // MP2 only accepts S16: the AudioPipeline must not force FLTP.
            assert!(
                log.contains("sample_fmt="),
                "mp2: diag must record the sample format\n{log}"
            );
            assert!(
                log.contains("sample_fmt=1"),
                "mp2: the encoder must be opened with AV_SAMPLE_FMT_S16 (1)\n{log}"
            );
        }
        let _ = std::fs::remove_dir_all(&out_dir);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "host only: needs linked FFmpeg + the ffprobe CLI"]
fn every_segment_starts_on_an_i_frame() {
    let _serial = serial();
    if !libav_ready() || !ffmpeg_cli_ready() {
        return;
    }
    let dir = temp_dir("keyframes");
    let input = h264_aac_fixture(&dir);

    for encoder in ["libx264", "mpeg2video", "mpeg4"] {
        let mut preset = matrix_base();
        preset.video_copy = false;
        preset.audio_copy = false;
        preset.sw_video_encoder = encoder.into();
        preset.sw_video_preset = "veryfast".into();
        preset.video_max_w = Some(160);
        preset.video_max_h = Some(120);
        let tag = format!("kf-{encoder}");
        let (_info, log, out_dir) = run_job(&input, &preset, &tag, 3);
        for k in 0..3 {
            let segment = out_dir.join(format!("seg{k}.ts"));
            let first = first_video_pict_type(&segment);
            eprintln!("{encoder}: seg{k}.ts first video frame = {first}");
            assert_eq!(
                first, "I",
                "{encoder}: seg{k}.ts must start on an I frame, got {first:?}\n{log}"
            );
        }
        let _ = std::fs::remove_dir_all(&out_dir);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The built-in `hevc` preset asks for MediaCodec HEVC and falls back to
/// `libx264`. On a host without a HEVC hardware encoder the session therefore
/// really produces H.264, and that downgrade must be announced (§4).
#[test]
#[ignore = "host only: needs linked FFmpeg + the ffmpeg CLI"]
fn hevc_preset_software_fallback_logs_the_downgrade() {
    let _serial = serial();
    if !libav_ready() || !ffmpeg_cli_ready() {
        return;
    }
    let dir = temp_dir("hevc-fallback");
    let input = h264_aac_fixture(&dir);

    let mut preset = preset::preset_hevc();
    preset.video_copy = false;
    preset.audio_copy = true;
    preset.hls_segment_secs = 2;
    preset.video_max_w = Some(320);
    preset.video_max_h = Some(240);
    let (info, log, out_dir) = run_job(&input, &preset, "hevc-fallback", 1);
    eprintln!(
        "hevc fallback: video={:?} video_action={:?}",
        info.video, info.video_action
    );
    assert_eq!(info.video_action.as_deref(), Some("sw"), "{log}");
    assert_eq!(info.video.as_deref(), Some("libx264"), "{log}");
    assert!(
        log.contains("video: sw fallback downgrades hevc to h264"),
        "the hevc->h264 downgrade must never be silent\n{log}"
    );
    let _ = std::fs::remove_dir_all(&out_dir);
    let _ = std::fs::remove_dir_all(&dir);
}

/// MPEG-2/MPEG-4 Part 2 must encode portrait, non-integer-frame-rate media.
/// This is the case that used to fail with `avcodec_send_frame` returning -22:
/// the encoder time base must be `1/framerate`, not `1/90000` (§3).
/// Set `WDMM_QUADRANT_MEDIA` to reuse a real sample (e.g. a `97/6` fps portrait
/// clip) instead of the synthesised one.
#[test]
#[ignore = "host only: needs linked FFmpeg + the ffmpeg CLI"]
fn portrait_non_integer_fps_media_encodes_with_mpeg2_and_mpeg4() {
    let _serial = serial();
    if !libav_ready() || !ffmpeg_cli_ready() {
        return;
    }
    let dir = temp_dir("portrait");
    let input = match std::env::var("WDMM_QUADRANT_MEDIA") {
        Ok(media) if !media.is_empty() => PathBuf::from(media),
        _ => portrait_non_integer_fps_fixture(&dir),
    };

    for encoder in ["mpeg4", "mpeg2video"] {
        let mut preset = preset::preset_h264();
        preset.hw_video_encoder = String::new(); // software encoder only
        preset.video_copy = false;
        preset.audio_copy = false;
        preset.sw_video_encoder = encoder.into();
        preset.sw_video_preset = "medium".into();
        preset.hls_segment_secs = 2;
        preset.video_max_w = Some(1280);
        preset.video_max_h = Some(1280);
        let tag = format!("portrait-{encoder}");
        let (info, log, out_dir) = run_job(&input, &preset, &tag, 1);
        eprintln!(
            "portrait {encoder}: video={:?} video_action={:?} from {}",
            info.video,
            info.video_action,
            input.display()
        );
        assert_eq!(info.video_action.as_deref(), Some("sw"), "{tag}\n{log}");
        assert_eq!(info.video.as_deref(), Some(encoder), "{tag}\n{log}");
        assert_eq!(
            first_video_pict_type(&out_dir.join("seg0.ts")),
            "I",
            "{tag}: first frame must be an I frame\n{log}"
        );
        // The synthesised clip runs at 97/6 (~16.17 fps), which MPEG-2 cannot
        // signal; the encoder must snap and say so rather than fail to open.
        let synthetic = std::env::var("WDMM_QUADRANT_MEDIA")
            .map(|media| media.is_empty())
            .unwrap_or(true);
        if encoder == "mpeg2video" && synthetic {
            let snap_line = log
                .lines()
                .find(|line| line.contains("is not an MPEG-2 frame rate"))
                .unwrap_or("<no snap line>");
            eprintln!("portrait {encoder}: {snap_line}");
            assert!(
                log.contains("is not an MPEG-2 frame rate, snapped to"),
                "{tag}: MPEG-2 must report the frame-rate snap\n{log}"
            );
        }
        let _ = std::fs::remove_dir_all(&out_dir);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Picture type of the first video frame of a segment, via the `ffprobe` CLI.
fn first_video_pict_type(segment: &Path) -> String {
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0"])
        .args(["-show_entries", "frame=pict_type", "-of", "csv=p=0"])
        .args(["-read_intervals", "%+#1"])
        .arg(segment)
        .output()
        .expect("spawn ffprobe");
    assert!(
        output.status.success(),
        "ffprobe failed for {}: {}",
        segment.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .split(',')
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}
