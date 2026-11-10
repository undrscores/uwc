//! Window rules and the decoration settings they can change.
//!
//! ```kdl
//! rule {
//!     match app-id="org.gnome.*"           // several `match` lines are alternatives
//!     match title="*YouTube*" focused=true // the properties of one match must all hold
//!     exclude app-id="org.gnome.Nautilus"  // a matching `exclude` takes the window back out
//!     open { tag 3; size 900 600; position 3000 0; pinned; fullscreen; minimized; focus false }
//!     opacity 0.9                          // any decoration option, for these windows
//!     focus-gradient "#00aaff" "#aa00ff" angle=45
//! }
//! ```
//!
//! Differences from uwm, on purpose: app-id / title are exact strings or globs (`*`, `?`), not
//! unanchored regexes; what happens once when a window opens is in an explicit `open { }` block,
//! while decoration settings apply live and follow the window's state (`focused`, `tag`, `pinned`,
//! `fullscreen`, `urgent` matchers are re-checked whenever drawing); `exclude` exists.

use std::collections::HashMap;

use anyhow::{anyhow, bail, Context, Result};

use crate::{
    canvas::TAGCOUNT,
    config::{parse_color, range, Color, Gradient},
    kdl::{Node, Val},
};

/// Everything about how a window is decorated: the global options, and what rules override per window.
#[derive(Debug, Clone, PartialEq)]
pub struct Deco {
    pub border_width: i32,
    pub border_color: Color,
    pub focus_color: Color,
    pub urgent_color: Color,
    pub corner_radius: f64,
    pub shadow_size: f64,
    pub shadow_offset_x: f64,
    pub shadow_offset_y: f64,
    pub shadow_color: Color,
    pub shadow_spread: f64,
    /// Alpha 0 = the same as shadow_color.
    pub focus_shadow_color: Color,
    /// None = the same as shadow_size (-1 in the config and over IPC).
    pub focus_shadow_size: Option<f64>,
    pub backdrop_color: Color,
    pub opacity: f64,
    pub inactive_opacity: f64,
    pub inactive_dim: f64,
    pub border_gradient: Option<Gradient>,
    pub focus_gradient: Option<Gradient>,
    pub urgent_gradient: Option<Gradient>,
    pub border_spin_ms: f64,
    /// A GLSL file drawing the border ("" = the built-in one).
    pub border_shader: String,
}

impl Default for Deco {
    fn default() -> Self {
        Deco {
            border_width: 0,
            border_color: [0.267, 0.267, 0.267, 1.0],
            focus_color: [0.0, 0.333, 0.467, 1.0],
            urgent_color: [1.0, 0.0, 0.0, 1.0],
            corner_radius: 10.0,
            shadow_size: 24.0,
            shadow_offset_x: 0.0,
            shadow_offset_y: 6.0,
            shadow_color: [0.0, 0.0, 0.0, 0.6],
            shadow_spread: 0.0,
            focus_shadow_color: [0.0; 4],
            focus_shadow_size: None,
            backdrop_color: [0.0; 4],
            opacity: 1.0,
            inactive_opacity: 1.0,
            inactive_dim: 0.0,
            border_gradient: None,
            focus_gradient: None,
            urgent_gradient: None,
            border_spin_ms: 0.0,
            border_shader: String::new(),
        }
    }
}

impl Deco {
    /// Apply one decoration setting. `Ok(false)`: `n` is not a decoration setting at all.
    pub fn set(&mut self, n: &Node, vars: &HashMap<String, String>) -> Result<bool> {
        crate::options::set(crate::options::DECO, self, n, vars)
    }
}

