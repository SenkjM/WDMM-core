//! Host-only feasibility probe for "whole-file keyframe index without demuxing"
//! (research task: VOD plan grid must match the hls muxer's real cut points).
//!
//! The index helpers (`AVIndexEntry`, `avformat_index_get_entries_count`,
//! `avformat_index_get_entry`) were removed from `src/sys.rs` in 44bc415, so
//! this standalone probe re-declares just the symbols it needs. It never runs
//! in CI: it requires linked FFmpeg 7.1 and `WDMM_PROBE_MEDIA`.
//!
//! `WDMM_PROBE_MEDIA` is a `;`-separated list of media paths, e.g.
//!
//! ```text
//! $env:FF='E:\test\video\_env\ffmpeg-n7.1.5-...-7.1'
//! $env:WDMM_FFMPEG_LIB_DIR="$env:FF\lib"; $env:RUSTFLAGS="-L native=$env:FF\lib"
//! $env:PATH="$env:FF\bin;$env:PATH"
//! $env:WDMM_PROBE_MEDIA='E:\test\video\1.m4v;E:\test\video\_env\out_copy.mkv;E:\test\video\_env\out_g48.mkv'
//! cargo test -p wdmm-ffmpeg --features ffmpeg --test keyframe_index_probe -- --ignored --nocapture
//! ```
//!
//! For each file it prints:
//! 1. per-stream `nb_index_entries` after `avformat_find_stream_info`;
//! 2. the video stream's first 10 / last 3 index entries (pts in seconds,
//!    raw flags, keyframe bit) plus the total keyframe count in the index;
//! 3. the same counts after `av_seek_frame(fmt, -1, 0, AVSEEK_FLAG_BACKWARD)`
//!    (does a cheap seek to the start trigger Matroska's deferred Cues parse?);
//! 4. the same counts after `av_seek_frame(fmt, -1, 100s, AVSEEK_FLAG_BACKWARD)`
//!    (does a seek populate the index?), and the first video packet's pts after
//!    the seek with its error against the 100 s target;
//! 5. the same counts after a seek to ~EOF (for cue-less files this point shows
//!    the index only completing via a full linear demux).

#![cfg(feature = "ffmpeg")]

use std::ffi::{c_char, c_int, c_void, CString};

use wdmm_ffmpeg::diag;

// ── Minimal local FFI (the real declarations live in the private src/sys.rs) ──

pub const AVMEDIA_TYPE_VIDEO: c_int = 0;
pub const AVSEEK_FLAG_BACKWARD: c_int = 1;
pub const AVINDEX_KEYFRAME: c_int = 0x0001;
pub const AV_PKT_FLAG_KEY: c_int = 0x0001;
/// `AV_TIME_BASE` — fixed-point time base of the `av_seek_frame` timestamp when
/// `stream_index == -1` (1 µs).
pub const AV_TIME_BASE: i64 = 1_000_000;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct AVRational {
    pub num: c_int,
    pub den: c_int,
}

#[repr(C)]
pub struct AVFormatContext {
    _p: [u8; 0],
}

#[repr(C)]
pub struct AVCodecParameters {
    pub codec_type: c_int, // +0
    pub codec_id: u32,     // +4
    _gap: [u8; 36],        // +8
    _rest: [u8; 132],      // +44 .. sizeof=176
}

#[repr(C)]
pub struct AVStream {
    _avclass: *const c_void,              // +0
    pub index: c_int,                     // +8
    _id: c_int,                           // +12
    pub codecpar: *mut AVCodecParameters, // +16
    _opaque: *mut c_void,                 // +24
    pub time_base: AVRational,            // +32
    pub start_time: i64,                  // +40
    pub duration: i64,                    // +48
    _gap0: [u8; 168],                     // +56 .. +224 (nb_frames .. r_frame_rate)
    _tail: [u8; 8],                       // +224 .. sizeof=232
}

/// `AVIndexEntry` (24 bytes, LP64): pos=0(8), timestamp=8(8),
/// flags:2+size:30 packed at +16(4), min_distance=+20(4).
#[repr(C)]
pub struct AVIndexEntry {
    pub pos: i64,
    pub timestamp: i64,
    pub flags_and_size: c_int,
    pub min_distance: c_int,
}

impl AVIndexEntry {
    fn flags(&self) -> c_int {
        self.flags_and_size & 0b11
    }
    fn is_keyframe(&self) -> bool {
        self.flags() & AVINDEX_KEYFRAME != 0
    }
}

