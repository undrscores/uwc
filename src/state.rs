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

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
    /// Set for clients that connected through a `wp_security_context_v1` socket (sandboxed apps).
    pub security: Option<smithay::wayland::security_context::SecurityContext>,
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
    /// Focus crossfade (`focus-ms`): 1 = settled on the current focus, 0 = just changed, with the
    /// configured duration alongside.
    pub focus_fade: f64,
    pub focus_ms: f64,
}

impl Fx {
    pub const REST: Fx =
        Fx { alpha: 1.0, scale: 1.0, off: (0.0, 0.0), fade_ms: 0.0, move_ms: 0.0, focus_fade: 1.0, focus_ms: 0.0 };

    pub fn at_rest(&self) -> bool {
        self.alpha == 1.0 && self.scale == 1.0 && self.off == (0.0, 0.0) && self.focus_fade == 1.0
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
    pub pin_mon: usize,
    /// The window's own scale on the canvas.
    pub scale: f64,
    pub fullscreen: bool,
    pub fs_mon: usize,
    /// The options with its rules applied, and whether it had focus, when it closed.
    pub deco: crate::rules::Deco,
    pub focused: bool,
    /// Fade-out state: opacity heads to 0, size to `open_scale` (the open animation reversed).
    pub alpha: f64,
    pub scale_fx: f64,
    pub fade_ms: f64,
    /// Ids for the dim, backdrop, border and shadow shader elements.
    pub deco_ids: [Id; 4],
    /// The closed window's id, kept as the key for its blur textures (blur.rs).
    pub key: u32,
}

impl Ghost {
    /// Whether the ghost is drawn on monitor `m` (like `Uwc::visible_on` for the window).
    pub fn visible_on(&self, m: usize) -> bool {
        if self.fullscreen {
            return m == self.fs_mon;
        }
        if self.pin.is_some() {
            return m == self.pin_mon;
        }
        true
    }

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
/// What a client is: a Wayland xdg-shell toplevel, or an X11 window through Xwayland.
#[derive(Clone)]
pub enum Shell {
    Wayland(Window),
    /// Boxed: an X11Surface is far larger than a Wayland window handle.
    X11(Box<smithay::xwayland::xwm::X11Surface>),
}

pub struct Client {
    pub id: u32,
    pub shell: Shell,
    /// Top-left of the window geometry in canvas px (what clients are told and IPC reports).
    pub canvas: Point<f64, Logical>,
    pub tags: u32,
    /// False until the client has committed a first buffer.
    pub mapped: bool,
    /// True once it has mapped (a window that hides itself maps again where it was).
    pub ever_mapped: bool,
    /// Wants attention (set by xdg-activation once that exists).
    pub urgent: bool,
    /// Pinned: the window's screen position (geometry top-left, logical px) on `pin_mon`. It stays
    /// there while the view moves, is not zoomed and is shown on every tag.
    pub pin: Option<Point<f64, Logical>>,
    pub pin_mon: usize,
    pub fullscreen: bool,
    /// Which monitor a fullscreen window covers (its own screen).
    pub fs_mon: usize,
    /// The size to go back to when leaving fullscreen.
    pub prev_size: Option<Size<i32, Logical>>,
    /// Minimized (the scratchpad): 0 = no, else the order it was hidden in (newest highest).
    pub minimized: u32,
    /// Kept out of the tiling layouts (tiling.rs): by a rule, `toggle-float`, or being moved by hand.
    pub floating: bool,
    /// Its announced (min, max) size, 0 = none; read at every commit.
    pub limits: crate::tiling::Limits,
    /// The size it showed it needs when it drew bigger than the slot it was told.
    pub tile_min: (i32, i32),
    /// The size its layout last asked for.
    pub tile_want: Option<(i32, i32)>,
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
    /// The icon the client set over xdg-toplevel-icon-v1: a stock icon name, and/or pixel data
    /// written to a PAM file under the cache dir (path here); `icon_hash` is what was written.
    pub icon_name: Option<String>,
    pub icon_path: Option<std::path::PathBuf>,
    pub icon_hash: u64,
}

impl Client {
    pub fn size(&self) -> Size<i32, Logical> {
        match &self.shell {
            Shell::Wayland(w) => w.geometry().size,
            Shell::X11(s) => s.geometry().size,
        }
    }

    /// The X11 window, when this client is one.
    pub fn x11(&self) -> Option<&smithay::xwayland::xwm::X11Surface> {
        match &self.shell {
            Shell::X11(s) => Some(s),
            Shell::Wayland(_) => None,
        }
    }

    /// The window's footprint on the canvas: its size times its own scale.
    pub fn rect(&self) -> Rectangle<f64, Logical> {
        let s = self.size();
        Rectangle::new(self.canvas, (s.w as f64 * self.scale, s.h as f64 * self.scale).into())
    }

    /// The surface the client draws with. An X11 window that only renders into pixmaps (an
    /// ancient X app) has none and is not drawn.
    pub fn surface(&self) -> Option<WlSurface> {
        match &self.shell {
            Shell::Wayland(w) => Some(w.toplevel().expect("wayland window").wl_surface().clone()),
            Shell::X11(s) => s.wl_surface(),
        }
    }

    pub fn title(&self) -> String {
        match &self.shell {
            Shell::X11(s) => s.title(),
            Shell::Wayland(w) => {
                let surface = w.toplevel().expect("wayland window").wl_surface();
                smithay::wayland::compositor::with_states(surface, |s| {
                    s.data_map
                        .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                        .and_then(|d| d.lock().ok().and_then(|d| d.title.clone()))
                })
                .unwrap_or_default()
            }
        }
    }

