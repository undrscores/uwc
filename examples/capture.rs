//! A capture client for checking uwc's capture protocols by hand or from tests/nested.py.
//!
//!     cargo run --example capture -- --title "Firefox" out.ppm   # a window, by wlr-foreign-toplevel title
//!     cargo run --example capture -- --id 3 out.ppm              # a window, by uwc window id
//!     cargo run --example capture -- --title X out.ppm --again   # then a second copy that waits for
//!                                                                # the window to change (like a live preview)
//!
//!     cargo run --example capture -- --output out.ppm            # the whole output (wlr-screencopy)
//!     cargo run --example capture -- --region 10 20 300 200 out.ppm
//!
//!     cargo run --example capture -- --ext-output out.ppm        # the same through ext-image-copy-capture
//!     cargo run --example capture -- --ext-title TITLE out.ppm   # a window, through ext-foreign-toplevel-list
//!
//! Windows go through hyprland-toplevel-export-v1 (what Quickshell's window previews use). The
//! picture is written as a binary PPM. Exit status 2 when the compositor answers `failed`.

use std::{io::Write, os::fd::AsFd};

use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{wl_buffer, wl_registry, wl_shm, wl_shm_pool},
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols::ext::{
    foreign_toplevel_list::v1::client::{
        ext_foreign_toplevel_handle_v1 as ext_handle, ext_foreign_toplevel_list_v1 as ext_list,
    },
    image_capture_source::v1::client::{
        ext_foreign_toplevel_image_capture_source_manager_v1 as ext_tl_sources,
        ext_image_capture_source_v1 as ext_source, ext_output_image_capture_source_manager_v1 as ext_out_sources,
    },
    image_copy_capture::v1::client::{
        ext_image_copy_capture_frame_v1 as ext_frame, ext_image_copy_capture_manager_v1 as ext_copy,
        ext_image_copy_capture_session_v1 as ext_session,
    },
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1 as tl_handle, zwlr_foreign_toplevel_manager_v1 as tl_manager,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1 as sc_frame, zwlr_screencopy_manager_v1 as sc_manager,
};

mod hyprland {
    #![allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables, non_upper_case_globals)]
    #![allow(non_snake_case, unused_imports, missing_docs, clippy::all)]
    use wayland_client;
    use wayland_client::protocol::*;
    use wayland_protocols_wlr::foreign_toplevel::v1::client::*;
    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        use wayland_protocols_wlr::foreign_toplevel::v1::client::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/hyprland-toplevel-export-v1.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("protocols/hyprland-toplevel-export-v1.xml");
}
use hyprland::{hyprland_toplevel_export_frame_v1 as frame_v1, hyprland_toplevel_export_manager_v1 as export_v1};

#[derive(Default)]
struct App {
    handles: Vec<(tl_handle::ZwlrForeignToplevelHandleV1, String)>,
    /// format, width, height, stride from the frame's `buffer` event.
    buffer: Option<(u32, u32, u32, u32)>,
    constraints_done: bool,
    ready: bool,
    failed: bool,
    /// ext-image-copy-capture: the session's buffer size, and whether its constraints are complete.
    ext_size: Option<(u32, u32)>,
    ext_done: bool,
    ext_handles: Vec<(ext_handle::ExtForeignToplevelHandleV1, String)>,
}

/// Write `len` bytes of ARGB8888 (B, G, R, A in memory) from `f` as a binary PPM.
fn write_ppm(f: &mut std::fs::File, w: u32, h: u32, stride: u32, path: &str) {
    use std::io::{Read, Seek};
    let mut px = vec![0u8; (stride * h) as usize];
    f.seek(std::io::SeekFrom::Start(0)).unwrap();
    f.read_exact(&mut px).unwrap();
    let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
    for y in 0..h as usize {
        for x in 0..w as usize {
            let i = y * stride as usize + x * 4;
            out.extend_from_slice(&[px[i + 2], px[i + 1], px[i]]);
        }
    }
    std::fs::write(path, out).unwrap();
}