/// `AVPacket` (104 bytes): pts=+8, dts=+16, size=+32, stream_index=+36,
/// flags=+40, duration=+64, pos=+72, time_base=+96.
#[repr(C)]
pub struct AVPacket {
    _buf: *mut c_void,
    pub pts: i64,
    pub dts: i64,
    pub data: *mut u8,
    pub size: c_int,
    pub stream_index: c_int,
    pub flags: c_int,
    _pad_a: c_int,
    _side_data: *mut c_void,
    _side_data_elems: c_int,
    _pad_b: c_int,
    pub duration: i64,
    pub pos: i64,
    _opaque: *mut c_void,
    _opaque_ref: *mut c_void,
    pub time_base: AVRational,
}

#[link(name = "avformat")]
extern "C" {
    fn avformat_open_input(
        ps: *mut *mut AVFormatContext,
        url: *const c_char,
        fmt: *const c_void,
        options: *mut *mut c_void,
    ) -> c_int;
    fn avformat_find_stream_info(ic: *mut AVFormatContext, options: *mut *mut c_void) -> c_int;
    fn avformat_close_input(s: *mut *mut AVFormatContext);
    fn avformat_index_get_entries_count(st: *const AVStream) -> c_int;
    fn avformat_index_get_entry(st: *mut AVStream, idx: c_int) -> *const AVIndexEntry;
    fn av_seek_frame(
        ic: *mut AVFormatContext,
        stream_index: c_int,
        timestamp: i64,
        flags: c_int,
    ) -> c_int;
    fn av_read_frame(ic: *mut AVFormatContext, pkt: *mut AVPacket) -> c_int;
    fn av_packet_unref(pkt: *mut AVPacket);
}

// ── Accessors for the opaque context (offsets match src/sys.rs) ──────────────

unsafe fn fmt_ctx_nb_streams(ctx: *const AVFormatContext) -> u32 {
    *(ctx as *const u8).add(44).cast()
}

unsafe fn fmt_ctx_streams(ctx: *const AVFormatContext) -> *const *mut AVStream {
    *(ctx as *const u8).add(48).cast()
}

unsafe fn fmt_ctx_duration(ctx: *const AVFormatContext) -> i64 {
    *(ctx as *const u8).add(104).cast()
}

fn secs(timestamp: i64, time_base: AVRational) -> f64 {
    if time_base.den == 0 {
        return 0.0;
    }
    timestamp as f64 * time_base.num as f64 / time_base.den as f64
}

unsafe fn stream_entries(st: *mut AVStream) -> Vec<(f64, c_int, bool)> {
    let count = avformat_index_get_entries_count(st);
    let tb = (*st).time_base;
    let mut entries = Vec::new();
    for idx in 0..count.max(0) {
        let entry = avformat_index_get_entry(st, idx as c_int);
        if entry.is_null() {
            continue;
        }
        let entry = &*entry;
        entries.push((
            secs(entry.timestamp, tb),
            entry.flags(),
            entry.is_keyframe(),
        ));
    }
    entries
}

unsafe fn print_stream_indexes(tag: &str, ctx: *mut AVFormatContext) {
    let nb = fmt_ctx_nb_streams(ctx) as usize;
    let streams = fmt_ctx_streams(ctx);
    for i in 0..nb {
        let st = *streams.add(i);
        if st.is_null() {
            continue;
        }
        let codec_type = if (*st).codecpar.is_null() {
            -1
        } else {
            (*(*st).codecpar).codec_type
        };
        let kf = if codec_type == AVMEDIA_TYPE_VIDEO {
            let entries = stream_entries(st);
            let keyframes = entries.iter().filter(|(_, _, k)| *k).count();
            format!(
                " video_entries={} keyframe_entries={}",
                entries.len(),
                keyframes
            )
        } else {
            String::new()
        };
        println!(
            "[{tag}] stream {} codec_type={} nb_index_entries={} time_base={}/{} {}",
            (*st).index,
            codec_type,
            avformat_index_get_entries_count(st),
            (*st).time_base.num,
            (*st).time_base.den,
            kf
        );
        if codec_type == AVMEDIA_TYPE_VIDEO {
            let entries = stream_entries(st);
            let shown_first = entries.len().min(10);
            for (n, (pts, flags, key)) in entries.iter().take(shown_first).enumerate() {
                println!(
                    "[{tag}]   video entry[{n}] pts={pts:.3}s flags=0x{flags:x} keyframe={key}"
                );
            }
            if entries.len() > 10 {
                let skip = entries.len().saturating_sub(3);
                for (n, (pts, flags, key)) in entries.iter().skip(skip).enumerate() {
                    println!(
                        "[{tag}]   video entry[{}] pts={pts:.3}s flags=0x{flags:x} keyframe={key}",
                        skip + n
                    );
                }
            }
        }
    }
}

