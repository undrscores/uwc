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
    /// The part of the output, physical px.
    region: Rectangle<i32, Physical>,
    cursor: bool,
    used: Mutex<bool>,
}

struct Pending {
    frame: ZwlrScreencopyFrameV1,
    buffer: WlBuffer,
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
    /// The output's size in physical px.
    fn output_pixels(&self) -> Size<i32, Physical> {
        let (w, h) = self.screen_size();
        let s = self.output_scale();
        ((w * s).round() as i32, (h * s).round() as i32).into()
    }

    fn screencopy_now(
        &mut self,
        frame: &ZwlrScreencopyFrameV1,
        buffer: &WlBuffer,
        region: Rectangle<i32, Physical>,
        cursor: bool,
        damage: bool,
    ) {
        let size = self.output_pixels();
        let scale = self.output_scale();
        // The root color is the real output's clear color, not an element: the picture needs it too.
        let clear = self.config.root_color;
        let res = self.with_renderer(|state, r| {
            let elements = state.scene_elements(r, scale, cursor);
            capture::render_region_to_shm(r, &elements, size, region, scale, clear, buffer)
        });
        match res.unwrap_or_else(|| Err("this backend cannot render (headless)".into())) {
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
            self.screencopy_now(&p.frame, &p.buffer, p.region, p.cursor, true);
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
        use zwlr_screencopy_manager_v1::Request as R;
        let full = Rectangle::from_size(state.output_pixels());
        let ours = |o: &smithay::reexports::wayland_server::protocol::wl_output::WlOutput| {
            state.mon.as_ref().is_some_and(|m| m.output.owns(o))
        };
        let (frame, cursor, region, valid) = match request {
            R::CaptureOutput { frame, overlay_cursor, output } => (frame, overlay_cursor != 0, full, ours(&output)),
            R::CaptureOutputRegion { frame, overlay_cursor, output, x, y, width, height } => {
                // Logical px of the output, to physical, kept inside it.
                let s = state.output_scale();
                let r = Rectangle::<f64, Physical>::new(
                    (x as f64 * s, y as f64 * s).into(),
                    (width as f64 * s, height as f64 * s).into(),
                )
                .to_i32_round();
                let r = r.intersection(full).unwrap_or_default();
                (frame, overlay_cursor != 0, r, ours(&output) && !r.is_empty())
            }
            _ => return,
        };
        let frame = init.init(frame, FrameData { region, cursor, used: Mutex::new(false) });
        if !valid {
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
                region: data.region,
                cursor: data.cursor,
                after,
            });
        } else {
            state.screencopy_now(frame, &buffer, data.region, data.cursor, false);
        }
    }

    fn destroyed(state: &mut Uwc, _: ClientId, frame: &ZwlrScreencopyFrameV1, _: &FrameData) {
        state.screencopy.pending.retain(|p| &p.frame != frame);
    }
}
