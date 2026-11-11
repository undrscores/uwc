//! zwlr-output-management-v1 (kanshi, wdisplays, bars' output widgets): every output is a head
//! with its modes and current state; a client builds a configuration (enable / disable heads, set
//! mode, transform, scale) and applies or tests it. Applying writes into the live monitor config
//! and rescans the connectors on udev, so the same machinery as the config file runs; on winit
//! and headless only scale and transform can be applied (there is no mode to change), and turning
//! the last output off is refused. Positions are uwc's alone: monitors tile left to right, so
//! `set_position` is accepted and ignored.

use smithay::reexports::wayland_protocols_wlr::output_management::v1::server::{
    zwlr_output_configuration_head_v1::{self, ZwlrOutputConfigurationHeadV1},
    zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
};
use smithay::{
    output::Output,
    reexports::wayland_server::{
        protocol::wl_output, Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum,
    },
};

use crate::state::Uwc;

const MANAGER_VERSION: u32 = 4;

/// What one head of a pending configuration says.
#[derive(Debug, Clone)]
pub(crate) struct HeadConfig {
    monitor: String,
    enabled: bool,
    mode: Option<(i32, i32, i32)>,
    transform: Option<smithay::utils::Transform>,
    scale: Option<f64>,
}

/// The manager state: the bound managers and the head objects each was told about.
#[derive(Default)]
pub struct OutputManagement {
    managers: Vec<ZwlrOutputManagerV1>,
    heads: Vec<HeadState>,
    serial: u32,
}

struct HeadState {
    head: ZwlrOutputHeadV1,
}

/// The output name a head resource stands for.
struct HeadData {
    monitor: String,
}

/// The mode a mode resource stands for, as (w, h, refresh mHz).
struct ModeData {
    size: (i32, i32),
    refresh: i32,
}

/// The heads of a pending configuration, shared with its configuration heads.
#[derive(Default)]
struct ConfigData {
    heads: std::sync::Arc<std::sync::Mutex<Vec<HeadConfig>>>,
}

/// Which entry of the configuration a configuration head stands for.
struct ConfigHeadData {
    heads: std::sync::Arc<std::sync::Mutex<Vec<HeadConfig>>>,
    index: usize,
}

impl OutputManagement {
    /// Tell every bound manager about every output, from scratch: the old heads are finished,
    /// the new state goes out with a fresh serial. Called after any change to the outputs.
    pub fn refresh(&mut self, state: &mut Uwc) {
        self.serial = self.serial.wrapping_add(1).max(1);
        for h in self.heads.drain(..) {
            h.head.finished();
        }
        let managers: Vec<ZwlrOutputManagerV1> = self.managers.clone();
        let serial = self.serial;
        for manager in &managers {
            let Ok(client) = state.display_handle.get_client(manager.id()) else { continue };
            for m in &state.monitors {
                let version = MANAGER_VERSION.min(manager.version());
                let Ok(head) = client.create_resource::<ZwlrOutputHeadV1, HeadData, Uwc>(
                    &state.display_handle,
                    version,
                    HeadData { monitor: m.output.name() },
                ) else {
                    continue;
                };
                send_head(&client, &state.display_handle, &head, &m.output, &m.origin, version);
                manager.head(&head);
                self.heads.push(HeadState { head });
            }
            manager.done(serial);
        }
    }
}

/// One head's worth of events: the output's identity and its current state (the modes are part
/// of the head's identity and are not re-sent on refresh; a client re-binding gets everything).
fn send_head(
    client: &Client,
    dh: &DisplayHandle,
    head: &ZwlrOutputHeadV1,
    output: &Output,
    origin: &smithay::utils::Point<f64, smithay::utils::Logical>,
    version: u32,
) {
    let p = output.physical_properties();
    head.name(output.name());
    head.description(output.description());
    head.physical_size(p.size.w, p.size.h);
    if version >= 2 {
        head.make(p.make.clone());
        head.model(p.model.clone());
        head.serial_number(String::new());
    }
    // The modes, marking the current and the preferred one.
    let current = output.current_mode();
    let preferred = output.preferred_mode();
    for mode in output.modes() {
        let Ok(m) = client.create_resource::<ZwlrOutputModeV1, ModeData, Uwc>(
            dh,
            version.min(3),
            ModeData { size: (mode.size.w, mode.size.h), refresh: mode.refresh },
        ) else {
            continue;
        };
        m.size(mode.size.w, mode.size.h);
        m.refresh(mode.refresh);
        if Some(mode) == preferred {
            m.preferred();
        }
        head.mode(&m);
        if Some(mode) == current {
            head.current_mode(&m);
        }
    }
    head.enabled(1);
    head.position(origin.x as i32, origin.y as i32);
    head.transform(wl_output::Transform::from(output.current_transform()));
    head.scale(output.current_scale().fractional_scale());
}

