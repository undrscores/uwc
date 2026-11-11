//! The control socket's protocol, typed. Requests are text lines (easy from a shell, and `set` /
//! `rule` carry KDL), parsed into `Request`; everything sent back is one of the serde types below,
//! one JSON object per line. `uwc msg` and the compositor share this module.

use serde::Serialize;
use serde_json::{Map, Value};

use crate::canvas::TAGCOUNT;

/// `frame ...`: labelled regions of the canvas (frames.rs).
#[derive(Debug, Clone, PartialEq)]
pub enum FrameCmd {
    Add {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        label: String,
    },
    Label(u32, String),
    /// Moves the windows inside with it.
    Move(u32, f64, f64),
    Resize(u32, f64, f64),
    Remove(u32),
    Fit(u32),
}

/// `tile ...`: the tiling of the tag in use (tiling.rs).
#[derive(Debug, Clone, PartialEq)]
pub enum TileCmd {
    /// Tile with this layout and keep doing so.
    Set(crate::layout::Layout),
    /// Arrange once.
    Once(crate::layout::Layout),
    Off,
    Cycle,
    Ratio(f64),
    Masters(usize),
}

/// A command, as understood by the compositor.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    State,
    Subscribe,
    Get(Option<String>),
    /// One line of the options block, e.g. `focus-color "#ff8800"`.
    Set(String),
    /// The whole line, which is KDL: `rule { ... }` or `rule clear`.
    Rule(String),
    Rules,
    GetWindow(u32),
    Pan(f64, f64),
    PanTo(f64, f64),
    Frame(FrameCmd),
    Tile(TileCmd),
    /// Keep a window out of the tiling layouts / let it be tiled.
    Float(u32, bool),
    /// Switch the keyboard layout: `next` or an index.
    Layout(Option<usize>),
    /// Panic inside the compositor, to test that it survives (only with UWC_DEBUG_PANIC=1).
    Crash,
    /// The window whose app-id and title best match this text (`#12` = window 12), brought into view.
    Jump(String),
    /// Start a glide at this velocity in screen px/s (what letting go of a touchpad swipe does).
    Fling(f64, f64),
    Zoom(f64, Option<(f64, f64)>),
    View(usize),
    ToggleView(usize),
    Focus(u32),
    Close(u32),
    Move(u32, f64, f64),
    Resize(u32, i32, i32),
    Pin(u32, bool),
    Minimize(u32, bool),
    Fullscreen(u32, bool),
    WindowScale(u32, f64),
    Bookmark {
        save: bool,
        n: usize,
    },
    /// `session save` (write it now) / `session forget` (drop what is remembered).
    Session {
        save: bool,
    },
    Overview,
    Home,
    Reload,
    Quit,
}

