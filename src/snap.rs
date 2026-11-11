//! Snapping while a window is dragged: its edges and centre line jump to the edges and centre lines
//! of nearby windows (`snap-distance`, in screen px, so it feels the same at any zoom),
//! and to a grid when `snap-grid` is set (canvas px). A guide line is drawn along each alignment
//! (`snap-color`). Holding Ctrl while dragging turns it off.

use smithay::{
    backend::renderer::element::{solid::SolidColorRenderElement, Id, Kind},
    utils::{Physical, Rectangle},
};

use crate::{render::UwcElement, state::Uwc};

/// An alignment to draw: a line at canvas `pos` (x when `vertical`), from `from` to `to` along the
/// other axis.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Guide {
    pub vertical: bool,
    pub pos: f64,
    pub from: f64,
    pub to: f64,
}

pub struct Guides {
    pub list: Vec<Guide>,
    pub ids: [Id; 6],
}

impl Default for Guides {
    fn default() -> Self {
        Guides { list: Vec::new(), ids: std::array::from_fn(|_| Id::new()) }
    }
}

/// The best snap on one axis. `mine` are the moving window's lines (start, centre, end) at its
/// proposed position, `targets` the other lines with the range they span. Returns the shift to
/// apply and the target it aligns with, if any is within `threshold`.
pub fn snap_axis(mine: [f64; 3], targets: &[(f64, (f64, f64))], threshold: f64) -> Option<(f64, f64, (f64, f64))> {
    let mut best: Option<(f64, f64, (f64, f64))> = None;
    for m in mine {
        for &(t, span) in targets {
            let d = t - m;
            if d.abs() <= threshold && best.is_none_or(|(b, _, _)| d.abs() < b.abs()) {
                best = Some((d, t, span));
            }
        }
    }
    best
}

impl Uwc {
    /// Where a dragged window at `(x, y)` ends up after snapping, and the guides to draw.
    pub fn snap_move(&self, id: u32, x: f64, y: f64) -> (f64, f64, Vec<Guide>) {
        let (dist, grid) = (self.config.snap_distance, self.config.snap_grid);
        let ctrl = self.keyboard().modifier_state().ctrl;
        let Some(i) = self.client_index(id) else { return (x, y, Vec::new()) };
        if ctrl || (dist <= 0.0 && grid <= 0.0) || self.clients[i].pin.is_some() {
            return (x, y, Vec::new());
        }
        let threshold = dist / self.viewport().zoom.max(0.01);
        let me = self.clients[i].rect().size;
        // Everything else that is drawn on this view: other canvas windows.
        let others: Vec<Rectangle<f64, smithay::utils::Logical>> = self
            .clients
            .iter()
            .filter(|c| c.id != id && self.visible_on(self.cur, c) && c.pin.is_none() && !c.fullscreen)
            .map(|c| c.rect())
            .collect();
        let lines = |r: &Rectangle<f64, smithay::utils::Logical>, vertical: bool| -> Vec<(f64, (f64, f64))> {
            let (a, len, span) = if vertical {
                (r.loc.x, r.size.w, (r.loc.y, r.loc.y + r.size.h))
            } else {
                (r.loc.y, r.size.h, (r.loc.x, r.loc.x + r.size.w))
            };
            vec![(a, span), (a + len / 2.0, span), (a + len, span)]
        };
        let xs: Vec<_> = others.iter().flat_map(|r| lines(r, true)).collect();
        let ys: Vec<_> = others.iter().flat_map(|r| lines(r, false)).collect();
        let mut guides = Vec::new();
        let (mut nx, mut ny) = (x, y);
        if dist > 0.0 {
            if let Some((d, t, span)) = snap_axis([x, x + me.w / 2.0, x + me.w], &xs, threshold) {
                nx = x + d;
                guides.push(Guide { vertical: true, pos: t, from: span.0.min(y), to: span.1.max(y + me.h) });
            }
            if let Some((d, t, span)) = snap_axis([y, y + me.h / 2.0, y + me.h], &ys, threshold) {
                ny = y + d;
                guides.push(Guide { vertical: false, pos: t, from: span.0.min(x), to: span.1.max(x + me.w) });
            }
        }
        // The grid takes what no window claimed.
        if grid > 0.0 {
            if !guides.iter().any(|g| g.vertical) {
                nx = (x / grid).round() * grid;
            }
            if !guides.iter().any(|g| !g.vertical) {
                ny = (y / grid).round() * grid;
            }
        }
        (nx, ny, guides)
    }

    /// The guide lines while a window is being dragged.
    pub fn guide_elements(&self, m: usize, scale: f64) -> Vec<UwcElement> {
        if !matches!(self.drag, Some(crate::state::Drag::Move { .. })) || self.guides.list.is_empty() {
            return Vec::new();
        }
        let vp = self.viewport_of(m);
        let color = self.config.snap_color;
        let thick = scale.round().max(1.0) as i32;
        let clamp = |v: f64| v.clamp(-1.0e6, 1.0e6).round() as i32;
        self.guides
            .list
            .iter()
            .zip(self.guides.ids.iter())
            .map(|(g, id)| {
                let (a, b) = if g.vertical {
                    (vp.canvas_to_screen((g.pos, g.from)), vp.canvas_to_screen((g.pos, g.to)))
                } else {
                    (vp.canvas_to_screen((g.from, g.pos)), vp.canvas_to_screen((g.to, g.pos)))
                };
                let (x0, y0, x1, y1) = (clamp(a.0 * scale), clamp(a.1 * scale), clamp(b.0 * scale), clamp(b.1 * scale));
                let rect = if g.vertical {
                    Rectangle::<i32, Physical>::new((x0 - thick / 2, y0).into(), (thick, (y1 - y0).max(1)).into())
                } else {
                    Rectangle::<i32, Physical>::new((x0, y0 - thick / 2).into(), ((x1 - x0).max(1), thick).into())
                };
                UwcElement::from(SolidColorRenderElement::new(
                    id.clone(),
                    rect,
                    self.deco_commit,
                    [color[0], color[1], color[2], 1.0],
                    Kind::Unspecified,
                ))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_nearest_line_within_the_threshold_wins() {
        let targets = [(100.0, (0.0, 50.0)), (112.0, (10.0, 20.0)), (300.0, (0.0, 1.0))];
        // Edges at 95, 110 (centre) and 125: 100 is 5 from the first, 112 is 2 from the centre.
        let (d, t, span) = snap_axis([95.0, 110.0, 125.0], &targets, 8.0).unwrap();
        assert_eq!((d, t, span), (2.0, 112.0, (10.0, 20.0)));
        assert!(snap_axis([200.0, 210.0, 220.0], &targets, 8.0).is_none(), "nothing near");
        // Exactly on a line: no shift, but still a guide.
        assert_eq!(snap_axis([100.0, 150.0, 200.0], &targets, 8.0).unwrap().0, 0.0);
    }
}
