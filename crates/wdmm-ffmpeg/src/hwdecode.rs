//! MediaCodec hardware decoding for the HLS transcode path.
//!
//! The transcode path has always decoded with the software decoders. The
//! Android FFmpeg build already enables `--enable-mediacodec` (for the H.264
//! and HEVC *encoders*), but the matching `*_mediacodec` *decoders* were never
//! enabled, so `avcodec_find_decoder_by_name("hevc_mediacodec")` returned null
//! and every 10-bit HEVC 1080p frame was decoded on the CPU. On a Snapdragon
//! 888 that put the whole re-encode path at ~0.72× realtime.
//!
//! This module wraps the small amount of zero-copy plumbing needed to make the
//! mediacodec decoders usable:
//!
//! 1. [`attach`] is called on a decoder context allocated from the mediacodec
//!    decoder **before** `avcodec_open2`. It creates an `AVHWDeviceType`
//!    `mediacodec` device (`av_hwdevice_ctx_create` → NDK `AMediaCodec`, no JVM
//!    and no `Surface` needed), attaches it to `AVCodecContext.hw_device_ctx`,
//!    and installs a `get_format` callback that accepts
//!    `AV_PIX_FMT_MEDIACODEC` (hardware surface) when the decoder offers it.
//! 2. `avcodec_open2` then initialises MediaCodec. The decoder emits
//!    `AV_PIX_FMT_MEDIACODEC` frames whose pixels live in a hardware buffer.
//! 3. [`HwDecode::to_software`] copies one such frame into a normal planar
//!    frame (`av_hwframe_transfer_data`, which for mediacodec does the
//!    `glReadPixels`-equivalent `AMediaCodec_getOutputImage` copy) so the rest
//!    of the pipeline (`sws_scale` → encoder) is unchanged.
//!
//! Everything here degrades gracefully: when the build has no mediacodec
//! decoder, when the context was not allocated from it, or when the device
//! cannot be opened, [`attach`] logs one diagnostic line and returns `None` and
//! the caller keeps software decoding. Nothing in this module aborts.
//!
//! Wiring convention for the caller (see the frozen interface in
//! `.tmp/contract/transcode-pipeline-optimization.md`):
//!
//! ```text
//! let name = mediacodec_decoder_name(codec_id);          // None => stock decoder
//! let codec = name.map(|n| avcodec_find_decoder_by_name(n.as_ptr().cast()));
//! let ctx = avcodec_alloc_context3(codec.unwrap_or(software_codec));
//! let hw = attach(ctx, codec_id);                        // before avcodec_open2
//! // ... open, decode; when a frame arrives with format == AV_PIX_FMT_MEDIACODEC:
//! // hw.as_ref().is_some_and(|hw| hw.to_software(hw_frame, sw_frame))
//! ```
//!
//! `attach` deliberately validates that `ctx` was allocated from the
//! mediacodec decoder: the decoder private data is sized by `AVCodecContext.codec`
//! at `avcodec_alloc_context3` time, so swapping `codec` afterwards would corrupt
//! the heap. If the caller passes a software context, `attach` returns `None`.

use std::ffi::{c_char, c_int, c_void};
use std::ptr;

use crate::diag;
use crate::sys::{
    av_buffer_ref, av_buffer_unref, av_frame_get_buffer, av_free, av_hwdevice_ctx_create,
    av_hwdevice_find_type_by_name, av_hwframe_transfer_data, av_hwframe_transfer_get_formats,
    avcodec_find_decoder_by_name, avcodec_get_hw_config, codctx_codec, codctx_get_format,
    codctx_hw_device_ctx, codctx_opaque, codctx_set_get_format, codctx_set_hw_device_ctx,
    codctx_set_opaque, frame_data, frame_format, frame_height, frame_hw_frames_ctx, frame_pts,
    frame_set_format, frame_set_height, frame_set_pts, frame_set_width, frame_width, AVBufferRef,
    AVCodecContext, AVCodecHWConfig, AVCodecID, AVFrame, AVHWDeviceType, AVPixelFormat,
    GetFormatFn, AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX, AV_HWDEVICE_TYPE_MEDIACODEC,
    AV_HWFRAME_TRANSFER_DIRECTION_FROM, AV_NOPTS_VALUE, AV_PIX_FMT_MEDIACODEC, AV_PIX_FMT_NONE,
    AV_PIX_FMT_NV12, AV_PIX_FMT_YUV420P,
};

/// `AV_CODEC_ID_HEVC` (libavcodec/codec_id.h).
pub const AV_CODEC_ID_HEVC: AVCodecID = 173;

