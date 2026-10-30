//! Hand-rolled FFI bindings for FFmpeg 7.1 (x86_64 / arm64 LP64).
//!
//! Struct field offsets were verified by compiling a C offset-probe against the
//! FFmpeg 7.1 source headers on x86_64. arm64 is identical (both LP64).
//! armeabi-v7a (ILP32, 32-bit pointers) is NOT supported.
//!
//! Large opaque types (AVFormatContext, AVCodecContext, AVFrame, …) are
//! declared as zero-sized markers; fields are accessed via pointer arithmetic
//! using the confirmed byte offsets to avoid any risk of Rust alignment padding.

#![allow(non_camel_case_types, dead_code, non_snake_case, clippy::too_many_arguments)]

use std::ffi::{c_char, c_int, c_uint, c_void};

// ── Primitive type aliases ────────────────────────────────────────────────────

pub type AVMediaType    = c_int;
pub type AVCodecID      = c_uint;
pub type AVPixelFormat  = c_int;
pub type AVSampleFormat = c_int;

// ── Constants ─────────────────────────────────────────────────────────────────

pub const AVMEDIA_TYPE_VIDEO: AVMediaType = 0;
pub const AVMEDIA_TYPE_AUDIO: AVMediaType = 1;
pub const AVMEDIA_TYPE_DATA:  AVMediaType = 2;

pub const AV_NOPTS_VALUE: i64 = i64::MIN;

/// `AVERROR(EAGAIN)` — no data available yet, try again.
pub const AVERROR_EAGAIN: c_int = -11;
/// `AVERROR_EOF` — end of stream.
pub const AVERROR_EOF: c_int = -541_478_725;

/// `avio_open` flag: write-only.
pub const AVIO_FLAG_WRITE: c_int = 2;

pub const AVSEEK_FLAG_BACKWARD: c_int = 1;
pub const AVSEEK_FLAG_ANY:      c_int = 4;

/// `AV_OPT_SEARCH_CHILDREN` — search sub-objects when setting options.
pub const AV_OPT_SEARCH_CHILDREN: c_int = 1;

// Well-known codec IDs.
pub const AV_CODEC_ID_H264: AVCodecID = 27;
pub const AV_CODEC_ID_AAC:  AVCodecID = 86018;
pub const AV_CODEC_ID_MP3:  AVCodecID = 86017;

// Pixel/sample format values (FFmpeg 7.1).
pub const AV_PIX_FMT_YUV420P:  AVPixelFormat  = 0;
pub const AV_SAMPLE_FMT_FLTP:  AVSampleFormat = 8;
pub const AV_SAMPLE_FMT_S16P:  AVSampleFormat = 6;

// AV_ROUND_NEAR_INF | AV_ROUND_PASS_MINMAX
pub const AV_ROUND_NEAR_INF_PASS: u32 = 5 | (1 << 15);

// ── AVRational (8 bytes) ──────────────────────────────────────────────────────

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct AVRational {
    pub num: c_int,
    pub den: c_int,
}

impl AVRational {
    pub const fn new(num: c_int, den: c_int) -> Self { Self { num, den } }
}

// ── AVChannelLayout (24 bytes, LP64) ─────────────────────────────────────────
// order=0(4), nb_channels=4(4), u=8(8 union mask|ptr), opaque=16(8).

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AVChannelLayout {
    pub order:       c_int,   // +0  AVChannelOrder
    pub nb_channels: c_int,   // +4
    _u:              u64,     // +8  union (channel mask for NATIVE order)
    _opaque:         u64,     // +16 custom channel-map pointer (usually 0)
}                             // sizeof=24 ✓

impl AVChannelLayout {
    /// Stereo (AV_CHANNEL_ORDER_NATIVE, mask = FL|FR = 3).
    pub const fn stereo() -> Self {
        Self { order: 1, nb_channels: 2, _u: 3, _opaque: 0 }
    }
    /// Mono (AV_CHANNEL_ORDER_NATIVE, mask = FL = 1).
    pub const fn mono() -> Self {
        Self { order: 1, nb_channels: 1, _u: 1, _opaque: 0 }
    }
}

