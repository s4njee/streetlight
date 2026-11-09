//! Hardware HEVC/AV1/H.264 decode to DRM-PRIME (dmabuf) frames.
//!
//! * Raspberry Pi 5: FFmpeg's V4L2 request hwaccel (`AV_HWDEVICE_TYPE_DRM`) emits
//!   DRM_PRIME frames directly (NV12 in Broadcom SAND128 column layout).
//! * Desktop AMD/Intel: VA-API decode, then `av_hwframe_map` → DRM_PRIME (zero copy).

use std::ffi::c_int;
use std::ptr;

use anyhow::{Result, bail};
use ffmpeg_sys_next as ff;
use streetlight_client_core::Codec;

pub struct Plane {
    pub fd: i32,
    pub offset: u32,
    pub stride: u32,
}

/// A decoded picture living in GPU memory. Dropping it returns the surface to the decoder.
pub struct DmabufFrame {
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub planes: Vec<Plane>,
    frames: [*mut ff::AVFrame; 2],
}

unsafe impl Send for DmabufFrame {}

impl Drop for DmabufFrame {
    fn drop(&mut self) {
        for f in &mut self.frames {
            if !f.is_null() {
                unsafe { ff::av_frame_free(f) };
            }
        }
    }
}

pub struct Decoder {
    ctx: *mut ff::AVCodecContext,
    pkt: *mut ff::AVPacket,
    hw: HwKind,
    pub description: String,
}

unsafe impl Send for Decoder {}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HwKind {
    Vaapi,
    Drm,
}

const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | (b as u32) << 8 | (c as u32) << 16 | (d as u32) << 24
}
const DRM_FORMAT_NV12: u32 = fourcc(b'N', b'V', b'1', b'2');
const DRM_FORMAT_P010: u32 = fourcc(b'P', b'0', b'1', b'0');
const DRM_FORMAT_R8: u32 = fourcc(b'R', b'8', b' ', b' ');
const DRM_FORMAT_R16: u32 = fourcc(b'R', b'1', b'6', b' ');

unsafe extern "C" fn get_format(ctx: *mut ff::AVCodecContext, fmts: *const ff::AVPixelFormat) -> ff::AVPixelFormat {
    unsafe {
        let want = (*ctx).opaque as usize;
        let mut p = fmts;
        while *p != ff::AVPixelFormat::AV_PIX_FMT_NONE {
            if (want == 1 && *p == ff::AVPixelFormat::AV_PIX_FMT_VAAPI) || (want == 2 && *p == ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME) {
                return *p;
            }
            p = p.add(1);
        }
        tracing::error!("decoder offered no hardware format");
        ff::AVPixelFormat::AV_PIX_FMT_NONE
    }
}

impl Decoder {
    pub fn new(codec: Codec) -> Result<Self> {
        let mut last_err = String::new();
        for hw in [HwKind::Drm, HwKind::Vaapi] {
            match unsafe { Self::try_new(codec, hw) } {
                Ok(d) => return Ok(d),
                Err(e) => last_err = format!("{e:#}"),
            }
        }
        bail!("no hardware decoder: {last_err}")
    }

