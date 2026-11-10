//! Canvas geometry: a viewport (camera) onto an infinite 2D plane.
//!
//! Windows live at canvas coordinates. `Viewport.x/y` is the canvas point shown at the
//! top-left corner of the screen and `zoom` is screen px per canvas px, exactly as in uwm.

pub const ZOOM_MIN: f64 = 0.05;
pub const ZOOM_MAX: f64 = 8.0;

/// A screen rectangle `(x, y, width, height)` in logical px.
pub type Area = (f64, f64, f64, f64);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    pub x: f64,
    pub y: f64,
    pub zoom: f64,
}

impl Default for Viewport {
    fn default() -> Self {
        Viewport { x: 0.0, y: 0.0, zoom: 1.0 }
    }
}

impl Viewport {
    pub fn canvas_to_screen(&self, p: (f64, f64)) -> (f64, f64) {
        ((p.0 - self.x) * self.zoom, (p.1 - self.y) * self.zoom)
    }

    pub fn screen_to_canvas(&self, p: (f64, f64)) -> (f64, f64) {
        (p.0 / self.zoom + self.x, p.1 / self.zoom + self.y)
    }

    /// Move the view by a screen-space delta (dragging the canvas under the pointer).
    pub fn pan_screen(&mut self, dx: f64, dy: f64) {
        self.x -= dx / self.zoom;
        self.y -= dy / self.zoom;
    }

    /// Move the view by a canvas-space delta.
    pub fn pan_canvas(&mut self, dx: f64, dy: f64) {
        self.x += dx;
        self.y += dy;
    }

    /// Multiply the zoom by `factor`, keeping the canvas point under screen point `at` fixed.
    pub fn zoom_at(&mut self, factor: f64, at: (f64, f64)) {
        self.set_zoom_at((self.zoom * factor).clamp(ZOOM_MIN, ZOOM_MAX), at);
    }

    pub fn set_zoom_at(&mut self, zoom: f64, at: (f64, f64)) {
        let zoom = zoom.clamp(ZOOM_MIN, ZOOM_MAX);
        let c = self.screen_to_canvas(at);
        self.zoom = zoom;
        self.x = c.0 - at.0 / zoom;
        self.y = c.1 - at.1 / zoom;
    }

    /// Centre canvas point `c` in `area`, a screen rectangle `(x, y, w, h)` in logical px (the work
    /// area: the screen minus what bars reserve).
    pub fn center_on(&mut self, c: (f64, f64), area: Area) {
        self.x = c.0 - (area.0 + area.2 / 2.0) / self.zoom;
        self.y = c.1 - (area.1 + area.3 / 2.0) / self.zoom;
    }

    /// Zoom and pan so the canvas rectangle `(x, y, w, h)` fits `area` with `margin` screen px around it.
    pub fn fit(&mut self, rect: (f64, f64, f64, f64), area: Area, margin: f64) {
        let (x, y, w, h) = rect;
        let zx = (area.2 - 2.0 * margin) / w.max(1.0);
        let zy = (area.3 - 2.0 * margin) / h.max(1.0);
        self.zoom = zx.min(zy).clamp(ZOOM_MIN, ZOOM_MAX);
        self.center_on((x + w / 2.0, y + h / 2.0), area);
    }
}

/// Number of tags (workspaces); each has its own canvas and viewport.
pub const TAGCOUNT: usize = 9;
pub const ALLTAGS: u32 = (1 << TAGCOUNT) - 1;

/// The viewport in use for a tag set is the one of its lowest tag, as in uwm.
pub fn lowest_tag(tagset: u32) -> usize {
    (tagset & ALLTAGS).trailing_zeros().min(TAGCOUNT as u32 - 1) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: (f64, f64), b: (f64, f64)) -> bool {
        (a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9
    }

    #[test]
    fn roundtrip() {
        let v = Viewport { x: 120.0, y: -40.0, zoom: 0.5 };
        let p = (333.0, 77.0);
        assert!(close(v.screen_to_canvas(v.canvas_to_screen(p)), p));
    }

    #[test]
    fn pan_moves_canvas_with_pointer() {
        let mut v = Viewport { x: 0.0, y: 0.0, zoom: 2.0 };
        let before = v.canvas_to_screen((100.0, 100.0));
        v.pan_screen(30.0, -10.0);
        let after = v.canvas_to_screen((100.0, 100.0));
        assert!(close((after.0 - before.0, after.1 - before.1), (30.0, -10.0)));
    }

    #[test]
    fn zoom_keeps_point_fixed() {
        let mut v = Viewport { x: 50.0, y: 10.0, zoom: 1.0 };
        let at = (400.0, 300.0);
        let c = v.screen_to_canvas(at);
        v.zoom_at(1.25, at);
        v.zoom_at(1.25, at);
        assert!(close(v.screen_to_canvas(at), c));
        assert!((v.zoom - 1.5625).abs() < 1e-9);
    }

    #[test]
    fn zoom_is_clamped() {
        let mut v = Viewport::default();
        v.zoom_at(1e9, (0.0, 0.0));
        assert_eq!(v.zoom, ZOOM_MAX);
        v.zoom_at(1e-9, (0.0, 0.0));
        assert_eq!(v.zoom, ZOOM_MIN);
    }

    #[test]
    fn fit_contains_rect() {
        let mut v = Viewport::default();
        v.fit((1000.0, 1000.0, 400.0, 200.0), (0.0, 0.0, 800.0, 600.0), 20.0);
        let (sx, sy) = v.canvas_to_screen((1000.0, 1000.0));
        let (ex, ey) = v.canvas_to_screen((1400.0, 1200.0));
        assert!(sx >= 19.9 && sy >= 0.0 && ex <= 780.1 && ey <= 600.0);
    }

    #[test]
    fn center_respects_the_work_area() {
        // A 30 px bar on top: the work area is 0,30 800x570, its centre is at screen (400, 315).
        let mut v = Viewport { x: 0.0, y: 0.0, zoom: 2.0 };
        v.center_on((500.0, 500.0), (0.0, 30.0, 800.0, 570.0));
        let s = v.canvas_to_screen((500.0, 500.0));
        assert!(close(s, (400.0, 315.0)), "{s:?}");
    }

    #[test]
    fn lowest_tag_picks_first_set_bit() {
        assert_eq!(lowest_tag(0b100), 2);
        assert_eq!(lowest_tag(0b110), 1);
        assert_eq!(lowest_tag(0), TAGCOUNT - 1);
    }
}
