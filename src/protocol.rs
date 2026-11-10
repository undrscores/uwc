//! The control socket's protocol, typed. Requests are text lines (easy from a shell, and `set` /
//! `rule` carry KDL), parsed into `Request`; everything sent back is one of the serde types below,
//! one JSON object per line. `uwc msg` and the compositor share this module.

use serde::Serialize;
use serde_json::{Map, Value};

use crate::canvas::TAGCOUNT;

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
            if v.is_finite() {
                Ok(v)
            } else {
                Err(format!("`{a}` is not a finite number"))
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
    pub layers: Vec<Layer>,
    pub tags: Vec<Tag>,
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
        monitors: Vec<Monitor>,
        cursor: Cursor,
        windows: Vec<Window>,
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