/// Tell the bound managers about the outputs as they are now (a new serial, new head objects).
/// A free function so `Uwc` methods can call it on themselves.
pub fn refresh(state: &mut Uwc) {
    let mut om = std::mem::take(&mut state.output_management);
    om.refresh(state);
    state.output_management = om;
}

impl Uwc {
    /// Apply (or test) a configuration a client built over zwlr-output-management. On udev the
    /// entries become the live monitor config and the connectors are rescanned (the same path a
    /// config reload takes); on winit and headless only scale and transform exist to change, and
    /// turning outputs off or setting modes fails.
    pub fn output_management_apply(&mut self, heads: Vec<HeadConfig>, apply: bool) -> bool {
        if heads.is_empty() || !heads.iter().any(|h| h.enabled) {
            return false;
        }
        for h in &heads {
            if let Some((w, hh, _)) = h.mode {
                // A client picks these: keep them to what a display can be.
                if !(1..=16384).contains(&w) || !(1..=16384).contains(&hh) {
                    return false;
                }
            }
            if h.mode.is_some_and(|(_, _, r)| !(0..=1_000_000).contains(&r)) {
                return false;
            }
            if let Some(s) = h.scale {
                if !(0.1..=10.0).contains(&s) {
                    return false;
                }
            }
        }
        if !apply {
            return true;
        }
        let real = matches!(self.backend, crate::backend::Backend::Udev(_));
        if !real && heads.iter().any(|h| !h.enabled || h.mode.is_some()) {
            return false;
        }
        // The entries become the live monitor config (until the next reload, like IPC `set`).
        for h in &heads {
            let entry = self.config.monitors.iter_mut().find(|c| c.name == h.monitor);
            match entry {
                Some(c) => {
                    c.off = !h.enabled;
                    if let Some((w, hh, r)) = h.mode {
                        c.mode = Some((w, hh, Some(r as f64 / 1000.0)));
                    }
                    c.scale = h.scale.or(c.scale);
                    c.transform = h.transform.or(c.transform);
                }
                None => self.config.monitors.push(crate::config::MonitorConfig {
                    name: h.monitor.clone(),
                    mode: h.mode.map(|(w, hh, r)| (w, hh, Some(r as f64 / 1000.0))),
                    scale: h.scale,
                    transform: h.transform,
                    off: !h.enabled,
                }),
            }
        }
        if real {
            self.backend.config_changed(&self.config);
            self.udev_scan_connectors();
        } else {
            // Winit / headless: the scale and transform go straight onto the output.
            for h in &heads {
                if let Some(m) = self.monitors.iter().find(|m| m.output.name() == h.monitor) {
                    m.output.change_current_state(
                        None,
                        h.transform,
                        h.scale.map(smithay::output::Scale::Fractional),
                        None,
                    );
                }
            }
            self.damage();
        }
        refresh(self);
        true
    }
}

impl GlobalDispatch<ZwlrOutputManagerV1, (), Uwc> for Uwc {
    fn can_view(client: smithay::reexports::wayland_server::Client, _: &()) -> bool {
        !crate::sandbox::is_sandboxed(&client)
    }

    fn bind(
        state: &mut Uwc,
        _: &DisplayHandle,
        _: &Client,
        manager: New<ZwlrOutputManagerV1>,
        _: &(),
        data_init: &mut DataInit<'_, Uwc>,
    ) {
        let manager = data_init.init(manager, ());
        state.output_management.managers.push(manager);
        refresh(state);
    }
}

