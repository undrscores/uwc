//! wlr-screencopy-unstable-v1: pictures of the output, for screenshot tools (grim), screen
//! recorders and shells (Quickshell's monitor ScreencopyView). The whole output or a region of it,
//! with or without the pointer, into an shm buffer, through the same capture core as window
//! previews. `copy_with_damage` waits for the next frame the output actually draws, so a recorder
//! gets one picture per change instead of spinning.

use std::sync::Mutex;

use smithay::{
    reexports::{
        wayland_protocols_wlr::screencopy::v1::server::{
            zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
            zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
        },
        wayland_server::{
            backend::ClientId, protocol::wl_buffer::WlBuffer, Client, DataInit, Dispatch, DisplayHandle,
            GlobalDispatch, New, Resource,
        },
    },
    utils::{Physical, Rectangle, Size},
};

use crate::{capture, state::Uwc};

pub struct FrameData {
    /// Which monitor is captured.
    mon: usize,
    /// The part of the output, physical px.
    region: Rectangle<i32, Physical>,
    cursor: bool,
    used: Mutex<bool>,
}

struct Pending {
    frame: ZwlrScreencopyFrameV1,
    buffer: WlBuffer,
    mon: usize,
    region: Rectangle<i32, Physical>,
    cursor: bool,
    /// Answered once more frames than this have been drawn.
    after: u64,
}

#[derive(Default)]
pub struct Screencopy {
    pending: Vec<Pending>,
}

impl Screencopy {
    pub fn new(dh: &DisplayHandle) -> Screencopy {
        dh.create_global::<Uwc, ZwlrScreencopyManagerV1, ()>(3, ());
        Screencopy::default()
    }
}

impl Uwc {
    /// A monitor's size in physical px.
    pub fn output_pixels(&self, m: usize) -> Size<i32, Physical> {
        let (w, h) = self.monitors.get(m).map(|x| (x.size.w as f64, x.size.h as f64)).unwrap_or((1280.0, 720.0));
        let s = self.output_scale_of(m);
        ((w * s).round() as i32, (h * s).round() as i32).into()
    }

    /// Draw monitor `mon` (the part `region`) into the shm `buffer`.
    pub fn capture_output(
        &mut self,
        mon: usize,
        region: Rectangle<i32, Physical>,
        cursor: bool,
        buffer: &WlBuffer,
    ) -> Result<(), String> {
        let size = self.output_pixels(mon);
        let scale = self.output_scale_of(mon);
        // The root color is the real output's clear color, not an element: the picture needs it too.
        let clear = self.config.root_color;
        let res = self.with_renderer(|state, r| {
            let elements = state.scene_elements(r, mon, cursor);
            capture::render_region_to_shm(r, &elements, size, region, scale, clear, buffer)
        });
        res.unwrap_or_else(|| Err("this backend cannot render (headless)".into()))
    }

    fn screencopy_now(
        &mut self,
        frame: &ZwlrScreencopyFrameV1,
        buffer: &WlBuffer,
        mon: usize,
        region: Rectangle<i32, Physical>,
        cursor: bool,
        damage: bool,
    ) {
        let res = self.capture_output(mon, region, cursor, buffer);
        match res {
            Ok(()) => {
                frame.flags(zwlr_screencopy_frame_v1::Flags::empty());
                if damage && frame.version() >= 2 {
                    frame.damage(0, 0, region.size.w as u32, region.size.h as u32);
                }
                let (secs, nsec) = capture::now();
                frame.ready((secs >> 32) as u32, secs as u32, nsec);
            }
            Err(e) => {
                tracing::debug!("screencopy failed: {e}");
                frame.failed();
            }
        }
    }

