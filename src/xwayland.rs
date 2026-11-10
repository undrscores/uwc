//! Xwayland: X11 apps on the canvas. The server is spawned at startup (when the `xwayland`
//! option is on, in a real session); the window manager attaches once it is ready. An X11
//! window becomes a client like any other, pinned to the monitor it asked to be on (X windows
//! think in screen coordinates, so they stay put on screen, unzoomed, on every tag): it gets
//! uwc's decoration, focus, move / resize / fullscreen and the IPC. Override-redirect windows
//! (X menus, tooltips) are not managed: they are drawn on top and take the pointer by their
//! own geometry. The clipboard and primary selection are bridged both ways: when an X client
//! takes a selection it is re-published to wayland clients as a compositor selection, and when
//! a wayland client takes one the X server is told, so pasting works across the divide.

use smithay::{
    reexports::calloop::LoopHandle,
    utils::{Logical, Point, Rectangle, Size},
    wayland::{
        selection::{data_device, primary_selection, SelectionTarget},
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{
        xwm::{Reorder, ResizeEdge, XwmId},
        X11Surface, X11Wm, XWayland, XWaylandEvent, XwmHandler,
    },
};
use std::os::unix::io::OwnedFd;

use crate::{
    canvas::ALLTAGS,
    state::{sel_idx, Client, Fx, SelectionOwner, Shell, Uwc},
};

impl Uwc {
    /// The client that is this X11 window, if any, by its X window id.
    pub fn x11_client(&self, s: &X11Surface) -> Option<usize> {
        self.clients.iter().position(|c| c.x11().is_some_and(|x| x.window_id() == s.window_id()))
    }

    /// An X11 window asked to be mapped: make it a client, on the monitor it asked for (its X
    /// position is a point in the global input space), else centred in the pointer's monitor.
    fn map_x11(&mut self, s: X11Surface) {
        let _ = s.set_mapped(true);
        let g = s.geometry();
        let global: Point<f64, Logical> = (g.loc.x as f64, g.loc.y as f64).into();
        let m = self.mon_at(global);
        let o = self.origin_of(m);
        let size = self.monitors.get(m).map(|x| x.size).unwrap_or((1280, 720).into());
        let on_screen =
            global.x >= o.x && global.y >= o.y && global.x < o.x + size.w as f64 && global.y < o.y + size.h as f64;
        let (ax, ay, aw, ah) = self.work_area_of(m);
        let pin = if on_screen {
            global - o
        } else {
            ((ax + aw / 2.0 - g.size.w as f64 / 2.0).max(ax), (ay + ah / 2.0 - g.size.h as f64 / 2.0).max(ay)).into()
        };
        let id = self.next_id;
        self.next_id += 1;
        self.clients.push(Client {
            id,
            shell: Shell::X11(Box::new(s.clone())),
            canvas: (0.0, 0.0).into(),
            tags: ALLTAGS,
            mapped: true,
            ever_mapped: true,
            urgent: false,
            pin: Some(pin),
            pin_mon: m,
            fullscreen: false,
            fs_mon: m,
            prev_size: None,
            minimized: 0,
            open: Default::default(),
            scale: 1.0,
            fx: Fx::REST,
            commits: 0,
            sent_scale: None,
            border_ids: std::array::from_fn(|_| smithay::backend::renderer::element::Id::new()),
            deco_ids: std::array::from_fn(|_| smithay::backend::renderer::element::Id::new()),
        });
        // Tell it where it ended up on our screen.
        let _ = s.configure(Rectangle::new((o + pin).to_i32_round(), g.size));
        self.focus_client(Some(id));
        self.ipc.dirty = true;
        self.damage();
    }

    /// An override-redirect window's rectangle on the global input space, if it has a surface.
    pub fn or_geometry(&self, s: &X11Surface) -> Option<Rectangle<f64, Logical>> {
        s.wl_surface()?;
        let g = s.geometry();
        Some(Rectangle::new((g.loc.x as f64, g.loc.y as f64).into(), Size::from((g.size.w as f64, g.size.h as f64))))
    }

    /// Start Xwayland: spawn the server; the window manager attaches once it is ready. The child
    /// gets a cleared environment (smithay keeps only PATH), so the library path Xwayland needs
    /// in a non-standard setup is passed on explicitly.
    pub fn start_xwayland(&mut self) -> std::io::Result<()> {
        let envs: Vec<(String, String)> = ["LD_LIBRARY_PATH", "XDG_DATA_DIRS"]
            .iter()
            .filter_map(|k| std::env::var(k).ok().map(|v| ((*k).to_string(), v)))
            .collect();
        let (xwayland, client) = XWayland::spawn(
            &self.display_handle,
            None,
            envs,
            true,
            std::process::Stdio::null(),
            std::process::Stdio::null(),
            |_| (),
        )?;
        let handle: LoopHandle<'static, Uwc> = self.loop_handle.clone();
        let token = self
            .loop_handle
            .insert_source(xwayland, move |event, _, state| match event {
                XWaylandEvent::Ready { x11_socket, display_number } => {
                    match X11Wm::start_wm(handle.clone(), x11_socket, client.clone()) {
                        Ok(wm) => {
                            tracing::info!("xwayland: ready, display :{display_number}");
                            state.xwm = Some(wm);
                        }
                        Err(e) => tracing::error!("xwayland window manager: {e:#}"),
                    }
                }
                XWaylandEvent::Error => tracing::error!("xwayland: failed to start"),
            })
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        self.xwayland = Some(token);
        Ok(())
    }
}

impl XWaylandShellHandler for Uwc {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell
    }
}

