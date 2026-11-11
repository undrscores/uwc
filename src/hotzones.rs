//! Hot zones: run an action when the pointer rests against a screen corner or edge.
//!
//! ```kdl
//! hot-zones {
//!     top-left     { toggle-overview; }
//!     bottom-right { jump-hints; }
//!     left         { view-previous; }
//! }
//! ```
//!
//! The pointer has to stay in the zone for `hot-zone-delay-ms` (so passing by does nothing), and the
//! zone fires once per visit: leave it and come back to fire again. Zones are at the edges of the
//! whole screen (all monitors together), `hot-zone-size` px deep; corners win over edges. Nothing
//! fires while a button is held, a drag is going on or the session is locked.

use std::time::Duration;

use smithay::reexports::calloop::timer::{TimeoutAction, Timer};

use crate::{config::Action, state::Uwc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    Top,
    Bottom,
    Left,
    Right,
}

impl Zone {
    pub fn parse(name: &str) -> Option<Zone> {
        Some(match name {
            "top-left" => Zone::TopLeft,
            "top-right" => Zone::TopRight,
            "bottom-left" => Zone::BottomLeft,
            "bottom-right" => Zone::BottomRight,
            "top" => Zone::Top,
            "bottom" => Zone::Bottom,
            "left" => Zone::Left,
            "right" => Zone::Right,
            _ => return None,
        })
    }
}

/// The zone at `(x, y)` in a screen of `(w, h)`, zones being `size` px deep.
pub fn zone_at(p: (f64, f64), screen: (f64, f64), size: f64) -> Option<Zone> {
    if size <= 0.0 {
        return None;
    }
    let (l, r) = (p.0 < size, p.0 >= screen.0 - size);
    let (t, b) = (p.1 < size, p.1 >= screen.1 - size);
    Some(match (l, r, t, b) {
        (true, _, true, _) => Zone::TopLeft,
        (_, true, true, _) => Zone::TopRight,
        (true, _, _, true) => Zone::BottomLeft,
        (_, true, _, true) => Zone::BottomRight,
        (_, _, true, _) => Zone::Top,
        (_, _, _, true) => Zone::Bottom,
        (true, ..) => Zone::Left,
        (_, true, ..) => Zone::Right,
        _ => return None,
    })
}

#[derive(Default)]
pub struct HotState {
    /// The zone the pointer is in, and a counter so a timer from an earlier visit does nothing.
    zone: Option<Zone>,
    visit: u64,
}

impl Uwc {
    /// Called whenever the pointer moved: start the dwell timer when it enters a zone.
    pub fn hot_zone_check(&mut self) {
        if self.config.hot_zones.is_empty() {
            return;
        }
        let zone = zone_at((self.cursor.x, self.cursor.y), self.global_size(), self.config.hot_zone_size);
        if zone == self.hot.zone {
            return;
        }
        self.hot.zone = zone;
        self.hot.visit += 1;
        let Some(z) = zone.filter(|z| self.config.hot_zones.iter().any(|(h, _)| h == z)) else { return };
        let visit = self.hot.visit;
        let delay = Duration::from_millis(self.config.hot_zone_delay_ms as u64);
        let _ = self.loop_handle.insert_source(Timer::from_duration(delay), move |_, _, state| {
            // Still the same visit, nothing held, nothing locked.
            if state.hot.visit == visit && state.hot.zone == Some(z) && state.drag.is_none() && !state.is_locked() {
                let action: Option<Action> =
                    state.config.hot_zones.iter().find(|(h, _)| *h == z).map(|(_, a)| a.clone());
                if let Some(a) = action {
                    state.run_action(&a);
                }
            }
            TimeoutAction::Drop
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corners_win_over_edges() {
        let s = (1000.0, 600.0);
        assert_eq!(zone_at((0.0, 0.0), s, 2.0), Some(Zone::TopLeft));
        assert_eq!(zone_at((999.0, 0.0), s, 2.0), Some(Zone::TopRight));
        assert_eq!(zone_at((0.0, 599.0), s, 2.0), Some(Zone::BottomLeft));
        assert_eq!(zone_at((999.0, 599.0), s, 2.0), Some(Zone::BottomRight));
        assert_eq!(zone_at((500.0, 0.0), s, 2.0), Some(Zone::Top));
        assert_eq!(zone_at((1.0, 300.0), s, 2.0), Some(Zone::Left));
        assert_eq!(zone_at((998.0, 300.0), s, 2.0), Some(Zone::Right));
        assert_eq!(zone_at((500.0, 300.0), s, 2.0), None);
        assert_eq!(zone_at((0.0, 0.0), s, 0.0), None, "size 0 turns zones off");
        assert_eq!(Zone::parse("bottom-left"), Some(Zone::BottomLeft));
        assert_eq!(Zone::parse("middle"), None);
    }
}
