//! A tiny Wayland client for looking at uwc by eye: opens a toplevel filled with a solid color
//! (with a lighter 10px frame so edges are visible) and keeps it up.
//!
//!     cargo run --example paint -- "title" 400 300 ff3366
//!     cargo run --example paint -- --bar 32 30a0d0     # a layer-shell bar along the top edge
//!     cargo run --example paint -- --stripes "title" 300 200   # 1 px stripes, re-rendered at the
//!                                                     # fractional scale it is told (a HiDPI client)
//!     cargo run --example paint -- --popup                 # a window with a grabbed popup (a menu);
//!                                                     # prints "popup done" and exits when it is dismissed
//!     cargo run --example paint -- --wallpaper 202040      # a full-screen background layer
//!     cargo run --example paint -- --mover "title" 300 200 ff3366   # a window that asks the compositor to
//!                                                     # move it when a button is pressed on it (CSD title bar)

use std::{io::Write, os::fd::AsFd};

use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface},
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols::xdg::shell::client::{xdg_popup, xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

struct App {
    quit: bool,
    layer_size: Option<(u32, u32)>,
    /// The preferred fractional scale (1/120) when it changed and was not drawn for yet.
    new_scale: Option<u32>,
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.get(1).map(String::as_str) == Some("--stripes") {
        return stripes(
            a.get(2).cloned().unwrap_or_else(|| "stripes".into()),
            a.get(3).and_then(|s| s.parse().ok()).unwrap_or(300),
            a.get(4).and_then(|s| s.parse().ok()).unwrap_or(200),
        );
    }
    if a.get(1).map(String::as_str) == Some("--bar") {
        return bar(
            a.get(2).and_then(|s| s.parse().ok()).unwrap_or(32),
            a.get(3).map(String::as_str).unwrap_or("30a0d0"),
        );
    }
    if a.get(1).map(String::as_str) == Some("--popup") {
        return popup();
    }
    if a.get(1).map(String::as_str) == Some("--wallpaper") {
        return wallpaper(a.get(2).map(String::as_str).unwrap_or("202040"));
    }
    let mover = a.get(1).map(String::as_str) == Some("--mover");
    let a: Vec<String> = if mover { std::iter::once(a[0].clone()).chain(a.into_iter().skip(2)).collect() } else { a };
    let title = a.get(1).cloned().unwrap_or_else(|| "paint".into());
    let w: i32 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(400);
    let h: i32 = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(300);
    let rgb = u32::from_str_radix(a.get(4).map(String::as_str).unwrap_or("3366ff"), 16).unwrap_or(0x3366ff);

    let conn = Connection::connect_to_env().expect("WAYLAND_DISPLAY");
    let (globals, mut queue) = registry_queue_init::<App>(&conn).unwrap();
    let qh = queue.handle();
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=6, ()).unwrap();
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
    let wm: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=6, ()).unwrap();
    let mut app = App { quit: false, layer_size: None, new_scale: None };

    let surface = compositor.create_surface(&qh, ());
    let xdg = wm.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    toplevel.set_title(title);
    toplevel.set_app_id("uwc.paint".into());
    if mover {
        use wayland_client::protocol::wl_seat;
        let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=7, ()).unwrap();
        let _pointer = seat.get_pointer(&qh, ());
        *MOVER.lock().unwrap() = Some((toplevel.clone(), seat));
        std::mem::forget(_pointer);
    }
    surface.commit();
    queue.roundtrip(&mut app).unwrap();

    let (r, g, b) = ((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8);
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let edge = x < 10 || y < 10 || x >= w - 10 || y >= h - 10;
            // A white strip under the top edge, like a title bar: the picture has a top and a bottom.
            if !edge && y < 30 {
                px.extend_from_slice(&[0xee, 0xee, 0xee, 255]);
                continue;
            }
            let k = if edge { 255 } else { 0 };
            px.extend_from_slice(&[b.saturating_add(k / 3), g.saturating_add(k / 3), r.saturating_add(k / 3), 255]);
        }
    }
    let mut f = tempfile_in_runtime();
    f.write_all(&px).unwrap();
    let pool = shm.create_pool(f.as_fd(), w * h * 4, &qh, ());
    let buf = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, &qh, ());
    surface.attach(Some(&buf), 0, 0);
    surface.commit();
    while !app.quit {
        queue.blocking_dispatch(&mut app).unwrap();
    }
}

