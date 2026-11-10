//! Turning the canvas into render elements for one frame.
//!
//! Smithay draws a list of elements front to back, so zoom is a `RescaleRenderElement` around each
//! window (no scene graph to fight, unlike wlroots). Decoration (corners, border, shadow, dim,
//! backdrop) comes from the shaders in `deco.rs`; without them, borders are four solid rects.

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

use std::collections::HashMap;

use smithay::backend::renderer::gles::GlesPixelProgram;

use crate::{
    deco::{compile_user, spin_degrees, uses_time, Paint, RoundedElement, ShaderElement, Shaders, UserShader, GL_LOG},
    rules::Deco,
    state::{Client, Uwc},
};

render_elements! {
    pub UwcElement<=GlesRenderer>;
    Surface=RescaleRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>,
    Rounded=RoundedElement,
    Shader=ShaderElement,
    Solid=SolidColorRenderElement,
    Cursor=MemoryRenderBufferRenderElement<GlesRenderer>,
    Layer=WaylandSurfaceRenderElement<GlesRenderer>,
}

/// How one window looks right now, resolved from the options and its focus.
struct Look {
    border: Paint,
    alpha: f32,
    dim: f64,
    shadow_size: f64,
    shadow_color: [f32; 4],
}

impl Uwc {
    /// Everything on one output for one frame, front to back: the pointer, overlay and top layer
    /// surfaces, the canvas, then bottom and background layer surfaces.
    /// `scale` is the output scale (physical px per logical px).
    pub fn frame_elements(&mut self, renderer: &mut GlesRenderer, scale: f64) -> Vec<UwcElement> {
        self.scene_elements(renderer, scale, true)
    }

    /// The whole picture of the output; `with_cursor` false leaves the pointer out (screencopy).
    pub fn scene_elements(&mut self, renderer: &mut GlesRenderer, scale: f64, with_cursor: bool) -> Vec<UwcElement> {
        use smithay::wayland::shell::wlr_layer::Layer;
        // A spinning gradient border changes every frame: new content, and another frame after this one.
        if self.spinning_border_visible() {
            self.deco_commit.increment();
            self.animating = true;
        }
        let shaders = Shaders::get(renderer);
        // User border shaders the visible windows use, compiled (once) before drawing.
        let mut user: HashMap<String, GlesPixelProgram> = HashMap::new();
        let wanted: Vec<String> = self
            .clients
            .iter()
            .filter(|c| self.visible(c) && !c.fullscreen)
            .map(|c| self.deco_for(c).border_shader)
            .filter(|s| !s.is_empty())
            .collect();
        let mut animated = false;
        for path in wanted {
            if let Some((p, anim)) = self.user_shader(renderer, UserShader::Border, &path) {
                animated |= anim;
                user.insert(path, p);
            }
        }
        let background = self.config.background_shader.clone();
        let background = self.user_shader(renderer, UserShader::Background, &background);
        animated |= background.as_ref().is_some_and(|b| b.1);
        if animated {
            self.deco_commit.increment();
            self.animating = true;
        }
        let mut out = if with_cursor { self.cursor_elements(renderer, scale) } else { Vec::new() };
        out.extend(self.layer_elements(renderer, scale, &[Layer::Overlay]));
        // Fullscreen windows cover the top layer (bars), over the fullscreen-bg color.
        let fullscreen = self.render_elements(renderer, shaders.as_ref(), &user, scale, true);
        if !fullscreen.is_empty() {
            out.extend(fullscreen);
            let (w, h) = self.screen_size();
            let geo = Rectangle::from_size(Size::from(((w * scale).round() as i32, (h * scale).round() as i32)));
            out.push(UwcElement::from(SolidColorRenderElement::new(
                self.fullscreen_bg_id.clone(),
                geo,
                self.deco_commit,
                self.config.fullscreen_bg,
                Kind::Unspecified,
            )));
        }
        out.extend(self.layer_elements(renderer, scale, &[Layer::Top]));
        out.extend(self.render_elements(renderer, shaders.as_ref(), &user, scale, false));
        out.extend(self.layer_elements(renderer, scale, &[Layer::Bottom, Layer::Background]));
        if let Some((program, _)) = background {
            let (w, h) = self.screen_size();
            let vp = self.viewport();
            out.push(UwcElement::from(ShaderElement::user_background(
                &program,
                &self.background_id,
                self.deco_commit,
                Rectangle::from_size(Size::from(((w * scale).round() as i32, (h * scale).round() as i32))),
                self.start_time.elapsed().as_secs_f64(),
                (vp.x, vp.y),
                vp.zoom * scale,
            )));
        }
        out
    }

