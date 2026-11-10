//! Window decoration drawn with GLES shaders: rounded corners, gradient borders, shadows (and the
//! focus glow), backdrop and dim. This is what uwm got from scenefx; smithay has no equivalent, so
//! uwc has three small shaders of its own:
//!
//! - `BOX`: a rounded rectangle, filled or as a ring, in one color or a gradient of up to 6.
//! - `SHADOW`: an analytic rounded-box shadow (Evan Wallace's erf approximation), with the window's
//!   own area cut out so translucent windows don't show it.
//! - `ROUND`: a texture shader that clips a window's surfaces to the rounded content box.
//!
//! The shaders are GLSL ES 1.00 and written for NVIDIA's strict compiler too: float literals
//! everywhere, no implicit int/float conversion, no redefined builtins, `highp` when available.
//! If they fail to compile, uwc logs it once and draws square solid borders instead.

use smithay::{
    backend::renderer::{
        element::{
            surface::WaylandSurfaceRenderElement, utils::RescaleRenderElement, Element, Id, Kind, RenderElement,
            UnderlyingStorage,
        },
        gles::{
            GlesError, GlesFrame, GlesPixelProgram, GlesRenderer, GlesTexProgram, Uniform, UniformName, UniformType,
            UniformValue,
        },
        utils::{CommitCounter, DamageSet, OpaqueRegions},
    },
    utils::{Buffer, Physical, Point, Rectangle, Scale, Size, Transform},
};

use crate::{
    config::{Color, Gradient},
    render::UwcElement,
};

const PRECISION: &str = r#"
#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif
"#;

const SDF: &str = r#"
// Signed distance from p to a rounded rectangle centred at the origin.
float rrect(vec2 p, vec2 halfsize, float r) {
    vec2 q = abs(p) - halfsize + vec2(r);
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2(0.0))) - r;
}
"#;

const BOX: &str = r#"
uniform float alpha;
uniform vec2 size;
varying vec2 v_coords;
#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

uniform vec4 u_box;       // the box inside the element: x, y, w, h (px)
uniform float u_radius;   // its outer corner radius
uniform float u_width;    // ring width; 0 fills the box
uniform float u_ncolors;  // 1 to 6
uniform float u_angle;    // radians, CSS convention: 0 = first color at the bottom, pi/2 = at the left
uniform vec4 u_c0;
uniform vec4 u_c1;
uniform vec4 u_c2;
uniform vec4 u_c3;
uniform vec4 u_c4;
uniform vec4 u_c5;

vec4 pick(float i) {
    if (i < 0.5) return u_c0;
    if (i < 1.5) return u_c1;
    if (i < 2.5) return u_c2;
    if (i < 3.5) return u_c3;
    if (i < 4.5) return u_c4;
    return u_c5;
}

vec4 color_at(float t) {
    if (u_ncolors < 1.5) return u_c0;
    float s = t * (u_ncolors - 1.0);
    float i = min(floor(s), u_ncolors - 2.0);
    return mix(pick(i), pick(i + 1.0), s - i);
}

void main() {
    vec2 p = v_coords * size;
    vec2 halfsize = u_box.zw * 0.5;
    vec2 q = p - (u_box.xy + halfsize);
    float cover = clamp(0.5 - rrect(q, halfsize, u_radius), 0.0, 1.0);
    if (u_width > 0.0) {
        float inner_r = max(u_radius - u_width, 0.0);
        float inner = clamp(0.5 - rrect(q, halfsize - vec2(u_width), inner_r), 0.0, 1.0);
        cover = cover * (1.0 - inner);
    }
    vec2 dir = vec2(sin(u_angle), -cos(u_angle));
    float len = abs(halfsize.x * dir.x) + abs(halfsize.y * dir.y);
    float t = clamp(0.5 + dot(q, dir) / (2.0 * max(len, 0.001)), 0.0, 1.0);
    vec4 c = color_at(t);
    vec4 color = vec4(c.rgb * c.a, c.a) * cover * alpha;
#if defined(DEBUG_FLAGS)
    if (tint == 1.0) color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif
    gl_FragColor = color;
}
"#;

const SHADOW: &str = r#"
uniform float alpha;
uniform vec2 size;
varying vec2 v_coords;
#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

uniform vec4 u_box;          // the box casting the shadow, inside the element
uniform float u_radius;
uniform float u_sigma;
uniform vec4 u_color;
uniform vec4 u_clip;         // the window itself: no shadow under it
uniform float u_clip_radius;

