//! Background blur: a window with `blur-radius` shows a blurred copy of whatever is behind it through
//! its translucent parts (a terminal at `opacity 0.8`, a bar).
//!
//! smithay has no blur, and GLES 2 cannot read the framebuffer it is drawing into, so it is done in a
//! pre-pass, before the frame is drawn (`Uwc::fill_blurs`, called at the end of `scene_elements`):
//!
//! 1. `render_elements` leaves a `BlurElement` placeholder under each blurred window's content, border and
//!    backdrop (and over its shadow). It knows where the window is and how far to blur, not yet what is behind it.
//! 2. The placeholders are visited from the back of the scene to the front. For each, everything behind it
//!    in the element list (windows, layer surfaces, the background, and the blurs already made, so blurred windows
//!    stacked on each other blur through each other) is drawn into an offscreen texture covering the window plus a
//!    margin the blur reaches into, over the root color.
//! 3. That texture is blurred with a dual Kawase filter (Marius Bjørge, "Bandwidth-Efficient Rendering"): a few
//!    passes each halving the size, then as many doubling it again. It is cheap (the work is done at low
//!    resolution), looks like a gaussian, and the radius is the offset between taps, so any radius works without
//!    more passes than `log2(radius)`.
//! 4. The last pass writes just the window's box into its own texture, which the element draws, clipped to the
//!    rounded corners, in the window's place under its content.
//!
//! Costs: one pass over the elements behind each blurred window (only those that overlap it) and the filter, but
//! only when something the blurred picture depends on changed: each element remembers a key of its own box, its
//! options and the ids, commits and geometry of everything behind it, and skips the passes while the key holds
//! (so a steady scene costs nothing). Textures are kept between frames (`BlurCache`) so nothing is allocated in a
//! steady state either.
//!
//! Layer surfaces that ask through ext-background-effect-v1 get the same blur (background_effect.rs), and a
//! closing window's ghost is blurred while it fades. The blur is of the output's scene as drawn, so it does not
//! include the window's own shadow inside its box (the shadow shader cuts that out) but does include it in the
//! margin.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            damage::OutputDamageTracker,
            element::{
                utils::{Relocate, RelocateRenderElement},
                Element, Id, Kind, RenderElement, UnderlyingStorage,
            },
            gles::{GlesError, GlesFrame, GlesRenderer, GlesTexProgram, GlesTexture, Uniform, UniformValue},
            utils::{CommitCounter, OpaqueRegions},
            Bind, Color32F, Frame, Offscreen, Renderer, Texture,
        },
    },
    utils::{Buffer, Physical, Point, Rectangle, Scale, Size, Transform},
};

use crate::{
    deco::{u1, u4, Shaders},
    render::UwcElement,
    state::Uwc,
};

/// The most a blur reaches, in physical px; keeps the offscreen textures a sane size.
const MAX_MARGIN: i32 = 256;
/// The most a blur texture may cover (one side, physical px).
const MAX_SIDE: i32 = 8192;

/// What one surface's blur looked like last frame: the texture, what it was made from, and the
/// commit it was drawn at, so an unchanged backdrop is not recomputed or even redrawn.
struct BlurState {
    key: u64,
    blurred: GlesTexture,
    commit: CommitCounter,
}

/// Textures kept from frame to frame, per surface and filter level, plus the ids of the elements
/// and each surface's last blurred picture.
#[derive(Default)]
pub struct BlurCache {
    textures: HashMap<(u32, u8), GlesTexture>,
    state: HashMap<u32, BlurState>,
    ids: HashMap<u32, Id>,
    commit: CommitCounter,
}

impl BlurCache {
    /// A texture of exactly `size` for `(window, level)`, the one from last frame if it still fits.
    fn texture(
        &mut self,
        r: &mut GlesRenderer,
        window: u32,
        level: u8,
        size: Size<i32, Buffer>,
    ) -> Result<GlesTexture, String> {
        if let Some(t) = self.textures.get(&(window, level)) {
            if t.size() == size {
                return Ok(t.clone());
            }
        }
        let t: GlesTexture = r.create_buffer(Fourcc::Abgr8888, size).map_err(|e| format!("blur texture: {e}"))?;
        self.textures.insert((window, level), t.clone());
        Ok(t)
    }

