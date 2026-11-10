//! Drawing into a client's buffer, for the capture protocols (window previews, screenshots): render
//! elements into an offscreen texture with the backend's renderer, read it back, copy it into the
//! client's shm buffer. Only shm buffers are offered (every capture client supports them); a
//! headless uwc has no renderer, so captures there fail cleanly.

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            damage::OutputDamageTracker,
            element::memory::MemoryRenderBuffer,
            element::{surface::WaylandSurfaceRenderElement, AsRenderElements, RenderElement},
            gles::{GlesRenderer, GlesTexture},
            Bind, ExportMem, Offscreen,
        },
    },
    reexports::wayland_server::{
        protocol::{wl_buffer::WlBuffer, wl_shm},
        Resource,
    },
    utils::{Buffer, Physical, Point, Rectangle, Scale, Size, Transform},
    wayland::shm::with_buffer_contents_mut,
};

use crate::{
    backend::Backend,
    state::{Ghost, Uwc},
};

/// The shm format captures are offered in (ARGB8888, which every client supports).
pub const FORMAT: wl_shm::Format = wl_shm::Format::Argb8888;

/// A time for `ready` events: CLOCK_MONOTONIC as (seconds, nanoseconds).
pub fn now() -> (u64, u32) {
    let t = smithay::utils::Clock::<smithay::utils::Monotonic>::new().now();
    let d: std::time::Duration = t.into();
    (d.as_secs(), d.subsec_nanos())
}

/// Whether `buffer` is an shm buffer a capture of `size` can be written into.
pub fn check_buffer(buffer: &WlBuffer, size: Size<i32, Physical>) -> Result<(), String> {
    with_buffer_contents_mut(buffer, |_, len, d| {
        if !matches!(d.format, wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888) {
            return Err(format!("buffer format {:?}, expected Argb8888 or Xrgb8888", d.format));
        }
        if d.width != size.w || d.height != size.h {
            return Err(format!("buffer is {}x{}, expected {}x{}", d.width, d.height, size.w, size.h));
        }
        if d.stride < size.w * 4 || (d.offset as usize + (d.stride * d.height) as usize) > len {
            return Err("buffer stride or size too small".into());
        }
        Ok(())
    })
    .map_err(|_| "not an shm buffer".to_string())?
}

impl Uwc {
    /// Run `f` with the backend's renderer, if this backend has one.
    pub fn with_renderer<T>(&mut self, f: impl FnOnce(&mut Uwc, &mut GlesRenderer) -> T) -> Option<T> {
        // Check before taking: a failed match must not leave the backend taken.
        if !matches!(self.backend, Backend::Winit(_) | Backend::Udev(_)) {
            return None;
        }
        let mut backend = std::mem::take(&mut self.backend);
        let out = match &mut backend {
            Backend::Winit(w) => Some(f(self, w.backend.renderer())),
            Backend::Udev(u) => u.renderer_mut().map(|r| f(self, r)),
            _ => None,
        };
        self.backend = backend;
        out
    }

    /// The pixel size a capture of window `id` has: its geometry at the output scale.
    pub fn window_capture_size(&self, id: u32) -> Option<Size<i32, Physical>> {
        let c = &self.clients[self.client_index(id)?];
        let s = c.size().to_f64().to_physical(self.output_scale()).to_i32_round();
        (c.mapped && s.w > 0 && s.h > 0).then_some(s)
    }

    /// A closing window's last frame, to fade out over `close-ms` (`crate::state::Ghost`). None
    /// when the option is off, the window was not visible or had nothing to show, or this backend
    /// cannot render (headless), so closing there stays instant.
    pub fn closing_ghost(&mut self, i: usize) -> Option<Ghost> {
        let c = &self.clients[i];
        if self.config.close_ms <= 0.0 || !self.visible(c) || !c.surface().is_alive() {
            return None;
        }
        let pl = self.place(c);
        // What the window is drawn at now: its own scale on the view's zoom, at the output scale.
        let k = (pl.zoom * self.output_scale()).clamp(1.0, 8.0);
        let size = c.size();
        if size.w <= 0 || size.h <= 0 {
            return None;
        }
        let deco = self.deco_for(c);
        let focused = self.focus == Some(c.id);
        let (canvas, pin, scale, fullscreen) = (c.canvas, c.pin, c.scale, c.fullscreen);
        let px = Size::from(((size.w as f64 * k).round() as i32, (size.h as f64 * k).round() as i32));
        let buffer = self
            .with_renderer(|state, r| {
                let c = &state.clients[i];
                let g = c.window.geometry().loc.to_f64();
                let at: Point<i32, Physical> = Point::<f64, Physical>::from((-g.x * k, -g.y * k)).to_i32_round();
                let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                    AsRenderElements::<GlesRenderer>::render_elements(&c.window, r, at, Scale::from(k), 1.0);
                render_to_memory(r, &elements, px, k)
            })
            .unwrap_or_else(|| Err("this backend cannot render (headless)".into()))
            .ok()?;
        Some(Ghost {
            buffer,
            k,
            size,
            canvas,
            pin,
            scale,
            fullscreen,
            deco,
            focused,
            alpha: 1.0,
            scale_fx: 1.0,
            fade_ms: self.config.close_ms,
            deco_ids: std::array::from_fn(|_| smithay::backend::renderer::element::Id::new()),
        })
    }