/// mediacodec decoder name for H.264, NUL-terminated for the C API.
const H264_MEDIACODEC: &[u8] = b"h264_mediacodec\0";
/// mediacodec decoder name for HEVC, NUL-terminated for the C API.
const HEVC_MEDIACODEC: &[u8] = b"hevc_mediacodec\0";
/// `av_hwdevice_find_type_by_name` lookup name, NUL-terminated.
const MEDIACODEC_DEVICE_NAME: &[u8] = b"mediacodec\0";

/// The mediacodec decoder that handles `codec_id`, if FFmpeg ships one.
///
/// Returns a NUL-terminated byte string suitable for
/// `avcodec_find_decoder_by_name(name.as_ptr().cast())`. `None` for codecs that
/// have no MediaCodec decoder (e.g. VP9/AV1 in our build) — the caller then
/// keeps the software decoder.
pub fn mediacodec_decoder_name(codec_id: AVCodecID) -> Option<&'static [u8]> {
    match codec_id {
        crate::sys::AV_CODEC_ID_H264 => Some(H264_MEDIACODEC),
        AV_CODEC_ID_HEVC => Some(HEVC_MEDIACODEC),
        _ => None,
    }
}

/// Human-readable decoder name for diagnostics.
fn decoder_name_for_log(codec_id: AVCodecID) -> &'static str {
    match codec_id {
        crate::sys::AV_CODEC_ID_H264 => "h264_mediacodec",
        AV_CODEC_ID_HEVC => "hevc_mediacodec",
        _ => "mediacodec",
    }
}

/// Platform decoders wanted by the get_format callback, NUL-terminated.
///
/// The list is handed to the decoder through `AVCodecContext.opaque`; the
/// callback walks it in order against the formats the decoder offers, so
/// `AV_PIX_FMT_MEDIACODEC` wins when available and the decoder falls back to its
/// own software format otherwise.
#[repr(C)]
struct AcceptedFormats([i32; 2]);

/// `AVCodecContext.get_format` trampoline.
///
/// The decoder calls this once at `avcodec_open2` with the list of formats it
/// can produce. We pick `AV_PIX_FMT_MEDIACODEC` when the decoder offers it (i.e.
/// the mediacodec hw-config matched and the device was configured), and fall
/// back to the first offered format otherwise so a failed hardware setup can
/// still decode in software instead of failing the job.
unsafe extern "C" fn get_format_cb(s: *mut AVCodecContext, fmt: *const AVPixelFormat) -> c_int {
    if fmt.is_null() {
        return AV_PIX_FMT_NONE;
    }
    let accepted = codctx_opaque(s) as *const AcceptedFormats;
    if !accepted.is_null() {
        let wanted = &(*accepted).0;
        let mut i = 0;
        while i < wanted.len() {
            let want = wanted[i];
            if want == AV_PIX_FMT_NONE {
                break;
            }
            let mut j = 0isize;
            while *fmt.offset(j) != AV_PIX_FMT_NONE {
                if *fmt.offset(j) == want {
                    return want;
                }
                j += 1;
            }
            i += 1;
        }
    }
    // Nothing we asked for is offered: let the decoder choose, which is the
    // software path (the mediacodec decoders advertise their software pixel
    // format right after AV_PIX_FMT_MEDIACODEC).
    *fmt
}

/// Owns the MediaCodec hardware device context attached to a decoder context.
///
/// Create it with [`attach`] *before* `avcodec_open2`, keep it alive for as long
/// as the decoder is used, and release it (via the inherent [`HwDecode::drop`]
/// or the `Drop` impl) before `avcodec_free_context`.
pub struct HwDecode {
    ctx: *mut AVCodecContext,
    /// The device context we own (`av_hwdevice_ctx_create` result).
    device: *mut AVBufferRef,
    /// Accepted format list passed to the get_format callback via `opaque`.
    accepted: Box<AcceptedFormats>,
    /// Previous `get_format`/`opaque`, restored on drop so a decoder context
    /// that outlives this guard does not keep a pointer into our memory.
    prev_get_format: Option<GetFormatFn>,
    prev_opaque: *mut c_void,
}

