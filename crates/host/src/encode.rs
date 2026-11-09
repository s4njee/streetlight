//! FFmpeg VA-API encoder fed with VA surfaces produced by our own VPP blit.

use std::ffi::{CString, c_int};
use std::ptr;

use anyhow::{Result, anyhow, bail};
use ffmpeg_sys_next as ff;
use streetlight_proto::control::Codec;

use crate::gpu::va::{self, OutputColor};

#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub ten_bit: bool,
    pub render_node: String,
    /// "CBR" | "VBR" | "QVBR"
    pub rc_mode: String,
}

pub struct Encoder {
    cfg: EncoderConfig,
    device: *mut ff::AVBufferRef,
    frames: *mut ff::AVBufferRef,
    ctx: *mut ff::AVCodecContext,
    pkt: *mut ff::AVPacket,
    pub va: va::Display,
    vpp: va::Vpp,
    pts: i64,
}

fn averr(code: c_int) -> String {
    let mut buf = [0i8; 256];
    unsafe { ff::av_strerror(code, buf.as_mut_ptr(), buf.len()) };
    unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned()
}

fn ck(code: c_int, what: &str) -> Result<c_int> {
    if code < 0 { bail!("{what}: {}", averr(code)) } else { Ok(code) }
}

fn opt(dict: &mut *mut ff::AVDictionary, k: &str, v: &str) {
    let (k, v) = (CString::new(k).unwrap(), CString::new(v).unwrap());
    unsafe { ff::av_dict_set(dict, k.as_ptr(), v.as_ptr(), 0) };
}

impl Encoder {
    pub fn new(cfg: EncoderConfig) -> Result<Self> {
        unsafe {
            let mut device = ptr::null_mut();
            let node = CString::new(cfg.render_node.clone())?;
            ck(
                ff::av_hwdevice_ctx_create(&mut device, ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI, node.as_ptr(), ptr::null_mut(), 0),
                "av_hwdevice_ctx_create(vaapi)",
            )?;
            let hwdev = (*device).data as *mut ff::AVHWDeviceContext;
            let vactx = (*hwdev).hwctx as *mut ff::AVVAAPIDeviceContext;
            let va = va::Display((*vactx).display as va::ffi::VADisplay);

            let frames = ff::av_hwframe_ctx_alloc(device);
            let fc = (*frames).data as *mut ff::AVHWFramesContext;
            (*fc).format = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
            (*fc).sw_format = if cfg.ten_bit { ff::AVPixelFormat::AV_PIX_FMT_P010LE } else { ff::AVPixelFormat::AV_PIX_FMT_NV12 };
            (*fc).width = cfg.width as c_int;
            (*fc).height = cfg.height as c_int;
            (*fc).initial_pool_size = 0;
            ck(ff::av_hwframe_ctx_init(frames), "av_hwframe_ctx_init")?;

            let name = match cfg.codec {
                Codec::Hevc => "hevc_vaapi",
                Codec::Av1 => "av1_vaapi",
                Codec::H264 => "h264_vaapi",
            };
            let cname = CString::new(name)?;
            let codec = ff::avcodec_find_encoder_by_name(cname.as_ptr());
            if codec.is_null() {
                bail!("encoder {name} not found");
            }
            let ctx = ff::avcodec_alloc_context3(codec);
            let c = &mut *ctx;
            c.width = cfg.width as c_int;
            c.height = cfg.height as c_int;
            c.time_base = ff::AVRational { num: 1, den: cfg.fps as c_int };
            c.framerate = ff::AVRational { num: cfg.fps as c_int, den: 1 };
            c.pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
            c.sw_pix_fmt = (*fc).sw_format;
            c.hw_frames_ctx = ff::av_buffer_ref(frames);
            c.hw_device_ctx = ff::av_buffer_ref(device);
            let bps = cfg.bitrate_kbps as i64 * 1000;
            c.bit_rate = bps;
            c.rc_max_rate = bps;
            // ~1 frame of VBV: bounds per-frame size so frames arrive in one frame interval.
            c.rc_buffer_size = (bps / cfg.fps as i64) as c_int;
            c.gop_size = i16::MAX as c_int;
            c.keyint_min = i16::MAX as c_int;
            c.max_b_frames = 0;
            c.refs = 1;
            c.flags |= ff::AV_CODEC_FLAG_LOW_DELAY as c_int | ff::AV_CODEC_FLAG_CLOSED_GOP as c_int;
            if cfg.ten_bit {
                c.color_primaries = ff::AVColorPrimaries::AVCOL_PRI_BT2020;
                c.color_trc = ff::AVColorTransferCharacteristic::AVCOL_TRC_SMPTE2084;
                c.colorspace = ff::AVColorSpace::AVCOL_SPC_BT2020_NCL;
            } else {
                c.color_primaries = ff::AVColorPrimaries::AVCOL_PRI_BT709;
                c.color_trc = ff::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
                c.colorspace = ff::AVColorSpace::AVCOL_SPC_BT709;
            }
            c.color_range = ff::AVColorRange::AVCOL_RANGE_MPEG;
            c.chroma_sample_location = ff::AVChromaLocation::AVCHROMA_LOC_LEFT;
            match cfg.codec {
                Codec::Hevc => c.profile = if cfg.ten_bit { 2 } else { 1 }, // Main10 / Main
                Codec::H264 => c.profile = 100,                             // High
                Codec::Av1 => c.profile = 0,
            }

            let mut dict: *mut ff::AVDictionary = ptr::null_mut();
            opt(&mut dict, "async_depth", "1");
            opt(&mut dict, "rc_mode", &cfg.rc_mode);
            opt(&mut dict, "b_depth", "1");
            if cfg.codec == Codec::Hevc {
                opt(&mut dict, "tier", "main");
            }
            if cfg.codec != Codec::Av1 {
                opt(&mut dict, "aud", "0");
            }
            let r = ff::avcodec_open2(ctx, codec, &mut dict);
            ff::av_dict_free(&mut dict);
            ck(r, &format!("avcodec_open2({name})"))?;

            let vpp = va::Vpp::new(va)?;
            Ok(Self { cfg, device, frames, ctx, pkt: ff::av_packet_alloc(), va, vpp, pts: 0 })
        }
    }