/// Every command with its arguments, for `uwc msg --help` and error messages.
pub const COMMANDS: &[(&str, &str)] = &[
    ("state", "one snapshot of monitors, tags, windows, layers, focus"),
    ("subscribe", "a snapshot now and one after every change"),
    ("get [OPTION]", "current option values"),
    ("set OPTION VALUE", "change an option until the next reload, e.g. set focus-color \"#ff8800\""),
    ("rule { ... } | rule clear", "add a window rule until the next reload / remove those"),
    ("rules", "how many rules are in effect"),
    ("get-window ID", "a window's decoration with its rules applied"),
    ("pan DX DY", "move the view by canvas px"),
    (
        "frame add|label|move|resize|remove|fit ...",
        "labelled regions of the canvas: add X Y W H [LABEL], label ID TEXT, move ID X Y (its windows go too), resize ID W H, remove ID, fit ID",
    ),
    (
        "tile LAYOUT|off|cycle|once LAYOUT|ratio R|masters N",
        "tile the tag in use: LAYOUT is master-stack, dwindle, spiral or columns; once arranges one time; ratio (0.1-0.9) and masters (1-16) change the layout",
    ),
    ("float ID | unfloat ID", "keep a window out of the tiling layouts / let it be tiled"),
    ("layout next|N", "switch the keyboard layout (0 is the first of input { keyboard { xkb-layout \"us,de\" } })"),
    ("crash", "panic on purpose to test recovery (only when UWC_DEBUG_PANIC=1 is set in uwc's environment)"),
    (
        "jump QUERY",
        "focus and reveal the window whose app-id and title best match (fuzzy), on any tag; #ID for a window id",
    ),
    ("fling VX VY", "glide the view like a released swipe: screen px/s, slowing down over pan-momentum-ms"),
    ("panto X Y", "put canvas point X,Y at the top-left of the screen"),
    ("zoom Z [X Y]", "set the zoom, keeping screen point X,Y (default: the centre) fixed"),
    ("view TAG | toggleview TAG", "show only TAG / show or hide TAG as well"),
    ("focus ID | close ID", "focus (and reveal) / ask to close a window"),
    ("move ID X Y | resize ID W H", "canvas px"),
    ("pin ID | unpin ID", "keep a window on screen while the view moves"),
    ("minimize ID | restore ID", "hide a window / bring it back where you are"),
    ("fullscreen ID | unfullscreen ID", ""),
    ("window-scale ID S", "a window's own scale on the canvas (0.1-4), around its centre"),
    ("bookmark save N | bookmark goto N", "remember / return to a view of the tag in use (1-9)"),
    ("session save | session forget", "write the remembered canvas now / forget it"),
    ("overview | home", "fit all windows / back to the origin at zoom 1"),
    ("reload | quit", ""),
];

