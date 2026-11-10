//! Compositor state: the window list, tags, viewport, focus. Protocol handlers live in `handlers.rs`,
//! input in `input.rs`, drawing in `render.rs`, the control socket in `ipc.rs`.

use std::{
    ffi::OsString,
    process::{Command, Stdio},
    sync::Arc,
    time::Instant,
};

use smithay::{
    backend::renderer::element::Id,
    desktop::{PopupManager, Window},
    input::{
        keyboard::{KeyboardHandle, XkbConfig},
        pointer::{CursorImageStatus, PointerHandle},
        Seat, SeatState,
    },
    output::Output,
    reexports::{
        calloop::{LoopHandle, LoopSignal},
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
            Display, DisplayHandle,
        },
    },
    utils::{Logical, Point, Rectangle, Size},
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        dmabuf::{DmabufGlobal, DmabufState},
        output::OutputManagerState,
        selection::data_device::DataDeviceState,
        shell::{
            wlr_layer::WlrLayerShellState,
            xdg::{decoration::XdgDecorationState, XdgShellState},
        },
        shm::ShmState,
        socket::ListeningSocketSource,
    },
};

use crate::{
    backend::Backend,
    canvas::{lowest_tag, Viewport, ALLTAGS, TAGCOUNT},
    config::{Action, Config, Dir},
    ipc::Ipc,
};

pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _: ClientId) {}
    fn disconnected(&self, _: ClientId, _: DisconnectReason) {}
}

/// A toplevel window and where it sits on the canvas.
pub struct Client {
    pub id: u32,
    pub window: Window,
    /// Top-left of the window geometry in canvas px (what clients are told and IPC reports).
    pub canvas: Point<f64, Logical>,
    pub tags: u32,
    /// False until the client has committed a first buffer.
    pub mapped: bool,
    /// Wants attention (set by xdg-activation once that exists).
    pub urgent: bool,
    /// Ids for the four border rects, stable so the damage tracker can tell frames apart.
    pub border_ids: [Id; 4],
    /// Ids for the dim, backdrop, border and shadow shader elements.
    pub deco_ids: [Id; 4],
}

impl Client {
    pub fn size(&self) -> Size<i32, Logical> {
        self.window.geometry().size
    }

    /// Where the surface origin goes: the geometry may start inside the surface (client shadows).
    pub fn surface_origin(&self) -> Point<f64, Logical> {
        let g = self.window.geometry().loc;
        (self.canvas.x - g.x as f64, self.canvas.y - g.y as f64).into()
    }

    pub fn rect(&self) -> Rectangle<f64, Logical> {
        let s = self.size();
        Rectangle::new(self.canvas, (s.w as f64, s.h as f64).into())
    }

    pub fn surface(&self) -> &WlSurface {
        self.window.toplevel().expect("wayland window").wl_surface()
    }

    pub fn title(&self) -> String {
        smithay::wayland::compositor::with_states(self.surface(), |s| {
            s.data_map
                .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().ok().and_then(|d| d.title.clone()))
        })
        .unwrap_or_default()
    }

    pub fn app_id(&self) -> String {
        smithay::wayland::compositor::with_states(self.surface(), |s| {
            s.data_map
                .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().ok().and_then(|d| d.app_id.clone()))
        })
        .unwrap_or_default()
    }
}

/// What a pointer drag with Mod held is doing.
#[derive(Debug, Clone, Copy)]
pub enum Drag {
    Move { id: u32, grab: Point<f64, Logical>, orig: Point<f64, Logical> },
    Resize { id: u32, start: Point<f64, Logical>, orig: Size<i32, Logical> },
    Pan,
}

pub struct Monitor {
    pub output: Output,
    /// Logical size in px (the screen the viewport looks through).
    pub size: Size<i32, Logical>,
    pub tagset: u32,
    pub prev_tagset: u32,
    pub vp: [Viewport; TAGCOUNT],
    /// The view before the overview was opened.
    pub overview: Option<Viewport>,
}

impl Monitor {
    pub fn viewport(&self) -> Viewport {
        self.vp[lowest_tag(self.tagset)]
    }

    pub fn viewport_mut(&mut self) -> &mut Viewport {
        &mut self.vp[lowest_tag(self.tagset)]
    }
}

