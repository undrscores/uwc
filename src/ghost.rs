//! Close animation: when a window goes away, what it looked like (content, border and shadow) is
//! kept as one texture and drawn fading and shrinking for `close-ms`, anchored to the canvas so it
//! follows the view. The snapshot is taken as the toplevel is destroyed, while its surface still
//! holds its last buffer; a backend without a renderer (headless) just skips the animation.

use std::collections::HashMap;

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            damage::OutputDamageTracker,
            element::{
                texture::TextureRenderElement,
                utils::{Relocate, RelocateRenderElement},
                Element, Id, Kind,
            },
            gles::{GlesPixelProgram, GlesRenderer, GlesTexture},
            Bind, Offscreen, Renderer,
        },
    },
    utils::{Logical, Physical, Point, Scale, Size, Transform},
};

use crate::{
    deco::{Shaders, UserShader},
    render::UwcElement,
    state::Uwc,
};

/// A closed window fading out.
pub struct Ghost {
    texture: GlesTexture,
    /// Where it was, in canvas px (or screen px if the window was pinned), and how big.
    loc: Point<f64, Logical>,
    size: Size<f64, Logical>,
    on_canvas: bool,
    alpha: f64,
    scale: f64,
}

impl Uwc {
    /// Keep a picture of window `id` to fade out after it is gone. Call before it is removed.
    pub fn snapshot_closing(&mut self, id: u32) {
        if self.config.close_ms <= 0.0 {
            return;
        }
        let Some(c) = self.client_index(id).map(|i| &self.clients[i]) else { return };
        if !(c.mapped && self.visible(c) && !c.fullscreen) {
            return;
        }
        let scale = self.output_scale();
        // A client that vanished (killed, crashed, disconnected) has lost its buffer by now: use the
        // picture taken when the close was asked for, if there is one.
        let has_buffer =
            smithay::backend::renderer::utils::with_renderer_surface_state(c.surface(), |s| s.buffer().is_some())
                .unwrap_or(false);
        let pending = self.closing.remove(&id).filter(|(_, at)| at.elapsed() < std::time::Duration::from_secs(10));
        let fresh =
            if has_buffer { self.with_renderer(|state, r| state.make_ghost(r, id, scale)).flatten() } else { None };
        let Some(ghost) = fresh.or(pending.map(|(g, _)| g)) else { return };
        if !self.anim_running() {
            self.begin_anim();
        }
        self.ghosts.push(ghost);
        self.damage();
    }

    /// A close was asked for: take the picture now, while the surface is sure to have its buffer, for
    /// when the client is gone before we can (see `snapshot_closing`).
    pub fn snapshot_before_close(&mut self, id: u32) {
        if self.config.close_ms <= 0.0 {
            return;
        }
        let Some(c) = self.client_index(id).map(|i| &self.clients[i]) else { return };
        if !(c.mapped && self.visible(c) && !c.fullscreen) {
            return;
        }
        let scale = self.output_scale();
        if let Some(Some(g)) = self.with_renderer(|state, r| state.make_ghost(r, id, scale)) {
            self.closing.insert(id, (g, std::time::Instant::now()));
        }
    }

    /// The border shaders already compiled for window `id` (nothing is compiled here).
    fn compiled_border_shaders(&self, id: u32) -> HashMap<String, GlesPixelProgram> {
        let mut out = HashMap::new();
        if let Some(c) = self.client_index(id).map(|i| &self.clients[i]) {
            let file = self.deco_for(c).border_shader;
            if !file.is_empty() {
                let path = crate::config::resolve(&file, &self.config.dir);
                if let Some(Some((program, _))) = self.user_shaders.get(&(UserShader::Border, path)) {
                    out.insert(file, program.clone());
                }
            }
        }
        out
    }

