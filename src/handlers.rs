//! Wayland protocol handlers: wl_compositor, xdg-shell (+ decoration), shm, seat, data device, output.

use smithay::{
    backend::allocator::dmabuf::Dmabuf,
    backend::renderer::utils::on_commit_buffer_handler,
    delegate_compositor, delegate_data_device, delegate_dmabuf, delegate_output, delegate_seat, delegate_shm,
    delegate_xdg_decoration, delegate_xdg_shell,
    desktop::{utils::output_update, PopupKind, Window},
    input::{pointer::CursorImageStatus, Seat, SeatHandler, SeatState},
    reexports::{
        wayland_protocols::xdg::{
            decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecoMode, shell::server::xdg_toplevel,
        },
        wayland_server::{
            protocol::{wl_buffer, wl_seat, wl_surface::WlSurface},
            Client, Resource,
        },
    },
    utils::{Rectangle, Serial},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            get_parent, is_sync_subsurface, with_states, CompositorClientState, CompositorHandler, CompositorState,
        },
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        output::OutputHandler,
        selection::{
            data_device::{
                set_data_device_focus, ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
            },
            SelectionHandler,
        },
        shell::xdg::{
            decoration::XdgDecorationHandler, PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler,
            XdgShellState, XdgToplevelSurfaceData,
        },
        shm::{ShmHandler, ShmState},
    },
};

use crate::state::{Client as UClient, ClientState, Uwc};

impl CompositorHandler for Uwc {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn new_surface(&mut self, surface: &WlSurface) {
        Uwc::watch_buffer_readiness(surface);
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        if is_sync_subsurface(surface) {
            return;
        }
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }

        if let Some(i) = self.client_by_surface(&root) {
            self.clients[i].window.on_commit();
            let toplevel = self.clients[i].window.toplevel().unwrap().clone();
            let sent = with_states(&root, |s| {
                s.data_map.get::<XdgToplevelSurfaceData>().unwrap().lock().unwrap().initial_configure_sent
            });
            if !sent {
                toplevel.send_configure();
            }
            if !self.clients[i].mapped && self.clients[i].window.geometry().size.w > 0 {
                self.map_client(i);
            }
            self.ipc.dirty = true;
        }

        self.layer_commit(&root);

        self.popups.commit(surface);
        if let Some(PopupKind::Xdg(popup)) = self.popups.find_popup(surface) {
            if !popup.is_initial_configure_sent() {
                let _ = popup.send_configure();
            }
        }
        self.redraw = true;
    }
}

impl Uwc {
    /// The first buffer arrived: place the window and focus it.
    fn map_client(&mut self, i: usize) {
        let size = self.clients[i].size();
        let vp = self.viewport();
        let (ax, ay, aw, ah) = self.work_area();
        let n = self.clients.iter().filter(|c| c.mapped).count() as i32;
        let off = (n % 8) * self.config.new_window_offset;
        // Centre in the work area (the screen minus bars), cascading so windows don't hide each other exactly.
        let (mx, my) = vp.screen_to_canvas((ax + aw / 2.0, ay + ah / 2.0));
        let cx = mx - size.w as f64 / 2.0 + off as f64;
        let cy = my - size.h as f64 / 2.0 + off as f64;
        self.clients[i].canvas = (cx.round(), cy.round()).into();
        self.clients[i].mapped = true;
        self.clients[i].tags = self.tagset();
        if let Some(m) = &self.mon {
            let rect = Rectangle::from_size(m.size);
            output_update(&m.output, Some(rect), self.clients[i].surface());
        }
        let id = self.clients[i].id;
        self.focus_client(Some(id));
        self.refresh_pointer();
    }
}

impl BufferHandler for Uwc {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for Uwc {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl XdgShellHandler for Uwc {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let id = self.next_id;
        self.next_id += 1;
        self.clients.push(UClient {
            id,
            window: Window::new_wayland_window(surface),
            canvas: (0.0, 0.0).into(),
            tags: self.tagset(),
            mapped: false,
            border_ids: std::array::from_fn(|_| smithay::backend::renderer::element::Id::new()),
            deco_ids: std::array::from_fn(|_| smithay::backend::renderer::element::Id::new()),
        });
    }

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        surface.with_pending_state(|s| s.geometry = positioner.get_geometry());
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
    }

    fn reposition_request(&mut self, surface: PopupSurface, positioner: PositionerState, token: u32) {
        surface.with_pending_state(|s| {
            s.geometry = positioner.get_geometry();
            s.positioner = positioner;
        });
        surface.send_repositioned(token);
    }

    // TODO: popup grabs (click outside dismisses a menu) need a PointerGrab; popups work without.
    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {}

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        if let Some(i) = self.client_by_surface(surface.wl_surface()) {
            let gone = self.clients.remove(i);
            if self.focus == Some(gone.id) {
                self.focus = None;
                let top = self.clients.iter().rev().find(|c| self.visible(c)).map(|c| c.id);
                self.focus_client(top);
            }
            if let Some(crate::state::Drag::Move { id, .. } | crate::state::Drag::Resize { id, .. }) = self.drag {
                if id == gone.id {
                    self.drag = None;
                }
            }
            self.refresh_pointer();
            self.damage();
        }
    }

    // Windows float on the canvas; a request to maximize or fullscreen is acknowledged unchanged
    // (the client must get a configure back or it waits forever).
    fn maximize_request(&mut self, surface: ToplevelSurface) {
        surface.send_pending_configure();
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
    ) {
        surface.send_pending_configure();
    }

    fn move_request(&mut self, surface: ToplevelSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        if let Some(i) = self.client_by_surface(surface.wl_surface()) {
            self.begin_move(i);
        }
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        _seat: wl_seat::WlSeat,
        _serial: Serial,
        _edges: xdg_toplevel::ResizeEdge,
    ) {
        if let Some(i) = self.client_by_surface(surface.wl_surface()) {
            self.begin_resize(i);
        }
    }

    fn title_changed(&mut self, _surface: ToplevelSurface) {
        self.ipc.dirty = true;
    }

    fn app_id_changed(&mut self, _surface: ToplevelSurface) {
        self.ipc.dirty = true;
    }
}

impl XdgDecorationHandler for Uwc {
    // uwc has no client-side decorations to negotiate: windows get borders from the compositor.
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|s| s.decoration_mode = Some(DecoMode::ServerSide));
        toplevel.send_configure();
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecoMode) {
        toplevel.with_pending_state(|s| s.decoration_mode = Some(DecoMode::ServerSide));
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        self.request_mode(toplevel, DecoMode::ServerSide);
    }
}

impl SeatHandler for Uwc {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Uwc> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let dh = &self.display_handle;
        let client = focused.and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client);
        self.ipc.dirty = true;
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
        self.redraw = true;
    }
}

impl SelectionHandler for Uwc {
    type SelectionUserData = ();
}

impl DataDeviceHandler for Uwc {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for Uwc {}
impl ServerDndGrabHandler for Uwc {}

impl OutputHandler for Uwc {}

impl DmabufHandler for Uwc {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(&mut self, _global: &DmabufGlobal, dmabuf: Dmabuf, notifier: ImportNotifier) {
        if self.backend.import_dmabuf(&dmabuf) {
            let _ = notifier.successful::<Uwc>();
        } else {
            notifier.failed();
        }
    }
}

delegate_compositor!(Uwc);
delegate_shm!(Uwc);
delegate_xdg_shell!(Uwc);
delegate_xdg_decoration!(Uwc);
delegate_seat!(Uwc);
delegate_data_device!(Uwc);
delegate_output!(Uwc);
delegate_dmabuf!(Uwc);
