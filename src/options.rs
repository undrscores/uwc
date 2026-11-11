//! Every option, once. A table line names the option, the field it sets and (for numbers) its range;
//! parsing, range checks, `set` and `get` all come from that line, so an option cannot be half-wired.
//! `config.def.kdl` stays the hand-written documentation; a test checks every option is in it.
//!
//! Two tables: `DECO` (window decoration, also settable by window rules) and `GLOBAL` (the rest).

use std::collections::HashMap;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Map, Value};

use crate::{
    config::{parse_color, range, Color, Config, Gradient, ModKey},
    kdl::{Node, Val},
    rules::{parse_gradient, Deco},
};

pub type Vars = HashMap<String, String>;

/// A type an option can have: how to read it from a config line and report it as JSON.
pub trait OptValue: Sized {
    fn parse(n: &Node, vars: &Vars, range: Option<(f64, f64)>) -> Result<Self>;
    fn json(&self) -> Value;
}

fn first(n: &Node) -> Result<&Val> {
    n.args.first().ok_or_else(|| anyhow!("missing value"))
}

fn number(n: &Node, r: Option<(f64, f64)>) -> Result<f64> {
    let v = first(n)?.as_num().ok_or_else(|| anyhow!("expected a number"))?;
    match r {
        Some((lo, hi)) => range(v, lo, hi),
        None if v.is_finite() => Ok(v),
        None => bail!("expected a finite number"),
    }
}

impl OptValue for bool {
    fn parse(n: &Node, _: &Vars, _: Option<(f64, f64)>) -> Result<Self> {
        first(n)?.as_bool().ok_or_else(|| anyhow!("expected true or false"))
    }
    fn json(&self) -> Value {
        json!(self)
    }
}

impl OptValue for f64 {
    fn parse(n: &Node, _: &Vars, r: Option<(f64, f64)>) -> Result<Self> {
        number(n, r)
    }
    fn json(&self) -> Value {
        json!(self)
    }
}

impl OptValue for i32 {
    fn parse(n: &Node, _: &Vars, r: Option<(f64, f64)>) -> Result<Self> {
        Ok(number(n, r)?.round() as i32)
    }
    fn json(&self) -> Value {
        json!(self)
    }
}

impl OptValue for u32 {
    fn parse(n: &Node, _: &Vars, r: Option<(f64, f64)>) -> Result<Self> {
        Ok(number(n, r)?.round().max(0.0) as u32)
    }
    fn json(&self) -> Value {
        json!(self)
    }
}

/// An optional size: negative means "not set" (e.g. focus-shadow-size -1 = the same as shadow-size).
impl OptValue for Option<f64> {
    fn parse(n: &Node, _: &Vars, r: Option<(f64, f64)>) -> Result<Self> {
        let v = number(n, None)?;
        if v < 0.0 {
            return Ok(None);
        }
        Ok(Some(match r {
            Some((lo, hi)) => range(v, lo.max(0.0), hi)?,
            None => v,
        }))
    }
    fn json(&self) -> Value {
        json!(self.unwrap_or(-1.0))
    }
}

impl OptValue for Color {
    fn parse(n: &Node, vars: &Vars, _: Option<(f64, f64)>) -> Result<Self> {
        match first(n)? {
            Val::Str(s) => parse_color(s, vars),
            _ => bail!("expected a color string like \"#rrggbb\""),
        }
    }
    fn json(&self) -> Value {
        let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        json!(format!("#{:02x}{:02x}{:02x}{:02x}", b(self[0]), b(self[1]), b(self[2]), b(self[3])))
    }
}

impl OptValue for Option<Gradient> {
    fn parse(n: &Node, vars: &Vars, _: Option<(f64, f64)>) -> Result<Self> {
        parse_gradient(n, vars)
    }
    fn json(&self) -> Value {
        match self {
            Some(g) => json!({"angle": g.angle, "colors": g.colors.iter().map(|c| c.json()).collect::<Vec<_>>()}),
            None => Value::Null,
        }
    }
}

impl OptValue for String {
    fn parse(n: &Node, _: &Vars, _: Option<(f64, f64)>) -> Result<Self> {
        first(n)?.as_str().map(str::to_string).ok_or_else(|| anyhow!("expected a string"))
    }
    fn json(&self) -> Value {
        json!(self)
    }
}