    /// Make sure each of `windows` has a stable element id (see `new`, which only reads them).
    pub fn ensure(&mut self, windows: &[u32]) {
        for &w in windows {
            self.ids.entry(w).or_insert_with(Id::new);
        }
    }

    fn id(&self, window: u32) -> Id {
        self.ids.get(&window).cloned().unwrap_or_else(Id::new)
    }

    /// Forget the surfaces that are not blurred (any more).
    pub fn keep(&mut self, windows: &[u32]) {
        self.textures.retain(|(w, _), _| windows.contains(w));
        self.state.retain(|w, _| windows.contains(w));
        self.ids.retain(|w, _| windows.contains(w));
    }
}

/// How many halving passes and what tap offset give a blur reaching about `radius` physical px.
pub fn plan(radius: f64) -> (u8, f32) {
    let radius = if radius.is_finite() { radius.clamp(1.0, MAX_MARGIN as f64) } else { 1.0 };
    let passes = radius.log2().round().clamp(1.0, 6.0) as u8;
    // A dual Kawase filter with offset o and n passes reaches roughly o * 2^n px.
    let offset = (radius / (1u32 << passes) as f64).max(0.5) as f32;
    (passes, offset)
}

/// The margin around the window's box that the blur needs sampled, physical px.
fn margin(radius: f64) -> i32 {
    (radius.ceil() as i32 * 2 + 2).clamp(2, MAX_MARGIN * 2)
}

/// How a blurred backdrop is made: the reach, the corner clip, the fade, and the light tweaks.
#[derive(Clone, Copy)]
pub struct BlurParams {
    pub radius: f64,
    pub corner: f64,
    pub alpha: f32,
    pub brightness: f32,
    pub saturation: f32,
    pub noise: f32,
}

impl BlurParams {
    pub fn from_deco(o: &crate::rules::Deco, radius: f64, corner: f64, alpha: f32) -> BlurParams {
        BlurParams {
            radius,
            corner,
            alpha,
            brightness: o.blur_brightness as f32,
            saturation: o.blur_saturation as f32,
            noise: o.blur_noise as f32,
        }
    }

    fn tweaked(&self) -> bool {
        self.brightness != 1.0 || self.saturation != 1.0 || self.noise != 0.0
    }
}

/// The blurred backdrop of one window (or layer surface, or closing ghost): a placeholder until
/// `fill` has drawn what is behind it.
pub struct BlurElement {
    id: Id,
    commit: CommitCounter,
    window: u32,
    /// The window's content box, physical px: where this element is drawn.
    geo: Rectangle<i32, Physical>,
    /// The box plus the margin: what is drawn behind it and blurred.
    region: Rectangle<i32, Physical>,
    p: BlurParams,
    round: GlesTexProgram,
    blurred: Option<GlesTexture>,
    /// What the current `blurred` was made from; while it holds, `fill` does nothing.
    key: Option<u64>,
}

impl BlurElement {
    /// A placeholder for a window whose content box is `geo`, blurred per `p`.
    pub fn new(
        cache: &BlurCache,
        shaders: &Shaders,
        window: u32,
        geo: Rectangle<i32, Physical>,
        p: BlurParams,
    ) -> BlurElement {
        let m = margin(p.radius);
        let region =
            Rectangle::new((geo.loc.x - m, geo.loc.y - m).into(), (geo.size.w + 2 * m, geo.size.h + 2 * m).into());
        // Adopt last frame's picture: while its key still holds, `fill` does not touch it and the
        // damage tracker sees no new commit, so a steady scene costs no blur work at all.
        let (blurred, key, commit) = match cache.state.get(&window) {
            Some(s) => (Some(s.blurred.clone()), Some(s.key), s.commit),
            None => (None, None, cache.commit),
        };
        BlurElement { id: cache.id(window), commit, window, geo, region, p, round: shaders.round.clone(), blurred, key }
    }