unsafe fn probe_one(path: &str) {
    println!("\n===== {path} =====");
    let Ok(url_c) = CString::new(path) else {
        println!("path contains NUL: {path}");
        return;
    };
    let mut ctx: *mut AVFormatContext = std::ptr::null_mut();
    let ret = avformat_open_input(
        &mut ctx,
        url_c.as_ptr(),
        std::ptr::null(),
        std::ptr::null_mut(),
    );
    if ret < 0 || ctx.is_null() {
        println!("avformat_open_input failed: rc={ret}");
        return;
    }
    let ret = avformat_find_stream_info(ctx, std::ptr::null_mut());
    if ret < 0 {
        println!("avformat_find_stream_info failed: rc={ret}");
        avformat_close_input(&mut ctx);
        return;
    }

    println!(
        "after avformat_find_stream_info: nb_streams={}",
        fmt_ctx_nb_streams(ctx)
    );
    print_stream_indexes("after-fsi", ctx);

    // Experiment A: a single cheap backward seek to the start. For Matroska the
    // Cues parse is deferred to the first seek, so this is expected to populate
    // the full keyframe index without demuxing the media. For MP4 the index is
    // already complete after find_stream_info, so the seek is a no-op there.
    let seek0_rc = av_seek_frame(ctx, -1, 0, AVSEEK_FLAG_BACKWARD);
    println!("av_seek_frame(-1, 0, BACKWARD) rc={seek0_rc}");
    if seek0_rc >= 0 {
        print_stream_indexes("after-seek-0", ctx);
    }

    // Ground-truth-ish: find the video stream index for the packet walk.
    let nb = fmt_ctx_nb_streams(ctx) as usize;
    let streams = fmt_ctx_streams(ctx);
    let mut video_idx = -1_i32;
    for i in 0..nb {
        let st = *streams.add(i);
        if st.is_null() || (*st).codecpar.is_null() {
            continue;
        }
        if (*(*st).codecpar).codec_type == AVMEDIA_TYPE_VIDEO {
            video_idx = (*st).index;
            break;
        }
    }

    // Experiment B: seek exactly like a future "plan from keyframe index"
    // implementation would: stream_index=-1, timestamp in AV_TIME_BASE,
    // backward to land on a keyframe.
    let target_secs = 100.0_f64;
    let target = (target_secs * AV_TIME_BASE as f64) as i64;
    let seek_rc = av_seek_frame(ctx, -1, target, AVSEEK_FLAG_BACKWARD);
    println!("av_seek_frame(-1, {target}, BACKWARD) rc={seek_rc}");
    if seek_rc >= 0 {
        print_stream_indexes("after-seek-100", ctx);

        let mut pkt: AVPacket = std::mem::zeroed();
        let mut consumed = 0_usize;
        let mut first_video: Option<(f64, c_int)> = None;
        loop {
            let rc = av_read_frame(ctx, &mut pkt);
            if rc < 0 {
                println!("   (av_read_frame stopped: rc={rc} after {consumed} packets)");
                break;
            }
            consumed += 1;
            if pkt.stream_index == video_idx {
                let st = *streams.add(video_idx as usize);
                let tb = if st.is_null() {
                    AVRational { num: 1, den: 1 }
                } else {
                    (*st).time_base
                };
                first_video = Some((secs(pkt.pts, tb), pkt.flags));
                av_packet_unref(&mut pkt);
                break;
            }
            av_packet_unref(&mut pkt);
            if consumed > 200_000 {
                println!("   (packet walk aborted after {consumed} packets)");
                break;
            }
        }
        match first_video {
            Some((pts, flags)) => {
                let delta = pts - target_secs;
                println!(
                    "first video packet after seek: pts={pts:.3}s flags=0x{flags:x} keyframe={} delta_vs_target={delta:+.3}s (packets_read={consumed})",
                    flags & AV_PKT_FLAG_KEY != 0
                );
            }
            None => println!("no video packet readable after seek (packets_read={consumed})"),
        }
    }

    // Experiment D: how does `av_seek_frame` land as a function of the target?
    // Used to calibrate the plan-start offset: the MP4 index stores DTS while
    // seeking uses PTS, so a plan start taken straight from the index can land
    // on the previous keyframe.
    if let Some(raw_targets) = std::env::var_os("WDMM_PROBE_SEEK_SECS") {
        for part in raw_targets
            .to_string_lossy()
            .split(';')
            .map(str::trim)
            .filter(|part| !part.is_empty())
        {
            let Ok(target_secs) = part.parse::<f64>() else {
                continue;
            };
            let target = (target_secs * AV_TIME_BASE as f64) as i64;
            let rc = av_seek_frame(ctx, -1, target, AVSEEK_FLAG_BACKWARD);
            let mut landing = String::from("no-video-packet");
            if rc >= 0 {
                let mut pkt: AVPacket = std::mem::zeroed();
                let mut consumed = 0_usize;
                loop {
                    let rc = av_read_frame(ctx, &mut pkt);
                    if rc < 0 {
                        break;
                    }
                    consumed += 1;
                    if pkt.stream_index == video_idx {
                        let st = *streams.add(video_idx as usize);
                        let tb = if st.is_null() {
                            AVRational { num: 1, den: 1 }
                        } else {
                            (*st).time_base
                        };
                        landing = format!(
                            "pts={:.3}s keyframe={} packets_read={consumed}",
                            secs(pkt.pts, tb),
                            pkt.flags & AV_PKT_FLAG_KEY != 0
                        );
                        av_packet_unref(&mut pkt);
                        break;
                    }
                    av_packet_unref(&mut pkt);
                    if consumed > 200_000 {
                        break;
                    }
                }
            }
            println!("seek target {target_secs:.3}s rc={rc} → first video {landing}");
        }
    }

    // Experiment C: seek near the very end. For a file without Cues this forces
    // a linear scan: the index grows to the full keyframe count only by reading
    // (demuxing) the whole file, which is exactly the cost we want to avoid for
    // remote WebDAV sources.
    let duration_us = fmt_ctx_duration(ctx);
    let vdur = if video_idx >= 0 {
        let st = *streams.add(video_idx as usize);
        if st.is_null() {
            0
        } else {
            (*st).duration
        }
    } else {
        0
    };
    let vtb = if video_idx >= 0 {
        let st = *streams.add(video_idx as usize);
        if st.is_null() {
            AVRational { num: 1, den: 1 }
        } else {
            (*st).time_base
        }
    } else {
        AVRational { num: 1, den: 1 }
    };
    println!(
        "durations: fmt={:.3}s video_stream={:.3}s (raw={vdur} tb={}/{})",
        duration_us as f64 / 1e6,
        secs(vdur, vtb),
        vtb.num,
        vtb.den
    );
    if duration_us > 0 {
        let eof_target = duration_us.saturating_sub(AV_TIME_BASE);
        let seek_eof_rc = av_seek_frame(ctx, -1, eof_target, AVSEEK_FLAG_BACKWARD);
        println!(
            "av_seek_frame(-1, {:.3}s, BACKWARD) rc={seek_eof_rc} (fmt duration {:.3}s)",
            eof_target as f64 / 1e6,
            duration_us as f64 / 1e6
        );
        if seek_eof_rc >= 0 {
            print_stream_indexes("after-seek-eof", ctx);
        }
    }

    avformat_close_input(&mut ctx);
}

#[test]
#[ignore = "needs WDMM_PROBE_MEDIA + linked FFmpeg 7.1"]
fn keyframe_index_probe() {
    let Some(raw) = std::env::var_os("WDMM_PROBE_MEDIA") else {
        eprintln!("skip: WDMM_PROBE_MEDIA not set");
        return;
    };
    let list = raw.to_string_lossy();
    let paths: Vec<&str> = list
        .split(';')
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .collect();
    println!("probing {} path(s)", paths.len());
    for path in paths {
        if !std::path::Path::new(path).is_file() {
            println!("\n===== {path} =====\nskip: not a file");
            continue;
        }
        unsafe { probe_one(path) };
    }
    println!("\n--- diag tail ---\n{}", diag::tail(20).join("\n"));
}