pub fn parse_gradient(n: &Node, vars: &HashMap<String, String>) -> Result<Option<Gradient>> {
    let colors = n
        .args
        .iter()
        .map(|v| match v {
            Val::Str(s) => parse_color(s, vars),
            _ => Err(anyhow!("gradient colors are strings like \"#rrggbb\"")),
        })
        .collect::<Result<Vec<_>>>()?;
    // No colors turns the gradient off.
    if colors.is_empty() {
        return Ok(None);
    }
    if !(2..=6).contains(&colors.len()) {
        bail!("a gradient has 2 to 6 colors");
    }
    let mut angle = 90.0;
    for (k, v) in &n.props {
        match k.as_str() {
            "angle" => angle = range(v.as_num().ok_or_else(|| anyhow!("angle needs a number"))?, -3600.0, 3600.0)?,
            other => bail!("unknown gradient property `{other}` (angle)"),
        }
    }
    Ok(Some(Gradient { colors, angle }))
}

/// `*` matches any run of characters, `?` any one; everything else is literal. No wildcards = exact.
pub fn glob(pattern: &str, s: &str) -> bool {
    let (p, s): (Vec<char>, Vec<char>) = (pattern.chars().collect(), s.chars().collect());
    let (mut pi, mut si) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// What a rule can look at.
pub struct Facts<'a> {
    pub app_id: &'a str,
    pub title: &'a str,
    pub tags: u32,
    pub focused: bool,
    pub pinned: bool,
    pub fullscreen: bool,
    pub urgent: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Matcher {
    app_id: Option<String>,
    title: Option<String>,
    tag: Option<usize>,
    focused: Option<bool>,
    pinned: Option<bool>,
    fullscreen: Option<bool>,
    urgent: Option<bool>,
}

impl Matcher {
    fn parse(n: &Node) -> Result<Matcher> {
        if !n.args.is_empty() {
            bail!("`{}` takes properties like app-id=\"foot\", not arguments", n.name);
        }
        let mut m = Matcher::default();
        for (k, v) in &n.props {
            let s = || v.as_str().map(str::to_string).ok_or_else(|| anyhow!("{k} needs a string"));
            let b = || v.as_bool().ok_or_else(|| anyhow!("{k} needs true or false"));
            match k.as_str() {
                "app-id" => m.app_id = Some(s()?),
                "title" => m.title = Some(s()?),
                "tag" => {
                    let t = v.as_num().ok_or_else(|| anyhow!("tag needs a number"))?;
                    if t.fract() != 0.0 || !(1.0..=TAGCOUNT as f64).contains(&t) {
                        bail!("tag must be 1 to {TAGCOUNT}");
                    }
                    m.tag = Some(t as usize - 1);
                }
                "focused" => m.focused = Some(b()?),
                "pinned" => m.pinned = Some(b()?),
                "fullscreen" => m.fullscreen = Some(b()?),
                "urgent" => m.urgent = Some(b()?),
                other => {
                    bail!("unknown match property `{other}` (app-id, title, tag, focused, pinned, fullscreen, urgent)")
                }
            }
        }
        Ok(m)
    }

    fn fits(&self, f: &Facts) -> bool {
        self.app_id.as_ref().is_none_or(|p| glob(p, f.app_id))
            && self.title.as_ref().is_none_or(|p| glob(p, f.title))
            && self.tag.is_none_or(|t| f.tags & (1 << t) != 0)
            && self.focused.is_none_or(|v| v == f.focused)
            && self.pinned.is_none_or(|v| v == f.pinned)
            && self.fullscreen.is_none_or(|v| v == f.fullscreen)
            && self.urgent.is_none_or(|v| v == f.urgent)
    }
}

/// What happens once, when a matching window first appears.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Open {
    pub tags: Option<u32>,
    pub size: Option<(i32, i32)>,
    /// Canvas px; screen px for a window that also opens pinned.
    pub position: Option<(f64, f64)>,
    pub pinned: bool,
    pub fullscreen: bool,
    pub minimized: bool,
    /// `focus false`: open without taking the keyboard.
    pub focus: Option<bool>,
    /// The window's own scale on the canvas.
    pub scale: Option<f64>,
    /// The monitor a pinned spot is on, and the one a fullscreen window covers (session restore).
    pub pin_mon: usize,
    pub fs_mon: usize,
}

