//! Query which DRM format modifiers the GPU can import, via EGL on a GBM device.
//! These are offered to KWin so it renders straight into buffers VA-API can read.

use std::ffi::{CString, c_char, c_int, c_void};
use std::os::fd::AsRawFd;

use anyhow::{Context, Result, bail};

pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;
pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

const EGL_PLATFORM_GBM_KHR: u32 = 0x31D7;

unsafe extern "C" {
    fn gbm_create_device(fd: c_int) -> *mut c_void;
    fn gbm_device_destroy(dev: *mut c_void);
    fn eglGetPlatformDisplay(platform: u32, native: *mut c_void, attribs: *const isize) -> *mut c_void;
    fn eglInitialize(dpy: *mut c_void, major: *mut i32, minor: *mut i32) -> u32;
    fn eglTerminate(dpy: *mut c_void) -> u32;
    fn eglGetProcAddress(name: *const c_char) -> *mut c_void;
}

type QueryModifiers = unsafe extern "C" fn(*mut c_void, i32, i32, *mut u64, *mut u32, *mut i32) -> u32;

pub fn query(render_node: &str, drm_formats: &[u32]) -> Result<Vec<(u32, Vec<u64>)>> {
    let file = std::fs::OpenOptions::new().read(true).write(true).open(render_node).with_context(|| format!("open {render_node}"))?;
    unsafe {
        let gbm = gbm_create_device(file.as_raw_fd());
        if gbm.is_null() {
            bail!("gbm_create_device failed");
        }
        let dpy = eglGetPlatformDisplay(EGL_PLATFORM_GBM_KHR, gbm, std::ptr::null());
        let result = (|| {
            if dpy.is_null() || eglInitialize(dpy, std::ptr::null_mut(), std::ptr::null_mut()) == 0 {
                bail!("eglInitialize failed");
            }
            let name = CString::new("eglQueryDmaBufModifiersEXT").unwrap();
            let f = eglGetProcAddress(name.as_ptr());
            if f.is_null() {
                bail!("EGL_EXT_image_dma_buf_import_modifiers missing");
            }
            let q: QueryModifiers = std::mem::transmute(f);
            let mut out = Vec::new();
            for &fmt in drm_formats {
                let mut n = 0;
                q(dpy, fmt as i32, 0, std::ptr::null_mut(), std::ptr::null_mut(), &mut n);
                let mut mods = vec![0u64; n as usize];
                let mut ext = vec![0u32; n as usize];
                q(dpy, fmt as i32, n, mods.as_mut_ptr(), ext.as_mut_ptr(), &mut n);
                // Keep modifiers that are renderable (not external-only).
                let mut m: Vec<u64> = mods.iter().zip(&ext).filter(|(_, e)| **e == 0).map(|(m, _)| *m).collect();
                if m.is_empty() {
                    m.push(DRM_FORMAT_MOD_LINEAR);
                }
                out.push((fmt, m));
            }
            eglTerminate(dpy);
            Ok(out)
        })();
        gbm_device_destroy(gbm);
        result
    }
}
