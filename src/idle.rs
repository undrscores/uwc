//! ext-idle-notify-v1 (idle daemons: swayidle/hypridle lock the screen, run scripts) and
//! zwp-idle-inhibit-v1 (video players, games: keep the screen on). The notifier's timers run in
//! smithay and are reset by `notify_activity` on real input; any inhibitor pauses them all.

use smithay::{
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    wayland::{
        idle_inhibit::IdleInhibitHandler,
        idle_notify::{IdleNotifierHandler, IdleNotifierState},
    },
};

use crate::state::Uwc;

impl IdleNotifierHandler for Uwc {
    fn idle_notifier_state(&mut self) -> &mut IdleNotifierState<Self> {
        &mut self.idle_notifier
    }
}

impl IdleInhibitHandler for Uwc {
    fn inhibit(&mut self, _surface: WlSurface) {
        self.idle_inhibitors += 1;
        self.idle_notifier.set_is_inhibited(self.idle_inhibitors > 0);
    }

    fn uninhibit(&mut self, _surface: WlSurface) {
        self.idle_inhibitors = self.idle_inhibitors.saturating_sub(1);
        self.idle_notifier.set_is_inhibited(self.idle_inhibitors > 0);
    }
}
