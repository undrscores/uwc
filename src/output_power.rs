//! zwlr-output-power-management-v1 (swayidle and friends): a client can ask for an output's power
//! mode. `off` blanks the monitor (nothing is drawn or captured from it) until someone sets `on`
//! again; the client is told the mode it ends up in. This is the screen-off half of DPMS -- the
//! idle daemon drives it over the protocol, and any input the compositor still gets while blanked
//! is the daemon's to answer (it sees it through ext-idle-notify and sets `on` back).

use smithay::{
    output::Output,
    reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New},
};

use crate::state::Uwc;
use smithay::reexports::wayland_protocols_wlr::output_power_management::v1::server::{
    zwlr_output_power_manager_v1::{self, ZwlrOutputPowerManagerV1},
    zwlr_output_power_v1::{self, ZwlrOutputPowerV1},
};

/// Which monitor a power resource is about.
struct PowerData {
    name: String,
}

pub struct OutputPower {
    #[allow(dead_code)] // held so the global stays alive
    global: (),
}

impl OutputPower {
    pub fn new(dh: &DisplayHandle) -> Self {
        dh.create_global::<Uwc, ZwlrOutputPowerManagerV1, ()>(1, ());
        OutputPower { global: () }
    }
}

impl GlobalDispatch<ZwlrOutputPowerManagerV1, (), Uwc> for Uwc {
    fn bind(
        _: &mut Uwc,
        _: &DisplayHandle,
        _: &Client,
        _: New<ZwlrOutputPowerManagerV1>,
        _: &(),
        _: &mut DataInit<'_, Uwc>,
    ) {
    }
}

impl Dispatch<ZwlrOutputPowerManagerV1, (), Uwc> for Uwc {
    fn request(
        state: &mut Uwc,
        _: &Client,
        _: &ZwlrOutputPowerManagerV1,
        request: zwlr_output_power_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Uwc>,
    ) {
        if let zwlr_output_power_manager_v1::Request::GetOutputPower { id, output } = request {
            // The output the request names; one without a name is the pointer's monitor.
            let name = Output::from_resource(&output)
                .and_then(|o| state.mon_by_output(&o))
                .map(|m| state.monitors[m].output.name())
                .or_else(|| state.monitors.get(state.cur).map(|m| m.output.name()))
                .unwrap_or_default();
            let power = data_init.init(id, PowerData { name: name.clone() });
            // Tell it where the output stands.
            let on = state.monitors.iter().find(|m| m.output.name() == name).is_some_and(|m| m.powered);
            power.mode(if on { zwlr_output_power_v1::Mode::On } else { zwlr_output_power_v1::Mode::Off });
        }
    }
}

impl Dispatch<ZwlrOutputPowerV1, PowerData, Uwc> for Uwc {
    fn request(
        state: &mut Uwc,
        _: &Client,
        power: &ZwlrOutputPowerV1,
        request: zwlr_output_power_v1::Request,
        data: &PowerData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Uwc>,
    ) {
        let zwlr_output_power_v1::Request::SetMode { mode } = request else { return };
        let on = match mode {
            smithay::reexports::wayland_server::WEnum::Value(zwlr_output_power_v1::Mode::On) => true,
            smithay::reexports::wayland_server::WEnum::Value(zwlr_output_power_v1::Mode::Off) => false,
            _ => {
                power.failed();
                return;
            }
        };
        if let Some(m) = state.monitors.iter_mut().find(|m| m.output.name() == data.name) {
            m.powered = on;
        }
        power.mode(if on { zwlr_output_power_v1::Mode::On } else { zwlr_output_power_v1::Mode::Off });
        state.damage();
    }
}
