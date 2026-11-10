//! Turning the canvas into render elements for one frame.
//!
//! Smithay draws a list of elements front to back, so zoom is a `RescaleRenderElement` around each
//! window (no scene graph to fight, unlike wlroots). Borders are four solid rects per window.

use smithay::{
    backend::renderer::{
        element::{
            memory::MemoryRenderBufferRenderElement,
            render_elements,
            solid::SolidColorRenderElement,
            surface::{render_elements_from_surface_tree, WaylandSurfaceRenderElement},
            utils::RescaleRenderElement,
            AsRenderElements, Kind,
        },
        gles::GlesRenderer,
    },
    utils::{Physical, Point, Rectangle, Scale, Size},
};

use crate::state::Uwc;

render_elements! {
    pub UwcElement<=GlesRenderer>;
    Surface=RescaleRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>,
    Solid=SolidColorRenderElement,
    Cursor=MemoryRenderBufferRenderElement<GlesRenderer>,
    Layer=WaylandSurfaceRenderElement<GlesRenderer>,
}

impl Uwc {
    /// Everything on one output for one frame, front to back: the pointer, overlay and top layer
    /// surfaces, the canvas, then bottom and background layer surfaces.
    /// `scale` is the output scale (physical px per logical px).
    pub fn frame_elements(&mut self, renderer: &mut GlesRenderer, scale: f64) -> Vec<UwcElement> {
        use smithay::wayland::shell::wlr_layer::Layer;
        let mut out = self.cursor_elements(renderer, scale);
        out.extend(self.layer_elements(renderer, scale, &[Layer::Overlay, Layer::Top]));
        out.extend(self.render_elements(renderer, scale));
        out.extend(self.layer_elements(renderer, scale, &[Layer::Bottom, Layer::Background]));
        out
    }

    /// Every visible window (and its popups and border) as elements, topmost first.
    pub fn render_elements(&self, renderer: &mut GlesRenderer, scale: f64) -> Vec<UwcElement> {
        let mut out: Vec<UwcElement> = Vec::new();
        let vp = self.viewport();
        let z = vp.zoom;
        let bw = self.config.border_width;
        // Screen (logical) px to physical px.
        let phys = |p: (f64, f64)| -> Point<i32, Physical> {
            ((p.0 * scale).round() as i32, (p.1 * scale).round() as i32).into()
        };

        for c in self.clients.iter().rev().filter(|c| self.visible(c)) {
            let origin = phys(vp.canvas_to_screen((c.surface_origin().x, c.surface_origin().y)));

            // Popups sit above their toplevel; `Space` does the same placement.
            let g = c.window.geometry().loc;
            for (popup, offset) in smithay::desktop::PopupManager::popups_for_surface(c.surface()) {
                // Same placement as smithay's `Space`: relative to the window geometry, not the surface.
                let rel: Point<i32, smithay::utils::Logical> = g + offset - popup.geometry().loc;
                let at = origin + rel.to_f64().to_physical(scale).to_i32_round();
                let els = render_elements_from_surface_tree::<_, WaylandSurfaceRenderElement<GlesRenderer>>(
                    renderer,
                    popup.wl_surface(),
                    at,
                    Scale::from(scale),
                    1.0,
                    Kind::Unspecified,
                );
                out.extend(els.into_iter().map(|e| UwcElement::from(RescaleRenderElement::from_element(e, origin, z))));
            }

            let els = AsRenderElements::<GlesRenderer>::render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
                &c.window,
                renderer,
                origin,
                Scale::from(scale),
                1.0,
            );
            out.extend(els.into_iter().map(|e| UwcElement::from(RescaleRenderElement::from_element(e, origin, z))));

            if bw > 0 {
                let r = c.rect();
                let (x0, y0) = vp.canvas_to_screen((r.loc.x - bw as f64, r.loc.y - bw as f64));
                let (x1, y1) = vp.canvas_to_screen((r.loc.x + r.size.w + bw as f64, r.loc.y + r.size.h + bw as f64));
                let (ix0, iy0) = vp.canvas_to_screen((r.loc.x, r.loc.y));
                let (ix1, iy1) = vp.canvas_to_screen((r.loc.x + r.size.w, r.loc.y + r.size.h));
                let color = if self.focus == Some(c.id) { self.config.focus_color } else { self.config.border_color };
                let rect = |ax: f64, ay: f64, bx: f64, by: f64| -> Rectangle<i32, Physical> {
                    let (a, b) = (phys((ax, ay)), phys((bx, by)));
                    Rectangle::new(a, Size::from(((b.x - a.x).max(0), (b.y - a.y).max(0))))
                };
                let strips = [
                    rect(x0, y0, x1, iy0),   // top
                    rect(x0, iy1, x1, y1),   // bottom
                    rect(x0, iy0, ix0, iy1), // left
                    rect(ix1, iy0, x1, iy1), // right
                ];
                for (id, geo) in c.border_ids.iter().zip(strips) {
                    out.push(UwcElement::from(SolidColorRenderElement::new(
                        id.clone(),
                        geo,
                        self.deco_commit,
                        color,
                        Kind::Unspecified,
                    )));
                }
            }
        }
        out
    }
}