    /// Everything the blurred picture depends on, hashed: this element's box and options, and the
    /// id, commit and geometry of every element behind it that overlaps the region.
    fn key(&self, behind: &[UwcElement], scale: f64, clear: [f32; 4]) -> u64 {
        let mut h = DefaultHasher::new();
        self.window.hash(&mut h);
        (self.geo.loc.x, self.geo.loc.y, self.geo.size.w, self.geo.size.h).hash(&mut h);
        self.p.radius.to_bits().hash(&mut h);
        self.p.corner.to_bits().hash(&mut h);
        self.p.alpha.to_bits().hash(&mut h);
        self.p.brightness.to_bits().hash(&mut h);
        self.p.saturation.to_bits().hash(&mut h);
        self.p.noise.to_bits().hash(&mut h);
        scale.to_bits().hash(&mut h);
        for c in clear {
            c.to_bits().hash(&mut h);
        }
        for e in behind.iter().filter(|e| e.geometry(Scale::from(scale)).overlaps(self.region)) {
            e.id().hash(&mut h);
            format!("{:?}", e.current_commit()).hash(&mut h);
            let g = e.geometry(Scale::from(scale));
            (g.loc.x, g.loc.y, g.size.w, g.size.h).hash(&mut h);
        }
        h.finish()
    }

    /// Draw what is behind (`behind`, front to back, in output px) and blur it. Does nothing while
    /// the picture would be the same as last frame's (`key`).
    fn fill(
        &mut self,
        r: &mut GlesRenderer,
        cache: &mut BlurCache,
        shaders: &Shaders,
        behind: &[UwcElement],
        scale: f64,
        clear: [f32; 4],
    ) -> Result<(), String> {
        let Some((down, up)) = shaders.blur.as_ref() else { return Ok(()) };
        if self.geo.size.w < 1 || self.geo.size.h < 1 || self.region.size.w > MAX_SIDE || self.region.size.h > MAX_SIDE
        {
            return Ok(());
        }
        let key = self.key(behind, scale, clear);
        if self.key == Some(key) {
            return Ok(());
        }
        let size = |s: Size<i32, Physical>| Size::<i32, Buffer>::from((s.w, s.h));
        let (passes, offset) = plan(self.p.radius);

        // 1. The scene behind the window, over the root color.
        let mut scene = cache.texture(r, self.window, 0, size(self.region.size))?;
        {
            let at = Point::<i32, Physical>::from((-self.region.loc.x, -self.region.loc.y));
            let els: Vec<RelocateRenderElement<&UwcElement>> = behind
                .iter()
                .filter(|e| e.geometry(Scale::from(scale)).overlaps(self.region))
                .map(|e| RelocateRenderElement::from_element(e, at, Relocate::Relative))
                .collect();
            let mut fb = r.bind(&mut scene).map_err(|e| format!("blur bind: {e}"))?;
            let mut tracker = OutputDamageTracker::new(self.region.size, scale, Transform::Normal);
            tracker.render_output(r, &mut fb, 0, &els, clear).map_err(|e| format!("blur scene: {e:?}"))?;
        }

        // 2. Down: each pass half the size of the one before.
        let mut sizes = vec![size(self.region.size)];
        for _ in 0..passes {
            let s = *sizes.last().unwrap();
            sizes.push(Size::from(((s.w + 1) / 2, (s.h + 1) / 2)).max_with_one());
        }
        let mut chain: Vec<GlesTexture> = vec![scene];
        for level in 1..=passes {
            let mut dst = cache.texture(r, self.window, level, sizes[level as usize])?;
            let src = chain[level as usize - 1].clone();
            let half = (0.5 / dst.size().w as f32, 0.5 / dst.size().h as f32);
            pass(r, &src, full(src.size()), &mut dst, down, &blur_uniforms(half, offset))?;
            chain.push(dst);
        }

        // 3. Up, to the level above the full size; the last pass writes the window's box only.
        let mut current = chain[passes as usize].clone();
        for level in (1..passes).rev() {
            let mut dst = cache.texture(r, self.window, 16 + level, sizes[level as usize])?;
            let half = (0.5 / current.size().w as f32, 0.5 / current.size().h as f32);
            pass(r, &current, full(current.size()), &mut dst, up, &blur_uniforms(half, offset))?;
            current = dst;
        }
        let mut out = cache.texture(r, self.window, 32, size(self.geo.size))?;
        let (sw, sh) = (current.size().w as f64, current.size().h as f64);
        let (kx, ky) = (sw / self.region.size.w as f64, sh / self.region.size.h as f64);
        let crop = Rectangle::<f64, Buffer>::new(
            ((self.geo.loc.x - self.region.loc.x) as f64 * kx, (self.geo.loc.y - self.region.loc.y) as f64 * ky).into(),
            (self.geo.size.w as f64 * kx, self.geo.size.h as f64 * ky).into(),
        );
        let half = (0.5 / sw as f32, 0.5 / sh as f32);
        pass(r, &current, crop, &mut out, up, &blur_uniforms(half, offset))?;

        // 4. The light tweaks (brightness / saturation / noise), one more pass over the box.
        if self.p.tweaked() {
            if let Some(tweak) = shaders.tweak.as_ref() {
                let mut done = cache.texture(r, self.window, 33, size(self.geo.size))?;
                pass(
                    r,
                    &out,
                    full(out.size()),
                    &mut done,
                    tweak,
                    &[
                        u1("u_brightness", self.p.brightness),
                        u1("u_saturation", self.p.saturation),
                        u1("u_noise", self.p.noise),
                        u1("u_seed", self.window as f32),
                    ],
                )?;
                out = done;
            }
        }

        cache.commit.increment();
        self.commit = cache.commit;
        self.key = Some(key);
        cache.state.insert(self.window, BlurState { key, blurred: out.clone(), commit: self.commit });
        self.blurred = Some(out);
        Ok(())
    }
}

