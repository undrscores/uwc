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
            Display, DisplayHandle, Resource,
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
    /// Pinned: the window's screen position (geometry top-left, logical px). It stays there while the
    /// view moves, is not zoomed and is shown on every tag.
    pub pin: Option<Point<f64, Logical>>,
    pub fullscreen: bool,
    /// The size to go back to when leaving fullscreen.
    pub prev_size: Option<Size<i32, Logical>>,
    /// Minimized (the scratchpad): 0 = no, else the order it was hidden in (newest highest).
    pub minimized: u32,
    /// What the rules' `open { }` blocks said, decided at the first configure, used when it maps.
    pub open: crate::rules::Open,
    /// The window's own scale on the canvas (1 = normal), on top of the view's zoom.
    pub scale: f64,
    /// Counts the window's commits (new content), for captures that wait for damage.
    pub commits: u64,
    /// The render scale it was last told (fractional scale / preferred buffer scale).
    pub sent_scale: Option<f64>,
    /// Ids for the four border rects, stable so the damage tracker can tell frames apart.
    pub border_ids: [Id; 4],
    /// Ids for the dim, backdrop, border and shadow shader elements.
    pub deco_ids: [Id; 4],
}

impl Client {
    pub fn size(&self) -> Size<i32, Logical> {
        self.window.geometry().size
    }

    /// The window's footprint on the canvas: its size times its own scale.
    pub fn rect(&self) -> Rectangle<f64, Logical> {
        let s = self.size();
        Rectangle::new(self.canvas, (s.w as f64 * self.scale, s.h as f64 * self.scale).into())
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
    /// `start` is the pointer's screen position when the drag began, `orig` the window's position
    /// then (canvas px, or screen px for a pinned window).
    Move {
        id: u32,
        start: Point<f64, Logical>,
        orig: Point<f64, Logical>,
    },
    Resize {
        id: u32,
        start: Point<f64, Logical>,
        orig: Size<i32, Logical>,
    },
    Pan,
}

/// Where a window is drawn: its geometry's top-left on the screen (logical px), and screen px per
/// window px. Canvas windows follow the viewport; pinned and fullscreen ones are fixed to the screen
/// and never zoomed.
#[derive(Debug, Clone, Copy)]
pub struct Place {
    pub origin: Point<f64, Logical>,
    pub zoom: f64,
    pub on_canvas: bool,
}

/// A window surface under the pointer. `origin` and `location` are in the window's own space (canvas
/// px for canvas windows, screen px for pinned and fullscreen ones) so surface-local coordinates
/// come out right whatever the zoom.
pub struct WindowHit {
    pub idx: usize,
    pub surface: WlSurface,
    pub origin: Point<f64, Logical>,
    pub location: Point<f64, Logical>,
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
    /// Saved views per tag: `bookmarks[tag][n]`.
    pub bookmarks: [[Option<Viewport>; 9]; TAGCOUNT],
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
    /// Rules added over IPC (`rule { ... }`), until the next reload.
    pub ipc_rules: Vec<crate::rules::Rule>,
    pub workspaces: crate::workspace::WorkspaceState,
    pub toplevels: crate::toplevels::ForeignToplevels,
    pub export: crate::export::ToplevelExport,
    pub screencopy: crate::screencopy::Screencopy,
    /// Frames the output has drawn (with damage), for screencopy's copy_with_damage.
    pub frames_drawn: u64,
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
    /// The zoom windows render for: follows the view's zoom once it has stayed put (`zoom-settle-ms`).
    pub settled_zoom: f64,
    /// The zoom seen last turn, and a counter so only the last zoom change of a burst settles.
    pub seen_zoom: f64,
    pub scale_gen: u64,
    #[allow(dead_code)] // held so the globals stay alive
    pub fractional_scale_state: smithay::wayland::fractional_scale::FractionalScaleManagerState,
    #[allow(dead_code)]
    pub viewporter_state: smithay::wayland::viewporter::ViewporterState,
    /// Counts minimize calls, so the newest minimized window comes back first.
    pub minimize_stamp: u32,
    /// Compiled user shaders by kind and file (None: it failed), and why the failed ones failed
    /// (by the path as written in the config). Cleared when the config is applied.
    pub user_shaders: std::collections::HashMap<
        (crate::deco::UserShader, std::path::PathBuf),
        Option<(smithay::backend::renderer::gles::GlesPixelProgram, bool)>,
    >,
    pub shader_errors: std::collections::BTreeMap<String, String>,
    pub background_id: Id,
    /// Remembered windows no window has claimed yet (session restore), and the last text written.
    pub session_pool: Vec<crate::session::SavedWindow>,
    pub session_written: Option<String>,
    /// The rect behind a fullscreen window (`fullscreen-bg`).
    pub fullscreen_bg_id: Id,
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
            compositor_state: CompositorState::new_v6::<Uwc>(&dh),
            xdg_shell_state: XdgShellState::new::<Uwc>(&dh),
            shm_state: ShmState::new::<Uwc>(&dh, vec![]),
            xdg_decoration_state: XdgDecorationState::new::<Uwc>(&dh),
            output_manager_state: OutputManagerState::new_with_xdg_output::<Uwc>(&dh),
            data_device_state: DataDeviceState::new::<Uwc>(&dh),
            popups: PopupManager::default(),
            layer_shell_state: WlrLayerShellState::new::<Uwc>(&dh),
            ipc_rules: Vec::new(),
            workspaces: crate::workspace::WorkspaceState::new(&dh),
            toplevels: crate::toplevels::ForeignToplevels::new(&dh),
            export: crate::export::ToplevelExport::new(&dh),
            screencopy: crate::screencopy::Screencopy::new(&dh),
            frames_drawn: 0,
            layer_focus: None,
            dmabuf_state: DmabufState::new(),
            syncobj_state: None,
            dmabuf_global: None,
            backend: Backend::None,
            cursor_status: CursorImageStatus::default_named(),
            cursor_theme: crate::cursor::CursorTheme::new(&config.cursor_theme, config.cursor_size),
            seat_state,
            seat,
            display_handle: dh.clone(),
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
            minimize_stamp: 0,
            settled_zoom: 1.0,
            seen_zoom: 1.0,
            scale_gen: 0,
            fractional_scale_state: smithay::wayland::fractional_scale::FractionalScaleManagerState::new::<Uwc>(&dh),
            viewporter_state: smithay::wayland::viewporter::ViewporterState::new::<Uwc>(&dh),
            fullscreen_bg_id: Id::new(),
            user_shaders: Default::default(),
            shader_errors: Default::default(),
            background_id: Id::new(),
            session_pool: Vec::new(),
            session_written: None,
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
            bookmarks: [[None; 9]; TAGCOUNT],
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
        c.mapped && c.minimized == 0 && (c.pin.is_some() || self.mon.as_ref().is_some_and(|m| c.tags & m.tagset != 0))
    }

