//! PipeWire consumer for the KWin screencast node, negotiating DMA-BUF with explicit
//! modifiers. Every frame is handed to `on_frame` as a `DmabufDesc` while the
//! PipeWire buffer is still dequeued; the callback must call `release` once the GPU
//! has finished reading it.

use std::cell::RefCell;
use std::rc::Rc;

use anyhow::{Context, Result};
use pipewire as pw;
use pw::spa;
use pw::spa::pod::{ChoiceValue, Object, Pod, Property, PropertyFlags, Value};
use pw::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Id, Rectangle, SpaTypes};
use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use spa::param::video::VideoFormat;

use crate::gpu::va::{DmabufDesc, DmabufPlane, drm_fourcc};

pub struct Frame<'a> {
    pub desc: &'a DmabufDesc,
    /// Stable identity of the underlying PipeWire buffer (for import caching).
    pub buffer_key: usize,
    pub pts_ns: i64,
}

pub enum Event<'a> {
    Frame(Frame<'a>),
    /// Buffer is going away; drop any cached import keyed by it.
    BufferRemoved(usize),
    /// Negotiated format changed (size etc.); drop all cached imports.
    FormatChanged { width: u32, height: u32, drm_format: u32, modifier: u64 },
}

pub enum Control {
    Stop,
}

fn spa_to_drm(f: VideoFormat) -> Option<u32> {
    Some(match f {
        VideoFormat::BGRx => drm_fourcc::XRGB8888,
        VideoFormat::BGRA => drm_fourcc::ARGB8888,
        VideoFormat::RGBx => drm_fourcc::XBGR8888,
        VideoFormat::RGBA => drm_fourcc::ABGR8888,
        VideoFormat::xRGB_210LE => drm_fourcc::XRGB2101010,
        VideoFormat::xBGR_210LE => drm_fourcc::XBGR2101010,
        _ => return None,
    })
}

fn serialize(obj: Object) -> Vec<u8> {
    pw::spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
        .expect("pod")
        .0
        .into_inner()
}

fn enum_format(fmt: VideoFormat, modifiers: &[u64], size: (u32, u32), max_fps: u32) -> Vec<u8> {
    let mods: Vec<i64> = modifiers.iter().map(|&m| m as i64).collect();
    serialize(Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: vec![
            Property::new(FormatProperties::MediaType.as_raw(), Value::Id(Id(MediaType::Video.as_raw()))),
            Property::new(FormatProperties::MediaSubtype.as_raw(), Value::Id(Id(MediaSubtype::Raw.as_raw()))),
            Property::new(FormatProperties::VideoFormat.as_raw(), Value::Id(Id(fmt.as_raw()))),
            Property {
                key: FormatProperties::VideoModifier.as_raw(),
                flags: PropertyFlags::MANDATORY | PropertyFlags::DONT_FIXATE,
                value: Value::Choice(ChoiceValue::Long(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Enum { default: mods[0], alternatives: mods.clone() },
                ))),
            },
            Property::new(
                FormatProperties::VideoSize.as_raw(),
                Value::Choice(ChoiceValue::Rectangle(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: Rectangle { width: size.0, height: size.1 },
                        min: Rectangle { width: 1, height: 1 },
                        max: Rectangle { width: 16384, height: 16384 },
                    },
                ))),
            ),
            Property::new(FormatProperties::VideoFramerate.as_raw(), Value::Fraction(Fraction { num: 0, denom: 1 })),
            Property::new(
                FormatProperties::VideoMaxFramerate.as_raw(),
                Value::Choice(ChoiceValue::Fraction(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: Fraction { num: max_fps, denom: 1 },
                        min: Fraction { num: 1, denom: 1 },
                        max: Fraction { num: max_fps, denom: 1 },
                    },
                ))),
            ),
        ],
    })
}

fn buffers_param() -> Vec<u8> {
    serialize(Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: spa::param::ParamType::Buffers.as_raw(),
        properties: vec![
            Property::new(
                spa::sys::SPA_PARAM_BUFFERS_buffers,
                Value::Choice(ChoiceValue::Int(Choice(ChoiceFlags::empty(), ChoiceEnum::Range { default: 4, min: 2, max: 16 }))),
            ),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_dataType, Value::Int(1 << spa::sys::SPA_DATA_DmaBuf)),
        ],
    })
}

struct UserData<F> {
    on_event: F,
    desc: DmabufDesc,
    warned_shm: bool,
}