float gaussian(float x, float sigma) {
    return exp(-(x * x) / (2.0 * sigma * sigma)) / (2.5066282746 * sigma);
}

vec2 erf2(vec2 x) {
    vec2 s = sign(x);
    vec2 a = abs(x);
    x = 1.0 + (0.278393 + (0.230389 + 0.078108 * (a * a)) * a) * a;
    x = x * x;
    return s - s / (x * x);
}

float shadow_x(float x, float y, float sigma, float corner, vec2 halfsize) {
    float delta = min(halfsize.y - corner - abs(y), 0.0);
    float curved = halfsize.x - corner + sqrt(max(0.0, corner * corner - delta * delta));
    vec2 integral = 0.5 + 0.5 * erf2((x + vec2(-curved, curved)) * (0.70710678 / sigma));
    return integral.y - integral.x;
}

float shadow(vec2 lower, vec2 upper, vec2 p, float sigma, float corner) {
    vec2 center = (lower + upper) * 0.5;
    vec2 halfsize = (upper - lower) * 0.5;
    p = p - center;
    float low = p.y - halfsize.y;
    float high = p.y + halfsize.y;
    float start = clamp(-3.0 * sigma, low, high);
    float end = clamp(3.0 * sigma, low, high);
    float step = (end - start) / 4.0;
    float y = start + step * 0.5;
    float value = 0.0;
    for (int i = 0; i < 4; i++) {
        value += shadow_x(p.x, p.y - y, sigma, corner, halfsize) * gaussian(y, sigma) * step;
        y += step;
    }
    return value;
}

void main() {
    vec2 p = v_coords * size;
    float a = shadow(u_box.xy, u_box.xy + u_box.zw, p, max(u_sigma, 0.001), u_radius);
    vec2 hc = u_clip.zw * 0.5;
    float under = clamp(0.5 - rrect(p - (u_clip.xy + hc), hc, u_clip_radius), 0.0, 1.0);
    a = a * (1.0 - under);
    vec4 color = vec4(u_color.rgb * u_color.a, u_color.a) * a * alpha;
#if defined(DEBUG_FLAGS)
    if (tint == 1.0) color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif
    gl_FragColor = color;
}
"#;

const ROUND: &str = r#"
//_DEFINES

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif

#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
varying vec2 v_coords;
#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

uniform vec4 u_dst;      // this surface on the output: x, y, w, h (physical px)
uniform vec4 u_rect;     // the window's content box on the output
uniform float u_radius;

float rrect(vec2 p, vec2 halfsize, float r) {
    vec2 q = abs(p) - halfsize + vec2(r);
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2(0.0))) - r;
}

void main() {
    vec4 color = texture2D(tex, v_coords);
#if defined(NO_ALPHA)
    color = vec4(color.rgb, 1.0);
#endif
    vec2 p = u_dst.xy + v_coords * u_dst.zw;
    vec2 halfsize = u_rect.zw * 0.5;
    float d = rrect(p - (u_rect.xy + halfsize), halfsize, u_radius);
    color = color * clamp(0.5 - d, 0.0, 1.0) * alpha;
#if defined(DEBUG_FLAGS)
    if (tint == 1.0) color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif
    gl_FragColor = color;
}
"#;

#[derive(Clone)]
pub struct Shaders {
    pub boxes: GlesPixelProgram,
    pub shadow: GlesPixelProgram,
    pub round: GlesTexProgram,
}

/// Cached per GL context; `None` once compiling failed.
struct ShaderCache(Option<Shaders>);

impl Shaders {
    pub fn get(renderer: &mut GlesRenderer) -> Option<Shaders> {
        if renderer.egl_context().user_data().get::<ShaderCache>().is_none() {
            let compiled = Shaders::compile(renderer);
            if let Err(e) = &compiled {
                tracing::error!("decoration shaders failed to compile, drawing plain borders: {e}");
            }
            renderer.egl_context().user_data().insert_if_missing(|| ShaderCache(compiled.ok()));
        }
        renderer.egl_context().user_data().get::<ShaderCache>().and_then(|c| c.0.clone())
    }

