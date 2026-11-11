//! Keyboard, pointer and drag handling. Everything the user does with an input device ends up here,
//! whichever backend it came from.

use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, GestureBeginEvent, GestureEndEvent,
        GesturePinchUpdateEvent, GestureSwipeUpdateEvent, InputBackend, InputEvent, KeyState, KeyboardKeyEvent,
        PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    },
    input::{
        keyboard::{FilterResult, ModifiersState},
        pointer::{
            AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
            GesturePinchEndEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent, MotionEvent,
        },
    },
    utils::{Logical, Point, SERIAL_COUNTER},
};

use crate::{
    config::{Action, ModKey, Mods, BTN_LEFT},
    layer::Under,
    state::{Drag, Gesture, Uwc},
};

fn mods_of(m: &ModifiersState) -> Mods {
    Mods { logo: m.logo, alt: m.alt, ctrl: m.ctrl, shift: m.shift }
}

/// Whether a key bind may run now. While the session is locked, or a client holds the shortcuts (a
/// VM, a remote desktop), only the ways out do: switching VT (so a lock client that died, or took the
/// lock without meaning well, cannot trap you) and, for inhibited shortcuts, taking them back.
pub fn bind_runs(action: &Action, locked: bool, inhibited: bool) -> bool {
    match action {
        Action::SwitchVt(_) => true,
        Action::ToggleShortcutsInhibit => !locked,
        _ => !locked && !inhibited,
    }
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
                        // Hint mode takes every key, pressed or released, away from binds and clients.
                        if state.hints_active() && !state.is_locked() {
                            let sym = handle.modified_sym();
                            let key = if !pressed {
                                crate::hints::HintKey::Ignore
                            } else if sym == smithay::input::keyboard::Keysym::Escape {
                                crate::hints::HintKey::Cancel
                            } else if sym == smithay::input::keyboard::Keysym::BackSpace {
                                crate::hints::HintKey::Backspace
                            } else {
                                match char::from_u32(smithay::input::keyboard::xkb::keysym_to_utf32(sym)) {
                                    Some(c) if c.is_ascii_alphanumeric() => crate::hints::HintKey::Char(c),
                                    _ => crate::hints::HintKey::Ignore,
                                }
                            };
                            return FilterResult::Intercept(Action::Hint(key));
                        }
                        if pressed {
                            let m = mods_of(modifiers);
                            let (locked, inhibited) = (state.is_locked(), state.shortcuts_inhibited());
                            for bind in &state.config.keys {
                                if !bind_runs(&bind.action, locked, inhibited) {
                                    continue;
                                }
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
                // A layout key (grp:alt_shift_toggle and friends) may have switched the layout.
                self.refresh_layout();
            }
            InputEvent::PointerMotion { event } => {
                self.pointer_moved_by(event.delta_x(), event.delta_y(), Event::time_msec(&event));
            }
            InputEvent::PointerMotionAbsolute { event } => {
                let (w, h) = self.global_size();
                let p = event.position_transformed((w as i32, h as i32).into());
                self.pointer_moved_to(p, Event::time_msec(&event));
            }
            InputEvent::PointerButton { event } => {
                self.pointer_button(event.button_code(), event.state(), Event::time_msec(&event))
            }
            InputEvent::PointerAxis { event } => {
                let time = Event::time_msec(&event);
                let mods = self.keyboard().modifier_state();
                if !self.is_locked() && self.mod_held(&mods) {
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
            // Graphics tablets act as an absolute mouse: the pen moves the pointer, touching down
            // is the left button, the pen's buttons are the buttons they report. (Pressure and
            // tilt are not forwarded: there is no tablet protocol yet.)
            InputEvent::TabletToolAxis { event } => {
                let (w, h) = self.global_size();
                let p = event.position_transformed((w as i32, h as i32).into());
                self.pointer_moved_to(p, Event::time_msec(&event));
            }
            InputEvent::TabletToolTip { event } => {
                use smithay::backend::input::{TabletToolTipEvent, TabletToolTipState};
                let state = match event.tip_state() {
                    TabletToolTipState::Down => ButtonState::Pressed,
                    TabletToolTipState::Up => ButtonState::Released,
                };
                self.idle_notifier.notify_activity(&self.seat);
                let (w, h) = self.global_size();
                let p: Point<f64, Logical> = (event.x_transformed(w as i32), event.y_transformed(h as i32)).into();
                self.pointer_moved_to(p, Event::time_msec(&event));
                self.pointer_button(BTN_LEFT, state, Event::time_msec(&event));
            }
            InputEvent::TabletToolButton { event } => {
                use smithay::backend::input::TabletToolButtonEvent;
                self.pointer_button(event.button(), event.button_state(), Event::time_msec(&event));
            }
            InputEvent::TouchDown { event } => self.touch_input_down::<B>(event),
            InputEvent::TouchMotion { event } => self.touch_input_motion::<B>(event),
            InputEvent::TouchUp { event } => self.touch_input_up::<B>(event),
            InputEvent::TouchCancel { .. } => self.touch_cancel(),
            // Touchpad gestures. What a gesture does is bound in `gestures { }` (by default a
            // 3-finger swipe pans the view, a 4-finger swipe focuses a neighbour, a 2-finger pinch
            // zooms); a gesture with no bind goes to the client under the pointer. libinput reports
            // 3/4-finger swipes as pinches on some trackpads (the Magic Trackpad does), so a pinch
            // whose finger count has swipe binds is treated as that swipe (Config::pinch_plan).
            InputEvent::GestureSwipeBegin { event } => {
                self.gesture_swipe_begin(event.fingers(), Event::time_msec(&event))
            }
            InputEvent::GestureSwipeUpdate { event } => {
                self.gesture_swipe_update(event.delta_x(), event.delta_y(), Event::time_msec(&event))
            }
            InputEvent::GestureSwipeEnd { event } => {
                self.gesture_swipe_end(event.cancelled(), Event::time_msec(&event))
            }
            InputEvent::GesturePinchBegin { event } => {
                self.gesture_pinch_begin(event.fingers(), Event::time_msec(&event))
            }
            InputEvent::GesturePinchUpdate { event } => self.gesture_pinch_update(
                event.delta_x(),
                event.delta_y(),
                event.scale(),
                event.rotation(),
                Event::time_msec(&event),
            ),
            InputEvent::GesturePinchEnd { event } => {
                self.gesture_pinch_end(event.cancelled(), Event::time_msec(&event))
            }
            InputEvent::GestureHoldBegin { event } => {
                let (time, fingers) = (Event::time_msec(&event), event.fingers());
                if !self.is_locked() {
                    self.pointer().gesture_hold_begin(
                        self,
                        &GestureHoldBeginEvent { serial: SERIAL_COUNTER.next_serial(), time, fingers },
                    );
                }
            }
            InputEvent::GestureHoldEnd { event } => {
                let (time, cancelled) = (Event::time_msec(&event), event.cancelled());
                if !self.is_locked() {
                    self.pointer().gesture_hold_end(
                        self,
                        &GestureHoldEndEvent { serial: SERIAL_COUNTER.next_serial(), time, cancelled },
                    );
                }
            }
            _ => {}
        }
    }

    /// A swipe with `fingers` fingers began: take it if `gestures { }` binds it, else send it to the
    /// client under the pointer. On a locked screen nothing happens; during a drag or grab the
    /// client gets it.
    pub(crate) fn gesture_swipe_begin(&mut self, fingers: u32, time: u32) {
        if self.is_locked() {
            return;
        }
        if self.drag.is_some() || self.pointer().is_grabbed() {
            return self.forward_swipe_begin(fingers, time);
        }
        match self.config.swipe_plan(fingers) {
            Some(g) => {
                tracing::debug!("swipe begin: {fingers} fingers -> {g:?}");
                self.gesture = Some(g);
                self.fling = None;
                self.pan_vel = (0.0, 0.0);
                self.pan_sampled = std::time::Instant::now();
            }
            None => self.forward_swipe_begin(fingers, time),
        }
    }

    pub(crate) fn gesture_swipe_update(&mut self, dx: f64, dy: f64, time: u32) {
        if self.is_locked() {
            self.gesture = None;
            return;
        }
        match self.gesture.take() {
            Some(Gesture::Pan) => {
                self.gesture_pan(dx, dy);
                self.gesture = Some(Gesture::Pan);
            }
            Some(Gesture::Dir { dirs, dx: ax, dy: ay }) => {
                self.gesture = Some(Gesture::Dir { dirs, dx: ax + dx, dy: ay + dy });
            }
            Some(g) => self.gesture = Some(g), // End/Zoom: nothing happens mid-gesture
            None => self.pointer().gesture_swipe_update(
                self,
                &smithay::input::pointer::GestureSwipeUpdateEvent { time, delta: (dx, dy).into() },
            ),
        }
    }

    pub(crate) fn gesture_swipe_end(&mut self, cancelled: bool, time: u32) {
        if self.gesture.is_none() {
            if !self.is_locked() {
                self.pointer().gesture_swipe_end(
                    self,
                    &GestureSwipeEndEvent { serial: SERIAL_COUNTER.next_serial(), time, cancelled },
                );
            }
            return;
        }
        if self.is_locked() {
            self.gesture = None;
            return;
        }
        self.gesture_end(cancelled);
    }

    pub(crate) fn gesture_pinch_begin(&mut self, fingers: u32, time: u32) {
        if self.is_locked() {
            return;
        }
        if self.drag.is_some() || self.pointer().is_grabbed() {
            return self.forward_pinch_begin(fingers, time);
        }
        match self.config.pinch_plan(fingers) {
            Some(g) => {
                tracing::debug!("pinch begin: {fingers} fingers -> {g:?}");
                self.gesture = Some(g);
                self.fling = None;
                self.pan_vel = (0.0, 0.0);
                self.pan_sampled = std::time::Instant::now();
            }
            None => self.forward_pinch_begin(fingers, time),
        }
    }

    pub(crate) fn gesture_pinch_update(&mut self, dx: f64, dy: f64, scale: f64, rotation: f64, time: u32) {
        if self.is_locked() {
            self.gesture = None;
            return;
        }
        match self.gesture.take() {
            Some(Gesture::Zoom { last }) => {
                // The pinch's scale is absolute since the begin; each update zooms by what it
                // changed since the last one, and pans with the fingers' drift so the content
                // stays under them.
                if last > 0.0 && (scale / last).is_finite() {
                    self.zoom_by(scale / last);
                }
                if dx != 0.0 || dy != 0.0 {
                    self.with_viewport(|v, _| v.pan_screen(dx, dy));
                    self.refresh_pointer();
                    self.damage();
                }
                self.gesture = Some(Gesture::Zoom { last: scale });
            }
            // A swipe the device reported as a pinch: the deltas are the swipe's.
            Some(Gesture::Pan) => {
                self.gesture_pan(dx, dy);
                self.gesture = Some(Gesture::Pan);
            }
            Some(Gesture::Dir { dirs, dx: ax, dy: ay }) => {
                self.gesture = Some(Gesture::Dir { dirs, dx: ax + dx, dy: ay + dy });
            }
            Some(g) => self.gesture = Some(g),
            None => self.pointer().gesture_pinch_update(
                self,
                &smithay::input::pointer::GesturePinchUpdateEvent { time, delta: (dx, dy).into(), scale, rotation },
            ),
        }
    }

    pub(crate) fn gesture_pinch_end(&mut self, cancelled: bool, time: u32) {
        if self.gesture.is_none() {
            if !self.is_locked() {
                self.pointer().gesture_pinch_end(
                    self,
                    &GesturePinchEndEvent { serial: SERIAL_COUNTER.next_serial(), time, cancelled },
                );
            }
            return;
        }
        if self.is_locked() {
            self.gesture = None;
            return;
        }
        self.gesture_end(cancelled);
    }

    /// The fingers lifted: finish a taken gesture (coast a pan on, run a bound action), unless the
    /// gesture was cancelled.
    fn gesture_end(&mut self, cancelled: bool) {
        let Some(g) = self.gesture.take() else { return };
        if cancelled {
            return;
        }
        match g {
            Gesture::Pan => self.pan_release(),
            Gesture::Dir { dirs, dx, dy } => {
                if let Some(dir) = crate::config::swipe_direction(dx, dy, self.config.swipe_threshold) {
                    if let Some((_, action)) = dirs.iter().find(|(d, _)| *d == dir) {
                        tracing::debug!("swipe end ({dx:.1}, {dy:.1}) -> {dir:?} {action:?}");
                        self.run_action(action);
                    }
                }
            }
            Gesture::End(action) => self.run_action(&action),
            Gesture::Zoom { .. } => {}
        }
    }

    /// Pan the view by a swipe delta (scaled by `swipe-gain`), remembering the speed for a fling.
    fn gesture_pan(&mut self, dx: f64, dy: f64) {
        let g = self.config.swipe_gain;
        let (dx, dy) = (dx * g, dy * g);
        self.with_viewport(|v, _| v.pan_screen(dx, dy));
        self.pan_sample(dx, dy);
        self.refresh_pointer();
        self.damage();
    }

    fn forward_swipe_begin(&mut self, fingers: u32, time: u32) {
        self.pointer()
            .gesture_swipe_begin(self, &GestureSwipeBeginEvent { serial: SERIAL_COUNTER.next_serial(), time, fingers });
    }

    fn forward_pinch_begin(&mut self, fingers: u32, time: u32) {
        self.pointer()
            .gesture_pinch_begin(self, &GesturePinchBeginEvent { serial: SERIAL_COUNTER.next_serial(), time, fingers });
    }

    /// The pointer moved by `(dx, dy)` (a mouse, or a virtual pointer).
    pub fn pointer_moved_by(&mut self, dx: f64, dy: f64, t: u32) {
        let (sw, sh) = self.global_size();
        let p: Point<f64, Logical> = (self.cursor.x + dx, self.cursor.y + dy).into();
        // A locked pointer consumes the motion (the client reads the deltas); a
        // confined one is kept inside the window.
        if let Some(p) = self.apply_pointer_constraint(p, (dx, dy), t) {
            self.cursor = (p.x.clamp(0.0, sw - 1.0), p.y.clamp(0.0, sh - 1.0)).into();
            self.real_motion(t);
        }
    }

    /// The pointer was put at `p` (a tablet or other absolute device, or a virtual pointer).
    pub fn pointer_moved_to(&mut self, p: Point<f64, Logical>, t: u32) {
        let d = (p.x - self.cursor.x, p.y - self.cursor.y);
        if let Some(p) = self.apply_pointer_constraint(p, d, t) {
            self.cursor = p;
            self.real_motion(t);
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
        self.hot_zone_check();
        self.ipc.dirty = true;
    }

    /// Send the pointer to `pos` (screen px): moves a drag, or tells the surface under it.
    pub fn pointer_motion_to(&mut self, pos: Point<f64, Logical>, time: u32) {
        // A session lock takes the pointer: it goes to the lock surface, nowhere else.
        if self.is_locked() {
            let m = self.mon_at(pos);
            let local = pos - self.origin_of(m);
            let focus = self.lock_surface_on(m).map(|s| (s.wl_surface().clone(), (0.0, 0.0).into()));
            let ptr = self.pointer();
            ptr.motion(self, focus, &MotionEvent { location: local, serial: SERIAL_COUNTER.next_serial(), time });
            ptr.frame(self);
            return;
        }
        if let Some(drag) = self.drag {
            // Moving a window refreshes the pointer, which lands here again: only the outermost
            // call applies the drag (without this, any drag-move overflowed the stack).
            if !std::mem::replace(&mut self.in_drag_motion, true) {
                self.drag_motion(drag, pos);
                self.in_drag_motion = false;
            }
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
        // A persistent constraint re-activates when the pointer enters its surface again.
        if let Some((surface, _)) = &focus {
            self.pointer_constraint_entered(surface);
        }
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
                    _ => {
                        let (x, y, guides) = self.snap_move(id, orig.x + dx, orig.y + dy);
                        if guides != self.guides.list {
                            self.guides.list = guides;
                            self.deco_commit.increment();
                        }
                        self.move_client(id, x, y)
                    }
                }
            }
            Drag::Resize { id, start, orig } => {
                let (dx, dy) = delta(start, zooms(self, id).1);
                self.resize_client(id, orig.w + dx as i32, orig.h + dy as i32)
            }
            Drag::TileResize { region, start, ratio0, zoom } => {
                self.tile_resize_to(region, ratio0, (pos.x - start.x) / zoom.max(1e-6));
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
        // A tile is lifted out of its slot; where it is dropped decides what happens (tile_drop).
        self.drag = Some(Drag::Move { id: c.id, start: self.cursor, orig: c.pin.unwrap_or(c.canvas) });
        self.drag_last = self.cursor;
    }

    pub fn begin_resize(&mut self, i: usize) {
        let c = &self.clients[i];
        if c.fullscreen {
            return;
        }
        let (id, orig) = (c.id, c.size());
        let zoom = self.place_at_rest(&self.clients[i]).zoom;
        // A tile moves the split of its layout; anything else changes its own size.
        self.drag = Some(match self.tile_begin_resize(id) {
            Some((region, ratio0)) => Drag::TileResize { region, start: self.cursor, ratio0, zoom },
            None => Drag::Resize { id, start: self.cursor, orig },
        });
        self.drag_last = self.cursor;
    }

    pub fn pointer_button(&mut self, button: u32, state: ButtonState, time: u32) {
        let serial = SERIAL_COUNTER.next_serial();
        self.buttons_down.retain(|b| *b != button);
        if state == ButtonState::Pressed {
            self.buttons_down.push(button);
        }
        // A session lock takes the button: it goes to the lock surface, and nothing else happens.
        if self.is_locked() {
            let ptr = self.pointer();
            ptr.button(self, &ButtonEvent { button, state, serial, time });
            ptr.frame(self);
            return;
        }
        // A grab (a popup menu, a drag-and-drop) takes the button: a press outside a grabbed popup
        // dismisses it, and is not also a focus change, a drag or a pan on top of that.
        // The exception is the release that ends a drag: a client-requested move or resize runs under the
        // client's own implicit grab (its press), and it must still end the drag below.
        let ends_drag = state == ButtonState::Released && self.drag.is_some();
        if self.pointer().is_grabbed() && !ends_drag {
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
            match bind {
                // Over nothing, a move or resize bind falls through to the click handling below.
                // Focusing raises the window, which reorders `clients`: find it again by id.
                Some(Action::DragMove) => {
                    if let Some(id) = under.map(|i| self.clients[i].id) {
                        self.focus_client(Some(id));
                        if let Some(i) = self.client_index(id) {
                            self.begin_move(i);
                        }
                        return;
                    }
                }
                Some(Action::DragResize) => {
                    if let Some(id) = under.map(|i| self.clients[i].id) {
                        self.focus_client(Some(id));
                        if let Some(i) = self.client_index(id) {
                            self.begin_resize(i);
                        }
                        return;
                    }
                }
                Some(Action::DragPan) => {
                    self.drag = Some(Drag::Pan);
                    self.drag_last = self.cursor;
                    return;
                }
                // Any other bind runs; the press is ours, so the client never sees it or its release.
                Some(other) => {
                    self.run_action(&other);
                    self.consumed_press = true;
                    return;
                }
                None => {}
            }
            match (&hit, under) {
                // Dragging empty canvas pans it; a wallpaper (background layer) is empty canvas too.
                (None | Some(Under::Layer { background: true, .. }), None) if button == BTN_LEFT => {
                    self.drag = Some(Drag::Pan);
                    self.drag_last = self.cursor;
                    return;
                }
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
                _ => {}
            }
        } else if let Some(drag) = self.drag.take() {
            // A tile that was carried goes where it was dropped.
            if let Drag::Move { id, .. } = drag {
                self.tile_drop(id);
            }
            // The alignment lines go with the drag.
            if !std::mem::take(&mut self.guides.list).is_empty() {
                self.deco_commit.increment();
                self.damage();
            }
            // The press that started a bind drag never reached a client, so neither does its release.
            // A client-requested drag did get its press, so it gets the release as well.
            self.refresh_pointer();
            if !std::mem::take(&mut self.drag_from_client) {
                return;
            }
        } else if std::mem::take(&mut self.consumed_press) {
            // The press went to a bind, so its release does not reach the client either.
            return;
        }
        let ptr = self.pointer();
        ptr.button(self, &ButtonEvent { button, state, serial, time });
        ptr.frame(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_ways_out_run_while_locked_or_inhibited() {
        let (vt, quit, esc) = (Action::SwitchVt(2), Action::Quit, Action::ToggleShortcutsInhibit);
        assert!(bind_runs(&quit, false, false));
        assert!(!bind_runs(&quit, true, false), "no binds on a locked screen");
        assert!(!bind_runs(&quit, false, true), "nor while a client holds the shortcuts");
        assert!(bind_runs(&vt, true, false), "but you can always leave for another VT");
        assert!(bind_runs(&vt, false, true));
        assert!(bind_runs(&esc, false, true), "and take the shortcuts back");
        assert!(!bind_runs(&esc, true, true), "nothing else unlocks a lock");
    }
}