impl XwmHandler for Uwc {
    fn xwm_state(&mut self, _: XwmId) -> &mut X11Wm {
        self.xwm.as_mut().expect("the xwm is running")
    }

    fn new_window(&mut self, _: XwmId, _: X11Surface) {}

    // An X client took a selection: re-publish it to wayland clients as a compositor selection.
    fn new_selection(&mut self, _: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        self.selection_owner[sel_idx(selection)] = SelectionOwner::X11;
        let mimes: Vec<String> = mime_types
            .iter()
            .filter_map(|m| match m.as_str() {
                "UTF8_STRING" | "STRING" | "TEXT" | "COMPOUND_TEXT" => Some("text/plain;charset=utf-8".to_string()),
                "TARGETS" | "TIMESTAMP" | "MULTIPLE" | "DELETE" | "SAVE_TARGETS" | "ATOM" | "ATOM_PAIR" => None,
                m => Some(m.to_string()),
            })
            .collect();
        let dh = self.display_handle.clone();
        let seat = self.seat.clone();
        match selection {
            SelectionTarget::Clipboard => data_device::set_data_device_selection::<Uwc>(&dh, &seat, mimes, ()),
            SelectionTarget::Primary => primary_selection::set_primary_selection::<Uwc>(&dh, &seat, mimes, ()),
        }
    }

    // The X selection owner is gone: clear the wayland side, but only if it was X-backed.
    fn cleared_selection(&mut self, _: XwmId, selection: SelectionTarget) {
        if self.selection_owner[sel_idx(selection)] != SelectionOwner::X11 {
            return;
        }
        self.selection_owner[sel_idx(selection)] = SelectionOwner::None;
        let dh = self.display_handle.clone();
        let seat = self.seat.clone();
        match selection {
            SelectionTarget::Clipboard => data_device::clear_data_device_selection::<Uwc>(&dh, &seat),
            SelectionTarget::Primary => primary_selection::clear_primary_selection::<Uwc>(&dh, &seat),
        }
    }

    // An X client wants to read the selection: only when a wayland client owns it.
    fn allow_selection_access(&mut self, _: XwmId, selection: SelectionTarget) -> bool {
        self.selection_owner[sel_idx(selection)] == SelectionOwner::Wayland
    }