impl OptValue for ModKey {
    fn parse(n: &Node, _: &Vars, _: Option<(f64, f64)>) -> Result<Self> {
        Ok(match first(n)?.as_str().unwrap_or("") {
            "Super" | "Logo" => ModKey::Super,
            "Alt" => ModKey::Alt,
            "Ctrl" | "Control" => ModKey::Ctrl,
            "Shift" => ModKey::Shift,
            s => bail!("unknown mod-key `{s}` (Super, Alt, Ctrl or Shift)"),
        })
    }
    fn json(&self) -> Value {
        json!(format!("{self:?}"))
    }
}

/// One option of a `T` (a `Deco` or the `Config`).
pub struct Opt<T: 'static> {
    pub name: &'static str,
    pub set: fn(&mut T, &Node, &Vars) -> Result<()>,
    pub get: fn(&T) -> Value,
}

macro_rules! option_table {
    ($ty:ty; $( $name:literal => $field:ident $( ($lo:expr, $hi:expr) )? ),* $(,)?) => {
        &[ $( Opt::<$ty> {
            name: $name,
            set: |t: &mut $ty, n: &Node, vars: &Vars| {
                t.$field = OptValue::parse(n, vars, option_table!(@range $( ($lo, $hi) )?))?;
                Ok(())
            },
            get: |t: &$ty| OptValue::json(&t.$field),
        } ),* ]
    };
    (@range) => { None };
    (@range ($lo:expr, $hi:expr)) => { Some(($lo as f64, $hi as f64)) };
}

/// Window decoration: options, and what window rules can set per window.
pub static DECO: &[Opt<Deco>] = option_table!(Deco;
    "border-width" => border_width (0, 64),
    "border-color" => border_color,
    "focus-color" => focus_color,
    "urgent-color" => urgent_color,
    "border-gradient" => border_gradient,
    "focus-gradient" => focus_gradient,
    "urgent-gradient" => urgent_gradient,
    "border-spin-ms" => border_spin_ms (-1e7, 1e7),
    "corner-radius" => corner_radius (0, 500),
    "shadow-size" => shadow_size (0, 500),
    "shadow-offset-x" => shadow_offset_x (-1000, 1000),
    "shadow-offset-y" => shadow_offset_y (-1000, 1000),
    "shadow-color" => shadow_color,
    "shadow-spread" => shadow_spread (-500, 500),
    "focus-shadow-color" => focus_shadow_color,
    "focus-shadow-size" => focus_shadow_size (0, 500),
    "backdrop-color" => backdrop_color,
    "opacity" => opacity (0, 1),
    "inactive-opacity" => inactive_opacity (0, 1),
    "inactive-dim" => inactive_dim (0, 1),
    "blur-radius" => blur_radius (0, 128),
    "blur-brightness" => blur_brightness (0, 2),
    "blur-saturation" => blur_saturation (0, 2),
    "blur-noise" => blur_noise (0, 1),
    "border-shader" => border_shader,
);

/// Everything else in the `options` block.
pub static GLOBAL: &[Opt<Config>] = option_table!(Config;
    "mod-key" => mod_key,
    "sloppy-focus" => sloppy_focus,
    "warp-cursor" => warp_cursor,
    "root-color" => root_color,
    "fullscreen-bg" => fullscreen_bg,
    "pan-step" => pan_step (1, 10000),
    "view-ms" => view_ms (0, 5000),
    "pan-momentum-ms" => pan_momentum_ms (0, 10000),
    "edge-pan-size" => edge_pan_size (0, 500),
    "hint-keys" => hint_keys,
    "per-window-layout" => per_window_layout,
    "hot-zone-size" => hot_zone_size (0, 50),
    "hot-zone-delay-ms" => hot_zone_delay_ms (0, 5000),
    "snap-distance" => snap_distance (0, 200),
    "snap-grid" => snap_grid (0, 2000),
    "snap-color" => snap_color,
    "frame-color" => frame_color,
    "tile-gap" => tile_gap (0, 500),
    "tile-inner-gap" => tile_inner_gap (0, 500),
    "tile-ratio" => tile_ratio (0.1, 0.9),
    "tile-masters" => tile_masters (1, 16),
    "frame-opacity" => frame_opacity (0, 1),
    "frame-label-scale" => frame_label_scale (1, 8),
    "frame-border-width" => frame_border_width (0, 20),
    "hint-scale" => hint_scale (1, 12),
    "edge-pan-speed" => edge_pan_speed (0, 10000),
    "open-ms" => open_ms (0, 5000),
    "open-scale" => open_scale (0.1, 1),
    "move-ms" => move_ms (0, 5000),
    "tag-ms" => tag_ms (0, 5000),
    "close-ms" => close_ms (0, 5000),
    "focus-ms" => focus_ms (0, 5000),
    "move-step" => move_step (1, 10000),
    "zoom-scroll" => zoom_scroll (1.01, 3),
    "zoom-settle-ms" => zoom_settle_ms (0, 10000),
    "zoom-render-scale-min" => zoom_render_scale_min (0.1, 1),
    "zoom-render-scale-max" => zoom_render_scale_max (1, 8),
    "new-window-offset" => new_window_offset (0, 500),
    "cursor-theme" => cursor_theme,
    "cursor-size" => cursor_size (8, 256),
    "hardware-cursor" => hardware_cursor,
    "direct-scanout" => direct_scanout,
    "explicit-sync" => explicit_sync,
    "capture-dmabuf" => capture_dmabuf,
    "background-shader" => background_shader,
    "session-restore" => session_restore,
    "xwayland" => xwayland,
);

