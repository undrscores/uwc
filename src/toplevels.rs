//! wlr-foreign-toplevel-management-v1: the window list for taskbars and shells (Quickshell's
//! ToplevelManager, Waybar's taskbar). Each mapped window gets a handle with its title, app-id,
//! output and state (activated, minimized, fullscreen); the handle's requests map onto uwc's own
//! actions: activate (restores a minimized window, focuses and reveals it), close, minimize,
//! fullscreen. Maximize does not exist on the canvas and is ignored; so is set_rectangle.
//!
//! Like the workspace protocol, changes go out once per event-loop turn (`toplevels_refresh`), and a
//! new manager hears about the existing windows at once.

use smithay::reexports::{
    wayland_protocols_wlr::foreign_toplevel::v1::server::{
        zwlr_foreign_toplevel_handle_v1::{self, State as TlState, ZwlrForeignToplevelHandleV1},
        zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
    },
    wayland_server::{
        backend::ClientId, protocol::wl_output::WlOutput, Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch,
        New, Resource,
    },
};

use crate::state::Uwc;

/// What a client was last told about one window.
#[derive(Clone, PartialEq, Default)]
struct Info {
    title: String,
    app_id: String,
    states: Vec<u32>,
    outputs: Vec<WlOutput>,
}

struct Handle {
    window: u32,
    handle: ZwlrForeignToplevelHandleV1,
    sent: Info,
}

struct Manager {
    manager: ZwlrForeignToplevelManagerV1,
    handles: Vec<Handle>,
}

#[derive(Default)]
pub struct ForeignToplevels {
    managers: Vec<Manager>,
}

impl ForeignToplevels {
    pub fn new(dh: &DisplayHandle) -> ForeignToplevels {
        dh.create_global::<Uwc, ZwlrForeignToplevelManagerV1, ()>(3, ());
        ForeignToplevels::default()
    }
}

impl Uwc {
    fn toplevel_info(&self, i: usize, client: &Client) -> Info {
        let c = &self.clients[i];
        let mut states = Vec::new();
        if self.focus == Some(c.id) {
            states.push(TlState::Activated as u32);
        }
        if c.minimized != 0 {
            states.push(TlState::Minimized as u32);
        }
        if c.fullscreen {
            states.push(TlState::Fullscreen as u32);
        }
        let outputs: Vec<_> = (0..self.monitors.len())
            .filter(|&m| self.visible_on(m, c))
            .flat_map(|m| self.monitors[m].output.client_outputs(client).collect::<Vec<_>>())
            .collect();
        Info { title: c.title(), app_id: c.app_id(), states, outputs }
    }

    /// Tell every manager what changed: new windows, changed titles / states / outputs, closed ones.
    pub fn toplevels_refresh(&mut self) {
        let dh = self.display_handle.clone();
        let mut managers = std::mem::take(&mut self.toplevels.managers);
        for m in &mut managers {
            let Ok(client) = dh.get_client(m.manager.id()) else { continue };
            // Closed windows.
            m.handles.retain(|h| {
                let alive = self.clients.iter().any(|c| c.id == h.window && c.mapped);
                if !alive {
                    h.handle.closed();
                }
                alive
            });
            for i in 0..self.clients.len() {
                if !self.clients[i].mapped {
                    continue;
                }
                let id = self.clients[i].id;
                let info = self.toplevel_info(i, &client);
                let pos = match m.handles.iter().position(|h| h.window == id) {
                    Some(p) => p,
                    None => {
                        let Ok(handle) = client.create_resource::<ZwlrForeignToplevelHandleV1, u32, Uwc>(
                            &dh,
                            m.manager.version(),
                            id,
                        ) else {
                            continue;
                        };
                        m.manager.toplevel(&handle);
                        m.handles.push(Handle { window: id, handle, sent: Info::default() });
                        m.handles.len() - 1
                    }
                };
                let h = &mut m.handles[pos];
                if h.sent == info {
                    continue;
                }
                if h.sent.title != info.title {
                    h.handle.title(info.title.clone());
                }
                if h.sent.app_id != info.app_id {
                    h.handle.app_id(info.app_id.clone());
                }
                for o in h.sent.outputs.iter().filter(|o| !info.outputs.contains(o) && o.is_alive()) {
                    h.handle.output_leave(o);
                }
                for o in info.outputs.iter().filter(|o| !h.sent.outputs.contains(o)) {
                    h.handle.output_enter(o);
                }
                if h.sent.states != info.states {
                    // fullscreen exists from version 2 on.
                    let v = h.handle.version();
                    let states: Vec<u8> = info
                        .states
                        .iter()
                        .filter(|s| v >= 2 || **s != TlState::Fullscreen as u32)
                        .flat_map(|s| s.to_ne_bytes())
                        .collect();
                    h.handle.state(states);
                }
                h.handle.done();
                h.sent = info;
            }
        }
        self.toplevels.managers = managers;
    }
}

impl GlobalDispatch<ZwlrForeignToplevelManagerV1, ()> for Uwc {
    fn bind(
        state: &mut Uwc,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ZwlrForeignToplevelManagerV1>,
        _: &(),
        init: &mut DataInit<'_, Uwc>,
    ) {
        let manager = init.init(resource, ());
        state.toplevels.managers.push(Manager { manager, handles: Vec::new() });
        // The existing windows right away.
        state.toplevels_refresh();
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for Uwc {
    fn request(
        state: &mut Uwc,
        _: &Client,
        manager: &ZwlrForeignToplevelManagerV1,
        request: zwlr_foreign_toplevel_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Uwc>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Request::Stop = request {
            manager.finished();
            state.toplevels.managers.retain(|m| &m.manager != manager);
        }
    }

    fn destroyed(state: &mut Uwc, _: ClientId, manager: &ZwlrForeignToplevelManagerV1, _: &()) {
        state.toplevels.managers.retain(|m| &m.manager != manager);
    }
}

impl Dispatch<ZwlrForeignToplevelHandleV1, u32> for Uwc {
    fn request(
        state: &mut Uwc,
        _: &Client,
        _: &ZwlrForeignToplevelHandleV1,
        request: zwlr_foreign_toplevel_handle_v1::Request,
        id: &u32,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Uwc>,
    ) {
        use zwlr_foreign_toplevel_handle_v1::Request as R;
        let id = *id;
        if state.client_index(id).is_none() {
            return;
        }
        match request {
            R::Activate { .. } => {
                let i = state.client_index(id).unwrap();
                if state.clients[i].minimized != 0 {
                    state.set_minimized(id, false);
                } else {
                    state.focus_and_reveal(id);
                }
            }
            R::Close => state.close_client(id),
            R::SetMinimized => state.set_minimized(id, true),
            R::UnsetMinimized => state.set_minimized(id, false),
            R::SetFullscreen { .. } => state.set_fullscreen(id, true),
            R::UnsetFullscreen => state.set_fullscreen(id, false),
            // No maximize on a canvas; the rectangle is for minimize animations uwc does not have.
            _ => {}
        }
    }

    fn destroyed(state: &mut Uwc, _: ClientId, handle: &ZwlrForeignToplevelHandleV1, _: &u32) {
        for m in &mut state.toplevels.managers {
            m.handles.retain(|h| &h.handle != handle);
        }
    }
}