fn shm_file(len: usize) -> std::fs::File {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let p = dir.join(format!("uwc-capture-{}", std::process::id()));
    let mut f = std::fs::File::options().read(true).write(true).create(true).truncate(true).open(&p).unwrap();
    let _ = std::fs::remove_file(&p);
    f.write_all(&vec![0u8; len]).unwrap();
    f
}

/// The output (or a region of it) through wlr-screencopy.
fn output(region: Option<(i32, i32, i32, i32)>, path: &str) {
    let conn = Connection::connect_to_env().expect("WAYLAND_DISPLAY");
    let (globals, mut queue) = registry_queue_init::<App>(&conn).unwrap();
    let qh = queue.handle();
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
    let out: wayland_client::protocol::wl_output::WlOutput = globals.bind(&qh, 1..=4, ()).unwrap();
    let sc: sc_manager::ZwlrScreencopyManagerV1 = globals.bind(&qh, 1..=3, ()).unwrap();
    let mut app = App::default();
    let frame = match region {
        Some((x, y, w, h)) => sc.capture_output_region(0, &out, x, y, w, h, &qh, ()),
        None => sc.capture_output(0, &out, &qh, ()),
    };
    while !app.constraints_done && !app.failed {
        queue.blocking_dispatch(&mut app).unwrap();
    }
    if app.failed {
        eprintln!("capture: failed");
        std::process::exit(2);
    }
    let (format, w, h, stride) = app.buffer.expect("a buffer event");
    let mut f = shm_file((stride * h) as usize);
    let pool = shm.create_pool(f.as_fd(), (stride * h) as i32, &qh, ());
    let buf =
        pool.create_buffer(0, w as i32, h as i32, stride as i32, wl_shm::Format::try_from(format).unwrap(), &qh, ());
    frame.copy(&buf);
    while !app.ready && !app.failed {
        queue.blocking_dispatch(&mut app).unwrap();
    }
    if app.failed {
        eprintln!("capture: failed");
        std::process::exit(2);
    }
    write_ppm(&mut f, w, h, stride, path);
    println!("{w}x{h}");
}