    // Forward the X read request to the wayland client that owns the selection.
    fn send_selection(&mut self, _: XwmId, selection: SelectionTarget, mime_type: String, fd: OwnedFd) {
        let seat = self.seat.clone();
        match selection {
            SelectionTarget::Clipboard => {
                let _ = data_device::request_data_device_client_selection::<Uwc>(&seat, mime_type, fd);
            }
            SelectionTarget::Primary => {
                let _ = primary_selection::request_primary_client_selection::<Uwc>(&seat, mime_type, fd);
            }
        }
    }

    fn new_override_redirect_window(&mut self, _: XwmId, _: X11Surface) {}

    fn mapped_override_redirect_window(&mut self, _: XwmId, s: X11Surface) {
        self.or_windows.push(s);
        self.damage();
    }

    fn map_window_request(&mut self, _: XwmId, s: X11Surface) {
        self.map_x11(s);
    }

    fn unmapped_window(&mut self, _: XwmId, s: X11Surface) {
        if let Some(i) = self.x11_client(&s) {
            self.remove_client(i);
        }
        self.damage();
    }

    fn destroyed_window(&mut self, _: XwmId, s: X11Surface) {
        if let Some(i) = self.x11_client(&s) {
            self.remove_client(i);
        }
        self.or_windows.retain(|w| w.window_id() != s.window_id());
        self.damage();
    }

    fn configure_request(
        &mut self,
        _: XwmId,
        s: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        // Grant the request, in our coordinates for a managed window.
        let g = s.geometry();
        let rect = match self.x11_client(&s) {
            Some(i) => {
                let at = self.x11_screen_pos(&self.clients[i]);
                Rectangle::new(
                    (x.unwrap_or(at.x), y.unwrap_or(at.y)).into(),
                    Size::from((w.unwrap_or(g.size.w as u32) as i32, h.unwrap_or(g.size.h as u32) as i32)),
                )
            }
            None => Rectangle::new(
                (x.unwrap_or(g.loc.x), y.unwrap_or(g.loc.y)).into(),
                Size::from((w.unwrap_or(g.size.w as u32) as i32, h.unwrap_or(g.size.h as u32) as i32)),
            ),
        };
        let _ = s.configure(rect);
        self.damage();
    }

    fn configure_notify(&mut self, _: XwmId, _: X11Surface, _: Rectangle<i32, Logical>, _: Option<u32>) {
        self.damage();
    }

    fn maximize_request(&mut self, _: XwmId, s: X11Surface) {
        let _ = s.set_maximized(true);
    }

    fn unmaximize_request(&mut self, _: XwmId, s: X11Surface) {
        let _ = s.set_maximized(false);
    }

    fn fullscreen_request(&mut self, _: XwmId, s: X11Surface) {
        if let Some(i) = self.x11_client(&s) {
            let id = self.clients[i].id;
            self.set_fullscreen(id, true);
        }
    }

    fn unfullscreen_request(&mut self, _: XwmId, s: X11Surface) {
        if let Some(i) = self.x11_client(&s) {
            let id = self.clients[i].id;
            self.set_fullscreen(id, false);
        }
    }

    fn minimize_request(&mut self, _: XwmId, s: X11Surface) {
        if let Some(i) = self.x11_client(&s) {
            let id = self.clients[i].id;
            self.set_minimized(id, true);
        }
    }

    fn unminimize_request(&mut self, _: XwmId, s: X11Surface) {
        if let Some(i) = self.x11_client(&s) {
            let id = self.clients[i].id;
            self.set_minimized(id, false);
        }
    }

    fn resize_request(&mut self, _: XwmId, s: X11Surface, _button: u32, _edge: ResizeEdge) {
        // The X client holds an implicit grab; uwc's own drag takes over from here and ends
        // on the button release, which the grab delivers.
        if let Some(i) = self.x11_client(&s) {
            self.begin_resize(i);
        }
    }

    fn move_request(&mut self, _: XwmId, s: X11Surface, _button: u32) {
        if let Some(i) = self.x11_client(&s) {
            self.begin_move(i);
        }
    }
}
