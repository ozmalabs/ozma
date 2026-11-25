// SPDX-License-Identifier: AGPL-3.0-only
//! Minimal Wayland keyboard probe used by the opt-in live input test.
//!
//! It connects to the compositor selected by the inherited
//! `XDG_RUNTIME_DIR`/`WAYLAND_DISPLAY`, maps one xdg-shell toplevel, and prints
//! `ENTER` plus one `KEY <keycode> <state>` line per key event. It never
//! creates a display, injects input, or talks to the operator's session unless
//! those environment variables point at it.
//!
//! ```text
//! XDG_RUNTIME_DIR=/tmp/... WAYLAND_DISPLAY=ozma-sway-90 \
//!   cargo run -p ozma-multiseat --bin ozma-wayland-key-probe
//! ```

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_keyboard::{self, WlKeyboard};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::protocol::wl_shm::{self, Format, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::xdg::shell::client::xdg_surface::{self, XdgSurface};
use wayland_protocols::xdg::shell::client::xdg_toplevel::{self, XdgToplevel};
use wayland_protocols::xdg::shell::client::xdg_wm_base::{self, XdgWmBase};

#[derive(Default)]
#[allow(dead_code)] // proxies are retained only to keep protocol objects alive
struct Probe {
    keyboard: Option<WlKeyboard>,
    surface: Option<WlSurface>,
    buffer: Option<WlBuffer>,
    pool: Option<WlShmPool>,
    configured: bool,
}

impl Dispatch<WlRegistry, GlobalListContents> for Probe {
    fn event(
        _state: &mut Self,
        _proxy: &WlRegistry,
        _event: <WlRegistry as Proxy>::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for Probe {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities } = event {
            if capabilities
                .into_result()
                .map(|capabilities| capabilities.contains(wl_seat::Capability::Keyboard))
                .unwrap_or(false)
                && state.keyboard.is_none()
            {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
                println!("KEYBOARD");
            }
        }
    }
}

impl Dispatch<WlKeyboard, ()> for Probe {
    fn event(
        _state: &mut Self,
        _proxy: &WlKeyboard,
        event: wl_keyboard::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Enter { .. } => println!("ENTER"),
            wl_keyboard::Event::Leave { .. } => println!("LEAVE"),
            wl_keyboard::Event::Key { key, state, .. } => {
                println!("KEY {key} {:?}", state);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlCompositor, ()> for Probe {
    fn event(
        _state: &mut Self,
        _proxy: &WlCompositor,
        _event: <WlCompositor as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSurface, ()> for Probe {
    fn event(
        _state: &mut Self,
        _proxy: &WlSurface,
        _event: <WlSurface as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<XdgWmBase, ()> for Probe {
    fn event(
        _state: &mut Self,
        proxy: &XdgWmBase,
        event: xdg_wm_base::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            proxy.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, ()> for Probe {
    fn event(
        state: &mut Self,
        proxy: &XdgSurface,
        event: xdg_surface::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            proxy.ack_configure(serial);
            if let Some(surface) = state.surface.as_ref() {
                if let Some(buffer) = state.buffer.as_ref() {
                    surface.attach(Some(buffer), 0, 0);
                    surface.damage(0, 0, 800, 600);
                }
                surface.commit();
            }
            if !state.configured {
                state.configured = true;
                println!("CONFIGURED");
            }
        }
    }
}

impl Dispatch<XdgToplevel, ()> for Probe {
    fn event(
        _state: &mut Self,
        _proxy: &XdgToplevel,
        _event: xdg_toplevel::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlShm, ()> for Probe {
    fn event(
        _state: &mut Self,
        _proxy: &WlShm,
        _event: wl_shm::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlShmPool, ()> for Probe {
    fn event(
        _state: &mut Self,
        _proxy: &WlShmPool,
        _event: <WlShmPool as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlBuffer, ()> for Probe {
    fn event(
        _state: &mut Self,
        _proxy: &WlBuffer,
        _event: <WlBuffer as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

fn shm_file(width: u32, height: u32) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::FileExt;
    let path = std::env::temp_dir().join(format!(
        "ozma-wayland-probe-{}-{}.shm",
        std::process::id(),
        width
    ));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)?;
    file.set_len(u64::from(width) * u64::from(height) * 4)?;
    file.write_at(&[0u8; 4], 0)?;
    let _ = std::fs::remove_file(path);
    Ok(file)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::os::fd::AsFd;

    let connection = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<Probe>(&connection)?;
    let qh = queue.handle();
    let compositor = globals.bind::<WlCompositor, _, _>(&qh, 1..=4, ())?;
    let wm_base = globals.bind::<XdgWmBase, _, _>(&qh, 1..=1, ())?;
    let seat = globals.bind::<WlSeat, _, _>(&qh, 1..=7, ())?;
    let shm = globals.bind::<WlShm, _, _>(&qh, 1..=1, ())?;

    let width = 800;
    let height = 600;
    let shm_file = shm_file(width, height)?;
    let pool = shm.create_pool(
        shm_file.as_fd(),
        i32::try_from(u64::from(width) * u64::from(height) * 4)?,
        &qh,
        (),
    );
    let buffer = pool.create_buffer(
        0,
        i32::try_from(width)?,
        i32::try_from(height)?,
        i32::try_from(width * 4)?,
        Format::Argb8888,
        &qh,
        (),
    );

    let surface = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_title("ozma-input-probe".to_owned());
    toplevel.set_app_id("ozma-input-probe".to_owned());
    // The first xdg-surface commit must not carry a buffer: it requests the
    // initial configure. The configure handler attaches the shm buffer and
    // commits again, which maps the toplevel and gives it keyboard focus.
    surface.commit();

    let mut state = Probe {
        keyboard: None,
        surface: Some(surface),
        buffer: Some(buffer),
        pool: Some(pool),
        configured: false,
    };
    // Force the initial configure/ack/commit cycle before reporting readiness.
    queue.roundtrip(&mut state)?;
    queue.roundtrip(&mut state)?;
    connection.flush()?;
    let _ = seat;
    let _ = shm_file;

    loop {
        queue.blocking_dispatch(&mut state)?;
    }
}