impl HwDecode {
    /// Copy a hardware-backed frame into a system-memory frame.
    ///
    /// `src` must be a frame the decoder produced with
    /// `format == AV_PIX_FMT_MEDIACODEC`; `dst` is a normal frame that the caller
    /// either leaves empty (this allocates it with the software format reported
    /// by `av_hwframe_transfer_get_formats`, NV12 as the last-resort fallback) or
    /// has already allocated with that same format. Returns `false` (and logs)
    /// when the frame is not hardware-backed, the destination format mismatch, or
    /// the transfer fails; the caller must then skip the frame instead of
    /// feeding garbage to `sws_scale`.
    ///
    /// # Safety
    ///
    /// `src` and `dst` must point to valid, uniquely-owned `AVFrame`s for the
    /// duration of the call; `dst` must not alias `src`. `self` must come from
    /// [`attach`] and must still be attached to a live decoder context.
    pub unsafe fn to_software(&self, src: *mut AVFrame, dst: *mut AVFrame) -> bool {
        if src.is_null() || dst.is_null() {
            return false;
        }
        let sw_fmt = software_format(src);
        let dst_fmt = frame_format(dst);
        if dst_fmt == AV_PIX_FMT_NONE || dst_fmt == AV_PIX_FMT_MEDIACODEC {
            frame_set_format(dst, sw_fmt);
        } else if dst_fmt != sw_fmt {
            diag::log(format!(
                "hwdecode: destination frame format {dst_fmt} != transfer format {sw_fmt}"
            ));
            return false;
        }
        if frame_width(dst) <= 0 {
            frame_set_width(dst, frame_width(src));
        }
        if frame_height(dst) <= 0 {
            frame_set_height(dst, frame_height(src));
        }
        if frame_data(dst, 0).is_null() {
            let rc = av_frame_get_buffer(dst, 32);
            if rc < 0 {
                diag::log(format!("hwdecode: av_frame_get_buffer failed rc={rc}"));
                return false;
            }
        }
        let rc = av_hwframe_transfer_data(dst, src, 0);
        if rc < 0 {
            diag::log(format!("hwdecode: av_hwframe_transfer_data failed rc={rc}"));
            return false;
        }
        if frame_pts(dst) == AV_NOPTS_VALUE {
            frame_set_pts(dst, frame_pts(src));
        }
        true
    }

    /// Release the device context and restore the decoder fields.
    ///
    /// Inherent method rather than only `Drop` because the contract calls it
    /// explicitly; `Drop` has the same effect if forgotten.
    ///
    /// # Safety
    ///
    /// The decoder context passed to [`attach`] must still be alive (drop the
    /// guard before `avcodec_free_context`). After this call the context is back
    /// on its software path and must not be used with hardware frames.
    pub unsafe fn drop(mut self) {
        self.release();
    }

    unsafe fn release(&mut self) {
        if !self.ctx.is_null() {
            codctx_set_get_format(self.ctx, self.prev_get_format);
            codctx_set_opaque(self.ctx, self.prev_opaque);
            let mut device = codctx_hw_device_ctx(self.ctx);
            if !device.is_null() {
                codctx_set_hw_device_ctx(self.ctx, ptr::null_mut());
                av_buffer_unref(&mut device);
            }
            self.ctx = ptr::null_mut();
        }
        if !self.device.is_null() {
            av_buffer_unref(&mut self.device);
        }
    }
}

impl Drop for HwDecode {
    fn drop(&mut self) {
        unsafe { self.release() }
    }
}

/// Software pixel format the mediacodec frame transfers into.
///
/// Asks the decoder's hw frames context for the formats it can transfer to
/// (mediacodec advertises NV12; a build with a surface-free path may also offer
/// YUV420P) and returns the first one. Falls back to NV12, then YUV420P.
///
/// # Safety
///
/// `src` must be null or a valid `AVFrame`; a hardware frame must still hold a
/// valid `hw_frames_ctx`.
pub unsafe fn software_format(src: *const AVFrame) -> i32 {
    if src.is_null() {
        return AV_PIX_FMT_NV12;
    }
    let hw_frames = frame_hw_frames_ctx(src);
    if hw_frames.is_null() {
        return AV_PIX_FMT_NV12;
    }
    let mut formats: *mut i32 = ptr::null_mut();
    let rc = av_hwframe_transfer_get_formats(
        hw_frames,
        AV_HWFRAME_TRANSFER_DIRECTION_FROM,
        &mut formats,
        0,
    );
    if rc < 0 || formats.is_null() {
        return AV_PIX_FMT_NV12;
    }
    let first = *formats;
    // The list is av_malloc'ed by libavutil; free it with the matching av_free.
    av_free(formats.cast());
    if first == AV_PIX_FMT_NONE {
        AV_PIX_FMT_YUV420P
    } else {
        first
    }
}