// ── AVCodecParameters (176 bytes, LP64) ──────────────────────────────────────
// codec_type=0(4), codec_id=4(4), _gap=8(36),
// format=44(4), bit_rate=48(8), _gap=56(16),
// width=72(4), height=76(4), _gap=80(48),
// ch_layout=128(24), sample_rate=152(4), _tail=156(20).

#[repr(C)]
pub struct AVCodecParameters {
    pub codec_type:  AVMediaType,      // +0  (4)
    pub codec_id:    AVCodecID,        // +4  (4)
    _gap0:           [u8; 36],         // +8  (36)
    pub format:      c_int,            // +44 (4)
    pub bit_rate:    i64,              // +48 (8)
    _gap1:           [u8; 16],         // +56 (16)
    pub width:       c_int,            // +72 (4)
    pub height:      c_int,            // +76 (4)
    _gap2:           [u8; 48],         // +80 (48)
    pub ch_layout:   AVChannelLayout,  // +128 (24)
    pub sample_rate: c_int,            // +152 (4)
    _tail:           [u8; 20],         // +156 (20)
}                                      // sizeof=176 ✓

// ── AVStream (232 bytes, LP64) ────────────────────────────────────────────────
// _avclass=0(8), index=8(4), id=12(4), codecpar=16(8),
// _opaque=24(8), time_base=32(8), start_time=40(8), duration=48(8), _tail=56(176).

#[repr(C)]
pub struct AVStream {
    _avclass:         *const c_void,         // +0  (8)
    pub index:        c_int,                 // +8  (4)
    pub id:           c_int,                 // +12 (4)
    pub codecpar:     *mut AVCodecParameters,// +16 (8)
    _opaque:          *mut c_void,           // +24 (8)
    pub time_base:    AVRational,            // +32 (8)
    pub start_time:   i64,                   // +40 (8)
    pub duration:     i64,                   // +48 (8)
    _tail:            [u8; 176],             // +56 (176)
}                                            // sizeof=232 ✓

// ── AVPacket (104 bytes, LP64) ────────────────────────────────────────────────
// _buf=0(8), pts=8(8), dts=16(8), data=24(8),
// size=32(4), stream_index=36(4), flags=40(4),
// _padA=44(4), _side_data=48(8), _side_data_elems=56(4), _padB=60(4),
// duration=64(8), pos=72(8), _opaque=80(8), _opaque_ref=88(8), time_base=96(8).

#[repr(C)]
pub struct AVPacket {
    _buf:             *mut c_void,   // +0  (8) AVBufferRef*
    pub pts:          i64,           // +8  (8)
    pub dts:          i64,           // +16 (8)
    pub data:         *mut u8,       // +24 (8)
    pub size:         c_int,         // +32 (4)
    pub stream_index: c_int,         // +36 (4)
    pub flags:        c_int,         // +40 (4)
    _pad_a:           c_int,         // +44 (4)
    _side_data:       *mut c_void,   // +48 (8)
    _side_data_elems: c_int,         // +56 (4)
    _pad_b:           c_int,         // +60 (4)
    pub duration:     i64,           // +64 (8)
    pub pos:          i64,           // +72 (8)
    _opaque:          *mut c_void,   // +80 (8)
    _opaque_ref:      *mut c_void,   // +88 (8)
    pub time_base:    AVRational,    // +96 (8)
}                                    // sizeof=104 ✓

// ── Opaque contexts (zero-size markers, always behind pointers) ───────────────

#[repr(C)] pub struct AVFormatContext { _p: [u8; 0] }
#[repr(C)] pub struct AVCodecContext  { _p: [u8; 0] }
#[repr(C)] pub struct AVFrame         { _p: [u8; 0] }
#[repr(C)] pub struct AVIOContext     { _p: [u8; 0] }
#[repr(C)] pub struct AVCodec         { _p: [u8; 0] }
#[repr(C)] pub struct AVDictionary    { _p: [u8; 0] }
#[repr(C)] pub struct SwsContext      { _p: [u8; 0] }
#[repr(C)] pub struct SwrContext      { _p: [u8; 0] }

// ── Field accessors for opaque types (pointer arithmetic, verified offsets) ───