    unsafe fn try_new(codec: Codec, hw: HwKind) -> Result<Self> {
        unsafe {
            let id = match codec {
                Codec::Hevc => ff::AVCodecID::AV_CODEC_ID_HEVC,
                Codec::Av1 => ff::AVCodecID::AV_CODEC_ID_AV1,
                Codec::H264 => ff::AVCodecID::AV_CODEC_ID_H264,
            };
            let dec = ff::avcodec_find_decoder(id);
            if dec.is_null() {
                bail!("no decoder for {codec:?}");
            }
            let mut dev = ptr::null_mut();
            let (ty, node) = match hw {
                HwKind::Vaapi => (ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI, c"/dev/dri/renderD128"),
                HwKind::Drm => (ff::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM, c"/dev/dri/card0"),
            };
            if hw == HwKind::Drm && !std::path::Path::new("/dev/video19").exists() && !std::path::Path::new("/proc/device-tree/model").exists() {
                bail!("not a V4L2-request (Raspberry Pi) system");
            }
            let r = ff::av_hwdevice_ctx_create(&mut dev, ty, node.as_ptr(), ptr::null_mut(), 0);
            if r < 0 {
                bail!("{hw:?} device: error {r}");
            }
            let ctx = ff::avcodec_alloc_context3(dec);
            (*ctx).hw_device_ctx = dev;
            (*ctx).opaque = match hw {
                HwKind::Vaapi => 1usize,
                HwKind::Drm => 2usize,
            } as *mut _;
            (*ctx).get_format = Some(get_format);
            (*ctx).flags |= ff::AV_CODEC_FLAG_LOW_DELAY as c_int;
            (*ctx).flags2 |= ff::AV_CODEC_FLAG2_FAST as c_int;
            (*ctx).thread_count = 1;
            (*ctx).extra_hw_frames = 4;
            let r = ff::avcodec_open2(ctx, dec, ptr::null_mut());
            if r < 0 {
                let mut c = ctx;
                ff::avcodec_free_context(&mut c);
                bail!("avcodec_open2: {r}");
            }
            let description = format!("{} via {:?}", std::ffi::CStr::from_ptr((*dec).name).to_string_lossy(), hw);
            tracing::info!("decoder: {description}");
            Ok(Self { ctx, pkt: ff::av_packet_alloc(), hw, description })
        }
    }

    /// Decode one access unit. Returns the picture if one is ready.
    pub fn decode(&mut self, au: &[u8]) -> Result<Option<DmabufFrame>> {
        unsafe {
            ff::av_new_packet(self.pkt, au.len() as c_int);
            ptr::copy_nonoverlapping(au.as_ptr(), (*self.pkt).data, au.len());
            let r = ff::avcodec_send_packet(self.ctx, self.pkt);
            ff::av_packet_unref(self.pkt);
            if r < 0 && r != ff::AVERROR(libc::EAGAIN) {
                bail!("send_packet: {r}");
            }
            let mut out = None;
            loop {
                let mut f = ff::av_frame_alloc();
                let r = ff::avcodec_receive_frame(self.ctx, f);
                if r < 0 {
                    ff::av_frame_free(&mut f);
                    break;
                }
                // Keep only the newest picture (never queue latency).
                out = self.to_dmabuf(f).ok();
            }
            Ok(out)
        }
    }

    unsafe fn to_dmabuf(&self, f: *mut ff::AVFrame) -> Result<DmabufFrame> {
        unsafe {
            let (src, mut mapped) = (f, ptr::null_mut::<ff::AVFrame>());
            let drm_frame = if self.hw == HwKind::Vaapi {
                mapped = ff::av_frame_alloc();
                (*mapped).format = ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as c_int;
                let r = ff::av_hwframe_map(mapped, src, (ff::AV_HWFRAME_MAP_READ as c_int) | (ff::AV_HWFRAME_MAP_DIRECT as c_int));
                if r < 0 {
                    let (mut a, mut b) = (src, mapped);
                    ff::av_frame_free(&mut a);
                    ff::av_frame_free(&mut b);
                    bail!("av_hwframe_map: {r}");
                }
                mapped
            } else {
                src
            };
            let desc = &*((*drm_frame).data[0] as *const ff::AVDRMFrameDescriptor);
            let mut planes = Vec::new();
            let mut fourcc = desc.layers[0].format;
            for l in 0..desc.nb_layers as usize {
                let layer = &desc.layers[l];
                for p in 0..layer.nb_planes as usize {
                    let pl = &layer.planes[p];
                    planes.push(Plane { fd: desc.objects[pl.object_index as usize].fd, offset: pl.offset as u32, stride: pl.pitch as u32 });
                }
            }
            // VA-API exports separate R8 + GR88 layers; present them as one NV12/P010 image.
            if desc.nb_layers > 1 {
                fourcc = match desc.layers[0].format {
                    DRM_FORMAT_R8 => DRM_FORMAT_NV12,
                    DRM_FORMAT_R16 => DRM_FORMAT_P010,
                    other => other,
                };
            }
            Ok(DmabufFrame {
                width: (*src).width as u32,
                height: (*src).height as u32,
                fourcc,
                modifier: desc.objects[0].format_modifier,
                planes,
                frames: [mapped, src],
            })
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            ff::av_packet_free(&mut self.pkt);
            ff::avcodec_free_context(&mut self.ctx);
        }
    }
}