/// The output or a window through ext-image-copy-capture-v1 (`title` None = the output).
fn ext(title: Option<&str>, path: &str) {
    let conn = Connection::connect_to_env().expect("WAYLAND_DISPLAY");
    let (globals, mut queue) = registry_queue_init::<App>(&conn).unwrap();
    let qh = queue.handle();
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
    let copy: ext_copy::ExtImageCopyCaptureManagerV1 = globals.bind(&qh, 1..=1, ()).unwrap();
    let mut app = App::default();
    let source = match title {
        None => {
            let out: wayland_client::protocol::wl_output::WlOutput = globals.bind(&qh, 1..=4, ()).unwrap();
            let m: ext_out_sources::ExtOutputImageCaptureSourceManagerV1 = globals.bind(&qh, 1..=1, ()).unwrap();
            m.create_source(&out, &qh, ())
        }
        Some(t) => {
            let _list: ext_list::ExtForeignToplevelListV1 = globals.bind(&qh, 1..=1, ()).unwrap();
            let m: ext_tl_sources::ExtForeignToplevelImageCaptureSourceManagerV1 =
                globals.bind(&qh, 1..=1, ()).unwrap();
            queue.roundtrip(&mut app).unwrap();
            queue.roundtrip(&mut app).unwrap();
            let h = app.ext_handles.iter().find(|(_, x)| x == t).map(|(h, _)| h.clone()).expect("no such window");
            m.create_source(&h, &qh, ())
        }
    };
    let session = copy.create_session(&source, ext_copy::Options::empty(), &qh, ());
    while !app.ext_done {
        queue.blocking_dispatch(&mut app).unwrap();
    }
    let (w, h) = app.ext_size.expect("a buffer size");
    let stride = w * 4;
    let mut f = shm_file((stride * h) as usize);
    let pool = shm.create_pool(f.as_fd(), (stride * h) as i32, &qh, ());
    let buf = pool.create_buffer(0, w as i32, h as i32, stride as i32, wl_shm::Format::Argb8888, &qh, ());
    let frame = session.create_frame(&qh, ());
    frame.attach_buffer(&buf);
    frame.damage_buffer(0, 0, w as i32, h as i32);
    frame.capture();
    while !app.ready && !app.failed {
        queue.blocking_dispatch(&mut app).unwrap();
    }
    if app.failed {
        eprintln!("capture: failed");
        std::process::exit(2);
    }
    write_ppm(&mut f, w, h, stride, path);
    println!("{w}x{h}");
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a.get(1).map(String::as_str) {
        Some("--ext-output") if a.len() == 3 => return ext(None, &a[2]),
        Some("--ext-title") if a.len() == 4 => return ext(Some(&a[2]), &a[3]),
        Some("--output") if a.len() == 3 => return output(None, &a[2]),
        Some("--region") if a.len() == 7 => {
            let n = |i: usize| a[i].parse::<i32>().expect("numbers");
            return output(Some((n(2), n(3), n(4), n(5))), &a[6]);
        }
        _ => {}
    }
    let again = a.get(4).map(String::as_str) == Some("--again");
    if !(a.len() == 4 || (a.len() == 5 && again)) || !matches!(a[1].as_str(), "--title" | "--id") {
        eprintln!("usage: capture --title TITLE OUT.ppm [--again] | --id ID OUT.ppm [--again]");
        std::process::exit(1);
    }
    let conn = Connection::connect_to_env().expect("WAYLAND_DISPLAY");
    let (globals, mut queue) = registry_queue_init::<App>(&conn).unwrap();
    let qh = queue.handle();
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
    let mut app = App::default();
    let export: export_v1::HyprlandToplevelExportManagerV1 = globals.bind(&qh, 1..=2, ()).unwrap();
    let handle = if a[1] == "--title" {
        let _m: tl_manager::ZwlrForeignToplevelManagerV1 = globals.bind(&qh, 1..=3, ()).unwrap();
        queue.roundtrip(&mut app).unwrap();
        queue.roundtrip(&mut app).unwrap();
        Some(app.handles.iter().find(|(_, t)| *t == a[2]).map(|(h, _)| h.clone()).expect("no window with that title"))
    } else {
        None
    };
    let new_frame = |app: &mut App| {
        app.ready = false;
        app.constraints_done = false;
        match &handle {
            Some(h) => export.capture_toplevel_with_wlr_toplevel_handle(0, h, &qh, ()),
            None => export.capture_toplevel(0, a[2].parse().expect("an id"), &qh, ()),
        }
    };
    let frame = new_frame(&mut app);
    while !app.constraints_done && !app.failed {
        queue.blocking_dispatch(&mut app).unwrap();
    }
    if app.failed {
        eprintln!("capture: failed");
        std::process::exit(2);
    }
    let (format, w, h, stride) = app.buffer.expect("a buffer event");
    let len = (stride * h) as usize;
    let mut f = shm_file(len);
    let pool = shm.create_pool(f.as_fd(), len as i32, &qh, ());
    let buf =
        pool.create_buffer(0, w as i32, h as i32, stride as i32, wl_shm::Format::try_from(format).unwrap(), &qh, ());
    frame.copy(&buf, 1);
    while !app.ready && !app.failed {
        queue.blocking_dispatch(&mut app).unwrap();
    }
    if app.failed {
        eprintln!("capture: failed");
        std::process::exit(2);
    }
    if again {
        // Without ignore_damage: answered only once the window has new content.
        let t = std::time::Instant::now();
        let frame2 = new_frame(&mut app);
        while !app.constraints_done && !app.failed {
            queue.blocking_dispatch(&mut app).unwrap();
        }
        frame2.copy(&buf, 0);
        while !app.ready && !app.failed {
            queue.blocking_dispatch(&mut app).unwrap();
        }
        println!("again after {} ms", t.elapsed().as_millis());
    }
    write_ppm(&mut f, w, h, stride, &a[3]);
    println!("{w}x{h}");
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
impl Dispatch<tl_manager::ZwlrForeignToplevelManagerV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &tl_manager::ZwlrForeignToplevelManagerV1,
        e: tl_manager::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let tl_manager::Event::Toplevel { toplevel } = e {
            app.handles.push((toplevel, String::new()));
        }
    }
    wayland_client::event_created_child!(App, tl_manager::ZwlrForeignToplevelManagerV1, [
        tl_manager::EVT_TOPLEVEL_OPCODE => (tl_handle::ZwlrForeignToplevelHandleV1, ()),
    ]);
}
impl Dispatch<tl_handle::ZwlrForeignToplevelHandleV1, ()> for App {
    fn event(
        app: &mut Self,
        h: &tl_handle::ZwlrForeignToplevelHandleV1,
        e: tl_handle::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let tl_handle::Event::Title { title } = e {
            if let Some(x) = app.handles.iter_mut().find(|(x, _)| x.id() == h.id()) {
                x.1 = title;
            }
        }
    }
}
impl Dispatch<sc_frame::ZwlrScreencopyFrameV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &sc_frame::ZwlrScreencopyFrameV1,
        e: sc_frame::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            sc_frame::Event::Buffer { format, width, height, stride } => {
                app.buffer = Some((format.into(), width, height, stride));
                app.constraints_done = true;
            }
            sc_frame::Event::BufferDone => app.constraints_done = true,
            sc_frame::Event::Ready { .. } => app.ready = true,
            sc_frame::Event::Failed => app.failed = true,
            _ => {}
        }
    }
}
impl Dispatch<frame_v1::HyprlandToplevelExportFrameV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &frame_v1::HyprlandToplevelExportFrameV1,
        e: frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            frame_v1::Event::Buffer { format, width, height, stride } => {
                app.buffer = Some((format.into(), width, height, stride));
                app.constraints_done = true; // v1 has no buffer_done; one shm buffer event is all uwc sends
            }
            frame_v1::Event::BufferDone => app.constraints_done = true,
            frame_v1::Event::Ready { .. } => app.ready = true,
            frame_v1::Event::Failed => app.failed = true,
            _ => {}
        }
    }
}
macro_rules! noop {
    ($($t:ty),*) => {$(
        impl Dispatch<$t, ()> for App {
            fn event(_: &mut Self, _: &$t, _: <$t as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
impl Dispatch<ext_list::ExtForeignToplevelListV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &ext_list::ExtForeignToplevelListV1,
        e: ext_list::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_list::Event::Toplevel { toplevel } = e {
            app.ext_handles.push((toplevel, String::new()));
        }
    }
    wayland_client::event_created_child!(App, ext_list::ExtForeignToplevelListV1, [
        ext_list::EVT_TOPLEVEL_OPCODE => (ext_handle::ExtForeignToplevelHandleV1, ()),
    ]);
}
impl Dispatch<ext_handle::ExtForeignToplevelHandleV1, ()> for App {
    fn event(
        app: &mut Self,
        h: &ext_handle::ExtForeignToplevelHandleV1,
        e: ext_handle::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_handle::Event::Title { title } = e {
            if let Some(x) = app.ext_handles.iter_mut().find(|(x, _)| x == h) {
                x.1 = title;
            }
        }
    }
}
impl Dispatch<ext_session::ExtImageCopyCaptureSessionV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &ext_session::ExtImageCopyCaptureSessionV1,
        e: ext_session::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            ext_session::Event::BufferSize { width, height } => app.ext_size = Some((width, height)),
            ext_session::Event::Done => app.ext_done = true,
            ext_session::Event::Stopped => app.failed = true,
            _ => {}
        }
    }
}
impl Dispatch<ext_frame::ExtImageCopyCaptureFrameV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &ext_frame::ExtImageCopyCaptureFrameV1,
        e: ext_frame::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            ext_frame::Event::Ready => app.ready = true,
            ext_frame::Event::Failed { .. } => app.failed = true,
            _ => {}
        }
    }
}
noop!(
    ext_copy::ExtImageCopyCaptureManagerV1,
    ext_source::ExtImageCaptureSourceV1,
    ext_out_sources::ExtOutputImageCaptureSourceManagerV1,
    ext_tl_sources::ExtForeignToplevelImageCaptureSourceManagerV1
);
noop!(wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_buffer::WlBuffer, export_v1::HyprlandToplevelExportManagerV1);
noop!(sc_manager::ZwlrScreencopyManagerV1, wayland_client::protocol::wl_output::WlOutput);