// AVFormatContext: sizeof=472
/// nb_streams (u32 @ +44)
#[inline] pub unsafe fn fmtctx_nb_streams(c: *const AVFormatContext) -> u32 {
    *(c as *const u8).add(44).cast()
}
/// streams (*mut *mut AVStream @ +48)
#[inline] pub unsafe fn fmtctx_streams(c: *const AVFormatContext) -> *const *mut AVStream {
    *(c as *const u8).add(48).cast()
}
/// duration (i64 @ +104)
#[inline] pub unsafe fn fmtctx_duration(c: *const AVFormatContext) -> i64 {
    *(c as *const u8).add(104).cast()
}

// AVCodecContext: sizeof=864
/// time_base (AVRational @ +84)
#[inline] pub unsafe fn codctx_time_base(c: *const AVCodecContext) -> AVRational {
    *(c as *const u8).add(84).cast()
}
#[inline] pub unsafe fn codctx_set_time_base(c: *mut AVCodecContext, v: AVRational) {
    *(c as *mut u8).add(84).cast() = v;
}
/// bit_rate (i64 @ +56)
#[inline] pub unsafe fn codctx_set_bit_rate(c: *mut AVCodecContext, v: i64) {
    *(c as *mut u8).add(56).cast() = v;
}
/// width (i32 @ +116), height (i32 @ +120)
#[inline] pub unsafe fn codctx_set_width(c: *mut AVCodecContext, v: c_int) {
    *(c as *mut u8).add(116).cast() = v;
}
#[inline] pub unsafe fn codctx_set_height(c: *mut AVCodecContext, v: c_int) {
    *(c as *mut u8).add(120).cast() = v;
}
/// pix_fmt (i32 @ +140)
#[inline] pub unsafe fn codctx_set_pix_fmt(c: *mut AVCodecContext, v: c_int) {
    *(c as *mut u8).add(140).cast() = v;
}
/// gop_size (i32 @ +332), max_b_frames (i32 @ +200)
#[inline] pub unsafe fn codctx_set_gop_size(c: *mut AVCodecContext, v: c_int) {
    *(c as *mut u8).add(332).cast() = v;
}
#[inline] pub unsafe fn codctx_set_max_b_frames(c: *mut AVCodecContext, v: c_int) {
    *(c as *mut u8).add(200).cast() = v;
}
/// sample_rate (i32 @ +344), sample_fmt (i32 @ +348)
#[inline] pub unsafe fn codctx_set_sample_rate(c: *mut AVCodecContext, v: c_int) {
    *(c as *mut u8).add(344).cast() = v;
}
#[inline] pub unsafe fn codctx_set_sample_fmt(c: *mut AVCodecContext, v: c_int) {
    *(c as *mut u8).add(348).cast() = v;
}
/// ch_layout (AVChannelLayout @ +352)
#[inline] pub unsafe fn codctx_set_ch_layout(c: *mut AVCodecContext, v: AVChannelLayout) {
    *(c as *mut u8).add(352).cast() = v;
}
/// frame_size (i32 @ +376) — read-only (set by encoder after open)
#[inline] pub unsafe fn codctx_frame_size(c: *const AVCodecContext) -> c_int {
    *(c as *const u8).add(376).cast()
}

// AVFrame: sizeof=440
/// width(i32@+104), height(i32@+108), nb_samples(i32@+112), format(i32@+116)
#[inline] pub unsafe fn frame_width(f: *const AVFrame) -> c_int { *(f as *const u8).add(104).cast() }
#[inline] pub unsafe fn frame_height(f: *const AVFrame) -> c_int { *(f as *const u8).add(108).cast() }
#[inline] pub unsafe fn frame_nb_samples(f: *const AVFrame) -> c_int { *(f as *const u8).add(112).cast() }
#[inline] pub unsafe fn frame_format(f: *const AVFrame) -> c_int { *(f as *const u8).add(116).cast() }
#[inline] pub unsafe fn frame_pts(f: *const AVFrame) -> i64 { *(f as *const u8).add(136).cast() }
#[inline] pub unsafe fn frame_sample_rate(f: *const AVFrame) -> c_int { *(f as *const u8).add(192).cast() }
/// data[plane] (*mut u8 @ +plane*8)
#[inline] pub unsafe fn frame_data(f: *const AVFrame, plane: usize) -> *mut u8 { *(f as *const u8).add(plane*8).cast() }
/// linesize[plane] (i32 @ +64+plane*4)
#[inline] pub unsafe fn frame_linesize(f: *const AVFrame, plane: usize) -> c_int { *(f as *const u8).add(64+plane*4).cast() }
/// ch_layout (AVChannelLayout @ +408)
#[inline] pub unsafe fn frame_ch_layout(f: *const AVFrame) -> *const AVChannelLayout { (f as *const u8).add(408).cast() }

