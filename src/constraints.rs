//! zwp-pointer-constraints-v1 and zwp-relative-pointer-manager-v1 (games, 3D apps, remote-viewer
//! panning): a confined pointer cannot leave the window (or the region it names), a locked one
//! keeps the cursor still and hands the motion to the client as relative pointer events. A
//! constraint activates when the pointer is on the surface it was made on, and deactivates when
//! it leaves (smithay does that on pointer focus loss; a oneshot constraint is then destroyed,
//! a persistent one re-activates on the next entry). While a lock is active uwc's own cursor is
//! not drawn.

use smithay::{
    input::pointer::{PointerHandle, RelativeMotionEvent},
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Rectangle, Size},
    wayland::{
        compositor::RectangleKind,
        pointer_constraints::{PointerConstraint, PointerConstraintsHandler},
    },
};

use crate::{
    layer::Under,
    state::{Client, Fx, Uwc},
};

/// The constraint on the surface under the pointer, while it is active.
enum Active {
    Locked,
    Confined(Vec<Rectangle<f64, Logical>>),
}

impl Uwc {
    /// The active constraint on the surface under the pointer, if any.
    fn active_constraint(&self) -> Option<Active> {
        let surface = match self.under(self.cursor)? {
            Under::Window { surface, .. } | Under::Layer { surface, .. } => surface,
        };
        let ptr = self.pointer();
        let region = smithay::wayland::pointer_constraints::with_pointer_constraint(&surface, &ptr, |c| {
            c.filter(|c| c.is_active()).map(|c| match &*c {
                PointerConstraint::Locked(_) => None,
                PointerConstraint::Confined(_) => c.region().cloned(),
            })
        })?;
        Some(match region {
            None => Active::Locked,
            Some(r) => Active::Confined(
                r.rects
                    .iter()
                    .filter(|(kind, _)| matches!(kind, RectangleKind::Add))
                    .map(|(_, r)| r.to_f64())
                    .collect(),
            ),
        })
    }

    /// Whether a locked pointer holds the cursor still (it is not drawn then).
    pub fn pointer_locked(&self) -> bool {
        matches!(self.active_constraint(), Some(Active::Locked))
    }

    /// Motion to `pos` (global px) with the constraint applied: a locked pointer consumes it
    /// (the deltas go to the client as relative motion, the cursor stays where it is), a
    /// confined one clamps it into the window's place, intersected with the region it named.
    pub fn apply_pointer_constraint(
        &mut self,
        pos: Point<f64, Logical>,
        delta: (f64, f64),
        time: u32,
    ) -> Option<Point<f64, Logical>> {
        match self.active_constraint() {
            None => Some(pos),
            Some(Active::Locked) => {
                let ev =
                    RelativeMotionEvent { delta: delta.into(), delta_unaccel: delta.into(), utime: time as u64 * 1000 };
                let focus = match self.under(self.cursor) {
                    Some(Under::Window { surface, origin, .. } | Under::Layer { surface, origin, .. }) => {
                        Some((surface, origin))
                    }
                    None => None,
                };
                self.pointer().relative_motion(self, focus, &ev);
                None
            }
            Some(Active::Confined(region)) => {
                let Some(idx) = self.under(self.cursor).and_then(|u| u.window()) else { return Some(pos) };
                let m = self.mon_of(&self.clients[idx]);
                let pl = self.place_on(m, &self.clients[idx], &Fx::REST);
                let size = self.clients[idx].size();
                let origin = self.origin_of(m) + pl.origin;
                let window = Rectangle::new(origin, Size::from((size.w as f64 * pl.zoom, size.h as f64 * pl.zoom)));
                let mut allowed: Vec<Rectangle<f64, Logical>> = if region.is_empty() {
                    vec![window]
                } else {
                    region
                        .iter()
                        .map(|r| {
                            Rectangle::new(origin + r.loc.to_f64().upscale(pl.zoom), r.size.to_f64().upscale(pl.zoom))
                        })
                        .filter_map(|r| r.intersection(window))
                        .collect()
                };
                if allowed.is_empty() {
                    allowed.push(window);
                }
                // The region rect the pointer is in, else the nearest one.
                let key = |r: &Rectangle<f64, Logical>| {
                    if r.contains((pos.x, pos.y)) {
                        0.0
                    } else {
                        let c = (r.loc.x + r.size.w / 2.0, r.loc.y + r.size.h / 2.0);
                        (c.0 - pos.x).abs() + (c.1 - pos.y).abs()
                    }
                };
                let best =
                    allowed.iter().min_by(|a, b| key(a).partial_cmp(&key(b)).unwrap()).copied().unwrap_or(window);
                Some(
                    (
                        pos.x.clamp(best.loc.x, best.loc.x + best.size.w),
                        pos.y.clamp(best.loc.y, best.loc.y + best.size.h),
                    )
                        .into(),
                )
            }
        }
    }

    /// Activate the surface's constraint if it is not active yet.
    fn activate_constraint(&mut self, surface: &WlSurface) {
        let ptr = self.pointer();
        smithay::wayland::pointer_constraints::with_pointer_constraint(surface, &ptr, |c| {
            if let Some(c) = c {
                if !c.is_active() {
                    c.activate();
                }
            }
        });
    }

    /// The constraints of the surface the pointer just entered (a persistent constraint
    /// re-activates here).
    pub fn pointer_constraint_entered(&mut self, surface: &WlSurface) {
        self.activate_constraint(surface);
    }
}

impl PointerConstraintsHandler for Uwc {
    fn new_constraint(&mut self, surface: &WlSurface, _pointer: &PointerHandle<Self>) {
        // Active right away when the pointer is already on the surface.
        let over = match self.under(self.cursor) {
            Some(Under::Window { surface: s, .. } | Under::Layer { surface: s, .. }) => s == *surface,
            None => false,
        };
        if over {
            self.activate_constraint(surface);
        }
        self.damage();
    }

    fn cursor_position_hint(
        &mut self,
        surface: &WlSurface,
        _pointer: &PointerHandle<Self>,
        location: Point<f64, Logical>,
    ) {
        // Where the client wants the cursor while it holds the pointer locked.
        if let Some((m, c)) = self.client_on(surface) {
            let pl = self.place_on(m, c, &Fx::REST);
            self.cursor = self.origin_of(m) + pl.origin + location.upscale(pl.zoom);
            self.redraw = true;
        }
    }
}

impl Uwc {
    /// The client whose main surface this is, and the monitor it is on.
    fn client_on(&self, surface: &WlSurface) -> Option<(usize, &Client)> {
        let (i, m) = self
            .clients
            .iter()
            .enumerate()
            .find(|(_, c)| c.surface().as_ref() == Some(surface))
            .map(|(i, c)| (i, self.mon_of(c)))?;
        Some((m, &self.clients[i]))
    }
}
