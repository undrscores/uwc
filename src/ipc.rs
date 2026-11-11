//! Control socket. Same shape as uwm's: text command lines in, JSON lines out
//! (`{"ok":true}`, `{"error":"..."}`, or an `{"event":...}` snapshot), so a shell written for
//! uwm's `state`/`subscribe` can follow uwc too.

use std::{
    ffi::OsStr,
    io::{ErrorKind, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
};

use serde_json::{Map, Value};
use smithay::reexports::calloop::{generic::Generic, Interest, LoopHandle, Mode, PostAction};

use crate::{
    canvas::{Viewport, TAGCOUNT},
    protocol as P,
    state::Uwc,
};

const MAX_LINE: usize = 511;

struct Conn {
    id: u32,
    out: UnixStream,
    subscribed: bool,
}

pub struct Ipc {
    pub path: PathBuf,
    conns: Vec<Conn>,
    next: u32,
    /// Something a subscriber would see changed; flushed once per event-loop turn.
    pub dirty: bool,
}

impl Drop for Ipc {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Where the control socket lives: `$XDG_RUNTIME_DIR` (which logind makes private), else a directory
/// of our own under the temp dir, created for the owner alone and refused if it is anyone else's
/// (a shared /tmp is where another user could plant a socket or a symlink).
pub fn socket_dir() -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR") {
        return Ok(PathBuf::from(d));
    }
    // Safety-free uid lookup: the owner of our own /proc entry.
    let uid = std::fs::metadata("/proc/self")?.uid();
    let dir = std::env::temp_dir().join(format!("uwc-{uid}"));
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    let meta = std::fs::symlink_metadata(&dir)?;
    if !meta.is_dir() || meta.uid() != uid || meta.permissions().mode() & 0o077 != 0 {
        anyhow::bail!(
            "{} exists but is not a private directory of yours: set XDG_RUNTIME_DIR (a login through logind does) or remove it",
            dir.display()
        );
    }
    Ok(dir)
}

