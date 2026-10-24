// FFprobe implementation

use crate::{AudioStream, FfmpegError, FfmpegResult, ProbeResult, SubtitleStream, VideoStream};
use std::path::Path;

pub fn probe_file(path: &Path) -> FfmpegResult<ProbeResult> {
    let ffprobe = super::spawn::ffprobe_bin()?;
    let output = std::process::Command::new(&ffprobe)
        .args([
            "-v", "quiet",
            "-print_format", "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(FfmpegError::Probe(format!("ffprobe exit {}: {}", output.status, stderr)));
    }

    let json_str = String::from_utf8_lossy(&output.stdout);
    let v: serde_json::Value = serde_json::from_str(&json_str)
        .map_err(|e| FfmpegError::Probe(format!("json parse: {}", e)))?;

    let mut result = ProbeResult {
        duration_secs: v["format"]["duration"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0),
        video: Vec::new(),
        audio: Vec::new(),
        subtitles: Vec::new(),
        format_name: v["format"]["format_name"]
            .as_str()
            .unwrap_or("unknown")
            .to_string(),
    };

    if let Some(streams) = v["streams"].as_array() {
        for (idx, stream) in streams.iter().enumerate() {
            let codec_type = stream["codec_type"].as_str().unwrap_or("");
            let codec_name = stream["codec_name"].as_str().unwrap_or("unknown").to_string();
            let language = stream["tags"]["language"].as_str().map(Into::into);

            match codec_type {
                "video" => result.video.push(VideoStream {
                    index: idx as u32,
                    codec: codec_name,
                    width: stream["width"].as_u64().unwrap_or(0) as u32,
                    height: stream["height"].as_u64().unwrap_or(0) as u32,
                    fps: stream["r_frame_rate"]
                        .as_str()
                        .and_then(|s| parse_fraction(s))
                        .unwrap_or(0.0),
                }),
                "audio" => result.audio.push(AudioStream {
                    index: idx as u32,
                    codec: codec_name,
                    channels: stream["channels"].as_u64().unwrap_or(0) as u32,
                    sample_rate: stream["sample_rate"]
                        .as_str()
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(0),
                    language,
                }),
                "subtitle" => result.subtitles.push(SubtitleStream {
                    index: idx as u32,
                    codec: codec_name,
                    language,
                }),
                _ => {}
            }
        }
    }

    Ok(result)
}

fn parse_fraction(s: &str) -> Option<f64> {
    let parts: Vec<&str> = s.split('/').collect();
    if parts.len() == 2 {
        let num = parts[0].parse::<f64>().ok()?;
        let den = parts[1].parse::<f64>().ok()?;
        if den > 0.0 {
            Some(num / den)
        } else {
            None
        }
    } else {
        s.parse::<f64>().ok()
    }
}