    pub fn config(&self) -> &EncoderConfig {
        &self.cfg
    }

    /// Colour-convert `src` into a fresh encoder surface and encode it.
    /// Returns after the VPP read of `src` has completed (so its buffer may be released),
    /// then delivers the encoded access unit(s) to `sink`.
    pub fn encode_surface(
        &mut self,
        src: va::ffi::VASurfaceID,
        src_size: (u32, u32),
        force_idr: bool,
        release_src: impl FnOnce(),
        mut sink: impl FnMut(&[u8], bool),
    ) -> Result<()> {
        unsafe {
            let mut frame = ff::av_frame_alloc();
            let r = (|| -> Result<()> {
                ck(ff::av_hwframe_get_buffer(self.frames, frame, 0), "av_hwframe_get_buffer")?;
                let dst = (*frame).data[3] as usize as va::ffi::VASurfaceID;
                let color = if self.cfg.ten_bit { OutputColor::Bt2020Limited } else { OutputColor::Bt709Limited };
                // No vaSyncSurface here: the encoder orders after the VPP job on the same
                // display, and the PipeWire buffer is only requeued after encode completes.
                self.vpp.blit(src, src_size, dst, (self.cfg.width, self.cfg.height), color)?;
                Ok(())
            })();
            release_src();
            if let Err(e) = r {
                ff::av_frame_free(&mut frame);
                return Err(e);
            }
            (*frame).pts = self.pts;
            self.pts += 1;
            if force_idr {
                (*frame).pict_type = ff::AVPictureType::AV_PICTURE_TYPE_I;
            }
            let r = ff::avcodec_send_frame(self.ctx, frame);
            ff::av_frame_free(&mut frame);
            ck(r, "avcodec_send_frame")?;
            self.drain(&mut sink)
        }
    }

    fn drain(&mut self, sink: &mut impl FnMut(&[u8], bool)) -> Result<()> {
        unsafe {
            loop {
                let r = ff::avcodec_receive_packet(self.ctx, self.pkt);
                if r == ff::AVERROR(libc::EAGAIN) || r == ff::AVERROR_EOF {
                    return Ok(());
                }
                ck(r, "avcodec_receive_packet")?;
                let p = &*self.pkt;
                let data = std::slice::from_raw_parts(p.data, p.size as usize);
                sink(data, p.flags & ff::AV_PKT_FLAG_KEY != 0);
                ff::av_packet_unref(self.pkt);
            }
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            ff::av_packet_free(&mut self.pkt);
            ff::avcodec_free_context(&mut self.ctx);
            ff::av_buffer_unref(&mut self.frames);
            ff::av_buffer_unref(&mut self.device);
        }
    }
}

#[allow(dead_code)]
fn _assert_send() -> Result<()> {
    Err(anyhow!("unused"))
}
