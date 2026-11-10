//! Nested backend: uwc as a window in another Wayland (or X11) session. Mouse and keyboard come from
//! that window. uwc draws its own pointer (as in a real session), so the host's is hidden.

use smithay::{
    backend::{
        renderer::{damage::OutputDamageTracker, gles::GlesRenderer, ImportDma},
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::calloop::LoopHandle,
    utils::Transform,
};

use super::Backend;
use crate::state::Uwc;

pub struct WinitData {
    pub backend: WinitGraphicsBackend<GlesRenderer>,
    output: Output,
    tracker: OutputDamageTracker,
}

pub fn init(state: &mut Uwc, handle: &LoopHandle<'static, Uwc>) -> anyhow::Result<()> {
    let (mut backend, winit) = winit::init::<GlesRenderer>().map_err(|e| anyhow::anyhow!("winit: {e}"))?;
    let size = backend.window_size();
    let mode = Mode { size, refresh: 60_000 };
    let output = Output::new(
        "winit".into(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "uwc".into(),
            model: "winit".into(),
        },
    );
    output.create_global::<Uwc>(&state.display_handle);
    output.change_current_state(Some(mode), Some(Transform::Flipped180), None, Some((0, 0).into()));
    output.set_preferred(mode);
    state.add_monitor(output.clone(), (size.w, size.h).into());
    backend.window().set_cursor_visible(false);

    // GL clients need linux-dmabuf (Mesa's EGL has no other way in).
    let formats = backend.renderer().dmabuf_formats();
    state.dmabuf_global = Some(state.dmabuf_state.create_global::<Uwc>(&state.display_handle, formats));

    let tracker = OutputDamageTracker::from_output(&output);
    state.backend = Backend::Winit(Box::new(WinitData { backend, output, tracker }));

    handle
        .insert_source(winit, move |event, _, state| match event {
            WinitEvent::Resized { size, .. } => {
                if let Backend::Winit(w) = &state.backend {
                    w.output.change_current_state(Some(Mode { size, refresh: 60_000 }), None, None, None);
                }
                if let Some(m) = state.mon.as_mut() {
                    m.size = (size.w, size.h).into();
                    smithay::desktop::layer_map_for_output(&m.output).arrange();
                }
                state.damage();
            }
            WinitEvent::Input(event) => state.process_input_event(event),
            WinitEvent::CloseRequested => state.loop_signal.stop(),
            WinitEvent::Focus(_) => state.damage(),
            WinitEvent::Redraw => state.winit_redraw(),
        })
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

impl Uwc {
    fn winit_redraw(&mut self) {
        // Check before taking: a failed match must not leave the backend taken.
        if !matches!(self.backend, Backend::Winit(_)) {
            return;
        }
        let Backend::Winit(mut w) = std::mem::take(&mut self.backend) else { unreachable!() };
        if std::mem::take(&mut self.redraw) {
            let age = w.backend.buffer_age().unwrap_or(0);
            let damage = match w.backend.bind() {
                Ok((renderer, mut fb)) => {
                    let elements = self.frame_elements(renderer, 1.0);
                    match w.tracker.render_output(renderer, &mut fb, age, &elements, self.config.root_color) {
                        Ok(r) => r.damage.cloned(),
                        Err(e) => {
                            tracing::error!("render: {e}");
                            None
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("bind: {e}");
                    None
                }
            };
            if let Err(e) = w.backend.submit(damage.as_deref()) {
                tracing::error!("submit: {e}");
            }
            super::send_frame_callbacks(self, &w.output);
            if std::mem::take(&mut self.animating) {
                self.redraw = true;
            }
        }
        self.popups.cleanup();
        w.backend.window().request_redraw();
        self.backend = Backend::Winit(w);
    }
}