    fn compile(r: &mut GlesRenderer) -> Result<Shaders, GlesError> {
        let f4 = |n| UniformName::new(n, UniformType::_4f);
        let f1 = |n| UniformName::new(n, UniformType::_1f);
        let boxes = r.compile_custom_pixel_shader(
            format!("{PRECISION}{SDF}{BOX}"),
            &[
                f4("u_box"),
                f1("u_radius"),
                f1("u_width"),
                f1("u_ncolors"),
                f1("u_angle"),
                f4("u_c0"),
                f4("u_c1"),
                f4("u_c2"),
                f4("u_c3"),
                f4("u_c4"),
                f4("u_c5"),
            ],
        )?;
        let shadow = r.compile_custom_pixel_shader(
            format!("{PRECISION}{SDF}{SHADOW}"),
            &[f4("u_box"), f1("u_radius"), f1("u_sigma"), f4("u_color"), f4("u_clip"), f1("u_clip_radius")],
        )?;
        let round = r.compile_custom_texture_shader(ROUND, &[f4("u_dst"), f4("u_rect"), f1("u_radius")])?;
        Ok(Shaders { boxes, shadow, round })
    }
}

fn u4(name: &'static str, v: [f32; 4]) -> Uniform<'static> {
    Uniform::new(name, UniformValue::_4f(v[0], v[1], v[2], v[3]))
}

fn u1(name: &'static str, v: f32) -> Uniform<'static> {
    Uniform::new(name, UniformValue::_1f(v))
}

/// A shader-drawn rectangle in physical output px, with an id that stays the same across frames so
/// the damage tracker only redraws it when it moved or `commit` changed.
#[derive(Debug)]
pub struct ShaderElement {
    id: Id,
    commit: CommitCounter,
    geo: Rectangle<i32, Physical>,
    program: GlesPixelProgram,
    uniforms: Vec<Uniform<'static>>,
}

impl ShaderElement {
    /// A filled box or (with `width` > 0) a ring, covering all of `geo`.
    pub fn rounded_box(
        shaders: &Shaders,
        id: &Id,
        commit: CommitCounter,
        geo: Rectangle<i32, Physical>,
        radius: f64,
        width: f64,
        paint: &Paint,
    ) -> ShaderElement {
        let (colors, angle) = match paint {
            Paint::Solid(c) => (vec![*c], 0.0),
            Paint::Gradient(g, extra) => (g.colors.clone(), (g.angle + extra).to_radians()),
        };
        let at = |i: usize| colors.get(i).copied().unwrap_or(*colors.last().unwrap());
        let r = radius.min(geo.size.w as f64 / 2.0).min(geo.size.h as f64 / 2.0).max(0.0);
        let uniforms = vec![
            u4("u_box", [0.0, 0.0, geo.size.w as f32, geo.size.h as f32]),
            u1("u_radius", r as f32),
            u1("u_width", width as f32),
            u1("u_ncolors", colors.len().clamp(1, 6) as f32),
            u1("u_angle", angle as f32),
            u4("u_c0", at(0)),
            u4("u_c1", at(1)),
            u4("u_c2", at(2)),
            u4("u_c3", at(3)),
            u4("u_c4", at(4)),
            u4("u_c5", at(5)),
        ];
        ShaderElement { id: id.clone(), commit, geo, program: shaders.boxes.clone(), uniforms }
    }

    /// The shadow of `box_` (the window's outer box, already offset and spread), blurred by `sigma`,
    /// with `clip` (the window's box, not offset) cut out. The element extends `reach` px around it.
    #[allow(clippy::too_many_arguments)]
    pub fn shadow(
        shaders: &Shaders,
        id: &Id,
        commit: CommitCounter,
        box_: Rectangle<f64, Physical>,
        radius: f64,
        sigma: f64,
        reach: f64,
        color: Color,
        clip: Rectangle<f64, Physical>,
        clip_radius: f64,
    ) -> ShaderElement {
        let geo = Rectangle::new(
            Point::from((box_.loc.x - reach, box_.loc.y - reach)),
            Size::from((box_.size.w + 2.0 * reach, box_.size.h + 2.0 * reach)),
        )
        .to_i32_round();
        let o = geo.loc.to_f64();
        let r = radius.min(box_.size.w / 2.0).min(box_.size.h / 2.0).max(0.0);
        let uniforms = vec![
            u4("u_box", [(box_.loc.x - o.x) as f32, (box_.loc.y - o.y) as f32, box_.size.w as f32, box_.size.h as f32]),
            u1("u_radius", r as f32),
            u1("u_sigma", sigma as f32),
            u4("u_color", color),
            u4(
                "u_clip",
                [(clip.loc.x - o.x) as f32, (clip.loc.y - o.y) as f32, clip.size.w as f32, clip.size.h as f32],
            ),
            u1("u_clip_radius", clip_radius.min(clip.size.w / 2.0).min(clip.size.h / 2.0).max(0.0) as f32),
        ];
        ShaderElement { id: id.clone(), commit, geo, program: shaders.shadow.clone(), uniforms }
    }
}

/// What a box is painted with. The gradient's extra angle (degrees) is the border spin.
pub enum Paint {
    Solid(Color),
    Gradient(Gradient, f64),
}

impl Element for ShaderElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size(Size::<f64, Buffer>::from((self.geo.size.w as f64, self.geo.size.h as f64)))
    }

    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.geo
    }

    fn kind(&self) -> Kind {
        Kind::Unspecified
    }
}

