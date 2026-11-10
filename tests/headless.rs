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
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1 as ext_tl_handle, ext_foreign_toplevel_list_v1 as ext_tl_list,
};
use wayland_protocols::ext::idle_notify::v1::client::{ext_idle_notification_v1, ext_idle_notifier_v1};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_foreign_toplevel_image_capture_source_manager_v1 as ext_tl_source_manager,
    ext_image_capture_source_v1 as ext_source, ext_output_image_capture_source_manager_v1 as ext_output_source_manager,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1 as ext_frame, ext_image_copy_capture_manager_v1 as ext_copy_manager,
    ext_image_copy_capture_session_v1 as ext_session,
};
use wayland_protocols::ext::session_lock::v1::client::{ext_session_lock_surface_v1, ext_session_lock_v1};
use wayland_protocols::ext::workspace::v1::client::{
    ext_workspace_group_handle_v1 as ws_group, ext_workspace_handle_v1 as ws_handle,
    ext_workspace_manager_v1 as ws_manager,
};
use wayland_protocols::wp::{
    fractional_scale::v1::client::{wp_fractional_scale_manager_v1, wp_fractional_scale_v1},
    idle_inhibit::zv1::client::{zwp_idle_inhibit_manager_v1, zwp_idle_inhibitor_v1},
    pointer_constraints::zv1::client::{zwp_confined_pointer_v1, zwp_locked_pointer_v1},
    text_input::zv3::client::{zwp_text_input_manager_v3, zwp_text_input_v3},
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols::xdg::{
    activation::v1::client::{xdg_activation_token_v1, xdg_activation_v1},
    shell::client::{xdg_popup, xdg_positioner},
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1 as tl_handle, zwlr_foreign_toplevel_manager_v1 as tl_manager,
};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1 as sc_frame, zwlr_screencopy_manager_v1 as sc_manager,
};

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
        Uwc::start_with_state(name, config, &dir.join("state"))
    }

    /// With the session file under `state` (it outlives this compositor).
    fn start_with_state(name: &str, config: Option<&str>, state: &std::path::Path) -> Uwc {
        Uwc::start_full(name, config, state, "1000x600")
    }

    /// Several virtual outputs: `screens` is a `--headless` size list.
    fn start_screens(name: &str, screens: &str) -> Uwc {
        let dir = std::env::temp_dir().join(format!("uwc-test-{}-{name}", std::process::id()));
        Uwc::start_full(name, None, &dir.join("state"), screens)
    }

    fn start_full(name: &str, config: Option<&str>, state: &std::path::Path, screens: &str) -> Uwc {
        let dir = std::env::temp_dir().join(format!("uwc-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_uwc"));
        cmd.arg("--headless")
            .arg(screens)
            .env("XDG_RUNTIME_DIR", &dir)
            .env("XDG_STATE_HOME", state)
            .env("UWC_DEBUG_PANIC", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
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
use hyprland::{
    hyprland_toplevel_export_frame_v1 as export_frame, hyprland_toplevel_export_manager_v1 as export_manager,
};

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
    /// The last fractional preferred scale (in 1/120) and integer preferred buffer scale told.
    frac: Option<u32>,
    /// wlr-foreign-toplevel: handles in announcement order and what was said about each.
    tl: Vec<tl_handle::ZwlrForeignToplevelHandleV1>,
    /// hyprland-toplevel-export: the last frame's buffer offer, and how it ended.
    export_buffer: Option<(u32, u32, u32, u32)>,
    export_done: bool,
    export_failed: bool,
    export_ready: bool,
    tl_info: std::collections::HashMap<wayland_client::backend::ObjectId, TlInfo>,
    buffer_scale: Option<i32>,
    closed: Vec<String>,
    /// The size the compositor last asked a toplevel for.
    asked: Option<(i32, i32)>,
    /// The size the last layer-surface configure gave.
    layer_size: Option<(u32, u32)>,
    /// The activation token string the last token request produced.
    token: Option<String>,
    /// Which idle notifications fired (their labels), and which resumed after.
    idle_fired: Vec<u32>,
    idle_resumed: Vec<u32>,
    /// popup_done arrived for the grabbed popup.
    popup_done: bool,
    /// ext-session-lock: the events so far ("locked", "finished", "configure WxH"), and the
    /// serial to ack.
    lock_events: Vec<String>,
    lock_serial: Option<u32>,
    /// zwp-pointer-constraints: the events so far ("confined", "locked", "unconfined",
    /// "unlocked").
    constraint_events: Vec<String>,
    /// zwp-text-input-v3: enter / leave.
    ti_enter: bool,
    ti_leave: bool,
    /// ext-foreign-toplevel-list: (identifier, title, app-id, closed) per handle.
    ext_tl: Vec<(ext_tl_handle::ExtForeignToplevelHandleV1, String, String, String, bool)>,
    /// ext-image-copy-capture: what the session offered and how frames ended.
    cap_events: Vec<String>,
}

#[derive(Default, Debug, Clone)]
struct TlInfo {
    title: String,
    app_id: String,
    states: Vec<u32>,
    outputs: usize,
    done: usize,
    closed: bool,
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
            frac: None,
            tl: vec![],
            export_buffer: None,
            export_done: false,
            export_failed: false,
            export_ready: false,
            tl_info: Default::default(),
            buffer_scale: None,
            closed: vec![],
            asked: None,
            layer_size: None,
            token: None,
            idle_fired: vec![],
            idle_resumed: vec![],
            popup_done: false,
            lock_events: vec![],
            lock_serial: None,
            constraint_events: vec![],
            ti_enter: false,
            ti_leave: false,
            ext_tl: Vec::new(),
            cap_events: Vec::new(),
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
        self.window_with_parent(title, w, h, None)
    }

    /// A toplevel that declares `parent` as the window it is a dialog of (xdg_toplevel.set_parent).
    fn window_with_parent(&mut self, title: &str, w: i32, h: i32, parent: Option<&Window>) -> Window {
        let qh = self.queue.handle();
        let surface = self.app.compositor.create_surface(&qh, ());
        let xdg = self.app.wm.get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg.get_toplevel(&qh, ());
        toplevel.set_parent(parent.map(|p| &p.toplevel));
        toplevel.set_title(title.into());
        toplevel.set_app_id("uwc.test".into());
        surface.commit();
        self.queue.roundtrip(&mut self.app).unwrap(); // initial configure arrives and is acked
                                                      // Like a toolkit: a size in the first configure wins over the one it would pick itself.
        let (w, h) = self.app.asked.take().unwrap_or((w, h));
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

    fn bind_seat(&mut self) -> wayland_client::protocol::wl_seat::WlSeat {
        let qh = self.queue.handle();
        let s = self.globals.bind(&qh, 1..=7, ()).expect("wl_seat");
        self.sync();
        s
    }

    /// xdg-activation, the way a launcher binds it.
    fn activation(&mut self) -> xdg_activation_v1::XdgActivationV1 {
        let qh = self.queue.handle();
        let a = self.globals.bind(&qh, 1..=1, ()).expect("xdg_activation_v1");
        self.sync();
        a
    }

    /// A fresh activation token (the string a launcher hands to the app it starts).
    fn get_token(&mut self, act: &xdg_activation_v1::XdgActivationV1) -> String {
        let qh = self.queue.handle();
        act.get_activation_token(&qh, ()).commit();
        self.sync();
        self.app.token.take().expect("activation token")
    }

    fn idle_notifier(&mut self) -> ext_idle_notifier_v1::ExtIdleNotifierV1 {
        let qh = self.queue.handle();
        let n = self.globals.bind(&qh, 1..=1, ()).expect("ext_idle_notifier_v1");
        self.sync();
        n
    }

    /// An idle notification labelled `label`: `idle_fired` gains it once `timeout_ms` passes without
    /// input, `idle_resumed` when activity follows.
    fn idle_notification(
        &mut self,
        notifier: &ext_idle_notifier_v1::ExtIdleNotifierV1,
        seat: &wayland_client::protocol::wl_seat::WlSeat,
        timeout_ms: u32,
        label: u32,
    ) -> ext_idle_notification_v1::ExtIdleNotificationV1 {
        let qh = self.queue.handle();
        let n = notifier.get_idle_notification(timeout_ms, seat, &qh, label);
        self.sync();
        n
    }

    fn idle_inhibit_manager(&mut self) -> zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1 {
        let qh = self.queue.handle();
        let m = self.globals.bind(&qh, 1..=1, ()).expect("zwp_idle_inhibit_manager_v1");
        self.sync();
        m
    }

    /// Lock the session over `output` (a lock screen client): returns the lock, the surface
    /// covering the output and its buffer.
    fn lock(&mut self, output: &wl_output::WlOutput) -> ext_session_lock_v1::ExtSessionLockV1 {
        let qh = self.queue.handle();
        let manager = self
            .globals
            .bind::<wayland_protocols::ext::session_lock::v1::client::ext_session_lock_manager_v1::ExtSessionLockManagerV1, _, _>(&qh, 1..=1, ())
            .expect("ext_session_lock_manager_v1");
        let lock = manager.lock(&qh, ());
        let surface = self.app.compositor.create_surface(&qh, ());
        let _ls = lock.get_lock_surface(&surface, output, &qh, ());
        self.sync();
        // The configure told the output's size; draw a frame over it.
        let (w, h) = self
            .app
            .lock_events
            .iter()
            .find_map(|e| {
                e.strip_prefix("configure ")
                    .and_then(|s| s.split_once('x'))
                    .map(|(a, b)| (a.parse().unwrap(), b.parse().unwrap()))
            })
            .expect("lock surface configure");
        let buf = self.buffer("lock", w, h);
        surface.attach(Some(&buf), 0, 0);
        surface.commit();
        self.sync();
        lock
    }

    /// Roundtrips until the text input got a leave event.
    fn wait_ti_leave(&mut self) {
        let start = Instant::now();
        while !self.app.ti_leave {
            self.sync();
            assert!(start.elapsed() < Duration::from_secs(5), "timed out waiting for text input leave");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Roundtrips until a pointer-constraint event named `what` arrives.
    fn wait_constraint(&mut self, what: &str) {
        let start = Instant::now();
        while !self.app.constraint_events.iter().any(|e| e == what) {
            self.sync();
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "timed out waiting for {what}: {:?}",
                self.app.constraint_events
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Roundtrips until a lock event named `what` arrives.
    fn wait_lock(&mut self, what: &str) {
        let start = Instant::now();
        while !self.app.lock_events.iter().any(|e| e == what) {
            self.sync();
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "timed out waiting for {what}: {:?}",
                self.app.lock_events
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A grabbed popup (a menu opened by a click): created, grabbed and committed.
    fn popup(
        &mut self,
        parent: &Window,
        seat: &wayland_client::protocol::wl_seat::WlSeat,
        grabbed: bool,
    ) -> xdg_popup::XdgPopup {
        let qh = self.queue.handle();
        let pos = self.app.wm.create_positioner(&qh, ());
        pos.set_size(120, 60);
        pos.set_anchor(xdg_positioner::Anchor::TopLeft);
        pos.set_gravity(xdg_positioner::Gravity::BottomRight);
        let surface = self.app.compositor.create_surface(&qh, ());
        let xdg = self.app.wm.get_xdg_surface(&surface, &qh, ());
        let popup = xdg.get_popup(Some(&parent.xdg), &pos, &qh, ());
        if grabbed {
            popup.grab(seat, 1);
        }
        surface.commit();
        self.sync();
        popup
    }

    /// The foreign-toplevel handle titled `title` and what was said about it.
    fn tl(&self, title: &str) -> (tl_handle::ZwlrForeignToplevelHandleV1, TlInfo) {
        let h = self.app.tl.iter().find(|h| self.app.tl_info[&h.id()].title == title).unwrap().clone();
        let i = self.app.tl_info[&h.id()].clone();
        (h, i)
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
impl Dispatch<xdg_activation_token_v1::XdgActivationTokenV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &xdg_activation_token_v1::XdgActivationTokenV1,
        e: xdg_activation_token_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_activation_token_v1::Event::Done { token } = e {
            app.token = Some(token);
        }
    }
}
impl Dispatch<ext_idle_notification_v1::ExtIdleNotificationV1, u32> for App {
    fn event(
        app: &mut Self,
        _: &ext_idle_notification_v1::ExtIdleNotificationV1,
        e: ext_idle_notification_v1::Event,
        label: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            ext_idle_notification_v1::Event::Idled => app.idle_fired.push(*label),
            ext_idle_notification_v1::Event::Resumed => app.idle_resumed.push(*label),
            _ => {}
        }
    }
}
impl Dispatch<ext_session_lock_v1::ExtSessionLockV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &ext_session_lock_v1::ExtSessionLockV1,
        e: ext_session_lock_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            ext_session_lock_v1::Event::Locked => app.lock_events.push("locked".into()),
            ext_session_lock_v1::Event::Finished => app.lock_events.push("finished".into()),
            _ => {}
        }
    }
}
impl Dispatch<ext_session_lock_surface_v1::ExtSessionLockSurfaceV1, ()> for App {
    fn event(
        app: &mut Self,
        s: &ext_session_lock_surface_v1::ExtSessionLockSurfaceV1,
        e: ext_session_lock_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_session_lock_surface_v1::Event::Configure { serial, width, height } = e {
            s.ack_configure(serial);
            app.lock_events.push(format!("configure {width}x{height}"));
            app.lock_serial = Some(serial);
        }
    }
}
impl Dispatch<zwp_confined_pointer_v1::ZwpConfinedPointerV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &zwp_confined_pointer_v1::ZwpConfinedPointerV1,
        e: zwp_confined_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_confined_pointer_v1::Event::Confined = e {
            app.constraint_events.push("confined".into());
        }
    }
}
impl Dispatch<zwp_locked_pointer_v1::ZwpLockedPointerV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &zwp_locked_pointer_v1::ZwpLockedPointerV1,
        e: zwp_locked_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            zwp_locked_pointer_v1::Event::Locked => app.constraint_events.push("locked".into()),
            zwp_locked_pointer_v1::Event::Unlocked => app.constraint_events.push("unlocked".into()),
            _ => {}
        }
    }
}
impl Dispatch<zwp_text_input_v3::ZwpTextInputV3, ()> for App {
    fn event(
        app: &mut Self,
        _: &zwp_text_input_v3::ZwpTextInputV3,
        e: zwp_text_input_v3::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            zwp_text_input_v3::Event::Enter { .. } => app.ti_enter = true,
            zwp_text_input_v3::Event::Leave { .. } => app.ti_leave = true,
            _ => {}
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
            app.popup_done = true;
        }
    }
}
noop!(zwlr_layer_shell_v1::ZwlrLayerShellV1);
noop!(
    wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1
);
noop!(wl_output::WlOutput);
noop!(wayland_client::protocol::wl_seat::WlSeat);
noop!(wayland_client::protocol::wl_pointer::WlPointer);
noop!(export_manager::HyprlandToplevelExportManagerV1);
noop!(sc_manager::ZwlrScreencopyManagerV1);
noop!(xdg_activation_v1::XdgActivationV1);
noop!(ext_idle_notifier_v1::ExtIdleNotifierV1);
noop!(wayland_protocols::ext::session_lock::v1::client::ext_session_lock_manager_v1::ExtSessionLockManagerV1);
noop!(wayland_protocols::wp::pointer_constraints::zv1::client::zwp_pointer_constraints_v1::ZwpPointerConstraintsV1);
noop!(
    wayland_protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1
);
noop!(zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1);
noop!(zwp_text_input_manager_v3::ZwpTextInputManagerV3);
noop!(zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1);
noop!(xdg_positioner::XdgPositioner);

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
                app.export_buffer = Some((format.into(), width, height, stride))
            }
            sc_frame::Event::BufferDone => app.export_done = true,
            sc_frame::Event::Failed => app.export_failed = true,
            sc_frame::Event::Ready { .. } => app.export_ready = true,
            _ => {}
        }
    }
}

