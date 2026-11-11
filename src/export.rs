//! hyprland-toplevel-export-v1: window previews for shells. This is what Quickshell's ScreencopyView
//! uses to show a live picture of a window (given a ToplevelManager toplevel, i.e. a
//! wlr-foreign-toplevel handle). The protocol also has a variant taking a plain number, which uwc
//! reads as its own window id (the `id` in the IPC state).
//!
//! A frame offers one ARGB8888 shm buffer of the window at the output scale. `copy` draws the window
//! into it. Without `ignore_damage` the copy waits for the window's next commit (so a live preview
//! redraws at the app's pace instead of spinning); with it, it happens at once.

use std::sync::Mutex;

use smithay::reexports::wayland_server::{
    backend::ClientId, protocol::wl_buffer::WlBuffer, Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New,
    Resource,
};

use crate::{
    capture,
    protocols::hyprland_toplevel_export::server::{
        hyprland_toplevel_export_frame_v1::{self, HyprlandToplevelExportFrameV1},
        hyprland_toplevel_export_manager_v1::{self, HyprlandToplevelExportManagerV1},
    },
    state::Uwc,
};

/// A frame's window, and whether `copy` was already used (a frame captures once).
pub struct FrameData {
    window: u32,
    used: Mutex<bool>,
}

struct Pending {
    frame: HyprlandToplevelExportFrameV1,
    window: u32,
    buffer: WlBuffer,
}

#[derive(Default)]
pub struct ToplevelExport {
    /// Copies waiting for their window's next commit.
    pending: Vec<Pending>,
    /// The window commit each client last captured, so a copy without new content waits.
    seen: Vec<(ClientId, u32, u64)>,
}

impl ToplevelExport {
    pub fn new(dh: &DisplayHandle) -> ToplevelExport {
        dh.create_global::<Uwc, HyprlandToplevelExportManagerV1, ()>(2, ());
        ToplevelExport::default()
    }
}

impl Uwc {
    fn export_frame(&mut self, frame: HyprlandToplevelExportFrameV1, window: Option<u32>) {
        let Some(size) = window.and_then(|w| self.window_capture_size(w)) else {
            frame.failed();
            return;
        };
        frame.buffer(capture::FORMAT, size.w as u32, size.h as u32, size.w as u32 * 4);
        if frame.version() >= 2 {
            frame.buffer_done();
        }
    }

    fn export_copy(
        &mut self,
        frame: HyprlandToplevelExportFrameV1,
        window: u32,
        buffer: WlBuffer,
        ignore_damage: bool,
    ) {
        let Ok(client) = self.display_handle.get_client(frame.id()) else { return };
        let commits = self.client_index(window).map(|i| self.clients[i].commits);
        let Some(commits) = commits else {
            frame.failed();
            return;
        };
        let seen = self.export.seen.iter().find(|(c, w, _)| *c == client.id() && *w == window).map(|s| s.2);
        if ignore_damage || seen != Some(commits) {
            self.export_now(&frame, window, &buffer, &client.id());
        } else {
            self.export.pending.push(Pending { frame, window, buffer });
        }
    }

    fn export_now(&mut self, frame: &HyprlandToplevelExportFrameV1, window: u32, buffer: &WlBuffer, client: &ClientId) {
        match self.capture_window(window, buffer) {
            Ok(()) => {
                let size = self.window_capture_size(window).unwrap_or_default();
                frame.flags(hyprland_toplevel_export_frame_v1::Flags::empty());
                frame.damage(0, 0, size.w as u32, size.h as u32);
                let (secs, nsec) = capture::now();
                frame.ready((secs >> 32) as u32, secs as u32, nsec);
                let commits = self.client_index(window).map(|i| self.clients[i].commits).unwrap_or(0);
                self.export.seen.retain(|(c, w, _)| !(c == client && *w == window));
                self.export.seen.push((client.clone(), window, commits));
            }
            Err(e) => {
                tracing::debug!("window capture failed: {e}");
                frame.failed();
            }
        }
    }