impl Request {
    pub fn parse(line: &str) -> Result<Request, String> {
        let line = line.trim();
        let mut it = line.split_whitespace();
        let cmd = it.next().ok_or("empty command")?;
        let args: Vec<&str> = it.collect();
        let rest = || line[cmd.len()..].trim().to_string();
        let num = |i: usize| -> Result<f64, String> {
            let a = args.get(i).ok_or_else(|| format!("`{cmd}` needs more arguments"))?;
            let v: f64 = a.parse().map_err(|_| format!("`{a}` is not a number"))?;
            // Far beyond any canvas, and small enough that arithmetic on it cannot overflow to infinity.
            if v.is_finite() && v.abs() <= 1e9 {
                Ok(v)
            } else {
                Err(format!("`{a}` is not a number between -1000000000 and 1000000000"))
            }
        };
        let id = |i: usize| -> Result<u32, String> {
            let v = num(i)?;
            if v.fract() == 0.0 && v >= 0.0 && v <= u32::MAX as f64 {
                Ok(v as u32)
            } else {
                Err("a window id is a whole number".into())
            }
        };
        let tag = |i: usize| -> Result<usize, String> {
            let t = num(i)?;
            if t.fract() == 0.0 && (1.0..=TAGCOUNT as f64).contains(&t) {
                Ok(t as usize - 1)
            } else {
                Err(format!("tag must be 1 to {TAGCOUNT}"))
            }
        };
        let none = |r: Request| if args.is_empty() { Ok(r) } else { Err(format!("`{cmd}` takes no arguments")) };
        Ok(match cmd {
            "state" => none(Request::State)?,
            "subscribe" => none(Request::Subscribe)?,
            "get" => Request::Get(args.first().map(|s| s.to_string())),
            "set" => {
                if args.len() < 2 {
                    return Err("set needs an option and a value, e.g. set focus-color \"#ff8800\"".into());
                }
                Request::Set(rest())
            }
            "rule" => Request::Rule(line.to_string()),
            "rules" => none(Request::Rules)?,
            "get-window" => Request::GetWindow(id(0)?),
            "pan" => Request::Pan(num(0)?, num(1)?),
            "panto" => Request::PanTo(num(0)?, num(1)?),
            "fling" => Request::Fling(num(0)?, num(1)?),
            "crash" => none(Request::Crash)?,
            "layout" => match args.first().copied() {
                Some("next") => Request::Layout(None),
                Some(n) => {
                    Request::Layout(Some(n.parse().map_err(|_| "layout takes `next` or a layout number".to_string())?))
                }
                None => return Err("layout takes `next` or a layout number (0 is the first)".into()),
            },
            "frame" => {
                let sub = args.first().copied().unwrap_or("");
                let text = |from: usize| args.get(from..).unwrap_or(&[]).join(" ");
                Request::Frame(match sub {
                    "add" => FrameCmd::Add { x: num(1)?, y: num(2)?, w: num(3)?, h: num(4)?, label: text(5) },
                    "label" => FrameCmd::Label(id(1)?, text(2)),
                    "move" => FrameCmd::Move(id(1)?, num(2)?, num(3)?),
                    "resize" => FrameCmd::Resize(id(1)?, num(2)?, num(3)?),
                    "remove" => FrameCmd::Remove(id(1)?),
                    "fit" => FrameCmd::Fit(id(1)?),
                    _ => return Err("frame needs add X Y W H [LABEL], label ID TEXT, move ID X Y, resize ID W H, remove ID or fit ID".into()),
                })
            }
            "tile" => {
                use crate::layout::Layout;
                let layout = |s: Option<&&str>| {
                    s.and_then(|s| Layout::parse(s)).ok_or_else(|| format!("a layout is one of: {}", Layout::names()))
                };
                Request::Tile(match args.first().copied() {
                    Some("off") if args.len() == 1 => TileCmd::Off,
                    Some("cycle") if args.len() == 1 => TileCmd::Cycle,
                    Some("once") => TileCmd::Once(layout(args.get(1))?),
                    Some("ratio") => {
                        let r = num(1)?;
                        if !(0.1..=0.9).contains(&r) {
                            return Err("a tile ratio is 0.1 to 0.9".into());
                        }
                        TileCmd::Ratio(r)
                    }
                    Some("masters") => {
                        let n = num(1)?;
                        if n.fract() != 0.0 || !(1.0..=16.0).contains(&n) {
                            return Err("masters is 1 to 16".into());
                        }
                        TileCmd::Masters(n as usize)
                    }
                    Some(_) if args.len() == 1 => TileCmd::Set(layout(args.first())?),
                    _ => return Err("tile LAYOUT, off, cycle, once LAYOUT, ratio R or masters N".into()),
                })
            }
            "float" | "unfloat" => Request::Float(id(0)?, cmd == "float"),
            "jump" => {
                if args.is_empty() {
                    return Err("jump needs some text to match, e.g. jump firefox (or #12 for a window id)".into());
                }
                Request::Jump(rest())
            }
            "zoom" => {
                let z = num(0)?;
                if z <= 0.0 {
                    return Err("zoom must be positive".into());
                }
                let at = if args.len() >= 3 { Some((num(1)?, num(2)?)) } else { None };
                Request::Zoom(z, at)
            }
            "view" => Request::View(tag(0)?),
            "toggleview" => Request::ToggleView(tag(0)?),
            "focus" => Request::Focus(id(0)?),
            "close" => Request::Close(id(0)?),
            "move" => Request::Move(id(0)?, num(1)?, num(2)?),
            "resize" => {
                let (w, h) = (num(1)?, num(2)?);
                if !(1.0..=1e5).contains(&w) || !(1.0..=1e5).contains(&h) {
                    return Err("size out of range".into());
                }
                Request::Resize(id(0)?, w as i32, h as i32)
            }
            "pin" | "unpin" => Request::Pin(id(0)?, cmd == "pin"),
            "minimize" | "restore" => Request::Minimize(id(0)?, cmd == "minimize"),
            "fullscreen" | "unfullscreen" => Request::Fullscreen(id(0)?, cmd == "fullscreen"),
            "window-scale" => {
                let s = num(1)?;
                if !(0.1..=4.0).contains(&s) {
                    return Err("a window scale is 0.1 to 4".into());
                }
                Request::WindowScale(id(0)?, s)
            }
            "bookmark" => {
                let save = match args.first().copied() {
                    Some("save") => true,
                    Some("goto") => false,
                    _ => return Err("bookmark save|goto N".into()),
                };
                let n = num(1)?;
                if n.fract() != 0.0 || !(1.0..=9.0).contains(&n) {
                    return Err("bookmarks are 1 to 9".into());
                }
                Request::Bookmark { save, n: n as usize - 1 }
            }
            "session" => match (args.first().copied(), args.len()) {
                (Some("save"), 1) => Request::Session { save: true },
                (Some("forget"), 1) => Request::Session { save: false },
                _ => return Err("session save|forget".into()),
            },
            "overview" => none(Request::Overview)?,
            "home" => none(Request::Home)?,
            "reload" => none(Request::Reload)?,
            "quit" => none(Request::Quit)?,
            other => return Err(format!("unknown command `{other}` (`uwc msg --help` lists them)")),
        })
    }
}

