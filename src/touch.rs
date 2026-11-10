//! Touchscreen input. A touch on a window or layer surface goes to its client (and focuses it, like
//! a click); a touch on empty canvas belongs to the compositor: one finger drags the canvas, two
//! pan and pinch-zoom it. The gesture arithmetic is `Pinch`, pure and unit-tested; the rest is
//! `impl Uwc` plumbing that backends feed through `process_input_event`.

use std::collections::HashSet;

use smithay::{
    backend::input::{AbsolutePositionEvent, Event, InputBackend, TouchEvent, TouchSlot},
    input::touch::{DownEvent, MotionEvent, UpEvent},
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, SERIAL_COUNTER},
};

use crate::{layer::Under, state::Uwc};

/// What a moved finger asks of the view: pan by `(dx, dy)` screen px, then scale by `zoom` about `at`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Step {
    pub dx: f64,
    pub dy: f64,
    pub zoom: f64,
    pub at: (f64, f64),
}

/// The fingers the compositor owns (those that went down on empty canvas), by slot.
#[derive(Debug, Default)]
pub struct Pinch {
    fingers: Vec<(i32, (f64, f64))>,
}

impl Pinch {
    pub fn down(&mut self, slot: i32, at: (f64, f64)) {
        self.fingers.retain(|f| f.0 != slot);
        self.fingers.push((slot, at));
    }

    pub fn up(&mut self, slot: i32) {
        self.fingers.retain(|f| f.0 != slot);
    }

    pub fn owns(&self, slot: i32) -> bool {
        self.fingers.iter().any(|f| f.0 == slot)
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.fingers.is_empty()
    }

    /// Centre and spread of the first two fingers (the spread of one finger is 0).
    fn shape(&self) -> ((f64, f64), f64) {
        match self.fingers.as_slice() {
            [] => ((0.0, 0.0), 0.0),
            [(_, a)] => (*a, 0.0),
            [(_, a), (_, b), ..] => (((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0), (a.0 - b.0).hypot(a.1 - b.1)),
        }
    }

    /// A finger moved. `None` if it is not ours or nothing changed.
    pub fn motion(&mut self, slot: i32, to: (f64, f64)) -> Option<Step> {
        let i = self.fingers.iter().position(|f| f.0 == slot)?;
        let (c0, s0) = self.shape();
        self.fingers[i].1 = to;
        let (c1, s1) = self.shape();
        // Only the first two fingers shape the gesture; a third one moving changes nothing.
        let zoom = if s0 > 1.0 && s1 > 1.0 { s1 / s0 } else { 1.0 };
        let step = Step { dx: c1.0 - c0.0, dy: c1.1 - c0.1, zoom, at: c1 };
        (step.dx != 0.0 || step.dy != 0.0 || zoom != 1.0).then_some(step)
    }
}

/// A surface and its origin in the space the location is given in.
type Focus = (WlSurface, Point<f64, Logical>);

/// Seat-side bookkeeping: which slots are held by the compositor, which by a client.
#[derive(Debug, Default)]
pub struct TouchState {
    pub pinch: Pinch,
    /// The slots whose touch a client has.
    pub forwarded: HashSet<i32>,
}

impl Uwc {
    pub fn touch_input_down<B: InputBackend>(&mut self, event: B::TouchDownEvent) {
        let slot = i32::from(event.slot());
        let (w, h) = self.global_size();
        let pos = event.position_transformed((w as i32, h as i32).into());
        let time = Event::time_msec(&event);
        self.touch_down(slot, pos, time);
    }

    pub fn touch_input_motion<B: InputBackend>(&mut self, event: B::TouchMotionEvent) {
        let slot = i32::from(event.slot());
        let (w, h) = self.global_size();
        let pos = event.position_transformed((w as i32, h as i32).into());
        let time = Event::time_msec(&event);
        self.touch_motion(slot, pos, time);
    }

    pub fn touch_input_up<B: InputBackend>(&mut self, event: B::TouchUpEvent) {
        let slot = i32::from(event.slot());
        self.touch_up(slot, Event::time_msec(&event));
    }

    pub fn touch_cancel(&mut self) {
        self.touch.pinch = Pinch::default();
        self.touch.forwarded.clear();
        if let Some(t) = self.seat.get_touch() {
            t.cancel(self);
        }
    }

