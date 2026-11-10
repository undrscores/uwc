//! Backends: where the compositor gets outputs and input, and where frames go.
//! `winit` runs uwc as a window inside another compositor (development); `headless` has no
//! output at all and is what the integration tests run against. A DRM/libinput backend for
//! real sessions is the next one to add.

pub mod headless;
pub mod winit;

use std::time::Duration;

use smithay::output::Output;

use crate::state::Uwc;

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
}
