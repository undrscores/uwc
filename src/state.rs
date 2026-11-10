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

/// How a window is drawn right now compared with where it rests: opacity and size factors and a
/// canvas offset, easing back to `Fx::REST` (so it never touches `canvas`, which clients and IPC
/// see). Driven by `Uwc::step_animations`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fx {
    pub alpha: f64,
    pub scale: f64,
    pub off: (f64, f64),
    /// How long the fade/grow and the glide take, as configured when they started.
    pub fade_ms: f64,
    pub move_ms: f64,
}

impl Fx {
    pub const REST: Fx = Fx { alpha: 1.0, scale: 1.0, off: (0.0, 0.0), fade_ms: 0.0, move_ms: 0.0 };

    pub fn at_rest(&self) -> bool {
        self.alpha == 1.0 && self.scale == 1.0 && self.off == (0.0, 0.0)
    }
}

/// A window that closed, kept while it fades out (`close-ms`): its content captured at close (a
/// memory buffer, `k` px per window px), drawn where the window would be now with the decoration it
/// closed with. Visual only: never hit-tested, focused, saved or reported over IPC.
pub struct Ghost {
    pub buffer: smithay::backend::renderer::element::memory::MemoryRenderBuffer,
    /// The buffer's pixel density: buffer px per window px.
    pub k: f64,
    /// The window's geometry size (window px) and place on the canvas (or its pinned screen spot).
    pub size: Size<i32, Logical>,
    pub canvas: Point<f64, Logical>,
    pub pin: Option<Point<f64, Logical>>,
    /// The window's own scale on the canvas.
    pub scale: f64,
    pub fullscreen: bool,
    /// The options with its rules applied, and whether it had focus, when it closed.
    pub deco: crate::rules::Deco,
    pub focused: bool,
    /// Fade-out state: opacity heads to 0, size to `open_scale` (the open animation reversed).
    pub alpha: f64,
    pub scale_fx: f64,
    pub fade_ms: f64,
    /// Ids for the dim, backdrop, border and shadow shader elements.
    pub deco_ids: [Id; 4],
}

impl Ghost {
    /// Where it is drawn now, like `Uwc::place` for a window: canvas windows follow the viewport
    /// and the view's zoom; pinned and fullscreen ones are fixed to the screen. The fade shrinks
    /// it about the centre, as the open animation's grow does.
    pub fn place(&self, vp: Viewport) -> Place {
        if self.fullscreen {
            return Place { origin: (0.0, 0.0).into(), zoom: 1.0, on_canvas: false };
        }
        let (mut origin, mut zoom) = match self.pin {
            Some(p) => (p, self.scale),
            None => (vp.canvas_to_screen((self.canvas.x, self.canvas.y)).into(), vp.zoom * self.scale),
        };
        origin = (
            origin.x + self.size.w as f64 * zoom * (1.0 - self.scale_fx) / 2.0,
            origin.y + self.size.h as f64 * zoom * (1.0 - self.scale_fx) / 2.0,
        )
            .into();
        zoom *= self.scale_fx;
        Place { origin, zoom, on_canvas: self.pin.is_none() }
    }
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
    /// Open, move and tag-switch animation state (drawing only).
    pub fx: Fx,
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
    /// An eased move in progress: the tag and the view `vp[tag]` is heading for.
    pub target: Option<(usize, Viewport)>,
}

impl Monitor {
    pub fn viewport(&self) -> Viewport {
        self.vp[lowest_tag(self.tagset)]
    }

    /// Jump to where an eased move is heading.
    pub fn settle(&mut self) {
        if let Some((t, v)) = self.target.take() {
            self.vp[t] = v;
        }
    }

    /// For direct, instant changes: a pending eased move is finished first, so they build on the
    /// view IPC reported and nothing is left to pull it back.
    pub fn viewport_mut(&mut self) -> &mut Viewport {
        self.settle();
        &mut self.vp[lowest_tag(self.tagset)]
    }

    /// Where the view of `tag` is going: the end of an eased move, else the view itself.
    pub fn view_of(&self, tag: usize) -> Viewport {
        match self.target {
            Some((t, v)) if t == tag => v,
            _ => self.vp[tag],
        }
    }

