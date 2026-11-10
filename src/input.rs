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
    state::{Drag, Uwc},
};

fn mods_of(m: &ModifiersState) -> Mods {
    Mods { logo: m.logo, alt: m.alt, ctrl: m.ctrl, shift: m.shift }
}

impl Uwc {
    pub fn process_input_event<B: InputBackend>(&mut self, event: InputEvent<B>) {
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
                let (sw, sh) = self.screen_size();
                let p: Point<f64, Logical> = (self.cursor.x + event.delta_x(), self.cursor.y + event.delta_y()).into();
                self.cursor = (p.x.clamp(0.0, sw - 1.0), p.y.clamp(0.0, sh - 1.0)).into();
                self.real_motion(t);
            }
            InputEvent::PointerMotionAbsolute { event } => {
                let t = Event::time_msec(&event);
                let size = self.mon.as_ref().map(|m| m.size).unwrap_or((1280, 720).into());
                self.cursor = event.position_transformed(size);
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
                        // Scrolling up (negative) zooms in.
                        let f = self.config.zoom_scroll.powf(-(dy / 15.0).clamp(-5.0, 5.0));
                        self.zoom_by(f);
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
        self.pointer_motion_to(self.cursor, time);
        if self.drag.is_none() && self.config.sloppy_focus {
            if let Some((i, _, _)) = self.surface_under_screen(self.cursor) {
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
        let vp = self.viewport();
        let (cx, cy) = vp.screen_to_canvas((pos.x, pos.y));
        let under = self.surface_under_screen(pos).map(|(_, s, loc)| (s, loc));
        let ptr = self.pointer();
        ptr.motion(self, under, &MotionEvent { location: (cx, cy).into(), serial: SERIAL_COUNTER.next_serial(), time });
        ptr.frame(self);
    }

    fn drag_motion(&mut self, drag: Drag, pos: Point<f64, Logical>) {
        let vp = self.viewport();
        let (cx, cy) = vp.screen_to_canvas((pos.x, pos.y));
        match drag {
            Drag::Move { id, grab, orig } => self.move_client(id, orig.x + cx - grab.x, orig.y + cy - grab.y),
            Drag::Resize { id, start, orig } => {
                self.resize_client(id, orig.w + (cx - start.x) as i32, orig.h + (cy - start.y) as i32)
            }
            Drag::Pan => {
                let (dx, dy) = (pos.x - self.drag_last.x, pos.y - self.drag_last.y);
                self.with_viewport(|v, _| v.pan_screen(dx, dy));
            }
        }
        self.drag_last = pos;
        self.damage();
    }

    pub fn begin_move(&mut self, i: usize) {
        let vp = self.viewport();
        let (cx, cy) = vp.screen_to_canvas((self.cursor.x, self.cursor.y));
        self.drag = Some(Drag::Move { id: self.clients[i].id, grab: (cx, cy).into(), orig: self.clients[i].canvas });
        self.drag_last = self.cursor;
    }

    pub fn begin_resize(&mut self, i: usize) {
        let vp = self.viewport();
        let (cx, cy) = vp.screen_to_canvas((self.cursor.x, self.cursor.y));
        self.drag = Some(Drag::Resize { id: self.clients[i].id, start: (cx, cy).into(), orig: self.clients[i].size() });
        self.drag_last = self.cursor;
    }

    fn pointer_button(&mut self, button: u32, state: ButtonState, time: u32) {
        let serial = SERIAL_COUNTER.next_serial();
        if state == ButtonState::Pressed {
            let mods = mods_of(&self.keyboard().modifier_state());
            let under = self.surface_under_screen(self.cursor).map(|(i, _, _)| i);
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
            match under {
                // A click focuses and raises, but never pans the view.
                Some(i) => self.focus_client(Some(self.clients[i].id)),
                // Dragging empty canvas pans it.
                None if button == BTN_LEFT => {
                    self.drag = Some(Drag::Pan);
                    self.drag_last = self.cursor;
                    return;
                }
                None => {}
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