    /// Draw window `id` alone (its surfaces, no decoration, unzoomed, at the output scale) into `buffer`.
    pub fn capture_window(&mut self, id: u32, buffer: &WlBuffer) -> Result<(), String> {
        let size = self.window_capture_size(id).ok_or("the window is gone or has no size")?;
        check_buffer(buffer, size)?;
        let scale = self.output_scale();
        let res = self.with_renderer(|state, r| {
            let Some(i) = state.client_index(id) else { return Err("the window is gone".to_string()) };
            let c = &state.clients[i];
            // The geometry's top-left at 0,0 (the surface may start further out, e.g. client shadows).
            let g = c.window.geometry().loc.to_f64().to_physical(scale);
            let at: Point<i32, Physical> = Point::<f64, Physical>::from((-g.x, -g.y)).to_i32_round();
            let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                AsRenderElements::<GlesRenderer>::render_elements(&c.window, r, at, Scale::from(scale), 1.0);
            render_to_shm(r, &elements, size, scale, buffer)
        });
        res.unwrap_or_else(|| Err("this backend cannot render (headless)".into()))
    }
}

/// Render `elements` (front to back) at `size` into a memory buffer (a closing window's last
/// frame): transparent around them, like `render_to_shm` without the shm copy.
pub fn render_to_memory<E: RenderElement<GlesRenderer>>(
    r: &mut GlesRenderer,
    elements: &[E],
    size: Size<i32, Physical>,
    scale: f64,
) -> Result<MemoryRenderBuffer, String> {
    let bsize: Size<i32, Buffer> = (size.w, size.h).into();
    let mut tex: GlesTexture =
        r.create_buffer(Fourcc::Abgr8888, bsize).map_err(|e| format!("offscreen buffer: {e}"))?;
    let pixels = {
        let mut fb = r.bind(&mut tex).map_err(|e| format!("bind: {e}"))?;
        let mut tracker = OutputDamageTracker::new(size, scale, Transform::Normal);
        tracker.render_output(r, &mut fb, 0, elements, [0.0; 4]).map_err(|e| format!("render: {e:?}"))?;
        let area = Rectangle::<i32, Buffer>::from_size(bsize);
        let mapping = r.copy_framebuffer(&fb, area, Fourcc::Argb8888).map_err(|e| format!("read back: {e}"))?;
        r.map_texture(&mapping).map_err(|e| format!("map: {e}"))?.to_vec()
    };
    // The read-back is in Argb8888 (GL_BGRA) byte order, like the shm captures, not Abgr8888.
    Ok(MemoryRenderBuffer::from_slice(&pixels, Fourcc::Argb8888, (size.w, size.h), 1, Transform::Normal, None))
}

/// Render `elements` (front to back) at `size` into the shm `buffer` (already checked).
pub fn render_to_shm<E: RenderElement<GlesRenderer>>(
    r: &mut GlesRenderer,
    elements: &[E],
    size: Size<i32, Physical>,
    scale: f64,
    buffer: &WlBuffer,
) -> Result<(), String> {
    // Transparent around the window.
    render_region_to_shm(r, elements, size, Rectangle::from_size(size), scale, [0.0; 4], buffer)
}

/// Render `elements` at `size` over `clear` and copy the part `region` of it into the shm `buffer`
/// (already checked to be `region`'s size).
pub fn render_region_to_shm<E: RenderElement<GlesRenderer>>(
    r: &mut GlesRenderer,
    elements: &[E],
    size: Size<i32, Physical>,
    region: Rectangle<i32, Physical>,
    scale: f64,
    clear: [f32; 4],
    buffer: &WlBuffer,
) -> Result<(), String> {
    let bsize: Size<i32, Buffer> = (size.w, size.h).into();
    let mut tex: GlesTexture =
        r.create_buffer(Fourcc::Abgr8888, bsize).map_err(|e| format!("offscreen buffer: {e}"))?;
    let mapping = {
        let mut fb = r.bind(&mut tex).map_err(|e| format!("bind: {e}"))?;
        let mut tracker = OutputDamageTracker::new(size, scale, Transform::Normal);
        tracker.render_output(r, &mut fb, 0, elements, clear).map_err(|e| format!("render: {e:?}"))?;
        let area =
            Rectangle::<i32, Buffer>::new((region.loc.x, region.loc.y).into(), (region.size.w, region.size.h).into());
        r.copy_framebuffer(&fb, area, Fourcc::Argb8888).map_err(|e| format!("read back: {e}"))?
    };
    let pixels = r.map_texture(&mapping).map_err(|e| format!("map: {e}"))?;
    let (w, h) = (region.size.w, region.size.h);
    let row = (w * 4) as usize;
    with_buffer_contents_mut(buffer, |ptr, len, d| {
        for y in 0..h as usize {
            // smithay renders offscreen textures upright, so their read-back rows are already top-down
            // (its mapping says `flipped`, which holds for the GL framebuffer, not here; checked
            // with asymmetric pictures in tests/nested.py).
            let src_y = y;
            let dst = d.offset as usize + y * d.stride as usize;
            if dst + row > len {
                break;
            }
            // Safety: the shm pool is mapped for `len` bytes and we stay inside it.
            unsafe { std::ptr::copy_nonoverlapping(pixels[src_y * row..].as_ptr(), ptr.add(dst), row) };
        }
    })
    .map_err(|_| "not an shm buffer".to_string())
}
