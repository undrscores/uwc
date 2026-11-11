//! zwp-text-input-v3 and zwp-input-method-v2 (IME support: fcitx5 and friends). Text input focus
//! follows the keyboard focus (the compositor relays it in `focus_changed`); while an input
//! method holds the keyboard grab, smithay routes every key to it first and it answers through
//! text input (commit, pre-edit) -- uwc's own key bindings do not fire then, which is what a
//! grab means. The method's popups (candidate lists) are drawn on top, placed under the focused
//! window's top-left corner, and take the pointer by their own rectangle.

use smithay::{
    backend::renderer::{
        element::{surface::render_elements_from_surface_tree, Kind},
        gles::GlesRenderer,
    },
    utils::{Logical, Point, Rectangle},
    wayland::input_method::{InputMethodHandler, PopupSurface},
};

use crate::{layer::Under, render::UwcElement, state::Uwc};

impl InputMethodHandler for Uwc {
    fn new_popup(&mut self, surface: PopupSurface) {
        // A candidate list: under the parent's geometry (the focused window).
        if let Some(parent) = surface.get_parent() {
            let geo = self.parent_geometry(&parent.surface);
            surface.set_location(Point::<i32, Logical>::from((geo.loc.x, geo.loc.y + geo.size.h)));
        }
        self.ime_popups.retain(|p| p.wl_surface() != surface.wl_surface());
        self.ime_popups.push(surface);
        self.damage();
    }

    fn dismiss_popup(&mut self, surface: PopupSurface) {
        self.ime_popups.retain(|p| p.wl_surface() != surface.wl_surface());
        self.damage();
    }

    fn popup_repositioned(&mut self, surface: PopupSurface) {
        self.ime_popups.retain(|p| p.wl_surface() != surface.wl_surface());
        self.ime_popups.push(surface);
        self.damage();
    }

    fn parent_geometry(
        &self,
        parent: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) -> Rectangle<i32, Logical> {
        match self.clients.iter().find(|c| c.surface().as_ref() == Some(parent)).map(|c| (self.mon_of(c), c)) {
            Some((m, c)) => {
                let pl = self.place_on(m, c, &crate::state::Fx::REST);
                let size = c.size();
                let o = self.origin_of(m);
                Rectangle::new(
                    Point::<i32, Logical>::from(((o.x + pl.origin.x) as i32, (o.y + pl.origin.y) as i32)),
                    size,
                )
            }
            None => {
                let (w, h) = self.screen_size();
                Rectangle::from_size((w as i32, h as i32).into())
            }
        }
    }
}

impl Uwc {
    /// The IME popups as render elements, on top of everything (global px on monitor `m`).
    pub fn ime_popup_elements(&self, renderer: &mut GlesRenderer, m: usize, scale: f64) -> Vec<UwcElement> {
        let o = self.origin_of(m);
        self.ime_popups
            .iter()
            .filter(|p| p.alive())
            .flat_map(|p| {
                let at = p.location().to_f64() - o;
                render_elements_from_surface_tree::<_, UwcElement>(
                    renderer,
                    p.wl_surface(),
                    Point::<f64, smithay::utils::Physical>::from((at.x * scale, at.y * scale)).to_i32_round(),
                    smithay::utils::Scale::from(scale),
                    1.0,
                    Kind::Unspecified,
                )
            })
            .collect()
    }

    /// The IME popup under a global point, if any (they take the pointer by their rectangle).
    pub fn ime_popup_under(&self, p: Point<f64, Logical>) -> Option<Under> {
        for popup in self.ime_popups.iter().rev().filter(|p| p.alive()) {
            let r = popup.location();
            let size = smithay::wayland::compositor::with_states(popup.wl_surface(), |s| {
                s.cached_state
                    .get::<smithay::wayland::compositor::SurfaceAttributes>()
                    .current()
                    .buffer
                    .as_ref()
                    .and_then(|a| match a {
                        smithay::wayland::compositor::BufferAssignment::NewBuffer(b) => {
                            smithay::backend::renderer::buffer_dimensions(b)
                                .map(|d| d.to_logical(1, smithay::utils::Transform::Normal))
                        }
                        _ => None,
                    })
                    .unwrap_or((1, 1).into())
            });
            let rect = Rectangle::<f64, Logical>::new(r.to_f64(), size.to_f64());
            if rect.contains(p) {
                let surface = popup.wl_surface().clone();
                return Some(Under::Layer {
                    root: surface.clone(),
                    surface,
                    origin: r.to_f64(),
                    interactive: false,
                    background: false,
                });
            }
        }
        None
    }
}
