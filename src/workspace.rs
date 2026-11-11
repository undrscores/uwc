//! ext-workspace-v1: uwc's tags as workspaces, for bars and docks that speak the protocol.
//!
//! One workspace group (the monitor; there is one until multi-monitor), holding nine workspaces
//! named "1".."9" with stable ids `uwc-tag-N` and 1-D coordinates. States: active (the tag is
//! viewed) and urgent; tags are never hidden. Capabilities: activate and deactivate, applied on the
//! client's `commit` as one change:
//!
//! - only activations in the batch: view exactly those tags (what a click on a workspace button
//!   means: switch to it);
//! - any deactivation in the batch: toggle (add the activated tags, remove the deactivated ones),
//!   like `toggle-view-tag`.
//!
//! An empty result is ignored, as in dwm. Changes reach clients as one `done` per event-loop turn
//! (`workspace_refresh`, from the main loop), whatever caused them (keys, IPC, this protocol).

use smithay::{
    output::Output,
    reexports::{
        wayland_protocols::ext::workspace::v1::server::{
            ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1, GroupCapabilities},
            ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1, State as WsState, WorkspaceCapabilities},
            ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
        },
        wayland_server::{
            backend::{ClientId, ObjectId},
            protocol::wl_output::WlOutput,
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
        },
    },
};

use crate::{
    canvas::{ALLTAGS, TAGCOUNT},
    state::Uwc,
};

/// One bound manager (one per client that asked).
struct Manager {
    manager: ExtWorkspaceManagerV1,
    group: ExtWorkspaceGroupHandleV1,
    workspaces: Vec<ExtWorkspaceHandleV1>,
    /// The wl_outputs of this client the group has been announced on.
    outputs: Vec<WlOutput>,
    /// activate (true) / deactivate (false) requests since the last commit, by tag.
    pending: Vec<(usize, bool)>,
}

#[derive(Default)]
pub struct WorkspaceState {
    managers: Vec<Manager>,
    /// What clients were last told: the state bits of each tag.
    sent: Option<[u32; TAGCOUNT]>,
}

/// Which manager and tag a workspace handle stands for.
pub struct WorkspaceData {
    manager: ObjectId,
    tag: usize,
}

impl WorkspaceState {
    pub fn new(dh: &DisplayHandle) -> WorkspaceState {
        dh.create_global::<Uwc, ExtWorkspaceManagerV1, ()>(1, ());
        WorkspaceState::default()
    }
}

/// The ext-workspace state bits for every tag.
fn tag_states(state: &Uwc) -> [u32; TAGCOUNT] {
    let ts = state.tagset();
    let mut urgent = 0u32;
    for c in &state.clients {
        if c.urgent {
            urgent |= c.tags;
        }
    }
    std::array::from_fn(|i| {
        let mut s = WsState::empty();
        if ts & (1 << i) != 0 {
            s |= WsState::Active;
        }
        if urgent & (1 << i) != 0 {
            s |= WsState::Urgent;
        }
        s.bits()
    })
}

/// The new tag set for a batch of requests, or `None` to leave it alone.
pub fn apply_batch(tagset: u32, batch: &[(usize, bool)]) -> Option<u32> {
    if batch.is_empty() {
        return None;
    }
    let bit = |t: usize| 1u32 << t;
    let activated = batch.iter().filter(|(_, a)| *a).fold(0, |m, (t, _)| m | bit(*t));
    let deactivated = batch.iter().filter(|(_, a)| !*a).fold(0, |m, (t, _)| m | bit(*t));
    let next = if deactivated == 0 { activated } else { (tagset | activated) & !deactivated };
    let next = next & ALLTAGS;
    (next != 0 && next != tagset).then_some(next)
}

impl Uwc {
    fn workspace_outputs(&self) -> Vec<Output> {
        self.monitors.iter().map(|m| m.output.clone()).collect()
    }

    /// Send what changed since last time (tag states, outputs) with one `done` per manager.
    pub fn workspace_refresh(&mut self) {
        let states = tag_states(self);
        let changed = self.workspaces.sent != Some(states);
        self.workspaces.sent = Some(states);
        let outputs = self.workspace_outputs();
        let dh = self.display_handle.clone();
        for m in &mut self.workspaces.managers {
            let mut any = changed;
            if changed {
                for (ws, s) in m.workspaces.iter().zip(states) {
                    ws.state(WsState::from_bits_truncate(s));
                }
            }
            // Announce the group on outputs the client bound since (or that replaced the old one).
            if let Ok(client) = dh.get_client(m.manager.id()) {
                let now: Vec<WlOutput> =
                    outputs.iter().flat_map(|o| o.client_outputs(&client).collect::<Vec<_>>()).collect();
                for old in m.outputs.iter().filter(|o| !now.contains(o)) {
                    if old.is_alive() {
                        m.group.output_leave(old);
                    }
                    any = true;
                }
                for new in now.iter().filter(|o| !m.outputs.contains(o)) {
                    m.group.output_enter(new);
                    any = true;
                }
                m.outputs = now;
            }
            if any {
                m.manager.done();
            }
        }
    }

