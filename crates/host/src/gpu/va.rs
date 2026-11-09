//! Thin libva layer: import DMA-BUFs as VA surfaces and run VPP colour conversion.

#![allow(non_upper_case_globals, non_camel_case_types, non_snake_case, dead_code, clippy::all)]

use std::ffi::{CStr, c_void};

use anyhow::{Result, bail};

pub mod ffi {
    #![allow(non_upper_case_globals, non_camel_case_types, non_snake_case, dead_code, unsafe_op_in_unsafe_fn, clippy::all)]
    include!(concat!(env!("OUT_DIR"), "/va.rs"));
}

use ffi::*;

pub fn check(st: VAStatus, what: &str) -> Result<()> {
    if st == VA_STATUS_SUCCESS as VAStatus {
        return Ok(());
    }
    let msg = unsafe { CStr::from_ptr(vaErrorStr(st)) }.to_string_lossy();
    bail!("{what}: {msg} ({st})")
}

pub const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | (b as u32) << 8 | (c as u32) << 16 | (d as u32) << 24
}

pub mod drm_fourcc {
    use super::fourcc;
    pub const XRGB8888: u32 = fourcc(b'X', b'R', b'2', b'4');
    pub const ARGB8888: u32 = fourcc(b'A', b'R', b'2', b'4');
    pub const XBGR8888: u32 = fourcc(b'X', b'B', b'2', b'4');
    pub const ABGR8888: u32 = fourcc(b'A', b'B', b'2', b'4');
    pub const XRGB2101010: u32 = fourcc(b'X', b'R', b'3', b'0');
    pub const XBGR2101010: u32 = fourcc(b'X', b'B', b'3', b'0');
}

/// Map DRM fourcc → (VA fourcc, VA RT format).
fn va_format(drm: u32) -> Option<(u32, u32)> {
    use drm_fourcc::*;
    Some(match drm {
        XRGB8888 => (VA_FOURCC_BGRX, VA_RT_FORMAT_RGB32),
        ARGB8888 => (VA_FOURCC_BGRA, VA_RT_FORMAT_RGB32),
        XBGR8888 => (VA_FOURCC_RGBX, VA_RT_FORMAT_RGB32),
        ABGR8888 => (VA_FOURCC_RGBA, VA_RT_FORMAT_RGB32),
        XRGB2101010 => (VA_FOURCC_X2R10G10B10, VA_RT_FORMAT_RGB32_10),
        XBGR2101010 => (VA_FOURCC_X2B10G10R10, VA_RT_FORMAT_RGB32_10),
        _ => return None,
    })
}

#[derive(Debug, Clone)]
pub struct DmabufPlane {
    pub fd: i32,
    pub offset: u32,
    pub stride: u32,
}

#[derive(Debug, Clone)]
pub struct DmabufDesc {
    pub width: u32,
    pub height: u32,
    pub drm_format: u32,
    pub modifier: u64,
    pub planes: Vec<DmabufPlane>,
}

#[derive(Clone, Copy)]
pub struct Display(pub VADisplay);
unsafe impl Send for Display {}

impl Display {
    /// Import a DMA-BUF as a VA surface (zero copy). The caller owns the surface.
    pub fn import(&self, d: &DmabufDesc) -> Result<VASurfaceID> {
        let Some((va_fourcc, rt)) = va_format(d.drm_format) else {
            bail!("unsupported DRM format {:#x}", d.drm_format);
        };
        let mut desc = VADRMPRIMESurfaceDescriptor { fourcc: va_fourcc, width: d.width, height: d.height, ..Default::default() };
        desc.num_layers = 1;
        desc.layers[0].drm_format = d.drm_format;
        desc.layers[0].num_planes = d.planes.len() as u32;
        let mut objs: Vec<i32> = Vec::new();
        for (i, p) in d.planes.iter().enumerate() {
            let obj = match objs.iter().position(|&f| f == p.fd) {
                Some(o) => o,
                None => {
                    objs.push(p.fd);
                    let o = objs.len() - 1;
                    let size = unsafe { libc::lseek(p.fd, 0, libc::SEEK_END) };
                    desc.objects[o].fd = p.fd;
                    desc.objects[o].size = size.max(0) as u32;
                    desc.objects[o].drm_format_modifier = d.modifier;
                    o
                }
            };
            desc.layers[0].object_index[i] = obj as u32;
            desc.layers[0].offset[i] = p.offset;
            desc.layers[0].pitch[i] = p.stride;
        }
        desc.num_objects = objs.len() as u32;

        let mut attrs = [
            VASurfaceAttrib {
                type_: VASurfaceAttribType_VASurfaceAttribMemoryType,
                flags: VA_SURFACE_ATTRIB_SETTABLE,
                value: VAGenericValue {
                    type_: VAGenericValueType_VAGenericValueTypeInteger,
                    value: _VAGenericValue__bindgen_ty_1 { i: VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2 as i32 },
                },
            },
            VASurfaceAttrib {
                type_: VASurfaceAttribType_VASurfaceAttribExternalBufferDescriptor,
                flags: VA_SURFACE_ATTRIB_SETTABLE,
                value: VAGenericValue {
                    type_: VAGenericValueType_VAGenericValueTypePointer,
                    value: _VAGenericValue__bindgen_ty_1 { p: &mut desc as *mut _ as *mut c_void },
                },
            },
        ];
        let mut surf: VASurfaceID = VA_INVALID_SURFACE;
        check(
            unsafe { vaCreateSurfaces(self.0, rt, d.width, d.height, &mut surf, 1, attrs.as_mut_ptr(), attrs.len() as u32) },
            "vaCreateSurfaces(DRM_PRIME_2)",
        )?;
        Ok(surf)
    }