    /// A user shader file, compiled once per config (`None` if it is unset or does not compile;
    /// then the reason is in `shader_errors`). The bool: it uses its time parameter.
    fn user_shader(&mut self, r: &mut GlesRenderer, kind: UserShader, file: &str) -> Option<(GlesPixelProgram, bool)> {
        if file.is_empty() {
            return None;
        }
        let path = crate::config::resolve(file, &self.config.dir);
        let key = (kind, path.clone());
        if let Some(v) = self.user_shaders.get(&key) {
            return v.clone();
        }
        let res = match std::fs::read_to_string(&path) {
            Err(e) => Err(format!("{}: {e}", path.display())),
            Ok(src) => {
                if let Ok(mut log) = GL_LOG.lock() {
                    *log = None;
                }
                match compile_user(r, kind, &src) {
                    Ok(p) => Ok((p, uses_time(kind, &src))),
                    Err(e) => Err(GL_LOG.lock().ok().and_then(|mut l| l.take()).unwrap_or_else(|| e.to_string())),
                }
            }
        };
        let v = match res {
            Ok(p) => {
                self.shader_errors.remove(file);
                Some(p)
            }
            Err(msg) => {
                tracing::warn!("shader {file}: {msg}");
                self.shader_errors.insert(file.to_string(), msg);
                None
            }
        };
        self.ipc.dirty = true;
        self.user_shaders.insert(key, v.clone());
        v
    }

    /// Some visible window shows a gradient border that turns (rules can turn it on per window).
    fn spinning_border_visible(&self) -> bool {
        self.clients.iter().filter(|w| self.visible(w) && !w.fullscreen).any(|w| {
            let d = self.deco_for(w);
            let gradient = d.border_gradient.is_some() || (d.focus_gradient.is_some() && self.focus == Some(w.id));
            d.border_spin_ms != 0.0 && d.border_width > 0 && gradient
        })
    }

    fn look(&self, c: &Client, o: &Deco) -> Look {
        let focused = self.focus == Some(c.id);
        let spin = spin_degrees(o.border_spin_ms, self.start_time.elapsed().as_secs_f64() * 1000.0);
        let border = match (focused, &o.focus_gradient, &o.border_gradient) {
            (true, Some(g), _) => Paint::Gradient(g.clone(), spin),
            (true, None, _) => Paint::Solid(o.focus_color),
            (false, _, Some(g)) => Paint::Gradient(g.clone(), spin),
            (false, _, None) => Paint::Solid(o.border_color),
        };
        let (shadow_size, shadow_color) = if focused {
            (
                o.focus_shadow_size.unwrap_or(o.shadow_size),
                if o.focus_shadow_color[3] == 0.0 { o.shadow_color } else { o.focus_shadow_color },
            )
        } else {
            (o.shadow_size, o.shadow_color)
        };
        Look {
            border,
            alpha: if focused { o.opacity } else { o.inactive_opacity } as f32,
            dim: if focused { 0.0 } else { o.inactive_dim },
            shadow_size,
            shadow_color,
        }
    }

