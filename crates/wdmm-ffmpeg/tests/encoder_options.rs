//! Host-only checks for the preset → `AVDictionary` contract (§13.4–§13.6).
//!
//! Run with:
//!
//! ```text
//! cargo test -p wdmm-ffmpeg --features ffmpeg --test encoder_options -- --ignored --nocapture
//! ```
//!
//! The encoder-presence gate compares the shipped encoder list against
//! libavcodec. A full desktop build has no MediaCodec encoders, so the gate only
//! fails on missing software/audio encoders unless `WDMM_ENCODER_GATE_STRICT=1`
//! is set — that is the mode the trimmed Android build is verified with (§13.6).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use wdmm_core::models::{TranscodePreset, HW_VIDEO_ENCODERS, SW_VIDEO_ENCODERS};
use wdmm_ffmpeg::plan::{EncoderFamily, SHIPPED_AUDIO_ENCODERS, SHIPPED_VIDEO_ENCODERS};
use wdmm_ffmpeg::preset::{self, audio_encoder_name};
use wdmm_ffmpeg::{
    diag, encoder_available, encoder_options_schema_json, open_job, video_encoder_options,
    video_encoder_options_without_user, EncoderInfo, JobOpts, SCHEMA_RATE_CONTROL_MODES,
};

const CLIP_SECS: u32 = 6;
const JOB_TIMEOUT: Duration = Duration::from_secs(120);

fn libav_ready() -> bool {
    if wdmm_ffmpeg::is_available() {
        return true;
    }
    eprintln!("skip: libav is not linked (run with --features ffmpeg)");
    false
}