// ---- replies --------------------------------------------------------------------------------

#[derive(Serialize)]
pub struct Ok {
    pub ok: bool,
}

#[derive(Serialize)]
pub struct Error {
    pub error: String,
}

#[derive(Serialize, Clone, Copy)]
pub struct Rect {
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
}

#[derive(Serialize, Clone, Copy)]
pub struct View {
    pub x: i64,
    pub y: i64,
    pub zoom: f64,
}

#[derive(Serialize)]
pub struct Bookmark {
    pub n: usize,
    #[serde(flatten)]
    pub view: View,
}

#[derive(Serialize)]
pub struct Tag {
    pub n: usize,
    pub active: bool,
    pub occupied: bool,
    pub urgent: bool,
    pub viewport: View,
    pub bookmarks: Vec<Bookmark>,
    /// The layout windows on this tag are tiled with, if any.
    pub tiling: Option<Tiling>,
}

#[derive(Serialize)]
pub struct Tiling {
    pub layout: &'static str,
    pub ratio: f64,
    pub masters: usize,
    /// The canvas rect the windows are laid out in.
    #[serde(flatten)]
    pub area: Rect,
}

#[derive(Serialize)]
pub struct Layer {
    pub namespace: String,
    pub layer: &'static str,
    #[serde(flatten)]
    pub rect: Rect,
    pub keyboard: bool,
}

#[derive(Serialize)]
pub struct Monitor {
    pub name: String,
    pub focused: bool,
    pub layout: &'static str,
    #[serde(flatten)]
    pub rect: Rect,
    pub scale: f64,
    pub viewport: View,
    pub work_area: Rect,
    /// An eased view move is still under way (`viewport` already shows where it ends).
    pub view_moving: bool,
    /// Anything is still easing: the view, or a window opening, gliding or fading in.
    pub animating: bool,
    pub layers: Vec<Layer>,
    pub tags: Vec<Tag>,
}

#[derive(Serialize)]
pub struct KeyboardLayout {
    pub index: usize,
    pub name: String,
    pub count: usize,
}

#[derive(Serialize)]
pub struct Frame {
    pub id: u32,
    pub label: String,
    pub tags: u32,
    /// Canvas px.
    #[serde(flatten)]
    pub rect: Rect,
    /// The windows whose centre is inside it (what moves with it).
    pub windows: Vec<u32>,
}

#[derive(Serialize)]
pub struct Window {
    pub id: u32,
    pub title: String,
    pub app_id: String,
    pub monitor: String,
    pub tags: u32,
    pub focused: bool,
    pub floating: bool,
    pub fullscreen: bool,
    pub urgent: bool,
    /// Canvas px (where a pinned window's screen spot is over the canvas now).
    #[serde(flatten)]
    pub rect: Rect,
    pub pinned: bool,
    pub minimized: bool,
    /// The window's own scale on the canvas (1 = normal).
    pub scale: f64,
}

#[derive(Serialize)]
pub struct Cursor {
    pub x: i64,
    pub y: i64,
    pub hidden: bool,
}

