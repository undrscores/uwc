//! wlr-layer-shell: bars, panels, docks, launchers, notifications, wallpaper programs.
//!
//! Layer surfaces belong to the screen, not the canvas: they never pan or zoom. Background and
//! bottom layers are drawn under the windows, top and overlay above them. Exclusive zones shrink
//! the work area, which is where new windows are centred and what reveal / center / fit aim at.
//!
//! Keyboard: a mapped top/overlay surface asking for exclusive interactivity takes the keyboard
//! until it unmaps or gives it up; on-demand surfaces take it when clicked. While a layer surface
//! has the keyboard, window focus changes are remembered but not applied.

use smithay::{
    backend::renderer::{
        element::{
            surface::{render_elements_from_surface_tree, WaylandSurfaceRenderElement},
            AsRenderElements, Kind,
        },
        gles::GlesRenderer,
    },
    delegate_layer_shell,
    desktop::{layer_map_for_output, LayerSurface, PopupKind, PopupManager, WindowSurfaceType},
    output::Output,
    reexports::wayland_server::protocol::{wl_output::WlOutput, wl_surface::WlSurface},
    utils::{Logical, Point, Scale, SERIAL_COUNTER},
    wayland::{
        compositor::with_states,
        shell::{
            wlr_layer::{
                KeyboardInteractivity, Layer as WlrLayer, LayerSurface as WlrLayerSurface, LayerSurfaceData,
                WlrLayerShellHandler, WlrLayerShellState,
            },
            xdg::PopupSurface,
        },
    },
};

use crate::{canvas::Area, render::UwcElement, state::Uwc};

/// What is under a screen point.
pub enum Under {
    /// A window's surface; `origin` and `location` are in the window's space (see `WindowHit`).
    Window { idx: usize, surface: WlSurface, origin: Point<f64, Logical>, location: Point<f64, Logical> },
    /// A layer surface (or one of its popups / subsurfaces); `origin` is in screen px.
    Layer { root: WlSurface, surface: WlSurface, origin: Point<f64, Logical>, interactive: bool },
}

impl Under {
    pub fn window(&self) -> Option<usize> {
        match self {
            Under::Window { idx, .. } => Some(*idx),
            Under::Layer { .. } => None,
        }
    }
}

pub fn layer_name(l: WlrLayer) -> &'static str {
    match l {
        WlrLayer::Background => "background",
        WlrLayer::Bottom => "bottom",
        WlrLayer::Top => "top",
        WlrLayer::Overlay => "overlay",
    }
}

impl Uwc {
    /// The cursor monitor's output.
    pub fn output(&self) -> Option<Output> {
        self.output_of(self.cur)
    }

    pub fn output_of(&self, m: usize) -> Option<Output> {
        self.monitors.get(m).map(|x| x.output.clone())
    }

    /// The monitor an output belongs to.
    pub fn mon_by_output(&self, o: &Output) -> Option<usize> {
        self.monitors.iter().position(|m| m.output.name() == o.name())
    }

    /// The screen minus what layer surfaces reserve (logical px), on the cursor monitor.
    pub fn work_area(&self) -> Area {
        self.work_area_of(self.cur)
    }

    pub fn work_area_of(&self, m: usize) -> Area {
        let (sw, sh) = self.monitors.get(m).map(|x| (x.size.w as f64, x.size.h as f64)).unwrap_or((1280.0, 720.0));
        let Some(o) = self.output_of(m) else { return (0.0, 0.0, sw, sh) };
        let z = layer_map_for_output(&o).non_exclusive_zone();
        if z.size.w <= 0 || z.size.h <= 0 {
            return (0.0, 0.0, sw, sh);
        }
        (z.loc.x as f64, z.loc.y as f64, z.size.w as f64, z.size.h as f64)
    }

    fn layer_under(&self, o: &Output, layer: WlrLayer, p: Point<f64, Logical>) -> Option<Under> {
        let map = layer_map_for_output(o);
        let l = map.layer_under(layer, p)?;
        let geo = map.layer_geometry(l)?;
        let (surface, loc) = l.surface_under(p - geo.loc.to_f64(), WindowSurfaceType::ALL)?;
        let interactive = l.cached_state().keyboard_interactivity != KeyboardInteractivity::None;
        Some(Under::Layer { root: l.wl_surface().clone(), surface, origin: (geo.loc + loc).to_f64(), interactive })
    }