    fn make_ghost(&mut self, r: &mut GlesRenderer, id: u32, scale: f64) -> Option<Ghost> {
        let shaders = Shaders::get(r);
        let user = self.compiled_border_shaders(id);
        let elements = self.render_elements(r, shaders.as_ref(), &user, scale, false, Some(id));
        let sc = Scale::from(scale);
        let bbox = elements.iter().map(|e| e.geometry(sc)).reduce(|a, b| a.merge(b))?;
        if bbox.size.w <= 0 || bbox.size.h <= 0 || bbox.size.w > 8192 || bbox.size.h > 8192 {
            return None;
        }
        let shifted: Vec<RelocateRenderElement<UwcElement>> = elements
            .into_iter()
            .map(|e| {
                RelocateRenderElement::from_element(
                    e,
                    Point::<i32, Physical>::from((-bbox.loc.x, -bbox.loc.y)),
                    Relocate::Relative,
                )
            })
            .collect();
        let mut texture: GlesTexture = r.create_buffer(Fourcc::Abgr8888, (bbox.size.w, bbox.size.h).into()).ok()?;
        {
            let mut fb = r.bind(&mut texture).ok()?;
            let mut tracker = OutputDamageTracker::new(bbox.size, scale, Transform::Normal);
            tracker.render_output(r, &mut fb, 0, &shifted, [0.0; 4]).ok()?;
        }
        let c = &self.clients[self.client_index(id)?];
        let on_canvas = self.place(c).on_canvas;
        let vp = self.viewport();
        let loc: Point<f64, Logical> = (bbox.loc.x as f64 / scale, bbox.loc.y as f64 / scale).into();
        let size: Size<f64, Logical> = (bbox.size.w as f64 / scale, bbox.size.h as f64 / scale).into();
        let (loc, size) = if on_canvas {
            (vp.screen_to_canvas((loc.x, loc.y)).into(), (size.w / vp.zoom, size.h / vp.zoom).into())
        } else {
            (loc, size)
        };
        Some(Ghost { texture, loc, size, on_canvas, alpha: 1.0, scale: 1.0 })
    }

    /// Fade and shrink the ghosts; true while any is left.
    pub fn step_ghosts(&mut self, dt: f64) -> bool {
        if self.ghosts.is_empty() {
            return false;
        }
        let tau = (self.config.close_ms / 1000.0 / 4.0).max(1e-4);
        let k = 1.0 - (-dt / tau).exp();
        let target = self.config.close_scale;
        for g in &mut self.ghosts {
            g.alpha -= g.alpha * k;
            g.scale += (target - g.scale) * k;
        }
        self.ghosts.retain(|g| g.alpha > 0.03);
        self.redraw = true;
        if self.ghosts.is_empty() {
            self.ipc.dirty = true;
        }
        !self.ghosts.is_empty()
    }

    /// The ghosts as elements (a fresh id each frame: their alpha changes without a new commit).
    pub fn ghost_elements(&self, renderer: &mut GlesRenderer, scale: f64) -> Vec<UwcElement> {
        let vp = self.viewport();
        self.ghosts
            .iter()
            .map(|g| {
                let (loc, size) = if g.on_canvas {
                    (vp.canvas_to_screen((g.loc.x, g.loc.y)), (g.size.w * vp.zoom, g.size.h * vp.zoom))
                } else {
                    ((g.loc.x, g.loc.y), (g.size.w, g.size.h))
                };
                // Shrink about the centre.
                let (w, h) = (size.0 * g.scale, size.1 * g.scale);
                let at = (loc.0 + (size.0 - w) / 2.0, loc.1 + (size.1 - h) / 2.0);
                let dest: Size<i32, Logical> = (w.round().max(1.0) as i32, h.round().max(1.0) as i32).into();
                UwcElement::from(TextureRenderElement::from_static_texture(
                    Id::new(),
                    renderer.context_id(),
                    Point::<f64, Physical>::from((at.0 * scale, at.1 * scale)),
                    g.texture.clone(),
                    1,
                    Transform::Normal,
                    Some(g.alpha as f32),
                    None,
                    Some(dest),
                    None,
                    Kind::Unspecified,
                ))
            })
            .collect()
    }
}