fn blur_uniforms(halfpixel: (f32, f32), offset: f32) -> [Uniform<'static>; 2] {
    [Uniform::new("u_halfpixel", UniformValue::_2f(halfpixel.0, halfpixel.1)), u1("u_offset", offset)]
}

trait MaxWithOne {
    fn max_with_one(self) -> Self;
}

impl MaxWithOne for Size<i32, Buffer> {
    fn max_with_one(self) -> Self {
        Size::from((self.w.max(1), self.h.max(1)))
    }
}

fn full(s: Size<i32, Buffer>) -> Rectangle<f64, Buffer> {
    Rectangle::from_size((s.w as f64, s.h as f64).into())
}

/// One filter pass: `src` (the part `src_rect`) drawn through `program` with `uniforms` into all of `dst`.
fn pass(
    r: &mut GlesRenderer,
    src: &GlesTexture,
    src_rect: Rectangle<f64, Buffer>,
    dst: &mut GlesTexture,
    program: &GlesTexProgram,
    uniforms: &[Uniform<'_>],
) -> Result<(), String> {
    let s = dst.size();
    let size = Size::<i32, Physical>::from((s.w, s.h));
    let all = Rectangle::from_size(size);
    let mut fb = r.bind(dst).map_err(|e| format!("blur bind: {e}"))?;
    let mut frame = r.render(&mut fb, size, Transform::Normal).map_err(|e| format!("blur frame: {e}"))?;
    frame.clear(Color32F::TRANSPARENT, &[all]).map_err(|e| format!("blur clear: {e}"))?;
    frame
        .render_texture_from_to(src, src_rect, all, &[all], &[], Transform::Normal, 1.0, Some(program), uniforms)
        .map_err(|e| format!("blur pass: {e}"))?;
    // Same GL context, so the next pass is ordered after this one without waiting on the fence.
    let _sync = frame.finish().map_err(|e| format!("blur finish: {e}"))?;
    Ok(())
}

impl Uwc {
    /// The texture keys of everything that can be blurred right now (windows on any monitor, layer
    /// surfaces that asked for a background effect, closing ghosts), so `BlurCache` can keep their
    /// ids, textures and pictures and forget the rest — once per scene, not per monitor.
    pub fn blur_keys(&self) -> Vec<u32> {
        let mut keys: Vec<u32> = self
            .clients
            .iter()
            .filter(|c| self.visible(c) && !c.fullscreen && self.deco_for(c).blur_radius > 0.0)
            .map(|c| c.id)
            .collect();
        keys.extend(self.bg_effect.values().map(|b| b.key));
        keys.extend(self.ghosts.iter().filter(|g| g.deco.blur_radius > 0.0).map(|g| g.key));
        keys
    }

    /// Make every `BlurElement` in `scene` (front to back) show what is behind it. Placeholders are visited
    /// from the back so a blurred window stacked on another one blurs the other one's blur too.
    pub fn fill_blurs(&mut self, r: &mut GlesRenderer, scene: &mut [UwcElement], scale: f64, clear: [f32; 4]) {
        let mut windows = Vec::new();
        for e in scene.iter() {
            if let UwcElement::Blur(b) = e {
                windows.push(b.window);
            }
        }
        if windows.is_empty() {
            return;
        }
        let Some(shaders) = Shaders::get(r) else { return };
        for i in (0..scene.len()).rev() {
            let (head, behind) = scene.split_at_mut(i + 1);
            let UwcElement::Blur(b) = &mut head[i] else { continue };
            if let Err(e) = b.fill(r, &mut self.blur, &shaders, behind, scale, clear) {
                tracing::debug!("blur for window {}: {e}", b.window);
            }
        }
    }
}

impl Element for BlurElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        match &self.blurred {
            Some(t) => full(t.size()),
            None => Rectangle::from_size((self.geo.size.w as f64, self.geo.size.h as f64).into()),
        }
    }

    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.geo
    }

    // The corners are transparent and the window may fade: nothing here hides what is behind it.
    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        OpaqueRegions::default()
    }

    fn alpha(&self) -> f32 {
        self.p.alpha
    }

    fn kind(&self) -> Kind {
        Kind::Unspecified
    }
}