impl Dispatch<export_frame::HyprlandToplevelExportFrameV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &export_frame::HyprlandToplevelExportFrameV1,
        e: export_frame::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            export_frame::Event::Buffer { format, width, height, stride } => {
                app.export_buffer = Some((format.into(), width, height, stride))
            }
            export_frame::Event::BufferDone => app.export_done = true,
            export_frame::Event::Failed => app.export_failed = true,
            export_frame::Event::Ready { .. } => app.export_ready = true,
            _ => {}
        }
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
            app.tl_info.insert(toplevel.id(), TlInfo::default());
            app.tl.push(toplevel);
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
        let i = app.tl_info.entry(h.id()).or_default();
        match e {
            tl_handle::Event::Title { title } => i.title = title,
            tl_handle::Event::AppId { app_id } => i.app_id = app_id,
            tl_handle::Event::State { state } => {
                i.states = state.chunks_exact(4).map(|c| u32::from_ne_bytes(c.try_into().unwrap())).collect()
            }
            tl_handle::Event::OutputEnter { .. } => i.outputs += 1,
            tl_handle::Event::Done => i.done += 1,
            tl_handle::Event::Closed => i.closed = true,
            _ => {}
        }
    }
}

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
noop!(wl_compositor::WlCompositor, wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_buffer::WlBuffer);
noop!(wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1);

impl Dispatch<ext_tl_list::ExtForeignToplevelListV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &ext_tl_list::ExtForeignToplevelListV1,
        e: ext_tl_list::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_tl_list::Event::Toplevel { toplevel } = e {
            app.ext_tl.push((toplevel, String::new(), String::new(), String::new(), false));
        }
    }

    wayland_client::event_created_child!(App, ext_tl_list::ExtForeignToplevelListV1, [
        ext_tl_list::EVT_TOPLEVEL_OPCODE => (ext_tl_handle::ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ext_tl_handle::ExtForeignToplevelHandleV1, ()> for App {
    fn event(
        app: &mut Self,
        h: &ext_tl_handle::ExtForeignToplevelHandleV1,
        e: ext_tl_handle::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(t) = app.ext_tl.iter_mut().find(|t| &t.0 == h) else { return };
        match e {
            ext_tl_handle::Event::Identifier { identifier } => t.1 = identifier,
            ext_tl_handle::Event::Title { title } => t.2 = title,
            ext_tl_handle::Event::AppId { app_id } => t.3 = app_id,
            ext_tl_handle::Event::Closed => t.4 = true,
            _ => {}
        }
    }
}

impl Dispatch<ext_tl_source_manager::ExtForeignToplevelImageCaptureSourceManagerV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ext_tl_source_manager::ExtForeignToplevelImageCaptureSourceManagerV1,
        _: ext_tl_source_manager::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<ext_output_source_manager::ExtOutputImageCaptureSourceManagerV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ext_output_source_manager::ExtOutputImageCaptureSourceManagerV1,
        _: ext_output_source_manager::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<ext_source::ExtImageCaptureSourceV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ext_source::ExtImageCaptureSourceV1,
        _: ext_source::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<ext_copy_manager::ExtImageCopyCaptureManagerV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ext_copy_manager::ExtImageCopyCaptureManagerV1,
        _: ext_copy_manager::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
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
            ext_session::Event::BufferSize { width, height } => app.cap_events.push(format!("size {width}x{height}")),
            ext_session::Event::ShmFormat { .. } => app.cap_events.push("shm".into()),
            ext_session::Event::Done => app.cap_events.push("done".into()),
            ext_session::Event::Stopped => app.cap_events.push("stopped".into()),
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
            ext_frame::Event::Ready => app.cap_events.push("ready".into()),
            ext_frame::Event::Failed { .. } => app.cap_events.push("failed".into()),
            _ => {}
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for App {
    fn event(
        app: &mut Self,
        _: &wl_surface::WlSurface,
        e: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_surface::Event::PreferredBufferScale { factor } = e {
            app.buffer_scale = Some(factor);
        }
    }
}

impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &wp_fractional_scale_v1::WpFractionalScaleV1,
        e: wp_fractional_scale_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = e {
            app.frac = Some(scale);
        }
    }
}

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
    assert_eq!(s["hints"], false);
    assert!(uwc.cmd("bogus").get("error").is_some());
    assert!(uwc.cmd("pan x y").get("error").is_some());
}

#[test]
fn the_everyday_protocols_are_advertised() {
    let uwc = Uwc::start("globals", None);
    let c = uwc.connect();
    let have: Vec<String> = c.globals.contents().with_list(|l| l.iter().map(|g| g.interface.clone()).collect());
    for want in [
        "wp_cursor_shape_manager_v1",
        "zwlr_data_control_manager_v1",
        "ext_data_control_manager_v1",
        "zwp_virtual_keyboard_manager_v1",
        "wp_single_pixel_buffer_manager_v1",
        "zxdg_exporter_v2",
        "zxdg_importer_v2",
        "xdg_wm_dialog_v1",
        "zwp_keyboard_shortcuts_inhibit_manager_v1",
        "xdg_system_bell_v1",
        "wp_content_type_manager_v1",
    ] {
        assert!(have.iter().any(|h| h == want), "missing global {want}; have {have:?}");
    }
}

#[test]
fn dialogs_open_over_their_parent() {
    let mut uwc = Uwc::start("dialog", None);
    let mut c = uwc.connect();
    let parent = c.window("parent", 400, 300);
    // Move the parent far from where a new window would open.
    let pid = win(&uwc.state(), "parent")["id"].as_u64().unwrap();
    uwc.cmd(&format!("move {pid} 3000 2500"));
    let _dialog = c.window_with_parent("dialog", 200, 100, Some(&parent));
    let s = uwc.state();
    let (p, d) = (win(&s, "parent"), win(&s, "dialog"));
    // Centred over the parent, in canvas pixels, on its tags.
    assert_eq!(num(&d["x"]) + 100.0, num(&p["x"]) + 200.0, "{d} {p}");
    assert_eq!(num(&d["y"]) + 50.0, num(&p["y"]) + 150.0, "{d} {p}");
    assert_eq!(d["tags"], p["tags"]);
}