impl Ipc {
    pub fn new(handle: &LoopHandle<'static, Uwc>, wayland_display: &OsStr) -> anyhow::Result<Ipc> {
        let dir = socket_dir()?;
        let path = dir.join(format!("uwc-{}.sock", wayland_display.to_string_lossy()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        // The control socket can move and close windows and change options: only its owner may connect.
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        listener.set_nonblocking(true)?;
        handle
            .insert_source(Generic::new(listener, Interest::READ, Mode::Level), |_, listener, state| {
                // Safety: the listener is not dropped while we use it.
                let listener = unsafe { listener.get_mut() };
                loop {
                    match listener.accept() {
                        Ok((stream, _)) => state.ipc_accept(stream),
                        Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                        Err(e) => {
                            tracing::warn!("ipc accept: {e}");
                            break;
                        }
                    }
                }
                Ok(PostAction::Continue)
            })
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(Ipc { path, conns: Vec::new(), next: 1, dirty: false })
    }
}

impl Uwc {
    fn ipc_accept(&mut self, stream: UnixStream) {
        if stream.set_nonblocking(true).is_err() {
            return;
        }
        let Ok(out) = stream.try_clone() else { return };
        let id = self.ipc.next;
        self.ipc.next += 1;
        self.ipc.conns.push(Conn { id, out, subscribed: false });

        let mut buf: Vec<u8> = Vec::new();
        let res = self.loop_handle.insert_source(
            Generic::new(stream, Interest::READ, Mode::Level),
            move |_, stream, state| {
                // Safety: the stream is not dropped while we use it.
                let stream = unsafe { stream.get_mut() };
                let mut chunk = [0u8; 1024];
                loop {
                    match stream.read(&mut chunk) {
                        Ok(0) => {
                            state.ipc_drop(id);
                            return Ok(PostAction::Remove);
                        }
                        Ok(n) => {
                            buf.extend_from_slice(&chunk[..n]);
                            // Stop reading a line that cannot be a command: a writer faster than this loop
                            // must not be able to grow the buffer without bound.
                            if buf.len() > MAX_LINE && !buf.contains(&b'\n') {
                                state.ipc_drop(id);
                                return Ok(PostAction::Remove);
                            }
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                        Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                        Err(_) => {
                            state.ipc_drop(id);
                            return Ok(PostAction::Remove);
                        }
                    }
                }
                while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=nl).collect();
                    let line = String::from_utf8_lossy(&line[..line.len() - 1]).trim().to_string();
                    if line.is_empty() {
                        continue;
                    }
                    state.ipc_line(id, &line);
                }
                if buf.len() > MAX_LINE {
                    state.ipc_drop(id);
                    return Ok(PostAction::Remove);
                }
                // The connection may have been dropped by a failed write above.
                if !state.ipc.conns.iter().any(|c| c.id == id) {
                    return Ok(PostAction::Remove);
                }
                Ok(PostAction::Continue)
            },
        );
        if res.is_err() {
            self.ipc_drop(id);
        }
    }

    fn ipc_drop(&mut self, id: u32) {
        self.ipc.conns.retain(|c| c.id != id);
    }

    /// Write one JSON line to a client; a client that cannot keep up is dropped.
    fn ipc_send(&mut self, id: u32, v: &Value) {
        let mut line = v.to_string();
        line.push('\n');
        let Some(c) = self.ipc.conns.iter_mut().find(|c| c.id == id) else { return };
        if c.out.write_all(line.as_bytes()).is_err() {
            self.ipc_drop(id);
        }
    }

    /// Send the snapshot to subscribers if anything changed since the last turn.
    pub fn ipc_flush(&mut self) {
        if !std::mem::take(&mut self.ipc.dirty) {
            return;
        }
        let ids: Vec<u32> = self.ipc.conns.iter().filter(|c| c.subscribed).map(|c| c.id).collect();
        if ids.is_empty() {
            return;
        }
        let snap = self.ipc_state();
        for id in ids {
            self.ipc_send(id, &snap);
        }
    }

    pub fn ipc_state(&self) -> Value {
        serde_json::to_value(self.snapshot()).expect("the snapshot serializes")
    }

    /// `minimap`: what an overview strip needs, in canvas px — per tag the bounds of its windows
    /// and the windows themselves (id for the preview, rect, icon), and per monitor each tag's view.
    fn ipc_minimap(&self) -> Value {
        use serde_json::json;
        let rj = |r: &smithay::utils::Rectangle<f64, smithay::utils::Logical>| json!({"x": r.loc.x.round() as i64, "y": r.loc.y.round() as i64, "width": r.size.w.round() as i64, "height": r.size.h.round() as i64});
        let ts = self.tagset();
        let tags: Vec<Value> = (0..TAGCOUNT)
            .map(|i| {
                let wins: Vec<&crate::state::Client> =
                    self.clients.iter().filter(|c| c.mapped && c.tags & (1 << i) != 0).collect();
                let bounds = wins.iter().map(|c| c.rect()).reduce(|a, b| {
                    let loc = (a.loc.x.min(b.loc.x), a.loc.y.min(b.loc.y)).into();
                    let end =
                        ((a.loc.x + a.size.w).max(b.loc.x + b.size.w), (a.loc.y + a.size.h).max(b.loc.y + b.size.h));
                    smithay::utils::Rectangle::new(loc, ((end.0 - loc.x).max(0.0), (end.1 - loc.y).max(0.0)).into())
                });
                json!({
                    "n": i + 1,
                    "active": ts & (1 << i) != 0,
                    "occupied": !wins.is_empty(),
                    "bounds": bounds.as_ref().map(rj),
                    "windows": wins.iter().map(|c| json!({
                        "id": c.id,
                        "title": c.title(),
                        "app_id": c.app_id(),
                        "rect": rj(&c.rect()),
                        "focused": self.focus == Some(c.id),
                        "minimized": c.minimized != 0,
                        "fullscreen": c.fullscreen,
                        "pinned": c.pin.is_some(),
                        "icon": c.icon_path.as_ref().map(|p| p.display().to_string()),
                        "icon_name": c.icon_name,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        let monitors: Vec<Value> = self
            .monitors
            .iter()
            .map(|m| {
                json!({
                    "name": m.output.name(),
                    "rect": {"x": m.origin.x as i64, "y": m.origin.y as i64, "width": m.size.w as i64, "height": m.size.h as i64},
                    "views": (0..TAGCOUNT).map(|i| {
                        let v = m.view_of(i, ts);
                        json!({"tag": i + 1, "x": v.x.round() as i64, "y": v.y.round() as i64, "zoom": v.zoom})
                    }).collect::<Vec<_>>(),
                })
            })
            .collect();
        json!({"tags": tags, "monitors": monitors})
    }

    fn snapshot(&self) -> P::Event {
        let view = |v: &Viewport| P::View { x: v.x.round() as i64, y: v.y.round() as i64, zoom: v.zoom };
        let ts = self.tagset();
        let (mut occ, mut urg) = (0u32, 0u32);
        for c in &self.clients {
            occ |= c.tags;
            if c.urgent {
                urg |= c.tags;
            }
        }
        let monitors = self
            .monitors
            .iter()
            .enumerate()
            .map(|(mi, m)| {
                let wa = self.work_area_of(mi);
                P::Monitor {
                    name: m.output.name(),
                    focused: mi == self.cur,
                    layout: "><>",
                    rect: P::Rect {
                        x: m.origin.x as i64,
                        y: m.origin.y as i64,
                        width: m.size.w as i64,
                        height: m.size.h as i64,
                    },
                    scale: m.output.current_scale().fractional_scale(),
                    viewport: view(&m.target_viewport(ts)),
                    view_moving: m.target.is_some() || self.fling.is_some_and(|f| f.mon == mi),
                    animating: self.anim_running(),
                    work_area: P::Rect { x: wa.0 as i64, y: wa.1 as i64, width: wa.2 as i64, height: wa.3 as i64 },
                    layers: self.layer_info(),
                    tags: (0..TAGCOUNT)
                        .map(|i| P::Tag {
                            n: i + 1,
                            active: ts & (1 << i) != 0,
                            occupied: occ & (1 << i) != 0,
                            urgent: urg & (1 << i) != 0,
                            viewport: view(&m.view_of(i, ts)),
                            bookmarks: m.bookmarks[i]
                                .iter()
                                .enumerate()
                                .filter_map(|(n, b)| b.as_ref().map(|v| P::Bookmark { n: n + 1, view: view(v) }))
                                .collect(),
                            tiling: self.tiling.tags[i].state.layout.map(|l| P::Tiling {
                                layout: l.name(),
                                ratio: self.tile_opts(crate::tiling::Region::Tag(i)).ratio,
                                masters: self.tile_opts(crate::tiling::Region::Tag(i)).masters,
                                area: {
                                    let a = self.tiling.tags[i].area;
                                    P::Rect {
                                        x: a.loc.x as i64,
                                        y: a.loc.y as i64,
                                        width: a.size.w as i64,
                                        height: a.size.h as i64,
                                    }
                                },
                            }),
                        })
                        .collect(),
                }
            })
            .collect();
        let windows = self
            .clients
            .iter()
            .filter(|c| c.mapped)
            .map(|c| {
                let mut r = c.rect();
                // A pinned window's canvas position is wherever its screen spot is over the canvas now.
                if let Some(p) = c.pin {
                    r.loc = self.target_viewport_of(c.pin_mon).screen_to_canvas((p.x, p.y)).into();
                }
                let name = self.monitors.get(self.mon_of(c)).map(|m| m.output.name()).unwrap_or_default();
                P::Window {
                    id: c.id,
                    title: c.title(),
                    app_id: c.app_id(),
                    monitor: name.clone(),
                    tags: c.tags,
                    focused: self.focus == Some(c.id),
                    floating: self.client_index(c.id).is_none_or(|i| self.tile_region(i).is_none()),
                    fullscreen: c.fullscreen,
                    urgent: c.urgent,
                    // The client's own size (what `resize` sets); the footprint is that times `scale`.
                    rect: P::Rect {
                        x: r.loc.x.round() as i64,
                        y: r.loc.y.round() as i64,
                        width: c.size().w as i64,
                        height: c.size().h as i64,
                    },
                    pinned: c.pin.is_some(),
                    minimized: c.minimized != 0,
                    scale: c.scale,
                    icon: c.icon_path.as_ref().map(|p| p.display().to_string()),
                    icon_name: c.icon_name.clone(),
                }
            })
            .collect();
        // The monitor the pointer is on, the way bars pick theirs.
        let selmon = self.monitors.get(self.cur).map(|m| m.output.name()).unwrap_or_default();
        P::Event::State {
            selmon,
            config_error: self.config_error.clone(),
            config_rev: self.config_rev,
            shader_errors: self.shader_errors.clone(),
            backend: self.backend.name(),
            keyboard_focus: self.keyboard_focus(),
            keyboard_layout: P::KeyboardLayout {
                index: self.layout.0,
                name: self.layout.1.clone(),
                count: self.layout.2,
            },
            hints: self.hints_active(),
            monitors,
            cursor: P::Cursor { x: self.cursor.x.round() as i64, y: self.cursor.y.round() as i64, hidden: false },
            windows,
        }
    }

    /// Where the seat's keyboard really is.
    fn keyboard_focus(&self) -> Option<P::KeyboardFocus> {
        let f = self.keyboard().current_focus()?;
        if let Some(i) = self.client_by_surface(&f) {
            return Some(P::KeyboardFocus::Window(self.clients[i].id));
        }
        if let Some(o) = self.output() {
            let map = smithay::desktop::layer_map_for_output(&o);
            if let Some(l) = map.layer_for_surface(&f, smithay::desktop::WindowSurfaceType::TOPLEVEL) {
                return Some(P::KeyboardFocus::Layer(l.namespace().to_string()));
            }
        }
        Some(P::KeyboardFocus::Other(true))
    }

    fn ipc_options(&self) -> Map<String, Value> {
        use crate::options::{json, DECO, GLOBAL};
        let mut all = json(GLOBAL, &self.config);
        all.extend(json(DECO, &self.config.deco));
        all
    }

    fn ipc_line(&mut self, id: u32, line: &str) {
        let reply = match P::Request::parse(line).and_then(|r| self.ipc_exec(id, r)) {
            Ok(Some(v)) => v,
            Ok(None) => serde_json::to_value(P::Ok { ok: true }).unwrap(),
            Err(error) => serde_json::to_value(P::Error { error }).unwrap(),
        };
        self.ipc_send(id, &reply);
    }

    fn event(e: P::Event) -> Result<Option<Value>, String> {
        Ok(Some(serde_json::to_value(e).expect("events serialize")))
    }

    fn ipc_exec(&mut self, id: u32, req: P::Request) -> Result<Option<Value>, String> {
        use P::Request as R;
        let window = |s: &Uwc, w: u32| s.client_index(w).ok_or_else(|| format!("no window {w}"));
        match req {
            R::State => return Ok(Some(self.ipc_state())),
            R::Minimap => return Ok(Some(self.ipc_minimap())),
            R::Subscribe => {
                if let Some(c) = self.ipc.conns.iter_mut().find(|c| c.id == id) {
                    c.subscribed = true;
                }
                return Ok(Some(self.ipc_state()));
            }
            R::Get(None) => return Self::event(P::Event::Options { options: self.ipc_options() }),
            R::Get(Some(name)) => {
                let value = self.ipc_options().remove(&name).ok_or_else(|| format!("unknown option `{name}`"))?;
                return Self::event(P::Event::Option { name, value });
            }
            R::Set(line) => {
                let c = self.config.set_line(&line).map_err(|e| format!("{e:#}"))?;
                self.config = c;
                self.user_shaders.clear();
                self.shader_errors.clear();
                // Like a reload for whoever follows config_rev, without re-reading the file.
                self.config_rev += 1;
                self.cursor_theme.configure(&self.config.cursor_theme, self.config.cursor_size);
                self.deco_commit.increment();
                self.retile_all();
                self.damage();
            }
            R::Rule(line) => {
                // The line is itself KDL: `rule { match app-id="mpv"; opacity 0.9 }`, or `rule clear`.
                let nodes = crate::kdl::parse(&line).map_err(|e| e.to_string())?;
                let [n] = nodes.as_slice() else { return Err("expected one rule".into()) };
                if n.arg_str(0) == Some("clear") && n.kids.is_empty() {
                    self.ipc_rules.clear();
                } else {
                    if !n.args.is_empty() {
                        return Err("expected `rule { ... }` or `rule clear`".into());
                    }
                    let r = crate::rules::Rule::parse(n, &self.config.vars).map_err(|e| format!("{e:#}"))?;
                    self.ipc_rules.push(r);
                }
                self.deco_commit.increment();
                self.damage();
            }
            R::Rules => {
                return Self::event(P::Event::Rules { file: self.config.rules.len(), ipc: self.ipc_rules.len() })
            }
            R::GetWindow(w) => {
                // A window's decoration as its rules make it right now.
                let i = window(self, w)?;
                let options = crate::options::json(crate::options::DECO, &self.deco_for(&self.clients[i]));
                return Self::event(P::Event::WindowOptions { id: w, options });
            }
            R::Bookmark { save: true, n } => self.bookmark_save(n),
            R::Bookmark { save: false, n } => {
                if !self.bookmark_goto(n) {
                    return Err(format!("bookmark {} is not set on this tag", n + 1));
                }
            }
            R::Pan(dx, dy) => self.pan_canvas(dx, dy),
            R::Layout(n) => {
                if n.is_some_and(|n| n >= self.layout.2) {
                    return Err(format!("there are {} layouts (0 to {})", self.layout.2, self.layout.2 - 1));
                }
                self.set_layout(n);
            }
            R::Crash => {
                if std::env::var_os("UWC_DEBUG_PANIC").is_none() {
                    return Err("crash is only available with UWC_DEBUG_PANIC=1 in uwc's environment".into());
                }
                panic!("uwc msg crash");
            }
            R::Jump(q) => {
                self.jump_to(&q)?;
            }
            R::Fling(vx, vy) => {
                self.pan_vel = (vx, vy);
                self.pan_sampled = std::time::Instant::now();
                self.pan_release();
            }
            R::PanTo(x, y) => self.with_viewport_eased(|v, _| {
                v.x = x;
                v.y = y;
            }),
            R::Zoom(z, at) => {
                let (sw, sh) = self.screen_size();
                let at = at.unwrap_or((sw / 2.0, sh / 2.0));
                self.with_viewport_eased(|v, _| v.set_zoom_at(z, at));
            }
            R::View(t) => self.set_tagset(1 << t),
            R::ToggleView(t) => {
                let ts = self.tagset() ^ (1 << t);
                self.set_tagset(ts);
            }
            R::Focus(w) => {
                window(self, w)?;
                self.focus_and_reveal(w);
            }
            R::Close(w) => {
                window(self, w)?;
                self.close_client(w);
            }
            R::Move(w, x, y) => {
                window(self, w)?;
                self.release_from_tiling(w);
                self.move_client_eased(w, x, y);
            }
            R::Resize(w, cw, ch) => {
                window(self, w)?;
                self.release_from_tiling(w);
                self.resize_client(w, cw, ch);
            }
            R::Tile(cmd) => {
                use P::TileCmd as T;
                match cmd {
                    T::Set(l) => self.tile_set(Some(l)),
                    T::Once(l) => self.tile_once(l),
                    T::Off => self.tile_set(None),
                    T::Cycle => self.tile_cycle(),
                    T::Ratio(r) => self.tile_set_ratio(r),
                    T::Masters(n) => self.tile_set_masters(n),
                }
            }
            R::Float(w, on) => {
                window(self, w)?;
                self.set_floating(w, on);
            }
            R::Pin(w, on) => {
                let i = window(self, w)?;
                if on && self.clients[i].fullscreen {
                    return Err("a fullscreen window cannot be pinned".into());
                }
                self.set_pinned(w, on);
            }
            R::Minimize(w, on) => {
                window(self, w)?;
                self.set_minimized(w, on);
            }
            R::Fullscreen(w, on) => {
                window(self, w)?;
                self.set_fullscreen(w, on);
            }
            R::WindowScale(w, s) => {
                let i = window(self, w)?;
                if self.clients[i].fullscreen {
                    return Err("a fullscreen window has no scale of its own".into());
                }
                self.set_window_scale(w, s, None);
            }
            R::Session { save: true } => self.session_save(true).map_err(|e| format!("{e:#}"))?,
            R::Session { save: false } => self.session_forget(),
            R::Overview => self.toggle_overview(),
            R::Home => self.with_viewport_eased(|v, _| *v = Default::default()),
            R::Reload => self.reload_config()?,
            R::Quit => self.loop_signal.stop(),
        }
        Ok(None)
    }
}