    /// Once per event-loop turn: answer `copy_with_damage` requests once a new frame was drawn.
    pub fn screencopy_flush(&mut self) {
        if self.screencopy.pending.is_empty() {
            return;
        }
        let drawn = self.frames_drawn;
        let (now, rest): (Vec<Pending>, Vec<Pending>) =
            std::mem::take(&mut self.screencopy.pending).into_iter().partition(|p| drawn > p.after);
        self.screencopy.pending = rest;
        for p in now {
            self.screencopy_now(&p.frame, &p.buffer, p.mon, p.region, p.cursor, true);
        }
    }
}

impl GlobalDispatch<ZwlrScreencopyManagerV1, ()> for Uwc {
    fn bind(
        _: &mut Uwc,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ZwlrScreencopyManagerV1>,
        _: &(),
        init: &mut DataInit<'_, Uwc>,
    ) {
        init.init(resource, ());
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for Uwc {
    fn request(
        state: &mut Uwc,
        _: &Client,
        _: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Uwc>,
    ) {
        use smithay::output::Output as SOutput;
        use zwlr_screencopy_manager_v1::Request as R;
        // The output the request names; one without one is the pointer's monitor.
        let named = |o: &smithay::reexports::wayland_server::protocol::wl_output::WlOutput| {
            SOutput::from_resource(o).and_then(|o| state.mon_by_output(&o))
        };
        let mon = match &request {
            R::CaptureOutput { output, .. } | R::CaptureOutputRegion { output, .. } => {
                named(output).unwrap_or(state.cur)
            }
            _ => return,
        };
        let full = Rectangle::from_size(state.output_pixels(mon));
        let (frame, cursor, region, valid) = match request {
            R::CaptureOutput { frame, overlay_cursor, .. } => (frame, overlay_cursor != 0, full, true),
            R::CaptureOutputRegion { frame, overlay_cursor, x, y, width, height, .. } => {
                // Logical px of the output, to physical, kept inside it.
                let s = state.output_scale_of(mon);
                let r = Rectangle::<f64, Physical>::new(
                    (x as f64 * s, y as f64 * s).into(),
                    (width as f64 * s, height as f64 * s).into(),
                )
                .to_i32_round();
                let r = r.intersection(full).unwrap_or_default();
                (frame, overlay_cursor != 0, r, !r.is_empty())
            }
            _ => return,
        };
        let frame = init.init(frame, FrameData { mon, region, cursor, used: Mutex::new(false) });
        // Nothing on screen can be captured while the session is locked.
        if state.is_locked() || !valid {
            frame.failed();
            return;
        }
        let (w, h) = (region.size.w as u32, region.size.h as u32);
        frame.buffer(capture::FORMAT, w, h, w * 4);
        if frame.version() >= 3 {
            frame.buffer_done();
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, FrameData> for Uwc {
    fn request(
        state: &mut Uwc,
        _: &Client,
        frame: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        data: &FrameData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Uwc>,
    ) {
        let (buffer, with_damage) = match request {
            zwlr_screencopy_frame_v1::Request::Copy { buffer } => (buffer, false),
            zwlr_screencopy_frame_v1::Request::CopyWithDamage { buffer } => (buffer, true),
            _ => return,
        };
        let mut used = data.used.lock().unwrap();
        if *used {
            frame.post_error(zwlr_screencopy_frame_v1::Error::AlreadyUsed, "copy was already requested");
            return;
        }
        *used = true;
        drop(used);
        if let Err(e) = capture::check_buffer(&buffer, data.region.size) {
            frame.post_error(zwlr_screencopy_frame_v1::Error::InvalidBuffer, e);
            return;
        }
        if with_damage {
            let after = state.frames_drawn;
            state.screencopy.pending.push(Pending {
                frame: frame.clone(),
                buffer,
                mon: data.mon,
                region: data.region,
                cursor: data.cursor,
                after,
            });
        } else {
            state.screencopy_now(frame, &buffer, data.mon, data.region, data.cursor, false);
        }
    }

    fn destroyed(state: &mut Uwc, _: ClientId, frame: &ZwlrScreencopyFrameV1, _: &FrameData) {
        state.screencopy.pending.retain(|p| &p.frame != frame);
    }
}
