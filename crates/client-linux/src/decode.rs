//! Hardware HEVC/AV1/H.264 decode to DRM-PRIME (dmabuf) frames.
//!
//! * Raspberry Pi 5: FFmpeg's V4L2 request hwaccel (`AV_HWDEVICE_TYPE_DRM`) emits
//!   DRM_PRIME frames directly (NV12 in Broadcom SAND128 column layout).
//! * Desktop AMD/Intel: VA-API decode, then `av_hwframe_map` → DRM_PRIME (zero copy).

use std::ffi::{CString, c_int};
use std::path::Path;
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
        let mut errors = Vec::new();
        for hw in [HwKind::Drm, HwKind::Vaapi] {
            if hw == HwKind::Drm && !crate::drm::has_request_decoder() && std::env::var_os("SL_DRM_DEVICE").is_none() {
                continue;
            }
            for node in crate::drm::decoder_candidates(hw == HwKind::Drm) {
                match unsafe { Self::try_new(codec, hw, &node) } {
                    Ok(d) => return Ok(d),
                    Err(e) => errors.push(format!("{} via {hw:?}: {e:#}", node.display())),
                }
            }
        }
        bail!("no hardware decoder: {}", errors.join("; "))
    }

    unsafe fn try_new(codec: Codec, hw: HwKind, node: &Path) -> Result<Self> {
        unsafe {
            // The generic AV1 lookup can choose libdav1d (software). Select the
            // native decoder, whose get_format callback exposes VA-API/DRM.
            let name = match codec { Codec::Hevc=>c"hevc", Codec::Av1=>c"av1", Codec::H264=>c"h264",
                Codec::Auto=>bail!("decoder requires a negotiated codec") };
            let dec = ff::avcodec_find_decoder_by_name(name.as_ptr());
            if dec.is_null() {
                bail!("no decoder for {codec:?}");
            }
            let mut dev = ptr::null_mut();
            let ty = match hw {
                HwKind::Vaapi => ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                HwKind::Drm => ff::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM,
            };
            let node_name = CString::new(node.as_os_str().as_encoded_bytes())?;
            let r = ff::av_hwdevice_ctx_create(&mut dev, ty, node_name.as_ptr(), ptr::null_mut(), 0);
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
            let description = format!("{} via {:?} ({})", std::ffi::CStr::from_ptr((*dec).name).to_string_lossy(), hw, node.display());
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
                    if r != ff::AVERROR(libc::EAGAIN) && r != ff::AVERROR_EOF { bail!("receive_frame: {r}"); }
                    break;
                }
                // Keep only the newest picture (never queue latency).
                out = Some(self.to_dmabuf(f)?);
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

/// Query the actual VA driver VLD profiles, rather than advertising every FFmpeg codec.
pub fn supported_codecs() -> Vec<Codec> {
    static CODECS: std::sync::OnceLock<Vec<Codec>> = std::sync::OnceLock::new();
    CODECS.get_or_init(|| {
        let mut codecs = Vec::new();
        for node in crate::drm::decoder_candidates(false) {
            if let Ok(found) = va_decode_profiles(&node,false) { for codec in found { if !codecs.contains(&codec) { codecs.push(codec); } } }
        }
        codecs
    }).clone()
}

fn va_decode_profiles(node: &Path,hdr:bool) -> Result<Vec<Codec>> {
    use std::os::fd::AsRawFd;
    unsafe {
        let va = libloading::Library::new("libva.so.2")?;
        let drm = libloading::Library::new("libva-drm.so.2")?;
        let get = drm.get::<unsafe extern "C" fn(i32) -> *mut libc::c_void>(b"vaGetDisplayDRM")?;
        let init = va.get::<unsafe extern "C" fn(*mut libc::c_void,*mut i32,*mut i32)->i32>(b"vaInitialize")?;
        let terminate = va.get::<unsafe extern "C" fn(*mut libc::c_void)->i32>(b"vaTerminate")?;
        let max = va.get::<unsafe extern "C" fn(*mut libc::c_void)->i32>(b"vaMaxNumEntrypoints")?;
        let query = va.get::<unsafe extern "C" fn(*mut libc::c_void,i32,*mut i32,*mut i32)->i32>(b"vaQueryConfigEntrypoints")?;
        let file = std::fs::OpenOptions::new().read(true).write(true).open(node)?;
        let display = get(file.as_raw_fd());
        anyhow::ensure!(!display.is_null() && init(display,&mut 0,&mut 0)==0,"VA initialization failed");
        let mut codecs=Vec::new();
        for (codec,profile) in [(Codec::Av1,32),(Codec::Hevc,if hdr {18} else {17}),(Codec::H264,7)] {
            if hdr && codec!=Codec::Hevc {continue;}
            let name=match codec{Codec::Av1=>c"av1",Codec::Hevc=>c"hevc",Codec::H264=>c"h264",Codec::Auto=>unreachable!()};
            if ff::avcodec_find_decoder_by_name(name.as_ptr()).is_null(){continue;}
            let mut entries=vec![0;max(display).max(0) as usize]; let mut count=0;
            if query(display,profile,entries.as_mut_ptr(),&mut count)==0 && entries.iter().take(count.max(0) as usize).any(|&e| e==1) { codecs.push(codec); }
        }
        terminate(display);
        Ok(codecs)
    }
}

pub fn supports_hdr() -> bool {
    static HDR:std::sync::OnceLock<bool>=std::sync::OnceLock::new();
    *HDR.get_or_init(|| gtk4::minor_version()>=20 && !crate::drm::has_request_decoder()
        && crate::drm::decoder_candidates(false).iter().any(|node|va_decode_profiles(node,true).is_ok_and(|c|c.contains(&Codec::Hevc))))
}