pub struct Uwc {
    pub start_time: Instant,
    pub display_handle: DisplayHandle,
    pub loop_handle: LoopHandle<'static, Uwc>,
    pub loop_signal: LoopSignal,
    pub socket_name: OsString,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    #[allow(dead_code)] // held so the global stays alive
    pub xdg_decoration_state: XdgDecorationState,
    #[allow(dead_code)]
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Uwc>,
    pub data_device_state: DataDeviceState,
    pub popups: PopupManager,
    pub seat: Seat<Uwc>,
    pub layer_shell_state: WlrLayerShellState,
    pub workspaces: crate::workspace::WorkspaceState,
    /// The layer surface that has the keyboard (a launcher, a lock screen), over any window focus.
    pub layer_focus: Option<WlSurface>,
    pub dmabuf_state: DmabufState,
    /// linux-drm-syncobj-v1 (explicit sync), when the GPU supports it.
    pub syncobj_state: Option<smithay::wayland::drm_syncobj::DrmSyncobjState>,
    pub dmabuf_global: Option<DmabufGlobal>,
    pub backend: Backend,

    pub config: Config,
    pub config_path: Option<std::path::PathBuf>,
    pub config_error: Option<String>,
    pub config_rev: u32,

    /// Bottom to top.
    pub clients: Vec<Client>,
    pub focus: Option<u32>,
    pub next_id: u32,
    pub mon: Option<Monitor>,
    /// Pointer position on the screen (logical px).
    pub cursor: Point<f64, Logical>,
    /// What the client under the pointer asked the pointer to look like.
    pub cursor_status: CursorImageStatus,
    /// The xcursor theme, loaded lazily per image.
    pub cursor_theme: crate::cursor::CursorTheme,
    pub drag: Option<Drag>,
    /// Pointer position at the previous step of a pan drag.
    pub drag_last: Point<f64, Logical>,
    /// Bumped on anything that changes how decorations look, so the damage tracker redraws them.
    pub deco_commit: smithay::backend::renderer::utils::CommitCounter,
    /// Set by anything that needs a new frame.
    pub redraw: bool,
    /// Set while drawing a frame that is part of an animation: the backend asks for the next one.
    pub animating: bool,
    pub ipc: Ipc,
}

impl Uwc {
    pub fn new(
        display: Display<Uwc>,
        loop_handle: LoopHandle<'static, Uwc>,
        loop_signal: LoopSignal,
        config: Config,
        config_path: Option<std::path::PathBuf>,
    ) -> anyhow::Result<Uwc> {
        let dh = display.handle();
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&dh, "seat0");
        seat.add_keyboard(Default::default(), 200, 25)?;
        seat.add_pointer();

        let socket_name = Self::init_socket(&loop_handle)?;

        // The display itself is polled by the event loop so clients' requests are dispatched.
        loop_handle
            .insert_source(
                smithay::reexports::calloop::generic::Generic::new(
                    display,
                    smithay::reexports::calloop::Interest::READ,
                    smithay::reexports::calloop::Mode::Level,
                ),
                |_, display, state| {
                    // Safety: the display is not dropped while it is being dispatched.
                    unsafe { display.get_mut().dispatch_clients(state).unwrap() };
                    Ok(smithay::reexports::calloop::PostAction::Continue)
                },
            )
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let ipc = Ipc::new(&loop_handle, &socket_name)?;

