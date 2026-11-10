//! xdg-activation-v1: apps ask for focus. A launcher or another app (an "open link" click) hands
//! the target an activation token; a fresh one focuses the window (revealing it on the canvas),
//! a stale one is a background app asking out of nowhere and is ignored. A window that cannot be
//! focused (on another tag, minimized) is flagged urgent instead: the deco's `urgent` colors,
//! the `urgent` rule matcher and ext-workspace's urgent tags pick that up.

use smithay::{
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    wayland::xdg_activation::{XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData},
};

use crate::state::Uwc;

/// Older than this, a token does not focus anything (the same freshness window most compositors
/// that check use).
const TOKEN_SECS: u64 = 10;

impl XdgActivationHandler for Uwc {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.activation_state
    }

    fn request_activation(&mut self, _token: XdgActivationToken, data: XdgActivationTokenData, surface: WlSurface) {
        if data.timestamp.elapsed().as_secs() > TOKEN_SECS {
            return;
        }
        let Some(i) = self.client_by_surface(&surface) else { return };
        let id = self.clients[i].id;
        if self.visible(&self.clients[i]) {
            self.focus_and_reveal(id);
        } else {
            self.clients[i].urgent = true;
            self.deco_commit.increment();
            self.ipc.dirty = true;
            self.damage();
        }
    }
}
