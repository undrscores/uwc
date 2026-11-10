//! Sharp zoom. Each window is told the scale it is shown at (output scale x zoom) through
//! wp_fractional_scale_v1 (and the integer wl_surface.preferred_buffer_scale), so a window you zoom
//! into re-renders with more pixels instead of being stretched. Smithay draws a viewported buffer at
//! its logical size, so the extra resolution simply comes out sharp under the zoom's rescale.
//!
//! Clients are not asked on every step of a zoom: the zoom has to stay put for `zoom-settle-ms`
//! first. `zoom-render-scale-min`/`-max` bound the zoom factor clients render for (rendering at 8x
//! for zoom 8 would cost a lot of memory for little gain).

use std::time::Duration;

use smithay::{
    delegate_fractional_scale, delegate_viewporter,
    desktop::{utils::with_surfaces_surface_tree, PopupManager},
    reexports::{
        calloop::timer::{TimeoutAction, Timer},
        wayland_server::protocol::wl_surface::WlSurface,
    },
    utils::Transform,
    wayland::{
        compositor::send_surface_state,
        fractional_scale::{with_fractional_scale, FractionalScaleHandler},
    },
};

use crate::state::{Client, Uwc};

/// Tell every surface of a tree its preferred scale.
fn apply(surface: &WlSurface, scale: f64) {
    with_surfaces_surface_tree(surface, |s, data| {
        with_fractional_scale(data, |fs| fs.set_preferred_scale(scale));
        send_surface_state(s, data, scale.ceil().max(1.0) as i32, Transform::Normal);
    });
    for (popup, _) in PopupManager::popups_for_surface(surface) {
        with_surfaces_surface_tree(popup.wl_surface(), |s, data| {
            with_fractional_scale(data, |fs| fs.set_preferred_scale(scale));
            send_surface_state(s, data, scale.ceil().max(1.0) as i32, Transform::Normal);
        });
    }
}

impl Uwc {
    pub fn output_scale(&self) -> f64 {
        self.output_scale_of(self.cur)
    }

    pub fn output_scale_of(&self, m: usize) -> f64 {
        self.monitors.get(m).map(|x| x.output.current_scale().fractional_scale()).unwrap_or(1.0)
    }

    /// The scale a window should render at: its monitor's output scale x its settled zoom there
    /// (bounded), in the protocol's 1/120 steps.
    pub fn render_scale(&self, c: &Client) -> f64 {
        let o = &self.config;
        let m = self.mon_of(c);
        let zoom = if c.fullscreen {
            1.0
        } else if c.pin.is_some() {
            c.scale
        } else {
            self.monitors.get(m).map(|x| x.settled_zoom).unwrap_or(1.0) * c.scale
        };
        let s = self.output_scale_of(m) * zoom.clamp(o.zoom_render_scale_min, o.zoom_render_scale_max);
        (s * 120.0).round().max(1.0) / 120.0
    }

    /// Once per event-loop turn: notice zoom changes (and let them settle), then tell windows whose
    /// render scale changed.
    pub fn sync_scales(&mut self) {
        // Each monitor settles its own zoom (only the last change of a burst counts).
        for m in 0..self.monitors.len() {
            let zoom = self.viewport_of(m).zoom;
            if zoom != self.monitors[m].seen_zoom {
                self.monitors[m].seen_zoom = zoom;
                self.scale_gen += 1;
                let generation = self.scale_gen;
                let wait = Duration::from_millis(self.config.zoom_settle_ms as u64);
                let _ = self.loop_handle.insert_source(Timer::from_duration(wait), move |_, _, state| {
                    if state.scale_gen == generation {
                        if let Some(mon) = state.monitors.get_mut(m) {
                            mon.settled_zoom = mon.seen_zoom;
                        }
                    }
                    TimeoutAction::Drop
                });
            }
        }
        for i in 0..self.clients.len() {
            if !self.clients[i].mapped && self.clients[i].sent_scale.is_none() {
                // Before the first commit the output scale is right; set it once so the first
                // buffer already has it.
                let s = self.output_scale_of(self.mon_of(&self.clients[i]));
                apply(self.clients[i].surface(), s);
                self.clients[i].sent_scale = Some(s);
                continue;
            }
            let s = self.render_scale(&self.clients[i]);
            if self.clients[i].sent_scale != Some(s) {
                apply(self.clients[i].surface(), s);
                self.clients[i].sent_scale = Some(s);
            }
        }
        // Layer surfaces are never zoomed: their output's scale.
        for m in &self.monitors {
            let s = self.output_scale_of(self.mon_by_output(&m.output).unwrap_or(0));
            for l in smithay::desktop::layer_map_for_output(&m.output).layers() {
                apply(l.wl_surface(), s);
            }
        }
    }
}

impl FractionalScaleHandler for Uwc {
    // A client asked for fractional scale on a surface: make sure it hears the current value.
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        let mut root = surface.clone();
        while let Some(p) = smithay::wayland::compositor::get_parent(&root) {
            root = p;
        }
        if let Some(i) = self.client_by_surface(&root) {
            self.clients[i].sent_scale = None;
        }
    }
}

delegate_fractional_scale!(Uwc);
delegate_viewporter!(Uwc);