#[test]
fn windows_and_outputs_can_be_captured_through_the_ext_protocols() {
    let uwc = Uwc::start("extcap", None);
    let mut c = uwc.connect();
    let qh = c.queue.handle();
    let list: ext_tl_list::ExtForeignToplevelListV1 = c.globals.bind(&qh, 1..=1, ()).unwrap();
    let w = c.window("shared", 320, 200);
    c.sync();
    c.sync();
    // The window is on the list, with its title and app-id.
    let t = c.app.ext_tl.iter().find(|t| t.2 == "shared").expect("window announced");
    assert_eq!(t.3, "uwc.test");
    assert!(!t.1.is_empty(), "an identifier");
    let handle = t.0.clone();

    let sources: ext_tl_source_manager::ExtForeignToplevelImageCaptureSourceManagerV1 =
        c.globals.bind(&qh, 1..=1, ()).unwrap();
    let out_sources: ext_output_source_manager::ExtOutputImageCaptureSourceManagerV1 =
        c.globals.bind(&qh, 1..=1, ()).unwrap();
    let copy: ext_copy_manager::ExtImageCopyCaptureManagerV1 = c.globals.bind(&qh, 1..=1, ()).unwrap();
    let output = c.bind_output();

    // A session on the window offers its size.
    let src = sources.create_source(&handle, &qh, ());
    let session = copy.create_session(&src, ext_copy_manager::Options::empty(), &qh, ());
    c.sync();
    assert_eq!(c.app.cap_events, ["size 320x200", "shm", "done"]);

    // A session on the output offers the output's size.
    c.app.cap_events.clear();
    let osrc = out_sources.create_source(&output, &qh, ());
    let osession = copy.create_session(&osrc, ext_copy_manager::Options::PaintCursors, &qh, ());
    c.sync();
    assert_eq!(c.app.cap_events, ["size 1000x600", "shm", "done"]);

    // A frame into a matching buffer: headless cannot render, so it fails cleanly (and never hangs).
    c.app.cap_events.clear();
    let buf = c.buffer("cap", 320, 200);
    let frame = session.create_frame(&qh, ());
    frame.attach_buffer(&buf);
    frame.capture();
    c.sync();
    assert_eq!(c.app.cap_events, ["failed"]);
    frame.destroy();

    // Closing the window closes its toplevel handle, and the session on it stops.
    c.app.cap_events.clear();
    w.toplevel.destroy();
    w.xdg.destroy();
    w.surface.destroy();
    c.sync();
    c.sync();
    assert!(c.app.ext_tl.iter().find(|t| t.2 == "shared").unwrap().4, "handle closed");
    let frame = session.create_frame(&qh, ());
    frame.attach_buffer(&buf);
    frame.capture();
    c.sync();
    assert_eq!(c.app.cap_events, ["failed", "stopped"]);
    let _ = (list, osession);
}

