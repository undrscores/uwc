//! wlr-virtual-pointer-unstable-v1: a client that acts as a mouse (wayvnc, wlrctl, waynergy, input
//! sharing, automation). Its events take the same paths as a real pointer's: motion moves uwc's
//! cursor (so sloppy focus, drags, constraints and key binds see it), buttons run the click
//! handling, and axis events go to the client under the pointer.

use std::sync::Mutex;

use smithay::{
    backend::input::{Axis, AxisSource, ButtonState},
    input::pointer::AxisFrame,
    reexports::{
        wayland_protocols_wlr::virtual_pointer::v1::server::{
            zwlr_virtual_pointer_manager_v1::{self, ZwlrVirtualPointerManagerV1},
            zwlr_virtual_pointer_v1::{self, ZwlrVirtualPointerV1},
        },
        wayland_server::{
            protocol::wl_pointer, Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum,
        },
    },
};

use crate::state::Uwc;

pub fn create_global(dh: &DisplayHandle) {
    dh.create_global::<Uwc, ZwlrVirtualPointerManagerV1, ()>(2, ());
}

/// An axis frame being built between `frame` requests.
#[derive(Default)]
pub struct Frame {
    time: u32,
    source: Option<AxisSource>,
    /// Per axis (horizontal, vertical): the value, discrete steps, and a stop.
    value: [f64; 2],
    discrete: [i32; 2],
    stop: [bool; 2],
    any: bool,
}

pub struct PointerData {
    frame: Mutex<Frame>,
}

impl GlobalDispatch<ZwlrVirtualPointerManagerV1, ()> for Uwc {
    fn can_view(client: smithay::reexports::wayland_server::Client, _: &()) -> bool {
        !crate::sandbox::is_sandboxed(&client)
    }

    fn bind(
        _: &mut Uwc,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ZwlrVirtualPointerManagerV1>,
        _: &(),
        init: &mut DataInit<'_, Uwc>,
    ) {
        init.init(resource, ());
    }
}

impl Dispatch<ZwlrVirtualPointerManagerV1, ()> for Uwc {
    fn request(
        _: &mut Uwc,
        _: &Client,
        _: &ZwlrVirtualPointerManagerV1,
        request: zwlr_virtual_pointer_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Uwc>,
    ) {
        use zwlr_virtual_pointer_manager_v1::Request as R;
        // uwc has one seat; the output, if given, only matters for absolute motion and is not
        // used: positions map onto the whole (tiled) output space.
        if let R::CreateVirtualPointer { id, .. } | R::CreateVirtualPointerWithOutput { id, .. } = request {
            init.init(id, PointerData { frame: Mutex::new(Frame::default()) });
        }
    }
}

fn axis_of(a: WEnum<wl_pointer::Axis>) -> Option<(usize, Axis)> {
    match a.into_result().ok()? {
        wl_pointer::Axis::HorizontalScroll => Some((0, Axis::Horizontal)),
        wl_pointer::Axis::VerticalScroll => Some((1, Axis::Vertical)),
        _ => None,
    }
}

impl Dispatch<ZwlrVirtualPointerV1, PointerData> for Uwc {
    fn request(
        state: &mut Uwc,
        _: &Client,
        res: &ZwlrVirtualPointerV1,
        request: zwlr_virtual_pointer_v1::Request,
        data: &PointerData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Uwc>,
    ) {
        use zwlr_virtual_pointer_v1::Request as R;
        let mut f = data.frame.lock().unwrap();
        match request {
            R::Motion { time, dx, dy } => {
                drop(f);
                state.idle_notifier.notify_activity(&state.seat);
                state.pointer_moved_by(dx, dy, time);
            }
            R::MotionAbsolute { time, x, y, x_extent, y_extent } => {
                drop(f);
                if x_extent > 0 && y_extent > 0 {
                    state.idle_notifier.notify_activity(&state.seat);
                    let (w, h) = state.global_size();
                    let p =
                        (x.min(x_extent) as f64 / x_extent as f64 * w, y.min(y_extent) as f64 / y_extent as f64 * h);
                    state.pointer_moved_to(p.into(), time);
                }
            }
            R::Button { time, button, state: s } => {
                drop(f);
                let s = match s.into_result() {
                    Ok(wl_pointer::ButtonState::Pressed) => ButtonState::Pressed,
                    _ => ButtonState::Released,
                };
                state.idle_notifier.notify_activity(&state.seat);
                state.pointer_button(button, s, time);
            }
            R::Axis { time, axis, value } => match axis_of(axis) {
                Some((i, _)) => {
                    f.time = time;
                    f.value[i] = value;
                    f.any = true;
                }
                None => res.post_error(zwlr_virtual_pointer_v1::Error::InvalidAxis, "unknown axis"),
            },
            R::AxisDiscrete { time, axis, value, discrete } => match axis_of(axis) {
                Some((i, _)) => {
                    f.time = time;
                    f.value[i] = value;
                    f.discrete[i] = discrete;
                    f.any = true;
                }
                None => res.post_error(zwlr_virtual_pointer_v1::Error::InvalidAxis, "unknown axis"),
            },
            R::AxisStop { time, axis } => match axis_of(axis) {
                Some((i, _)) => {
                    f.time = time;
                    f.stop[i] = true;
                    f.any = true;
                }
                None => res.post_error(zwlr_virtual_pointer_v1::Error::InvalidAxis, "unknown axis"),
            },
            R::AxisSource { axis_source } => match axis_source.into_result() {
                Ok(wl_pointer::AxisSource::Wheel) => f.source = Some(AxisSource::Wheel),
                Ok(wl_pointer::AxisSource::Finger) => f.source = Some(AxisSource::Finger),
                Ok(wl_pointer::AxisSource::Continuous) => f.source = Some(AxisSource::Continuous),
                Ok(wl_pointer::AxisSource::WheelTilt) => f.source = Some(AxisSource::WheelTilt),
                _ => res.post_error(zwlr_virtual_pointer_v1::Error::InvalidAxisSource, "unknown axis source"),
            },
            R::Frame => {
                let done = std::mem::take(&mut *f);
                drop(f);
                if done.any {
                    state.virtual_axis(done);
                }
            }
            _ => {}
        }
    }
}

impl Uwc {
    fn virtual_axis(&mut self, f: Frame) {
        if self.is_locked() {
            return;
        }
        let mut frame = AxisFrame::new(f.time);
        if let Some(s) = f.source {
            frame = frame.source(s);
        }
        for (i, axis) in [(0, Axis::Horizontal), (1, Axis::Vertical)] {
            if f.value[i] != 0.0 {
                frame = frame.value(axis, f.value[i]);
                if f.discrete[i] != 0 {
                    frame = frame.v120(axis, f.discrete[i] * 120);
                }
            }
            if f.stop[i] {
                frame = frame.stop(axis);
            }
        }
        let ptr = self.pointer();
        ptr.axis(self, frame);
        ptr.frame(self);
    }
}
