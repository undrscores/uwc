//! Keyboard, pointer and drag handling. Everything the user does with an input device ends up here,
//! whichever backend it came from.

use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent, KeyState,
        KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    },
    input::{
        keyboard::{FilterResult, ModifiersState},
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
    utils::{Logical, Point, SERIAL_COUNTER},
};

use crate::{
    config::{Action, ModKey, Mods, BTN_LEFT},
    layer::Under,
    state::{Drag, Uwc},
};

fn mods_of(m: &ModifiersState) -> Mods {
    Mods { logo: m.logo, alt: m.alt, ctrl: m.ctrl, shift: m.shift }
}

impl Uwc {
    pub fn process_input_event<B: InputBackend>(&mut self, event: InputEvent<B>) {
        // Real user input, from whichever backend: it resets the idle timers (ext-idle-notify).
        self.idle_notifier.notify_activity(&self.seat);
        match event {
            InputEvent::Keyboard { event } => {
                let serial = SERIAL_COUNTER.next_serial();
                let time = Event::time_msec(&event);
                let pressed = event.state() == KeyState::Pressed;
                let kb = self.keyboard();
                let action = kb.input::<Action, _>(
                    self,
                    event.key_code(),
                    event.state(),
                    serial,
                    time,
                    |state, modifiers, handle| {
                        if pressed {
                            let m = mods_of(modifiers);
                            for bind in &state.config.keys {
                                if bind.mods == m
                                    && handle.raw_syms().iter().any(|&s| crate::config::lower(s) == bind.sym)
                                {
                                    return FilterResult::Intercept(bind.action.clone());
                                }
                            }
                        }
                        FilterResult::Forward
                    },
                );
                if let Some(a) = action {
                    self.run_action(&a);
                }
            }
            InputEvent::PointerMotion { event } => {
                let t = Event::time_msec(&event);
                let (sw, sh) = self.global_size();
                let p: Point<f64, Logical> = (self.cursor.x + event.delta_x(), self.cursor.y + event.delta_y()).into();
                self.cursor = (p.x.clamp(0.0, sw - 1.0), p.y.clamp(0.0, sh - 1.0)).into();
                self.real_motion(t);
            }
            InputEvent::PointerMotionAbsolute { event } => {
                let t = Event::time_msec(&event);
                let (w, h) = self.global_size();
                self.cursor = event.position_transformed((w as i32, h as i32).into());
                self.real_motion(t);
            }
            InputEvent::PointerButton { event } => {
                self.pointer_button(event.button_code(), event.state(), Event::time_msec(&event))
            }
            InputEvent::PointerAxis { event } => {
                let time = Event::time_msec(&event);
                let mods = self.keyboard().modifier_state();
                if self.mod_held(&mods) {
                    let dy = event
                        .amount(Axis::Vertical)
                        .or_else(|| event.amount_v120(Axis::Vertical).map(|v| v / 120.0 * 15.0))
                        .unwrap_or(0.0);
                    if dy != 0.0 {
                        // Scrolling up (negative) zooms in. With Ctrl too, only the window under the pointer.
                        let f = self.config.zoom_scroll.powf(-(dy / 15.0).clamp(-5.0, 5.0));
                        let under = self.under(self.cursor).and_then(|u| u.window()).map(|i| self.clients[i].id);
                        match under {
                            Some(id) if mods.ctrl => self.scale_window(id, f, Some(self.cursor)),
                            _ => self.zoom_by(f),
                        }
                    }
                    return;
                }
                let mut frame = AxisFrame::new(time).source(event.source());
                for axis in [Axis::Horizontal, Axis::Vertical] {
                    let v120 = event.amount_v120(axis);
                    let amount = event.amount(axis).or_else(|| v120.map(|v| v * 15.0 / 120.0)).unwrap_or(0.0);
                    if amount != 0.0 {
                        frame = frame.relative_direction(axis, event.relative_direction(axis)).value(axis, amount);
                        if let Some(v) = v120 {
                            frame = frame.v120(axis, v as i32);
                        }
                    } else if event.source() == AxisSource::Finger {
                        frame = frame.stop(axis);
                    }
                }
                let ptr = self.pointer();
                ptr.axis(self, frame);
                ptr.frame(self);
            }
            _ => {}
        }
    }

    fn mod_held(&self, m: &ModifiersState) -> bool {
        match self.config.mod_key {
            ModKey::Super => m.logo,
            ModKey::Alt => m.alt,
            ModKey::Ctrl => m.ctrl,
            ModKey::Shift => m.shift,
        }
    }

    /// Pointer moved by the user (as opposed to the scene moving under it): applies drags and sloppy focus.
    fn real_motion(&mut self, time: u32) {
        // The pointer is drawn by uwc, so moving it is a new frame.
        self.redraw = true;
        // The monitor the pointer is on is where view actions apply.
        self.cur = self.mon_at(self.cursor);
        self.pointer_motion_to(self.cursor, time);
        // A popup grab keeps the focus where it is while the pointer moves over others.
        if self.drag.is_none() && self.config.sloppy_focus && !self.pointer().is_grabbed() {
            if let Some(i) = self.under(self.cursor).and_then(|u| u.window()) {
                let id = self.clients[i].id;
                if self.focus != Some(id) {
                    // Sloppy focus does not raise or pan; it only hands over keyboard focus.
                    self.set_focus(Some(id), false);
                }
            }
        }
        self.ipc.dirty = true;
    }

