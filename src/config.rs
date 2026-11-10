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
    ToggleFullscreen,
    TogglePin,
    MinimizeWindow,
    RestoreWindow,
    /// 0-based slot.
    BookmarkSave(usize),
    BookmarkGoto(usize),
    DragMove,
    DragResize,
    DragPan,
    SwitchVt(i32),
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
    pub move_step: f64,
    pub zoom_scroll: f64,
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
    pub keys: Vec<KeyBind>,
    pub mouse: Vec<MouseBind>,
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
            move_step: 50.0,
            zoom_scroll: 1.1,
            new_window_offset: 32,
            cursor_theme: String::new(),
            cursor_size: 24,
            hardware_cursor: true,
            direct_scanout: true,
            explicit_sync: true,
            keys: Vec::new(),
            mouse: Vec::new(),
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
        for n in nodes.iter().filter(|n| n.name == "binds") {
            for b in &n.kids {
                cfg.add_bind(b).with_context(|| format!("bind `{}` (line {})", b.name, b.line))?;
            }
        }
        Ok(cfg)
    }

    fn set_option(&mut self, n: &Node, vars: &HashMap<String, String>) -> Result<()> {
        let name = n.name.as_str();
        if self.deco.set(n, vars)? {
            return Ok(());
        }
        let v = n.args.first().ok_or_else(|| anyhow!("missing value"))?;
        let boolean = || v.as_bool().ok_or_else(|| anyhow!("expected true or false"));
        let num = || v.as_num().ok_or_else(|| anyhow!("expected a number"));
        let col = || match v {
            Val::Str(s) => parse_color(s, vars),
            _ => Err(anyhow!("expected a color string")),
        };
        match name {
            "mod-key" => {
                self.mod_key = match v.as_str().unwrap_or("") {
                    "Super" | "Logo" => ModKey::Super,
                    "Alt" => ModKey::Alt,
                    "Ctrl" | "Control" => ModKey::Ctrl,
                    "Shift" => ModKey::Shift,
                    s => bail!("unknown mod-key `{s}` (Super, Alt, Ctrl or Shift)"),
                }
            }
            "sloppy-focus" => self.sloppy_focus = boolean()?,
            "warp-cursor" => self.warp_cursor = boolean()?,
            "root-color" => self.root_color = col()?,
            "fullscreen-bg" => self.fullscreen_bg = col()?,
            "pan-step" => self.pan_step = range(num()?, 1.0, 10000.0)?,
            "move-step" => self.move_step = range(num()?, 1.0, 10000.0)?,
            "zoom-scroll" => self.zoom_scroll = range(num()?, 1.01, 3.0)?,
            "new-window-offset" => self.new_window_offset = range(num()?, 0.0, 500.0)? as i32,
            "cursor-theme" => self.cursor_theme = v.as_str().ok_or_else(|| anyhow!("expected a theme name"))?.into(),
            "cursor-size" => self.cursor_size = range(num()?, 8.0, 256.0)? as u32,
            "hardware-cursor" => self.hardware_cursor = boolean()?,
            "direct-scanout" => self.direct_scanout = boolean()?,
            "explicit-sync" => self.explicit_sync = boolean()?,
            other => bail!("unknown option `{other}`"),
        }
        Ok(())
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
        let path = if let Some(rest) = file.strip_prefix("~/") {
            Path::new(&std::env::var_os("HOME").unwrap_or_default()).join(rest)
        } else if Path::new(file).is_absolute() {
            file.into()
        } else {
            dir.join(file)
        };
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
        "toggle-fullscreen" => Action::ToggleFullscreen,
        "toggle-pin" => Action::TogglePin,
        "minimize-window" => Action::MinimizeWindow,
        "restore-window" => Action::RestoreWindow,
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
        assert_eq!(c.deco.focus_shadow_size, -1.0, "any negative size means 'same as shadow-size'");
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