    fn workspace_commit(&mut self, manager: &ExtWorkspaceManagerV1) {
        let Some(m) = self.workspaces.managers.iter_mut().find(|m| &m.manager == manager) else { return };
        let batch = std::mem::take(&mut m.pending);
        if let Some(ts) = apply_batch(self.tagset(), &batch) {
            self.set_tagset(ts);
        }
    }
}

impl GlobalDispatch<ExtWorkspaceManagerV1, ()> for Uwc {
    fn can_view(client: smithay::reexports::wayland_server::Client, _: &()) -> bool {
        !crate::sandbox::is_sandboxed(&client)
    }

    fn bind(
        state: &mut Uwc,
        dh: &DisplayHandle,
        client: &Client,
        resource: New<ExtWorkspaceManagerV1>,
        _: &(),
        init: &mut DataInit<'_, Uwc>,
    ) {
        let manager = init.init(resource, ());
        let version = manager.version();
        let Ok(group) = client.create_resource::<ExtWorkspaceGroupHandleV1, (), Uwc>(dh, version, ()) else { return };
        manager.workspace_group(&group);
        // Workspaces cannot be created from a bar: they are the fixed tags.
        group.capabilities(GroupCapabilities::empty());
        let outputs: Vec<WlOutput> =
            state.workspace_outputs().iter().flat_map(|o| o.client_outputs(client).collect::<Vec<_>>()).collect();
        for o in &outputs {
            group.output_enter(o);
        }
        let states = tag_states(state);
        let mut workspaces = Vec::new();
        for (tag, s) in states.iter().enumerate() {
            let data = WorkspaceData { manager: manager.id(), tag };
            let Ok(ws) = client.create_resource::<ExtWorkspaceHandleV1, WorkspaceData, Uwc>(dh, version, data) else {
                return;
            };
            manager.workspace(&ws);
            ws.id(format!("uwc-tag-{}", tag + 1));
            ws.name((tag + 1).to_string());
            ws.coordinates(((tag + 1) as u32).to_ne_bytes().to_vec());
            ws.state(WsState::from_bits_truncate(*s));
            ws.capabilities(WorkspaceCapabilities::Activate | WorkspaceCapabilities::Deactivate);
            group.workspace_enter(&ws);
            workspaces.push(ws);
        }
        manager.done();
        state.workspaces.managers.push(Manager { manager, group, workspaces, outputs, pending: Vec::new() });
    }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for Uwc {
    fn request(
        state: &mut Uwc,
        _: &Client,
        manager: &ExtWorkspaceManagerV1,
        request: ext_workspace_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Uwc>,
    ) {
        match request {
            ext_workspace_manager_v1::Request::Commit => state.workspace_commit(manager),
            ext_workspace_manager_v1::Request::Stop => {
                manager.finished();
                state.workspaces.managers.retain(|m| &m.manager != manager);
            }
            _ => {}
        }
    }

    fn destroyed(state: &mut Uwc, _: ClientId, manager: &ExtWorkspaceManagerV1, _: &()) {
        state.workspaces.managers.retain(|m| &m.manager != manager);
    }
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for Uwc {
    fn request(
        _: &mut Uwc,
        _: &Client,
        _: &ExtWorkspaceGroupHandleV1,
        request: ext_workspace_group_handle_v1::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Uwc>,
    ) {
        // create_workspace is not offered (no capability); destroy needs nothing.
        let _ = request;
    }
}

impl Dispatch<ExtWorkspaceHandleV1, WorkspaceData> for Uwc {
    fn request(
        state: &mut Uwc,
        _: &Client,
        _: &ExtWorkspaceHandleV1,
        request: ext_workspace_handle_v1::Request,
        data: &WorkspaceData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Uwc>,
    ) {
        let want = match request {
            ext_workspace_handle_v1::Request::Activate => true,
            ext_workspace_handle_v1::Request::Deactivate => false,
            // remove and assign are not offered (no capability); destroy needs nothing.
            _ => return,
        };
        if let Some(m) = state.workspaces.managers.iter_mut().find(|m| m.manager.id() == data.manager) {
            m.pending.push((data.tag, want));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::apply_batch;

    #[test]
    fn batches_map_to_view_and_toggle() {
        // Activating one tag views it alone.
        assert_eq!(apply_batch(0b001, &[(2, true)]), Some(0b100));
        // Activating several views them together.
        assert_eq!(apply_batch(0b001, &[(1, true), (2, true)]), Some(0b110));
        // A deactivation makes the batch a toggle.
        assert_eq!(apply_batch(0b011, &[(1, false)]), Some(0b001));
        assert_eq!(apply_batch(0b001, &[(0, false), (3, true)]), Some(0b1000));
        // Nothing left visible, or no change: ignored.
        assert_eq!(apply_batch(0b001, &[(0, false)]), None);
        assert_eq!(apply_batch(0b001, &[(0, true)]), None);
        assert_eq!(apply_batch(0b001, &[]), None);
    }
}