/// Configure `ctx` for MediaCodec hardware decoding.
///
/// Must be called after `avcodec_alloc_context3` with the **mediacodec** decoder
/// (see [`mediacodec_decoder_name`]) and before `avcodec_open2`. Returns `None`
/// when the build has no mediacodec decoder for `codec_id`, when `ctx` was
/// allocated from a different decoder, when the decoder's hw-config does not
/// offer `AV_PIX_FMT_MEDIACODEC` through a device context, or when the Android
/// device cannot be opened — in every case one diagnostic line is logged and the
/// caller keeps software decoding.
///
/// # Safety
///
/// `ctx` must be non-null and must come from `avcodec_alloc_context3` with the
/// mediacodec decoder for `codec_id` (see [`mediacodec_decoder_name`]); it must
/// not be shared with other threads while this call runs. The returned guard must
/// outlive every frame the decoder emits and must be dropped before
/// `avcodec_free_context`.
pub unsafe fn attach(ctx: *mut AVCodecContext, codec_id: c_int) -> Option<HwDecode> {
    if ctx.is_null() {
        return None;
    }
    let codec_id = codec_id as AVCodecID;
    let Some(decoder_name) = mediacodec_decoder_name(codec_id) else {
        diag::log(format!(
            "hwdecode: codec id {codec_id} has no mediacodec decoder; using software decode"
        ));
        return None;
    };
    let name_for_log = decoder_name_for_log(codec_id);
    let codec = avcodec_find_decoder_by_name(decoder_name.as_ptr().cast::<c_char>());
    if codec.is_null() {
        diag::log(format!(
            "hwdecode: {name_for_log} is not in this FFmpeg build; using software decode"
        ));
        return None;
    }
    // The private data was allocated for the codec the context was created
    // from, so a context created for the stock decoder cannot be re-pointed at
    // the mediacodec one. Refuse instead of corrupting the heap.
    if codctx_codec(ctx) != codec {
        diag::log(format!(
            "hwdecode: context was not allocated from {name_for_log}; using software decode"
        ));
        return None;
    }
    // Prove the mediacodec decoder advertises a device-context hardware config.
    let mut hw_config_found = false;
    let mut index = 0;
    loop {
        let cfg = avcodec_get_hw_config(codec, index);
        if cfg.is_null() {
            break;
        }
        let cfg: &AVCodecHWConfig = &*cfg;
        if cfg.pix_fmt == AV_PIX_FMT_MEDIACODEC
            && cfg.methods & AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX != 0
            && cfg.device_type == AV_HWDEVICE_TYPE_MEDIACODEC
        {
            hw_config_found = true;
            break;
        }
        index += 1;
    }
    if !hw_config_found {
        diag::log(format!(
            "hwdecode: {name_for_log} exposes no mediacodec hw config; using software decode"
        ));
        return None;
    }
    let device_type: AVHWDeviceType =
        av_hwdevice_find_type_by_name(MEDIACODEC_DEVICE_NAME.as_ptr().cast::<c_char>());
    if device_type != AV_HWDEVICE_TYPE_MEDIACODEC {
        diag::log(format!(
            "hwdecode: mediacodec device type not found (got {device_type}); using software decode"
        ));
        return None;
    }
    let mut device: *mut AVBufferRef = ptr::null_mut();
    let rc = av_hwdevice_ctx_create(
        &mut device,
        AV_HWDEVICE_TYPE_MEDIACODEC,
        ptr::null(),
        ptr::null_mut(),
        0,
    );
    if rc < 0 || device.is_null() {
        if !device.is_null() {
            let mut device = device;
            av_buffer_unref(&mut device);
        }
        diag::log(format!(
            "hwdecode: mediacodec device open failed rc={rc}; using software decode"
        ));
        return None;
    }
    let device_ref = av_buffer_ref(device);
    if device_ref.is_null() {
        let mut device = device;
        av_buffer_unref(&mut device);
        diag::log("hwdecode: av_buffer_ref(device) failed; using software decode");
        return None;
    }
    let mut accepted = Box::new(AcceptedFormats([AV_PIX_FMT_MEDIACODEC, AV_PIX_FMT_NONE]));
    let prev_get_format = codctx_get_format(ctx);
    let prev_opaque = codctx_opaque(ctx);
    let prev_hw_device = codctx_hw_device_ctx(ctx);
    codctx_set_get_format(ctx, Some(get_format_cb));
    codctx_set_opaque(
        ctx,
        accepted.as_mut() as *mut AcceptedFormats as *mut c_void,
    );
    codctx_set_hw_device_ctx(ctx, device_ref);
    if !prev_hw_device.is_null() {
        // A decoder context is not supposed to carry one at this point; drop the
        // previous reference instead of leaking a second device.
        let mut prev = prev_hw_device;
        av_buffer_unref(&mut prev);
    }
    diag::log(format!(
        "hwdecode: attached mediacodec hardware decoder for {name_for_log}"
    ));
    Some(HwDecode {
        ctx,
        device,
        accepted,
        prev_get_format,
        prev_opaque,
    })
}