    pub fn app_id(&self) -> String {
        match &self.shell {
            Shell::X11(s) => s.class(),
            Shell::Wayland(w) => {
                let surface = w.toplevel().expect("wayland window").wl_surface();
                smithay::wayland::compositor::with_states(surface, |s| {
                    s.data_map
                        .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                        .and_then(|d| d.lock().ok().and_then(|d| d.app_id.clone()))
                })
                .unwrap_or_default()
            }
        }
    }
}

/// A touchpad gesture the compositor is taking (see `gestures { }` in the config); gestures with no
/// bind go to the client under the pointer (zwp-pointer-gestures-v1).
#[derive(Debug, Clone, PartialEq)]
pub enum Gesture {
    /// A swipe that pans the view with the fingers; releasing while moving coasts on (fling).
    Pan,
    /// A swipe with direction binds: when the fingers lift, run the action of the direction the
    /// swipe went (if it went far enough), with the total travel in `dx`/`dy`.
    Dir { dirs: Vec<(Dir, Action)>, dx: f64, dy: f64 },
    /// Run this action when the fingers lift (a swipe or pinch bound to an action).
    End(Action),
    /// A pinch that zooms the view; `last` is the scale the previous update reported (absolute).
    Zoom { last: f64 },
}

/// A touchpad or touch pan that was let go while moving: the view keeps gliding, slowing down
/// (`pan-momentum-ms`). Screen px per second, on monitor `mon`.
#[derive(Debug, Clone, Copy)]
pub struct Fling {
    pub mon: usize,
    pub vx: f64,
    pub vy: f64,
}

/// Which side owns a selection target (clipboard, primary): a wayland client, an X11 client
/// (via the Xwayland bridge), or nobody.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionOwner {
    None,
    Wayland,
    X11,
}

/// Index of a selection target in `Uwc::selection_owner`.
pub fn sel_idx(ty: smithay::wayland::selection::SelectionTarget) -> usize {
    match ty {
        smithay::wayland::selection::SelectionTarget::Clipboard => 0,
        smithay::wayland::selection::SelectionTarget::Primary => 1,
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
    /// Resizing a tile moves its region's main split: `ratio0` is the ratio when the drag began.
    TileResize {
        region: crate::tiling::Region,
        start: Point<f64, Logical>,
        ratio0: f64,
        /// Screen px per canvas px of the window being dragged.
        zoom: f64,
    },
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
    /// Logical size in px (this monitor's screen).
    pub size: Size<i32, Logical>,
    /// Where the monitor sits in the global input space: monitors tile left to right.
    pub origin: Point<f64, Logical>,
    /// This monitor's view of each tag: one canvas, one camera per monitor.
    pub vp: [Viewport; TAGCOUNT],
    /// The view before the overview was opened.
    pub overview: Option<Viewport>,
    /// Saved views per tag: `bookmarks[tag][n]`.
    pub bookmarks: [[Option<Viewport>; 9]; TAGCOUNT],
    /// An eased move in progress: the view `vp[tag]` is heading for.
    /// The eased move in progress, and the tag it belongs to.
    pub target: Option<(usize, Viewport)>,
    /// Sharp zoom: the zoom this monitor's windows are told, once it has settled.
    pub settled_zoom: f64,
    /// The output's power mode (zwlr-output-power-management): a blanked monitor draws nothing.
    pub powered: bool,
    /// The zoom seen last turn, and a counter so only the last zoom change of a burst settles.
    pub seen_zoom: f64,
}

impl Monitor {
    /// The view of the tag in use; tags are shared, each monitor has its own camera.
    pub fn viewport(&self, tagset: u32) -> Viewport {
        self.vp[lowest_tag(tagset)]
    }

    /// Jump to where an eased move is heading.
    pub fn settle(&mut self, _tagset: u32) {
        if let Some((t, v)) = self.target.take() {
            self.vp[t] = v;
        }
    }

    /// For direct, instant changes: a pending eased move is finished first, so they build on the
    /// view IPC reported and nothing is left to pull it back.
    pub fn viewport_mut(&mut self, tagset: u32) -> &mut Viewport {
        self.settle(tagset);
        &mut self.vp[lowest_tag(tagset)]
    }

    /// Where the view of `tag` is going: the end of an eased move, else the view itself.
    pub fn view_of(&self, tag: usize, _tagset: u32) -> Viewport {
        match self.target {
            Some((t, v)) if t == tag => v,
            _ => self.vp[tag],
        }
    }

    pub fn target_viewport(&self, tagset: u32) -> Viewport {
        self.view_of(lowest_tag(tagset), tagset)
    }
}

pub struct Uwc {
    pub start_time: Instant,
    pub extras: crate::extras::Extras,
    pub ext_capture: crate::ext_capture::ExtCapture,
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
    /// xdg-activation-v1 (apps asking for focus).
    pub activation_state: smithay::wayland::xdg_activation::XdgActivationState,
    /// ext-idle-notify-v1 (idle daemons); its timers are reset by real input.
    pub idle_notifier: smithay::wayland::idle_notify::IdleNotifierState<Uwc>,
    /// ext-session-lock-v1 (lock screen clients).
    pub session_lock: crate::sessionlock::SessionLock,
    /// zwp-pointer-constraints-v1 (games, 3D apps); held so the global stays alive.
    #[allow(dead_code)]
    pub pointer_constraints: smithay::wayland::pointer_constraints::PointerConstraintsState,
    /// zwp-relative-pointer-manager-v1 (the deltas a locked pointer hands over); held so the
    /// global stays alive.
    #[allow(dead_code)]
    pub relative_pointer: smithay::wayland::relative_pointer::RelativePointerManagerState,
    /// xwayland-shell-v1, so X11 windows can associate a wl_surface.
    pub xwayland_shell: smithay::wayland::xwayland_shell::XWaylandShellState,
    /// The X11 window manager, once Xwayland is up.
    pub xwm: Option<smithay::xwayland::X11Wm>,
    /// Keeps the Xwayland server source in the loop (dropping it would stop the server).
    #[allow(dead_code)]
    pub xwayland: Option<smithay::reexports::calloop::RegistrationToken>,
    /// Override-redirect X11 windows (menus, tooltips): drawn on top, not managed.
    pub or_windows: Vec<smithay::xwayland::X11Surface>,
    /// zwp-pointer-gestures-v1 (clients that want touchpad gestures); held so the global stays
    /// alive.
    #[allow(dead_code)]
    pub pointer_gestures: smithay::wayland::pointer_gestures::PointerGesturesState,
    /// zwp-text-input-v3 (clients that accept IME input); held so the global stays alive.
    #[allow(dead_code)]
    pub text_input: smithay::wayland::text_input::TextInputManagerState,
    /// zwp-input-method-v2 (the IME itself); held so the global stays alive.
    #[allow(dead_code)]
    pub input_method: smithay::wayland::input_method::InputMethodManagerState,
    /// The IME's popups (candidate lists), drawn on top.
    pub ime_popups: Vec<smithay::wayland::input_method::PopupSurface>,
    /// zwlr-output-management-v1 (kanshi, wdisplays).
    pub output_management: crate::output_management::OutputManagement,
    /// zwlr-output-power-management-v1 (swayidle): held so the global stays alive.
    #[allow(dead_code)]
    pub output_power: crate::output_power::OutputPower,
    #[allow(dead_code)] // held so the global stays alive
    pub idle_inhibit: smithay::wayland::idle_inhibit::IdleInhibitManagerState,
    /// Live zwp-idle-inhibitor-v1 objects; any one pauses the idle notifications.
    pub idle_inhibitors: u32,
    /// Frames the output has drawn (with damage), for screencopy's copy_with_damage.
    pub frames_drawn: u64,
    /// The layer surface that has the keyboard (a launcher, a lock screen), over any window focus.
    pub layer_focus: Option<WlSurface>,
    pub dmabuf_state: DmabufState,
    /// linux-drm-syncobj-v1 (explicit sync), when the GPU supports it.
    pub syncobj_state: Option<smithay::wayland::drm_syncobj::DrmSyncobjState>,
    pub dmabuf_global: Option<DmabufGlobal>,
    /// The render node and formats output captures can be written to as dmabufs; set by the udev backend.
    pub capture_dmabuf: Option<crate::capture::DmabufOffer>,
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
    /// The monitors, left to right in the global input space. Each is another camera onto the one
    /// canvas, with its own view of every tag.
    pub monitors: Vec<Monitor>,
    /// The monitor the pointer is on: where view actions (pan, zoom, overview, bookmarks) apply.
    pub cur: usize,
    /// The tags in use (shared by all monitors) and the ones shown before.
    pub tagset: u32,
    pub prev_tagset: u32,
    /// Pointer position in the global input space (logical px).
    pub cursor: Point<f64, Logical>,
    /// What the client under the pointer asked the pointer to look like.
    pub cursor_status: CursorImageStatus,
    /// The xcursor theme, loaded lazily per image.
    pub cursor_theme: crate::cursor::CursorTheme,
    pub drag: Option<Drag>,
    /// The touchpad gesture in progress, when it is ours.
    pub gesture: Option<Gesture>,
    pub fling: Option<Fling>,
    /// The keyboard layout each window last had (`per-window-layout`).
    pub window_layouts: std::collections::HashMap<u32, usize>,
    /// The active keyboard layout: (index, name, count).
    pub layout: (usize, String, usize),
    pub hot: crate::hotzones::HotState,
    /// Each tag's tiling layout (tiling.rs).
    pub tiling: crate::tiling::Tiling,
    /// Alignment lines drawn while a window is dragged (snap.rs).
    pub guides: crate::snap::Guides,
    /// Hint mode (`jump-hints`) while it is up.
    pub hints: Option<crate::hints::Hints>,
    /// `drag_motion` is running (see `pointer_motion_to`).
    pub in_drag_motion: bool,
    /// The recent speed of a pan gesture (screen px/s) and when it was last sampled.
    pub pan_vel: (f64, f64),
    pub pan_sampled: Instant,
    pub touch: crate::touch::TouchState,
    pub selection_owner: [SelectionOwner; 2],
    pub primary_selection: smithay::wayland::selection::primary_selection::PrimarySelectionState,
    /// Pointer position at the previous step of a pan drag.
    pub drag_last: Point<f64, Logical>,
    /// A mouse bind took the press, so the matching release is swallowed too.
    pub consumed_press: bool,
    /// The running drag was asked for by a client (xdg move/resize request): its press already reached
    /// the client, so the release must too, or the client's implicit grab never ends.
    pub drag_from_client: bool,
    /// Pointer buttons currently held (as the compositor saw them).
    pub buttons_down: Vec<u32>,
    /// Bumped on anything that changes how decorations look, so the damage tracker redraws them.
    pub deco_commit: smithay::backend::renderer::utils::CommitCounter,
    /// Set by anything that needs a new frame.
    pub redraw: bool,
    /// Counts zoom changes, so only the last of a burst settles.
    pub scale_gen: u64,
    #[allow(dead_code)] // held so the globals stay alive
    pub fractional_scale_state: smithay::wayland::fractional_scale::FractionalScaleManagerState,
    #[allow(dead_code)]
    pub viewporter_state: smithay::wayland::viewporter::ViewporterState,
    /// wp_presentation, wp_fifo_v1 and wp_commit_timing_v1 (presentation.rs); held so the globals stay alive.
    #[allow(dead_code)]
    pub presentation_state: smithay::wayland::presentation::PresentationState,
    #[allow(dead_code)]
    pub fifo_state: smithay::wayland::fifo::FifoManagerState,
    #[allow(dead_code)]
    pub commit_timing_state: smithay::wayland::commit_timing::CommitTimingManagerState,
    /// wp_security_context_v1 (sandbox.rs); held so the global stays alive.
    #[allow(dead_code)]
    pub security_context_state: smithay::wayland::security_context::SecurityContextState,
    /// CLOCK_MONOTONIC, the clock presentation timestamps are given on.
    pub present_clock: crate::presentation::PresentClock,
    /// A timer for a commit that waits for a time is pending / a hidden window's fifo barrier is queued.
    pub commit_timer_armed: bool,
    /// Textures the background blur keeps between frames (blur.rs).
    pub blur: crate::blur::BlurCache,
    /// ext-background-effect-v1: the surfaces that asked for blur behind them (background_effect.rs).
    pub bg_effect: std::collections::HashMap<WlSurface, crate::background_effect::BgEffect>,
    /// Texture keys for blur.rs beyond window ids (layer surfaces, ghosts).
    pub next_blur_key: u32,
    #[allow(dead_code)] // held so the global stays alive
    pub alpha_modifier: smithay::wayland::alpha_modifier::AlphaModifierState,
    #[allow(dead_code)] // held so the global stays alive
    pub toplevel_icon: smithay::wayland::xdg_toplevel_icon::XdgToplevelIconManager,
    pub pace_idle: bool,
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
        seat.add_touch();

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
                    let display = unsafe { display.get_mut() };
                    dispatch_surviving_panics(display, state);
                    Ok(smithay::reexports::calloop::PostAction::Continue)
                },
            )
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let ipc = Ipc::new(&loop_handle, &socket_name)?;
        crate::virtual_pointer::create_global(&dh);
        Uwc::bg_effect_new(&dh);
        let primary_selection = smithay::wayland::selection::primary_selection::PrimarySelectionState::new::<Uwc>(&dh);
        let extras = crate::extras::Extras::new(&dh, &primary_selection);