    pub fn touch_down(&mut self, slot: i32, pos: Point<f64, Logical>, time: u32) {
        self.idle_notifier.notify_activity(&self.seat);
        self.cur = self.mon_at(pos);
        let local = pos - self.origin_of(self.cur);
        let tslot = TouchSlot::from(Some(slot as u32));
        let serial = SERIAL_COUNTER.next_serial();
        if self.is_locked() {
            let focus = self.lock_surface_on(self.cur).map(|s| (s.wl_surface().clone(), (0.0, 0.0).into()));
            if let (Some(t), true) = (self.seat.get_touch(), focus.is_some()) {
                t.down(self, focus, &DownEvent { slot: tslot, location: local, serial, time });
                self.touch.forwarded.insert(slot);
            }
            return;
        }
        let hit = self.under(pos);
        let (location, focus) = match &hit {
            Some(Under::Window { surface, origin, location, .. }) => (*location, Some((surface.clone(), *origin))),
            Some(Under::Layer { surface, origin, .. }) => (local, Some((surface.clone(), *origin))),
            None => (local, None),
        };
        match &hit {
            Some(Under::Layer { root, interactive: true, .. }) => self.set_layer_focus(Some(root.clone())),
            Some(Under::Layer { .. }) => {}
            Some(u) => {
                if let Some(i) = u.window() {
                    if !self.layer_focus_is_exclusive() {
                        self.layer_focus = None;
                    }
                    self.focus_client(Some(self.clients[i].id));
                }
            }
            None => {}
        }
        if focus.is_some() {
            if let Some(t) = self.seat.get_touch() {
                t.down(self, focus, &DownEvent { slot: tslot, location, serial, time });
                t.frame(self);
                self.touch.forwarded.insert(slot);
            }
        } else {
            self.touch.pinch.down(slot, (local.x, local.y));
        }
    }

    pub fn touch_motion(&mut self, slot: i32, pos: Point<f64, Logical>, time: u32) {
        self.idle_notifier.notify_activity(&self.seat);
        let local = pos - self.origin_of(self.cur);
        if self.touch.pinch.owns(slot) {
            if let Some(s) = self.touch.pinch.motion(slot, (local.x, local.y)) {
                self.with_viewport(|v, _| {
                    v.pan_screen(s.dx, s.dy);
                    if s.zoom != 1.0 {
                        v.zoom_at(s.zoom, s.at);
                    }
                });
            }
            return;
        }
        if !self.touch.forwarded.contains(&slot) {
            return;
        }
        // The client keeps the touch it started: its location is in the space of whatever it went
        // down on (canvas px for a window, screen px for a pinned one or a layer surface).
        let tslot = TouchSlot::from(Some(slot as u32));
        let (location, focus) = match self.under_touch(pos) {
            Some((location, focus)) => (location, Some(focus)),
            None => (local, None),
        };
        if let Some(t) = self.seat.get_touch() {
            t.motion(self, focus, &MotionEvent { slot: tslot, location, time });
            t.frame(self);
        }
    }

    pub fn touch_up(&mut self, slot: i32, time: u32) {
        if self.touch.pinch.owns(slot) {
            self.touch.pinch.up(slot);
            return;
        }
        if self.touch.forwarded.remove(&slot) {
            let tslot = TouchSlot::from(Some(slot as u32));
            let serial = SERIAL_COUNTER.next_serial();
            if let Some(t) = self.seat.get_touch() {
                t.up(self, &UpEvent { slot: tslot, serial, time });
                t.frame(self);
            }
        }
    }

    /// Location and focus for a moving touch: what is under it now. Smithay keeps the focus the
    /// touch went down with, so this only has to be in the right coordinate space.
    fn under_touch(&self, pos: Point<f64, Logical>) -> Option<(Point<f64, Logical>, Focus)> {
        match self.under(pos)? {
            Under::Window { surface, origin, location, .. } => Some((location, (surface, origin))),
            Under::Layer { surface, origin, .. } => Some((pos - self.origin_of(self.cur), (surface, origin))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_finger_drags() {
        let mut p = Pinch::default();
        p.down(0, (100.0, 100.0));
        let s = p.motion(0, (110.0, 95.0)).unwrap();
        assert_eq!((s.dx, s.dy, s.zoom), (10.0, -5.0, 1.0));
        assert!(p.motion(7, (0.0, 0.0)).is_none(), "not our finger");
        assert!(p.motion(0, (110.0, 95.0)).is_none(), "no movement, no step");
    }

    #[test]
    fn two_fingers_pinch_about_their_centre() {
        let mut p = Pinch::default();
        p.down(0, (100.0, 100.0));
        p.down(1, (200.0, 100.0));
        // Spread the second finger: the distance doubles, the centre moves by half of what it moved.
        let s = p.motion(1, (300.0, 100.0)).unwrap();
        assert_eq!(s.zoom, 2.0);
        assert_eq!((s.dx, s.dy), (50.0, 0.0));
        assert_eq!(s.at, (200.0, 100.0));
        // Lifting one leaves a dragging finger again, with no zoom.
        p.up(0);
        let s = p.motion(1, (310.0, 100.0)).unwrap();
        assert_eq!((s.zoom, s.dx), (1.0, 10.0));
        p.up(1);
        assert!(p.is_empty());
    }
}
