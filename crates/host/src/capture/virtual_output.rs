//! KWin virtual output / output mirroring via `zkde_screencast_unstable_v1`.
//!
//! KWin creates a real (virtual) monitor of exactly the requested logical size and
//! exposes it as a PipeWire node. The output lives as long as the stream object,
//! so a dedicated thread keeps the Wayland connection dispatching.

use std::sync::mpsc;
use std::thread;

use anyhow::{Context, Result, anyhow, bail};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};

#[allow(non_upper_case_globals, non_camel_case_types, dead_code, clippy::all)]
pub mod protocol {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/zkde-screencast-unstable-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/zkde-screencast-unstable-v1.xml");
}

use protocol::zkde_screencast_stream_unstable_v1::{self as zstream, ZkdeScreencastStreamUnstableV1};
use protocol::zkde_screencast_unstable_v1::{self as zsc, ZkdeScreencastUnstableV1};

#[derive(Debug, Clone, Copy)]
pub enum CursorMode {
    Hidden,
    Embedded,
    Metadata,
}

impl CursorMode {
    fn raw(self) -> u32 {
        match self {
            Self::Hidden => 1,
            Self::Embedded => 2,
            Self::Metadata => 4,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Source {
    /// New virtual monitor. Size is in *logical* pixels; physical = logical × scale.
    Virtual { name: String, width: u32, height: u32, scale: f64 },
    /// Mirror an existing output (first one if `name` is None).
    Output { name: Option<String> },
}

/// Handle to a live screencast. Dropping it closes the stream (and removes the virtual output).
pub struct ScreencastHandle {
    pub node_id: u32,
    stop: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Drop for ScreencastHandle {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[derive(Default)]
struct State {
    node: Option<u32>,
    failed: Option<String>,
    closed: bool,
    outputs: Vec<(wl_output::WlOutput, Option<String>)>,
}

pub fn start(source: Source, cursor: CursorMode) -> Result<ScreencastHandle> {
    let (tx, rx) = mpsc::channel::<Result<u32>>();
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let thread = thread::Builder::new().name("wl-screencast".into()).spawn(move || {
        if let Err(e) = run(source, cursor, &tx, stop_rx) {
            let _ = tx.send(Err(e));
        }
    })?;
    let node_id = rx.recv().context("screencast thread died")??;
    Ok(ScreencastHandle { node_id, stop: Some(stop_tx), thread: Some(thread) })
}

fn run(source: Source, cursor: CursorMode, tx: &mpsc::Sender<Result<u32>>, stop: mpsc::Receiver<()>) -> Result<()> {
    let conn = Connection::connect_to_env().context("connect to Wayland (is WAYLAND_DISPLAY set?)")?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();
    let sc: ZkdeScreencastUnstableV1 = globals
        .bind(&qh, 4..=5, ())
        .map_err(|e| anyhow!("KWin zkde_screencast_unstable_v1 not available: {e}"))?;
    let mut state = State::default();

    let stream = match &source {
        Source::Virtual { name, width, height, scale } => sc.stream_virtual_output_with_description(
            name.clone(),
            format!("Streetlight ({name})"),
            *width as i32,
            *height as i32,
            *scale,
            cursor.raw(),
            &qh,
            (),
        ),
        Source::Output { name } => {
            for g in globals.contents().clone_list() {
                if g.interface == wl_output::WlOutput::interface().name {
                    let o: wl_output::WlOutput = globals.registry().bind(g.name, g.version.min(4), &qh, ());
                    state.outputs.push((o, None));
                }
            }
            queue.roundtrip(&mut state)?;
            let out = state
                .outputs
                .iter()
                .find(|(_, n)| name.is_none() || n.as_deref() == name.as_deref())
                .map(|(o, _)| o.clone())
                .ok_or_else(|| anyhow!("output {name:?} not found"))?;
            sc.stream_output(&out, cursor.raw(), &qh, ())
        }
    };

    while state.node.is_none() && state.failed.is_none() {
        queue.blocking_dispatch(&mut state)?;
    }
    if let Some(err) = state.failed.take() {
        bail!("KWin refused screencast: {err}");
    }
    tx.send(Ok(state.node.unwrap())).ok();

    // Keep the stream alive until the handle is dropped or KWin closes it.
    let fd = conn.backend().poll_fd().as_raw_fd_compat();
    loop {
        match stop.try_recv() {
            Err(mpsc::TryRecvError::Empty) => {}
            _ => break,
        }
        queue.dispatch_pending(&mut state)?;
        conn.flush()?;
        if let Some(guard) = conn.prepare_read() {
            let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
            unsafe { libc::poll(&mut pfd, 1, 100) };
            if pfd.revents & libc::POLLIN != 0 {
                let _ = guard.read();
            }
        }
        if state.closed {
            tracing::warn!("KWin closed the screencast stream");
            break;
        }
    }
    stream.close();
    conn.flush()?;
    Ok(())
}

trait AsRawFdCompat {
    fn as_raw_fd_compat(&self) -> i32;
}
impl AsRawFdCompat for std::os::fd::BorrowedFd<'_> {
    fn as_raw_fd_compat(&self) -> i32 {
        use std::os::fd::AsRawFd;
        self.as_raw_fd()
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(state: &mut Self, o: &wl_output::WlOutput, ev: wl_output::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_output::Event::Name { name } = ev
            && let Some(e) = state.outputs.iter_mut().find(|(x, _)| x == o)
        {
            e.1 = Some(name);
        }
    }
}

impl Dispatch<ZkdeScreencastUnstableV1, ()> for State {
    fn event(_: &mut Self, _: &ZkdeScreencastUnstableV1, _: zsc::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZkdeScreencastStreamUnstableV1, ()> for State {
    fn event(state: &mut Self, _: &ZkdeScreencastStreamUnstableV1, ev: zstream::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            zstream::Event::Created { node } => state.node = Some(node),
            zstream::Event::Failed { error } => state.failed = Some(error),
            zstream::Event::Closed => state.closed = true,
            _ => {}
        }
    }
}

#[allow(dead_code)]
fn _unused(_: WEnum<u32>) {}