/// A window of 1-buffer-px black and white vertical stripes that redraws at whatever scale the
/// compositor prefers, with a viewport keeping its logical size: what a HiDPI-aware toolkit does.
fn stripes(title: String, w: i32, h: i32) {
    use wayland_protocols::wp::{
        fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
        viewporter::client::wp_viewporter::WpViewporter,
    };
    let conn = Connection::connect_to_env().expect("WAYLAND_DISPLAY");
    let (globals, mut queue) = registry_queue_init::<App>(&conn).unwrap();
    let qh = queue.handle();
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=6, ()).unwrap();
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
    let wm: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=6, ()).unwrap();
    let fsm: WpFractionalScaleManagerV1 = globals.bind(&qh, 1..=1, ()).unwrap();
    let vp: WpViewporter = globals.bind(&qh, 1..=1, ()).unwrap();
    let mut app = App { quit: false, layer_size: None, new_scale: Some(120) };
    let surface = compositor.create_surface(&qh, ());
    let _fs = fsm.get_fractional_scale(&surface, &qh, ());
    let viewport = vp.get_viewport(&surface, &qh, ());
    let xdg = wm.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    toplevel.set_title(title);
    toplevel.set_app_id("uwc.paint".into());
    surface.commit();
    queue.roundtrip(&mut app).unwrap();
    let mut keep = Vec::new();
    while !app.quit {
        if let Some(s) = app.new_scale.take() {
            let (bw, bh) = ((w as i64 * s as i64 / 120) as i32, (h as i64 * s as i64 / 120) as i32);
            let mut px = Vec::with_capacity((bw * bh * 4) as usize);
            for _ in 0..bh {
                for x in 0..bw {
                    let c = if x % 2 == 0 { 0 } else { 255 };
                    px.extend_from_slice(&[c, c, c, 255]);
                }
            }
            let mut f = tempfile_in_runtime();
            f.write_all(&px).unwrap();
            let pool = shm.create_pool(f.as_fd(), bw * bh * 4, &qh, ());
            let buf = pool.create_buffer(0, bw, bh, bw * 4, wl_shm::Format::Argb8888, &qh, ());
            viewport.set_destination(w, h);
            surface.attach(Some(&buf), 0, 0);
            surface.damage_buffer(0, 0, bw, bh);
            surface.commit();
            keep.push((pool, buf));
        }
        queue.blocking_dispatch(&mut app).unwrap();
    }
}

/// A window with a grabbed popup: what a menu is. When the compositor dismisses the popup (a click
/// outside it), it prints "popup done" and exits, so a script can check the grab works.
fn popup() {
    use wayland_client::protocol::wl_seat;
    use wayland_protocols::xdg::shell::client::xdg_positioner;

    let conn = Connection::connect_to_env().expect("WAYLAND_DISPLAY");
    let (globals, mut queue) = registry_queue_init::<App>(&conn).unwrap();
    let qh = queue.handle();
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=6, ()).unwrap();
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
    let wm: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=6, ()).unwrap();
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=7, ()).unwrap();
    let mut app = App { quit: false, layer_size: None, new_scale: None };

    let surface = compositor.create_surface(&qh, ());
    let xdg = wm.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    toplevel.set_title("P".into());
    toplevel.set_app_id("uwc.paint".into());
    surface.commit();
    queue.roundtrip(&mut app).unwrap();
    let (w, h) = (200, 150);
    let mut f = tempfile_in_runtime();
    f.write_all(&vec![0x33u8; (w * h * 4) as usize]).unwrap();
    let pool = shm.create_pool(f.as_fd(), w * h * 4, &qh, ());
    let buf = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, &qh, ());
    surface.attach(Some(&buf), 0, 0);
    surface.commit();
    queue.roundtrip(&mut app).unwrap();

    // The menu: a grabbed popup at the window's top-left.
    let pos = wm.create_positioner(&qh, ());
    pos.set_size(100, 50);
    pos.set_gravity(xdg_positioner::Gravity::BottomRight);
    let psurface = compositor.create_surface(&qh, ());
    let pxdg = wm.get_xdg_surface(&psurface, &qh, ());
    let popup = pxdg.get_popup(Some(&xdg), &pos, &qh, ());
    popup.grab(&seat, 1);
    psurface.commit();
    queue.roundtrip(&mut app).unwrap();
    let (pw, ph) = (100, 50);
    let mut f = tempfile_in_runtime();
    f.write_all(&vec![0xd0u8; (pw * ph * 4) as usize]).unwrap();
    let pool = shm.create_pool(f.as_fd(), pw * ph * 4, &qh, ());
    let pbuf = pool.create_buffer(0, pw, ph, pw * 4, wl_shm::Format::Argb8888, &qh, ());
    psurface.attach(Some(&pbuf), 0, 0);
    psurface.commit();
    while !app.quit {
        queue.blocking_dispatch(&mut app).unwrap();
    }
}