#[inline] pub unsafe fn frame_set_width(f: *mut AVFrame, v: c_int) { *(f as *mut u8).add(104).cast() = v; }
#[inline] pub unsafe fn frame_set_height(f: *mut AVFrame, v: c_int) { *(f as *mut u8).add(108).cast() = v; }
#[inline] pub unsafe fn frame_set_nb_samples(f: *mut AVFrame, v: c_int) { *(f as *mut u8).add(112).cast() = v; }
#[inline] pub unsafe fn frame_set_format(f: *mut AVFrame, v: c_int) { *(f as *mut u8).add(116).cast() = v; }
#[inline] pub unsafe fn frame_set_pts(f: *mut AVFrame, v: i64) { *(f as *mut u8).add(136).cast() = v; }
#[inline] pub unsafe fn frame_set_sample_rate(f: *mut AVFrame, v: c_int) { *(f as *mut u8).add(192).cast() = v; }
#[inline] pub unsafe fn frame_set_ch_layout(f: *mut AVFrame, v: AVChannelLayout) { *(f as *mut u8).add(408).cast() = v; }

// ── extern "C" declarations ───────────────────────────────────────────────────

extern "C" {
    // format / demux
    pub fn avformat_open_input(ps: *mut *mut AVFormatContext, url: *const c_char,
        fmt: *const c_void, options: *mut *mut AVDictionary) -> c_int;
    pub fn avformat_find_stream_info(ic: *mut AVFormatContext, opts: *mut *mut AVDictionary) -> c_int;
    pub fn av_read_frame(s: *mut AVFormatContext, pkt: *mut AVPacket) -> c_int;
    pub fn av_seek_frame(s: *mut AVFormatContext, stream_index: c_int, timestamp: i64, flags: c_int) -> c_int;
    pub fn avformat_close_input(s: *mut *mut AVFormatContext);
    pub fn avformat_free_context(s: *mut AVFormatContext);

    // mux / output
    pub fn avformat_alloc_output_context2(ctx: *mut *mut AVFormatContext, oformat: *const c_void,
        format_name: *const c_char, filename: *const c_char) -> c_int;
    pub fn avformat_new_stream(s: *mut AVFormatContext, c: *const AVCodec) -> *mut AVStream;
    pub fn avformat_write_header(s: *mut AVFormatContext, options: *mut *mut AVDictionary) -> c_int;
    pub fn av_write_trailer(s: *mut AVFormatContext) -> c_int;
    pub fn av_interleaved_write_frame(s: *mut AVFormatContext, pkt: *mut AVPacket) -> c_int;

    // packets
    pub fn av_packet_alloc() -> *mut AVPacket;
    pub fn av_packet_free(pkt: *mut *mut AVPacket);
    pub fn av_packet_unref(pkt: *mut AVPacket);
    pub fn av_packet_rescale_ts(pkt: *mut AVPacket, tb_src: AVRational, tb_dst: AVRational);

    // codecs
    pub fn avcodec_find_decoder(id: AVCodecID) -> *const AVCodec;
    pub fn avcodec_find_decoder_by_name(name: *const c_char) -> *const AVCodec;
    pub fn avcodec_find_encoder(id: AVCodecID) -> *const AVCodec;
    pub fn avcodec_find_encoder_by_name(name: *const c_char) -> *const AVCodec;
    pub fn avcodec_alloc_context3(codec: *const AVCodec) -> *mut AVCodecContext;
    pub fn avcodec_free_context(avctx: *mut *mut AVCodecContext);
    pub fn avcodec_parameters_to_context(codec: *mut AVCodecContext, par: *const AVCodecParameters) -> c_int;
    pub fn avcodec_parameters_from_context(par: *mut AVCodecParameters, codec: *const AVCodecContext) -> c_int;
    pub fn avcodec_parameters_copy(dst: *mut AVCodecParameters, src: *const AVCodecParameters) -> c_int;
    pub fn avcodec_open2(avctx: *mut AVCodecContext, codec: *const AVCodec, opts: *mut *mut AVDictionary) -> c_int;
    pub fn avcodec_send_packet(avctx: *mut AVCodecContext, avpkt: *const AVPacket) -> c_int;
    pub fn avcodec_receive_frame(avctx: *mut AVCodecContext, frame: *mut AVFrame) -> c_int;
    pub fn avcodec_send_frame(avctx: *mut AVCodecContext, frame: *const AVFrame) -> c_int;
    pub fn avcodec_receive_packet(avctx: *mut AVCodecContext, avpkt: *mut AVPacket) -> c_int;
    pub fn avcodec_flush_buffers(avctx: *mut AVCodecContext);

    // frames
    pub fn av_frame_alloc() -> *mut AVFrame;
    pub fn av_frame_free(frame: *mut *mut AVFrame);
    pub fn av_frame_unref(frame: *mut AVFrame);
    pub fn av_frame_get_buffer(frame: *mut AVFrame, align: c_int) -> c_int;

    // I/O
    pub fn avio_open(s: *mut *mut AVIOContext, url: *const c_char, flags: c_int) -> c_int;
    pub fn avio_closep(s: *mut *mut AVIOContext) -> c_int;

    // options / dictionary
    pub fn av_dict_set(pm: *mut *mut AVDictionary, key: *const c_char, val: *const c_char, flags: c_int) -> c_int;
    pub fn av_dict_free(m: *mut *mut AVDictionary);
    pub fn av_opt_set(obj: *mut c_void, name: *const c_char, val: *const c_char, search_flags: c_int) -> c_int;

    // scaling
    pub fn sws_getContext(srcW: c_int, srcH: c_int, srcFormat: AVPixelFormat,
        dstW: c_int, dstH: c_int, dstFormat: AVPixelFormat,
        flags: c_int, sf: *const c_void, df: *const c_void, p: *const f64) -> *mut SwsContext;
    pub fn sws_scale(c: *mut SwsContext, src: *const *const u8, srcStride: *const c_int,
        srcSliceY: c_int, srcSliceH: c_int, dst: *const *mut u8, dstStride: *const c_int) -> c_int;
    pub fn sws_freeContext(ctx: *mut SwsContext);

    // resampling
    pub fn swr_alloc_set_opts2(ps: *mut *mut SwrContext, out_ch: *const AVChannelLayout,
        out_fmt: AVSampleFormat, out_sr: c_int, in_ch: *const AVChannelLayout,
        in_fmt: AVSampleFormat, in_sr: c_int, log_offset: c_int, log_ctx: *mut c_void) -> c_int;
    pub fn swr_init(s: *mut SwrContext) -> c_int;
    pub fn swr_free(s: *mut *mut SwrContext);
    pub fn swr_convert_frame(swr: *mut SwrContext, output: *mut AVFrame, input: *const AVFrame) -> c_int;

    // misc
    pub fn av_strerror(errnum: c_int, errbuf: *mut c_char, errbuf_size: usize) -> c_int;
    pub fn av_rescale_q(a: i64, bq: AVRational, cq: AVRational) -> i64;
    pub fn av_q2d(q: AVRational) -> f64;
    pub fn av_find_best_stream(ic: *const AVFormatContext, t: AVMediaType, wanted: c_int,
        related: c_int, decoder_ret: *mut *const AVCodec, flags: c_int) -> c_int;
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Convert an FFmpeg error code to a human-readable string.
pub fn av_err2str(errnum: c_int) -> String {
    let mut buf = [0u8; 256];
    unsafe { av_strerror(errnum, buf.as_mut_ptr().cast(), buf.len()); }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}