#[test]
fn a_virtual_pointer_moves_the_cursor_and_clicks() {
    use wayland_protocols_wlr::virtual_pointer::v1::client::{
        zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1, zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    };
    let mut uwc = Uwc::start("vptr", None);
    let mut c = uwc.connect();
    let qh = c.queue.handle();
    let _a = c.window("A", 200, 150);
    let _b = c.window("B", 200, 150);
    let (a, b) = (win(&uwc.state(), "A")["id"].as_u64().unwrap(), win(&uwc.state(), "B")["id"].as_u64().unwrap());
    uwc.cmd(&format!("move {a} 100 100"));
    uwc.cmd(&format!("move {b} 600 300"));
    // Let the open and move animations settle: hit-testing follows what is drawn.
    for _ in 0..100 {
        if uwc.state()["monitors"][0]["animating"] == false {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let m: ZwlrVirtualPointerManagerV1 = c.globals.bind(&qh, 1..=2, ()).unwrap();
    let p: ZwlrVirtualPointerV1 = m.create_virtual_pointer(None, &qh, ());
    // Absolute motion maps onto the 1000x600 output: put the cursor over A's centre. B has the
    // focus (it opened last); a click through the virtual pointer focuses A.
    let st = uwc.state();
    assert_eq!(st["keyboard_focus"]["window"], b);
    let (vp, wa) = (&st["monitors"][0]["viewport"], win(&st, "A"));
    let (px, py) = (
        (num(&wa["x"]) - num(&vp["x"]) + num(&wa["width"]) / 2.0) as u32,
        (num(&wa["y"]) - num(&vp["y"]) + num(&wa["height"]) / 2.0) as u32,
    );
    p.motion_absolute(0, px, py, 1000, 600);
    p.frame();
    c.sync();
    let s = uwc.state();
    assert_eq!((num(&s["cursor"]["x"]), num(&s["cursor"]["y"])), (px as f64, py as f64));
    p.button(1, 0x110, wayland_client::protocol::wl_pointer::ButtonState::Pressed);
    p.button(2, 0x110, wayland_client::protocol::wl_pointer::ButtonState::Released);
    p.frame();
    c.sync();
    assert_eq!(uwc.state()["keyboard_focus"]["window"], a, "the click focused A");
    // Relative motion moves from there.
    p.motion(3, 10.0, -20.0);
    p.frame();
    c.sync();
    let s = uwc.state();
    assert_eq!((num(&s["cursor"]["x"]), num(&s["cursor"]["y"])), (px as f64 + 10.0, py as f64 - 20.0));
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
fn view_moves_ease_to_their_target() {
    let mut uwc = Uwc::start("ease", None);
    // IPC shows where the view is going at once; the view itself follows over view-ms.
    uwc.ok("pan 400 0");
    let s = uwc.state();
    assert_eq!(s["monitors"][0]["viewport"]["x"], 400.0);
    assert_eq!(s["monitors"][0]["view_moving"], true);
    // A second pan builds on the target, not on wherever the view is by now.
    uwc.ok("pan 100 0");
    assert_eq!(uwc.state()["monitors"][0]["viewport"]["x"], 500.0);
    uwc.wait("the view to settle", |s| s["monitors"][0]["view_moving"] == false);
    assert_eq!(uwc.state()["monitors"][0]["viewport"]["x"], 500.0);
    // Zoom eases too, about its anchor, and builds on the target like pans do.
    uwc.ok("panto 0 0");
    uwc.ok("zoom 2 100 100");
    uwc.ok("zoom 4 100 100");
    let s = uwc.state();
    let v = &s["monitors"][0]["viewport"];
    let f = |k: &str| v[k].as_f64().unwrap();
    assert_eq!((f("x"), f("y"), f("zoom")), (75.0, 75.0, 4.0), "{s:#}");
    assert_eq!(s["monitors"][0]["view_moving"], true);
    let s = uwc.wait("the zoom to settle", |s| s["monitors"][0]["view_moving"] == false);
    assert_eq!(s["monitors"][0]["viewport"]["zoom"], 4.0);
    // An instant change (the overview) in the middle of a move finishes it first.
    uwc.ok("panto 300 0");
    uwc.ok("overview");
    uwc.ok("overview");
    assert_eq!(uwc.state()["monitors"][0]["viewport"]["x"], 300.0);
    // view-ms 0 jumps.
    uwc.ok("set view-ms 0");
    uwc.ok("pan 40 0");
    assert_eq!(uwc.state()["monitors"][0]["view_moving"], false);
}

#[test]
fn a_released_swipe_glides_on_and_slows_down() {
    let mut uwc = Uwc::start("fling", None);
    let x0 = uwc.state()["monitors"][0]["viewport"]["x"].as_f64().unwrap();
    // Content moving left at 2000 px/s (the view goes right), losing speed with a 600 ms constant:
    // it travels about v * (600 / 4000) = 300 px.
    uwc.ok("fling -2000 0");
    assert_eq!(uwc.state()["monitors"][0]["view_moving"], true);
    let s = uwc.wait("the glide to end", |s| s["monitors"][0]["view_moving"] == false);
    let dx = s["monitors"][0]["viewport"]["x"].as_f64().unwrap() - x0;
    assert!((200.0..400.0).contains(&dx), "glided {dx} px");
    assert_eq!(s["monitors"][0]["animating"], false);
    // Doing anything else with the view stops a glide at once.
    uwc.ok("fling 2000 0");
    uwc.ok("pan 0 10");
    assert_eq!(uwc.state()["monitors"][0]["view_moving"], true, "the pan itself eases");
    let s = uwc.wait("the pan to end", |s| s["monitors"][0]["view_moving"] == false);
    let x1 = s["monitors"][0]["viewport"]["x"].as_f64().unwrap();
    assert!(x1 > x0 + 100.0, "the second fling was cancelled before it travelled far: {x1} vs {x0}");
    // 0 turns momentum off: letting go stops dead.
    uwc.ok("set pan-momentum-ms 0");
    uwc.ok("fling -2000 0");
    assert_eq!(uwc.state()["monitors"][0]["view_moving"], false);
}

#[test]
fn dragging_a_window_moves_it_and_the_screen_edge_carries_the_view() {
    use wayland_protocols_wlr::virtual_pointer::v1::client::{
        zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1, zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    };
    // A plain right-button drag-move bind, so the test needs no keyboard modifiers.
    let mut uwc =
        Uwc::start("drag", Some("options { open-ms 0\n move-ms 0\n view-ms 0 }\nbinds { MouseRight { drag-move; } }"));
    let mut c = uwc.connect();
    let qh = c.queue.handle();
    let _w = c.window("D", 200, 150);
    let id = win(&uwc.state(), "D")["id"].as_u64().unwrap();
    uwc.cmd(&format!("move {id} 100 100"));
    let m: ZwlrVirtualPointerManagerV1 = c.globals.bind(&qh, 1..=2, ()).unwrap();
    let p: ZwlrVirtualPointerV1 = m.create_virtual_pointer(None, &qh, ());
    let right = wayland_client::protocol::wl_pointer::ButtonState::Pressed;
    let vp = |s: &Value| (num(&s["monitors"][0]["viewport"]["x"]), num(&s["monitors"][0]["viewport"]["y"]));
    let s = uwc.state();
    let (vx0, vy0) = vp(&s);
    let (px, py) = ((100.0 - vx0 + 50.0) as u32, (100.0 - vy0 + 50.0) as u32);
    p.motion_absolute(0, px, py, 1000, 600);
    p.frame();
    p.button(1, 0x111, right);
    p.motion(2, 120.0, 40.0);
    p.frame();
    c.sync();
    let w = |uwc: &mut Uwc| {
        let s = uwc.state();
        let d = win(&s, "D");
        (num(&d["x"]), num(&d["y"]))
    };
    // The window followed the pointer: this used to overflow the stack and kill the compositor.
    assert_eq!(w(&mut uwc), (220.0, 140.0));
    // Against the right edge the view pans that way and the window stays under the pointer.
    p.motion_absolute(3, 999, 140, 1000, 600);
    p.frame();
    c.sync();
    let (before, _) = w(&mut uwc);
    uwc.wait("the view to pan", |s| num(&s["monitors"][0]["viewport"]["x"]) > vx0 + 100.0);
    let s = uwc.state();
    let (after, _) = w(&mut uwc);
    assert!(after > before + 100.0, "the window moved with the view: {before} -> {after}");
    // ... in step: it is still under the pointer (at the same screen position).
    let on_screen = after - num(&s["monitors"][0]["viewport"]["x"]);
    assert!((0.0..1000.0).contains(&on_screen), "{on_screen}");
    p.button(4, 0x111, wayland_client::protocol::wl_pointer::ButtonState::Released);
    p.frame();
    c.sync();
    // Released: the view stops.
    std::thread::sleep(Duration::from_millis(150));
    let a = vp(&uwc.state());
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(vp(&uwc.state()), a, "the view stopped when the drag ended");
    assert_eq!(vy0, vp(&uwc.state()).1);
}

#[test]
fn jump_finds_a_window_on_any_tag_and_brings_it_into_view() {
    let mut uwc = Uwc::start("jump", None);
    let mut c = uwc.connect();
    let _a = c.window("Alpha notes", 200, 150);
    let _b = c.window("Beta build log", 200, 150);
    let s = uwc.state();
    let (a, b) = (win(&s, "Alpha notes")["id"].as_u64().unwrap(), win(&s, "Beta build log")["id"].as_u64().unwrap());
    // B is far away on the canvas.
    uwc.ok(&format!("move {b} 5000 4000"));
    uwc.ok(&format!("move {a} 100 100"));
    uwc.ok("view 1");
    uwc.ok(&format!("focus {a}"));
    uwc.wait("windows to settle", |s| s["monitors"][0]["animating"] == false);
    // Fuzzy, case-insensitive, over app-id and title: "bbl" is B's initials-ish subsequence.
    uwc.ok("jump bbl");
    let s = uwc.state();
    assert_eq!(s["keyboard_focus"]["window"], b);
    // The view went to it: it is within the screen.
    uwc.wait("the view to settle", |s| s["monitors"][0]["view_moving"] == false);
    let s = uwc.state();
    let vp = &s["monitors"][0]["viewport"];
    let wb = win(&s, "Beta build log");
    let sx = num(&wb["x"]) - num(&vp["x"]);
    assert!((0.0..1000.0).contains(&sx), "B is on screen at {sx}");
    // Back to A by window id, and a miss is an error that changes nothing.
    uwc.ok(&format!("jump #{a}"));
    assert_eq!(uwc.state()["keyboard_focus"]["window"], a);
    assert!(uwc.cmd("jump zzzzqq").get("error").is_some());
    assert!(uwc.cmd("jump").get("error").is_some());
    assert_eq!(uwc.state()["keyboard_focus"]["window"], a);
    // A minimized window comes back where you are.
    uwc.ok(&format!("minimize {b}"));
    uwc.ok("jump beta");
    let s = uwc.state();
    assert_eq!(s["keyboard_focus"]["window"], b);
    assert_eq!(win(&s, "Beta build log")["minimized"], false);
}

#[test]
fn a_panic_in_a_handler_does_not_take_the_session_down() {
    let mut uwc = Uwc::start("panic", None);
    let mut c = uwc.connect();
    let _w = c.window("Survivor", 200, 150);
    let id = win(&uwc.state(), "Survivor")["id"].as_u64().unwrap();
    let before = uwc.state()["windows"].as_array().unwrap().len();
    let sock = |u: &Uwc| {
        std::fs::read_dir(&u.dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| e.file_name().to_string_lossy().ends_with(".sock"))
            .map(|e| e.path())
    };
    // The panic happens while this very request is handled, so the reply never comes.
    writeln!(uwc.ipc, "crash").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(uwc.child.try_wait().unwrap().is_none(), "uwc is still running");
    // A fresh connection works, the window is still there and the wayland client is still connected.
    let s = UnixStream::connect(sock(&uwc).unwrap()).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    uwc.reader = BufReader::new(s.try_clone().unwrap());
    uwc.ipc = s;
    assert_eq!(uwc.state()["windows"].as_array().unwrap().len(), before);
    uwc.ok(&format!("focus {id}"));
    c.sync();
    // Three in a row mean something is badly wrong: the session is saved and uwc exits non-zero.
    let mut held = Vec::new();
    for _ in 0..3 {
        if let Some(Ok(mut s)) = sock(&uwc).map(UnixStream::connect) {
            let _ = writeln!(s, "crash");
            held.push(s);
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    let start = Instant::now();
    let status = loop {
        if let Some(st) = uwc.child.try_wait().unwrap() {
            break st;
        }
        assert!(start.elapsed() < Duration::from_secs(5), "uwc should have given up");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(!status.success());
}

#[test]
fn doctor_reports_problems_and_exits_nonzero() {
    let dir = std::env::temp_dir().join(format!("uwc-test-{}-doctor", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let bad = dir.join("bad.kdl");
    std::fs::write(&bad, "options { corner-radius \"wide\" }").unwrap();
    // No runtime dir and a config that does not parse: both are failures with a way out.
    let out = Command::new(env!("CARGO_BIN_EXE_uwc"))
        .args(["doctor", "-c", bad.to_str().unwrap()])
        .env_remove("XDG_RUNTIME_DIR")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "{text}");
    assert!(text.contains("FAIL  XDG_RUNTIME_DIR is not set"), "{text}");
    assert!(text.contains("FAIL  config:"), "{text}");
    assert!(text.contains("-> "), "every problem says what to do: {text}");
    assert!(text.trim_end().ends_with("warnings") || text.contains(" failed, "), "{text}");
    // It changes nothing and does not need a compositor.
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn frames_are_regions_that_carry_their_windows() {
    let mut uwc = Uwc::start("frames", Some("options { open-ms 0\n move-ms 0\n view-ms 0 }"));
    let mut c = uwc.connect();
    let _a = c.window("Inside", 200, 150);
    let _b = c.window("Outside", 200, 150);
    let s = uwc.state();
    let (a, b) = (win(&s, "Inside")["id"].as_u64().unwrap(), win(&s, "Outside")["id"].as_u64().unwrap());
    uwc.ok(&format!("move {a} 1000 1000"));
    uwc.ok(&format!("move {b} 3000 3000"));
    // A frame around where A is: it holds A (its centre is inside) and not B.
    uwc.ok("frame add 900 900 500 400 mail and notes");
    let s = uwc.state();
    let f = &s["frames"][0];
    assert_eq!((f["label"].as_str().unwrap(), num(&f["x"]), num(&f["width"])), ("mail and notes", 900.0, 500.0));
    assert_eq!(f["windows"], serde_json::json!([a]));
    let id = f["id"].as_u64().unwrap();
    // Moving the frame moves its windows, and only them.
    uwc.ok(&format!("frame move {id} 1900 1500"));
    let s = uwc.state();
    assert_eq!((num(&s["frames"][0]["x"]), num(&s["frames"][0]["y"])), (1900.0, 1500.0));
    assert_eq!((num(&win(&s, "Inside")["x"]), num(&win(&s, "Inside")["y"])), (2000.0, 1600.0));
    assert_eq!((num(&win(&s, "Outside")["x"]), num(&win(&s, "Outside")["y"])), (3000.0, 3000.0));
    // Relabel, resize (never below a minimum), fit the view to it.
    uwc.ok(&format!("frame label {id} build"));
    uwc.ok(&format!("frame resize {id} 10 10"));
    let s = uwc.state();
    assert_eq!(s["frames"][0]["label"], "build");
    assert!(num(&s["frames"][0]["width"]) >= 80.0);
    uwc.ok(&format!("frame resize {id} 600 400"));
    uwc.ok(&format!("frame fit {id}"));
    let s = uwc.state();
    let vp = &s["monitors"][0]["viewport"];
    let (vx, vy, z) = (num(&vp["x"]), num(&vp["y"]), num(&vp["zoom"]));
    assert!(vx < 1900.0 + 300.0 && vx + 1000.0 / z > 1900.0 + 300.0, "the frame's centre is in view: {vp}");
    assert!(vy < 1500.0 + 200.0 && vy + 600.0 / z > 1500.0 + 200.0, "{vp}");
    // Errors name the problem; a frame is on its tags only.
    assert!(uwc.cmd("frame move 99 0 0").get("error").is_some());
    assert!(uwc.cmd("frame").get("error").is_some());
    uwc.ok("view 2");
    assert_eq!(uwc.state()["frames"].as_array().unwrap().len(), 1, "still listed, but drawn on its tag only");
    uwc.ok(&format!("frame remove {id}"));
    assert!(uwc.state()["frames"].as_array().unwrap().is_empty());
}

#[test]
fn a_frame_is_dragged_by_its_title_strip_and_resized_by_its_corner() {
    use wayland_client::protocol::wl_pointer::ButtonState;
    use wayland_protocols_wlr::virtual_pointer::v1::client::{
        zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1, zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    };
    let mut uwc = Uwc::start("framedrag", Some("options { open-ms 0\n move-ms 0\n view-ms 0 }"));
    let mut c = uwc.connect();
    let qh = c.queue.handle();
    let _w = c.window("Held", 150, 100);
    let id = win(&uwc.state(), "Held")["id"].as_u64().unwrap();
    uwc.ok(&format!("move {id} 200 200"));
    uwc.ok("frame add 150 150 400 300 zone");
    let m: ZwlrVirtualPointerManagerV1 = c.globals.bind(&qh, 1..=2, ()).unwrap();
    let p: ZwlrVirtualPointerV1 = m.create_virtual_pointer(None, &qh, ());
    let s = uwc.state();
    let vp = &s["monitors"][0]["viewport"];
    let (vx, vy) = (num(&vp["x"]), num(&vp["y"]));
    let pos = |x: f64, y: f64| ((x - vx) as u32, (y - vy) as u32);
    // The strip is the top 22 px of the frame (label scale 2 x 11): press at (160, 160), drag by +100, +50.
    let (sx, sy) = pos(160.0, 160.0);
    p.motion_absolute(0, sx, sy, 1000, 600);
    p.frame();
    p.button(1, 0x110, ButtonState::Pressed);
    p.motion(2, 100.0, 50.0);
    p.frame();
    p.button(3, 0x110, ButtonState::Released);
    p.frame();
    c.sync();
    let s = uwc.state();
    assert_eq!((num(&s["frames"][0]["x"]), num(&s["frames"][0]["y"])), (250.0, 200.0), "{}", s["frames"]);
    assert_eq!((num(&win(&s, "Held")["x"]), num(&win(&s, "Held")["y"])), (300.0, 250.0), "its window went along");
    // The bottom-right corner resizes: press 4 px inside it, drag by +60, +40.
    let (cx, cy) = pos(250.0 + 400.0 - 4.0, 200.0 + 300.0 - 4.0);
    p.motion_absolute(4, cx, cy, 1000, 600);
    p.frame();
    p.button(5, 0x110, ButtonState::Pressed);
    p.motion(6, 60.0, 40.0);
    p.frame();
    p.button(7, 0x110, ButtonState::Released);
    p.frame();
    c.sync();
    let s = uwc.state();
    assert_eq!((num(&s["frames"][0]["width"]), num(&s["frames"][0]["height"])), (460.0, 340.0), "{}", s["frames"]);
    assert_eq!((num(&s["frames"][0]["x"]), num(&s["frames"][0]["y"])), (250.0, 200.0));
}

#[test]
fn dragged_windows_snap_to_other_windows_and_to_a_grid() {
    use wayland_client::protocol::wl_pointer::ButtonState;
    use wayland_protocols_wlr::virtual_pointer::v1::client::{
        zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1, zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    };
    let mut uwc =
        Uwc::start("snap", Some("options { open-ms 0\n move-ms 0\n view-ms 0 }\nbinds { MouseRight { drag-move; } }"));
    let mut c = uwc.connect();
    let qh = c.queue.handle();
    let _a = c.window("Anchor", 200, 150);
    let _b = c.window("Mover", 200, 150);
    let s = uwc.state();
    let (a, b) = (win(&s, "Anchor")["id"].as_u64().unwrap(), win(&s, "Mover")["id"].as_u64().unwrap());
    let m: ZwlrVirtualPointerManagerV1 = c.globals.bind(&qh, 1..=2, ()).unwrap();
    let p: ZwlrVirtualPointerV1 = m.create_virtual_pointer(None, &qh, ());
    let at = |uwc: &mut Uwc, who: &str| {
        let s = uwc.state();
        let w = win(&s, who);
        (num(&w["x"]), num(&w["y"]))
    };
    // Put the mover at (500, 300) and drag it by (dx, dy) from its centre.
    let drag = |uwc: &mut Uwc, c: &mut Client, dx: f64, dy: f64| {
        uwc.ok(&format!("move {b} 500 300"));
        let s = uwc.state();
        let vp = &s["monitors"][0]["viewport"];
        let (px, py) = ((500.0 - num(&vp["x"]) + 100.0) as u32, (300.0 - num(&vp["y"]) + 75.0) as u32);
        p.motion_absolute(0, px, py, 1000, 600);
        p.frame();
        p.button(1, 0x111, ButtonState::Pressed);
        p.motion(2, dx, dy);
        p.frame();
        p.button(3, 0x111, ButtonState::Released);
        p.frame();
        c.sync();
    };
    uwc.ok(&format!("move {a} 100 100"));
    // Within 8 px of the anchor's right edge (300) and top (100): both snap flush.
    drag(&mut uwc, &mut c, 305.0 - 500.0, 103.0 - 300.0);
    assert_eq!(at(&mut uwc, "Mover"), (300.0, 100.0));
    // Its own right edge to the anchor's left edge (100): 100 - 200 = -100 for its x.
    drag(&mut uwc, &mut c, -97.0 - 500.0, 250.0 - 300.0);
    assert_eq!(at(&mut uwc, "Mover").0, -100.0);
    // Out of range: left alone.
    drag(&mut uwc, &mut c, 320.0 - 500.0, 130.0 - 300.0);
    assert_eq!(at(&mut uwc, "Mover"), (320.0, 130.0));
    // snap-distance 0 turns window snapping off, and the grid then rounds.
    uwc.ok("set snap-distance 0");
    drag(&mut uwc, &mut c, 305.0 - 500.0, 103.0 - 300.0);
    assert_eq!(at(&mut uwc, "Mover"), (305.0, 103.0));
    uwc.ok("set snap-grid 50");
    drag(&mut uwc, &mut c, 277.0 - 500.0, 331.0 - 300.0);
    assert_eq!(at(&mut uwc, "Mover"), (300.0, 350.0));
}

#[test]
fn dragging_a_window_under_another_moves_that_window() {
    use wayland_client::protocol::wl_pointer::ButtonState;
    use wayland_protocols_wlr::virtual_pointer::v1::client::{
        zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1, zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    };
    let mut uwc = Uwc::start(
        "dragunder",
        Some("options { open-ms 0\n move-ms 0\n view-ms 0\n snap-distance 0 }\nbinds {\nMouseRight { drag-move; }\nMouseMiddle { drag-resize; }\n}"),
    );
    let mut c = uwc.connect();
    let qh = c.queue.handle();
    let _a = c.window("Below", 200, 150);
    let _b = c.window("Above", 200, 150);
    let s = uwc.state();
    let (a, b) = (win(&s, "Below")["id"].as_u64().unwrap(), win(&s, "Above")["id"].as_u64().unwrap());
    uwc.ok(&format!("move {a} 100 100"));
    uwc.ok(&format!("move {b} 600 100"));
    let m: ZwlrVirtualPointerManagerV1 = c.globals.bind(&qh, 1..=2, ()).unwrap();
    let p: ZwlrVirtualPointerV1 = m.create_virtual_pointer(None, &qh, ());
    assert_eq!(uwc.state()["keyboard_focus"]["window"], b, "Above is on top and focused");
    let s = uwc.state();
    let vp = &s["monitors"][0]["viewport"];
    let (px, py) = ((200.0 - num(&vp["x"])) as u32, (175.0 - num(&vp["y"])) as u32);
    // Press on the lower window: focusing it raises it (the window list is reordered), and it is that
    // window that moves, not whichever took its place in the list.
    p.motion_absolute(0, px, py, 1000, 600);
    p.frame();
    p.button(1, 0x111, ButtonState::Pressed);
    p.motion(2, 30.0, 20.0);
    p.frame();
    p.button(3, 0x111, ButtonState::Released);
    p.frame();
    c.sync();
    let s = uwc.state();
    assert_eq!(s["keyboard_focus"]["window"], a);
    assert_eq!((num(&win(&s, "Below")["x"]), num(&win(&s, "Below")["y"])), (130.0, 120.0));
    assert_eq!((num(&win(&s, "Above")["x"]), num(&win(&s, "Above")["y"])), (600.0, 100.0), "the other one stayed");
    // The same for a resize drag started on a window that is not on top.
    let (px, py) = ((600.0 + 100.0 - num(&vp["x"])) as u32, (100.0 + 75.0 - num(&vp["y"])) as u32);
    c.app.asked = None;
    p.motion_absolute(4, px, py, 1000, 600);
    p.frame();
    p.button(5, 0x112, ButtonState::Pressed);
    p.motion(6, 40.0, 30.0);
    p.frame();
    p.button(7, 0x112, ButtonState::Released);
    p.frame();
    c.sync();
    let s = uwc.state();
    assert_eq!(s["keyboard_focus"]["window"], b);
    // The client is asked for the new size (this test client does not redraw at it).
    assert_eq!(c.app.asked, Some((240, 180)), "{}", s["windows"]);
}

#[test]
fn hooks_run_commands_with_the_event_in_their_environment() {
    let log = std::env::temp_dir().join(format!("uwc-test-{}-hooks.log", std::process::id()));
    let _ = std::fs::remove_file(&log);
    let l = log.display();
    let line =
        r#"printf '%s|%s|%s|%s\n' $UWC_EVENT $UWC_APP_ID $UWC_TITLE $UWC_TAGSET >> LOG"#.replace("LOG", &l.to_string());
    let cfg = format!(
        "hooks {{\n window-open \"{line}\"\n window-close \"{line}\"\n focus \"{line}\"\n tag-change \"{line}\"\n}}\n"
    );
    let mut uwc = Uwc::start("hooks", Some(&cfg));
    assert!(uwc.state()["config_error"].is_null(), "{}", uwc.state()["config_error"]);
    let mut c = uwc.connect();
    let w = c.window("Hooked", 200, 150);
    let read = || std::fs::read_to_string(&log).unwrap_or_default();
    let wait_for = |want: &str| {
        let start = Instant::now();
        while !read().lines().any(|l| l == want) {
            assert!(start.elapsed() < Duration::from_secs(5), "waiting for {want:?}, have:\n{}", read());
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    wait_for("window-open|uwc.test|Hooked|1");
    wait_for("focus|uwc.test|Hooked|1");
    uwc.ok("view 2");
    wait_for("tag-change|2||");
    uwc.ok("view 1");
    wait_for("tag-change|1||");
    w.toplevel.destroy();
    w.xdg.destroy();
    w.surface.destroy();
    c.sync();
    wait_for("window-close|uwc.test|Hooked|1");
    // A bad hook name is a config error, not silence.
    let bad_cfg = std::env::temp_dir().join(format!("uwc-test-{}-hooks-bad.kdl", std::process::id()));
    std::fs::write(&bad_cfg, "hooks { sunrise \"x\" }").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_uwc")).args(["-t", "-c", bad_cfg.to_str().unwrap()]).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown hook `sunrise`"), "{:?}", out);
    let _ = std::fs::remove_file(&bad_cfg);
    let _ = std::fs::remove_file(&log);
}

#[test]
fn hot_zones_fire_once_when_the_pointer_rests_in_a_corner() {
    use wayland_protocols_wlr::virtual_pointer::v1::client::{
        zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1, zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    };
    let mut uwc = Uwc::start(
        "hot",
        Some("options { hot-zone-delay-ms 150 }\nhot-zones {\n top-left { view-tag 2; }\n bottom-right { view-tag 3; }\n}"),
    );
    assert!(uwc.state()["config_error"].is_null());
    let c = uwc.connect();
    let qh = c.queue.handle();
    let m: ZwlrVirtualPointerManagerV1 = c.globals.bind(&qh, 1..=2, ()).unwrap();
    let p: ZwlrVirtualPointerV1 = m.create_virtual_pointer(None, &qh, ());
    let mut c = c;
    let active = |uwc: &mut Uwc| {
        let s = uwc.state();
        s["monitors"][0]["tags"].as_array().unwrap().iter().position(|t| t["active"] == true).unwrap() + 1
    };
    p.motion_absolute(0, 500, 300, 1000, 600);
    p.frame();
    c.sync();
    assert_eq!(active(&mut uwc), 1);
    // Passing through the corner without resting does nothing.
    p.motion_absolute(1, 0, 0, 1000, 600);
    p.frame();
    p.motion_absolute(2, 500, 300, 1000, 600);
    p.frame();
    c.sync();
    std::thread::sleep(Duration::from_millis(350));
    assert_eq!(active(&mut uwc), 1, "no dwell, no action");
    // Resting in it fires.
    p.motion_absolute(3, 0, 0, 1000, 600);
    p.frame();
    c.sync();
    uwc.wait("the top-left zone to fire", |s| s["monitors"][0]["tags"][1]["active"] == true);
    // Staying does not fire again; going to the other corner fires that one.
    p.motion_absolute(4, 999, 599, 1000, 600);
    p.frame();
    c.sync();
    uwc.wait("the bottom-right zone to fire", |s| s["monitors"][0]["tags"][2]["active"] == true);
    // Bad zone names and actions are config errors.
    for bad in ["hot-zones { middle { zoom-reset; } }", "hot-zones { left { } }", "hot-zones { left { sing; } }"] {
        let f = std::env::temp_dir().join(format!("uwc-test-{}-hot.kdl", std::process::id()));
        std::fs::write(&f, bad).unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_uwc")).args(["-t", "-c", f.to_str().unwrap()]).output().unwrap();
        assert!(!out.status.success(), "{bad}");
        let _ = std::fs::remove_file(&f);
    }
}

#[test]
fn the_keyboard_layout_is_reported_switched_and_remembered_per_window() {
    let mut uwc = Uwc::start(
        "layout",
        Some("input { keyboard { xkb-layout \"us,de\" } }\noptions { per-window-layout true\n open-ms 0 }"),
    );
    assert!(uwc.state()["config_error"].is_null(), "{}", uwc.state()["config_error"]);
    let s = uwc.state();
    assert_eq!((s["keyboard_layout"]["index"].as_u64(), s["keyboard_layout"]["count"].as_u64()), (Some(0), Some(2)));
    assert!(s["keyboard_layout"]["name"].as_str().unwrap().contains("English"), "{}", s["keyboard_layout"]);
    uwc.ok("layout next");
    let s = uwc.state();
    assert_eq!(s["keyboard_layout"]["index"], 1);
    assert!(s["keyboard_layout"]["name"].as_str().unwrap().contains("German"), "{}", s["keyboard_layout"]);
    uwc.ok("layout 0");
    assert_eq!(uwc.state()["keyboard_layout"]["index"], 0);
    assert!(uwc.cmd("layout 2").get("error").is_some());
    assert!(uwc.cmd("layout sideways").get("error").is_some());
    // Each window keeps the layout it was typed in.
    let mut c = uwc.connect();
    let _a = c.window("Writer", 200, 150);
    let _b = c.window("Coder", 200, 150);
    let s = uwc.state();
    let (a, b) = (win(&s, "Writer")["id"].as_u64().unwrap(), win(&s, "Coder")["id"].as_u64().unwrap());
    uwc.ok(&format!("focus {a}"));
    uwc.ok("layout 1");
    uwc.ok(&format!("focus {b}"));
    assert_eq!(uwc.state()["keyboard_layout"]["index"], 0, "a window that never chose gets the first layout");
    uwc.ok(&format!("focus {a}"));
    assert_eq!(uwc.state()["keyboard_layout"]["index"], 1, "Writer is back in German");
}

#[test]
fn windows_animate_in_moves_and_tag_switches() {
    let mut uwc = Uwc::start("fx", Some("options { open-ms 300\nmove-ms 300\ntag-ms 300 }"));
    let mut c = uwc.connect();
    let _w = c.window("A", 300, 200);
    // Opening fades and grows in; the reported position is the resting one throughout.
    let s = uwc.wait("A", |s| !s["windows"].as_array().unwrap().is_empty());
    let id = win(&s, "A")["id"].as_u64().unwrap();
    assert_eq!((num(&win(&s, "A")["x"]), num(&win(&s, "A")["y"])), (350.0, 200.0));
    assert_eq!(s["monitors"][0]["animating"], true, "{s:#}");
    let s = uwc.wait("the open to finish", |s| s["monitors"][0]["animating"] == false);
    assert_eq!(num(&win(&s, "A")["x"]), 350.0);
    // A move by IPC reports its target at once and glides there.
    uwc.ok(&format!("move {id} 600 400"));
    let s = uwc.state();
    assert_eq!((num(&win(&s, "A")["x"]), num(&win(&s, "A")["y"])), (600.0, 400.0));
    assert_eq!(s["monitors"][0]["animating"], true);
    let s = uwc.wait("the glide to finish", |s| s["monitors"][0]["animating"] == false);
    assert_eq!(num(&win(&s, "A")["x"]), 600.0);
    // A window that comes into view with a tag switch fades in; going away is instant.
    uwc.ok("view 2");
    assert_eq!(uwc.state()["monitors"][0]["animating"], false);
    uwc.ok("view 1");
    assert_eq!(uwc.state()["monitors"][0]["animating"], true);
    uwc.wait("the fade to finish", |s| s["monitors"][0]["animating"] == false);
    // The options turn each off.
    uwc.ok("set move-ms 0");
    uwc.ok("set tag-ms 0");
    uwc.ok(&format!("move {id} 100 100"));
    uwc.ok("view 2");
    uwc.ok("view 1");
    assert_eq!(uwc.state()["monitors"][0]["animating"], false);
    uwc.ok("set open-ms 0");
    let _b = c.window("B", 200, 100);
    let s = uwc.wait("B", |s| s["windows"].as_array().unwrap().len() == 2);
    assert_eq!(s["monitors"][0]["animating"], false);
}

#[test]
fn closing_windows_fade_out() {
    // The fade itself needs a renderer (nested.py checks it pixel by pixel); headless skips the
    // ghost, but closing must behave with the option on: the window goes, focus falls back at
    // once (not after the fade), and nothing is left animating.
    let mut uwc = Uwc::start("close", Some("options { close-ms 300 }"));
    let mut c = uwc.connect();
    let wa = c.window("A", 300, 200);
    let _wb = c.window("B", 200, 100);
    let s = uwc.wait("two windows", |s| s["windows"].as_array().unwrap().len() == 2);
    let (ia, ib) = (win(&s, "A")["id"].as_u64().unwrap(), win(&s, "B")["id"].as_u64().unwrap());
    assert_eq!(uwc.cmd("get close-ms")["value"], 300.0);

    uwc.ok(&format!("close {ia}"));
    c.sync();
    c.dispatch_pending();
    wa.toplevel.destroy();
    wa.xdg.destroy();
    wa.surface.destroy();
    c.sync();
    let s = uwc.wait("A gone", |s| s["windows"].as_array().unwrap().len() == 1);
    assert_eq!(win(&s, "B")["focused"], true, "focus falls back at once, not after the fade");
    assert_eq!(win(&s, "B")["id"].as_u64().unwrap(), ib);
    uwc.wait("nothing left animating", |s| s["monitors"][0]["animating"] == false);
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
fn a_window_that_hides_itself_unmaps_and_comes_back_where_it_was() {
    let mut uwc = Uwc::start("unmap", None);
    let mut c = uwc.connect();
    let a = c.window("A", 300, 200);
    let _b = c.window("B", 300, 200);
    let s = uwc.wait("two", |s| s["windows"].as_array().unwrap().len() == 2);
    let (ia, ib) = (win(&s, "A")["id"].as_u64().unwrap(), win(&s, "B")["id"].as_u64().unwrap());
    let (ax, ay) = (win(&s, "A")["x"].clone(), win(&s, "A")["y"].clone());

    // A NULL buffer attach is the client hiding its window: it vanishes and focus moves on.
    uwc.ok(&format!("focus {ia}"));
    a.surface.attach(None, 0, 0);
    a.surface.commit();
    c.sync();
    let s = uwc.wait("A hidden", |s| s["windows"].as_array().unwrap().len() == 1);
    assert_eq!(win(&s, "B")["id"].as_u64().unwrap(), ib);
    assert_eq!(s["keyboard_focus"]["window"], ib, "focus falls to what is left");

    // Committing a buffer again maps it where it was, not centred, with the same id.
    a.surface.attach(Some(&a._buf), 0, 0);
    a.surface.commit();
    c.sync();
    let s = uwc.wait("A back", |s| s["windows"].as_array().unwrap().len() == 2);
    let w = win(&s, "A");
    assert_eq!((w["id"].clone(), w["x"].clone(), w["y"].clone()), (ia.into(), ax, ay));

    // The session keeps a window that hid itself, so it can be restored next time.
    uwc.ok("session save");
    let file =
        std::env::temp_dir().join(format!("uwc-test-{}-unmap", std::process::id())).join("state/uwc/session.json");
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(saved["windows"].as_array().unwrap().len(), 2);
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

const RULES: &str = r#"
rule {
    match title="Pip"
    open { pinned; size 320 180; position 20 20; focus false }
}
rule {
    match title="Later"
    open { tag 5 }
}
rule {
    match title="Hidden*"
    open { minimized }
}
rule {
    match title="Big"
    open { fullscreen }
}
rule {
    match app-id="uwc.*" focused=false
    exclude title="Pip"
    border-width 9
}
"#;

#[test]
fn open_rules_place_size_and_hide_new_windows() {
    let mut uwc = Uwc::start("open-rules", Some(RULES));
    let mut c = uwc.connect();
    let _main = c.window("Main", 300, 200);
    let main = win(&uwc.wait("Main", |s| !s["windows"].as_array().unwrap().is_empty()), "Main")["id"].clone();

    let _pip = c.window("Pip", 500, 500);
    let s = uwc.wait("Pip", |s| s["windows"].as_array().unwrap().len() == 2);
    let pip = win(&s, "Pip");
    assert_eq!(
        (pip["pinned"].clone(), pip["width"].clone(), pip["height"].clone()),
        (true.into(), 320.into(), 180.into())
    );
    assert_eq!((num(&pip["x"]), num(&pip["y"])), (20.0, 20.0), "a pinned window's position is a screen spot");
    assert_eq!(s["keyboard_focus"]["window"], main, "focus false keeps the keyboard where it was");

    let _later = c.window("Later", 100, 100);
    let s = uwc.wait("Later", |s| s["windows"].as_array().unwrap().len() == 3);
    assert_eq!(win(&s, "Later")["tags"], 16);
    assert_eq!(s["keyboard_focus"]["window"], main, "a window opening out of sight does not take the keyboard");

    let _hidden = c.window("Hidden scratch", 100, 100);
    let s = uwc.wait("Hidden", |s| s["windows"].as_array().unwrap().len() == 4);
    assert_eq!(win(&s, "Hidden scratch")["minimized"], true);

    let _big = c.window("Big", 100, 100);
    let s = uwc.wait("Big", |s| s["windows"].as_array().unwrap().len() == 5);
    let big = win(&s, "Big");
    assert_eq!(
        (big["fullscreen"].clone(), big["width"].clone(), big["height"].clone()),
        (true.into(), 1000.into(), 600.into()),
        "fullscreen from the very first configure"
    );
}

#[test]
fn decoration_rules_follow_window_state_live() {
    let mut uwc = Uwc::start("deco-rules", Some(RULES));
    let mut c = uwc.connect();
    let _a = c.window("A", 300, 200);
    let _b = c.window("B", 300, 200);
    let s = uwc.wait("two", |s| s["windows"].as_array().unwrap().len() == 2);
    let (a, b) = (win(&s, "A")["id"].as_u64().unwrap(), win(&s, "B")["id"].as_u64().unwrap());
    let bw = |uwc: &mut Uwc, id: u64| uwc.cmd(&format!("get-window {id}"))["options"]["border-width"].clone();
    let base = uwc.cmd("get border-width")["value"].clone();
    assert_ne!(base, 9);
    // B has focus; the focused=false rule thickens A's border only.
    assert_eq!((bw(&mut uwc, a), bw(&mut uwc, b)), (9.into(), base.clone()));
    uwc.ok(&format!("focus {a}"));
    assert_eq!((bw(&mut uwc, a), bw(&mut uwc, b)), (base, 9.into()), "the rule follows focus");

    // Rules over IPC: later ones win, and `rule clear` / reload drop them.
    uwc.ok(r#"rule { match title="A"; opacity 0.5; border-width 3 }"#);
    let o = uwc.cmd(&format!("get-window {a}"))["options"].clone();
    assert_eq!((o["opacity"].clone(), o["border-width"].clone()), (0.5.into(), 3.into()));
    assert_eq!(uwc.cmd("rules"), serde_json::json!({"event": "rules", "file": 5, "ipc": 1}));
    for bad in [r#"rule { match app-id=1 }"#, "rule { sloppy-focus false }", "rule nonsense", "rule { opacity 9 }"] {
        assert!(uwc.cmd(bad).get("error").is_some(), "should refuse: {bad}");
    }
    uwc.ok("rule clear");
    assert_eq!(uwc.cmd(&format!("get-window {a}"))["options"]["opacity"], 1.0);
    uwc.ok(r#"rule { opacity 0.7 }"#);
    uwc.ok("reload");
    assert_eq!(uwc.cmd("rules")["ipc"], 0, "reload drops IPC rules");
}

#[test]
fn bookmarks_remember_views_per_tag() {
    let mut uwc = Uwc::start("bookmarks", None);
    uwc.ok("panto 1000 -500");
    uwc.ok("zoom 0.5");
    uwc.ok("bookmark save 1");
    let saved = uwc.state()["monitors"][0]["viewport"].clone();
    uwc.ok("home");
    uwc.ok("bookmark goto 1");
    assert_eq!(uwc.state()["monitors"][0]["viewport"], saved);
    let bm = uwc.state()["monitors"][0]["tags"][0]["bookmarks"].clone();
    assert_eq!(bm.as_array().unwrap().len(), 1);
    assert_eq!(bm[0]["n"], 1);
    // Another tag has its own.
    uwc.ok("view 2");
    assert!(uwc.cmd("bookmark goto 1").get("error").is_some());
    for bad in ["bookmark goto 0", "bookmark goto 10", "bookmark jump 1", "bookmark save"] {
        assert!(uwc.cmd(bad).get("error").is_some(), "should refuse: {bad}");
    }
}

#[test]
fn uwc_msg_talks_to_the_compositor() {
    let mut uwc = Uwc::start("msg", None);
    let sock = uwc.dir.join(format!("uwc-{}.sock", uwc.display));
    let msg = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_uwc")).arg("msg").args(args).env("UWC_SOCKET", &sock).output().unwrap()
    };
    let out = msg(&["state"]);
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["event"], "state");
    assert_eq!(String::from_utf8_lossy(&out.stdout).lines().count(), 1, "one JSON line");

    // Arguments are joined into the command line; a shell-unquoted color is a bare KDL string.
    assert!(msg(&["set", "focus-color", "#ff0000"]).status.success());
    assert_eq!(uwc.cmd("get focus-color")["value"], "#ff0000ff");
    assert!(msg(&["rule", "{ match app-id=\"mpv\"; opacity 0.9 }"]).status.success());
    assert_eq!(uwc.cmd("rules")["ipc"], 1);

    // Errors: caught locally before connecting, or reported by the compositor; both exit 1.
    let bad = msg(&["pan", "x"]);
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("not a number"));
    let bad = msg(&["focus", "999"]);
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("no window 999"));

    let pretty = msg(&["-p", "get", "corner-radius"]);
    assert!(String::from_utf8_lossy(&pretty.stdout).lines().count() > 1);
    let help = msg(&["--help"]);
    assert!(String::from_utf8_lossy(&help.stdout).contains("bookmark save N"));

    // subscribe streams: the first snapshot arrives, then one per change.
    let mut sub = Command::new(env!("CARGO_BIN_EXE_uwc"))
        .args(["msg", "subscribe"])
        .env("UWC_SOCKET", &sock)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(sub.stdout.take().unwrap()).lines();
    let first: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(first["event"], "state");
    uwc.ok("pan 7 0");
    let next: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(next["monitors"][0]["viewport"]["x"], 7);
    let _ = sub.kill();
    let _ = sub.wait();
}

#[test]
fn zoomed_windows_are_asked_to_render_sharper() {
    let mut uwc = Uwc::start("sharp-zoom", Some("options { zoom-settle-ms 60 }"));
    let mut c = uwc.connect();
    let w = c.window("A", 300, 200);
    uwc.wait("A", |s| !s["windows"].as_array().unwrap().is_empty());
    let qh = c.queue.handle();
    let mgr: wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1 = c.globals.bind(&qh, 1..=1, ()).unwrap();
    let _fs = mgr.get_fractional_scale(&w.surface, &qh, ());
    c.sync();
    assert_eq!(c.app.frac, Some(120), "scale 1 at zoom 1");

    let settle = |c: &mut Client| {
        std::thread::sleep(Duration::from_millis(150));
        c.sync();
    };
    uwc.ok("set view-ms 0"); // these are about render scale, not easing
    uwc.ok("zoom 2");
    c.sync();
    assert_eq!(c.app.frac, Some(120), "not while the zoom may still be moving");
    settle(&mut c);
    assert_eq!(c.app.frac, Some(240), "2x once it settled");
    assert_eq!(c.app.buffer_scale, Some(2), "and the integer preferred buffer scale for older clients");

    uwc.ok("zoom 8");
    settle(&mut c);
    assert_eq!(c.app.frac, Some(360), "capped at zoom-render-scale-max 3");
    uwc.ok("zoom 0.5");
    settle(&mut c);
    assert_eq!(c.app.frac, Some(120), "zoomed out: no lower than zoom-render-scale-min 1");
    uwc.ok("set zoom-render-scale-min 0.5");
    settle(&mut c);
    assert_eq!(c.app.frac, Some(60), "unless that allows it");

    // A pinned window is not zoomed, so it renders at the output scale.
    let id = win(&uwc.state(), "A")["id"].as_u64().unwrap();
    uwc.ok(&format!("pin {id}"));
    settle(&mut c);
    assert_eq!(c.app.frac, Some(120));
}

#[test]
fn windows_have_their_own_scale_on_the_canvas() {
    let mut uwc = Uwc::start(
        "window-scale",
        Some("options { zoom-settle-ms 40 }\nrule { match title=\"Small\"; open { scale 0.5 } }"),
    );
    let mut c = uwc.connect();
    let w = c.window("A", 300, 200);
    let s = uwc.wait("A", |s| !s["windows"].as_array().unwrap().is_empty());
    let a = win(&s, "A").clone();
    let id = a["id"].as_u64().unwrap();
    assert_eq!((num(&a["x"]), num(&a["y"]), num(&a["scale"])), (350.0, 200.0, 1.0));

    // Half size around its centre (500, 300): the footprint is 150x100 at (425, 250).
    uwc.ok(&format!("window-scale {id} 0.5"));
    let a = win(&uwc.state(), "A").clone();
    assert_eq!((num(&a["x"]), num(&a["y"]), num(&a["scale"])), (425.0, 250.0, 0.5));
    assert_eq!((num(&a["width"]), num(&a["height"])), (300.0, 200.0), "the client keeps its size");

    // It renders for the scale it is shown at: view zoom x its own scale.
    let qh = c.queue.handle();
    let mgr: wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1 = c.globals.bind(&qh, 1..=1, ()).unwrap();
    let _fs = mgr.get_fractional_scale(&w.surface, &qh, ());
    uwc.ok("set view-ms 0"); // these are about render scale, not easing
    uwc.ok("zoom 4");
    std::thread::sleep(Duration::from_millis(120));
    c.sync();
    assert_eq!(c.app.frac, Some(240), "zoom 4 x scale 0.5 = 2");

    for bad in [format!("window-scale {id} 9"), format!("window-scale {id} 0"), "window-scale 999 1".to_string()] {
        assert!(uwc.cmd(&bad).get("error").is_some(), "should refuse: {bad}");
    }
    uwc.ok(&format!("fullscreen {id}"));
    assert!(uwc.cmd(&format!("window-scale {id} 1")).get("error").is_some(), "not while fullscreen");

    let _small = c.window("Small", 200, 100);
    let s = uwc.wait("Small", |s| s["windows"].as_array().unwrap().len() == 2);
    assert_eq!(num(&win(&s, "Small")["scale"]), 0.5, "open {{ scale }} from a rule");
}

#[test]
fn saving_a_shader_file_reloads_it() {
    let mut uwc = Uwc::start("shader-watch", Some("options { background-shader \"bg.glsl\" }"));
    // Relative paths are next to the config file.
    std::fs::write(
        uwc.dir.join("bg.glsl"),
        "vec4 background(vec2 p, vec2 s, float t, vec2 v, float z) { return vec4(1.0); }\n",
    )
    .unwrap();
    uwc.ok("reload");
    let rev = num(&uwc.state()["config_rev"]);
    std::thread::sleep(Duration::from_millis(1100)); // the mtime has 1 s resolution on some filesystems
    std::fs::write(
        uwc.dir.join("bg.glsl"),
        "vec4 background(vec2 p, vec2 s, float t, vec2 v, float z) { return vec4(0.5); }\n",
    )
    .unwrap();
    uwc.wait("reload after the shader changed", |s| num(&s["config_rev"]) > rev);
    assert_eq!(uwc.cmd("get background-shader")["value"], "bg.glsl");
}

#[test]
fn the_canvas_comes_back_after_a_restart() {
    let state = std::env::temp_dir().join(format!("uwc-test-{}-session-state", std::process::id()));
    let file = state.join("uwc/session.json");
    let where_a;
    {
        let mut uwc = Uwc::start_with_state("session1", None, &state);
        let mut c = uwc.connect();
        let _a = c.window("A", 300, 200);
        let _b = c.window("B", 200, 100);
        let s = uwc.wait("two", |s| s["windows"].as_array().unwrap().len() == 2);
        let (a, b) = (win(&s, "A")["id"].as_u64().unwrap(), win(&s, "B")["id"].as_u64().unwrap());
        uwc.ok(&format!("move {a} 1234 567"));
        uwc.ok(&format!("window-scale {a} 0.75"));
        // Scaling keeps the centre, so the corner moved: remember where it is now.
        let s = uwc.state();
        where_a = (num(&win(&s, "A")["x"]), num(&win(&s, "A")["y"]));
        uwc.ok(&format!("pin {b}"));
        uwc.ok("panto 40 50");
        uwc.ok("bookmark save 2");
        uwc.ok("view 3");
        uwc.ok("panto 500 -200");
        uwc.ok("frame add 100 100 600 400 notes");
        uwc.ok("session save");
        assert!(file.exists());
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(saved["windows"].as_array().unwrap().len(), 2);
        uwc.ok("quit");
        let start = Instant::now();
        while uwc.child.try_wait().unwrap().is_none() {
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    let mut uwc = Uwc::start_with_state("session2", None, &state);
    let s = uwc.state();
    let m = &s["monitors"][0];
    assert_eq!(m["tags"][2]["active"], true, "the tags in view come back");
    assert_eq!((num(&m["viewport"]["x"]), num(&m["viewport"]["y"])), (500.0, -200.0), "and their views");
    assert_eq!(m["tags"][0]["bookmarks"][0]["n"], 2, "and the bookmarks");
    assert_eq!(s["frames"][0]["label"], "notes", "and the frames");

    let mut c = uwc.connect();
    let _a = c.window("A", 640, 480);
    let s = uwc.wait("A", |s| !s["windows"].as_array().unwrap().is_empty());
    let a = win(&s, "A");
    assert_eq!((num(&a["x"]), num(&a["y"]), num(&a["scale"])), (where_a.0, where_a.1, 0.75), "where it was, its scale");
    assert_eq!((num(&a["width"]), num(&a["height"])), (300.0, 200.0), "its size, in the first configure");
    assert_eq!(a["tags"], 1);
    let _b = c.window("B", 200, 100);
    let s = uwc.wait("B", |s| s["windows"].as_array().unwrap().len() == 2);
    assert_eq!(win(&s, "B")["pinned"], true);
    // Nothing remembered for this one: it opens as usual.
    let _c2 = c.window("C", 100, 100);
    let s = uwc.wait("C", |s| s["windows"].as_array().unwrap().len() == 3);
    assert_eq!(win(&s, "C")["tags"], 4, "on the tag in view");

    uwc.ok("session forget");
    assert!(!file.exists());
    assert!(uwc.cmd("session").get("error").is_some());
    drop(uwc);
    let _ = std::fs::remove_dir_all(&state);
}

#[test]
fn taskbars_list_and_control_windows() {
    const ACTIVATED: u32 = 2;
    const MINIMIZED: u32 = 1;
    const FULLSCREEN: u32 = 3;
    let mut uwc = Uwc::start("foreign-toplevel", None);
    let mut c = uwc.connect();
    let wa = c.window("A", 300, 200);
    let _wb = c.window("B", 300, 200);
    let s = uwc.wait("two", |s| s["windows"].as_array().unwrap().len() == 2);
    let (a, b) = (win(&s, "A")["id"].as_u64().unwrap(), win(&s, "B")["id"].as_u64().unwrap());
    let qh = c.queue.handle();
    let seat: wayland_client::protocol::wl_seat::WlSeat = c.globals.bind(&qh, 1..=7, ()).unwrap();
    let _o: wl_output::WlOutput = c.globals.bind(&qh, 1..=4, ()).unwrap();
    let _m: tl_manager::ZwlrForeignToplevelManagerV1 = c.globals.bind(&qh, 1..=3, ()).unwrap();
    c.sync();

    // The list, at once on bind.
    assert_eq!(c.app.tl.len(), 2);
    let (ha, ia) = c.tl("A");
    let (hb, ib) = c.tl("B");
    assert_eq!((ia.app_id.as_str(), ia.outputs, ia.done >= 1), ("uwc.test", 1, true));
    assert!(ib.states.contains(&ACTIVATED) && !ia.states.contains(&ACTIVATED), "B has focus");

    // A taskbar click.
    ha.activate(&seat);
    c.sync();
    let s = uwc.state();
    assert_eq!(s["keyboard_focus"]["window"], a);
    c.sync();
    assert!(c.tl("A").1.states.contains(&ACTIVATED) && !c.tl("B").1.states.contains(&ACTIVATED), "the bar is told");

    ha.set_minimized();
    c.sync();
    assert_eq!(win(&uwc.state(), "A")["minimized"], true);
    c.sync();
    assert!(c.tl("A").1.states.contains(&MINIMIZED));
    ha.activate(&seat);
    c.sync();
    assert_eq!(win(&uwc.state(), "A")["minimized"], false, "activating a minimized window restores it");

    hb.set_fullscreen(None);
    c.sync();
    assert_eq!(win(&uwc.state(), "B")["fullscreen"], true);
    c.sync();
    assert!(c.tl("B").1.states.contains(&FULLSCREEN));

    // Close asks the app; when its window goes, the handle is closed.
    hb.close();
    c.sync();
    c.dispatch_pending();
    assert_eq!(c.app.closed.len(), 1);
    wa.toplevel.destroy();
    wa.xdg.destroy();
    wa.surface.destroy();
    c.sync();
    uwc.wait("A gone", |s| s["windows"].as_array().unwrap().len() == 1);
    c.sync();
    assert!(c.tl("A").1.closed);

    // A window opened later shows up.
    let _wc = c.window("C", 100, 100);
    uwc.wait("C", |s| s["windows"].as_array().unwrap().len() == 2);
    c.sync();
    assert_eq!(c.tl("C").1.title, "C");
    let _ = b;
}

#[test]
fn window_previews_offer_the_window_and_fail_cleanly_without_a_renderer() {
    let mut uwc = Uwc::start("export", None);
    let mut c = uwc.connect();
    let _a = c.window("A", 300, 200);
    let s = uwc.wait("A", |s| !s["windows"].as_array().unwrap().is_empty());
    let a = win(&s, "A")["id"].as_u64().unwrap() as u32;
    let qh = c.queue.handle();
    let m: export_manager::HyprlandToplevelExportManagerV1 = c.globals.bind(&qh, 1..=2, ()).unwrap();

    let frame = m.capture_toplevel(0, a, &qh, ());
    c.sync();
    // ARGB8888 (0) at the window's size, stride 4 per pixel, then buffer_done (v2).
    assert_eq!(c.app.export_buffer, Some((0, 300, 200, 1200)));
    assert!(c.app.export_done);
    let buf = c.buffer("cap", 300, 200);
    frame.copy(&buf, 1);
    c.sync();
    assert!(c.app.export_failed && !c.app.export_ready, "headless has no renderer: failed, not a hang");

    c.app.export_failed = false;
    let _f2 = m.capture_toplevel(0, 9999, &qh, ());
    c.sync();
    assert!(c.app.export_failed, "an unknown window fails at once");
}

#[test]
fn screencopy_offers_the_output_and_fails_cleanly_without_a_renderer() {
    let uwc = Uwc::start("screencopy", None);
    let mut c = uwc.connect();
    let qh = c.queue.handle();
    let out: wl_output::WlOutput = c.globals.bind(&qh, 1..=4, ()).unwrap();
    let m: sc_manager::ZwlrScreencopyManagerV1 = c.globals.bind(&qh, 1..=3, ()).unwrap();
    let frame = m.capture_output(0, &out, &qh, ());
    c.sync();
    assert_eq!(c.app.export_buffer, Some((0, 1000, 600, 4000)), "the whole output, ARGB8888");
    assert!(c.app.export_done, "buffer_done (v3)");
    let buf = c.buffer("sc", 1000, 600);
    frame.copy(&buf);
    c.sync();
    assert!(c.app.export_failed && !c.app.export_ready, "headless: failed, not a hang");

    // A region, in logical px.
    c.app.export_buffer = None;
    let _f = m.capture_output_region(0, &out, 10, 20, 300, 200, &qh, ());
    c.sync();
    assert_eq!(c.app.export_buffer, Some((0, 300, 200, 1200)));
    drop(uwc);
}

#[test]
fn activation_focuses_a_window_and_flags_an_unseen_one_urgent() {
    let mut uwc = Uwc::start("activate", None);
    let mut a = uwc.connect();
    let wa = a.window("A", 300, 200);
    let mut b = uwc.connect();
    let _wb = b.window("B", 200, 100);
    let s = uwc.wait("two windows", |s| s["windows"].as_array().unwrap().len() == 2);
    assert_eq!(win(&s, "B")["focused"], true, "the newest window takes focus");

    // B (a launcher) hands A a token; A activates itself with it and takes the focus.
    let act_b = b.activation();
    let token = b.get_token(&act_b);
    let act_a = a.activation();
    act_a.activate(token, &wa.surface);
    a.sync();
    uwc.wait("A focused by the activation", |s| win(s, "A")["focused"] == true);

    // A window on an unseen tag cannot be focused: the request flags it urgent instead, and
    // focusing it (which reveals it) answers the request.
    uwc.ok("set tag-ms 0");
    uwc.ok("view 2");
    let wc = a.window("C", 100, 100);
    uwc.ok("view 1");
    let s = uwc.state();
    assert_eq!(win(&s, "C")["urgent"], false);
    let token = b.get_token(&act_b);
    act_a.activate(token, &wc.surface);
    a.sync();
    let s = uwc.wait("C flagged urgent", |s| win(s, "C")["urgent"] == true);
    let ic = win(&s, "C")["id"].as_u64().unwrap();
    uwc.ok(&format!("focus {ic}"));
    let s = uwc.wait("C revealed and focused", |s| win(s, "C")["focused"] == true);
    assert_eq!(win(&s, "C")["urgent"], false, "focus answers the request");
}

#[test]
fn idle_notifications_fire_unless_inhibited() {
    let mut uwc = Uwc::start("idle", None);
    let mut c = uwc.connect();
    let w = c.window("A", 200, 100);
    uwc.wait("A", |s| !s["windows"].as_array().unwrap().is_empty());
    let seat = c.bind_seat();
    let notifier = c.idle_notifier();

    // No input happens, so a notification fires once its timeout passes.
    let _n1 = c.idle_notification(&notifier, &seat, 150, 1);
    std::thread::sleep(Duration::from_millis(600));
    c.sync();
    assert_eq!(c.app.idle_fired, vec![1], "the idle notification fired");

    // While an inhibitor lives (a video player), nothing fires...
    let manager = c.idle_inhibit_manager();
    let qh = c.queue.handle();
    let inhibitor = manager.create_inhibitor(&w.surface, &qh, ());
    c.sync();
    let _n2 = c.idle_notification(&notifier, &seat, 150, 2);
    std::thread::sleep(Duration::from_millis(600));
    c.sync();
    assert!(!c.app.idle_fired.contains(&2), "an inhibitor pauses the notifications");

    // ...and when it goes, they fire again.
    inhibitor.destroy();
    c.sync();
    std::thread::sleep(Duration::from_millis(600));
    c.sync();
    assert!(c.app.idle_fired.contains(&2), "the notification fires once the inhibitor is gone");
}

#[test]
fn popup_grabs_are_taken_and_end_cleanly() {
    let mut uwc = Uwc::start("grab", None);
    let mut c = uwc.connect();
    let w = c.window("A", 300, 200);
    uwc.wait("A", |s| !s["windows"].as_array().unwrap().is_empty());
    let seat = c.bind_seat();

    // A grabbed popup (a menu): the grab is taken without a protocol error.
    let popup = c.popup(&w, &seat, true);
    assert!(!c.app.popup_done);
    // The window can still be driven over IPC while the grab is held.
    assert_eq!(uwc.state()["windows"].as_array().unwrap().len(), 1);

    // The client dismissing its own popup ends the grab (popup_done, no error).
    popup.destroy();
    w.surface.commit();
    c.sync();
    uwc.ok("close 1");
    c.sync();
    assert_eq!(c.app.closed.len(), 1, "client is asked to close");
    w.toplevel.destroy();
    w.xdg.destroy();
    w.surface.destroy();
    c.sync();
    let s = uwc.wait("the window gone", |s| s["windows"].as_array().unwrap().is_empty());
    assert_eq!(s["monitors"][0]["animating"], false);
}

#[test]
fn multiple_monitors_share_the_canvas() {
    let mut uwc = Uwc::start_screens("multi", "1000x600,1000x600");
    let mut c = uwc.connect();
    let _a = c.window("A", 300, 200);
    let s = uwc.wait("A", |s| !s["windows"].as_array().unwrap().is_empty());

    // Two outputs tiled left to right; the pointer's monitor is the focused one.
    let ms = s["monitors"].as_array().unwrap();
    assert_eq!(ms.len(), 2);
    assert_eq!((num(&ms[0]["width"]), num(&ms[1]["x"])), (1000.0, 1000.0));
    assert_eq!(ms[0]["focused"], true);
    assert_eq!(ms[1]["focused"], false);

    // A pan moves only the pointer's monitor's view; the window follows the monitor that
    // shows it.
    uwc.ok("pan 700 0");
    let s = uwc.wait("monitor 1 panned", |s| s["monitors"][0]["viewport"]["x"] == 700.0);
    assert_eq!(s["monitors"][1]["viewport"]["x"], 0.0, "the other monitor keeps its view");
    uwc.wait("the pan to settle", |s| s["monitors"][0]["view_moving"] == false);
    assert_eq!(win(&uwc.state(), "A")["monitor"], "HEADLESS-2");
}

#[test]
fn a_session_lock_covers_the_screen_and_unlocks() {
    let mut uwc = Uwc::start("lock", None);
    let mut c = uwc.connect();
    let _w = c.window("A", 300, 200);
    let s = uwc.wait("A", |s| !s["windows"].as_array().unwrap().is_empty());
    let id = win(&s, "A")["id"].clone();

    // A lock screen client: the lock is confirmed once it covers the output, and it takes
    // the keyboard (no window has focus while the session is locked).
    let output = c.bind_output();
    let lock = c.lock(&output);
    c.wait_lock("locked");
    assert_eq!(uwc.state()["keyboard_focus"]["window"], serde_json::Value::Null);

    // Unlocking ends the lock: the window has the keyboard again.
    lock.unlock_and_destroy();
    c.sync();
    let s = uwc.wait("unlocked", |s| s["keyboard_focus"]["window"] == id);
    assert_eq!(s["keyboard_focus"]["window"], id);
}

#[test]
fn pointer_constraints_activate_over_the_window() {
    let mut uwc = Uwc::start("constraints", None);
    let mut c = uwc.connect();
    let w = c.window("A", 300, 200);
    uwc.wait("A", |s| !s["windows"].as_array().unwrap().is_empty());
    let seat = c.bind_seat();
    let qh = c.queue.handle();
    let manager = c
        .globals
        .bind::<wayland_protocols::wp::pointer_constraints::zv1::client::zwp_pointer_constraints_v1::ZwpPointerConstraintsV1, _, _>(&qh, 1..=1, ())
        .expect("zwp_pointer_constraints_v1");

    let pointer = seat.get_pointer(&qh, ());
    use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_pointer_constraints_v1::Lifetime;

    // The pointer sits over the window, so a confinement activates right away.
    let confined = manager.confine_pointer(&w.surface, &pointer, None, Lifetime::Oneshot, &qh, ());
    c.wait_constraint("confined");
    confined.destroy();
    c.sync();

    // A lock takes over: the cursor is held still and the client reads the deltas.
    let _locked = manager.lock_pointer(&w.surface, &pointer, None, Lifetime::Oneshot, &qh, ());
    c.wait_constraint("locked");
}

#[test]
fn text_input_follows_the_keyboard_focus() {
    let mut uwc = Uwc::start("textinput", None);
    let mut c = uwc.connect();
    let seat = c.bind_seat();
    let qh = c.queue.handle();
    let manager = c
        .globals
        .bind::<zwp_text_input_manager_v3::ZwpTextInputManagerV3, _, _>(&qh, 1..=1, ())
        .expect("zwp_text_input_manager_v3");

    // Like a toolkit: the text input exists before the window takes the keyboard, and the
    // compositor enters it when the focus arrives.
    let _ti = manager.get_text_input(&seat, &qh, ());
    let _w = c.window("A", 300, 200);
    uwc.wait("ti enter", |s| !s["windows"].as_array().unwrap().is_empty());
    c.sync();
    assert!(c.app.ti_enter, "text input entered the focused window");

    // Closing the window takes the keyboard, and the text input leaves with it.
    uwc.ok("close 1");
    c.wait_ti_leave();
    assert!(c.app.ti_leave, "text input left with the focus");
}