    pub fn target_viewport(&self) -> Viewport {
        self.view_of(lowest_tag(self.tagset))
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
    /// Windows that closed, fading out (`close-ms`).
    pub ghosts: Vec<Ghost>,
    /// Last frames captured when a close was asked for (the client may destroy its surface before
    /// its toplevel, leaving nothing to capture then), used when the window goes.
    pub pending_ghosts: std::collections::HashMap<u32, Ghost>,
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
    /// When the eased view move last stepped.
    pub anim_stepped: Instant,
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
            ghosts: Vec::new(),
            pending_ghosts: Default::default(),
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
            anim_stepped: Instant::now(),
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
            target: None,
        });
        self.cursor = (size.w as f64 / 2.0, size.h as f64 / 2.0).into();
    }

    pub fn viewport(&self) -> Viewport {
        self.mon.as_ref().map(|m| m.viewport()).unwrap_or_default()
    }

    /// The view an eased move is heading for (what IPC reports and relative moves build on).
    pub fn target_viewport(&self) -> Viewport {
        self.mon.as_ref().map(|m| m.target_viewport()).unwrap_or_default()
    }

    pub fn screen_size(&self) -> (f64, f64) {
        self.mon.as_ref().map(|m| (m.size.w as f64, m.size.h as f64)).unwrap_or((1280.0, 720.0))
    }

    pub fn visible(&self, c: &Client) -> bool {
        c.mapped && c.minimized == 0 && (c.pin.is_some() || self.mon.as_ref().is_some_and(|m| c.tags & m.tagset != 0))
    }

    /// Where a window is drawn and hit right now: its placement with the open/move/tag animation on.
    pub fn place(&self, c: &Client) -> Place {
        self.place_with(c, &c.fx)
    }

    /// Where it rests, for maths on the window's real position (anchoring a scale, drag factors).
    pub fn place_at_rest(&self, c: &Client) -> Place {
        self.place_with(c, &Fx::REST)
    }

    fn place_with(&self, c: &Client, fx: &Fx) -> Place {
        if c.fullscreen {
            return Place { origin: (0.0, 0.0).into(), zoom: 1.0, on_canvas: false };
        }
        // Pinned windows ignore the view's zoom but keep their own scale.
        let (mut origin, mut zoom, on_canvas, view_zoom) = match c.pin {
            Some(p) => (p, c.scale, false, 1.0),
            None => {
                let vp = self.viewport();
                (vp.canvas_to_screen((c.canvas.x, c.canvas.y)).into(), vp.zoom * c.scale, true, vp.zoom)
            }
        };
        // The animation on top: the glide offset, then the grow about the window's centre.
        origin = (origin.x + fx.off.0 * view_zoom, origin.y + fx.off.1 * view_zoom).into();
        if fx.scale != 1.0 {
            let s = c.size();
            let k = (1.0 - fx.scale) / 2.0;
            origin = (origin.x + s.w as f64 * zoom * k, origin.y + s.h as f64 * zoom * k).into();
            zoom *= fx.scale;
        }
        Place { origin, zoom, on_canvas }
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
        let pl = self.place_at_rest(&self.clients[i]);
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

    /// Like `with_viewport`, but the view eases to the result over `view-ms` (builds on where an
    /// earlier eased move is heading, so repeated key pans add up).
    pub fn with_viewport_eased(&mut self, f: impl FnOnce(&mut Viewport, crate::canvas::Area)) {
        if self.config.view_ms <= 0.0 {
            return self.with_viewport(f);
        }
        let area = self.work_area();
        let idle = !self.anim_running();
        let Some(m) = self.mon.as_mut() else { return };
        m.overview = None;
        let tag = lowest_tag(m.tagset);
        let mut v = m.target_viewport();
        f(&mut v, area);
        m.target = (v != m.vp[tag]).then_some((tag, v));
        if idle {
            self.begin_anim();
        }
        self.damage();
    }

    /// One step of an eased move, called when a frame is drawn: the view closes in on its target
    /// (the centre of the screen and the log of the zoom move by the same fraction, so the time
    /// does not matter and a new target mid-move just bends the path). True while still moving.
    fn step_view(&mut self, dt: f64) -> bool {
        let tau = (self.config.view_ms / 1000.0 / 4.0).max(1e-4);
        let Some(m) = self.mon.as_mut() else { return false };
        let Some((tag, goal)) = m.target else { return false };
        let cur = m.vp[tag];
        let dz = (goal.zoom / cur.zoom).ln();
        // The pivot: the screen point both views show the same canvas point at (a scroll zoom's
        // pointer), found per axis from p/z0 + x0 = p/z1 + x1. Scaling about it reaches the goal
        // exactly and keeps that point still. Without a zoom change the screen centre will do.
        let centre = (m.size.w as f64 / 2.0, m.size.h as f64 / 2.0);
        let d = 1.0 / cur.zoom - 1.0 / goal.zoom;
        let (w, h) = if dz.abs() > 1e-3 { ((goal.x - cur.x) / d, (goal.y - cur.y) / d) } else { centre };
        let (c, g) = (cur.screen_to_canvas((w, h)), goal.screen_to_canvas((w, h)));
        let near = {
            let (cc, gc) = (cur.screen_to_canvas(centre), goal.screen_to_canvas(centre));
            (gc.0 - cc.0).abs() * cur.zoom < 0.25 && (gc.1 - cc.1).abs() * cur.zoom < 0.25 && dz.abs() < 1e-3
        };
        let moving = if tag != lowest_tag(m.tagset) || near || self.config.view_ms <= 0.0 {
            m.vp[tag] = goal;
            m.target = None;
            false
        } else {
            let k = 1.0 - (-dt / tau).exp();
            let zoom = cur.zoom * (dz * k).exp();
            let c = (c.0 + (g.0 - c.0) * k, c.1 + (g.1 - c.1) * k);
            m.vp[tag] = Viewport { x: c.0 - w / zoom, y: c.1 - h / zoom, zoom };
            true
        };
        self.refresh_pointer();
        self.redraw = true;
        if !moving {
            self.ipc.dirty = true;
        }
        moving
    }

    /// Something is easing (the view, a window, or a closing window fading out).
    pub fn anim_running(&self) -> bool {
        self.mon.as_ref().is_some_and(|m| m.target.is_some())
            || self.clients.iter().any(|c| !c.fx.at_rest())
            || !self.ghosts.is_empty()
    }

    /// Call before starting an animation when none was running: time counts from now, not from the
    /// last frame, which may have been long ago.
    pub fn begin_anim(&mut self) {
        self.anim_stepped = Instant::now();
    }

    /// Everything that eases, stepped once per drawn frame. True while something is still moving.
    pub fn step_animations(&mut self) -> bool {
        let now = Instant::now();
        let dt = now.duration_since(self.anim_stepped).as_secs_f64().min(0.05);
        self.anim_stepped = now;
        let view = self.step_view(dt);
        let fx = self.step_fx(dt);
        let ghosts = self.step_ghosts(dt);
        view || fx || ghosts
    }

    /// Windows' open, move and tag fades close in on rest, each by the same fraction per time.
    fn step_fx(&mut self, dt: f64) -> bool {
        let mut moving = false;
        let mut finished = false;
        for c in self.clients.iter_mut().filter(|c| !c.fx.at_rest()) {
            let f = &mut c.fx;
            let k = |ms: f64| 1.0 - (-dt / (ms / 1000.0 / 4.0).max(1e-4)).exp();
            let (kf, km) = (k(f.fade_ms), k(f.move_ms));
            f.alpha += (1.0 - f.alpha) * kf;
            f.scale += (1.0 - f.scale) * kf;
            f.off = (f.off.0 - f.off.0 * km, f.off.1 - f.off.1 * km);
            if (1.0 - f.alpha) < 0.01 {
                f.alpha = 1.0;
            }
            if (1.0 - f.scale).abs() < 0.002 {
                f.scale = 1.0;
            }
            if f.off.0.abs() < 0.25 && f.off.1.abs() < 0.25 {
                f.off = (0.0, 0.0);
            }
            if f.at_rest() {
                *f = Fx::REST;
                finished = true;
            } else {
                moving = true;
            }
        }
        if moving || finished {
            // Fades change how the shader elements look without changing their geometry.
            self.deco_commit.increment();
            self.refresh_pointer();
            self.redraw = true;
        }
        if finished && !moving {
            self.ipc.dirty = true;
        }
        moving
    }

    /// Closing windows fade out, each by the same fraction per time as the other eases, towards
    /// `open_scale` (the open animation reversed). Gone once fully transparent.
    fn step_ghosts(&mut self, dt: f64) -> bool {
        if self.ghosts.is_empty() {
            return false;
        }
        let target = self.config.open_scale;
        let mut moving = false;
        for g in &mut self.ghosts {
            let k = 1.0 - (-dt / (g.fade_ms / 1000.0 / 4.0).max(1e-4)).exp();
            g.alpha -= g.alpha * k;
            g.scale_fx += (target - g.scale_fx) * k;
            if g.alpha < 0.01 {
                g.alpha = 0.0;
            } else {
                moving = true;
            }
        }
        self.ghosts.retain(|g| g.alpha > 0.0);
        // The fade changes how the shader elements look without changing their geometry.
        self.deco_commit.increment();
        self.redraw = true;
        if !moving {
            self.ipc.dirty = true;
        }
        moving
    }

    pub fn pan_canvas(&mut self, dx: f64, dy: f64) {
        self.with_viewport_eased(|v, _| v.pan_canvas(dx, dy));
    }

    pub fn zoom_by(&mut self, factor: f64) {
        let c = (self.cursor.x, self.cursor.y);
        self.with_viewport_eased(|v, _| v.zoom_at(factor, c));
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
        let vp = self.target_viewport();
        let (sx, sy) = vp.canvas_to_screen((r.loc.x, r.loc.y));
        let (ex, ey) = vp.canvas_to_screen((r.loc.x + r.size.w, r.loc.y + r.size.h));
        if sx >= ax && sy >= ay && ex <= ax + aw && ey <= ay + ah {
            return;
        }
        let c = (r.loc.x + r.size.w / 2.0, r.loc.y + r.size.h / 2.0);
        self.with_viewport_eased(|v, s| v.center_on(c, s));
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
            // The last frame for the fade-out: the client may destroy its surface before its
            // toplevel, leaving nothing to capture when the destroy arrives.
            if let Some(g) = self.closing_ghost(i) {
                self.pending_ghosts.insert(id, g);
            }
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

    /// `move_client`, but the window glides there over `move-ms` (keys and IPC; drags are instant).
    pub fn move_client_eased(&mut self, id: u32, x: f64, y: f64) {
        let Some(i) = self.client_index(id) else { return };
        let idle = !self.anim_running();
        // Where it is drawn now, in the space `fx.off` is in: canvas px, or screen px when pinned.
        let before =
            self.clients[i].pin.map(|p| (p.x, p.y)).unwrap_or((self.clients[i].canvas.x, self.clients[i].canvas.y));
        self.move_client(id, x, y);
        let c = &mut self.clients[i];
        let after = c.pin.map(|p| (p.x, p.y)).unwrap_or((c.canvas.x, c.canvas.y));
        if self.config.move_ms > 0.0 && !c.fullscreen && before != after {
            c.fx.off = (c.fx.off.0 + before.0 - after.0, c.fx.off.1 + before.1 - after.1);
            c.fx.move_ms = self.config.move_ms;
            if idle {
                self.begin_anim();
            }
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
        let idle = !self.anim_running();
        let was: Vec<bool> = self.clients.iter().map(|c| self.visible(c)).collect();
        let Some(m) = self.mon.as_mut() else { return };
        if m.tagset != tags {
            m.prev_tagset = m.tagset;
            m.tagset = tags;
            m.overview = None;
        }
        // Windows that just came into view fade in.
        if self.config.tag_ms > 0.0 {
            let now: Vec<bool> = self.clients.iter().map(|c| self.visible(c)).collect();
            let mut any = false;
            for ((c, was), now) in self.clients.iter_mut().zip(was).zip(now) {
                if now && !was {
                    c.fx.alpha = 0.0;
                    c.fx.fade_ms = self.config.tag_ms;
                    any = true;
                }
            }
            if any && idle {
                self.begin_anim();
            }
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
            m.bookmarks[t][n] = Some(m.target_viewport());
            self.ipc.dirty = true;
        }
    }

    /// Jump to bookmark `n` of the tag in use; false if it was never saved.
    pub fn bookmark_goto(&mut self, n: usize) -> bool {
        let Some(v) = self.mon.as_ref().and_then(|m| m.bookmarks[crate::canvas::lowest_tag(m.tagset)][n]) else {
            return false;
        };
        self.with_viewport_eased(|vp, _| *vp = v);
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
                    self.move_client_eased(id, c.x + v.0 * s, c.y + v.1 * s);
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
                self.with_viewport_eased(|v, _| v.set_zoom_at(1.0, c));
            }
            Action::CenterWindow => {
                if let Some(i) = self.focus.and_then(|id| self.client_index(id)) {
                    let r = self.clients[i].rect();
                    let c = (r.loc.x + r.size.w / 2.0, r.loc.y + r.size.h / 2.0);
                    self.with_viewport_eased(|v, s| v.center_on(c, s));
                }
            }
            Action::FitWindow => {
                if let Some(i) = self.focus.and_then(|id| self.client_index(id)) {
                    let r = self.clients[i].rect();
                    self.with_viewport_eased(|v, s| v.fit((r.loc.x, r.loc.y, r.size.w, r.size.h), s, 40.0));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A ghost with a 1x1 frame at canvas position (0, 0), its own scale 1.
    fn ghost(scale_fx: f64) -> Ghost {
        Ghost {
            buffer: smithay::backend::renderer::element::memory::MemoryRenderBuffer::from_slice(
                &[0u8; 4],
                smithay::backend::allocator::Fourcc::Abgr8888,
                (1, 1),
                1,
                smithay::utils::Transform::Normal,
                None,
            ),
            k: 1.0,
            size: (100, 50).into(),
            canvas: (0.0, 0.0).into(),
            pin: None,
            scale: 1.0,
            fullscreen: false,
            deco: Default::default(),
            focused: false,
            alpha: 1.0,
            scale_fx,
            fade_ms: 160.0,
            deco_ids: std::array::from_fn(|_| Id::new()),
        }
    }

    #[test]
    fn a_ghost_follows_the_view_and_shrinks_about_its_centre() {
        // The view looks at canvas (5, 10) zoomed 2x, so canvas (0, 0) is at screen (-10, -20).
        let vp = Viewport { x: 5.0, y: 10.0, zoom: 2.0 };
        let pl = ghost(1.0).place(vp);
        assert_eq!((pl.origin.x, pl.origin.y), (-10.0, -20.0));
        assert_eq!(pl.zoom, 2.0);
        assert!(pl.on_canvas);

        // Shrunk to half: half the size, about the centre, so the centre stays put.
        let pl = ghost(0.5).place(vp);
        assert_eq!((pl.origin.x, pl.origin.y), (40.0, 5.0));
        assert_eq!(pl.zoom, 1.0);

        // Pinned: a screen position, unzoomed but for its own scale, like a pinned window.
        let mut g = ghost(1.0);
        g.pin = Some((30.0, 40.0).into());
        g.scale = 2.0;
        let pl = g.place(vp);
        assert_eq!((pl.origin.x, pl.origin.y), (30.0, 40.0));
        assert_eq!(pl.zoom, 2.0);
        assert!(!pl.on_canvas);

        // Fullscreen: the screen, whatever the view.
        let mut g = ghost(1.0);
        g.fullscreen = true;
        let pl = g.place(vp);
        assert_eq!((pl.origin.x, pl.origin.y), (0.0, 0.0));
        assert_eq!(pl.zoom, 1.0);
        assert!(!pl.on_canvas);
    }
}
