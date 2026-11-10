//! End-to-end tests: run `uwc --headless`, connect real Wayland clients, drive it over the IPC socket
//! and read its state back, the same way uwm's Python suites do.

use std::{
    io::{BufRead, BufReader, Write},
    os::{fd::AsFd, unix::net::UnixStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use serde_json::Value;
use wayland_client::{
    globals::GlobalList,
    globals::{registry_queue_init, GlobalListContents},
    protocol::{wl_buffer, wl_compositor, wl_output, wl_registry, wl_shm, wl_shm_pool, wl_surface},
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
};
use wayland_protocols::ext::workspace::v1::client::{
    ext_workspace_group_handle_v1 as ws_group, ext_workspace_handle_v1 as ws_handle,
    ext_workspace_manager_v1 as ws_manager,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

// ---- the compositor under test -----------------------------------------------------------

struct Uwc {
    child: Child,
    dir: PathBuf,
    display: String,
    ipc: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Uwc {
    fn start(name: &str, config: Option<&str>) -> Uwc {
        let dir = std::env::temp_dir().join(format!("uwc-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_uwc"));
        cmd.arg("--headless").arg("1000x600").env("XDG_RUNTIME_DIR", &dir).stdout(Stdio::null()).stderr(Stdio::null());
        if let Some(c) = config {
            let f = dir.join("config.kdl");
            std::fs::write(&f, c).unwrap();
            cmd.arg("-c").arg(f);
        } else {
            cmd.env("UWC_CONFIG", dir.join("none.kdl"));
        }
        let child = cmd.spawn().expect("spawn uwc");
        // The sockets appear once the compositor is up.
        let start = Instant::now();
        let (display, ipc_path) = loop {
            let found = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).find_map(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.strip_prefix("uwc-").and_then(|r| r.strip_suffix(".sock")).map(|d| (d.to_string(), e.path()))
            });
            if let Some(f) = found {
                break f;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "uwc did not start");
            std::thread::sleep(Duration::from_millis(20));
        };
        let ipc = loop {
            match UnixStream::connect(&ipc_path) {
                Ok(s) => break s,
                Err(_) => {
                    assert!(start.elapsed() < Duration::from_secs(10), "ipc not accepting");
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        };
        ipc.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let reader = BufReader::new(ipc.try_clone().unwrap());
        Uwc { child, dir, display, ipc, reader }
    }

    fn connect(&self) -> Client {
        Client::new(&self.dir, &self.display)
    }

    fn line(&mut self) -> Value {
        let mut l = String::new();
        self.reader.read_line(&mut l).expect("ipc read");
        serde_json::from_str(&l).unwrap_or_else(|e| panic!("bad json {l:?}: {e}"))
    }

    fn cmd(&mut self, c: &str) -> Value {
        writeln!(self.ipc, "{c}").unwrap();
        self.line()
    }

    fn ok(&mut self, c: &str) {
        let r = self.cmd(c);
        assert_eq!(r, serde_json::json!({"ok": true}), "{c}");
    }

    fn state(&mut self) -> Value {
        self.cmd("state")
    }

    /// Poll the state until `f` accepts it (clients and the compositor run asynchronously).
    fn wait(&mut self, what: &str, f: impl Fn(&Value) -> bool) -> Value {
        let start = Instant::now();
        loop {
            let s = self.state();
            if f(&s) {
                return s;
            }
            assert!(start.elapsed() < Duration::from_secs(5), "timed out waiting for {what}: {s:#}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Uwc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn win<'a>(s: &'a Value, title: &str) -> &'a Value {
    s["windows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["title"] == title)
        .unwrap_or_else(|| panic!("no window {title}: {s:#}"))
}

fn num(v: &Value) -> f64 {
    v.as_f64().unwrap()
}

// ---- a minimal Wayland client ------------------------------------------------------------

struct App {
    compositor: wl_compositor::WlCompositor,
    shm: wl_shm::WlShm,
    wm: xdg_wm_base::XdgWmBase,
    layer_shell: zwlr_layer_shell_v1::ZwlrLayerShellV1,
    /// ext-workspace: the handles in announcement order, and what the compositor said about each.
    ws: Vec<ws_handle::ExtWorkspaceHandleV1>,
    ws_info: std::collections::HashMap<wayland_client::backend::ObjectId, WsInfo>,
    ws_done: usize,
    ws_group_outputs: usize,
    closed: Vec<String>,
    /// The size the compositor last asked a toplevel for.
    asked: Option<(i32, i32)>,
    /// The size the last layer-surface configure gave.
    layer_size: Option<(u32, u32)>,
}

#[derive(Default, Debug, Clone)]
struct WsInfo {
    id: String,
    name: String,
    state: u32,
    coordinates: Vec<u8>,
}

struct Layer {
    surface: wl_surface::WlSurface,
    layer: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    _buf: wl_buffer::WlBuffer,
}

struct Window {
    surface: wl_surface::WlSurface,
    xdg: xdg_surface::XdgSurface,
    toplevel: xdg_toplevel::XdgToplevel,
    _buf: wl_buffer::WlBuffer,
}

struct Client {
    globals: GlobalList,
    conn: Connection,
    queue: EventQueue<App>,
    app: App,
    _dir: PathBuf,
}

impl Client {
    fn new(runtime_dir: &std::path::Path, display: &str) -> Client {
        let sock = UnixStream::connect(runtime_dir.join(display)).expect("wayland socket");
        let conn = Connection::from_socket(sock).unwrap();
        let (globals, mut queue) = registry_queue_init::<App>(&conn).unwrap();
        let qh = queue.handle();
        let mut app = App {
            compositor: globals.bind(&qh, 4..=6, ()).unwrap(),
            shm: globals.bind(&qh, 1..=1, ()).unwrap(),
            wm: globals.bind(&qh, 1..=6, ()).unwrap(),
            layer_shell: globals.bind(&qh, 1..=4, ()).unwrap(),
            ws: vec![],
            ws_info: Default::default(),
            ws_done: 0,
            ws_group_outputs: 0,
            closed: vec![],
            asked: None,
            layer_size: None,
        };
        queue.roundtrip(&mut app).unwrap();
        Client { conn, queue, app, globals, _dir: runtime_dir.to_path_buf() }
    }

    /// A layer surface the way a bar or launcher makes one: set it up, commit, take the configured
    /// size, attach a buffer of that size.
    fn layer(
        &mut self,
        name: &str,
        layer: zwlr_layer_shell_v1::Layer,
        anchor: zwlr_layer_surface_v1::Anchor,
        size: (u32, u32),
        exclusive: i32,
        keyboard: zwlr_layer_surface_v1::KeyboardInteractivity,
    ) -> Layer {
        let qh = self.queue.handle();
        let surface = self.app.compositor.create_surface(&qh, ());
        let l = self.app.layer_shell.get_layer_surface(&surface, None, layer, name.into(), &qh, ());
        l.set_size(size.0, size.1);
        l.set_anchor(anchor);
        l.set_exclusive_zone(exclusive);
        l.set_keyboard_interactivity(keyboard);
        surface.commit();
        self.queue.roundtrip(&mut self.app).unwrap();
        let (w, h) = self.app.layer_size.take().expect("layer surface configured");
        let buf = self.buffer(name, w as i32, h as i32);
        surface.attach(Some(&buf), 0, 0);
        surface.commit();
        self.queue.roundtrip(&mut self.app).unwrap();
        Layer { surface, layer: l, _buf: buf }
    }

    /// Open a toplevel the way a toolkit does: configure, ack, attach a buffer, commit.
    fn window(&mut self, title: &str, w: i32, h: i32) -> Window {
        let qh = self.queue.handle();
        let surface = self.app.compositor.create_surface(&qh, ());
        let xdg = self.app.wm.get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg.get_toplevel(&qh, ());
        toplevel.set_title(title.into());
        toplevel.set_app_id("uwc.test".into());
        surface.commit();
        self.queue.roundtrip(&mut self.app).unwrap(); // initial configure arrives and is acked
        let buf = self.buffer(title, w, h);
        surface.attach(Some(&buf), 0, 0);
        surface.commit();
        self.queue.roundtrip(&mut self.app).unwrap();
        Window { surface, xdg, toplevel, _buf: buf }
    }

    fn buffer(&mut self, name: &str, w: i32, h: i32) -> wl_buffer::WlBuffer {
        let qh = self.queue.handle();
        let stride = w * 4;
        let file = std::fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(self._dir.join(format!("shm-{name}-{w}x{h}")))
            .unwrap();
        file.set_len((stride * h) as u64).unwrap();
        let pool = self.app.shm.create_pool(file.as_fd(), stride * h, &qh, ());
        pool.create_buffer(0, w, h, stride, wl_shm::Format::Argb8888, &qh, ())
    }

    /// Do what a toolkit does after a configure: draw at the size the compositor asked for.
    fn follow_configure(&mut self, name: &str, win: &mut Window) {
        self.sync();
        if let Some((w, h)) = self.app.asked.take() {
            let buf = self.buffer(name, w, h);
            win.surface.attach(Some(&buf), 0, 0);
            win.surface.commit();
            win._buf = buf;
            self.sync();
        }
    }

    fn bind_workspaces(&mut self) -> ws_manager::ExtWorkspaceManagerV1 {
        let qh = self.queue.handle();
        let m = self.globals.bind(&qh, 1..=1, ()).expect("ext_workspace_manager_v1");
        self.sync();
        m
    }

    fn bind_output(&mut self) -> wl_output::WlOutput {
        let qh = self.queue.handle();
        let o = self.globals.bind(&qh, 1..=4, ()).expect("wl_output");
        self.sync();
        o
    }

    /// The workspace named `name` and its state bits.
    fn ws(&self, name: &str) -> (ws_handle::ExtWorkspaceHandleV1, u32) {
        let h = self.app.ws.iter().find(|h| self.app.ws_info[&h.id()].name == name).unwrap().clone();
        let s = self.app.ws_info[&h.id()].state;
        (h, s)
    }

    fn sync(&mut self) {
        self.queue.roundtrip(&mut self.app).unwrap();
    }

    fn dispatch_pending(&mut self) {
        self.conn.flush().unwrap();
        if let Some(g) = self.queue.prepare_read() {
            let _ = g.read();
        }
        self.queue.dispatch_pending(&mut self.app).unwrap();
    }
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
        match e {
            xdg_toplevel::Event::Close => app.closed.push("close".into()),
            xdg_toplevel::Event::Configure { width, height, .. } if width > 0 && height > 0 => {
                app.asked = Some((width, height))
            }
            _ => {}
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
impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for App {
    fn event(
        app: &mut Self,
        l: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        e: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure { serial, width, height } = e {
            l.ack_configure(serial);
            app.layer_size = Some((width, height));
        }
    }
}
noop!(zwlr_layer_shell_v1::ZwlrLayerShellV1);
noop!(wl_output::WlOutput);

impl Dispatch<ws_manager::ExtWorkspaceManagerV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &ws_manager::ExtWorkspaceManagerV1,
        e: ws_manager::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            ws_manager::Event::Workspace { workspace } => {
                app.ws_info.insert(workspace.id(), WsInfo::default());
                app.ws.push(workspace);
            }
            ws_manager::Event::Done => app.ws_done += 1,
            _ => {}
        }
    }

    wayland_client::event_created_child!(App, ws_manager::ExtWorkspaceManagerV1, [
        ws_manager::EVT_WORKSPACE_GROUP_OPCODE => (ws_group::ExtWorkspaceGroupHandleV1, ()),
        ws_manager::EVT_WORKSPACE_OPCODE => (ws_handle::ExtWorkspaceHandleV1, ()),
    ]);
}

impl Dispatch<ws_group::ExtWorkspaceGroupHandleV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &ws_group::ExtWorkspaceGroupHandleV1,
        e: ws_group::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ws_group::Event::OutputEnter { .. } = e {
            app.ws_group_outputs += 1;
        }
    }
}

impl Dispatch<ws_handle::ExtWorkspaceHandleV1, ()> for App {
    fn event(
        app: &mut Self,
        h: &ws_handle::ExtWorkspaceHandleV1,
        e: ws_handle::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let info = app.ws_info.entry(h.id()).or_default();
        match e {
            ws_handle::Event::Id { id } => info.id = id,
            ws_handle::Event::Name { name } => info.name = name,
            ws_handle::Event::Coordinates { coordinates } => info.coordinates = coordinates,
            ws_handle::Event::State { state } => info.state = state.into(),
            _ => {}
        }
    }
}
noop!(wl_compositor::WlCompositor, wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_buffer::WlBuffer, wl_surface::WlSurface);

// ---- tests ---------------------------------------------------------------------------------

#[test]
fn state_shape_and_defaults() {
    let mut uwc = Uwc::start("shape", None);
    let s = uwc.state();
    assert_eq!(s["event"], "state");
    assert_eq!(s["config_error"], Value::Null);
    let m = &s["monitors"][0];
    assert_eq!(m["tags"].as_array().unwrap().len(), 9);
    assert_eq!((num(&m["width"]), num(&m["height"])), (1000.0, 600.0));
    assert_eq!(m["viewport"]["zoom"], 1.0);
    assert_eq!(m["tags"][0]["active"], true);
    assert!(s["windows"].as_array().unwrap().is_empty());
    assert_eq!(s["backend"], "headless");
    assert!(uwc.cmd("bogus").get("error").is_some());
    assert!(uwc.cmd("pan x y").get("error").is_some());
}

#[test]
fn window_maps_centered_and_focused() {
    let mut uwc = Uwc::start("map", None);
    let mut c = uwc.connect();
    let _w = c.window("A", 300, 200);
    let s = uwc.wait("window A", |s| s["windows"].as_array().is_some_and(|w| !w.is_empty()));
    let a = win(&s, "A");
    assert_eq!((num(&a["width"]), num(&a["height"])), (300.0, 200.0));
    // Centred on the 1000x600 view.
    assert_eq!((num(&a["x"]), num(&a["y"])), (350.0, 200.0));
    assert_eq!(a["focused"], true);
    assert_eq!(s["keyboard_focus"]["window"], a["id"], "the seat's keyboard follows window focus");
    assert_eq!(a["app_id"], "uwc.test");
    assert_eq!(a["tags"], 1);
    assert_eq!(s["monitors"][0]["tags"][0]["occupied"], true);
    // Regression: the per-turn render hook once took the backend out of the state and never put it back.
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(uwc.state()["backend"], "headless", "backend lost after event-loop turns");
}

#[test]
fn panning_moves_the_view_not_the_windows() {
    let mut uwc = Uwc::start("pan", None);
    let mut c = uwc.connect();
    let _w = c.window("A", 300, 200);
    let before = uwc.wait("A", |s| !s["windows"].as_array().unwrap().is_empty());
    let a0 = win(&before, "A").clone();
    uwc.ok("pan 100 50");
    let s = uwc.state();
    assert_eq!((num(&s["monitors"][0]["viewport"]["x"]), num(&s["monitors"][0]["viewport"]["y"])), (100.0, 50.0));
    let a1 = win(&s, "A");
    assert_eq!((&a1["x"], &a1["y"]), (&a0["x"], &a0["y"]), "canvas position must not change");
    uwc.ok("panto -20 30");
    assert_eq!(uwc.state()["monitors"][0]["viewport"]["x"], -20.0);
    uwc.ok("zoom 2");
    assert_eq!(uwc.state()["monitors"][0]["viewport"]["zoom"], 2.0);
    // A window's size in canvas px is independent of the zoom.
    assert_eq!(win(&uwc.state(), "A")["width"], 300);
    assert!(uwc.cmd("zoom -1").get("error").is_some());
    assert!(uwc.cmd("zoom nan").get("error").is_some());
    uwc.ok("home");
    assert_eq!(uwc.state()["monitors"][0]["viewport"]["zoom"], 1.0);
}

#[test]
fn move_resize_focus_close() {
    let mut uwc = Uwc::start("wm", None);
    let mut c = uwc.connect();
    let wa = c.window("A", 300, 200);
    let mut wb = c.window("B", 200, 100);
    let s = uwc.wait("two windows", |s| s["windows"].as_array().unwrap().len() == 2);
    let (ia, ib) = (win(&s, "A")["id"].as_u64().unwrap(), win(&s, "B")["id"].as_u64().unwrap());
    assert_eq!(win(&s, "B")["focused"], true, "newest window takes focus");

    uwc.ok(&format!("move {ia} -500 4000"));
    let a = uwc.state();
    assert_eq!((num(&win(&a, "A")["x"]), num(&win(&a, "A")["y"])), (-500.0, 4000.0), "the canvas is unbounded");

    uwc.ok(&format!("focus {ia}"));
    let s = uwc.state();
    assert_eq!(win(&s, "A")["focused"], true);
    assert_eq!(win(&s, "B")["focused"], false);
    // Keyboard focus follows into an off-screen window by panning to it.
    let vp = &s["monitors"][0]["viewport"];
    assert!(num(&vp["y"]) > 3000.0, "view should have moved to the window: {vp}");

    uwc.ok(&format!("resize {ib} 400 250"));
    std::thread::sleep(Duration::from_millis(50));
    c.follow_configure("B", &mut wb);
    uwc.wait("B resized", |s| win(s, "B")["width"] == 400 && win(s, "B")["height"] == 250);
    assert!(uwc.cmd("focus 9999").get("error").is_some());

    uwc.ok(&format!("close {ia}"));
    c.sync();
    c.dispatch_pending();
    assert_eq!(c.app.closed.len(), 1, "client is asked to close");
    wa.toplevel.destroy();
    wa.xdg.destroy();
    wa.surface.destroy();
    c.sync();
    let s = uwc.wait("A gone", |s| s["windows"].as_array().unwrap().len() == 1);
    assert_eq!(win(&s, "B")["focused"], true, "focus falls back to the remaining window");
}

#[test]
fn tags_are_separate_workspaces() {
    let mut uwc = Uwc::start("tags", None);
    let mut c = uwc.connect();
    let _a = c.window("A", 300, 200);
    uwc.wait("A", |s| !s["windows"].as_array().unwrap().is_empty());
    uwc.ok("pan 700 0"); // tag 1's view moves

    uwc.ok("view 2");
    let s = uwc.state();
    assert_eq!(s["monitors"][0]["tags"][1]["active"], true);
    assert_eq!(s["monitors"][0]["tags"][0]["active"], false);
    // Tag 2 has its own viewport, still at the origin.
    assert_eq!(s["monitors"][0]["viewport"]["x"], 0.0);
    let _b = c.window("B", 100, 100);
    let s = uwc.wait("B", |s| s["windows"].as_array().unwrap().len() == 2);
    assert_eq!(win(&s, "B")["tags"], 2, "a new window opens on the visible tag");
    assert_eq!(win(&s, "A")["tags"], 1);

    uwc.ok("view 1");
    let s = uwc.state();
    assert_eq!(s["monitors"][0]["viewport"]["x"], 700.0, "tag 1 keeps its own viewport");
    assert_eq!(win(&s, "A")["focused"], true);

    uwc.ok("toggleview 2");
    let s = uwc.state();
    assert!(s["monitors"][0]["tags"][0]["active"] == true && s["monitors"][0]["tags"][1]["active"] == true);
    assert!(uwc.cmd("view 10").get("error").is_some());
    assert!(uwc.cmd("view 0").get("error").is_some());
}

#[test]
fn subscribers_see_changes_coalesced() {
    let mut uwc = Uwc::start("sub", None);
    let mut sub = UnixStream::connect(uwc.dir.join(format!("uwc-{}.sock", uwc.display))).unwrap();
    sub.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    writeln!(sub, "subscribe").unwrap();
    let mut r = BufReader::new(sub.try_clone().unwrap());
    let mut l = String::new();
    r.read_line(&mut l).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&l).unwrap()["event"], "state");

    for _ in 0..50 {
        writeln!(uwc.ipc, "pan 1 0").unwrap();
    }
    std::thread::sleep(Duration::from_millis(300));
    // Drain the 50 replies so the connection stays healthy.
    for _ in 0..50 {
        uwc.line();
    }
    sub.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    let mut events = 0;
    loop {
        l.clear();
        match r.read_line(&mut l) {
            Ok(n) if n > 0 => events += 1,
            _ => break,
        }
    }
    assert!((1..50).contains(&events), "{events} events for 50 pans");
}

#[test]
fn config_error_is_reported_and_old_config_kept() {
    let dir_cfg = "options { border-width 5 }";
    let mut uwc = Uwc::start("cfg", Some(dir_cfg));
    let got = uwc.cmd("get border-width");
    assert_eq!(got["value"], 5);
    std::fs::write(uwc.dir.join("config.kdl"), "options { nonsense 1 }").unwrap();
    let r = uwc.cmd("reload");
    assert!(r["error"].as_str().unwrap().contains("nonsense"), "{r}");
    let s = uwc.state();
    assert!(s["config_error"].as_str().unwrap().contains("nonsense"));
    assert_eq!(uwc.cmd("get border-width")["value"], 5, "the old config stays");
    std::fs::write(uwc.dir.join("config.kdl"), "options { border-width 7 }").unwrap();
    uwc.ok("reload");
    assert_eq!(uwc.cmd("get border-width")["value"], 7);
    let s = uwc.state();
    assert_eq!(s["config_error"], Value::Null);
    assert!(num(&s["config_rev"]) >= 2.0);
}

#[test]
fn overview_fits_all_windows_and_toggles_back() {
    let mut uwc = Uwc::start("overview", None);
    let mut c = uwc.connect();
    let _a = c.window("A", 300, 200);
    let _b = c.window("B", 300, 200);
    let s = uwc.wait("two", |s| s["windows"].as_array().unwrap().len() == 2);
    let ib = win(&s, "B")["id"].as_u64().unwrap();
    uwc.ok(&format!("move {ib} 3000 2000"));
    uwc.ok("overview");
    let z = num(&uwc.state()["monitors"][0]["viewport"]["zoom"]);
    assert!(z < 0.5, "zoomed out to fit both windows: {z}");
    uwc.ok("overview");
    assert_eq!(uwc.state()["monitors"][0]["viewport"]["zoom"], 1.0);
}

#[test]
fn quit_ends_the_compositor() {
    let mut uwc = Uwc::start("quit", None);
    uwc.ok("quit");
    let start = Instant::now();
    while uwc.child.try_wait().unwrap().is_none() {
        assert!(start.elapsed() < Duration::from_secs(5), "did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn bars_reserve_space_and_windows_open_in_the_rest() {
    use zwlr_layer_surface_v1::{Anchor, KeyboardInteractivity as K};
    let mut uwc = Uwc::start("bar", None);
    let mut c = uwc.connect();
    // A 30 px bar along the top edge, full width (width 0 = stretch between the anchors).
    let _bar = c.layer(
        "bar",
        zwlr_layer_shell_v1::Layer::Top,
        Anchor::Top | Anchor::Left | Anchor::Right,
        (0, 30),
        30,
        K::None,
    );
    let s = uwc.wait("bar", |s| !s["monitors"][0]["layers"].as_array().unwrap().is_empty());
    let m = &s["monitors"][0];
    let bar = &m["layers"][0];
    assert_eq!((&bar["namespace"], &bar["layer"]), (&"bar".into(), &"top".into()));
    assert_eq!((num(&bar["x"]), num(&bar["y"]), num(&bar["width"]), num(&bar["height"])), (0.0, 0.0, 1000.0, 30.0));
    let wa = &m["work_area"];
    assert_eq!((num(&wa["y"]), num(&wa["height"])), (30.0, 570.0), "the bar's exclusive zone is reserved");

    // A window opens centred in what is left: y = 30 + (570 - 200) / 2.
    let _w = c.window("A", 300, 200);
    let s = uwc.wait("A", |s| !s["windows"].as_array().unwrap().is_empty());
    let a = win(&s, "A");
    assert_eq!((num(&a["x"]), num(&a["y"])), (350.0, 215.0));
    // The bar is screen-fixed: panning and zooming leave it alone.
    uwc.ok("pan 500 500");
    uwc.ok("zoom 0.5");
    let bar = &uwc.state()["monitors"][0]["layers"][0];
    assert_eq!((num(&bar["x"]), num(&bar["y"]), num(&bar["width"])), (0.0, 0.0, 1000.0));
}

#[test]
fn an_exclusive_launcher_takes_the_keyboard_and_gives_it_back() {
    use zwlr_layer_surface_v1::{Anchor, KeyboardInteractivity as K};
    let mut uwc = Uwc::start("launcher", None);
    let mut c = uwc.connect();
    let _w = c.window("A", 300, 200);
    uwc.wait("A", |s| win(s, "A")["focused"] == true);

    let l = c.layer("launcher", zwlr_layer_shell_v1::Layer::Overlay, Anchor::empty(), (400, 300), 0, K::Exclusive);
    let s = uwc.wait("launcher has the keyboard", |s| {
        s["monitors"][0]["layers"].as_array().unwrap().iter().any(|l| l["keyboard"] == true)
    });
    assert_eq!(s["keyboard_focus"]["layer"], "launcher");
    let la = &s["monitors"][0]["layers"][0];
    // Centred when not anchored.
    assert_eq!((num(&la["x"]), num(&la["y"])), (300.0, 150.0));
    assert!(uwc.cmd("focus 1").get("ok").is_some());
    let s = uwc.state();
    assert_eq!(
        s["monitors"][0]["layers"][0]["keyboard"], true,
        "window focus does not take it from an exclusive layer"
    );
    assert_eq!(s["keyboard_focus"]["layer"], "launcher");

    l.layer.destroy();
    l.surface.destroy();
    c.sync();
    let s = uwc.wait("launcher gone", |s| s["monitors"][0]["layers"].as_array().unwrap().is_empty());
    assert_eq!(win(&s, "A")["focused"], true);
    assert_eq!(s["keyboard_focus"]["window"], win(&s, "A")["id"], "the window gets the seat's keyboard back");
}

#[test]
fn set_changes_options_live_and_rejects_bad_values() {
    let mut uwc = Uwc::start("set", Some("vars { accent \"#7aa2f7\" }\noptions { corner-radius 12 }"));
    let rev = num(&uwc.state()["config_rev"]);
    uwc.ok("set focus-color \"$accent\"");
    assert_eq!(uwc.cmd("get focus-color")["value"], "#7aa2f7ff", "vars from the file work in set");
    uwc.ok("set focus-gradient \"#ff0000\" \"#00ff00\" \"#0000ff\" angle=30");
    let g = uwc.cmd("get focus-gradient")["value"].clone();
    assert_eq!(g["angle"], 30.0);
    assert_eq!(g["colors"].as_array().unwrap().len(), 3);
    uwc.ok("set inactive-opacity 0.8");
    assert_eq!(uwc.cmd("get inactive-opacity")["value"], 0.8);
    assert!(num(&uwc.state()["config_rev"]) > rev, "config_rev tells shells to get again");

    for bad in [
        "set corner-radius -3",
        "set opacity 2",
        "set nonsense 1",
        "set mod-key \"Alt\"",
        "set",
        "set focus-gradient \"#fff\"",
    ] {
        assert!(uwc.cmd(bad).get("error").is_some(), "should refuse: {bad}");
    }
    assert_eq!(uwc.cmd("get corner-radius")["value"], 12.0, "a refused set changes nothing");
    // `set` lasts until the next reload, like in uwm.
    uwc.ok("reload");
    assert_eq!(uwc.cmd("get inactive-opacity")["value"], 1.0);
    assert_eq!(uwc.cmd("get focus-gradient")["value"], Value::Null);
}

#[test]
fn bars_see_tags_as_workspaces_and_can_switch_them() {
    const ACTIVE: u32 = 1;
    let mut uwc = Uwc::start("workspaces", None);
    let mut c = uwc.connect();
    let m = c.bind_workspaces();
    assert_eq!(c.app.ws.len(), 9);
    let names: Vec<String> = c.app.ws.iter().map(|h| c.app.ws_info[&h.id()].name.clone()).collect();
    assert_eq!(names, (1..=9).map(|i| i.to_string()).collect::<Vec<_>>());
    let three = c.app.ws_info[&c.ws("3").0.id()].clone();
    assert_eq!(three.id, "uwc-tag-3");
    assert_eq!(three.coordinates, 3u32.to_ne_bytes().to_vec());
    assert_eq!((c.ws("1").1, c.ws("2").1), (ACTIVE, 0));
    assert!(c.app.ws_done >= 1);

    // A click on workspace 3: activate + commit views it alone.
    c.ws("3").0.activate();
    m.commit();
    c.sync();
    let s = uwc.wait("tag 3 viewed", |s| s["monitors"][0]["tags"][2]["active"] == true);
    assert_eq!(s["monitors"][0]["tags"][0]["active"], false);
    c.sync();
    assert_eq!((c.ws("1").1, c.ws("3").1), (0, ACTIVE), "the bar is told");

    // Changes from elsewhere (IPC, keys) reach the bar too, with a done.
    let done = c.app.ws_done;
    uwc.ok("toggleview 5");
    std::thread::sleep(Duration::from_millis(50));
    c.sync();
    assert_eq!((c.ws("3").1, c.ws("5").1), (ACTIVE, ACTIVE));
    assert!(c.app.ws_done > done);

    // A deactivation makes the batch a toggle.
    c.ws("3").0.deactivate();
    m.commit();
    c.sync();
    uwc.wait("only tag 5", |s| {
        s["monitors"][0]["tags"][2]["active"] == false && s["monitors"][0]["tags"][4]["active"] == true
    });
    // Deactivating the last visible tag is ignored.
    c.ws("5").0.deactivate();
    m.commit();
    c.sync();
    assert_eq!(uwc.state()["monitors"][0]["tags"][4]["active"], true);
    // Requests without a commit do nothing.
    c.ws("7").0.activate();
    c.sync();
    assert_eq!(uwc.state()["monitors"][0]["tags"][6]["active"], false);

    // A wl_output bound after the manager still gets the group announced on it.
    assert_eq!(c.app.ws_group_outputs, 0);
    let _o = c.bind_output();
    std::thread::sleep(Duration::from_millis(50));
    c.sync();
    assert_eq!(c.app.ws_group_outputs, 1);
}

#[test]
fn pinned_windows_stay_on_screen_and_on_every_tag() {
    let mut uwc = Uwc::start("pin", None);
    let mut c = uwc.connect();
    let _a = c.window("A", 300, 200);
    let s = uwc.wait("A", |s| !s["windows"].as_array().unwrap().is_empty());
    let a = win(&s, "A").clone();
    let id = a["id"].as_u64().unwrap();
    uwc.ok(&format!("pin {id}"));
    assert_eq!(win(&uwc.state(), "A")["pinned"], true);
    // Panning moves the canvas under it: its canvas position follows the fixed screen spot.
    uwc.ok("pan 500 -100");
    let s = uwc.state();
    assert_eq!((num(&win(&s, "A")["x"]), num(&win(&s, "A")["y"])), (num(&a["x"]) + 500.0, num(&a["y"]) - 100.0));
    // Shown on every tag: it keeps the focus across a tag switch.
    uwc.ok("view 4");
    let s = uwc.state();
    assert_eq!(win(&s, "A")["focused"], true);
    assert_eq!(s["keyboard_focus"]["window"], id);
    // Unpinned, it lands on the canvas where it was on screen, on the tag in view.
    uwc.ok(&format!("unpin {id}"));
    let s = uwc.state();
    let a2 = win(&s, "A");
    assert_eq!((a2["pinned"].clone(), a2["tags"].clone()), (false.into(), 8.into()));
    // Tag 4's view is at the origin, so the screen spot (350, 200) is canvas (350, 200).
    assert_eq!((num(&a2["x"]), num(&a2["y"])), (350.0, 200.0));
}

#[test]
fn minimized_windows_leave_and_come_back_where_you_are() {
    let mut uwc = Uwc::start("minimize", None);
    let mut c = uwc.connect();
    let _a = c.window("A", 300, 200);
    let _b = c.window("B", 300, 200);
    let s = uwc.wait("two", |s| s["windows"].as_array().unwrap().len() == 2);
    let (ia, ib) = (win(&s, "A")["id"].as_u64().unwrap(), win(&s, "B")["id"].as_u64().unwrap());
    uwc.ok(&format!("minimize {ib}"));
    let s = uwc.state();
    assert_eq!(win(&s, "B")["minimized"], true);
    assert_eq!(s["keyboard_focus"]["window"], ia, "focus moves to what is left");
    // Restored on another tag: it comes to you.
    uwc.ok("view 3");
    uwc.ok(&format!("restore {ib}"));
    let s = uwc.state();
    let b = win(&s, "B");
    assert_eq!(
        (b["minimized"].clone(), b["tags"].clone(), b["focused"].clone()),
        (false.into(), 4.into(), true.into())
    );
}

#[test]
fn fullscreen_covers_the_output_and_gives_the_size_back() {
    let mut uwc = Uwc::start("fullscreen", None);
    let mut c = uwc.connect();
    let mut a = c.window("A", 300, 200);
    let s = uwc.wait("A", |s| !s["windows"].as_array().unwrap().is_empty());
    let (id, x, y) = (win(&s, "A")["id"].as_u64().unwrap(), win(&s, "A")["x"].clone(), win(&s, "A")["y"].clone());

    uwc.ok(&format!("fullscreen {id}"));
    std::thread::sleep(Duration::from_millis(50));
    c.follow_configure("A", &mut a);
    let s = uwc.wait("fullscreen size", |s| win(s, "A")["width"] == 1000);
    assert_eq!((win(&s, "A")["height"].clone(), win(&s, "A")["fullscreen"].clone()), (600.into(), true.into()));
    assert!(uwc.cmd(&format!("pin {id}")).get("error").is_some(), "no pinning while fullscreen");

    uwc.ok(&format!("unfullscreen {id}"));
    std::thread::sleep(Duration::from_millis(50));
    c.follow_configure("A", &mut a);
    let s = uwc.wait("size back", |s| win(s, "A")["width"] == 300);
    let w = win(&s, "A");
    assert_eq!((w["height"].clone(), w["fullscreen"].clone()), (200.into(), false.into()));
    assert_eq!((w["x"].clone(), w["y"].clone()), (x, y), "same place on the canvas");

    // The client can ask for it too.
    a.toplevel.set_fullscreen(None);
    c.sync();
    std::thread::sleep(Duration::from_millis(50));
    c.follow_configure("A", &mut a);
    uwc.wait("client-requested fullscreen", |s| win(s, "A")["fullscreen"] == true && win(s, "A")["width"] == 1000);
    a.toplevel.unset_fullscreen();
    c.sync();
    std::thread::sleep(Duration::from_millis(50));
    c.follow_configure("A", &mut a);
    uwc.wait("client left fullscreen", |s| win(s, "A")["fullscreen"] == false && win(s, "A")["width"] == 300);
}
