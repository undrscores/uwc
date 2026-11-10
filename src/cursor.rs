//! The pointer image. A real session has no host cursor, so uwc draws it: an xcursor theme image
//! (the config's `cursor-theme` / `cursor-size`), a built-in arrow when no theme has one, or the
//! surface a client set with `wl_pointer.set_cursor`.

use std::collections::HashMap;

use smithay::{
    backend::renderer::gles::GlesRenderer,
    backend::{
        allocator::Fourcc,
        renderer::element::{
            memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
            surface::{render_elements_from_surface_tree, WaylandSurfaceRenderElement},
            utils::RescaleRenderElement,
            Kind,
        },
    },
    input::pointer::{CursorImageStatus, CursorImageSurfaceData},
    utils::{IsAlive, Physical, Point, Scale, Transform},
    wayland::compositor::with_states,
};

use crate::{render::UwcElement, state::Uwc};

pub struct CursorImage {
    pub buffer: MemoryRenderBuffer,
    /// In image pixels.
    pub hotspot: (i32, i32),
}

pub struct CursorTheme {
    name: String,
    size: u32,
    theme: xcursor::CursorTheme,
    /// (icon name, pixel size) -> image; `None` means the theme has no such icon.
    cache: HashMap<(String, u32), Option<CursorImage>>,
    fallback: HashMap<u32, CursorImage>,
}

impl CursorTheme {
    pub fn new(name: &str, size: u32) -> CursorTheme {
        let theme_name = theme_name(name);
        CursorTheme {
            theme: xcursor::CursorTheme::load(&theme_name),
            name: theme_name,
            size,
            cache: HashMap::new(),
            fallback: HashMap::new(),
        }
    }

    /// Reload when the config changed the theme or size.
    pub fn configure(&mut self, name: &str, size: u32) {
        if theme_name(name) != self.name || size != self.size {
            *self = CursorTheme::new(name, size);
        }
    }

    /// The image for `icon` at `scale`, falling back to the theme's default, then to a built-in arrow.
    pub fn image(&mut self, icon: &str, scale: f64) -> &CursorImage {
        let px = ((self.size as f64) * scale).round().max(1.0) as u32;
        for name in [icon, "default", "left_ptr"] {
            let key = (name.to_string(), px);
            if !self.cache.contains_key(&key) {
                let img = load(&self.theme, name, px);
                self.cache.insert(key.clone(), img);
            }
            if self.cache[&key].is_some() {
                return self.cache[&key].as_ref().unwrap();
            }
        }
        self.fallback.entry(px).or_insert_with(|| arrow(px))
    }
}

fn theme_name(name: &str) -> String {
    if !name.is_empty() {
        return name.into();
    }
    std::env::var("XCURSOR_THEME").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "default".into())
}

fn load(theme: &xcursor::CursorTheme, name: &str, px: u32) -> Option<CursorImage> {
    let path = theme.load_icon(name)?;
    let data = std::fs::read(path).ok()?;
    let images = xcursor::parser::parse_xcursor(&data)?;
    // The nominal size nearest to what we want; the first frame of an animated cursor.
    let best = images.iter().min_by_key(|i| (i.size as i64 - px as i64).abs())?;
    let img = images.iter().find(|i| i.size == best.size)?;
    let buffer = MemoryRenderBuffer::from_slice(
        &img.pixels_rgba,
        Fourcc::Abgr8888,
        (img.width as i32, img.height as i32),
        1,
        Transform::Normal,
        None,
    );
    Some(CursorImage { buffer, hotspot: (img.xhot as i32, img.yhot as i32) })
}

/// A plain arrow (white with a black outline) for systems without a cursor theme.
fn arrow(px: u32) -> CursorImage {
    let n = px.max(8) as i32;
    let (w, h) = (n * 2 / 3, n);
    let inside = |x: i32, y: i32| -> bool {
        // A triangle with its tip at the hotspot, and a short tail.
        let f = (x as f64 + 0.5) / w as f64;
        let g = (y as f64 + 0.5) / h as f64;
        (g < 0.78 && f <= g * 0.9) || (g >= 0.55 && (f - 0.3).abs() < 0.11 && f < g * 0.9)
    };
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            if !inside(x, y) {
                continue;
            }
            let edge =
                [(-1, 0), (1, 0), (0, -1), (0, 1)].iter().any(|(dx, dy)| !inside(x + dx, y + dy)) || x == 0 || y == 0;
            let c = if edge { 0 } else { 255 };
            let i = ((y * w + x) * 4) as usize;
            rgba[i..i + 4].copy_from_slice(&[c, c, c, 255]);
        }
    }
    let buffer = MemoryRenderBuffer::from_slice(&rgba, Fourcc::Abgr8888, (w, h), 1, Transform::Normal, None);
    CursorImage { buffer, hotspot: (0, 0) }
}

impl Uwc {
    /// The pointer as render elements at output scale `scale` (empty when hidden).
    pub fn cursor_elements(&mut self, renderer: &mut GlesRenderer, scale: f64) -> Vec<UwcElement> {
        let pos: Point<f64, Physical> = (self.cursor.x * scale, self.cursor.y * scale).into();
        match &self.cursor_status {
            CursorImageStatus::Hidden => vec![],
            CursorImageStatus::Surface(surface) if surface.alive() => {
                let hotspot = with_states(surface, |s| {
                    s.data_map.get::<CursorImageSurfaceData>().map(|d| d.lock().unwrap().hotspot).unwrap_or_default()
                });
                let at = pos - hotspot.to_f64().to_physical(scale);
                let at: Point<i32, Physical> = at.to_i32_round();
                render_elements_from_surface_tree::<_, WaylandSurfaceRenderElement<GlesRenderer>>(
                    renderer,
                    surface,
                    at,
                    Scale::from(scale),
                    1.0,
                    Kind::Cursor,
                )
                .into_iter()
                .map(|e| UwcElement::from(RescaleRenderElement::from_element(e, at, 1.0)))
                .collect()
            }
            status => {
                let icon = match status {
                    CursorImageStatus::Named(icon) => icon.name(),
                    _ => "default",
                };
                let img = self.cursor_theme.image(icon, scale);
                let at = pos - Point::<f64, Physical>::from((img.hotspot.0 as f64, img.hotspot.1 as f64));
                match MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    at,
                    &img.buffer,
                    None,
                    None,
                    None,
                    Kind::Cursor,
                ) {
                    Ok(e) => vec![UwcElement::from(e)],
                    Err(e) => {
                        tracing::warn!("cursor: {e}");
                        vec![]
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn fallback_arrow_has_a_tip_at_the_hotspot() {
        let a = super::arrow(24);
        assert_eq!(a.hotspot, (0, 0));
    }
}
