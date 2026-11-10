//! Wayland protocol handlers: wl_compositor, xdg-shell (+ decoration), shm, seat, data device, output.

use std::os::unix::io::OwnedFd;

use smithay::{
    backend::allocator::dmabuf::Dmabuf,
    backend::renderer::utils::on_commit_buffer_handler,
    delegate_compositor, delegate_data_device, delegate_dmabuf, delegate_idle_inhibit, delegate_idle_notify,
    delegate_input_method_manager, delegate_output, delegate_pointer_constraints, delegate_pointer_gestures,
    delegate_primary_selection, delegate_relative_pointer, delegate_seat, delegate_session_lock, delegate_shm,
    delegate_text_input_manager, delegate_xdg_activation, delegate_xdg_decoration, delegate_xdg_shell,
    delegate_xwayland_shell,
    desktop::{utils::output_update, PopupKeyboardGrab, PopupKind, PopupPointerGrab, Window},
    input::{
        pointer::{CursorImageStatus, Focus},
        Seat, SeatHandler, SeatState,
    },
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
            get_parent, is_sync_subsurface, with_states, BufferAssignment, CompositorClientState, CompositorHandler,
            CompositorState, SurfaceAttributes,
        },
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        output::OutputHandler,
        selection::{
            data_device::{
                set_data_device_focus, ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
            },
            primary_selection::{PrimarySelectionHandler, PrimarySelectionState},
            SelectionHandler, SelectionSource, SelectionTarget,
        },
        shell::xdg::{
            decoration::XdgDecorationHandler, PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler,
            XdgShellState, XdgToplevelSurfaceData,
        },
        shm::{ShmHandler, ShmState},
        text_input::TextInputSeat,
    },
};

use crate::{
    canvas::ALLTAGS,
    state::{Client as UClient, ClientState, Shell, Uwc},
};