impl RenderElement<GlesRenderer> for ShaderElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        frame.render_pixel_shader_to(
            &self.program,
            src,
            dst,
            Size::<i32, Buffer>::from((self.geo.size.w, self.geo.size.h)),
            Some(damage),
            1.0,
            &self.uniforms,
        )
    }

    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

type Surface = RescaleRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>;

/// A window surface clipped to the rounded content box.
pub struct RoundedElement {
    inner: Surface,
    program: GlesTexProgram,
    /// The content box on the output, physical px.
    rect: Rectangle<f64, Physical>,
    radius: f64,
}

impl RoundedElement {
    /// `inner` as an element, clipped by the shader if it reaches into a rounded corner of `rect` (or
    /// past its edges); a surface well inside the content box is drawn as it is.
    pub fn wrap(
        inner: Surface,
        shaders: Option<&Shaders>,
        rect: Rectangle<f64, Physical>,
        radius: f64,
        scale: Scale<f64>,
    ) -> UwcElement {
        let Some(s) = shaders.filter(|_| radius > 0.0) else { return UwcElement::from(inner) };
        let g = inner.geometry(scale).to_f64();
        let r = radius;
        let corners = [
            Rectangle::new(rect.loc, (r, r).into()),
            Rectangle::new((rect.loc.x + rect.size.w - r, rect.loc.y).into(), (r, r).into()),
            Rectangle::new((rect.loc.x, rect.loc.y + rect.size.h - r).into(), (r, r).into()),
            Rectangle::new((rect.loc.x + rect.size.w - r, rect.loc.y + rect.size.h - r).into(), (r, r).into()),
        ];
        if !rect.contains_rect(g) || corners.iter().any(|c| c.overlaps(g)) {
            UwcElement::from(RoundedElement { inner, program: s.round.clone(), rect, radius })
        } else {
            UwcElement::from(inner)
        }
    }
}

impl Element for RoundedElement {
    fn id(&self) -> &Id {
        self.inner.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }

    fn transform(&self) -> Transform {
        self.inner.transform()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }

    fn damage_since(&self, scale: Scale<f64>, commit: Option<CommitCounter>) -> DamageSet<i32, Physical> {
        self.inner.damage_since(scale, commit)
    }

    // The corners are transparent now, so nothing here may hide what is behind it.
    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        OpaqueRegions::default()
    }

    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }

    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}

impl RenderElement<GlesRenderer> for RoundedElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        let r = self.rect;
        let radius = self.radius.min(r.size.w / 2.0).min(r.size.h / 2.0);
        frame.override_default_tex_program(
            self.program.clone(),
            vec![
                u4("u_dst", [dst.loc.x as f32, dst.loc.y as f32, dst.size.w as f32, dst.size.h as f32]),
                u4("u_rect", [r.loc.x as f32, r.loc.y as f32, r.size.w as f32, r.size.h as f32]),
                u1("u_radius", radius as f32),
            ],
        );
        let res = self.inner.draw(frame, src, dst, damage, opaque_regions);
        frame.clear_tex_program_override();
        res
    }

    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        // Not usable for direct scan-out: the corners would be lost.
        None
    }
}

/// The angle (degrees) a spinning gradient border has turned by at `now_ms`: one full turn every
/// `|spin_ms|` ms, clockwise for positive values.
pub fn spin_degrees(spin_ms: f64, now_ms: f64) -> f64 {
    if spin_ms == 0.0 {
        return 0.0;
    }
    (now_ms / spin_ms.abs()).rem_euclid(1.0) * 360.0 * spin_ms.signum()
}

#[cfg(test)]
mod tests {
    use super::spin_degrees;

    #[test]
    fn spin_turns_once_per_period() {
        assert_eq!(spin_degrees(0.0, 1234.0), 0.0);
        assert!((spin_degrees(1000.0, 250.0) - 90.0).abs() < 1e-9);
        assert!((spin_degrees(1000.0, 1250.0) - 90.0).abs() < 1e-9);
        assert!((spin_degrees(-1000.0, 250.0) + 90.0).abs() < 1e-9);
    }
}