impl Open {
    /// Later settings override earlier ones.
    fn merge(&mut self, o: &Open) {
        self.tags = o.tags.or(self.tags);
        self.size = o.size.or(self.size);
        self.position = o.position.or(self.position);
        self.pinned |= o.pinned;
        self.fullscreen |= o.fullscreen;
        self.minimized |= o.minimized;
        self.focus = o.focus.or(self.focus);
        self.scale = o.scale.or(self.scale);
        self.pin_mon = o.pin_mon;
        self.fs_mon = o.fs_mon;
    }

    fn parse(n: &Node) -> Result<Open> {
        let mut o = Open::default();
        for s in &n.kids {
            let nums = || -> Result<Vec<f64>> {
                s.args.iter().map(|v| v.as_num().ok_or_else(|| anyhow!("`{}` needs numbers", s.name))).collect()
            };
            let flag = || -> Result<bool> {
                match s.args.first() {
                    None => Ok(true),
                    Some(Val::Bool(b)) => Ok(*b),
                    _ => bail!("`{}` takes nothing, true or false", s.name),
                }
            };
            match s.name.as_str() {
                "tag" => {
                    let mut mask = 0;
                    for t in nums()? {
                        if t.fract() != 0.0 || !(1.0..=TAGCOUNT as f64).contains(&t) {
                            bail!("tag must be 1 to {TAGCOUNT}");
                        }
                        mask |= 1 << (t as u32 - 1);
                    }
                    if mask == 0 {
                        bail!("tag needs at least one number");
                    }
                    o.tags = Some(mask);
                }
                "size" => {
                    let [w, h] = nums()?[..] else { bail!("size needs a width and a height") };
                    o.size = Some((range(w, 1.0, 1e5)? as i32, range(h, 1.0, 1e5)? as i32));
                }
                "position" => {
                    let [x, y] = nums()?[..] else { bail!("position needs x and y") };
                    o.position = Some((range(x, -1e9, 1e9)?, range(y, -1e9, 1e9)?));
                }
                "pinned" => o.pinned = flag()?,
                "fullscreen" => o.fullscreen = flag()?,
                "minimized" => o.minimized = flag()?,
                "focus" => o.focus = Some(flag()?),
                "scale" => {
                    let [s] = nums()?[..] else { bail!("scale needs one number") };
                    o.scale = Some(range(s, 0.1, 4.0)?);
                }
                other => {
                    bail!("unknown open setting `{other}` (tag, size, position, pinned, fullscreen, minimized, focus, scale)")
                }
            }
        }
        Ok(o)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    matches: Vec<Matcher>,
    excludes: Vec<Matcher>,
    pub open: Option<Open>,
    /// Decoration settings, checked when parsed, applied over the options for matching windows.
    deco: Vec<Node>,
}

impl Rule {
    pub fn parse(n: &Node, vars: &HashMap<String, String>) -> Result<Rule> {
        let mut r = Rule { matches: vec![], excludes: vec![], open: None, deco: vec![] };
        let mut probe = Deco::default();
        for s in &n.kids {
            let at = || format!("`{}` (line {})", s.name, s.line);
            match s.name.as_str() {
                "match" => r.matches.push(Matcher::parse(s).with_context(at)?),
                "exclude" => r.excludes.push(Matcher::parse(s).with_context(at)?),
                "open" => r.open = Some(Open::parse(s).with_context(at)?),
                _ => {
                    if !probe.set(s, vars).with_context(at)? {
                        bail!(
                            "{}: not a decoration setting (rules take match, exclude, open {{ }} and decoration options)",
                            at()
                        );
                    }
                    r.deco.push(s.clone());
                }
            }
        }
        Ok(r)
    }