/// The window that asks to be moved on a button press (`--mover`).
static MOVER: std::sync::Mutex<Option<(xdg_toplevel::XdgToplevel, wayland_client::protocol::wl_seat::WlSeat)>> =
    std::sync::Mutex::new(None);

/// A full-screen layer on the background layer, like a wallpaper program's window.
fn wallpaper(color: &str) {
    let rgb = u32::from_str_radix(color, 16).unwrap_or(0x202040);
    let conn = Connection::connect_to_env().expect("WAYLAND_DISPLAY");
    let (globals, mut queue) = registry_queue_init::<App>(&conn).unwrap();
    let qh = queue.handle();
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=6, ()).unwrap();
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
    let shell: zwlr_layer_shell_v1::ZwlrLayerShellV1 = globals.bind(&qh, 1..=4, ()).unwrap();
    let mut app = App { quit: false, layer_size: None, new_scale: None };
    let surface = compositor.create_surface(&qh, ());
    use zwlr_layer_surface_v1::Anchor;
    let layer =
        shell.get_layer_surface(&surface, None, zwlr_layer_shell_v1::Layer::Background, "wallpaper".into(), &qh, ());
    layer.set_size(0, 0);
    layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
    layer.set_exclusive_zone(-1);
    surface.commit();
    queue.roundtrip(&mut app).unwrap();
    let (w, h) = app.layer_size.take().unwrap_or((800, 600));
    let (w, h) = (w as i32, h as i32);
    let (r, g, b) = ((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8);
    let px: Vec<u8> = (0..w * h).flat_map(|_| [b, g, r, 255]).collect();
    let mut f = tempfile_in_runtime();
    f.write_all(&px).unwrap();
    let pool = shm.create_pool(f.as_fd(), w * h * 4, &qh, ());
    let buf = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, &qh, ());
    surface.attach(Some(&buf), 0, 0);
    surface.commit();
    while !app.quit {
        queue.blocking_dispatch(&mut app).unwrap();
    }
}

fn bar(height: u32, color: &str) {
    let rgb = u32::from_str_radix(color, 16).unwrap_or(0x30a0d0);
    let conn = Connection::connect_to_env().expect("WAYLAND_DISPLAY");
    let (globals, mut queue) = registry_queue_init::<App>(&conn).unwrap();
    let qh = queue.handle();
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=6, ()).unwrap();
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
    let shell: zwlr_layer_shell_v1::ZwlrLayerShellV1 = globals.bind(&qh, 1..=4, ()).unwrap();
    let mut app = App { quit: false, layer_size: None, new_scale: None };
    let surface = compositor.create_surface(&qh, ());
    use zwlr_layer_surface_v1::Anchor;
    let layer = shell.get_layer_surface(&surface, None, zwlr_layer_shell_v1::Layer::Top, "bar".into(), &qh, ());
    layer.set_size(0, height);
    layer.set_anchor(Anchor::Top | Anchor::Left | Anchor::Right);
    layer.set_exclusive_zone(height as i32);
    surface.commit();
    queue.roundtrip(&mut app).unwrap();
    let (w, h) = app.layer_size.take().unwrap_or((800, height));
    let (w, h) = (w as i32, h as i32);
    let (r, g, b) = ((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8);
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..h {
        for x in 0..w {
            // A few "workspace buttons" on the left.
            let button = x < 200 && (x % 40) < 30;
            let k = if button { 60 } else { 0 };
            px.extend_from_slice(&[b.saturating_add(k), g.saturating_add(k), r.saturating_add(k), 255]);
        }
    }
    let mut f = tempfile_in_runtime();
    f.write_all(&px).unwrap();
    let pool = shm.create_pool(f.as_fd(), w * h * 4, &qh, ());
    let buf = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, &qh, ());
    surface.attach(Some(&buf), 0, 0);
    surface.commit();
    while !app.quit {
        queue.blocking_dispatch(&mut app).unwrap();
    }
}