impl Dispatch<ZwlrOutputManagerV1, (), Uwc> for Uwc {
    fn request(
        state: &mut Uwc,
        _: &Client,
        manager: &ZwlrOutputManagerV1,
        request: zwlr_output_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Uwc>,
    ) {
        match request {
            zwlr_output_manager_v1::Request::CreateConfiguration { id, serial: _ } => {
                data_init.init(id, ConfigData::default());
            }
            zwlr_output_manager_v1::Request::Stop => {
                manager.finished();
                state.output_management.managers.retain(|m| m != manager);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputHeadV1, HeadData, Uwc> for Uwc {
    fn request(
        _: &mut Uwc,
        _: &Client,
        _: &ZwlrOutputHeadV1,
        _: zwlr_output_head_v1::Request,
        _: &HeadData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Uwc>,
    ) {
    }
}

impl Dispatch<ZwlrOutputModeV1, ModeData, Uwc> for Uwc {
    fn request(
        _: &mut Uwc,
        _: &Client,
        _: &ZwlrOutputModeV1,
        _: zwlr_output_mode_v1::Request,
        _: &ModeData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Uwc>,
    ) {
    }
}

impl Dispatch<ZwlrOutputConfigurationV1, ConfigData, Uwc> for Uwc {
    fn request(
        state: &mut Uwc,
        _: &Client,
        config: &ZwlrOutputConfigurationV1,
        request: zwlr_output_configuration_v1::Request,
        data: &ConfigData,
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Uwc>,
    ) {
        match request {
            zwlr_output_configuration_v1::Request::EnableHead { id, head } => {
                let Some(hd) = head.data::<HeadData>() else { return };
                let mut heads = data.heads.lock().unwrap();
                heads.push(HeadConfig {
                    monitor: hd.monitor.clone(),
                    enabled: true,
                    mode: None,
                    transform: None,
                    scale: None,
                });
                data_init.init(id, ConfigHeadData { heads: data.heads.clone(), index: heads.len() - 1 });
            }
            zwlr_output_configuration_v1::Request::DisableHead { head } => {
                let Some(hd) = head.data::<HeadData>() else { return };
                data.heads.lock().unwrap().push(HeadConfig {
                    monitor: hd.monitor.clone(),
                    enabled: false,
                    mode: None,
                    transform: None,
                    scale: None,
                });
            }
            zwlr_output_configuration_v1::Request::Test => {
                let heads = data.heads.lock().unwrap().clone();
                if state.output_management_apply(heads, false) {
                    config.succeeded();
                } else {
                    config.failed();
                }
            }
            zwlr_output_configuration_v1::Request::Apply => {
                let heads = data.heads.lock().unwrap().clone();
                if state.output_management_apply(heads, true) {
                    config.succeeded();
                    refresh(state);
                } else {
                    config.failed();
                }
            }
            zwlr_output_configuration_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}

impl Dispatch<ZwlrOutputConfigurationHeadV1, ConfigHeadData, Uwc> for Uwc {
    fn request(
        _: &mut Uwc,
        _: &Client,
        _: &ZwlrOutputConfigurationHeadV1,
        request: zwlr_output_configuration_head_v1::Request,
        data: &ConfigHeadData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Uwc>,
    ) {
        let mut heads = data.heads.lock().unwrap();
        match request {
            zwlr_output_configuration_head_v1::Request::SetMode { mode } => {
                if let Some(md) = mode.data::<ModeData>() {
                    if let Some(entry) = heads.get_mut(data.index) {
                        entry.mode = Some((md.size.0, md.size.1, md.refresh));
                    }
                }
            }
            zwlr_output_configuration_head_v1::Request::SetCustomMode { width, height, refresh } => {
                if let Some(entry) = heads.get_mut(data.index) {
                    entry.mode = Some((width, height, refresh));
                }
            }
            // Monitors tile left to right in uwc; the position is accepted and ignored.
            zwlr_output_configuration_head_v1::Request::SetPosition { .. } => {}
            zwlr_output_configuration_head_v1::Request::SetTransform { transform } => {
                if let WEnum::Value(t) = transform {
                    if let Some(entry) = heads.get_mut(data.index) {
                        entry.transform = Some(t.into());
                    }
                }
            }
            zwlr_output_configuration_head_v1::Request::SetScale { scale } => {
                if let Some(entry) = heads.get_mut(data.index) {
                    entry.scale = Some(scale);
                }
            }
            zwlr_output_configuration_head_v1::Request::SetAdaptiveSync { .. } => {}
            _ => unreachable!(),
        }
    }
}