/// Runs the PipeWire loop on the current thread until `Control::Stop` is received
/// or the stream errors out.
pub fn run<F>(
    node_id: u32,
    formats: &[(u32, Vec<u64>)],
    size: (u32, u32),
    max_fps: u32,
    ctl: pw::channel::Receiver<Control>,
    on_event: F,
) -> Result<()>
where
    F: FnMut(Event<'_>) + 'static,
{
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None).context("connect to PipeWire")?;

    let stream = pw::stream::StreamRc::new(
        core,
        "streetlight-capture",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    let ml = mainloop.clone();
    let _ctl = ctl.attach(mainloop.loop_(), move |c| match c {
        Control::Stop => ml.quit(),
    });

    let data = UserData {
        on_event,
        desc: DmabufDesc { width: size.0, height: size.1, drm_format: 0, modifier: 0, planes: vec![] },
        warned_shm: false,
    };
    let ml_err = mainloop.clone();
    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(move |_, _, old, new| {
            tracing::debug!("pipewire stream {old:?} -> {new:?}");
            if let pw::stream::StreamState::Error(e) = new {
                tracing::error!("pipewire stream error: {e}");
                ml_err.quit();
            }
        })
        .param_changed(|stream, ud, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let mut info = spa::param::video::VideoInfoRaw::default();
            if info.parse(param).is_err() {
                return;
            }
            let Some(drm) = spa_to_drm(info.format()) else {
                tracing::error!("unsupported negotiated format {:?}", info.format());
                return;
            };
            ud.desc.width = info.size().width;
            ud.desc.height = info.size().height;
            ud.desc.drm_format = drm;
            ud.desc.modifier = info.modifier();
            tracing::info!(
                "negotiated {:?} {}x{} modifier {:#x} max {}/{} fps",
                info.format(),
                info.size().width,
                info.size().height,
                info.modifier(),
                info.max_framerate().num,
                info.max_framerate().denom
            );
            (ud.on_event)(Event::FormatChanged {
                width: ud.desc.width,
                height: ud.desc.height,
                drm_format: drm,
                modifier: ud.desc.modifier,
            });
            let b = buffers_param();
            let mut params = [Pod::from_bytes(&b).unwrap()];
            if let Err(e) = stream.update_params(&mut params) {
                tracing::error!("update_params: {e}");
            }
        })
        .remove_buffer(|_, ud, buf| {
            (ud.on_event)(Event::BufferRemoved(unsafe { (*buf).buffer } as usize));
        })
        .process(|stream, ud| unsafe {
            // Drain to the newest buffer; return stale ones immediately.
            let mut newest: *mut pw::sys::pw_buffer = std::ptr::null_mut();
            loop {
                let b = stream.dequeue_raw_buffer();
                if b.is_null() {
                    break;
                }
                if !newest.is_null() {
                    stream.queue_raw_buffer(newest);
                }
                newest = b;
            }
            if newest.is_null() {
                return;
            }
            let sbuf = (*newest).buffer;
            let n = (*sbuf).n_datas as usize;
            let datas = std::slice::from_raw_parts((*sbuf).datas, n);
            if n == 0 || datas[0].type_ != spa::sys::SPA_DATA_DmaBuf {
                if !ud.warned_shm {
                    tracing::warn!("received non-DMA-BUF buffer (type {}); skipping", datas.first().map_or(0, |d| d.type_));
                    ud.warned_shm = true;
                }
                stream.queue_raw_buffer(newest);
                return;
            }
            let chunk = &*datas[0].chunk;
            if chunk.flags & spa::sys::SPA_CHUNK_FLAG_CORRUPTED as i32 != 0 || chunk.size == 0 {
                stream.queue_raw_buffer(newest);
                return;
            }
            ud.desc.planes.clear();
            for d in datas {
                let c = &*d.chunk;
                ud.desc.planes.push(DmabufPlane { fd: d.fd as i32, offset: c.offset, stride: c.stride as u32 });
            }
            let pts_ns = stream.time().map(|t| t.now()).unwrap_or(0);
            (ud.on_event)(Event::Frame(Frame { desc: &ud.desc, buffer_key: sbuf as usize, pts_ns }));
            stream.queue_raw_buffer(newest);
        })
        .register()?;

    let pods: Vec<Vec<u8>> = [VideoFormat::BGRx, VideoFormat::BGRA, VideoFormat::RGBx, VideoFormat::RGBA]
        .into_iter()
        .filter_map(|f| {
            let drm = spa_to_drm(f)?;
            let mods = &formats.iter().find(|(d, _)| *d == drm)?.1;
            Some(enum_format(f, mods, size, max_fps))
        })
        .collect();
    let mut params: Vec<&Pod> = pods.iter().map(|p| Pod::from_bytes(p).unwrap()).collect();
    stream.connect(
        spa::utils::Direction::Input,
        Some(node_id),
        pw::stream::StreamFlags::AUTOCONNECT,
        &mut params,
    )?;
    mainloop.run();
    let _ = stream.disconnect();
    Ok(())
}

#[allow(dead_code)]
fn _keep(_: Rc<RefCell<()>>) {}
