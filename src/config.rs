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
    DragMove,
    DragResize,
    DragPan,
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

#[derive(Debug, Clone)]
pub struct Config {
    pub mod_key: ModKey,
    pub sloppy_focus: bool,
    pub warp_cursor: bool,
    pub border_width: i32,
    pub root_color: Color,
    pub border_color: Color,
    pub focus_color: Color,
    pub urgent_color: Color,
    pub pan_step: f64,
    pub move_step: f64,
    pub zoom_scroll: f64,
    pub new_window_offset: i32,
    pub keys: Vec<KeyBind>,
    pub mouse: Vec<MouseBind>,
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
            border_width: 0,
            root_color: [0.133, 0.133, 0.133, 1.0],
            border_color: [0.267, 0.267, 0.267, 1.0],
            focus_color: [0.0, 0.333, 0.467, 1.0],
            urgent_color: [1.0, 0.0, 0.0, 1.0],
            pan_step: 200.0,
            move_step: 50.0,
            zoom_scroll: 1.1,
            new_window_offset: 32,
            keys: Vec::new(),
            mouse: Vec::new(),
            files,
        };
        // Options first, so `mod-key` applies to every bind whatever the section order.
        for n in &nodes {
            match n.name.as_str() {
                "vars" | "include" | "binds" => {}
                "options" => {
                    for o in &n.kids {
                        cfg.set_option(o, &vars).with_context(|| format!("option `{}` (line {})", o.name, o.line))?;
                    }
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
            "border-width" => self.border_width = range(num()?, 0.0, 64.0)? as i32,
            "root-color" => self.root_color = col()?,
            "border-color" => self.border_color = col()?,
            "focus-color" => self.focus_color = col()?,
            "urgent-color" => self.urgent_color = col()?,
            "pan-step" => self.pan_step = range(num()?, 1.0, 10000.0)?,
            "move-step" => self.move_step = range(num()?, 1.0, 10000.0)?,
            "zoom-scroll" => self.zoom_scroll = range(num()?, 1.01, 3.0)?,
            "new-window-offset" => self.new_window_offset = range(num()?, 0.0, 500.0)? as i32,
            other => bail!("unknown option `{other}`"),
        }
        Ok(())
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

fn range(v: f64, lo: f64, hi: f64) -> Result<f64> {
    if v.is_finite() && v >= lo && v <= hi {
        Ok(v)
    } else {
        bail!("{v} is out of range ({lo} to {hi})")
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
        "drag-move" => Action::DragMove,
        "drag-resize" => Action::DragResize,
        "drag-pan" => Action::DragPan,
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
        assert_eq!(c.border_width, 3);
        assert!(!c.sloppy_focus);
        assert!((c.focus_color[0] - 1.0).abs() < 1e-6 && (c.focus_color[1] - 0.533).abs() < 1e-3);
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
    fn mod_key_option_changes_mod() {
        let c = Config::parse("options { mod-key \"Alt\" }\nbinds { Mod+T { quit; } }", Some(Path::new("/x/c.kdl")))
            .unwrap();
        assert!(c.keys[0].mods.alt && !c.keys[0].mods.logo);
    }
}