    /// No `match` line matches every window.
    pub fn fits(&self, f: &Facts) -> bool {
        (self.matches.is_empty() || self.matches.iter().any(|m| m.fits(f))) && !self.excludes.iter().any(|m| m.fits(f))
    }
}

/// The decoration for a window: the options, then every fitting rule's settings in order.
pub fn resolve_deco(base: &Deco, rules: &[&Rule], f: &Facts, vars: &HashMap<String, String>) -> Deco {
    let mut d = base.clone();
    for r in rules.iter().filter(|r| !r.deco.is_empty() && r.fits(f)) {
        for n in &r.deco {
            // Checked at parse time; a failure here would only mean a var changed meaning.
            let _ = d.set(n, vars);
        }
    }
    d
}

/// What fitting rules say about a window that is opening.
pub fn resolve_open(rules: &[&Rule], f: &Facts) -> Open {
    let mut o = Open::default();
    for r in rules.iter().filter(|r| r.fits(f)) {
        if let Some(ro) = &r.open {
            o.merge(ro);
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kdl;

    fn facts<'a>(app_id: &'a str, title: &'a str) -> Facts<'a> {
        Facts { app_id, title, tags: 1, focused: false, pinned: false, fullscreen: false, urgent: false }
    }

    fn rule(src: &str) -> Result<Rule> {
        let n = kdl::parse(src).unwrap();
        Rule::parse(&n[0], &HashMap::new())
    }

    #[test]
    fn globs() {
        assert!(glob("foot", "foot"));
        assert!(!glob("foot", "footclient"), "no wildcard means exact");
        assert!(glob("org.gnome.*", "org.gnome.Nautilus"));
        assert!(glob("*YouTube*", "Video - YouTube - Firefox"));
        assert!(glob("f??t", "foot"));
        assert!(!glob("f??t", "fot"));
        assert!(glob("*", ""));
        assert!(glob("a*b*c", "aXXbYYc"));
        assert!(!glob("a*b*c", "aXXbYY"));
    }

    #[test]
    fn matches_are_alternatives_and_excludes_win() {
        let r = rule("rule { match app-id=\"org.gnome.*\"; match title=\"*YouTube*\"; exclude app-id=\"org.gnome.Nautilus\"; opacity 0.9 }").unwrap();
        assert!(r.fits(&facts("org.gnome.Calculator", "")));
        assert!(r.fits(&facts("firefox", "x - YouTube")));
        assert!(!r.fits(&facts("org.gnome.Nautilus", "")));
        assert!(!r.fits(&facts("foot", "")));
        // No match line: every window.
        assert!(rule("rule { opacity 0.5 }").unwrap().fits(&facts("anything", "")));
    }

    #[test]
    fn state_matchers_and_deco_resolution() {
        let r = rule("rule { match app-id=\"foot\" focused=false; inactive-dim 0.4; border-width 7 }").unwrap();
        let base = Deco::default();
        let mut f = facts("foot", "");
        let d = resolve_deco(&base, &[&r], &f, &HashMap::new());
        assert_eq!((d.inactive_dim, d.border_width), (0.4, 7));
        f.focused = true;
        assert_eq!(resolve_deco(&base, &[&r], &f, &HashMap::new()), base, "a state matcher follows the state");
    }

    #[test]
    fn later_rules_override_and_open_merges() {
        let a = rule("rule { open { tag 2 3; size 800 600 } ; opacity 0.5 }").unwrap();
        let b = rule("rule { match app-id=\"mpv\"; open { pinned; focus false }; opacity 0.8 }").unwrap();
        let f = facts("mpv", "");
        assert_eq!(resolve_deco(&Deco::default(), &[&a, &b], &f, &HashMap::new()).opacity, 0.8);
        let o = resolve_open(&[&a, &b], &f);
        assert_eq!((o.tags, o.size, o.pinned, o.focus), (Some(0b110), Some((800, 600)), true, Some(false)));
    }

    #[test]
    fn bad_rules_are_rejected() {
        for bad in [
            "rule { match app-id=foot extra=1 }",
            "rule { match \"foot\" }",
            "rule { match tag=10 }",
            "rule { open { tag 0 } }",
            "rule { open { size 10 } }",
            "rule { open { teleport } }",
            "rule { opacity 3 }",
            "rule { sloppy-focus false }",
            "rule { frobnicate 1 }",
        ] {
            assert!(rule(bad).is_err(), "should reject: {bad}");
        }
    }
}