        // zwlr-output-management-v1 (kanshi, wdisplays): the manager global.
        dh.create_global::<Uwc, smithay::reexports::wayland_protocols_wlr::output_management::v1::server::zwlr_output_manager_v1::ZwlrOutputManagerV1, ()>(4, ());

        Ok(Uwc {
            start_time: Instant::now(),
            extras,
            ext_capture: crate::ext_capture::ExtCapture::new(&dh),
            compositor_state: CompositorState::new_v6::<Uwc>(&dh),
            xdg_shell_state: XdgShellState::new::<Uwc>(&dh),
            shm_state: ShmState::new::<Uwc>(&dh, vec![]),
            xdg_decoration_state: XdgDecorationState::new::<Uwc>(&dh),
            output_manager_state: OutputManagerState::new_with_xdg_output::<Uwc>(&dh),
            data_device_state: DataDeviceState::new::<Uwc>(&dh),
            primary_selection,
            popups: PopupManager::default(),
            layer_shell_state: WlrLayerShellState::new_with_filter::<Uwc, _>(&dh, |c| !crate::sandbox::is_sandboxed(c)),
            ipc_rules: Vec::new(),
            workspaces: crate::workspace::WorkspaceState::new(&dh),
            toplevels: crate::toplevels::ForeignToplevels::new(&dh),
            export: crate::export::ToplevelExport::new(&dh),
            screencopy: crate::screencopy::Screencopy::new(&dh),
            activation_state: smithay::wayland::xdg_activation::XdgActivationState::new::<Uwc>(&dh),
            idle_notifier: smithay::wayland::idle_notify::IdleNotifierState::new(&dh, loop_handle.clone()),
            session_lock: crate::sessionlock::SessionLock::new(&dh),
            pointer_constraints: smithay::wayland::pointer_constraints::PointerConstraintsState::new::<Uwc>(&dh),
            relative_pointer: smithay::wayland::relative_pointer::RelativePointerManagerState::new::<Uwc>(&dh),
            xwayland_shell: smithay::wayland::xwayland_shell::XWaylandShellState::new::<Uwc>(&dh),
            xwm: None,
            xwayland: None,
            or_windows: Vec::new(),
            pointer_gestures: smithay::wayland::pointer_gestures::PointerGesturesState::new::<Uwc>(&dh),
            text_input: smithay::wayland::text_input::TextInputManagerState::new::<Uwc>(&dh),
            input_method: smithay::wayland::input_method::InputMethodManagerState::new::<Uwc, _>(&dh, |c| {
                !crate::sandbox::is_sandboxed(c)
            }),
            ime_popups: Vec::new(),
            output_management: Default::default(),
            output_power: crate::output_power::OutputPower::new(&dh),
            idle_inhibit: smithay::wayland::idle_inhibit::IdleInhibitManagerState::new::<Uwc>(&dh),
            idle_inhibitors: 0,
            frames_drawn: 0,
            layer_focus: None,
            dmabuf_state: DmabufState::new(),
            syncobj_state: None,
            dmabuf_global: None,
            capture_dmabuf: None,
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
            monitors: Vec::new(),
            cur: 0,
            tagset: 1,
            prev_tagset: 1,
            cursor: (0.0, 0.0).into(),
            drag: None,
            gesture: None,
            fling: None,
            window_layouts: Default::default(),
            layout: (0, String::new(), 1),
            hot: Default::default(),
            tiling: Default::default(),
            guides: Default::default(),
            hints: None,
            in_drag_motion: false,
            pan_vel: (0.0, 0.0),
            pan_sampled: Instant::now(),
            touch: Default::default(),
            selection_owner: [SelectionOwner::None; 2],
            drag_last: (0.0, 0.0).into(),
            consumed_press: false,
            drag_from_client: false,
            buttons_down: Vec::new(),
            deco_commit: Default::default(),
            redraw: true,
            minimize_stamp: 0,
            scale_gen: 0,
            fractional_scale_state: smithay::wayland::fractional_scale::FractionalScaleManagerState::new::<Uwc>(&dh),
            viewporter_state: smithay::wayland::viewporter::ViewporterState::new::<Uwc>(&dh),
            presentation_state: smithay::wayland::presentation::PresentationState::new::<Uwc>(
                &dh,
                smithay::reexports::rustix::time::ClockId::Monotonic as u32,
            ),
            fifo_state: smithay::wayland::fifo::FifoManagerState::new::<Uwc>(&dh),
            commit_timing_state: smithay::wayland::commit_timing::CommitTimingManagerState::new::<Uwc>(&dh),
            security_context_state: crate::sandbox::create_state(&dh),
            present_clock: crate::presentation::PresentClock::new(),
            commit_timer_armed: false,
            blur: Default::default(),
            bg_effect: Default::default(),
            next_blur_key: 0x4000_0000,
            alpha_modifier: smithay::wayland::alpha_modifier::AlphaModifierState::new::<Uwc>(&dh),
            toplevel_icon: smithay::wayland::xdg_toplevel_icon::XdgToplevelIconManager::new::<Uwc>(&dh),
            pace_idle: false,
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
                if let Err(e) = state.display_handle.insert_client(stream, Arc::new(ClientState::default())) {
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

    /// Use `output` as a monitor. An output that replaces one with the same name (a mode change, a
    /// hotplug of the same connector) keeps its views; a new one becomes another camera onto the
    /// canvas, tiled to the right of the others.
    pub fn add_monitor(&mut self, output: Output, size: Size<i32, Logical>) {
        if let Some(i) = self.monitors.iter().position(|m| m.output.name() == output.name()) {
            let old = self.monitors[i].output.clone();
            if old != output {
                self.move_layers(&old, &output);
            }
            self.monitors[i].output = output;
            self.monitors[i].size = size;
            self.layout();
            let (w, h) = (size.w as f64, size.h as f64);
            self.cursor = (self.cursor.x.clamp(0.0, w - 1.0), self.cursor.y.clamp(0.0, h - 1.0)).into();
            self.damage();
            return;
        }
        self.monitors.push(Monitor {
            output,
            size,
            origin: (0.0, 0.0).into(),
            vp: [Viewport::default(); TAGCOUNT],
            overview: None,
            bookmarks: [[None; 9]; TAGCOUNT],
            target: None,
            settled_zoom: 1.0,
            seen_zoom: 1.0,
            powered: true,
        });
        self.layout();
        if self.monitors.len() == 1 {
            self.cursor = (size.w as f64 / 2.0, size.h as f64 / 2.0).into();
        }
        crate::output_management::refresh(self);
    }

    /// Drop the monitor named `name` (its output went away); windows pinned or fullscreen on it
    /// fall back to the first monitor.
    pub fn remove_monitor(&mut self, name: &str) {
        let Some(i) = self.monitors.iter().position(|m| m.output.name() == name) else { return };
        self.monitors.remove(i);
        for c in &mut self.clients {
            if c.pin_mon == i {
                c.pin_mon = 0;
            } else if c.pin_mon > i {
                c.pin_mon -= 1;
            }
            if c.fs_mon == i {
                c.fs_mon = 0;
            } else if c.fs_mon > i {
                c.fs_mon -= 1;
            }
        }
        for g in &mut self.ghosts {
            if g.pin_mon == i {
                g.pin_mon = 0;
            } else if g.pin_mon > i {
                g.pin_mon -= 1;
            }
            if g.fs_mon == i {
                g.fs_mon = 0;
            } else if g.fs_mon > i {
                g.fs_mon -= 1;
            }
        }
        if self.cur >= self.monitors.len() {
            self.cur = 0;
        }
        self.layout();
        let (w, h) = self.global_size();
        self.cursor = (self.cursor.x.clamp(0.0, w - 1.0), self.cursor.y.clamp(0.0, h - 1.0)).into();
        crate::output_management::refresh(self);
        self.damage();
    }

    /// Where each monitor sits: tiled left to right, making up the global input space.
    pub fn layout(&mut self) {
        let mut x = 0.0;
        for m in &mut self.monitors {
            m.origin = (x, 0.0).into();
            x += m.size.w as f64;
        }
    }

    /// The monitor a global point is on (the first one, for points between monitors).
    pub fn mon_at(&self, p: Point<f64, Logical>) -> usize {
        self.monitors
            .iter()
            .position(|m| p.x >= m.origin.x && p.x < m.origin.x + m.size.w as f64)
            .unwrap_or(self.cur)
            .min(self.monitors.len().saturating_sub(1))
    }

    /// The monitor that shows a window: the one it is pinned or fullscreened to, else the one whose
    /// view has its centre on screen (views can overlap; a window can be on several, and follow the
    /// view as it moves, like everything on the canvas).
    pub fn mon_of(&self, c: &Client) -> usize {
        if c.fullscreen {
            return c.fs_mon;
        }
        if c.pin.is_some() {
            return c.pin_mon;
        }
        let r = c.rect();
        let centre = (r.loc.x + r.size.w / 2.0, r.loc.y + r.size.h / 2.0);
        for (m, mon) in self.monitors.iter().enumerate() {
            let vp = mon.viewport(self.tagset);
            let (sx, sy) = vp.canvas_to_screen(centre);
            if sx >= 0.0 && sy >= 0.0 && sx < mon.size.w as f64 && sy < mon.size.h as f64 {
                return m;
            }
        }
        self.cur
    }

    pub fn viewport(&self) -> Viewport {
        self.viewport_of(self.cur)
    }

    pub fn viewport_of(&self, m: usize) -> Viewport {
        self.monitors.get(m).map(|x| x.viewport(self.tagset)).unwrap_or_default()
    }

    /// The view an eased move is heading for (what IPC reports and relative moves build on).
    pub fn target_viewport_of(&self, m: usize) -> Viewport {
        self.monitors.get(m).map(|x| x.target_viewport(self.tagset)).unwrap_or_default()
    }

    /// The cursor monitor's size: the screen view actions and window placements work with.
    pub fn screen_size(&self) -> (f64, f64) {
        self.monitors.get(self.cur).map(|m| (m.size.w as f64, m.size.h as f64)).unwrap_or((1280.0, 720.0))
    }

    /// Where monitor `m` sits in the global input space.
    pub fn origin_of(&self, m: usize) -> Point<f64, Logical> {
        self.monitors.get(m).map(|x| x.origin).unwrap_or((0.0, 0.0).into())
    }

    /// The global input space: the monitors side by side.
    pub fn global_size(&self) -> (f64, f64) {
        (
            self.monitors.iter().map(|m| m.size.w as f64).sum::<f64>(),
            self.monitors.iter().map(|m| m.size.h as f64).fold(0.0, f64::max),
        )
    }

    /// Whether the window is drawn anywhere at all.
    /// What output captures may be written into besides shm: only with `capture-dmabuf` on and a udev backend.
    pub fn dmabuf_offer(&self) -> Option<&crate::capture::DmabufOffer> {
        self.config.capture_dmabuf.then_some(self.capture_dmabuf.as_ref()).flatten()
    }

    pub fn visible(&self, c: &Client) -> bool {
        c.mapped && c.minimized == 0 && (c.pin.is_some() || c.tags & self.tagset != 0)
    }

    /// Whether the window is drawn on monitor `m`.
    pub fn visible_on(&self, m: usize, c: &Client) -> bool {
        if !c.mapped || c.minimized != 0 {
            return false;
        }
        if c.fullscreen {
            return m == c.fs_mon;
        }
        if c.pin.is_some() {
            return m == c.pin_mon;
        }
        c.tags & self.tagset != 0
    }

    /// Where a window is drawn and hit right now (on the cursor monitor): its placement with the
    /// open/move/tag animation on.
    pub fn place(&self, c: &Client) -> Place {
        self.place_on(self.cur, c, &c.fx)
    }

    /// Where it rests, for maths on the window's real position (anchoring a scale, drag factors).
    pub fn place_at_rest(&self, c: &Client) -> Place {
        self.place_on(self.cur, c, &Fx::REST)
    }

    /// Where a window is drawn on monitor `m` (each monitor is another camera onto the canvas).
    pub fn place_on(&self, m: usize, c: &Client, fx: &Fx) -> Place {
        if c.fullscreen {
            return Place { origin: (0.0, 0.0).into(), zoom: 1.0, on_canvas: false };
        }
        // Pinned windows ignore the view's zoom but keep their own scale.
        let (mut origin, mut zoom, on_canvas, view_zoom) = match c.pin {
            Some(p) if m == c.pin_mon => (p, c.scale, false, 1.0),
            _ => {
                let vp = self.viewport_of(m);
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
        let (m, pinned) = (self.mon_of(&self.clients[i]), self.clients[i].pin.is_some());
        let canvas = self.viewport_of(m).screen_to_canvas((origin.x, origin.y));
        let c = &mut self.clients[i];
        c.scale = new;
        if pinned {
            c.pin = Some(origin);
        } else {
            c.canvas = canvas.into();
        }
        self.refresh_pointer();
        self.damage();
    }

    /// Apply `f` to the viewport in use, then re-send the pointer position so what is under
    /// it is right after the canvas moved.
    /// `f` gets the work area (the screen minus bars) for centring.
    pub fn with_viewport(&mut self, f: impl FnOnce(&mut Viewport, crate::canvas::Area)) {
        self.with_viewport_on(self.cur, f);
    }

    /// `with_viewport`, on monitor `m`.
    pub fn with_viewport_on(&mut self, m: usize, f: impl FnOnce(&mut Viewport, crate::canvas::Area)) {
        self.fling = None;
        let area = self.work_area_of(m);
        let tagset = self.tagset;
        if let Some(mon) = self.monitors.get_mut(m) {
            mon.overview = None;
            f(mon.viewport_mut(tagset), area);
        }
        self.refresh_pointer();
        self.damage();
    }

    /// Like `with_viewport`, but the view eases to the result over `view-ms` (builds on where an
    /// earlier eased move is heading, so repeated key pans add up).
    pub fn with_viewport_eased(&mut self, f: impl FnOnce(&mut Viewport, crate::canvas::Area)) {
        self.with_viewport_eased_on(self.cur, f);
    }

    /// `with_viewport_eased`, on monitor `m`.
    pub fn with_viewport_eased_on(&mut self, m: usize, f: impl FnOnce(&mut Viewport, crate::canvas::Area)) {
        self.fling = None;
        if self.config.view_ms <= 0.0 {
            return self.with_viewport_on(m, f);
        }
        let area = self.work_area_of(m);
        let idle = !self.anim_running();
        let tagset = self.tagset;
        let Some(m) = self.monitors.get_mut(m) else { return };
        m.overview = None;
        let tag = lowest_tag(tagset);
        let mut v = m.target_viewport(tagset);
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
        let mut moving_any = false;
        for mi in 0..self.monitors.len() {
            moving_any |= self.step_view_of(mi, dt, tau);
        }
        moving_any
    }

    fn step_view_of(&mut self, mi: usize, dt: f64, tau: f64) -> bool {
        let Some(m) = self.monitors.get_mut(mi) else { return false };
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
        let moving = if near || self.config.view_ms <= 0.0 {
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

    /// Something is easing (a view, a window, or a closing window fading out).
    pub fn anim_running(&self) -> bool {
        self.fling.is_some()
            || self.edge_pan_velocity().is_some()
            || self.monitors.iter().any(|m| m.target.is_some())
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
        let view = self.step_view(dt) | self.step_fling(dt) | self.step_edge_pan(dt);
        let fx = self.step_fx(dt);
        let ghosts = self.step_ghosts(dt);
        view || fx || ghosts
    }

    /// While a canvas window is dragged with the pointer against a screen edge: the view's velocity.
    fn edge_pan_velocity(&self) -> Option<(f64, f64)> {
        let Some(Drag::Move { id, .. }) = self.drag else { return None };
        let i = self.client_index(id)?;
        if self.clients[i].pin.is_some() || self.clients[i].fullscreen {
            return None;
        }
        let v = crate::canvas::edge_pan_velocity(
            (self.cursor.x, self.cursor.y),
            self.global_size(),
            self.config.edge_pan_size,
            self.config.edge_pan_speed,
        );
        (v != (0.0, 0.0)).then_some(v)
    }

    /// Dragging a window against a screen edge carries the view along: the view pans towards the
    /// edge and the window stays under the pointer.
    fn step_edge_pan(&mut self, dt: f64) -> bool {
        let Some((vx, vy)) = self.edge_pan_velocity() else { return false };
        let z = self.viewport().zoom;
        let (dx, dy) = (vx * dt / z, vy * dt / z);
        self.with_viewport(|v, _| v.pan_canvas(dx, dy));
        if let Some(Drag::Move { orig, .. }) = &mut self.drag {
            orig.x += dx;
            orig.y += dy;
        }
        // Re-apply the drag at the unchanged pointer position: the window follows the view.
        self.pointer_motion_to(self.cursor, 0);
        true
    }

    /// A pan gesture moved the view by `(dx, dy)` screen px: remember how fast.
    pub fn pan_sample(&mut self, dx: f64, dy: f64) {
        let now = Instant::now();
        let dt = now.duration_since(self.pan_sampled).as_secs_f64().clamp(0.001, 0.1);
        self.pan_sampled = now;
        let (ix, iy) = (dx / dt, dy / dt);
        self.pan_vel = (self.pan_vel.0 * 0.6 + ix * 0.4, self.pan_vel.1 * 0.6 + iy * 0.4);
    }

    /// A pan gesture ended: if the fingers were still moving, the view carries on.
    pub fn pan_release(&mut self) {
        let (vx, vy) = std::mem::take(&mut self.pan_vel);
        let fresh = self.pan_sampled.elapsed().as_secs_f64() < 0.08;
        if self.config.pan_momentum_ms > 0.0 && fresh && vx.hypot(vy) > 150.0 {
            if !self.anim_running() {
                self.begin_anim();
            }
            self.fling = Some(Fling { mon: self.cur, vx, vy });
            self.damage();
        }
    }

    /// The view glides on after a fling, losing speed by the same fraction per time.
    fn step_fling(&mut self, dt: f64) -> bool {
        let Some(mut f) = self.fling else { return false };
        let tau = (self.config.pan_momentum_ms / 1000.0 / 4.0).max(1e-3);
        let k = (-dt / tau).exp();
        f.vx *= k;
        f.vy *= k;
        if f.vx.hypot(f.vy) < 8.0 || f.mon >= self.monitors.len() {
            self.fling = None;
            return false;
        }
        let tagset = self.tagset;
        self.monitors[f.mon].overview = None;
        self.monitors[f.mon].viewport_mut(tagset).pan_screen(f.vx * dt, f.vy * dt);
        self.fling = Some(f);
        self.refresh_pointer();
        self.damage();
        true
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
            f.focus_fade += (1.0 - f.focus_fade) * k(f.focus_ms);
            if (1.0 - f.alpha) < 0.01 {
                f.alpha = 1.0;
            }
            if (1.0 - f.scale).abs() < 0.002 {
                f.scale = 1.0;
            }
            if f.off.0.abs() < 0.25 && f.off.1.abs() < 0.25 {
                f.off = (0.0, 0.0);
            }
            if (1.0 - f.focus_fade) < 0.01 {
                f.focus_fade = 1.0;
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
        // The viewport's screen space is the monitor's own.
        let p = self.cursor - self.origin_of(self.cur);
        self.with_viewport_eased(|v, _| v.zoom_at(factor, (p.x, p.y)));
    }

    // ---- windows ------------------------------------------------------------------------

    pub fn client_index(&self, id: u32) -> Option<usize> {
        self.clients.iter().position(|c| c.id == id)
    }

    pub fn client_by_surface(&self, s: &WlSurface) -> Option<usize> {
        self.clients.iter().position(|c| c.surface().as_ref() == Some(s))
    }

    /// Topmost visible client under a screen point and the surface within it.
    /// Topmost visible window surface under screen point `p`, among fullscreen windows or the others.
    pub fn window_under(&self, m: usize, p: Point<f64, Logical>, fullscreen: bool) -> Option<WindowHit> {
        for (idx, c) in self.clients.iter().enumerate().rev() {
            if !self.visible_on(m, c) || c.fullscreen != fullscreen {
                continue;
            }
            let pl = self.place_on(m, c, &c.fx);
            // The point relative to the window geometry, in window px.
            let local: Point<f64, Logical> = ((p.x - pl.origin.x) / pl.zoom, (p.y - pl.origin.y) / pl.zoom).into();
            match &c.shell {
                Shell::Wayland(w) => {
                    let g = w.geometry().loc.to_f64();
                    if let Some((surface, loc)) = w.surface_under(local + g, smithay::desktop::WindowSurfaceType::ALL) {
                        let space = if pl.on_canvas { c.canvas } else { pl.origin };
                        return Some(WindowHit {
                            idx,
                            surface,
                            origin: space - g + loc.to_f64(),
                            location: space + local,
                        });
                    }
                }
                // An X11 window has no subsurfaces: the whole geometry is the hit rect.
                Shell::X11(s) => {
                    let g = s.geometry();
                    let in_rect = local.x >= g.loc.x as f64
                        && local.y >= g.loc.y as f64
                        && local.x < (g.loc.x + g.size.w) as f64
                        && local.y < (g.loc.y + g.size.h) as f64;
                    if in_rect {
                        if let Some(surface) = s.wl_surface() {
                            let g = g.loc.to_f64();
                            let space = if pl.on_canvas { c.canvas } else { pl.origin };
                            return Some(WindowHit {
                                idx,
                                surface: surface.clone(),
                                origin: space - g,
                                location: space + local,
                            });
                        }
                    }
                }
            }
        }
        None
    }

    /// Give a client keyboard focus and raise it.
    pub fn focus_client(&mut self, id: Option<u32>) {
        self.set_focus(id, true);
    }

    /// Give a client keyboard focus, optionally raising it (sloppy focus does not).
    /// Read the keyboard layout in use from xkb into the cache IPC reports: (index, name, how many).
    /// Called after anything that can change it (a key, `set_layout`, a new keymap).
    pub fn refresh_layout(&mut self) {
        let kb = self.keyboard();
        let info = kb.with_xkb_state(self, |ctx| {
            let xkb = ctx.xkb().lock().unwrap();
            let i = xkb.active_layout();
            (i.0 as usize, xkb.layout_name(i).to_string(), xkb.layouts().count())
        });
        if info != self.layout {
            self.layout = info;
            self.ipc.dirty = true;
        }
    }

    /// Switch to layout `n`, or the next one.
    pub fn set_layout(&mut self, n: Option<usize>) {
        let kb = self.keyboard();
        kb.with_xkb_state(self, |mut ctx| match n {
            Some(n) => ctx.set_layout(smithay::input::keyboard::Layout(n as u32)),
            None => ctx.cycle_next_layout(),
        });
        self.refresh_layout();
    }

    pub fn set_focus(&mut self, id: Option<u32>, raise: bool) {
        let changed = self.focus != id;
        let old = self.focus;
        if changed && self.config.per_window_layout {
            // Each window keeps the layout it was typed in.
            let (cur, _, count) = self.layout.clone();
            if let Some(o) = old {
                self.window_layouts.insert(o, cur);
            }
            if let Some(n) = id {
                let want = self.window_layouts.get(&n).copied().unwrap_or(0).min(count.saturating_sub(1));
                if want != cur {
                    self.set_layout(Some(want));
                }
            }
        }
        self.focus = id;
        // The focus look crossfades between the old and the new window over `focus-ms`.
        if changed && self.config.focus_ms > 0.0 {
            for c in &mut self.clients {
                if Some(c.id) == old || Some(c.id) == id {
                    c.fx.focus_fade = 0.0;
                    c.fx.focus_ms = self.config.focus_ms;
                }
            }
        }
        if let Some(i) = id.and_then(|id| self.client_index(id)).filter(|_| raise) {
            let c = self.clients.remove(i);
            self.clients.push(c);
        }
        if changed && id.is_some() {
            self.run_hook(crate::hooks::Hook::Focus, id.and_then(|id| self.client_index(id)));
        }
        // Being focused answers the attention request (xdg-activation) that flagged it.
        if let Some(i) = id.and_then(|id| self.client_index(id)) {
            if self.clients[i].urgent {
                self.clients[i].urgent = false;
                self.deco_commit.increment();
            }
        }
        for c in &self.clients {
            match &c.shell {
                Shell::Wayland(w) => {
                    let Some(t) = w.toplevel() else { continue };
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
                Shell::X11(s) => {
                    let _ = s.set_activated(Some(c.id) == id);
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
        let m = self.mon_of(&self.clients[i]);
        // Pinned and fullscreen windows are on screen wherever the view is.
        if !self.place_on(m, &self.clients[i], &Fx::REST).on_canvas {
            return;
        }
        let r = self.clients[i].rect();
        let (ax, ay, aw, ah) = self.work_area_of(m);
        let vp = self.target_viewport_of(m);
        let (sx, sy) = vp.canvas_to_screen((r.loc.x, r.loc.y));
        let (ex, ey) = vp.canvas_to_screen((r.loc.x + r.size.w, r.loc.y + r.size.h));
        if sx >= ax && sy >= ay && ex <= ax + aw && ey <= ay + ah {
            return;
        }
        let c = (r.loc.x + r.size.w / 2.0, r.loc.y + r.size.h / 2.0);
        self.with_viewport_eased_on(m, |v, s| v.center_on(c, s));
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
            match &self.clients[i].shell {
                Shell::Wayland(w) => {
                    if let Some(t) = w.toplevel() {
                        t.send_close();
                    }
                }
                Shell::X11(s) => {
                    let _ = s.close();
                }
            }
        }
    }

    /// Take a client out of the list: keep its last frame for the fade-out, hand focus to what
    /// is left on top, and end a drag it was part of.
    pub fn remove_client(&mut self, i: usize) {
        if self.clients[i].ever_mapped {
            self.run_hook(crate::hooks::Hook::WindowClose, Some(i));
        }
        // Keep the last frame to fade out over close-ms: the one captured when the close was
        // asked for, else one captured now if the surface is still alive (skipped without a
        // renderer, so closing stays instant headless).
        let idle = !self.anim_running();
        let g = self.pending_ghosts.remove(&self.clients[i].id).or_else(|| self.closing_ghost(i));
        if let Some(g) = g {
            self.ghosts.push(g);
            if idle {
                self.begin_anim();
            }
            self.damage();
        }
        let gone = self.clients.remove(i);
        self.export_on_close(gone.id);
        if self.focus == Some(gone.id) {
            self.focus = None;
            let top = self.clients.iter().rev().find(|c| self.visible(c)).map(|c| c.id);
            self.focus_client(top);
        }
        if let Some(Drag::Move { id, .. } | Drag::Resize { id, .. }) = self.drag {
            if id == gone.id {
                self.drag = None;
            }
        }
        self.retile_all();
        self.refresh_pointer();
        self.damage();
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
            match &self.clients[i].shell {
                Shell::Wayland(win) => {
                    if let Some(t) = win.toplevel() {
                        t.with_pending_state(|s| s.size = Some((w.max(50), h.max(30)).into()));
                        t.send_pending_configure();
                    }
                }
                // The X11 window is told its new X geometry: its screen position and the size.
                Shell::X11(s) => {
                    let c = &self.clients[i];
                    let at = self.x11_screen_pos(c);
                    let _ = s.configure(Rectangle::new(at, Size::from((w.max(50), h.max(30)))));
                }
            }
        }
    }

    /// An X11 window's top-left corner in the global input space (its pinned screen spot).
    pub fn x11_screen_pos(&self, c: &Client) -> Point<i32, Logical> {
        let p = c.pin.unwrap_or_else(|| self.monitors.first().map(|m| m.origin).unwrap_or((0.0, 0.0).into()));
        let o = self.origin_of(c.pin_mon);
        Point::<f64, Logical>::from((p.x + o.x, p.y + o.y)).to_i32_round()
    }

    /// Re-send the pointer position so focus follows what is under it after the scene moved.
    pub fn refresh_pointer(&mut self) {
        let t = self.start_time.elapsed().as_millis() as u32;
        self.pointer_motion_to(self.cursor, t);
    }

    // ---- tags ---------------------------------------------------------------------------

    /// After a panic in a handler: drop what was half done (drags, gestures, hint mode, held
    /// touches) so the next event starts clean, and redraw.
    pub fn recover_after_panic(&mut self) {
        self.drag = None;
        self.gesture = None;
        self.fling = None;
        self.in_drag_motion = false;
        self.consumed_press = false;
        self.drag_from_client = false;
        self.hints_cancel();
        self.touch_cancel();
        self.damage();
    }

    /// The look of the tag in view (its own, over the global options).
    pub fn tag_look(&self) -> (crate::config::Color, String) {
        let l = &self.config.tag_looks[lowest_tag(self.tagset)];
        (
            l.root_color.unwrap_or(self.config.root_color),
            l.background_shader.clone().unwrap_or_else(|| self.config.background_shader.clone()),
        )
    }

    pub fn set_tagset(&mut self, tags: u32) {
        self.hints_cancel();
        let tags = tags & ALLTAGS;
        if tags == 0 {
            return;
        }
        let idle = !self.anim_running();
        let was: Vec<bool> = self.clients.iter().map(|c| self.visible(c)).collect();
        if self.monitors.is_empty() {
            return;
        }
        if self.tagset != tags {
            self.prev_tagset = self.tagset;
            self.tagset = tags;
            for m in &mut self.monitors {
                m.overview = None;
            }
            self.run_hook(crate::hooks::Hook::TagChange, None);
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
        self.tagset
    }

    pub fn send_to_tag(&mut self, tag: usize) {
        if let Some(i) = self.focus.and_then(|id| self.client_index(id)) {
            self.clients[i].tags = 1 << tag;
            let keep = self.visible(&self.clients[i]);
            if !keep {
                let top = self.clients.iter().rev().find(|c| self.visible(c)).map(|c| c.id);
                self.focus_client(top);
            }
            self.retile_all();
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
        let ts = self.tagset();
        let cur = self.cur;
        let vp = self.viewport();
        let pin_vp = self.viewport_of(self.clients[i].pin_mon);
        let c = &mut self.clients[i];
        if on {
            // Pinned to the monitor it is on when pinned; the spot is that monitor's screen px.
            c.pin_mon = cur;
            c.pin = Some(vp.canvas_to_screen((c.canvas.x, c.canvas.y)).into());
            c.tags = ALLTAGS;
        } else {
            let p = c.pin.take().unwrap();
            c.canvas = pin_vp.screen_to_canvas((p.x, p.y)).into();
            c.tags = ts;
        }
        self.focus_client(Some(id));
        self.retile_all();
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
            self.retile_all();
            self.refresh_pointer();
            self.damage();
        } else {
            self.clients[i].minimized = 0;
            if self.clients[i].pin.is_none() {
                self.clients[i].tags = self.tagset();
            }
            self.retile_all();
            self.focus_and_reveal(id);
            self.damage();
        }
    }

    /// Remember the view of the tag in use as bookmark `n` (0-based).
    pub fn bookmark_save(&mut self, n: usize) {
        let t = crate::canvas::lowest_tag(self.tagset);
        if let Some(m) = self.monitors.get_mut(self.cur) {
            m.bookmarks[t][n] = Some(m.target_viewport(self.tagset));
            self.ipc.dirty = true;
        }
    }

    /// Jump to bookmark `n` of the tag in use; false if it was never saved.
    pub fn bookmark_goto(&mut self, n: usize) -> bool {
        let t = crate::canvas::lowest_tag(self.tagset);
        let Some(v) = self.monitors.get(self.cur).and_then(|m| m.bookmarks[t][n]) else {
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
        let fs_mon = self.mon_of(&self.clients[i]);
        let screen = self.monitors.get(fs_mon).map(|m| m.size).unwrap_or((1280, 720).into());
        if let Some(s) = self.clients[i].x11().cloned() {
            let c = &mut self.clients[i];
            if c.fullscreen != on {
                c.fullscreen = on;
                if on {
                    c.fs_mon = fs_mon;
                    // A repeated request must not overwrite the saved size with the fullscreen one.
                    c.prev_size = Some(c.size()).filter(|s| s.w > 0 && s.h > 0);
                }
            }
            let prev = c.prev_size;
            let at = self.x11_screen_pos(&self.clients[i]);
            // An X11 window is told its state, and its X geometry: the monitor, or its old size.
            let _ = s.set_fullscreen(on);
            let size = if on { screen } else { prev.unwrap_or(screen) };
            let _ = s.configure(Rectangle::new(at, size));
            if on {
                self.focus_client(Some(id));
            }
            self.retile_all();
            self.refresh_pointer();
            self.damage();
            return;
        }
        let c = &mut self.clients[i];
        if c.fullscreen != on {
            c.fullscreen = on;
            if on {
                c.fs_mon = fs_mon;
                // A repeated request must not overwrite the saved size with the fullscreen one.
                c.prev_size = Some(c.size()).filter(|s| s.w > 0 && s.h > 0);
            }
        }
        let prev = c.prev_size;
        let Some(t) = (match &c.shell {
            Shell::Wayland(w) => w.toplevel().cloned(),
            Shell::X11(_) => None,
        }) else {
            return;
        };
        let output = self.monitors.get(fs_mon).and_then(|m| {
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
        self.retile_all();
        self.refresh_pointer();
        self.damage();
    }

    pub fn toggle_overview(&mut self) {
        // Only canvas windows: pinned and fullscreen ones are on screen anyway. The overview fits
        // what this monitor's view can see of them.
        let cur = self.cur;
        let rects: Vec<Rectangle<f64, Logical>> = self
            .clients
            .iter()
            .filter(|c| self.visible_on(cur, c) && self.place_on(cur, c, &crate::state::Fx::REST).on_canvas)
            .map(|c| c.rect())
            .collect();
        let screen = self.work_area();
        let tagset = self.tagset;
        let Some(m) = self.monitors.get_mut(cur) else { return };
        if let Some(prev) = m.overview.take() {
            *m.viewport_mut(tagset) = prev;
        } else if let Some(first) = rects.first() {
            let all = rects.iter().fold(*first, |a, r| a.merge(*r));
            let before = m.viewport(tagset);
            m.viewport_mut(tagset).fit((all.loc.x, all.loc.y, all.size.w, all.size.h), screen, 60.0);
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
                let p = self.prev_tagset;
                self.set_tagset(p);
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
                    self.release_from_tiling(id);
                    self.move_client_eased(id, c.x + v.0 * s, c.y + v.1 * s);
                }
            }
            Action::ResizeWindow(d) => {
                if let Some(i) = self.focus.and_then(|id| self.client_index(id)) {
                    let v = d.vec();
                    let s = self.config.move_step;
                    let sz = self.clients[i].size();
                    let id = self.clients[i].id;
                    self.release_from_tiling(id);
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
            Action::ToggleShortcutsInhibit => self.toggle_shortcuts_inhibit(),
            Action::JumpHints => self.hints_toggle(),
            Action::NextLayout => self.set_layout(None),
            Action::Tile(l) => self.tile_set(Some(*l)),
            Action::TileOnce(l) => self.tile_once(*l),
            Action::TileOff => self.tile_set(None),
            Action::TileCycle => self.tile_cycle(),
            Action::TileRatio(d) => self.tile_ratio(*d),
            Action::TileMasters(d) => self.tile_masters(*d),
            Action::ToggleFloat => {
                if let Some(i) = self.focus.and_then(|id| self.client_index(id)) {
                    let (id, on) = (self.clients[i].id, !self.clients[i].floating);
                    self.set_floating(id, on);
                }
            }
            Action::Hint(k) => self.hint_key(k.clone()),
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
        self.window_layouts.clear();
        self.refresh_layout();
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
                self.retile_all();
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

/// Dispatch the clients' requests. A panic in a protocol handler unwinds out of `dispatch_clients` (uwc uses
/// the pure Rust wayland backend, see Cargo.toml), leaving the requests that client had sent after the
/// guilty one buffered in the backend: the socket is empty, so nothing would wake the loop for them and the
/// client would hang waiting for its reply. So after a panic every client is dispatched again, then the panic
/// goes on to `serve` (main.rs), which counts it and drops the half-done state.
fn dispatch_surviving_panics(display: &mut Display<Uwc>, state: &mut Uwc) {
    use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
    let Err(first) = catch_unwind(AssertUnwindSafe(|| display.dispatch_clients(state).unwrap())) else {
        return;
    };
    state.recover_after_panic();
    // A bounded number of rounds: each one that panics again has used up a request, but a client could
    // send a lot of them.
    for _ in 0..16 {
        let mut ids = Vec::new();
        display.backend().handle().with_all_clients(|id| ids.push(id));
        let mut more = false;
        for id in ids {
            match catch_unwind(AssertUnwindSafe(|| display.backend().dispatch_single_client(state, id))) {
                Ok(Ok(n)) => more |= n > 0,
                Ok(Err(_)) => {}
                Err(_) => {
                    state.recover_after_panic();
                    more = true;
                }
            }
        }
        if !more {
            break;
        }
    }
    resume_unwind(first);
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
            pin_mon: 0,
            scale: 1.0,
            fullscreen: false,
            fs_mon: 0,
            deco: Default::default(),
            focused: false,
            alpha: 1.0,
            scale_fx,
            fade_ms: 160.0,
            deco_ids: std::array::from_fn(|_| Id::new()),
            key: 0,
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