    /// Send the pointer to `pos` (screen px): moves a drag, or tells the surface under it.
    pub fn pointer_motion_to(&mut self, pos: Point<f64, Logical>, time: u32) {
        if let Some(drag) = self.drag {
            self.drag_motion(drag, pos);
            return;
        }
        // Canvas windows live in canvas px; pinned and fullscreen windows and layer surfaces in screen
        // px. The location handed to the seat is in the space of whatever is under the pointer, so
        // surface-local coordinates come out right whatever the zoom.
        let m = self.mon_at(pos);
        let local = pos - self.monitors.get(m).map(|x| x.origin).unwrap_or((0.0, 0.0).into());
        let (location, focus) = match self.under(pos) {
            Some(Under::Window { surface, origin, location, .. }) => (location, Some((surface, origin))),
            Some(Under::Layer { surface, origin, .. }) => (local, Some((surface, origin))),
            None => (local, None),
        };
        let ptr = self.pointer();
        ptr.motion(self, focus, &MotionEvent { location, serial: SERIAL_COUNTER.next_serial(), time });
        ptr.frame(self);
    }

    fn drag_motion(&mut self, drag: Drag, pos: Point<f64, Logical>) {
        // The screen delta divided by `z`.
        let delta = |start: Point<f64, Logical>, z: f64| ((pos.x - start.x) / z, (pos.y - start.y) / z);
        // A move is in canvas px (screen px for a pinned window); a resize in the window's own px.
        let zooms = |s: &Uwc, id: u32| -> (f64, f64) {
            match s.client_index(id) {
                Some(i) => {
                    let pl = s.place_at_rest(&s.clients[i]);
                    (pl.zoom / s.clients[i].scale, pl.zoom)
                }
                None => (1.0, 1.0),
            }
        };
        match drag {
            Drag::Move { id, start, orig } => {
                let (dx, dy) = delta(start, zooms(self, id).0);
                match self.client_index(id) {
                    Some(i) if self.clients[i].pin.is_some() => {
                        self.clients[i].pin = Some((orig.x + dx, orig.y + dy).into());
                        self.refresh_pointer();
                    }
                    _ => self.move_client(id, orig.x + dx, orig.y + dy),
                }
            }
            Drag::Resize { id, start, orig } => {
                let (dx, dy) = delta(start, zooms(self, id).1);
                self.resize_client(id, orig.w + dx as i32, orig.h + dy as i32)
            }
            Drag::Pan => {
                let (dx, dy) = (pos.x - self.drag_last.x, pos.y - self.drag_last.y);
                self.with_viewport(|v, _| v.pan_screen(dx, dy));
            }
        }
        self.drag_last = pos;
        self.damage();
    }

    /// Fullscreen windows are not dragged.
    pub fn begin_move(&mut self, i: usize) {
        let c = &self.clients[i];
        if c.fullscreen {
            return;
        }
        let orig = c.pin.unwrap_or(c.canvas);
        self.drag = Some(Drag::Move { id: c.id, start: self.cursor, orig });
        self.drag_last = self.cursor;
    }

    pub fn begin_resize(&mut self, i: usize) {
        let c = &self.clients[i];
        if c.fullscreen {
            return;
        }
        self.drag = Some(Drag::Resize { id: c.id, start: self.cursor, orig: c.size() });
        self.drag_last = self.cursor;
    }

    fn pointer_button(&mut self, button: u32, state: ButtonState, time: u32) {
        let serial = SERIAL_COUNTER.next_serial();
        // A grab (a popup menu, a drag-and-drop) takes the button: a press outside a grabbed popup
        // dismisses it, and is not also a focus change, a drag or a pan on top of that.
        if self.pointer().is_grabbed() {
            let ptr = self.pointer();
            ptr.button(self, &ButtonEvent { button, state, serial, time });
            ptr.frame(self);
            return;
        }
        if state == ButtonState::Pressed {
            let mods = mods_of(&self.keyboard().modifier_state());
            let hit = self.under(self.cursor);
            let under = hit.as_ref().and_then(Under::window);
            let bind =
                self.config.mouse.iter().find(|b| b.button == button && b.mods == mods).map(|b| b.action.clone());
            match (bind, under) {
                (Some(Action::DragMove), Some(i)) => {
                    self.focus_client(Some(self.clients[i].id));
                    self.begin_move(i);
                    return;
                }
                (Some(Action::DragResize), Some(i)) => {
                    self.focus_client(Some(self.clients[i].id));
                    self.begin_resize(i);
                    return;
                }
                (Some(Action::DragPan), _) => {
                    self.drag = Some(Drag::Pan);
                    self.drag_last = self.cursor;
                    return;
                }
                _ => {}
            }
            match (&hit, under) {
                // A click on a layer surface that takes keyboard input gives it the keyboard.
                (Some(Under::Layer { root, interactive: true, .. }), _) => self.set_layer_focus(Some(root.clone())),
                (Some(Under::Layer { .. }), _) => {}
                // A click focuses and raises, but never pans the view. It takes the keyboard back from a
                // layer surface unless that one holds it exclusively.
                (_, Some(i)) => {
                    if !self.layer_focus_is_exclusive() {
                        self.layer_focus = None;
                    }
                    self.focus_client(Some(self.clients[i].id));
                }
                // Dragging empty canvas pans it.
                (None, None) if button == BTN_LEFT => {
                    self.drag = Some(Drag::Pan);
                    self.drag_last = self.cursor;
                    return;
                }
                _ => {}
            }
        } else if self.drag.take().is_some() {
            // The press that started the drag never reached a client, so neither does its release.
            self.refresh_pointer();
            return;
        }
        let ptr = self.pointer();
        ptr.button(self, &ButtonEvent { button, state, serial, time });
        ptr.frame(self);
    }
}