    /// Topmost thing under global point `p` (the monitors side by side), in drawing order: overlay
    /// layer, fullscreen windows, top layer, windows, bottom and background layers — all on the
    /// monitor the point is on, in that monitor's coordinates.
    pub fn under(&self, p: Point<f64, Logical>) -> Option<Under> {
        let m = self.mon_at(p);
        let o = self.output_of(m);
        let p = p - self.monitors.get(m).map(|x| x.origin).unwrap_or((0.0, 0.0).into());
        let window = |h: crate::state::WindowHit| Under::Window {
            idx: h.idx,
            surface: h.surface,
            origin: h.origin,
            location: h.location,
        };
        if let Some(u) = o.as_ref().and_then(|o| self.layer_under(o, WlrLayer::Overlay, p)) {
            return Some(u);
        }
        if let Some(h) = self.window_under(m, p, true) {
            return Some(window(h));
        }
        // Under a fullscreen window everything is covered by fullscreen-bg: nothing there takes the pointer.
        if self.clients.iter().any(|c| c.fullscreen && self.visible_on(m, c)) {
            return None;
        }
        if let Some(u) = o.as_ref().and_then(|o| self.layer_under(o, WlrLayer::Top, p)) {
            return Some(u);
        }
        if let Some(h) = self.window_under(m, p, false) {
            return Some(window(h));
        }
        if let Some(o) = &o {
            for l in [WlrLayer::Bottom, WlrLayer::Background] {
                if let Some(u) = self.layer_under(o, l, p) {
                    return Some(u);
                }
            }
        }
        None
    }

    /// Point the keyboard at the focused layer surface if there is one, else at the focused window.
    pub fn update_keyboard_focus(&mut self) {
        let target = self
            .layer_focus
            .clone()
            .or_else(|| self.focus.and_then(|id| self.client_index(id)).map(|i| self.clients[i].surface().clone()));
        let kb = self.keyboard();
        if kb.current_focus() != target {
            kb.set_focus(self, target, SERIAL_COUNTER.next_serial());
        }
        self.ipc.dirty = true;
    }

    pub fn set_layer_focus(&mut self, surface: Option<WlSurface>) {
        if self.layer_focus != surface {
            self.layer_focus = surface;
            self.update_keyboard_focus();
        }
    }

    /// The focused layer surface wants the keyboard for itself (exclusive, on top or overlay).
    pub fn layer_focus_is_exclusive(&self) -> bool {
        let Some(f) = &self.layer_focus else { return false };
        self.monitors.iter().any(|m| self.layer_exclusive_on(&m.output, f))
    }

    fn layer_exclusive_on(&self, o: &Output, f: &WlSurface) -> bool {
        let map = layer_map_for_output(o);
        map.layer_for_surface(f, WindowSurfaceType::TOPLEVEL).is_some_and(|l| {
            l.cached_state().keyboard_interactivity == KeyboardInteractivity::Exclusive
                && matches!(l.layer(), WlrLayer::Top | WlrLayer::Overlay)
        })
    }

    /// A commit on a layer surface: lay the layers out again, configure, and follow its keyboard wishes.
    /// Returns false if `root` is not a layer surface.
    pub fn layer_commit(&mut self, root: &WlSurface) -> bool {
        let Some(o) = self
            .monitors
            .iter()
            .map(|m| m.output.clone())
            .find(|o| layer_map_for_output(o).layer_for_surface(root, WindowSurfaceType::TOPLEVEL).is_some())
        else {
            return false;
        };
        let mut map = layer_map_for_output(&o);
        let Some(layer) = map.layer_for_surface(root, WindowSurfaceType::TOPLEVEL).cloned() else {
            return false;
        };
        let initial = with_states(root, |s| {
            s.data_map.get::<LayerSurfaceData>().map(|d| d.lock().unwrap().initial_configure_sent).unwrap_or(true)
        });
        map.arrange();
        if !initial {
            layer.layer_surface().send_configure();
        }
        let mapped = map.layer_geometry(&layer).is_some_and(|g| !g.is_empty())
            && with_states(root, |s| {
                s.data_map
                    .get::<smithay::backend::renderer::utils::RendererSurfaceStateUserData>()
                    .is_some_and(|d| d.lock().unwrap().buffer().is_some())
            });
        drop(map);

        let wants = layer.cached_state().keyboard_interactivity;
        let on_top = matches!(layer.layer(), WlrLayer::Top | WlrLayer::Overlay);
        if mapped && wants == KeyboardInteractivity::Exclusive && on_top {
            self.set_layer_focus(Some(root.clone()));
        } else if self.layer_focus.as_ref() == Some(root) && (wants == KeyboardInteractivity::None || !mapped) {
            self.set_layer_focus(None);
        }
        self.refresh_pointer();
        self.damage();
        true
    }