/// Where the seat's keyboard is.
#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyboardFocus {
    Window(u32),
    Layer(String),
    Other(bool),
}

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum Event {
    State {
        selmon: String,
        config_error: Option<String>,
        config_rev: u32,
        /// User shader files that did not compile (or load), by the path in the config.
        shader_errors: std::collections::BTreeMap<String, String>,
        backend: &'static str,
        keyboard_focus: Option<KeyboardFocus>,
        keyboard_layout: KeyboardLayout,
        /// Hint mode (`jump-hints`) is up: every key goes to it.
        hints: bool,
        monitors: Vec<Monitor>,
        cursor: Cursor,
        windows: Vec<Window>,
        frames: Vec<Frame>,
    },
    Options {
        options: Map<String, Value>,
    },
    Option {
        name: String,
        value: Value,
    },
    WindowOptions {
        id: u32,
        options: Map<String, Value>,
    },
    Rules {
        file: usize,
        ipc: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        assert_eq!(Request::parse("state"), Ok(Request::State));
        assert_eq!(Request::parse("  pan 10 -2.5 "), Ok(Request::Pan(10.0, -2.5)));
        assert_eq!(Request::parse("zoom 2"), Ok(Request::Zoom(2.0, None)));
        assert_eq!(Request::parse("zoom 2 10 20"), Ok(Request::Zoom(2.0, Some((10.0, 20.0)))));
        assert_eq!(Request::parse("view 3"), Ok(Request::View(2)));
        assert_eq!(Request::parse("unpin 4"), Ok(Request::Pin(4, false)));
        assert_eq!(Request::parse("bookmark goto 9"), Ok(Request::Bookmark { save: false, n: 8 }));
        assert_eq!(Request::parse("set focus-color \"#ff8800\""), Ok(Request::Set("focus-color \"#ff8800\"".into())));
        assert_eq!(Request::parse("rule clear"), Ok(Request::Rule("rule clear".into())));
    }

    #[test]
    fn rejects_bad_commands() {
        for bad in [
            "",
            "bogus",
            "pan x y",
            "pan 1",
            "zoom 0",
            "zoom nan",
            "view 0",
            "view 10",
            "focus -1",
            "focus 1.5",
            "resize 1 0 10",
            "bookmark jump 1",
            "bookmark save 10",
            "state now",
            "set",
            "set opacity",
        ] {
            assert!(Request::parse(bad).is_err(), "should reject: {bad:?}");
        }
    }

    #[test]
    fn every_command_is_listed_in_the_help() {
        let help: String = COMMANDS.iter().map(|(c, _)| *c).collect::<Vec<_>>().join(" ");
        for cmd in [
            "state",
            "subscribe",
            "get",
            "set",
            "rule",
            "rules",
            "get-window",
            "pan",
            "panto",
            "fling",
            "jump",
            "crash",
            "layout",
            "frame",
            "tile",
            "float",
            "unfloat",
            "zoom",
            "view",
            "toggleview",
            "focus",
            "close",
            "move",
            "resize",
            "pin",
            "unpin",
            "minimize",
            "restore",
            "fullscreen",
            "unfullscreen",
            "window-scale",
            "bookmark",
            "session",
            "overview",
            "home",
            "reload",
            "quit",
        ] {
            assert!(help.split(|c: char| !c.is_alphanumeric() && c != '-').any(|w| w == cmd), "{cmd} not in help");
        }
    }

    #[test]
    fn events_have_the_wire_shape() {
        let e = serde_json::to_value(Event::Rules { file: 1, ipc: 2 }).unwrap();
        assert_eq!(e, serde_json::json!({"event": "rules", "file": 1, "ipc": 2}));
        let e = serde_json::to_value(Event::WindowOptions { id: 3, options: Map::new() }).unwrap();
        assert_eq!(e["event"], "window-options");
        let k = serde_json::to_value(KeyboardFocus::Window(5)).unwrap();
        assert_eq!(k, serde_json::json!({"window": 5}));
    }
}