    pub fn destroy_surface(&self, s: VASurfaceID) {
        let mut s = s;
        unsafe { vaDestroySurfaces(self.0, &mut s, 1) };
    }

    pub fn sync(&self, s: VASurfaceID) -> Result<()> {
        check(unsafe { vaSyncSurface(self.0, s) }, "vaSyncSurface")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputColor {
    /// BT.709 limited range (SDR).
    Bt709Limited,
    /// BT.601 full range for legacy unmanaged YUV presentation.
    Bt601Full,
    /// BT.2020 limited range (HDR10 path; transfer handled by source).
    Bt2020Limited,
}

/// Video post-processing context (RGB → NV12/P010, scaling).
pub struct Vpp {
    dpy: Display,
    config: VAConfigID,
    context: VAContextID,
}

impl Vpp {
    pub fn new(dpy: Display) -> Result<Self> {
        let mut config = 0;
        check(
            unsafe {
                vaCreateConfig(dpy.0, VAProfile_VAProfileNone, VAEntrypoint_VAEntrypointVideoProc, std::ptr::null_mut(), 0, &mut config)
            },
            "vaCreateConfig(VideoProc)",
        )?;
        let mut context = 0;
        check(
            unsafe { vaCreateContext(dpy.0, config, 0, 0, 0, std::ptr::null_mut(), 0, &mut context) },
            "vaCreateContext(VideoProc)",
        )?;
        Ok(Self { dpy, config, context })
    }

    /// Convert/scale `src` into `dst` (whole surfaces). Asynchronous; use `Display::sync`.
    pub fn blit(&self, src: VASurfaceID, src_size: (u32, u32), dst: VASurfaceID, dst_size: (u32, u32), color: OutputColor) -> Result<()> {
        let src_rect = VARectangle { x: 0, y: 0, width: src_size.0 as u16, height: src_size.1 as u16 };
        let dst_rect = VARectangle { x: 0, y: 0, width: dst_size.0 as u16, height: dst_size.1 as u16 };
        let mut p = VAProcPipelineParameterBuffer::default();
        p.surface = src;
        p.surface_region = &src_rect;
        p.output_region = &dst_rect;
        p.output_background_color = 0xff000000;
        p.filter_flags = VA_FILTER_SCALING_FAST;
        p.surface_color_standard = _VAProcColorStandardType_VAProcColorStandardSRGB;
        p.input_color_properties.color_range = VA_SOURCE_RANGE_FULL as u8;
        if color==OutputColor::Bt2020Limited {
            p.surface_color_standard=_VAProcColorStandardType_VAProcColorStandardExplicit;
            p.input_color_properties.colour_primaries=9;
            p.input_color_properties.transfer_characteristics=16;
            p.input_color_properties.matrix_coefficients=0;
        }
        match color {
            OutputColor::Bt709Limited => {
                p.output_color_standard = _VAProcColorStandardType_VAProcColorStandardBT709;
            }
            OutputColor::Bt601Full => {
                // Keep sRGB primaries/TRC; only the YCbCr matrix is BT.601.
                p.output_color_standard = _VAProcColorStandardType_VAProcColorStandardExplicit;
                p.output_color_properties.colour_primaries = 1;
                p.output_color_properties.transfer_characteristics = 13;
                p.output_color_properties.matrix_coefficients = 5;
            }
            OutputColor::Bt2020Limited => {
                p.output_color_standard = _VAProcColorStandardType_VAProcColorStandardExplicit;
                p.output_color_properties.colour_primaries=9;
                p.output_color_properties.transfer_characteristics=16;
                p.output_color_properties.matrix_coefficients=9;
            }
        }
        p.output_color_properties.color_range = if color == OutputColor::Bt601Full { VA_SOURCE_RANGE_FULL } else { VA_SOURCE_RANGE_REDUCED } as u8;
        p.output_color_properties.chroma_sample_location = VA_CHROMA_SITING_HORIZONTAL_LEFT as u8 | VA_CHROMA_SITING_VERTICAL_CENTER as u8;

        let mut buf: VABufferID = 0;
        unsafe {
            check(
                vaCreateBuffer(
                    self.dpy.0,
                    self.context,
                    VABufferType_VAProcPipelineParameterBufferType,
                    std::mem::size_of::<VAProcPipelineParameterBuffer>() as u32,
                    1,
                    &mut p as *mut _ as *mut c_void,
                    &mut buf,
                ),
                "vaCreateBuffer(pipeline)",
            )?;
            let r = (|| {
                check(vaBeginPicture(self.dpy.0, self.context, dst), "vaBeginPicture")?;
                check(vaRenderPicture(self.dpy.0, self.context, &mut buf, 1), "vaRenderPicture")?;
                check(vaEndPicture(self.dpy.0, self.context), "vaEndPicture")
            })();
            vaDestroyBuffer(self.dpy.0, buf);
            r
        }
    }
}

impl Drop for Vpp {
    fn drop(&mut self) {
        unsafe {
            vaDestroyContext(self.dpy.0, self.context);
            vaDestroyConfig(self.dpy.0, self.config);
        }
    }
}