    /// Layer surfaces of `layers` (in that order, each topmost first) as elements, popups above their parent.
    pub fn layer_elements_on(
        &self,
        m: usize,
        renderer: &mut GlesRenderer,
        scale: f64,
        layers: &[WlrLayer],
    ) -> Vec<UwcElement> {
        let Some(o) = self.output_of(m) else { return vec![] };
        let map = layer_map_for_output(&o);
        let mut out = Vec::new();
        for &which in layers {
            for l in map.layers_on(which).rev() {
                let Some(geo) = map.layer_geometry(l) else { continue };
                let loc = geo.loc.to_f64().to_physical(scale).to_i32_round();
                for (popup, offset) in PopupManager::popups_for_surface(l.wl_surface()) {
                    let at = geo.loc + offset - popup.geometry().loc;
                    out.extend(
                        render_elements_from_surface_tree::<_, WaylandSurfaceRenderElement<GlesRenderer>>(
                            renderer,
                            popup.wl_surface(),
                            at.to_f64().to_physical(scale).to_i32_round(),
                            Scale::from(scale),
                            1.0,
                            Kind::Unspecified,
                        )
                        .into_iter()
                        .map(UwcElement::Layer),
                    );
                }
                out.extend(
                    AsRenderElements::<GlesRenderer>::render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
                        l,
                        renderer,
                        loc,
                        Scale::from(scale),
                        1.0,
                    )
                    .into_iter()
                    .map(UwcElement::Layer),
                );
            }
        }
        out
    }

    /// Move the layer surfaces of the old output to a new one (output hotplug, a new mode).
    pub fn move_layers(&mut self, from: &Output, to: &Output) {
        if from == to {
            return;
        }
        let layers: Vec<LayerSurface> = layer_map_for_output(from).layers().cloned().collect();
        let mut old = layer_map_for_output(from);
        for l in &layers {
            old.unmap_layer(l);
        }
        drop(old);
        let mut new = layer_map_for_output(to);
        for l in &layers {
            let _ = new.map_layer(l);
        }
        new.arrange();
    }

    /// For IPC: the layer surfaces on every output.
    pub fn layer_info(&self) -> Vec<crate::protocol::Layer> {
        let mut out = Vec::new();
        for m in &self.monitors {
            let map = layer_map_for_output(&m.output);
            for l in map.layers() {
                let Some(g) = map.layer_geometry(l) else { continue };
                out.push(crate::protocol::Layer {
                    namespace: l.namespace().to_string(),
                    layer: layer_name(l.layer()),
                    rect: crate::protocol::Rect {
                        x: g.loc.x as i64,
                        y: g.loc.y as i64,
                        width: g.size.w as i64,
                        height: g.size.h as i64,
                    },
                    keyboard: self.layer_focus.as_ref() == Some(l.wl_surface()),
                });
            }
        }
        out
    }
}

impl WlrLayerShellHandler for Uwc {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        _output: Option<WlOutput>,
        _layer: WlrLayer,
        namespace: String,
    ) {
        // One output for now; a surface asking for another one gets this one.
        let Some(o) = self.output() else {
            surface.send_close();
            return;
        };
        let l = LayerSurface::new(surface, namespace);
        let res = layer_map_for_output(&o).map_layer(&l);
        if let Err(e) = res {
            tracing::warn!("layer surface: {e}");
        }
    }

    fn new_popup(&mut self, _parent: WlrLayerSurface, popup: PopupSurface) {
        let _ = self.popups.track_popup(PopupKind::Xdg(popup));
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        for m in &self.monitors {
            let mut map = layer_map_for_output(&m.output);
            let found = map.layers().find(|l| l.layer_surface() == &surface).cloned();
            if let Some(l) = found {
                map.unmap_layer(&l);
            }
        }
        if self.layer_focus.as_ref() == Some(surface.wl_surface()) {
            self.set_layer_focus(None);
        }
        self.refresh_pointer();
        self.damage();
    }
}

delegate_layer_shell!(Uwc);
