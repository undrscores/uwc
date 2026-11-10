//! Control socket. Same shape as uwm's: text command lines in, JSON lines out
//! (`{"ok":true}`, `{"error":"..."}`, or an `{"event":...}` snapshot), so a shell written for
//! uwm's `state`/`subscribe` can follow uwc too.

use std::{
    ffi::OsStr,
    io::{ErrorKind, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
};

use serde_json::{json, Value};
use smithay::reexports::calloop::{generic::Generic, Interest, LoopHandle, Mode, PostAction};

use crate::{canvas::TAGCOUNT, config::Color, state::Uwc};

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

impl Ipc {
    pub fn new(handle: &LoopHandle<'static, Uwc>, wayland_display: &OsStr) -> anyhow::Result<Ipc> {
        let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let path = dir.join(format!("uwc-{}.sock", wayland_display.to_string_lossy()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
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

fn hex(c: Color) -> String {
    let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}{:02x}", b(c[0]), b(c[1]), b(c[2]), b(c[3]))
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
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
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
        let screen = self.screen_size();
        let ts = self.tagset();
        let mut occ = 0u32;
        for c in &self.clients {
            occ |= c.tags;
        }
        let mons: Vec<Value> = self
            .mon
            .iter()
            .map(|m| {
                let vp = m.viewport();
                let tags: Vec<Value> = (0..TAGCOUNT)
                    .map(|i| {
                        json!({
                            "n": i + 1,
                            "active": ts & (1 << i) != 0,
                            "occupied": occ & (1 << i) != 0,
                            "urgent": false,
                            "viewport": {"x": m.vp[i].x.round() as i64, "y": m.vp[i].y.round() as i64, "zoom": m.vp[i].zoom},
                            "bookmarks": [],
                        })
                    })
                    .collect();
                json!({
                    "name": m.output.name(),
                    "focused": true,
                    "layout": "><>",
                    "x": 0, "y": 0, "width": screen.0 as i64, "height": screen.1 as i64,
                    "scale": m.output.current_scale().fractional_scale(),
                    "viewport": {"x": vp.x.round() as i64, "y": vp.y.round() as i64, "zoom": vp.zoom},
                    "tags": tags,
                })
            })
            .collect();
        let name = self.mon.as_ref().map(|m| m.output.name()).unwrap_or_default();
        let windows: Vec<Value> = self
            .clients
            .iter()
            .filter(|c| c.mapped)
            .map(|c| {
                let r = c.rect();
                json!({
                    "id": c.id,
                    "title": c.title(),
                    "app_id": c.app_id(),
                    "monitor": name,
                    "tags": c.tags,
                    "focused": self.focus == Some(c.id),
                    "floating": true,
                    "fullscreen": false,
                    "urgent": false,
                    "x": r.loc.x.round() as i64, "y": r.loc.y.round() as i64, "width": r.size.w as i64, "height": r.size.h as i64,
                    "pinned": false,
                    "minimized": false,
                })
            })
            .collect();
        json!({
            "event": "state",
            "selmon": name,
            "config_error": self.config_error,
            "config_rev": self.config_rev,
            "monitors": mons,
            "cursor": {"x": self.cursor.x.round() as i64, "y": self.cursor.y.round() as i64, "hidden": false},
            "windows": windows,
        })
    }

    fn ipc_options(&self) -> Value {
        let c = &self.config;
        json!({
            "sloppy-focus": c.sloppy_focus,
            "warp-cursor": c.warp_cursor,
            "border-width": c.border_width,
            "root-color": hex(c.root_color),
            "border-color": hex(c.border_color),
            "focus-color": hex(c.focus_color),
            "urgent-color": hex(c.urgent_color),
            "pan-step": c.pan_step,
            "move-step": c.move_step,
            "zoom-scroll": c.zoom_scroll,
            "new-window-offset": c.new_window_offset,
        })
    }

    fn ipc_line(&mut self, id: u32, line: &str) {
        let reply = match self.ipc_command(id, line) {
            Ok(Some(v)) => v,
            Ok(None) => json!({"ok": true}),
            Err(e) => json!({"error": e}),
        };
        self.ipc_send(id, &reply);
    }

    fn ipc_command(&mut self, id: u32, line: &str) -> Result<Option<Value>, String> {
        let mut it = line.split_whitespace();
        let cmd = it.next().unwrap_or("");
        let args: Vec<&str> = it.collect();
        let num = |i: usize| -> Result<f64, String> {
            let v: f64 = args.get(i).ok_or("missing argument")?.parse().map_err(|_| "not a number".to_string())?;
            if v.is_finite() {
                Ok(v)
            } else {
                Err("not a finite number".into())
            }
        };
        let win = |s: &Uwc, i: usize| -> Result<u32, String> {
            let w = num(i)? as u32;
            s.client_index(w).map(|_| w).ok_or_else(|| format!("no window {w}"))
        };
        let tag = |i: usize| -> Result<usize, String> {
            let t = num(i)?;
            if t.fract() == 0.0 && t >= 1.0 && t <= TAGCOUNT as f64 {
                Ok(t as usize - 1)
            } else {
                Err(format!("tag must be 1 to {TAGCOUNT}"))
            }
        };
        match cmd {
            "state" => return Ok(Some(self.ipc_state())),
            "subscribe" => {
                if let Some(c) = self.ipc.conns.iter_mut().find(|c| c.id == id) {
                    c.subscribed = true;
                }
                return Ok(Some(self.ipc_state()));
            }
            "get" => {
                let all = self.ipc_options();
                return match args.first() {
                    None => Ok(Some(json!({"event": "options", "options": all}))),
                    Some(name) => match all.get(*name) {
                        Some(v) => Ok(Some(json!({"event": "option", "name": name, "value": v}))),
                        None => Err(format!("unknown option `{name}`")),
                    },
                };
            }
            "pan" => {
                let (dx, dy) = (num(0)?, num(1)?);
                self.pan_canvas(dx, dy);
            }
            "panto" => {
                let (x, y) = (num(0)?, num(1)?);
                self.with_viewport(|v, _| {
                    v.x = x;
                    v.y = y;
                });
            }
            "zoom" => {
                let z = num(0)?;
                if z <= 0.0 {
                    return Err("zoom must be positive".into());
                }
                let at = if args.len() >= 3 {
                    (num(1)?, num(2)?)
                } else {
                    (self.screen_size().0 / 2.0, self.screen_size().1 / 2.0)
                };
                self.with_viewport(|v, _| v.set_zoom_at(z, at));
            }
            "view" => {
                let t = tag(0)?;
                self.set_tagset(1 << t);
            }
            "toggleview" => {
                let t = tag(0)?;
                let ts = self.tagset() ^ (1 << t);
                self.set_tagset(ts);
            }
            "focus" => {
                let w = win(self, 0)?;
                self.focus_and_reveal(w);
            }
            "close" => {
                let w = win(self, 0)?;
                self.close_client(w);
            }
            "move" => {
                let w = win(self, 0)?;
                let (x, y) = (num(1)?, num(2)?);
                self.move_client(w, x, y);
            }
            "resize" => {
                let w = win(self, 0)?;
                let (cw, ch) = (num(1)?, num(2)?);
                if cw < 1.0 || ch < 1.0 || cw > 1e5 || ch > 1e5 {
                    return Err("size out of range".into());
                }
                self.resize_client(w, cw as i32, ch as i32);
            }
            "overview" => self.toggle_overview(),
            "home" => self.with_viewport(|v, _| *v = Default::default()),
            "reload" => {
                self.reload_config()?;
            }
            "quit" => self.loop_signal.stop(),
            "" => return Err("empty command".into()),
            other => return Err(format!("unknown command `{other}`")),
        }
        Ok(None)
    }
}