fn ffmpeg_cli_ready() -> bool {
    match Command::new("ffmpeg").arg("-version").output() {
        Ok(output) if output.status.success() => true,
        _ => {
            eprintln!("skip: the ffmpeg CLI is not on PATH");
            false
        }
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wdmm-options-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

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

fn matrix_base() -> TranscodePreset {
    let mut preset = preset::preset_h264();
    preset.hw_video_encoder = String::new(); // software encoder only
    preset.hls_segment_secs = 2;
    preset.video_max_w = Some(320);
    preset.video_max_h = Some(240);
    preset.video_fps_cap = Some(25);
    preset
}

/// The rate-control paragraph of §13 must be reachable end to end: each mode
/// encodes a real playlist, writes the intended value into the diagnostics and
/// every segment starts on an I frame.
#[test]
#[ignore = "host only: needs linked FFmpeg + the ffmpeg CLI"]
fn every_rate_control_encodes_segments_that_start_on_i_frames() {
    if !libav_ready() {
        return;
    }
    if !ffmpeg_cli_ready() {
        return;
    }
    let dir = temp_dir("rate-control");
    let input = h264_aac_fixture(&dir);

    for mode in ["auto", "vbr", "cbr", "cq"] {
        let mut preset = matrix_base();
        preset.video_copy = false;
        preset.audio_copy = true;
        preset.sw_video_encoder = "libx264".into();
        preset.video_rate_control = mode.into();
        preset.video_quality = Some(23);
        let (info, log, out_dir) = run_job(&input, &preset, &format!("rc-{mode}"));
        assert_eq!(info.video_action.as_deref(), Some("sw"), "{mode}: {log}");
        let plan_line = log
            .lines()
            .find(|line| line.contains("video plan: action=sw"))
            .unwrap_or_else(|| panic!("{mode}: no video plan diagnostic\n{log}"));
        assert!(
            plan_line.contains(&format!("rate_control={mode}")),
            "{mode}: the diag must name the shipped rate control: {plan_line}"
        );

        let segments = segment_paths(&out_dir);
        assert!(
            segments.len() >= 2,
            "{mode}: only {} segments",
            segments.len()
        );
        for (index, segment) in segments.iter().enumerate() {
            let pict = first_video_pict_type(segment);
            assert!(
                pict.starts_with('I'),
                "{mode}: seg{index} starts with {pict:?}, not an I frame"
            );
        }
        eprintln!(
            "rate control {mode}: {} segments, {} bytes, {plan_line}",
            segments.len(),
            segment_bytes(&out_dir)
        );
        let _ = std::fs::remove_dir_all(&out_dir);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// CQVBR/CRF and CBR must actually pick different encodings — otherwise the
/// structured `video_quality` field would silently do nothing.
#[test]
#[ignore = "host only: needs linked FFmpeg + the ffmpeg CLI"]
fn cbr_and_cq_produce_different_bytes_per_second() {
    if !libav_ready() {
        return;
    }
    if !ffmpeg_cli_ready() {
        return;
    }
    let dir = temp_dir("rate-size");
    let input = h264_aac_fixture(&dir);

    let mut base = matrix_base();
    base.video_copy = false;
    base.audio_copy = true;
    base.sw_video_encoder = "libx264".into();
    base.video_max_bitrate = Some(4_000_000);

    let mut cbr = base.clone();
    cbr.video_rate_control = "cbr".into();
    let (_, cbr_log, cbr_dir) = run_job(&input, &cbr, "size-cbr");

    let mut cq = base;
    cq.video_rate_control = "cq".into();
    cq.video_quality = Some(23);
    let (_, cq_log, cq_dir) = run_job(&input, &cq, "size-cq");

    let cbr_bytes = segment_bytes(&cbr_dir);
    let cq_bytes = segment_bytes(&cq_dir);
    assert!(cbr_bytes > 0, "cbr produced no segments\n{cbr_log}");
    assert!(cq_bytes > 0, "cq produced no segments\n{cq_log}");
    let largest = cbr_bytes.max(cq_bytes) as f64;
    let difference = (cbr_bytes as f64 - cq_bytes as f64).abs() / largest;
    eprintln!(
        "cbr={cbr_bytes}B cq={cq_bytes}B difference={:.1}%",
        difference * 100.0
    );
    assert!(
        difference > 0.05,
        "cbr and cq produced nearly identical output ({cbr_bytes}B vs {cq_bytes}B)"
    );
    let _ = std::fs::remove_dir_all(&cbr_dir);
    let _ = std::fs::remove_dir_all(&cq_dir);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Segments written so far, in playlist order (stops at the first gap).
fn segment_paths(out_dir: &Path) -> Vec<PathBuf> {
    let mut segments = Vec::new();
    for index in 0..64 {
        let segment = out_dir.join(format!("seg{index}.ts"));
        if !segment.is_file() {
            break;
        }
        segments.push(segment);
    }
    segments
}

fn segment_bytes(out_dir: &Path) -> u64 {
    segment_paths(out_dir)
        .iter()
        .map(|path| std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0))
        .sum()
}

/// Picture type of the first video frame in a segment (`ffprobe` csv output).
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

/// Run one job to completion and return its telemetry, diagnostics and output.
fn run_job(input: &Path, preset: &TranscodePreset, tag: &str) -> (EncoderInfo, String, PathBuf) {
    let out_dir =
        std::env::temp_dir().join(format!("wdmm-options-out-{tag}-{}", std::process::id()));
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
    while job.is_running() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let info = job.encoder_info();
    let log = diag::tail(400).join("\n");
    let result = job.take_result();
    drop(job);
    if let Some(Err(err)) = result {
        panic!("{tag}: job failed: {err}\n{log}");
    }
    (info, log, out_dir)
}

/// The schema is the editor's only source of truth (§13.4): it must cover every
/// selectable encoder and only ever advertise keys the option builder accepts.
#[test]
fn schema_covers_every_selectable_encoder_and_whitelisted_key() {
    let raw = encoder_options_schema_json().expect("schema serialises");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("schema is JSON");
    let codecs = value["codecs"].as_array().expect("codecs array");

    let selectable: Vec<&str> = HW_VIDEO_ENCODERS
        .iter()
        .chain(SW_VIDEO_ENCODERS.iter())
        .copied()
        .collect();
    assert_eq!(
        codecs.len(),
        selectable.len(),
        "the schema must list exactly the selectable encoders"
    );

    for codec in codecs {
        let id = codec["id"].as_str().expect("codec id");
        assert!(selectable.contains(&id), "{id} is not a selectable encoder");
        assert!(
            SHIPPED_VIDEO_ENCODERS.contains(&id),
            "{id} is missing from the shipped-encoder list"
        );
        let kind = codec["kind"].as_str().expect("codec kind");
        let family = EncoderFamily::of(id).expect("known encoder family");
        assert_eq!(
            kind,
            if family == EncoderFamily::Hardware {
                "hw"
            } else {
                "sw"
            },
            "{id}: kind must follow the encoder family"
        );
        let rate_control = codec["rate_control"].as_array().expect("rate_control");
        let advertised: Vec<&str> = rate_control.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(advertised, SCHEMA_RATE_CONTROL_MODES, "{id}: rate controls");

        let options = codec["options"].as_array().expect("options");
        let allowed = family.allowed_keys();
        for option in options {
            let key = option["key"].as_str().expect("option key");
            assert!(
                allowed.contains(&key),
                "{id}: schema advertises {key}, the option builder would reject it"
            );
            let kind = option["type"].as_str().expect("option type");
            assert!(
                matches!(kind, "enum" | "int" | "float" | "bool"),
                "{id}: {key} has an unknown type {kind}"
            );
        }
    }

    for codec in SHIPPED_AUDIO_ENCODERS {
        assert!(
            !audio_encoder_name(codec).is_empty(),
            "audio encoder {codec} must map to an avcodec encoder name"
        );
    }
}

/// §13.6: every encoder the Android build explicitly enables must resolve in
/// libavcodec. On a desktop build the MediaCodec encoders are missing, so they
/// are only enforced when `WDMM_ENCODER_GATE_STRICT=1`.
#[test]
#[ignore = "host only: needs linked FFmpeg"]
fn shipped_encoders_resolve_in_libavcodec() {
    if !libav_ready() {
        return;
    }
    let strict = std::env::var("WDMM_ENCODER_GATE_STRICT")
        .map(|value| value == "1")
        .unwrap_or(false);

    let mut missing_video = Vec::new();
    for encoder in SHIPPED_VIDEO_ENCODERS {
        if !encoder_available(encoder) {
            missing_video.push(*encoder);
        }
    }
    let software_missing: Vec<&&str> = missing_video
        .iter()
        .filter(|name| !name.ends_with("_mediacodec"))
        .collect();
    assert!(
        software_missing.is_empty(),
        "software encoders from the trimmed build are missing: {software_missing:?}"
    );
    if strict {
        assert!(
            missing_video.is_empty(),
            "strict gate: shipped encoders missing from libavcodec: {missing_video:?}"
        );
    } else if !missing_video.is_empty() {
        eprintln!(
            "note: hardware encoders absent on this host (set WDMM_ENCODER_GATE_STRICT=1 on the trim build): {missing_video:?}"
        );
    }

    for codec in SHIPPED_AUDIO_ENCODERS {
        let name = audio_encoder_name(codec);
        assert!(
            encoder_available(name),
            "audio encoder {name} (preset value {codec}) is missing from libavcodec"
        );
    }
}

/// §13.5 priority: invariants > the preset's advanced options > the structured
/// fields. The dictionary is what `avcodec_open2` actually receives.
#[test]
fn preset_advanced_options_reach_the_dictionary() {
    let mut preset = preset::preset_h264();
    preset.sw_video_options.insert("tune".into(), "film".into());
    preset.sw_video_options.insert("crf".into(), "20".into());
    preset
        .sw_video_options
        .insert("x264-params".into(), "keyint=10:tune=animation".into());

    let options = video_encoder_options(&preset, "libx264", false, 4_000_000, 50)
        .expect("libx264 options build");
    assert_eq!(options.get("preset"), Some(preset.sw_video_preset.as_str()));
    assert_eq!(options.get("tune"), Some("film"));
    assert_eq!(options.get("crf"), Some("20"));
    let params = options.get("x264-params").expect("x264-params");
    assert!(
        params.contains("keyint=50")
            && params.contains("min-keyint=50")
            && params.contains("scenecut=0"),
        "the segment grid invariant must win: {params}"
    );
    assert!(
        params.contains("tune=animation"),
        "free-form x264-params tokens survive into the dictionary: {params}"
    );
    assert!(
        options
            .overridden
            .contains(&"x264-params:keyint".to_string()),
        "the overridden invariant must be reported: {:?}",
        options.overridden
    );
    assert_eq!(options.user_option_count, 3);

    let stripped = video_encoder_options_without_user(&preset, "libx264", false, 4_000_000, 50)
        .expect("libx264 options without user entries");
    assert_eq!(stripped.get("tune"), None);
    assert_eq!(stripped.get("crf"), None);
    assert_eq!(
        stripped.get("preset"),
        Some(preset.sw_video_preset.as_str()),
        "structured fields survive the retry pass"
    );
    assert_eq!(stripped.user_option_count, 0);
}

/// §13.5 failure path: a preset option the encoder refuses must not fail the
/// cast — the job retries without it and says so.
#[test]
#[ignore = "host only: needs linked FFmpeg + the ffmpeg CLI"]
fn rejected_preset_options_are_dropped_and_announced() {
    if !libav_ready() {
        return;
    }
    if !ffmpeg_cli_ready() {
        return;
    }
    let dir = temp_dir("retry");
    let input = h264_aac_fixture(&dir);

    let mut preset = matrix_base();
    preset.video_copy = false;
    preset.audio_copy = true;
    preset.sw_video_encoder = "libx264".into();
    // `profile` is whitelisted, but no libx264 profile is called this, so the
    // first `avcodec_open2` attempt fails exactly like a bad user option would.
    preset
        .sw_video_options
        .insert("profile".into(), "definitely-not-a-profile".into());
    let (info, log, out_dir) = run_job(&input, &preset, "retry");
    assert_eq!(info.video_action.as_deref(), Some("sw"), "{log}");
    assert!(
        log.contains("video: encoder options rejected by the codec"),
        "dropping the preset's advanced options must be announced\n{log}"
    );
    let _ = std::fs::remove_dir_all(&out_dir);

    let mut preset = matrix_base();
    preset.video_copy = false;
    preset.audio_copy = true;
    preset.sw_video_encoder = "libx264".into();
    preset.sw_video_options.insert("tune".into(), "film".into());
    preset.video_rate_control = "cq".into();
    preset.video_quality = Some(24);
    let (info, log, out_dir) = run_job(&input, &preset, "accepted");
    assert_eq!(info.video_action.as_deref(), Some("sw"), "{log}");
    assert!(
        !log.contains("encoder options rejected by the codec"),
        "valid options must be accepted\n{log}"
    );
    let plan_line = log
        .lines()
        .find(|line| line.contains("video plan: action=sw"))
        .unwrap_or_else(|| panic!("no video plan diagnostic\n{log}"));
    eprintln!("accepted: {plan_line}");
    for expected in ["rate_control=cq", "options=", "tune=film", "crf=24"] {
        assert!(
            plan_line.contains(expected),
            "the diag must spell out the shipped options ({expected}): {plan_line}"
        );
    }
    let _ = std::fs::remove_dir_all(&out_dir);
    let _ = std::fs::remove_dir_all(&dir);
}
