//! Desktop output layout (logical coordinates) via xdg-output.
//!
//! KWin maps absolute-pointer devices across the bounding box of *all* outputs, so a
//! client's position on the streamed display must be rescaled into that box.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1::ZxdgOutputManagerV1, zxdg_output_v1};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Maps 0..=65535 coordinates on the streamed output to 0..=65535 over the whole desktop.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AbsMap {
    pub output: Rect,
    pub desktop: Rect,
}

impl AbsMap {
    pub fn map(&self, x: u16, y: u16) -> (u16, u16) {
        let px = self.output.x as f64 + x as f64 / 65535.0 * (self.output.w - 1).max(0) as f64;
        let py = self.output.y as f64 + y as f64 / 65535.0 * (self.output.h - 1).max(0) as f64;
        let nx = (px - self.desktop.x as f64) / (self.desktop.w - 1).max(1) as f64;
        let ny = (py - self.desktop.y as f64) / (self.desktop.h - 1).max(1) as f64;
        ((nx.clamp(0.0, 1.0) * 65535.0).round() as u16, (ny.clamp(0.0, 1.0) * 65535.0).round() as u16)
    }
}

#[derive(Default)]
struct Out {
    name: Option<String>,
    pos: Option<(i32, i32)>,
    size: Option<(i32, i32)>,
}

#[derive(Default)]
struct State {
    outs: Vec<(wl_output::WlOutput, Out)>,
}

/// Query the layout; returns None if `output_name` doesn't exist (yet).
pub fn query(output_name: &str) -> Result<Option<AbsMap>> {
    let conn = Connection::connect_to_env().context("connect to Wayland")?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();
    let mgr: ZxdgOutputManagerV1 = globals.bind(&qh, 2..=3, ())?;
    let mut state = State::default();
    for g in globals.contents().clone_list() {
        if g.interface == wl_output::WlOutput::interface().name {
            let o: wl_output::WlOutput = globals.registry().bind(g.name, g.version.min(4), &qh, ());
            mgr.get_xdg_output(&o, &qh, state.outs.len());
            state.outs.push((o, Out::default()));
        }
    }
    queue.roundtrip(&mut state)?;
    queue.roundtrip(&mut state)?;
    let rects: Vec<(Option<String>, Rect)> = state
        .outs
        .iter()
        .filter_map(|(_, o)| {
            let ((x, y), (w, h)) = (o.pos?, o.size?);
            Some((o.name.clone(), Rect { x, y, w, h }))
        })
        .collect();
    let Some(output) = rects.iter().find(|(n, _)| n.as_deref() == Some(output_name)).map(|(_, r)| *r) else {
        return Ok(None);
    };
    let x0 = rects.iter().map(|(_, r)| r.x).min().unwrap();
    let y0 = rects.iter().map(|(_, r)| r.y).min().unwrap();
    let x1 = rects.iter().map(|(_, r)| r.x + r.w).max().unwrap();
    let y1 = rects.iter().map(|(_, r)| r.y + r.h).max().unwrap();
    Ok(Some(AbsMap { output, desktop: Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 } }))
}

/// Poll until the output shows up (a new virtual output appears asynchronously).
pub fn wait_for(output_name: &str, timeout: Duration) -> Option<AbsMap> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        match query(output_name) {
            Ok(Some(m)) => return Some(m),
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => {
                tracing::warn!("output layout: {e:#}");
                return None;
            }
        }
    }
    None
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(s: &mut Self, o: &wl_output::WlOutput, ev: wl_output::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_output::Event::Name { name } = ev
            && let Some(e) = s.outs.iter_mut().find(|(x, _)| x == o)
        {
            e.1.name = Some(name);
        }
    }
}

impl Dispatch<ZxdgOutputManagerV1, ()> for State {
    fn event(_: &mut Self, _: &ZxdgOutputManagerV1, _: <ZxdgOutputManagerV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, usize> for State {
    fn event(s: &mut Self, _: &zxdg_output_v1::ZxdgOutputV1, ev: zxdg_output_v1::Event, idx: &usize, _: &Connection, _: &QueueHandle<Self>) {
        let Some((_, o)) = s.outs.get_mut(*idx) else { return };
        match ev {
            zxdg_output_v1::Event::LogicalPosition { x, y } => o.pos = Some((x, y)),
            zxdg_output_v1::Event::LogicalSize { width, height } => o.size = Some((width, height)),
            zxdg_output_v1::Event::Name { name } if o.name.is_none() => o.name = Some(name),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_into_bounding_box() {
        // DP-3 at 0,0 2560x1440 + virtual 1920x1080 at 2560,0 → desktop 4480x1440.
        let m = AbsMap { output: Rect { x: 2560, y: 0, w: 1920, h: 1080 }, desktop: Rect { x: 0, y: 0, w: 4480, h: 1440 } };
        let (x, y) = m.map(0, 0);
        assert_eq!((x, y), ((2560.0 / 4479.0 * 65535.0f64).round() as u16, 0));
        let (x, y) = m.map(65535, 65535);
        assert_eq!(x, 65535);
        assert_eq!(y, (1079.0 / 1439.0 * 65535.0f64).round() as u16);
    }
}