fn tempfile_in_runtime() -> std::fs::File {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let p = dir.join(format!("uwc-paint-{}", std::process::id()));
    let f = std::fs::File::options().read(true).write(true).create(true).truncate(true).open(&p).unwrap();
    let _ = std::fs::remove_file(&p);
    f
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for App {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<xdg_wm_base::XdgWmBase, ()> for App {
    fn event(
        _: &mut Self,
        wm: &xdg_wm_base::XdgWmBase,
        e: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = e {
            wm.pong(serial);
        }
    }
}
impl Dispatch<xdg_surface::XdgSurface, ()> for App {
    fn event(
        _: &mut Self,
        xdg: &xdg_surface::XdgSurface,
        e: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = e {
            xdg.ack_configure(serial);
        }
    }
}
impl Dispatch<xdg_toplevel::XdgToplevel, ()> for App {
    fn event(
        app: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        e: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Close = e {
            app.quit = true;
        }
    }
}
impl Dispatch<xdg_popup::XdgPopup, ()> for App {
    fn event(
        app: &mut Self,
        _: &xdg_popup::XdgPopup,
        e: xdg_popup::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_popup::Event::PopupDone = e {
            println!("popup done");
            std::io::stdout().flush().unwrap();
            app.quit = true;
        }
    }
}
impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for App {
    fn event(
        app: &mut Self,
        l: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        e: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                l.ack_configure(serial);
                app.layer_size = Some((width, height));
            }
            zwlr_layer_surface_v1::Event::Closed => app.quit = true,
            _ => {}
        }
    }
}
impl Dispatch<wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::WpFractionalScaleV1, ()>
    for App
{
    fn event(
        app: &mut Self,
        _: &wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::WpFractionalScaleV1,
        e: wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::Event::PreferredScale {
            scale,
        } = e
        {
            app.new_scale = Some(scale);
        }
    }
}
impl Dispatch<wayland_client::protocol::wl_pointer::WlPointer, ()> for App {
    fn event(
        _: &mut Self,
        _: &wayland_client::protocol::wl_pointer::WlPointer,
        e: wayland_client::protocol::wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_client::{protocol::wl_pointer::Event, WEnum};
        // What a client-side title bar does: ask for a move on a press.
        if let Event::Button {
            serial,
            state: WEnum::Value(wayland_client::protocol::wl_pointer::ButtonState::Pressed),
            ..
        } = e
        {
            if let Some((toplevel, seat)) = MOVER.lock().unwrap().as_ref() {
                toplevel._move(seat, serial);
            }
        }
    }
}
macro_rules! noop {
    ($($t:ty),*) => {$(
        impl Dispatch<$t, ()> for App {
            fn event(_: &mut Self, _: &$t, _: <$t as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
noop!(zwlr_layer_shell_v1::ZwlrLayerShellV1);
noop!(wayland_client::protocol::wl_seat::WlSeat);
noop!(wayland_protocols::xdg::shell::client::xdg_positioner::XdgPositioner);
noop!(wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1);
noop!(wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter);
noop!(wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport);
noop!(wl_compositor::WlCompositor, wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_buffer::WlBuffer, wl_surface::WlSurface);
