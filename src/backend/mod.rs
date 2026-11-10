//! Backends: where outputs and input come from and where frames go.
//!
//! - `udev`: a real session on a TTY (DRM/KMS, GBM, libinput, libseat).
//! - `winit`: uwc as a window inside another session, for development.
//! - `headless`: no output at all; what the integration tests run against.
//!
//! The state holds the backend (`Uwc.backend`) so actions and protocol handlers can reach it
//! (VT switching, dmabuf import). Code that needs the state and the backend's renderer at the same
//! time takes the backend out with `std::mem::take` and puts it back.

pub mod headless;
pub mod udev;
pub mod winit;

use std::time::Duration;

use smithay::{backend::allocator::dmabuf::Dmabuf, output::Output};

use crate::{config::Config, state::Uwc};

#[derive(Default)]
pub enum Backend {
    /// Before a backend is set up, or while it is taken out of the state.
    #[default]
    None,
    Headless,
    Winit(Box<winit::WinitData>),
    Udev(Box<udev::UdevData>),
}

impl Backend {
    pub fn name(&self) -> &'static str {
        match self {
            Backend::None => "none",
            Backend::Headless => "headless",
            Backend::Winit(_) => "winit",
            Backend::Udev(_) => "udev",
        }
    }

    pub fn switch_vt(&mut self, vt: i32) {
        if let Backend::Udev(u) = self {
            u.switch_vt(vt);
        }
    }

    /// Check that a client's dmabuf can be used for rendering.
    pub fn import_dmabuf(&mut self, dmabuf: &Dmabuf) -> bool {
        use smithay::backend::renderer::ImportDma;
        match self {
            Backend::Winit(w) => w.backend.renderer().import_dmabuf(dmabuf, None).is_ok(),
            Backend::Udev(u) => u.renderer_mut().is_some_and(|r| r.import_dmabuf(dmabuf, None).is_ok()),
            _ => false,
        }
    }

    pub fn config_changed(&mut self, config: &Config) {
        if let Backend::Udev(u) = self {
            u.config_changed(config);
        }
    }
}

/// Tell every visible window a frame was drawn so clients can produce the next one.
pub fn send_frame_callbacks(state: &Uwc, output: &Output) {
    let time = state.start_time.elapsed();
    for c in state.clients.iter().filter(|c| state.visible(c)) {
        c.window.send_frame(output, time, Some(Duration::ZERO), |_, _| Some(output.clone()));
        for (popup, _) in smithay::desktop::PopupManager::popups_for_surface(c.surface()) {
            smithay::desktop::utils::send_frames_surface_tree(
                popup.wl_surface(),
                output,
                time,
                Some(Duration::ZERO),
                |_, _| Some(output.clone()),
            );
        }
    }
    if let smithay::input::pointer::CursorImageStatus::Surface(s) = &state.cursor_status {
        smithay::desktop::utils::send_frames_surface_tree(s, output, time, Some(Duration::ZERO), |_, _| {
            Some(output.clone())
        });
    }
}
