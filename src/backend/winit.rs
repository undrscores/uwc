//! Nested backend: uwc as a window in another Wayland (or X11) session. Mouse and keyboard come from
//! that window; the host draws the pointer.

use smithay::{
    backend::{
        renderer::{damage::OutputDamageTracker, gles::GlesRenderer},
        winit::{self, WinitEvent},
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::calloop::LoopHandle,
    utils::Transform,
};

use crate::state::Uwc;

pub fn init(state: &mut Uwc, handle: &LoopHandle<'static, Uwc>) -> anyhow::Result<()> {
    let (mut backend, winit) = winit::init::<GlesRenderer>().map_err(|e| anyhow::anyhow!("winit: {e}"))?;
    let mode = Mode { size: backend.window_size(), refresh: 60_000 };
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
    let size = backend.window_size();
    state.add_monitor(output.clone(), (size.w, size.h).into());

    let mut tracker = OutputDamageTracker::from_output(&output);
    handle
        .insert_source(winit, move |event, _, state| match event {
            WinitEvent::Resized { size, .. } => {
                let mode = Mode { size, refresh: 60_000 };
                output.change_current_state(Some(mode), None, None, None);
                if let Some(m) = state.mon.as_mut() {
                    m.size = (size.w, size.h).into();
                }
                state.damage();
            }
            WinitEvent::Input(event) => state.process_input_event(event),
            WinitEvent::CloseRequested => state.loop_signal.stop(),
            WinitEvent::Focus(_) => state.damage(),
            WinitEvent::Redraw => {
                if std::mem::take(&mut state.redraw) {
                    let age = backend.buffer_age().unwrap_or(0);
                    let damage = {
                        let (renderer, mut fb) = match backend.bind() {
                            Ok(b) => b,
                            Err(e) => {
                                tracing::error!("bind: {e}");
                                return;
                            }
                        };
                        let elements = state.render_elements(renderer);
                        match tracker.render_output(renderer, &mut fb, age, &elements, state.config.root_color) {
                            Ok(r) => r.damage.cloned(),
                            Err(e) => {
                                tracing::error!("render: {e}");
                                None
                            }
                        }
                    };
                    if let Err(e) = backend.submit(damage.as_deref()) {
                        tracing::error!("submit: {e}");
                    }
                    super::send_frame_callbacks(state, &output);
                }
                state.popups.cleanup();
                backend.window().request_redraw();
            }
        })
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}