impl CompositorHandler for Uwc {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        if let Some(d) = client.get_data::<ClientState>() {
            return &d.compositor_state;
        }
        // The Xwayland server's client carries its own.
        &client.get_data::<smithay::xwayland::XWaylandClientData>().unwrap().compositor_state
    }

    fn new_surface(&mut self, surface: &WlSurface) {
        Uwc::watch_buffer_readiness(surface);
    }

    fn commit(&mut self, surface: &WlSurface) {
        // `on_commit_buffer_handler` consumes the buffer assignment, so read it first: a NULL attach
        // (`Removed`) is the client hiding its window, and it may commit a buffer again later.
        let (attached, removed) = with_states(surface, |s| {
            let mut guard = s.cached_state.get::<SurfaceAttributes>();
            let b = guard.current().buffer.as_ref();
            (matches!(b, Some(BufferAssignment::NewBuffer(_))), matches!(b, Some(BufferAssignment::Removed)))
        });
        on_commit_buffer_handler::<Self>(surface);
        if is_sync_subsurface(surface) {
            return;
        }
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }

        if let Some(i) = self.client_by_surface(&root) {
            self.clients[i].commits += 1;
            match self.clients[i].shell.clone() {
                Shell::Wayland(w) => {
                    w.on_commit();
                    let toplevel = w.toplevel().unwrap().clone();
                    let sent = with_states(&root, |s| {
                        s.data_map.get::<XdgToplevelSurfaceData>().unwrap().lock().unwrap().initial_configure_sent
                    });
                    if !sent {
                        self.open_rules(i);
                        toplevel.send_configure();
                    }
                    if root == *surface {
                        if !self.clients[i].mapped {
                            if attached && w.geometry().size.w > 0 {
                                self.map_client(i);
                            }
                        } else if removed {
                            self.unmap_client(i);
                        }
                    }
                }
                // An X11 window maps when the X server asks for it, not on a commit.
                Shell::X11(_) => {}
            }
            self.ipc.dirty = true;
            let id = self.clients[i].id;
            self.export_on_commit(id);
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
    /// Before the first configure: decide what the rules' `open { }` blocks say (app-id and title are
    /// set by then), and put the size and fullscreen state into that configure so the window never
    /// shows at the wrong size.
    fn open_rules(&mut self, i: usize) {
        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State;
        let c = &self.clients[i];
        let (app_id, title) = (c.app_id(), c.title());
        let f = self.facts(c, &app_id, &title);
        let rules: Vec<&crate::rules::Rule> = self.config.rules.iter().chain(&self.ipc_rules).collect();
        let mut open = crate::rules::resolve_open(&rules, &f);
        // Where it was last time wins over the rules (session restore).
        if self.config.session_restore {
            if let Some(saved) = crate::session::take(&mut self.session_pool, &app_id, &title) {
                saved.apply(&mut open);
            }
        }
        let screen = self.monitors.get(self.cur).map(|m| m.size).unwrap_or((1280, 720).into());
        let c = &mut self.clients[i];
        if let Shell::Wayland(w) = &c.shell {
            if let Some(t) = w.toplevel() {
                t.with_pending_state(|s| {
                    if let Some((w, h)) = open.size {
                        s.size = Some((w, h).into());
                    }
                    if open.fullscreen {
                        s.states.set(State::Fullscreen);
                        s.size = Some(screen);
                    }
                });
            }
        }
        if open.fullscreen {
            c.fullscreen = true;
            c.fs_mon = open.fs_mon.min(self.monitors.len().saturating_sub(1));
            c.prev_size = open.size.map(Into::into);
        }
        c.open = open;
    }

    /// The first buffer arrived: place the window as its rules say (else centred in the work area,
    /// cascading), and focus it. A window that comes back after hiding itself maps where it was.
    fn map_client(&mut self, i: usize) {
        let n = self.clients.iter().filter(|c| c.mapped).count() as i32;
        self.clients[i].mapped = true;
        let first_map = !self.clients[i].ever_mapped;
        if !self.clients[i].ever_mapped {
            self.clients[i].ever_mapped = true;
            let open = self.clients[i].open.clone();
            let size = self.clients[i].size();
            let vp = self.viewport();
            let (ax, ay, aw, ah) = self.work_area();
            let off = (n % 8) * self.config.new_window_offset;
            let (mx, my) = vp.screen_to_canvas((ax + aw / 2.0, ay + ah / 2.0));
            let mut centred = (mx - size.w as f64 / 2.0 + off as f64, my - size.h as f64 / 2.0 + off as f64);
            // A dialog (xdg parent, X11 transient-for) opens over its parent, on its tags, pinned
            // if it is: on an infinite canvas "somewhere in the middle" is nowhere near the window
            // it belongs to.
            let parent = self.parent_of(i).map(|p| {
                let p = &self.clients[p];
                (p.rect(), p.tags, p.pin, p.pin_mon, p.scale)
            });
            let mut open = open;
            let c = &mut self.clients[i];
            c.scale = open.scale.unwrap_or(1.0);
            c.tags = open.tags.unwrap_or(self.tagset);
            if let Some((pr, ptags, ppin, pmon, pscale)) = parent {
                c.scale = open.scale.unwrap_or(pscale);
                let (w, h) = (size.w as f64 * c.scale, size.h as f64 * c.scale);
                centred = (pr.loc.x + (pr.size.w - w) / 2.0, pr.loc.y + (pr.size.h - h) / 2.0);
                c.tags = open.tags.unwrap_or(ptags);
                if let Some(pp) = ppin {
                    open.pinned = true;
                    open.pin_mon = pmon;
                    if open.position.is_none() {
                        open.position = Some((pp.x + (pr.size.w - w) / 2.0, pp.y + (pr.size.h - h) / 2.0));
                    }
                }
            }
            if open.pinned && !c.fullscreen {
                // A pinned window's position is a screen position, on the monitor it opens on.
                let at = open.position.unwrap_or(vp.canvas_to_screen(centred));
                c.pin_mon = open.pin_mon.min(self.monitors.len().saturating_sub(1));
                c.pin = Some(at.into());
                c.canvas = vp.screen_to_canvas(at).into();
                c.tags = ALLTAGS;
            } else {
                let (x, y) = open.position.unwrap_or(centred);
                c.canvas = (x.round(), y.round()).into();
            }
            if self.config.open_ms > 0.0 && !open.minimized && !self.clients[i].fullscreen {
                if !self.anim_running() {
                    self.begin_anim();
                }
                let c = &mut self.clients[i];
                c.fx = crate::state::Fx {
                    alpha: 0.0,
                    scale: self.config.open_scale,
                    fade_ms: self.config.open_ms,
                    ..crate::state::Fx::REST
                };
            }
            if open.minimized {
                self.minimize_stamp += 1;
                self.clients[i].minimized = self.minimize_stamp;
            }
        }
        if let Some(surface) = self.clients[i].surface() {
            for m in &self.monitors {
                let rect = Rectangle::from_size(m.size);
                output_update(&m.output, Some(rect), &surface);
            }
        }
        let id = self.clients[i].id;
        // It takes the keyboard unless a rule says not to, or it opened out of sight.
        if self.clients[i].open.focus != Some(false) && self.visible(&self.clients[i]) {
            self.focus_client(Some(id));
        }
        if first_map {
            self.run_hook(crate::hooks::Hook::WindowOpen, self.client_index(id));
        }
        self.refresh_pointer();
        self.damage();
    }

    /// The window `i` is a dialog of, if any.
    fn parent_of(&self, i: usize) -> Option<usize> {
        let parent = match &self.clients[i].shell {
            Shell::Wayland(w) => w.toplevel()?.parent().and_then(|s| self.client_by_surface(&s)),
            Shell::X11(s) => {
                let id = s.is_transient_for()?;
                self.clients.iter().position(|c| c.x11().is_some_and(|x| x.window_id() == id))
            }
        };
        parent.filter(|&p| p != i && self.clients[p].mapped)
    }

    /// The client took its buffer back (a NULL attach): hide it until it maps again.
    fn unmap_client(&mut self, i: usize) {
        self.clients[i].mapped = false;
        let id = self.clients[i].id;
        self.export_on_close(id);
        if self.focus == Some(id) {
            self.focus = None;
            let top = self.clients.iter().rev().find(|c| self.visible(c)).map(|c| c.id);
            self.focus_client(top);
        }
        if let Some(crate::state::Drag::Move { id: d, .. } | crate::state::Drag::Resize { id: d, .. }) = self.drag {
            if d == id {
                self.drag = None;
            }
        }
        self.refresh_pointer();
        self.damage();
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
            shell: Shell::Wayland(Window::new_wayland_window(surface)),
            canvas: (0.0, 0.0).into(),
            tags: self.tagset(),
            mapped: false,
            ever_mapped: false,
            urgent: false,
            pin: None,
            pin_mon: 0,
            fullscreen: false,
            fs_mon: 0,
            prev_size: None,
            minimized: 0,
            open: Default::default(),
            scale: 1.0,
            fx: crate::state::Fx::REST,
            sent_scale: None,
            commits: 0,
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

    // A popup asked for a grab (a menu opened by a click): smithay's PopupGrab redirects keyboard
    // input to it and dismisses it when a press lands outside it (or its client), so the click that
    // closes a menu does not also focus or drag what is under it.
    fn grab(&mut self, surface: PopupSurface, _seat: wl_seat::WlSeat, serial: Serial) {
        let kind = PopupKind::Xdg(surface);
        let Ok(root) = smithay::desktop::find_popup_root_surface(&kind) else { return };
        let seat = self.seat.clone();
        match self.popups.grab_popup(root, kind, &seat, serial) {
            Ok(grab) => {
                if let Some(kb) = seat.get_keyboard() {
                    kb.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
                }
                if let Some(ptr) = seat.get_pointer() {
                    ptr.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
                }
            }
            Err(e) => tracing::warn!("popup grab: {e}"),
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        if let Some(i) = self.client_by_surface(surface.wl_surface()) {
            self.remove_client(i);
        }
    }

    // Windows float on the canvas; a request to maximize is acknowledged unchanged (the client must
    // get a configure back or it waits forever).
    fn maximize_request(&mut self, surface: ToplevelSurface) {
        if surface.is_initial_configure_sent() {
            surface.send_pending_configure();
        }
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
    ) {
        match self.client_by_surface(surface.wl_surface()) {
            Some(i) => {
                let id = self.clients[i].id;
                self.set_fullscreen(id, true);
            }
            None => {
                surface.send_pending_configure();
            }
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        if let Some(i) = self.client_by_surface(surface.wl_surface()) {
            let id = self.clients[i].id;
            self.set_fullscreen(id, false);
        }
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
        // Text input follows the keyboard focus, so the IME knows where its text goes.
        let ti = seat.text_input();
        ti.leave();
        ti.set_focus(focused.cloned());
        if focused.is_some() {
            ti.enter();
        }
        self.ipc.dirty = true;
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
        self.redraw = true;
    }
}

impl SelectionHandler for Uwc {
    type SelectionUserData = ();

    // A wayland client took (or cleared) a selection: remember the owner and tell the X server,
    // so X clients see it.
    fn new_selection(&mut self, ty: SelectionTarget, source: Option<SelectionSource>, _seat: Seat<Self>) {
        self.selection_owner[crate::state::sel_idx(ty)] =
            if source.is_some() { crate::state::SelectionOwner::Wayland } else { crate::state::SelectionOwner::None };
        if let Some(xwm) = self.xwm.as_mut() {
            let _ = xwm.new_selection(ty, source.map(|s| s.mime_types()));
        }
    }

    // A wayland client reads an X11-owned selection: forward the request to the X server.
    fn send_selection(&mut self, ty: SelectionTarget, mime_type: String, fd: OwnedFd, _seat: Seat<Self>, _: &()) {
        if let Some(xwm) = self.xwm.as_mut() {
            let _ = xwm.send_selection(ty, mime_type, fd, self.loop_handle.clone());
        }
    }
}

impl PrimarySelectionHandler for Uwc {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.primary_selection
    }
}

delegate_primary_selection!(Uwc);

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
delegate_xdg_activation!(Uwc);
delegate_session_lock!(Uwc);
delegate_pointer_constraints!(Uwc);
delegate_relative_pointer!(Uwc);
delegate_xwayland_shell!(Uwc);
delegate_text_input_manager!(Uwc);
delegate_pointer_gestures!(Uwc);
delegate_input_method_manager!(Uwc);
delegate_idle_notify!(Uwc);
delegate_idle_inhibit!(Uwc);