/// Set option `n` from `table` on `t`. `Ok(false)`: the table has no such option.
pub fn set<T>(table: &[Opt<T>], t: &mut T, n: &Node, vars: &Vars) -> Result<bool> {
    match table.iter().find(|o| o.name == n.name) {
        Some(o) => (o.set)(t, n, vars).map(|_| true),
        None => Ok(false),
    }
}

/// Every option of `table` as `{name: value}`, the way IPC `get` reports them.
pub fn json<T>(table: &[Opt<T>], t: &T) -> Map<String, Value> {
    table.iter().map(|o| (o.name.to_string(), (o.get)(t))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_across_tables() {
        let mut names: Vec<&str> = DECO.iter().map(|o| o.name).chain(GLOBAL.iter().map(|o| o.name)).collect();
        let n = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), n);
    }

    #[test]
    fn every_option_is_documented_in_config_def_kdl() {
        let doc = crate::config::DEFAULT_CONFIG;
        let documented = |name: &str| {
            doc.lines().any(|l| {
                let l = l.trim_start().trim_start_matches("//").trim_start();
                l.split_whitespace().next() == Some(name)
            })
        };
        for o in DECO.iter().map(|o| o.name).chain(GLOBAL.iter().map(|o| o.name)) {
            assert!(documented(o), "option `{o}` is not documented in config.def.kdl");
        }
    }

    #[test]
    fn every_option_round_trips_through_get_and_set() {
        // What `get` reports can be fed back to `set` and gives the same value (what a shell or a
        // theme switcher relies on).
        let c = Config::default();
        for o in GLOBAL {
            let v = (o.get)(&c);
            let mut c2 = Config::default();
            let line = format!("{} {}", o.name, kdl_value(&v));
            let n = crate::kdl::parse(&line).unwrap().remove(0);
            (o.set)(&mut c2, &n, &Vars::new()).unwrap_or_else(|e| panic!("{line}: {e}"));
            assert_eq!((o.get)(&c2), v, "{line}");
        }
        let d = Deco {
            focus_gradient: Some(Gradient { colors: vec![[1.0, 0.0, 0.0, 1.0], [0.0, 0.0, 1.0, 1.0]], angle: 30.0 }),
            ..Default::default()
        };
        for o in DECO {
            let v = (o.get)(&d);
            let mut d2 = Deco::default();
            let line = format!("{} {}", o.name, kdl_value(&v));
            let n = crate::kdl::parse(&line).unwrap().remove(0);
            (o.set)(&mut d2, &n, &Vars::new()).unwrap_or_else(|e| panic!("{line}: {e}"));
            assert_eq!((o.get)(&d2), v, "{line}");
        }
    }

    /// A JSON value as the KDL arguments `set` takes.
    fn kdl_value(v: &Value) -> String {
        match v {
            Value::Null => String::new(),
            Value::String(s) => format!("{s:?}"),
            Value::Object(o) => {
                let colors: Vec<String> = o["colors"].as_array().unwrap().iter().map(|c| format!("{c}")).collect();
                format!("{} angle={}", colors.join(" "), o["angle"])
            }
            other => other.to_string(),
        }
    }
}