        Ok(Uwc {
            start_time: Instant::now(),
            compositor_state: CompositorState::new::<Uwc>(&dh),
            xdg_shell_state: XdgShellState::new::<Uwc>(&dh),
            shm_state: ShmState::new::<Uwc>(&dh, vec![]),
            xdg_decoration_state: XdgDecorationState::new::<Uwc>(&dh),
            output_manager_state: OutputManagerState::new_with_xdg_output::<Uwc>(&dh),
            data_device_state: DataDeviceState::new::<Uwc>(&dh),
            popups: PopupManager::default(),
            layer_shell_state: WlrLayerShellState::new::<Uwc>(&dh),
            workspaces: crate::workspace::WorkspaceState::new(&dh),
            layer_focus: None,
            dmabuf_state: DmabufState::new(),
            syncobj_state: None,
            dmabuf_global: None,
            backend: Backend::None,
            cursor_status: CursorImageStatus::default_named(),
            cursor_theme: crate::cursor::CursorTheme::new(&config.cursor_theme, config.cursor_size),
            seat_state,
            seat,
            display_handle: dh,
            loop_handle,
            loop_signal,
            socket_name,
            config,
            config_path,
            config_error: None,
            config_rev: 1,
            clients: Vec::new(),
            focus: None,
            next_id: 1,
            mon: None,
            cursor: (0.0, 0.0).into(),
            drag: None,
            drag_last: (0.0, 0.0).into(),
            deco_commit: Default::default(),
            redraw: true,
            animating: false,
            ipc,
        })
    }

    fn init_socket(handle: &LoopHandle<'static, Uwc>) -> anyhow::Result<OsString> {
        let source = ListeningSocketSource::new_auto()?;
        let name = source.socket_name().to_os_string();
        handle
            .insert_source(source, |stream, _, state| {
                if let Err(e) = state
                    .display_handle
                    .insert_client(stream, Arc::new(ClientState { compositor_state: Default::default() }))
                {
                    tracing::warn!("rejecting client: {e}");
                }
            })
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(name)
    }

    pub fn keyboard(&self) -> KeyboardHandle<Uwc> {
        self.seat.get_keyboard().unwrap()
    }

    pub fn pointer(&self) -> PointerHandle<Uwc> {
        self.seat.get_pointer().unwrap()
    }

    pub fn damage(&mut self) {
        self.redraw = true;
        self.ipc.dirty = true;
    }

    // ---- monitor / viewport -------------------------------------------------------------

    /// Use `output` as the monitor. A new output after a hotplug keeps the tags and viewports.
    pub fn add_monitor(&mut self, output: Output, size: Size<i32, Logical>) {
        if let Some(old) = self.mon.as_ref().map(|m| m.output.clone()) {
            self.move_layers(&old, &output);
        }
        if let Some(m) = self.mon.as_mut() {
            m.output = output;
            m.size = size;
            let (w, h) = (size.w as f64, size.h as f64);
            self.cursor = (self.cursor.x.clamp(0.0, w - 1.0), self.cursor.y.clamp(0.0, h - 1.0)).into();
            self.damage();
            return;
        }
        self.mon = Some(Monitor {
            output,
            size,
            tagset: 1,
            prev_tagset: 1,
            vp: [Viewport::default(); TAGCOUNT],
            overview: None,
        });
        self.cursor = (size.w as f64 / 2.0, size.h as f64 / 2.0).into();
    }

    pub fn viewport(&self) -> Viewport {
        self.mon.as_ref().map(|m| m.viewport()).unwrap_or_default()
    }

    pub fn screen_size(&self) -> (f64, f64) {
        self.mon.as_ref().map(|m| (m.size.w as f64, m.size.h as f64)).unwrap_or((1280.0, 720.0))
    }

    pub fn visible(&self, c: &Client) -> bool {
        c.mapped && self.mon.as_ref().is_some_and(|m| c.tags & m.tagset != 0)
    }

    /// Apply `f` to the viewport in use, then re-send the pointer position so what is under
    /// it is right after the canvas moved.
    /// `f` gets the work area (the screen minus bars) for centring.
    pub fn with_viewport(&mut self, f: impl FnOnce(&mut Viewport, crate::canvas::Area)) {
        let area = self.work_area();
        if let Some(m) = self.mon.as_mut() {
            m.overview = None;
            f(m.viewport_mut(), area);
        }
        self.refresh_pointer();
        self.damage();
    }

    /// Smooth viewport changes are not animated yet: IPC and key pans are instant.
    pub fn pan_canvas(&mut self, dx: f64, dy: f64) {
        self.with_viewport(|v, _| v.pan_canvas(dx, dy));
    }

    pub fn zoom_by(&mut self, factor: f64) {
        let c = (self.cursor.x, self.cursor.y);
        self.with_viewport(|v, _| v.zoom_at(factor, c));
    }

    // ---- windows ------------------------------------------------------------------------

    pub fn client_index(&self, id: u32) -> Option<usize> {
        self.clients.iter().position(|c| c.id == id)
    }

    pub fn client_by_surface(&self, s: &WlSurface) -> Option<usize> {
        self.clients.iter().position(|c| c.surface() == s)
    }

    /// Topmost visible client under a screen point and the surface within it.
    pub fn surface_under_screen(&self, p: Point<f64, Logical>) -> Option<(usize, WlSurface, Point<f64, Logical>)> {
        let vp = self.viewport();
        let (cx, cy) = vp.screen_to_canvas((p.x, p.y));
        let canvas: Point<f64, Logical> = (cx, cy).into();
        for (i, c) in self.clients.iter().enumerate().rev() {
            if !self.visible(c) {
                continue;
            }
            let origin = c.surface_origin();
            let rel = canvas - origin;
            if let Some((s, loc)) = c.window.surface_under(rel, smithay::desktop::WindowSurfaceType::ALL) {
                // `loc` is the surface's offset inside the window; pointer focus wants the
                // surface origin in the same space as the pointer location (canvas px).
                return Some((i, s, origin + loc.to_f64()));
            }
        }
        None
    }

    /// Give a client keyboard focus and raise it.
    pub fn focus_client(&mut self, id: Option<u32>) {
        self.set_focus(id, true);
    }

    /// Give a client keyboard focus, optionally raising it (sloppy focus does not).
    pub fn set_focus(&mut self, id: Option<u32>, raise: bool) {
        self.focus = id;
        if let Some(i) = id.and_then(|id| self.client_index(id)).filter(|_| raise) {
            let c = self.clients.remove(i);
            self.clients.push(c);
        }
        for c in &self.clients {
            if let Some(t) = c.window.toplevel() {
                t.with_pending_state(|s| {
                    if Some(c.id) == id {
                        s.states.set(
                            smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Activated,
                        );
                    } else {
                        s.states.unset(
                            smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Activated,
                        );
                    }
                });
                if c.mapped {
                    t.send_pending_configure();
                }
            }
        }
        // A layer surface holding the keyboard (a launcher) keeps it; the window focus is remembered.
        self.update_keyboard_focus();
        self.deco_commit.increment();
        self.damage();
    }

    /// Pan (not zoom) so the window is fully on screen if it is not; used by keyboard focus changes.
    pub fn reveal(&mut self, id: u32) {
        let Some(i) = self.client_index(id) else { return };
        let r = self.clients[i].rect();
        let (ax, ay, aw, ah) = self.work_area();
        let vp = self.viewport();
        let (sx, sy) = vp.canvas_to_screen((r.loc.x, r.loc.y));
        let (ex, ey) = vp.canvas_to_screen((r.loc.x + r.size.w, r.loc.y + r.size.h));
        if sx >= ax && sy >= ay && ex <= ax + aw && ey <= ay + ah {
            return;
        }
        let c = (r.loc.x + r.size.w / 2.0, r.loc.y + r.size.h / 2.0);
        self.with_viewport(|v, s| v.center_on(c, s));
    }

    pub fn focus_and_reveal(&mut self, id: u32) {
        self.focus_client(Some(id));
        self.reveal(id);
        if self.config.warp_cursor {
            self.warp_cursor_into(id);
        }
    }

    fn warp_cursor_into(&mut self, id: u32) {
        let Some(i) = self.client_index(id) else { return };
        let r = self.clients[i].rect();
        let vp = self.viewport();
        let (x, y) = vp.canvas_to_screen((r.loc.x + r.size.w / 2.0, r.loc.y + r.size.h / 2.0));
        self.cursor = (x, y).into();
        self.refresh_pointer();
    }

    pub fn focus_visible_cycle(&mut self, forward: bool) {
        let ids: Vec<u32> = self.clients.iter().filter(|c| self.visible(c)).map(|c| c.id).collect();
        if ids.is_empty() {
            return;
        }
        // `clients` is in stacking order, top last; "next" walks down the stack.
        let pos = self.focus.and_then(|f| ids.iter().position(|&i| i == f));
        let n = ids.len();
        let next = match (pos, forward) {
            (Some(p), true) => ids[(p + n - 1) % n],
            (Some(p), false) => ids[(p + 1) % n],
            (None, _) => ids[n - 1],
        };
        self.focus_and_reveal(next);
    }

    pub fn focus_direction(&mut self, dir: Dir) {
        let Some(cur) = self.focus.and_then(|id| self.client_index(id)) else {
            return self.focus_visible_cycle(true);
        };
        let r = self.clients[cur].rect();
        let from = (r.loc.x + r.size.w / 2.0, r.loc.y + r.size.h / 2.0);
        let d = dir.vec();
        let best = self
            .clients
            .iter()
            .enumerate()
            .filter(|(i, c)| *i != cur && self.visible(c))
            .filter_map(|(_, c)| {
                let r = c.rect();
                let to = (r.loc.x + r.size.w / 2.0 - from.0, r.loc.y + r.size.h / 2.0 - from.1);
                let along = to.0 * d.0 + to.1 * d.1;
                let across = (to.0 * d.1 - to.1 * d.0).abs();
                // Within 45 degrees of the wanted direction; nearer and straighter wins.
                (along > 0.0 && across <= along).then_some((c.id, along + 2.0 * across))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1));
        if let Some((id, _)) = best {
            self.focus_and_reveal(id);
        }
    }

    pub fn close_client(&mut self, id: u32) {
        if let Some(i) = self.client_index(id) {
            if let Some(t) = self.clients[i].window.toplevel() {
                t.send_close();
            }
        }
    }

    pub fn move_client(&mut self, id: u32, x: f64, y: f64) {
        if let Some(i) = self.client_index(id) {
            self.clients[i].canvas = (x, y).into();
            self.refresh_pointer();
            self.damage();
        }
    }

    pub fn resize_client(&mut self, id: u32, w: i32, h: i32) {
        if let Some(i) = self.client_index(id) {
            if let Some(t) = self.clients[i].window.toplevel() {
                t.with_pending_state(|s| s.size = Some((w.max(50), h.max(30)).into()));
                t.send_pending_configure();
            }
        }
    }

    /// Re-send the pointer position so focus follows what is under it after the scene moved.
    pub fn refresh_pointer(&mut self) {
        let t = self.start_time.elapsed().as_millis() as u32;
        self.pointer_motion_to(self.cursor, t);
    }

    // ---- tags ---------------------------------------------------------------------------

    pub fn set_tagset(&mut self, tags: u32) {
        let tags = tags & ALLTAGS;
        if tags == 0 {
            return;
        }
        let Some(m) = self.mon.as_mut() else { return };
        if m.tagset != tags {
            m.prev_tagset = m.tagset;
            m.tagset = tags;
            m.overview = None;
        }
        // Keyboard focus follows the tag: keep it if still visible, else take the top window.
        let keep = self.focus.and_then(|id| self.client_index(id)).is_some_and(|i| self.visible(&self.clients[i]));
        if !keep {
            let top = self.clients.iter().rev().find(|c| self.visible(c)).map(|c| c.id);
            self.focus_client(top);
        }
        self.refresh_pointer();
        self.damage();
    }

    pub fn tagset(&self) -> u32 {
        self.mon.as_ref().map(|m| m.tagset).unwrap_or(1)
    }

    pub fn send_to_tag(&mut self, tag: usize) {
        if let Some(i) = self.focus.and_then(|id| self.client_index(id)) {
            self.clients[i].tags = 1 << tag;
            let keep = self.visible(&self.clients[i]);
            if !keep {
                let top = self.clients.iter().rev().find(|c| self.visible(c)).map(|c| c.id);
                self.focus_client(top);
            }
            self.damage();
        }
    }

    pub fn toggle_overview(&mut self) {
        let rects: Vec<Rectangle<f64, Logical>> =
            self.clients.iter().filter(|c| self.visible(c)).map(|c| c.rect()).collect();
        let screen = self.work_area();
        let Some(m) = self.mon.as_mut() else { return };
        if let Some(prev) = m.overview.take() {
            *m.viewport_mut() = prev;
        } else if let Some(first) = rects.first() {
            let all = rects.iter().fold(*first, |a, r| a.merge(*r));
            let before = m.viewport();
            m.viewport_mut().fit((all.loc.x, all.loc.y, all.size.w, all.size.h), screen, 60.0);
            m.overview = Some(before);
        }
        self.refresh_pointer();
        self.damage();
    }

    // ---- actions ------------------------------------------------------------------------

    pub fn run_action(&mut self, action: &Action) {
        match action {
            Action::Spawn(argv) => self.spawn(&argv[0], &argv[1..]),
            Action::SpawnSh(cmd) => self.spawn("sh", &["-c".to_string(), cmd.clone()]),
            Action::CloseWindow => {
                if let Some(id) = self.focus {
                    self.close_client(id)
                }
            }
            Action::Quit => self.loop_signal.stop(),
            Action::ViewTag(t) => self.set_tagset(1 << t),
            Action::ToggleViewTag(t) => {
                let ts = self.tagset() ^ (1 << t);
                self.set_tagset(ts);
            }
            Action::SendToTag(t) => self.send_to_tag(*t),
            Action::ViewAll => self.set_tagset(ALLTAGS),
            Action::ViewPrevious => {
                if let Some(m) = &self.mon {
                    let p = m.prev_tagset;
                    self.set_tagset(p);
                }
            }
            Action::FocusNext => self.focus_visible_cycle(true),
            Action::FocusPrevious => self.focus_visible_cycle(false),
            Action::FocusDirection(d) => self.focus_direction(*d),
            Action::MoveWindow(d) => {
                if let Some(i) = self.focus.and_then(|id| self.client_index(id)) {
                    let v = d.vec();
                    let s = self.config.move_step;
                    let c = self.clients[i].canvas;
                    let id = self.clients[i].id;
                    self.move_client(id, c.x + v.0 * s, c.y + v.1 * s);
                }
            }
            Action::ResizeWindow(d) => {
                if let Some(i) = self.focus.and_then(|id| self.client_index(id)) {
                    let v = d.vec();
                    let s = self.config.move_step;
                    let sz = self.clients[i].size();
                    let id = self.clients[i].id;
                    self.resize_client(id, sz.w + (v.0 * s) as i32, sz.h + (v.1 * s) as i32);
                }
            }
            Action::PanView(d) => {
                let v = d.vec();
                let s = self.config.pan_step;
                self.pan_canvas(v.0 * s, v.1 * s);
            }
            Action::Zoom(f) => self.zoom_by(*f),
            Action::ZoomReset => {
                let c = (self.cursor.x, self.cursor.y);
                self.with_viewport(|v, _| v.set_zoom_at(1.0, c));
            }
            Action::CenterWindow => {
                if let Some(i) = self.focus.and_then(|id| self.client_index(id)) {
                    let r = self.clients[i].rect();
                    let c = (r.loc.x + r.size.w / 2.0, r.loc.y + r.size.h / 2.0);
                    self.with_viewport(|v, s| v.center_on(c, s));
                }
            }
            Action::FitWindow => {
                if let Some(i) = self.focus.and_then(|id| self.client_index(id)) {
                    let r = self.clients[i].rect();
                    self.with_viewport(|v, s| v.fit((r.loc.x, r.loc.y, r.size.w, r.size.h), s, 40.0));
                }
            }
            Action::ToggleOverview => self.toggle_overview(),
            // Mouse-only actions are handled where the button press is.
            Action::DragMove | Action::DragResize | Action::DragPan => {}
            Action::SwitchVt(vt) => self.backend.switch_vt(*vt),
        }
    }

    /// Keyboard layout and repeat from the config (all backends); pointer settings are libinput's (udev).
    pub fn apply_input_config(&mut self) {
        let k = self.config.keyboard.clone();
        let kb = self.keyboard();
        let xkb = XkbConfig {
            rules: &k.rules,
            model: &k.model,
            layout: &k.layout,
            variant: &k.variant,
            options: k.options.clone(),
        };
        // The config was only accepted if this keymap compiles, so this does not fail in practice.
        if let Err(e) = kb.set_xkb_config(self, xkb) {
            tracing::warn!("keymap: {e:?}");
        }
        kb.change_repeat_info(k.repeat_rate, k.repeat_delay);
    }

    pub fn spawn(&self, cmd: &str, args: &[String]) {
        let mut c = Command::new(cmd);
        c.args(args)
            .env("WAYLAND_DISPLAY", &self.socket_name)
            .env("UWC_SOCKET", &self.ipc.path)
            .env("XDG_CURRENT_DESKTOP", "uwc")
            .stdin(Stdio::null());
        match c.spawn() {
            // Reap it so it doesn't linger as a zombie.
            Ok(mut child) => {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            Err(e) => tracing::warn!("spawn {cmd}: {e}"),
        }
    }

    // ---- config -------------------------------------------------------------------------

    pub fn reload_config(&mut self) -> Result<(), String> {
        match Config::load(self.config_path.as_deref()) {
            Ok(c) => {
                self.config = c;
                self.apply_input_config();
                self.cursor_theme.configure(&self.config.cursor_theme, self.config.cursor_size);
                self.backend.config_changed(&self.config);
                self.udev_scan_connectors();
                self.config_error = None;
                self.config_rev += 1;
                self.deco_commit.increment();
                self.damage();
                Ok(())
            }
            Err(e) => {
                let msg = format!("{e:#}");
                tracing::warn!("config rejected: {msg}");
                self.config_error = Some(msg.clone());
                self.ipc.dirty = true;
                Err(msg)
            }
        }
    }
}