    pub fn place(&self, c: &Client) -> Place {
        if c.fullscreen {
            return Place { origin: (0.0, 0.0).into(), zoom: 1.0, on_canvas: false };
        }
        // Pinned windows ignore the view's zoom but keep their own scale.
        if let Some(p) = c.pin {
            return Place { origin: p, zoom: c.scale, on_canvas: false };
        }
        let vp = self.viewport();
        Place { origin: vp.canvas_to_screen((c.canvas.x, c.canvas.y)).into(), zoom: vp.zoom * c.scale, on_canvas: true }
    }

    /// Scale a window on the canvas by `factor` (its own scale, 0.1 to 4), keeping screen point
    /// `anchor` over the same spot of the window (default: its centre).
    pub fn scale_window(&mut self, id: u32, factor: f64, anchor: Option<Point<f64, Logical>>) {
        self.set_window_scale(id, self.client_index(id).map(|i| self.clients[i].scale * factor).unwrap_or(1.0), anchor);
    }

    pub fn set_window_scale(&mut self, id: u32, scale: f64, anchor: Option<Point<f64, Logical>>) {
        let Some(i) = self.client_index(id) else { return };
        if self.clients[i].fullscreen || !scale.is_finite() {
            return;
        }
        let new = scale.clamp(0.1, 4.0);
        let old = self.clients[i].scale;
        let pl = self.place(&self.clients[i]);
        let size = self.clients[i].size();
        let anchor = anchor.unwrap_or_else(|| {
            (pl.origin.x + size.w as f64 * pl.zoom / 2.0, pl.origin.y + size.h as f64 * pl.zoom / 2.0).into()
        });
        // Screen origin so the anchor stays put: origin' = anchor - (anchor - origin) * new / old.
        let r = new / old;
        let origin: Point<f64, Logical> =
            (anchor.x - (anchor.x - pl.origin.x) * r, anchor.y - (anchor.y - pl.origin.y) * r).into();
        let c = &mut self.clients[i];
        c.scale = new;
        if c.pin.is_some() {
            c.pin = Some(origin);
        } else {
            c.canvas = self
                .mon
                .as_ref()
                .map(|m| m.viewport())
                .unwrap_or_default()
                .screen_to_canvas((origin.x, origin.y))
                .into();
        }
        self.refresh_pointer();
        self.damage();
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
    /// Topmost visible window surface under screen point `p`, among fullscreen windows or the others.
    pub fn window_under(&self, p: Point<f64, Logical>, fullscreen: bool) -> Option<WindowHit> {
        for (idx, c) in self.clients.iter().enumerate().rev() {
            if !self.visible(c) || c.fullscreen != fullscreen {
                continue;
            }
            let pl = self.place(c);
            // The point relative to the window geometry, in window px.
            let local: Point<f64, Logical> = ((p.x - pl.origin.x) / pl.zoom, (p.y - pl.origin.y) / pl.zoom).into();
            let g = c.window.geometry().loc.to_f64();
            if let Some((surface, loc)) = c.window.surface_under(local + g, smithay::desktop::WindowSurfaceType::ALL) {
                let space = if pl.on_canvas { c.canvas } else { pl.origin };
                return Some(WindowHit { idx, surface, origin: space - g + loc.to_f64(), location: space + local });
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
        // Pinned and fullscreen windows are on screen wherever the view is.
        if !self.place(&self.clients[i]).on_canvas {
            return;
        }
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
            .filter(|(i, c)| *i != cur && self.visible(c) && !c.fullscreen)
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

    /// Move a window to canvas position `x`, `y`. A pinned window goes where that is on screen now.
    pub fn move_client(&mut self, id: u32, x: f64, y: f64) {
        if let Some(i) = self.client_index(id) {
            if self.clients[i].pin.is_some() {
                let s = self.viewport().canvas_to_screen((x, y));
                self.clients[i].pin = Some(s.into());
            }
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

    // ---- pin, minimize, fullscreen -------------------------------------------------------------

    /// Pin (keep on screen while the view moves, unzoomed, on every tag) or unpin, as in uwm.
    /// A fullscreen window cannot be pinned.
    pub fn set_pinned(&mut self, id: u32, on: bool) {
        let Some(i) = self.client_index(id) else { return };
        if self.clients[i].fullscreen || self.clients[i].pin.is_some() == on {
            return;
        }
        let vp = self.viewport();
        let ts = self.tagset();
        let c = &mut self.clients[i];
        if on {
            c.pin = Some(vp.canvas_to_screen((c.canvas.x, c.canvas.y)).into());
            c.tags = ALLTAGS;
        } else {
            let p = c.pin.take().unwrap();
            c.canvas = vp.screen_to_canvas((p.x, p.y)).into();
            c.tags = ts;
        }
        self.focus_client(Some(id));
        self.refresh_pointer();
        self.damage();
    }

    /// Hide a window until it is restored (the scratchpad), or bring it back where you are.
    pub fn set_minimized(&mut self, id: u32, on: bool) {
        let Some(i) = self.client_index(id) else { return };
        if (self.clients[i].minimized != 0) == on || !self.clients[i].mapped {
            return;
        }
        if on {
            if self.clients[i].fullscreen {
                self.set_fullscreen(id, false);
            }
            self.minimize_stamp += 1;
            self.clients[i].minimized = self.minimize_stamp;
            if self.focus == Some(id) {
                let top = self.clients.iter().rev().find(|c| self.visible(c)).map(|c| c.id);
                self.focus_client(top);
            }
            self.refresh_pointer();
            self.damage();
        } else {
            self.clients[i].minimized = 0;
            if self.clients[i].pin.is_none() {
                self.clients[i].tags = self.tagset();
            }
            self.focus_and_reveal(id);
            self.damage();
        }
    }

    /// Remember the view of the tag in use as bookmark `n` (0-based).
    pub fn bookmark_save(&mut self, n: usize) {
        if let Some(m) = self.mon.as_mut() {
            let t = crate::canvas::lowest_tag(m.tagset);
            m.bookmarks[t][n] = Some(m.viewport());
            self.ipc.dirty = true;
        }
    }

    /// Jump to bookmark `n` of the tag in use; false if it was never saved.
    pub fn bookmark_goto(&mut self, n: usize) -> bool {
        let Some(v) = self.mon.as_ref().and_then(|m| m.bookmarks[crate::canvas::lowest_tag(m.tagset)][n]) else {
            return false;
        };
        self.with_viewport(|vp, _| *vp = v);
        true
    }

    /// Bring back the window minimized last.
    pub fn restore_newest(&mut self) {
        if let Some(id) = self.clients.iter().filter(|c| c.minimized != 0).max_by_key(|c| c.minimized).map(|c| c.id) {
            self.set_minimized(id, false);
        }
    }

    /// Make a window cover the monitor (unscaled, undecorated, above bars), or put it back at its
    /// earlier size. The canvas position is kept throughout.
    pub fn set_fullscreen(&mut self, id: u32, on: bool) {
        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State;
        let Some(i) = self.client_index(id) else { return };
        let screen = self.mon.as_ref().map(|m| m.size).unwrap_or((1280, 720).into());
        let c = &mut self.clients[i];
        let Some(t) = c.window.toplevel().cloned() else { return };
        if c.fullscreen != on {
            c.fullscreen = on;
            if on {
                // A repeated request must not overwrite the saved size with the fullscreen one.
                c.prev_size = Some(c.size()).filter(|s| s.w > 0 && s.h > 0);
            }
        }
        let prev = c.prev_size;
        let output = self.mon.as_ref().and_then(|m| {
            let client = self.display_handle.get_client(t.wl_surface().id()).ok()?;
            m.output.client_outputs(&client).next()
        });
        t.with_pending_state(|s| {
            if on {
                s.states.set(State::Fullscreen);
                s.size = Some(screen);
                s.fullscreen_output = output;
            } else {
                s.states.unset(State::Fullscreen);
                s.size = prev;
                s.fullscreen_output = None;
            }
        });
        // Before the first configure the state simply goes out with it.
        if t.is_initial_configure_sent() {
            t.send_pending_configure();
        }
        if on {
            self.focus_client(Some(id));
        }
        self.refresh_pointer();
        self.damage();
    }

    pub fn toggle_overview(&mut self) {
        // Only canvas windows: pinned and fullscreen ones are on screen anyway.
        let rects: Vec<Rectangle<f64, Logical>> =
            self.clients.iter().filter(|c| self.visible(c) && self.place(c).on_canvas).map(|c| c.rect()).collect();
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
            Action::ToggleFullscreen => {
                if let Some(i) = self.focus.and_then(|id| self.client_index(id)) {
                    let (id, on) = (self.clients[i].id, !self.clients[i].fullscreen);
                    self.set_fullscreen(id, on);
                }
            }
            Action::TogglePin => {
                if let Some(i) = self.focus.and_then(|id| self.client_index(id)) {
                    let (id, on) = (self.clients[i].id, self.clients[i].pin.is_none());
                    self.set_pinned(id, on);
                }
            }
            Action::MinimizeWindow => {
                if let Some(id) = self.focus {
                    self.set_minimized(id, true);
                }
            }
            Action::RestoreWindow => self.restore_newest(),
            Action::ScaleWindow(f) => {
                if let Some(id) = self.focus {
                    self.scale_window(id, *f, None);
                }
            }
            Action::ScaleWindowReset => {
                if let Some(id) = self.focus {
                    self.set_window_scale(id, 1.0, None);
                }
            }
            Action::BookmarkSave(n) => self.bookmark_save(*n),
            Action::BookmarkGoto(n) => {
                self.bookmark_goto(*n);
            }
            // Mouse-only actions are handled where the button press is.
            Action::DragMove | Action::DragResize | Action::DragPan => {}
            Action::SwitchVt(vt) => self.backend.switch_vt(*vt),
        }
    }

    /// What rules see of a window, and its decoration: the options with every fitting rule applied
    /// (the file's rules, then those added over IPC).
    pub fn deco_for(&self, c: &Client) -> crate::rules::Deco {
        let (app_id, title) = (c.app_id(), c.title());
        let f = self.facts(c, &app_id, &title);
        let rules: Vec<&crate::rules::Rule> = self.config.rules.iter().chain(&self.ipc_rules).collect();
        crate::rules::resolve_deco(&self.config.deco, &rules, &f, &self.config.vars)
    }

    pub fn facts<'a>(&self, c: &Client, app_id: &'a str, title: &'a str) -> crate::rules::Facts<'a> {
        crate::rules::Facts {
            app_id,
            title,
            tags: c.tags,
            focused: self.focus == Some(c.id),
            pinned: c.pin.is_some(),
            fullscreen: c.fullscreen,
            urgent: c.urgent,
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
                self.ipc_rules.clear();
                // Shader files may have changed (saving one is what reloaded the config).
                self.user_shaders.clear();
                self.shader_errors.clear();
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