    /// The visible windows that are fullscreen (or the ones that are not), with their popups and
    /// decoration, topmost first.
    pub fn render_elements(
        &self,
        renderer: &mut GlesRenderer,
        shaders: Option<&Shaders>,
        user: &HashMap<String, GlesPixelProgram>,
        scale: f64,
        fullscreen: bool,
    ) -> Vec<UwcElement> {
        let mut out: Vec<UwcElement> = Vec::new();
        let commit = self.deco_commit;

        for c in self.clients.iter().rev().filter(|c| self.visible(c) && c.fullscreen == fullscreen) {
            // The options with this window's rules applied.
            let deco = self.deco_for(c);
            let o = &deco;
            let look = self.look(c, o);
            let pl = self.place(c);
            // Window px to physical output px.
            let z = pl.zoom;
            let k = z * scale;
            let g = c.window.geometry().loc;
            let size = c.size();
            // The content box (the window geometry) and the outer (border) box, physical px.
            let content = Rectangle::new(
                Point::<f64, Physical>::from((pl.origin.x * scale, pl.origin.y * scale)),
                Size::from((size.w as f64 * k, size.h as f64 * k)),
            );
            // Where the surface origin goes; the geometry may start inside the surface (client shadows).
            let origin: Point<i32, Physical> =
                Point::<f64, Physical>::from((content.loc.x - g.x as f64 * k, content.loc.y - g.y as f64 * k))
                    .to_i32_round();
            // Fullscreen windows get no decoration.
            let decorated = !c.fullscreen;
            let bw = if decorated { o.border_width as f64 * k } else { 0.0 };
            let outer = Rectangle::new(
                (content.loc.x - bw, content.loc.y - bw).into(),
                Size::from((content.size.w + 2.0 * bw, content.size.h + 2.0 * bw)),
            );
            let radius = if decorated { o.corner_radius * k } else { 0.0 };
            // Content corners follow the border's inner edge.
            let inner_radius = (radius - bw).max(0.0);
            let [dim_id, backdrop_id, border_id, shadow_id] = &c.deco_ids;

            // Popups sit above their toplevel; `Space` does the same placement.
            let g = c.window.geometry().loc;
            for (popup, offset) in smithay::desktop::PopupManager::popups_for_surface(c.surface()) {
                let rel: Point<i32, smithay::utils::Logical> = g + offset - popup.geometry().loc;
                let at = origin + rel.to_f64().to_physical(scale).to_i32_round();
                let els = render_elements_from_surface_tree::<_, WaylandSurfaceRenderElement<GlesRenderer>>(
                    renderer,
                    popup.wl_surface(),
                    at,
                    Scale::from(scale),
                    look.alpha,
                    Kind::Unspecified,
                );
                out.extend(els.into_iter().map(|e| UwcElement::from(RescaleRenderElement::from_element(e, origin, z))));
            }

            if let (Some(s), true) = (shaders, decorated && look.dim > 0.0) {
                out.push(UwcElement::from(ShaderElement::rounded_box(
                    s,
                    dim_id,
                    commit,
                    content.to_i32_round(),
                    inner_radius,
                    0.0,
                    &Paint::Solid([0.0, 0.0, 0.0, look.dim as f32]),
                )));
            }

            let els = AsRenderElements::<GlesRenderer>::render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
                &c.window,
                renderer,
                origin,
                Scale::from(scale),
                look.alpha,
            );
            for e in els {
                let e = RescaleRenderElement::from_element(e, origin, z);
                out.push(RoundedElement::wrap(e, shaders, content, inner_radius, Scale::from(scale)));
            }

            if !decorated {
                continue;
            }
            let Some(s) = shaders else {
                out.extend(self.solid_border(c, o, &look, scale));
                continue;
            };
            if o.backdrop_color[3] > 0.0 {
                out.push(UwcElement::from(ShaderElement::rounded_box(
                    s,
                    backdrop_id,
                    commit,
                    content.to_i32_round(),
                    inner_radius,
                    0.0,
                    &Paint::Solid(o.backdrop_color),
                )));
            }
            if bw > 0.0 {
                out.push(UwcElement::from(match user.get(&o.border_shader) {
                    Some(program) => ShaderElement::user_box(
                        program,
                        border_id,
                        commit,
                        outer.to_i32_round(),
                        radius,
                        bw,
                        &look.border,
                        self.start_time.elapsed().as_secs_f64(),
                        self.focus == Some(c.id),
                    ),
                    None => {
                        ShaderElement::rounded_box(s, border_id, commit, outer.to_i32_round(), radius, bw, &look.border)
                    }
                }));
            }
            if look.shadow_size > 0.0 && look.shadow_color[3] > 0.0 {
                let spread = o.shadow_spread * k;
                let caster = Rectangle::new(
                    (outer.loc.x - spread + o.shadow_offset_x * k, outer.loc.y - spread + o.shadow_offset_y * k).into(),
                    Size::from(((outer.size.w + 2.0 * spread).max(1.0), (outer.size.h + 2.0 * spread).max(1.0))),
                );
                let size = look.shadow_size * k;
                out.push(UwcElement::from(ShaderElement::shadow(
                    s,
                    shadow_id,
                    commit,
                    caster,
                    radius + spread.max(0.0),
                    size / 3.0,
                    size,
                    look.shadow_color,
                    outer,
                    radius,
                )));
            }
        }
        out
    }

    /// Square solid borders, for when the shaders are not available.
    fn solid_border(&self, c: &Client, o: &Deco, look: &Look, scale: f64) -> Vec<UwcElement> {
        let bw = o.border_width;
        if bw <= 0 {
            return vec![];
        }
        let pl = self.place(c);
        // Window px (relative to the geometry's top-left) to physical px.
        let phys = |x: f64, y: f64| -> Point<i32, Physical> {
            (((pl.origin.x + x * pl.zoom) * scale).round() as i32, ((pl.origin.y + y * pl.zoom) * scale).round() as i32)
                .into()
        };
        let s = c.size();
        let (w, h, b) = (s.w as f64, s.h as f64, bw as f64);
        let (o0, o1) = (phys(-b, -b), phys(w + b, h + b));
        let (i0, i1) = (phys(0.0, 0.0), phys(w, h));
        let color = match &look.border {
            Paint::Solid(c) => *c,
            Paint::Gradient(g, _) => g.colors[0],
        };
        let rect = |a: Point<i32, Physical>, b: Point<i32, Physical>| {
            Rectangle::new(a, Size::from(((b.x - a.x).max(0), (b.y - a.y).max(0))))
        };
        let strips = [
            rect(o0, (o1.x, i0.y).into()),
            rect((o0.x, i1.y).into(), o1),
            rect((o0.x, i0.y).into(), (i0.x, i1.y).into()),
            rect((i1.x, i0.y).into(), (o1.x, i1.y).into()),
        ];
        c.border_ids
            .iter()
            .zip(strips)
            .map(|(id, geo)| {
                UwcElement::from(SolidColorRenderElement::new(
                    id.clone(),
                    geo,
                    self.deco_commit,
                    color,
                    Kind::Unspecified,
                ))
            })
            .collect()
    }
}