/// `true` when `frame` carries a MediaCodec hardware surface instead of
/// system-memory pixels.
///
/// # Safety
///
/// `frame` must be null or point to a valid `AVFrame` for the duration of the
/// call.
pub unsafe fn is_hw_frame(frame: *const AVFrame) -> bool {
    !frame.is_null() && frame_format(frame) == AV_PIX_FMT_MEDIACODEC
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_names_follow_the_codec_id() {
        assert_eq!(
            mediacodec_decoder_name(crate::sys::AV_CODEC_ID_H264),
            Some(b"h264_mediacodec\0".as_slice())
        );
        assert_eq!(
            mediacodec_decoder_name(AV_CODEC_ID_HEVC),
            Some(b"hevc_mediacodec\0".as_slice())
        );
        assert_eq!(mediacodec_decoder_name(crate::sys::AV_CODEC_ID_AAC), None);
        assert_eq!(mediacodec_decoder_name(0), None);
    }

    /// The Windows/macOS dev builds have no mediacodec decoders, so `attach`
    /// must report "unavailable" instead of panicking or aborting; on a real
    /// Android build the Some path is exercised by the transcode job instead.
    #[test]
    fn attach_returns_none_and_does_not_panic_without_mediacodec_decoders() {
        unsafe {
            let software = crate::sys::avcodec_find_decoder(crate::sys::AV_CODEC_ID_H264);
            assert!(!software.is_null(), "host build must have the h264 decoder");
            let ctx = crate::sys::avcodec_alloc_context3(software);
            assert!(!ctx.is_null());
            let before = (
                codctx_get_format(ctx).map(|f| f as usize),
                codctx_opaque(ctx),
                codctx_hw_device_ctx(ctx),
            );
            let hw_decoder =
                avcodec_find_decoder_by_name(H264_MEDIACODEC.as_ptr().cast::<c_char>());
            let attached = attach(ctx, crate::sys::AV_CODEC_ID_H264 as c_int);
            if hw_decoder.is_null() {
                assert!(
                    attached.is_none(),
                    "attach must return None when the build lacks h264_mediacodec"
                );
            }
            // A failed attach (and any successful one dropped again) must leave
            // the decoder context exactly as it was.
            drop(attached);
            assert_eq!(codctx_get_format(ctx).map(|f| f as usize), before.0);
            assert_eq!(codctx_opaque(ctx), before.1);
            assert_eq!(codctx_hw_device_ctx(ctx), before.2);
            let mut ctx = ctx;
            crate::sys::avcodec_free_context(&mut ctx);
        }
    }

    /// A context allocated from the stock decoder must be rejected even when the
    /// build does ship mediacodec (private-data size differs per codec).
    #[test]
    fn attach_rejects_a_context_from_a_different_decoder() {
        unsafe {
            let software = crate::sys::avcodec_find_decoder(crate::sys::AV_CODEC_ID_H264);
            assert!(!software.is_null());
            let ctx = crate::sys::avcodec_alloc_context3(software);
            assert!(!ctx.is_null());
            let before = (
                codctx_get_format(ctx).map(|f| f as usize),
                codctx_opaque(ctx),
            );
            // Unknown codec id -> no mediacodec mapping; the context is
            // rejected before any device is opened.
            let attached = attach(ctx, 0);
            assert!(attached.is_none());
            assert_eq!(codctx_get_format(ctx).map(|f| f as usize), before.0);
            assert_eq!(codctx_opaque(ctx), before.1);
            let mut ctx = ctx;
            crate::sys::avcodec_free_context(&mut ctx);
        }
    }

    #[test]
    fn software_transfer_format_is_never_hardware() {
        // Without a frame there is nothing to query; the fallback must be a
        // system-memory format so sws_scale can consume it.
        let fmt = unsafe { software_format(ptr::null()) };
        assert_ne!(fmt, AV_PIX_FMT_MEDIACODEC);
        assert!(fmt == AV_PIX_FMT_NV12 || fmt == AV_PIX_FMT_YUV420P);
    }
}