    /// Window `id` committed: answer the copies that were waiting for new content.
    pub fn export_on_commit(&mut self, id: u32) {
        if !self.export.pending.iter().any(|p| p.window == id) {
            return;
        }
        let (now, rest): (Vec<Pending>, Vec<Pending>) =
            std::mem::take(&mut self.export.pending).into_iter().partition(|p| p.window == id);
        self.export.pending = rest;
        for p in now {
            if let Ok(client) = self.display_handle.get_client(p.frame.id()) {
                self.export_now(&p.frame, p.window, &p.buffer, &client.id());
            }
        }
    }

    /// Window `id` is gone: its waiting copies fail.
    pub fn export_on_close(&mut self, id: u32) {
        self.export.pending.retain(|p| {
            if p.window == id {
                p.frame.failed();
            }
            p.window != id
        });
        self.export.seen.retain(|(_, w, _)| *w != id);
    }
}

impl GlobalDispatch<HyprlandToplevelExportManagerV1, ()> for Uwc {
    fn can_view(client: smithay::reexports::wayland_server::Client, _: &()) -> bool {
        !crate::sandbox::is_sandboxed(&client)
    }

    fn bind(
        _: &mut Uwc,
        _: &DisplayHandle,
        _: &Client,
        resource: New<HyprlandToplevelExportManagerV1>,
        _: &(),
        init: &mut DataInit<'_, Uwc>,
    ) {
        init.init(resource, ());
    }
}

impl Dispatch<HyprlandToplevelExportManagerV1, ()> for Uwc {
    fn request(
        state: &mut Uwc,
        _: &Client,
        _: &HyprlandToplevelExportManagerV1,
        request: hyprland_toplevel_export_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Uwc>,
    ) {
        use hyprland_toplevel_export_manager_v1::Request as R;
        // No window previews while the session is locked: nothing must be captured.
        if state.is_locked() {
            if let R::CaptureToplevel { frame, .. } | R::CaptureToplevelWithWlrToplevelHandle { frame, .. } = request {
                let frame = init.init(frame, FrameData { window: 0, used: Mutex::new(true) });
                frame.failed();
            }
            return;
        }
        let (frame, window) = match request {
            // The plain-number form: uwc's own window id.
            R::CaptureToplevel { frame, handle, .. } => (frame, Some(handle)),
            R::CaptureToplevelWithWlrToplevelHandle { frame, handle, .. } => (frame, handle.data::<u32>().copied()),
            _ => return,
        };
        let frame = init.init(frame, FrameData { window: window.unwrap_or(0), used: Mutex::new(false) });
        state.export_frame(frame, window);
    }
}

impl Dispatch<HyprlandToplevelExportFrameV1, FrameData> for Uwc {
    fn request(
        state: &mut Uwc,
        _: &Client,
        frame: &HyprlandToplevelExportFrameV1,
        request: hyprland_toplevel_export_frame_v1::Request,
        data: &FrameData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Uwc>,
    ) {
        if let hyprland_toplevel_export_frame_v1::Request::Copy { buffer, ignore_damage } = request {
            let mut used = data.used.lock().unwrap();
            if *used {
                frame.post_error(hyprland_toplevel_export_frame_v1::Error::AlreadyUsed, "copy was already requested");
                return;
            }
            *used = true;
            drop(used);
            if let Some(size) = state.window_capture_size(data.window) {
                if let Err(e) = capture::check_buffer(&buffer, size) {
                    frame.post_error(hyprland_toplevel_export_frame_v1::Error::InvalidBuffer, e);
                    return;
                }
            }
            state.export_copy(frame.clone(), data.window, buffer, ignore_damage != 0);
        }
    }

    fn destroyed(state: &mut Uwc, _: ClientId, frame: &HyprlandToplevelExportFrameV1, _: &FrameData) {
        state.export.pending.retain(|p| &p.frame != frame);
    }
}
