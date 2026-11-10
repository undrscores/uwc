//! No display at all: a fixed-size virtual output and a 60 Hz frame clock. Nothing is rendered,
//! but windows map, move and get frame callbacks, so protocol and IPC behaviour can be tested.

use std::time::Duration;

use smithay::{
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::calloop::{
        timer::{TimeoutAction, Timer},
        LoopHandle,
    },
    utils::Transform,
};

use crate::state::Uwc;

pub fn init(state: &mut Uwc, handle: &LoopHandle<'static, Uwc>, size: (i32, i32)) -> anyhow::Result<()> {
    let mode = Mode { size: size.into(), refresh: 60_000 };
    let output = Output::new(
        "HEADLESS-1".into(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "uwc".into(),
            model: "headless".into(),
        },
    );
    output.create_global::<Uwc>(&state.display_handle);
    output.change_current_state(Some(mode), Some(Transform::Normal), None, Some((0, 0).into()));
    output.set_preferred(mode);
    state.add_monitor(output.clone(), (size.0, size.1).into());
    state.backend = super::Backend::Headless;

    handle
        .insert_source(Timer::from_duration(Duration::from_millis(16)), move |_, _, state| {
            if std::mem::take(&mut state.redraw) {
                super::send_frame_callbacks(state, &output);
            }
            state.popups.cleanup();
            TimeoutAction::ToDuration(Duration::from_millis(16))
        })
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}
