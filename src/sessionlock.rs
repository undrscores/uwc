//! ext-session-lock-v1: a lock screen client (swaylock, waylock, Quickshell's lock screen) covers
//! every output and takes all input. While a lock is up nothing else is drawn, captured or
//! interacted with: windows, layers and ghosts are left out of the frame, screencopy and window
//! previews fail, key bindings do not run, and the keyboard and pointer go to the lock surface
//! only. The lock is confirmed (`locked` event) once every output is covered — the screen is
//! already cleared by then, so no client content can leak. A lock client that dies without
//! unlocking leaves the session locked, with a blank screen (the protocol demands it).

use smithay::{
    backend::renderer::{
        element::{surface::render_elements_from_surface_tree, Kind},
        gles::GlesRenderer,
    },
    output::Output,
    reexports::wayland_server::{protocol::wl_output::WlOutput, DisplayHandle},
    utils::Scale,
    wayland::session_lock::{LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker},
};

use crate::{render::UwcElement, state::Uwc};

pub struct SessionLock {
    pub manager: SessionLockManagerState,
    /// The lock in progress: one surface per output, and the confirmation that fires once
    /// every output is covered.
    pub locked: Option<Locked>,
}

pub struct Locked {
    /// The lock surface of each monitor, by monitor index.
    pub surfaces: Vec<(usize, LockSurface)>,
    pub confirmation: Option<SessionLocker>,
}

impl SessionLock {
    pub fn new(dh: &DisplayHandle) -> Self {
        SessionLock { manager: SessionLockManagerState::new::<Uwc, _>(dh, |_| true), locked: None }
    }
}

impl Uwc {
    pub fn is_locked(&self) -> bool {
        self.session_lock.locked.is_some()
    }

    /// The lock surface of monitor `m`, if it is alive.
    pub fn lock_surface_on(&self, m: usize) -> Option<&LockSurface> {
        self.session_lock.locked.as_ref()?.surfaces.iter().find(|(mi, s)| *mi == m && s.alive()).map(|(_, s)| s)
    }

    /// The lock surface of monitor `m` as render elements, at output scale `scale`.
    pub fn lock_surface_elements(&self, renderer: &mut GlesRenderer, m: usize, scale: f64) -> Vec<UwcElement> {
        let Some(s) = self.lock_surface_on(m) else { return vec![] };
        render_elements_from_surface_tree::<_, UwcElement>(
            renderer,
            s.wl_surface(),
            smithay::utils::Point::<i32, smithay::utils::Physical>::from((0, 0)),
            Scale::from(scale),
            1.0,
            Kind::Unspecified,
        )
    }
}

impl SessionLockHandler for Uwc {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.session_lock.manager
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        self.session_lock.locked = Some(Locked { surfaces: Vec::new(), confirmation: Some(confirmation) });
        self.update_keyboard_focus();
        self.damage();
    }

    fn unlock(&mut self) {
        self.session_lock.locked = None;
        self.update_keyboard_focus();
        self.refresh_pointer();
        self.damage();
    }

    fn new_surface(&mut self, surface: LockSurface, output: WlOutput) {
        let Some(m) = Output::from_resource(&output).and_then(|o| self.mon_by_output(&o)) else { return };
        let size = self.monitors.get(m).map(|x| x.size).unwrap_or((1280, 720).into());
        surface.with_pending_state(|s| s.size = Some((size.w as u32, size.h as u32).into()));
        surface.send_configure();
        if let Some(l) = &mut self.session_lock.locked {
            l.surfaces.retain(|(mi, s)| *mi != m && s.alive());
            l.surfaces.push((m, surface.clone()));
            // Confirm once every output is covered: the frame is clear from the lock request on.
            if l.surfaces.len() >= self.monitors.len() {
                if let Some(c) = l.confirmation.take() {
                    c.lock();
                }
            }
        }
        self.update_keyboard_focus();
        self.damage();
    }
}