impl RenderElement<GlesRenderer> for BlurElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        let Some(tex) = &self.blurred else { return Ok(()) };
        // The same corner clip the window's content gets (see `RoundedElement`).
        let radius = self.p.corner.min(dst.size.w as f64 / 2.0).min(dst.size.h as f64 / 2.0);
        frame.override_default_tex_program(
            self.round.clone(),
            vec![
                u4("u_dst", [dst.loc.x as f32, dst.loc.y as f32, dst.size.w as f32, dst.size.h as f32]),
                u4("u_rect", [dst.loc.x as f32, dst.loc.y as f32, dst.size.w as f32, dst.size.h as f32]),
                u1("u_radius", radius as f32),
            ],
        );
        let res = frame.render_texture_from_to(tex, src, dst, damage, &[], Transform::Normal, self.p.alpha, None, &[]);
        frame.clear_tex_program_override();
        res
    }

    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plan_reaches_about_the_radius() {
        for radius in [1.0, 2.0, 4.0, 8.0, 16.0, 40.0, 100.0, 256.0] {
            let (passes, offset) = plan(radius);
            assert!((1..=6).contains(&passes), "{radius}: {passes} passes");
            let reach = offset as f64 * (1u32 << passes) as f64;
            assert!(
                reach >= radius * 0.5 && reach <= radius * 2.0,
                "{radius}: reaches {reach} ({passes} passes, {offset})"
            );
        }
    }

    #[test]
    fn silly_radii_stay_bounded() {
        for radius in [0.0, -5.0, 1e9, f64::NAN] {
            let (passes, offset) = plan(radius);
            assert!((1..=6).contains(&passes) && offset.is_finite() && offset > 0.0, "{radius}");
        }
        assert!(margin(1e6) <= MAX_MARGIN * 2);
    }
}
