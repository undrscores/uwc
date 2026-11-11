//! KDL config: `include`, `vars`, `options`, `binds`.
//!
//! The syntax is the one uwm uses (KDL v1, so `true`/`false` are bare). `config.def.kdl` is the
//! built-in default and the documentation; a user file only needs what it changes.
//! A file that fails to parse or validate is rejected as a whole.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use crate::kdl::{self, Node, Val};
use anyhow::{anyhow, bail, Context, Result};
use smithay::input::keyboard::{xkb, Keysym};

pub const DEFAULT_CONFIG: &str = include_str!("../config.def.kdl");

pub type Color = [f32; 4];

/// A gradient: 2 to 6 colors along `angle` (CSS degrees: 0 = first color at the bottom, 90 = at the left).
#[derive(Debug, Clone, PartialEq)]
pub struct Gradient {
    pub colors: Vec<Color>,
    pub angle: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModKey {
    Super,
    Alt,
    Ctrl,
    Shift,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Mods {
    pub logo: bool,
    pub alt: bool,
    pub ctrl: bool,
    pub shift: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

impl Dir {
    /// Unit vector in canvas space (y grows downwards).
    pub fn vec(self) -> (f64, f64) {
        match self {
            Dir::Left => (-1.0, 0.0),
            Dir::Right => (1.0, 0.0),
            Dir::Up => (0.0, -1.0),
            Dir::Down => (0.0, 1.0),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Spawn(Vec<String>),
    SpawnSh(String),
    CloseWindow,
    Quit,
    ViewTag(usize),
    ToggleViewTag(usize),
    SendToTag(usize),
    ViewAll,
    ViewPrevious,
    FocusNext,
    FocusPrevious,
    FocusDirection(Dir),
    MoveWindow(Dir),
    ResizeWindow(Dir),
    PanView(Dir),
    Zoom(f64),
    ZoomReset,
    CenterWindow,
    FitWindow,
    ToggleOverview,
    /// Take the keyboard back from a client that inhibited the shortcuts (works while inhibited).
    ToggleShortcutsInhibit,
    /// Label every window and jump to the one you type (see hints.rs).
    JumpHints,
    /// Switch to the next keyboard layout.
    NextLayout,
    /// Tile the tag in use with a layout (and keep it tiled), tile it once, stop tiling it, or step to
    /// the next layout (tiling.rs).
    Tile(crate::layout::Layout),
    TileOnce(crate::layout::Layout),
    TileOff,
    TileCycle,
    /// Change the share the master column / first split takes, or the number of masters, by this much.
    TileRatio(f64),
    TileMasters(i32),
    /// Keep the focused window out of the layouts, or put it back.
    ToggleFloat,
    /// A key typed while hint mode is up; never in a config.
    Hint(crate::hints::HintKey),
    ToggleFullscreen,
    TogglePin,
    MinimizeWindow,
    RestoreWindow,
    /// Multiply the focused window's own scale.
    ScaleWindow(f64),
    ScaleWindowReset,
    /// 0-based slot.
    BookmarkSave(usize),
    BookmarkGoto(usize),
    DragMove,
    DragResize,
    DragPan,
    SwitchVt(i32),
}

/// A tag's own look: the colour behind the canvas and a background shader. None = the global option.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TagLook {
    pub root_color: Option<Color>,
    pub background_shader: Option<String>,
}

fn parse_tag_look(n: &Node, vars: &HashMap<String, String>) -> Result<(usize, TagLook)> {
    let t =
        n.args.first().and_then(Val::as_num).ok_or_else(|| anyhow!("`tag` needs a number, e.g. tag 2 {{ ... }}"))?;
    if t.fract() != 0.0 || t < 1.0 || t > crate::canvas::TAGCOUNT as f64 {
        bail!("tag must be 1 to {}", crate::canvas::TAGCOUNT);
    }
    let mut look = TagLook::default();
    for k in &n.kids {
        let arg = k.arg_str(0).ok_or_else(|| anyhow!("`{}` needs a value in quotes (line {})", k.name, k.line))?;
        match k.name.as_str() {
            "root-color" => look.root_color = Some(parse_color(arg, vars)?),
            "background-shader" => look.background_shader = Some(arg.to_string()),
            other => bail!("unknown tag option `{other}` (line {}); known: root-color, background-shader", k.line),
        }
    }
    Ok((t as usize - 1, look))
}

#[derive(Debug, Clone)]
pub struct KeyBind {
    pub mods: Mods,
    pub sym: Keysym,
    pub action: Action,
}

#[derive(Debug, Clone)]
pub struct MouseBind {
    pub mods: Mods,
    pub button: u32,
    pub action: Action,
}

/// `swipe` or `pinch` in a `gestures { }` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GestureKind {
    Swipe,
    Pinch,
}

/// What one touchpad gesture does, from `gestures { swipe 3 { pan; } ... }`.
#[derive(Debug, Clone, PartialEq)]
pub enum GestureBind {
    /// Send the gesture to the client under the pointer (zwp-pointer-gestures-v1, e.g. a browser's pinch).
    Client,
    /// Pan the view with the fingers while they move; release while moving and it coasts on.
    Pan,
    /// Zoom the view with the pinch while it runs.
    Zoom,
    /// libinput reports a swipe as a pinch on some trackpads (the Magic Trackpad does): handle the
    /// pinch per the `swipe N` binds for its finger count.
    AsSwipe,
    /// Run this action when the fingers lift.
    Act(Action),
}

/// One `gestures { }` line: `swipe 4 left { focus-direction left; }`.
#[derive(Debug, Clone, PartialEq)]
pub struct GestureEntry {
    pub kind: GestureKind,
    pub fingers: u32,
    pub dir: Option<Dir>,
    pub bind: GestureBind,
}

/// The gesture defaults (uwm's): a 3-finger swipe pans the view, a 4-finger swipe focuses the
/// neighbouring window in the swipe's direction, a 2-finger pinch zooms. A pinch reported with a
/// finger count that has swipe binds is treated as that swipe (see GestureBind::AsSwipe's note).
pub fn default_gestures() -> Vec<GestureEntry> {
    let mut v = vec![GestureEntry { kind: GestureKind::Swipe, fingers: 3, dir: None, bind: GestureBind::Pan }];
    // The canvas follows the fingers, so a swipe left acts on what was to the right (uwm's rule).
    for (d, opposite) in [(Dir::Left, Dir::Right), (Dir::Right, Dir::Left), (Dir::Up, Dir::Down), (Dir::Down, Dir::Up)]
    {
        v.push(GestureEntry {
            kind: GestureKind::Swipe,
            fingers: 4,
            dir: Some(d),
            bind: GestureBind::Act(Action::FocusDirection(opposite)),
        });
    }
    v.push(GestureEntry { kind: GestureKind::Pinch, fingers: 2, dir: None, bind: GestureBind::Zoom });
    v
}

/// The direction a swipe of `(dx, dy)` mostly went, if it travelled at least `threshold` that way.
pub fn swipe_direction(dx: f64, dy: f64, threshold: f64) -> Option<Dir> {
    let (along, dir) = if dx.abs() >= dy.abs() {
        (dx.abs(), if dx < 0.0 { Dir::Left } else { Dir::Right })
    } else {
        (dy.abs(), if dy < 0.0 { Dir::Up } else { Dir::Down })
    };
    (along >= threshold).then_some(dir)
}

/// `input { keyboard { ... } }`. Empty strings mean "from XKB_DEFAULT_*".
#[derive(Debug, Clone, PartialEq)]
pub struct Keyboard {
    pub rules: String,
    pub model: String,
    pub layout: String,
    pub variant: String,
    pub options: Option<String>,
    pub repeat_rate: i32,
    pub repeat_delay: i32,
}

impl Default for Keyboard {
    fn default() -> Self {
        Keyboard {
            rules: String::new(),
            model: String::new(),
            layout: String::new(),
            variant: String::new(),
            options: None,
            repeat_rate: 25,
            repeat_delay: 600,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollMethod {
    None,
    TwoFinger,
    Edge,
    OnButtonDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickMethod {
    None,
    ButtonAreas,
    Clickfinger,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccelProfile {
    Flat,
    Adaptive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendEvents {
    Enabled,
    Disabled,
    DisabledOnExternalMouse,
}

/// `input { touchpad { ... } }` / `mouse { ... }`: libinput settings; `None` keeps the device default.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pointer {
    pub tap: Option<bool>,
    pub tap_and_drag: Option<bool>,
    pub drag_lock: Option<bool>,
    pub natural_scroll: Option<bool>,
    pub disable_while_typing: Option<bool>,
    pub left_handed: Option<bool>,
    pub middle_emulation: Option<bool>,
    pub scroll_method: Option<ScrollMethod>,
    pub click_method: Option<ClickMethod>,
    pub accel_profile: Option<AccelProfile>,
    pub accel_speed: Option<f64>,
    /// true = left/right/middle for 1/2/3 fingers ("lrm"), false = "lmr".
    pub tap_button_map_lrm: Option<bool>,
    pub send_events: Option<SendEvents>,
}

/// `monitor "NAME" { mode W H [HZ]; scale S; transform "90" }`.
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorConfig {
    pub name: String,
    pub mode: Option<(i32, i32, Option<f64>)>,
    pub scale: Option<f64>,
    pub transform: Option<smithay::utils::Transform>,
    pub off: bool,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub mod_key: ModKey,
    pub sloppy_focus: bool,
    pub warp_cursor: bool,
    pub root_color: Color,
    pub fullscreen_bg: Color,
    /// Window decoration: the options (rules override them per window).
    pub deco: crate::rules::Deco,
    /// `rule { }` sections, in file order.
    pub rules: Vec<crate::rules::Rule>,
    /// The `vars` of the file, so IPC `set` can use them too.
    pub vars: HashMap<String, String>,
    pub pan_step: f64,
    /// How long eased view moves (pan keys, centre, fit, bookmarks, home) take; 0 = jump.
    pub view_ms: f64,
    pub pan_momentum_ms: f64,
    /// Screen px the view moves per unit of swipe-pan finger travel (the canvas follows the fingers).
    pub swipe_gain: f64,
    /// Finger travel (same units as the swipe deltas) before a directional swipe counts as that direction.
    pub swipe_threshold: f64,
    pub edge_pan_size: f64,
    pub hint_keys: String,
    pub tile_gap: i32,
    pub tile_inner_gap: i32,
    pub tile_ratio: f64,
    pub tile_masters: u32,
    pub snap_distance: f64,
    pub snap_grid: f64,
    pub snap_color: Color,
    pub hint_scale: f64,
    pub edge_pan_speed: f64,
    /// A new window fades in and grows from `open_scale` over this long (0 = appears at once).
    pub open_ms: f64,
    pub open_scale: f64,
    /// Windows moved by key or IPC glide for this long (dragging is always instant); 0 = jump.
    pub move_ms: f64,
    /// Windows that come into view with a tag switch fade in over this long; 0 = appear at once.
    pub tag_ms: f64,
    /// A closing window fades out (towards `open_scale`) over this long; 0 = vanishes at once.
    pub close_ms: f64,
    /// The focus look (border, shadow, dim) crossfades between windows over this long; 0 = jumps.
    pub focus_ms: f64,
    pub move_step: f64,
    pub zoom_scroll: f64,
    /// Zoom is told to clients only after it stayed put this long.
    pub zoom_settle_ms: f64,
    /// Bounds of the zoom factor clients are asked to render for.
    pub zoom_render_scale_min: f64,
    pub zoom_render_scale_max: f64,
    pub new_window_offset: i32,
    /// xcursor theme ("" = $XCURSOR_THEME, else "default") and size in px.
    pub cursor_theme: String,
    pub cursor_size: u32,
    /// Real session only: use the GPU's cursor plane for the pointer.
    pub hardware_cursor: bool,
    /// Real session only: let a fullscreen-sized client buffer go straight to the screen.
    pub direct_scanout: bool,
    /// Real session only: offer linux-drm-syncobj-v1 when the GPU supports it.
    pub explicit_sync: bool,
    /// Real session only: offer dmabuf buffers to output capture clients (wlr-screencopy, ext-image-copy-capture).
    pub capture_dmabuf: bool,
    /// A GLSL file drawing the background behind everything ("" = root-color).
    pub background_shader: String,
    /// Remember the canvas (views, window layouts) across restarts.
    pub session_restore: bool,
    /// Run Xwayland so X11 apps work (real sessions only).
    pub xwayland: bool,
    /// Where relative paths in the config (shaders) are resolved from.
    pub dir: PathBuf,
    pub keys: Vec<KeyBind>,
    pub mouse: Vec<MouseBind>,
    /// `gestures { }` entries; starts as the defaults and file entries replace per key.
    pub gestures: Vec<GestureEntry>,
    pub hooks: crate::hooks::Hooks,
    /// `hot-zones { top-left { action; } ... }`.
    pub hot_zones: Vec<(crate::hotzones::Zone, Action)>,
    pub hot_zone_size: f64,
    pub per_window_layout: bool,
    pub hot_zone_delay_ms: f64,
    /// `tag N { ... }`: what a tag looks like, over the global options (index = tag - 1).
    pub tag_looks: Vec<TagLook>,
    pub keyboard: Keyboard,
    pub touchpad: Pointer,
    pub mouse_input: Pointer,
    pub monitors: Vec<MonitorConfig>,
    /// Every file that went into this config (the main one and its includes), for watching.
    pub files: Vec<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Config::parse(DEFAULT_CONFIG, None).expect("built-in config.def.kdl must be valid")
    }
}

pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;

pub fn config_path(arg: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = arg {
        return Some(p.into());
    }
    if let Some(p) = std::env::var_os("UWC_CONFIG") {
        return Some(p.into());
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".config")))?;
    Some(base.join("uwc/config.kdl"))
}

impl Config {
    /// Built-in defaults overlaid with the user's file (if it exists).
    pub fn load(path: Option<&Path>) -> Result<Config> {
        let Some(path) = path.filter(|p| p.exists()) else {
            return Ok(Config::default());
        };
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Config::parse(&text, Some(path))
    }

    /// Parse `text` on top of the built-in defaults. `path` is where includes are resolved from.
    pub fn parse(text: &str, path: Option<&Path>) -> Result<Config> {
        let mut files = Vec::new();
        let defaults = if path.is_some() {
            Some(kdl::parse(DEFAULT_CONFIG).map_err(|e| anyhow!("built-in config: {e}"))?)
        } else {
            None
        };
        let mut doc = kdl::parse(text).map_err(|e| anyhow!("{e}"))?;
        if let Some(p) = path {
            files.push(p.to_path_buf());
            let dir = p.parent().unwrap_or(Path::new("."));
            let mut depth = 0;
            expand_includes(&mut doc, dir, &mut files, &mut depth)?;
        }

        // The user's binds replace the default section as a whole, like in uwm; options merge.
        let mut nodes: Vec<Node> = Vec::new();
        if let Some(d) = defaults {
            let user_has_binds = doc.iter().any(|n| n.name == "binds");
            nodes.extend(d.into_iter().filter(|n| !(user_has_binds && n.name == "binds")));
        }
        nodes.extend(doc);

        let mut vars = HashMap::new();
        for n in nodes.iter().filter(|n| n.name == "vars") {
            for v in &n.kids {
                let val = v.arg_str(0).ok_or_else(|| anyhow!("vars: `{}` needs a color string", v.name))?;
                vars.insert(v.name.clone(), val.to_string());
            }
        }

        let mut cfg = Config {
            mod_key: ModKey::Super,
            sloppy_focus: true,
            warp_cursor: true,
            root_color: [0.133, 0.133, 0.133, 1.0],
            fullscreen_bg: [0.0, 0.0, 0.0, 1.0],
            deco: crate::rules::Deco::default(),
            rules: Vec::new(),
            vars: HashMap::new(),
            pan_step: 200.0,
            view_ms: 180.0,
            pan_momentum_ms: 600.0,
            swipe_gain: 2.0,
            swipe_threshold: 60.0,
            edge_pan_size: 12.0,
            hint_keys: "asdfghjkl".into(),
            tile_gap: 12,
            tile_inner_gap: 12,
            tile_ratio: 0.55,
            tile_masters: 1,
            snap_distance: 8.0,
            snap_grid: 0.0,
            snap_color: [1.0, 0.416, 0.757, 1.0],
            hint_scale: 3.0,
            edge_pan_speed: 900.0,
            open_ms: 160.0,
            open_scale: 0.92,
            move_ms: 140.0,
            tag_ms: 140.0,
            close_ms: 160.0,
            focus_ms: 0.0,
            move_step: 50.0,
            zoom_scroll: 1.1,
            zoom_settle_ms: 200.0,
            zoom_render_scale_min: 1.0,
            zoom_render_scale_max: 3.0,
            new_window_offset: 32,
            cursor_theme: String::new(),
            cursor_size: 24,
            hardware_cursor: true,
            direct_scanout: true,
            explicit_sync: true,
            capture_dmabuf: false,
            background_shader: String::new(),
            session_restore: true,
            xwayland: true,
            dir: path.and_then(|p| p.parent()).map(Path::to_path_buf).unwrap_or_default(),
            keys: Vec::new(),
            mouse: Vec::new(),
            gestures: default_gestures(),
            hooks: Default::default(),
            hot_zones: Vec::new(),
            hot_zone_size: 2.0,
            per_window_layout: false,
            hot_zone_delay_ms: 250.0,
            tag_looks: vec![TagLook::default(); crate::canvas::TAGCOUNT],
            keyboard: Keyboard::default(),
            touchpad: Pointer::default(),
            mouse_input: Pointer::default(),
            monitors: Vec::new(),
            files,
        };
        cfg.vars = vars.clone();
        // Options first, so `mod-key` applies to every bind whatever the section order.
        for n in &nodes {
            match n.name.as_str() {
                "vars" | "include" | "binds" => {}
                // uwm's `grid`, accepted at the top level too, ignored (see set_option).
                "grid" => {}
                "hot-zones" => {
                    for z in &n.kids {
                        let zone = crate::hotzones::Zone::parse(&z.name).ok_or_else(|| {
                            anyhow!("unknown hot zone `{}` (line {}); known: top-left, top-right, bottom-left, bottom-right, top, bottom, left, right", z.name, z.line)
                        })?;
                        let act = z.kids.first().ok_or_else(|| {
                            anyhow!("hot zone `{}` needs an action in braces (line {})", z.name, z.line)
                        })?;
                        let action =
                            parse_action(act).with_context(|| format!("hot zone `{}` (line {})", z.name, z.line))?;
                        cfg.hot_zones.retain(|(h, _)| *h != zone);
                        cfg.hot_zones.push((zone, action));
                    }
                }
                "tag" => {
                    let (i, look) = parse_tag_look(n, &vars).with_context(|| format!("tag (line {})", n.line))?;
                    cfg.tag_looks[i] = look;
                }
                "hooks" => {
                    cfg.hooks = crate::hooks::Hooks::parse(n).with_context(|| format!("hooks (line {})", n.line))?
                }
                "gestures" => {
                    for g in &n.kids {
                        cfg.add_gesture(g).with_context(|| format!("gesture `{}` (line {})", g.name, g.line))?;
                    }
                }
                "rule" => {
                    let r = crate::rules::Rule::parse(n, &vars).with_context(|| format!("rule (line {})", n.line))?;
                    cfg.rules.push(r);
                }
                "options" => {
                    for o in &n.kids {
                        cfg.set_option(o, &vars).with_context(|| format!("option `{}` (line {})", o.name, o.line))?;
                    }
                }
                "input" => {
                    for s in &n.kids {
                        let what = format!("input {} (line {})", s.name, s.line);
                        match s.name.as_str() {
                            "keyboard" => parse_keyboard(s, &mut cfg.keyboard).with_context(|| what)?,
                            "touchpad" => parse_pointer(s, &mut cfg.touchpad).with_context(|| what)?,
                            "mouse" => parse_pointer(s, &mut cfg.mouse_input).with_context(|| what)?,
                            other => bail!("unknown input section `{other}` (line {})", s.line),
                        }
                    }
                }
                "monitor" => {
                    let m = parse_monitor(n).with_context(|| format!("monitor (line {})", n.line))?;
                    // A later section for the same output replaces the earlier one.
                    cfg.monitors.retain(|o| o.name != m.name);
                    cfg.monitors.push(m);
                }
                other => bail!("unknown section `{other}` (line {})", n.line),
            }
        }
        // Shader files are watched like includes: saving one reloads the config, which recompiles it.
        let mut shaders: Vec<String> = vec![cfg.background_shader.clone(), cfg.deco.border_shader.clone()];
        shaders.extend(cfg.tag_looks.iter().filter_map(|l| l.background_shader.clone()));
        for n in nodes.iter().filter(|n| n.name == "rule") {
            shaders.extend(
                n.kids.iter().filter(|k| k.name == "border-shader").filter_map(|k| k.arg_str(0).map(str::to_string)),
            );
        }
        for s in shaders.into_iter().filter(|s| !s.is_empty()) {
            let p = resolve(&s, &cfg.dir);
            if !cfg.files.contains(&p) {
                cfg.files.push(p);
            }
        }
        for n in nodes.iter().filter(|n| n.name == "binds") {
            for b in &n.kids {
                cfg.add_bind(b).with_context(|| format!("bind `{}` (line {})", b.name, b.line))?;
            }
        }
        Ok(cfg)
    }

    fn set_option(&mut self, n: &Node, vars: &HashMap<String, String>) -> Result<()> {
        use crate::options::{set, DECO, GLOBAL};
        if set(DECO, &mut self.deco, n, vars)? || set(GLOBAL, self, n, vars)? {
            return Ok(());
        }
        // uwm's `grid` (a grid drawn on the canvas): accepted so uwm themes load as a whole; uwc
        // draws the canvas grid with a background-shader (see shaders/) and snaps with snap-grid.
        if n.name == "grid" {
            tracing::debug!("ignoring uwm's `grid` option (line {}); use background-shader / snap-grid", n.line);
            return Ok(());
        }
        bail!("unknown option `{}`", n.name)
    }

    /// IPC `set`: one line of the `options` block (`focus-color "#ff8800"`), applied to a copy so a
    /// bad value changes nothing. `mod-key` is refused: the binds were built with the old one.
    pub fn set_line(&self, line: &str) -> Result<Config> {
        let nodes = kdl::parse(line).map_err(|e| anyhow!("{e}"))?;
        let [n] = nodes.as_slice() else { bail!("expected one option") };
        if n.name == "mod-key" {
            bail!("mod-key can only be changed in the config file");
        }
        let mut c = self.clone();
        c.set_option(n, &self.vars).with_context(|| format!("option `{}`", n.name))?;
        Ok(c)
    }

    fn add_bind(&mut self, n: &Node) -> Result<()> {
        let (mods, key) = parse_combo(&n.name, self.mod_key)?;
        let act = n.kids.first().ok_or_else(|| anyhow!("needs an action in braces"))?;
        let action = parse_action(act)?;
        let button = match key {
            "MouseLeft" => Some(BTN_LEFT),
            "MouseRight" => Some(BTN_RIGHT),
            "MouseMiddle" => Some(BTN_MIDDLE),
            _ => None,
        };
        if let Some(button) = button {
            self.mouse.push(MouseBind { mods, button, action });
        } else {
            let sym = xkb::keysym_from_name(key, xkb::KEYSYM_CASE_INSENSITIVE);
            if sym.raw() == 0 {
                bail!("unknown key `{key}`");
            }
            self.keys.push(KeyBind { mods, sym: lower(sym), action });
        }
        Ok(())
    }

    /// One `gestures { }` line: `swipe 4 left { focus-direction left; }`, `pinch 2 { zoom; }`, ...
    fn add_gesture(&mut self, n: &Node) -> Result<()> {
        let kind = match n.name.as_str() {
            "swipe" => GestureKind::Swipe,
            "pinch" => GestureKind::Pinch,
            other => bail!("unknown gesture `{other}`; known: swipe, pinch"),
        };
        let fingers = n
            .args
            .first()
            .and_then(Val::as_num)
            .filter(|f| f.fract() == 0.0 && (1.0..=10.0).contains(f))
            .ok_or_else(|| anyhow!("`{}` needs a finger count from 1 to 10", n.name))? as u32;
        let dir = match n.args.get(1).and_then(Val::as_str) {
            None => None,
            Some("left") => Some(Dir::Left),
            Some("right") => Some(Dir::Right),
            Some("up") => Some(Dir::Up),
            Some("down") => Some(Dir::Down),
            Some(other) => bail!("unknown direction `{other}`; known: left, right, up, down"),
        };
        if kind == GestureKind::Pinch && dir.is_some() {
            bail!("a pinch has no direction");
        }
        if n.args.len() > 2 {
            bail!("`{} {fingers}` takes at most a direction, nothing more", n.name);
        }
        if n.kids.len() > 1 {
            bail!("`{} {fingers}` takes one handler, not {}", n.name, n.kids.len());
        }
        let act = n.kids.first().ok_or_else(|| anyhow!("`{} {fingers}` needs a handler in braces", n.name))?;
        if !act.kids.is_empty() {
            bail!("the handler of `{} {fingers}` takes no block", n.name);
        }
        let bind = match act.name.as_str() {
            "client" => GestureBind::Client,
            "pan" => {
                if kind == GestureKind::Pinch || dir.is_some() {
                    bail!("`pan` is a whole-swipe handler, not a pinch or direction one");
                }
                GestureBind::Pan
            }
            "zoom" => {
                if kind == GestureKind::Swipe || dir.is_some() {
                    bail!("`zoom` is a whole-pinch handler, not a swipe or direction one");
                }
                GestureBind::Zoom
            }
            "swipe" => {
                if kind == GestureKind::Swipe {
                    bail!("a swipe is already a swipe");
                }
                GestureBind::AsSwipe
            }
            _ => {
                // In a directional entry, `focus-direction` with no argument takes the entry's direction.
                let node = if act.name == "focus-direction" && act.args.is_empty() {
                    match dir {
                        Some(d) => Node {
                            args: vec![Val::Str(
                                match d {
                                    Dir::Left => "left",
                                    Dir::Right => "right",
                                    Dir::Up => "up",
                                    Dir::Down => "down",
                                }
                                .to_string(),
                            )],
                            ..act.clone()
                        },
                        None => act.clone(),
                    }
                } else {
                    act.clone()
                };
                GestureBind::Act(parse_action(&node)?)
            }
        };
        // A direction-less entry replaces every entry for the same kind and finger count, so
        // `swipe 4 { client; }` turns off all four 4-finger directions at once.
        match dir {
            Some(d) => self.gestures.retain(|g| !(g.kind == kind && g.fingers == fingers && g.dir == Some(d))),
            None => self.gestures.retain(|g| !(g.kind == kind && g.fingers == fingers)),
        }
        self.gestures.push(GestureEntry { kind, fingers, dir, bind });
        Ok(())
    }

    /// The plan for a swipe with `fingers` fingers; `None` = forward it to the client under the pointer.
    pub fn swipe_plan(&self, fingers: u32) -> Option<crate::state::Gesture> {
        let dirs: Vec<(Dir, Action)> = self
            .gestures
            .iter()
            .filter(|g| g.kind == GestureKind::Swipe && g.fingers == fingers && g.dir.is_some())
            .filter_map(|g| match &g.bind {
                GestureBind::Act(a) => g.dir.map(|d| (d, a.clone())),
                _ => None,
            })
            .collect();
        if !dirs.is_empty() {
            return Some(crate::state::Gesture::Dir { dirs, dx: 0.0, dy: 0.0 });
        }
        match self
            .gestures
            .iter()
            .find(|g| g.kind == GestureKind::Swipe && g.fingers == fingers && g.dir.is_none())
            .map(|g| &g.bind)
        {
            Some(GestureBind::Pan) => Some(crate::state::Gesture::Pan),
            Some(GestureBind::Act(a)) => Some(crate::state::Gesture::End(a.clone())),
            _ => None,
        }
    }

    /// The plan for a pinch with `fingers` fingers; `None` = forward it to the client. With no
    /// `pinch N` entry a pinch falls back to the `swipe N` plan: libinput reports 3/4-finger swipes
    /// as pinches on some trackpads (the Magic Trackpad does).
    pub fn pinch_plan(&self, fingers: u32) -> Option<crate::state::Gesture> {
        match self
            .gestures
            .iter()
            .find(|g| g.kind == GestureKind::Pinch && g.fingers == fingers && g.dir.is_none())
            .map(|g| &g.bind)
        {
            Some(GestureBind::AsSwipe) => self.swipe_plan(fingers),
            Some(GestureBind::Zoom) => Some(crate::state::Gesture::Zoom { last: 1.0 }),
            Some(GestureBind::Act(a)) => Some(crate::state::Gesture::End(a.clone())),
            // Client; Pan is rejected at parse time for a pinch but means the same here.
            Some(GestureBind::Client | GestureBind::Pan) => None,
            None => self.swipe_plan(fingers),
        }
    }
}

/// Binds match on the lowercase keysym, so `Mod+Shift+H` matches whatever Shift+H produces.
pub fn lower(sym: Keysym) -> Keysym {
    // Only the ASCII letters have case pairs that matter for binds.
    let r = sym.raw();
    if (0x41..=0x5a).contains(&r) {
        Keysym::from(r + 0x20)
    } else {
        sym
    }
}

pub(crate) fn range(v: f64, lo: f64, hi: f64) -> Result<f64> {
    if v.is_finite() && v >= lo && v <= hi {
        Ok(v)
    } else {
        bail!("{v} is out of range ({lo} to {hi})")
    }
}

fn parse_keyboard(n: &Node, k: &mut Keyboard) -> Result<()> {
    for f in &n.kids {
        let s = || f.arg_str(0).map(str::to_string).ok_or_else(|| anyhow!("`{}` needs a string", f.name));
        let num = || f.args.first().and_then(Val::as_num).ok_or_else(|| anyhow!("`{}` needs a number", f.name));
        match f.name.as_str() {
            "xkb-rules" => k.rules = s()?,
            "xkb-model" => k.model = s()?,
            "xkb-layout" => k.layout = s()?,
            "xkb-variant" => k.variant = s()?,
            "xkb-options" => k.options = Some(s()?),
            "repeat-rate" => k.repeat_rate = range(num()?, 0.0, 1000.0)? as i32,
            "repeat-delay" => k.repeat_delay = range(num()?, 0.0, 10000.0)? as i32,
            other => bail!("unknown keyboard setting `{other}`"),
        }
    }
    // A layout that does not compile rejects the whole config.
    let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_names(
        &ctx,
        &k.rules,
        &k.model,
        &k.layout,
        &k.variant,
        k.options.clone(),
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    );
    if keymap.is_none() {
        bail!(
            "the keymap (layout `{}`, variant `{}`, options `{:?}`) does not compile",
            k.layout,
            k.variant,
            k.options
        );
    }
    Ok(())
}

/// A flag: `tap` alone means true, `tap false` sets it off.
fn flag(f: &Node) -> Result<bool> {
    match f.args.first() {
        None => Ok(true),
        Some(Val::Bool(b)) => Ok(*b),
        _ => bail!("`{}` takes nothing, true or false", f.name),
    }
}

fn parse_pointer(n: &Node, p: &mut Pointer) -> Result<()> {
    for f in &n.kids {
        let s = || f.arg_str(0).ok_or_else(|| anyhow!("`{}` needs a string", f.name));
        match f.name.as_str() {
            "tap" => p.tap = Some(flag(f)?),
            "tap-and-drag" => p.tap_and_drag = Some(flag(f)?),
            "drag-lock" => p.drag_lock = Some(flag(f)?),
            "natural-scroll" => p.natural_scroll = Some(flag(f)?),
            "disable-while-typing" => p.disable_while_typing = Some(flag(f)?),
            "left-handed" => p.left_handed = Some(flag(f)?),
            "middle-emulation" => p.middle_emulation = Some(flag(f)?),
            "scroll-method" => {
                p.scroll_method = Some(match s()? {
                    "none" => ScrollMethod::None,
                    "two-finger" => ScrollMethod::TwoFinger,
                    "edge" => ScrollMethod::Edge,
                    "on-button-down" => ScrollMethod::OnButtonDown,
                    o => bail!("unknown scroll-method `{o}` (none, two-finger, edge, on-button-down)"),
                })
            }
            "click-method" => {
                p.click_method = Some(match s()? {
                    "none" => ClickMethod::None,
                    "button-areas" => ClickMethod::ButtonAreas,
                    "clickfinger" => ClickMethod::Clickfinger,
                    o => bail!("unknown click-method `{o}` (none, button-areas, clickfinger)"),
                })
            }
            "accel-profile" => {
                p.accel_profile = Some(match s()? {
                    "flat" => AccelProfile::Flat,
                    "adaptive" => AccelProfile::Adaptive,
                    o => bail!("unknown accel-profile `{o}` (flat, adaptive)"),
                })
            }
            "accel-speed" => {
                let v = f.args.first().and_then(Val::as_num).ok_or_else(|| anyhow!("accel-speed needs a number"))?;
                p.accel_speed = Some(range(v, -1.0, 1.0)?);
            }
            "tap-button-map" => {
                p.tap_button_map_lrm = Some(match s()? {
                    "lrm" => true,
                    "lmr" => false,
                    o => bail!("unknown tap-button-map `{o}` (lrm, lmr)"),
                })
            }
            "send-events" => {
                p.send_events = Some(match s()? {
                    "enabled" => SendEvents::Enabled,
                    "disabled" => SendEvents::Disabled,
                    "disabled-on-external-mouse" => SendEvents::DisabledOnExternalMouse,
                    o => bail!("unknown send-events `{o}` (enabled, disabled, disabled-on-external-mouse)"),
                })
            }
            other => bail!("unknown pointer setting `{other}`"),
        }
    }
    Ok(())
}

fn parse_monitor(n: &Node) -> Result<MonitorConfig> {
    use smithay::utils::Transform;
    let name = n.arg_str(0).ok_or_else(|| anyhow!("monitor needs an output name, e.g. monitor \"eDP-1\""))?;
    let mut m = MonitorConfig { name: name.into(), mode: None, scale: None, transform: None, off: false };
    for f in &n.kids {
        let num = |i: usize| f.args.get(i).and_then(Val::as_num);
        match f.name.as_str() {
            "mode" => {
                let (w, h) = (num(0), num(1));
                let (Some(w), Some(h)) = (w, h) else { bail!("mode needs a width and a height") };
                let hz = match num(2) {
                    Some(hz) => Some(range(hz, 1.0, 1000.0)?),
                    None => None,
                };
                m.mode = Some((range(w, 1.0, 16384.0)? as i32, range(h, 1.0, 16384.0)? as i32, hz));
            }
            "scale" => m.scale = Some(range(num(0).ok_or_else(|| anyhow!("scale needs a number"))?, 0.25, 10.0)?),
            "transform" => {
                m.transform = Some(match f.arg_str(0).unwrap_or("") {
                    "normal" => Transform::Normal,
                    "90" => Transform::_90,
                    "180" => Transform::_180,
                    "270" => Transform::_270,
                    "flipped" => Transform::Flipped,
                    "flipped-90" => Transform::Flipped90,
                    "flipped-180" => Transform::Flipped180,
                    "flipped-270" => Transform::Flipped270,
                    o => bail!("unknown transform `{o}`"),
                })
            }
            "off" => m.off = flag(f)?,
            // uwm settings that need several monitors or wallpapers; accepted so uwm configs load.
            "position" | "wallpaper" | "wallpaper-fit" => {}
            other => bail!("unknown monitor setting `{other}`"),
        }
    }
    Ok(m)
}

/// A path from the config: "~/" is the home directory, a relative one is next to the config file.
pub fn resolve(file: &str, dir: &Path) -> PathBuf {
    if let Some(rest) = file.strip_prefix("~/") {
        Path::new(&std::env::var_os("HOME").unwrap_or_default()).join(rest)
    } else if Path::new(file).is_absolute() {
        file.into()
    } else {
        dir.join(file)
    }
}

fn expand_includes(doc: &mut Vec<Node>, dir: &Path, files: &mut Vec<PathBuf>, depth: &mut u32) -> Result<()> {
    *depth += 1;
    if *depth > 8 {
        bail!("include nested too deeply");
    }
    let mut out = Vec::new();
    for n in std::mem::take(doc) {
        if n.name != "include" {
            out.push(n);
            continue;
        }
        let file = n.arg_str(0).ok_or_else(|| anyhow!("include needs a file name (line {})", n.line))?;
        let path = resolve(file, dir);
        let text = std::fs::read_to_string(&path).with_context(|| format!("include {}", path.display()))?;
        let mut inc = kdl::parse(&text).map_err(|e| anyhow!("{}: {e}", path.display()))?;
        files.push(path.clone());
        expand_includes(&mut inc, path.parent().unwrap_or(dir), files, depth)?;
        out.extend(inc);
    }
    *doc = out;
    *depth -= 1;
    Ok(())
}

pub fn parse_color(s: &str, vars: &HashMap<String, String>) -> Result<Color> {
    let s = match s.strip_prefix('$') {
        Some(name) => vars.get(name).ok_or_else(|| anyhow!("unknown variable ${name}"))?.as_str(),
        None => s,
    };
    let h = s.strip_prefix('#').ok_or_else(|| anyhow!("color `{s}` must look like #rrggbb or #rrggbbaa"))?;
    if !(h.len() == 6 || h.len() == 8) || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("color `{s}` must look like #rrggbb or #rrggbbaa");
    }
    let b = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).unwrap() as f32 / 255.0;
    Ok([b(0), b(2), b(4), if h.len() == 8 { b(6) } else { 1.0 }])
}

pub fn parse_combo(combo: &str, mod_key: ModKey) -> Result<(Mods, &str)> {
    let mut mods = Mods::default();
    let mut parts = combo.split('+').collect::<Vec<_>>();
    let key = parts.pop().filter(|k| !k.is_empty()).ok_or_else(|| anyhow!("empty key"))?;
    for m in parts {
        let m = if m == "Mod" {
            match mod_key {
                ModKey::Super => "Super",
                ModKey::Alt => "Alt",
                ModKey::Ctrl => "Ctrl",
                ModKey::Shift => "Shift",
            }
        } else {
            m
        };
        match m {
            "Super" | "Logo" | "Win" => mods.logo = true,
            "Alt" => mods.alt = true,
            "Ctrl" | "Control" => mods.ctrl = true,
            "Shift" => mods.shift = true,
            other => bail!("unknown modifier `{other}`"),
        }
    }
    Ok((mods, key))
}

fn parse_action(n: &Node) -> Result<Action> {
    let args: &Vec<Val> = &n.args;
    let strs = || -> Result<Vec<String>> {
        args.iter()
            .map(|v| v.as_str().map(str::to_string).ok_or_else(|| anyhow!("expected string arguments")))
            .collect()
    };
    let tag = || -> Result<usize> {
        let t = args.first().and_then(Val::as_num).ok_or_else(|| anyhow!("needs a tag number"))?;
        if t.fract() != 0.0 || t < 1.0 || t > crate::canvas::TAGCOUNT as f64 {
            bail!("tag must be 1 to {}", crate::canvas::TAGCOUNT);
        }
        Ok(t as usize - 1)
    };
    let dir = || -> Result<Dir> {
        match args.first().and_then(|v| v.as_str()) {
            Some("left") => Ok(Dir::Left),
            Some("right") => Ok(Dir::Right),
            Some("up") => Ok(Dir::Up),
            Some("down") => Ok(Dir::Down),
            _ => bail!("needs a direction: left, right, up or down"),
        }
    };
    Ok(match n.name.as_str() {
        "spawn" => {
            let s = strs()?;
            if s.is_empty() {
                bail!("spawn needs a command");
            }
            Action::Spawn(s)
        }
        "spawn-sh" => Action::SpawnSh(strs()?.into_iter().next().ok_or_else(|| anyhow!("spawn-sh needs a command"))?),
        "close-window" => Action::CloseWindow,
        "quit" => Action::Quit,
        "view-tag" => Action::ViewTag(tag()?),
        "toggle-view-tag" => Action::ToggleViewTag(tag()?),
        "send-to-tag" => Action::SendToTag(tag()?),
        "view-all" => Action::ViewAll,
        "view-previous" => Action::ViewPrevious,
        "focus-next-window" => Action::FocusNext,
        "focus-previous-window" => Action::FocusPrevious,
        "focus-direction" => Action::FocusDirection(dir()?),
        "move-window" => Action::MoveWindow(dir()?),
        "resize-window" => Action::ResizeWindow(dir()?),
        "pan-view" => Action::PanView(dir()?),
        "zoom" => {
            let z = args.first().and_then(Val::as_num).ok_or_else(|| anyhow!("zoom needs a factor"))?;
            Action::Zoom(range(z, 0.1, 10.0)?)
        }
        "zoom-reset" => Action::ZoomReset,
        "center-window" => Action::CenterWindow,
        "fit-window" => Action::FitWindow,
        "toggle-overview" => Action::ToggleOverview,
        "toggle-shortcuts-inhibit" => Action::ToggleShortcutsInhibit,
        "jump-hints" => Action::JumpHints,
        "next-layout" => Action::NextLayout,
        "tile" | "tile-once" => {
            let name = args.first().and_then(Val::as_str).unwrap_or("");
            let l = crate::layout::Layout::parse(name)
                .ok_or_else(|| anyhow!("{} needs a layout: {}", n.name, crate::layout::Layout::names()))?;
            if n.name == "tile" {
                Action::Tile(l)
            } else {
                Action::TileOnce(l)
            }
        }
        "tile-off" => Action::TileOff,
        "tile-cycle" => Action::TileCycle,
        "tile-ratio" => {
            let d =
                args.first().and_then(Val::as_num).ok_or_else(|| anyhow!("tile-ratio needs a change, e.g. 0.05"))?;
            Action::TileRatio(range(d, -0.8, 0.8)?)
        }
        "tile-masters" => {
            let d = args.first().and_then(Val::as_num).ok_or_else(|| anyhow!("tile-masters needs a change, e.g. 1"))?;
            Action::TileMasters(range(d.round(), -16.0, 16.0)? as i32)
        }
        "toggle-float" => Action::ToggleFloat,
        "toggle-fullscreen" => Action::ToggleFullscreen,
        "toggle-pin" => Action::TogglePin,
        "minimize-window" => Action::MinimizeWindow,
        "restore-window" => Action::RestoreWindow,
        "scale-window" => {
            let f = args.first().and_then(Val::as_num).ok_or_else(|| anyhow!("scale-window needs a factor"))?;
            Action::ScaleWindow(range(f, 0.1, 10.0)?)
        }
        "scale-window-reset" => Action::ScaleWindowReset,
        "bookmark-save" | "bookmark-goto" => {
            let b = args.first().and_then(Val::as_num).ok_or_else(|| anyhow!("needs a bookmark number 1 to 9"))?;
            if b.fract() != 0.0 || !(1.0..=9.0).contains(&b) {
                bail!("bookmarks are 1 to 9");
            }
            if n.name == "bookmark-save" {
                Action::BookmarkSave(b as usize - 1)
            } else {
                Action::BookmarkGoto(b as usize - 1)
            }
        }
        "drag-move" => Action::DragMove,
        "drag-resize" => Action::DragResize,
        "drag-pan" => Action::DragPan,
        "switch-vt" => {
            let vt = args.first().and_then(Val::as_num).ok_or_else(|| anyhow!("switch-vt needs a number"))?;
            Action::SwitchVt(range(vt, 1.0, 63.0)? as i32)
        }
        other => bail!("unknown action `{other}`"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_default_is_valid() {
        let c = Config::default();
        assert!(c.keys.len() > 20, "default binds missing");
        assert!(c.mouse.iter().any(|m| m.action == Action::DragMove));
    }

    #[test]
    fn gesture_defaults_match_uwm() {
        let c = Config::default();
        // A 3-finger swipe pans, a 4-finger swipe focuses the neighbour that way, a 2-finger pinch zooms.
        assert_eq!(c.swipe_plan(3), Some(crate::state::Gesture::Pan));
        assert_eq!(
            c.swipe_plan(4),
            Some(crate::state::Gesture::Dir {
                // the canvas follows the fingers: a swipe left focuses what was to the right
                dirs: vec![
                    (Dir::Left, Action::FocusDirection(Dir::Right)),
                    (Dir::Right, Action::FocusDirection(Dir::Left)),
                    (Dir::Up, Action::FocusDirection(Dir::Down)),
                    (Dir::Down, Action::FocusDirection(Dir::Up)),
                ],
                dx: 0.0,
                dy: 0.0,
            })
        );
        assert_eq!(c.pinch_plan(2), Some(crate::state::Gesture::Zoom { last: 1.0 }));
        // libinput reports a 3/4-finger swipe as a pinch on some trackpads (the Magic Trackpad):
        // with no `pinch N` entry the pinch falls back to the swipe plan.
        assert_eq!(c.pinch_plan(3), Some(crate::state::Gesture::Pan));
        assert!(matches!(c.pinch_plan(4), Some(crate::state::Gesture::Dir { .. })));
        // Unbound finger counts go to the client.
        assert_eq!(c.swipe_plan(2), None);
        assert_eq!(c.pinch_plan(5), None);
    }

    #[test]
    fn gestures_section_parses_and_replaces() {
        let c = Config::parse(
            "gestures {\n  swipe 3 { client; }\n  swipe 4 left { toggle-overview; }\n  swipe 4 up { focus-direction; }\n  pinch 2 { client; }\n  pinch 3 { zoom; }\n}",
            None,
        )
        .unwrap();
        // `swipe 3 { client; }` replaced the default pan; `pinch 3 { zoom; }` now wins over the fallback.
        assert_eq!(c.swipe_plan(3), None);
        assert_eq!(c.pinch_plan(3), Some(crate::state::Gesture::Zoom { last: 1.0 }));
        // A directional entry replaces only that direction; `focus-direction` with no argument
        // takes the entry's direction.
        assert_eq!(
            c.swipe_plan(4),
            Some(crate::state::Gesture::Dir {
                dirs: vec![
                    (Dir::Right, Action::FocusDirection(Dir::Left)),
                    (Dir::Down, Action::FocusDirection(Dir::Up)),
                    (Dir::Left, Action::ToggleOverview),
                    (Dir::Up, Action::FocusDirection(Dir::Up)),
                ],
                dx: 0.0,
                dy: 0.0,
            })
        );
        assert_eq!(c.pinch_plan(2), None, "pinch 2 client forwards two-finger pinches to the app");
        // A direction-less entry replaces every entry for that kind and finger count.
        let c = Config::parse("gestures { swipe 4 { client; } }", None).unwrap();
        assert_eq!(c.swipe_plan(4), None, "swipe 4 client turns off all four 4-finger directions");
        // Bad entries are errors, not panics.
        assert!(Config::parse("gestures { swipe { pan; } }", None).is_err());
        assert!(Config::parse("gestures { swipe 0 { pan; } }", None).is_err());
        assert!(Config::parse("gestures { swipe 3 sideways { pan; } }", None).is_err());
        assert!(Config::parse("gestures { pinch 3 left { zoom; } }", None).is_err());
        assert!(Config::parse("gestures { swipe 3 { zoom; } }", None).is_err());
        assert!(Config::parse("gestures { pinch 2 { pan; } }", None).is_err());
        assert!(Config::parse("gestures { tap 3 { pan; } }", None).is_err());
        // Extra arguments or handlers are errors, not silently ignored.
        assert!(Config::parse("gestures { swipe 3 left extra { pan; } }", None).is_err());
        assert!(Config::parse("gestures { swipe 3 { pan; zoom; } }", None).is_err());
        assert!(Config::parse("gestures { swipe 3 { pan { deep; } } }", None).is_err());
    }

    #[test]
    fn swipe_direction_needs_the_threshold() {
        assert_eq!(swipe_direction(-100.0, 10.0, 60.0), Some(Dir::Left));
        assert_eq!(swipe_direction(100.0, -10.0, 60.0), Some(Dir::Right));
        assert_eq!(swipe_direction(5.0, -80.0, 60.0), Some(Dir::Up));
        assert_eq!(swipe_direction(5.0, 80.0, 60.0), Some(Dir::Down));
        assert_eq!(swipe_direction(-59.9, 0.0, 60.0), None, "not far enough: the swipe bounces back");
        assert_eq!(swipe_direction(0.0, 0.0, 60.0), None);
        // Diagonal: the dominant axis wins.
        assert_eq!(swipe_direction(-70.0, 69.0, 60.0), Some(Dir::Left));
        assert_eq!(swipe_direction(10.0, 100.0, 60.0), Some(Dir::Down));
    }

    #[test]
    fn options_and_vars() {
        let c = Config::parse(
            "vars { accent \"#ff8800\" }\noptions { focus-color \"$accent\"\n sloppy-focus false\n border-width 3 }",
            Some(Path::new("/nonexistent/x.kdl")),
        )
        .unwrap();
        assert_eq!(c.deco.border_width, 3);
        assert!(!c.sloppy_focus);
        assert!((c.deco.focus_color[0] - 1.0).abs() < 1e-6 && (c.deco.focus_color[1] - 0.533).abs() < 1e-3);
    }

    /// Garbage must be an error, never a panic or a stack overflow: configs come from files a user pastes
    /// from the internet, and `rule { }` lines arrive over the control socket.
    #[test]
    fn random_text_never_crashes_the_parsers() {
        let tokens = [
            "options",
            "binds",
            "rule",
            "match",
            "open",
            "tag",
            "hot-zones",
            "hooks",
            "input",
            "keyboard",
            "gestures",
            "swipe",
            "pinch",
            "left",
            "pan",
            "client",
            "monitor",
            "include",
            "vars",
            "{",
            "}",
            "{",
            "}",
            ";",
            "\n",
            " ",
            "\"",
            "\"x\"",
            "\\",
            "//",
            "/*",
            "*/",
            "1",
            "-1",
            "1e999",
            "nan",
            "0x10",
            "#ff0000",
            "\"#zzzz\"",
            "Mod+Q",
            "Mod+",
            "spawn",
            "zoom",
            "app-id=",
            "title=\"*\"",
            "corner-radius",
            "border-width",
            "99999999999",
            "\u{0}",
            "\u{1F600}",
            "$accent",
            "=",
            "r#\"raw\"#",
        ];
        let mut x: u64 = 0x2545F4914F6CDD1D;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for _ in 0..3000 {
            let n = (next() % 40) as usize;
            let text: String = (0..n).map(|_| tokens[(next() % tokens.len() as u64) as usize]).collect();
            if let Ok(nodes) = kdl::parse(&text) {
                for n in &nodes {
                    let _ = crate::rules::Rule::parse(n, &HashMap::new());
                }
            }
            let _ = Config::parse(&text, Some(Path::new("/x/c.kdl")));
        }
        // Deep nesting is refused, not recursed into until the stack is gone.
        for depth in [100, 10_000, 200_000] {
            let deep = format!("{}{}", "a {".repeat(depth), "}".repeat(depth));
            let _ = kdl::parse(&deep);
        }
    }

    #[test]
    fn the_shipped_themes_are_valid_configs() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("themes");
        let mut n = 0;
        for e in std::fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            let text = std::fs::read_to_string(&p).unwrap();
            let c = Config::parse(&text, Some(&p)).unwrap_or_else(|e| panic!("{}: {e:#}", p.display()));
            assert!(c.deco.focus_gradient.is_some(), "{} sets a focus gradient", p.display());
            n += 1;
        }
        assert!(n >= 4, "themes are shipped");
        // And they work through `include`, under the user's own options.
        let tmp = std::env::temp_dir().join(format!("uwc-theme-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let inc = format!("include \"{}/nord.kdl\"\noptions {{ corner-radius 3 }}", dir.display());
        let c = Config::parse(&inc, Some(&tmp.join("c.kdl"))).unwrap();
        assert_eq!(c.deco.corner_radius, 3.0);
        assert!(c.deco.focus_gradient.is_some());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn tags_can_have_their_own_look() {
        let p = Some(Path::new("/x/c.kdl"));
        let c = Config::parse(
            "options { root-color \"#112233\" }\ntag 2 { root-color \"#445566\"\n background-shader \"w.glsl\" }",
            p,
        )
        .unwrap();
        assert_eq!(c.tag_looks[1].background_shader.as_deref(), Some("w.glsl"));
        assert!((c.tag_looks[1].root_color.unwrap()[0] - 0x44 as f32 / 255.0).abs() < 1e-3);
        assert_eq!(c.tag_looks[0], TagLook::default(), "other tags use the global options");
        assert!(c.files.iter().any(|f| f.ends_with("w.glsl")), "its shader is watched for changes");
        for bad in ["tag 0 { }", "tag 10 { }", "tag { }", "tag 1 { sparkle \"x\" }", "tag 1 { root-color \"red\" }"] {
            assert!(Config::parse(bad, p).is_err(), "{bad}");
        }
    }

    #[test]
    fn uwm_grid_is_accepted_and_ignored() {
        let p = Some(Path::new("/x/c.kdl"));
        // In the options block and at the top level, with uwm's arguments: a theme that sets it
        // loads as a whole instead of being rejected.
        let c = Config::parse("options { grid 32 \"#1a1b26\"; border-width 2 }", p).unwrap();
        assert_eq!(c.deco.border_width, 2);
        let c = Config::parse("grid 32 \"#1a1b26\"\noptions { corner-radius 3 }", p).unwrap();
        assert_eq!(c.deco.corner_radius, 3.0);
        // A made-up option is still rejected.
        assert!(Config::parse("options { sparkle 1 }", p).is_err());
    }

    #[test]
    fn user_binds_replace_defaults() {
        let c = Config::parse("binds { Mod+T { spawn \"foot\"; } }", Some(Path::new("/x/c.kdl"))).unwrap();
        assert_eq!(c.keys.len(), 1);
        assert_eq!(c.keys[0].action, Action::Spawn(vec!["foot".into()]));
    }

    #[test]
    fn rejects_bad_input() {
        let p = Some(Path::new("/x/c.kdl"));
        for bad in [
            "options { nonsense 1 }",
            "options { border-width 9999 }",
            "options { root-color \"red\" }",
            "options { focus-color \"$nope\" }",
            "binds { Mod+T { frobnicate; } }",
            "binds { Hyper+T { quit; } }",
            "binds { Mod+NoSuchKey { quit; } }",
            "binds { Mod+1 { view-tag 10; } }",
            "wat { }",
            "options {",
        ] {
            assert!(Config::parse(bad, p).is_err(), "should reject: {bad}");
        }
    }

    #[test]
    fn input_and_monitor_sections() {
        let c = Config::parse(
            "input {\n keyboard { xkb-layout \"de\"; xkb-options \"ctrl:nocaps\"; repeat-rate 40 }\n touchpad { tap; natural-scroll false; scroll-method \"edge\"; accel-speed 0.5 }\n mouse { accel-profile \"flat\" }\n}\nmonitor \"eDP-1\" { mode 1920 1080 60; scale 1.5; transform \"90\"; position 0 0 }",
            Some(Path::new("/x/c.kdl")),
        )
        .unwrap();
        assert_eq!(c.keyboard.layout, "de");
        assert_eq!(c.keyboard.repeat_rate, 40);
        assert_eq!(c.touchpad.tap, Some(true));
        assert_eq!(c.touchpad.natural_scroll, Some(false));
        assert_eq!(c.touchpad.scroll_method, Some(ScrollMethod::Edge));
        assert_eq!(c.mouse_input.accel_profile, Some(AccelProfile::Flat));
        let m = &c.monitors[0];
        assert_eq!((m.name.as_str(), m.mode, m.scale), ("eDP-1", Some((1920, 1080, Some(60.0))), Some(1.5)));
        assert_eq!(m.transform, Some(smithay::utils::Transform::_90));
    }

    #[test]
    fn bad_input_sections_are_rejected() {
        let p = Some(Path::new("/x/c.kdl"));
        for bad in [
            "input { keyboard { xkb-layout \"no-such-layout\" } }",
            "input { touchpad { tap 3 } }",
            "input { touchpad { accel-speed 2 } }",
            "input { touchpad { scroll-method \"sideways\" } }",
            "input { joystick { } }",
            "monitor { scale 2 }",
            "monitor \"X\" { scale 0 }",
            "monitor \"X\" { mode 1920 }",
            "binds { Ctrl+Alt+F1 { switch-vt 0; } }",
        ] {
            assert!(Config::parse(bad, p).is_err(), "should reject: {bad}");
        }
    }

    #[test]
    fn decoration_options() {
        let c = Config::parse(
            "vars { a \"#ff0000\"; b \"#0000ff\" }\noptions { corner-radius 12; shadow-size 30; focus-gradient \"$a\" \"$b\" angle=45; inactive-opacity 0.9; inactive-dim 0.2; focus-shadow-size -5 }",
            Some(Path::new("/x/c.kdl")),
        )
        .unwrap();
        assert_eq!(
            (c.deco.corner_radius, c.deco.shadow_size, c.deco.inactive_opacity, c.deco.inactive_dim),
            (12.0, 30.0, 0.9, 0.2)
        );
        let g = c.deco.focus_gradient.as_ref().unwrap();
        assert_eq!((g.colors.len(), g.angle), (2, 45.0));
        assert_eq!(c.deco.focus_shadow_size, None, "any negative size means 'same as shadow-size'");
        // An empty gradient turns it off.
        let c2 = Config::parse("options { focus-gradient }", Some(Path::new("/x/c.kdl"))).unwrap();
        assert!(c2.deco.focus_gradient.is_none());
        for bad in [
            "options { focus-gradient \"#fff\" }",
            "options { focus-gradient \"#fff\" \"#000\" spin=1 }",
            "options { opacity 1.5 }",
            "options { corner-radius -1 }",
        ] {
            assert!(Config::parse(bad, Some(Path::new("/x/c.kdl"))).is_err(), "should reject: {bad}");
        }
    }

    #[test]
    fn set_line_validates_and_uses_vars() {
        let c = Config::parse("vars { accent \"#00ff00\" }", Some(Path::new("/x/c.kdl"))).unwrap();
        let c2 = c.set_line("focus-color \"$accent\"").unwrap();
        assert_eq!(c2.deco.focus_color, [0.0, 1.0, 0.0, 1.0]);
        let c3 = c.set_line("border-gradient \"#ff0000\" \"#0000ff\" angle=10").unwrap();
        assert_eq!(c3.deco.border_gradient.unwrap().angle, 10.0);
        assert!(c.set_line("mod-key \"Alt\"").is_err());
        assert!(c.set_line("opacity 7").is_err());
        assert!(c.set_line("nonsense 1").is_err());
        assert!(c.set_line("a 1; b 2").is_err());
    }

    #[test]
    fn mod_key_option_changes_mod() {
        let c = Config::parse("options { mod-key \"Alt\" }\nbinds { Mod+T { quit; } }", Some(Path::new("/x/c.kdl")))
            .unwrap();
        assert!(c.keys[0].mods.alt && !c.keys[0].mods.logo);
    }
}
