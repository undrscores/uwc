//! Session restore: the canvas comes back the way you left it.
//!
//! Saved to `$XDG_STATE_HOME/uwc/session.json` (else `~/.local/state/uwc/session.json`) on quit and,
//! when something changed, every few seconds (so a crash loses little): the tags in view, every
//! tag's viewport and bookmarks, and every window's layout by app-id and title (canvas position or
//! pinned screen spot, size, tags, own scale, minimized, fullscreen).
//!
//! Views come back at startup. A window gets its saved layout when its app opens again: the first
//! saved entry with the same app-id and title, else the same app-id; each entry is used once. The
//! remembered layout wins over a rule's `open { }` (it is where you last put the window). Entries no
//! window has claimed are kept in later saves, so an app you did not start this time still
//! remembers its place. `session-restore false` turns all of it off.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{
    canvas::{Viewport, TAGCOUNT},
    rules::Open,
    state::Uwc,
};

const VERSION: u32 = 1;
/// Unclaimed entries kept at most (oldest dropped).
const MAX_WINDOWS: usize = 200;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedWindow {
    pub app_id: String,
    pub title: String,
    /// Canvas px of the top-left corner, or screen px when pinned.
    pub x: f64,
    pub y: f64,
    pub width: i32,
    pub height: i32,
    pub tags: u32,
    pub pinned: bool,
    pub scale: f64,
    pub minimized: bool,
    pub fullscreen: bool,
    /// The monitor a pinned spot is on, and the one a fullscreen window covers.
    #[serde(default)]
    pub pin_mon: usize,
    #[serde(default)]
    pub fs_mon: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SavedView {
    pub x: f64,
    pub y: f64,
    pub zoom: f64,
}

impl From<Viewport> for SavedView {
    fn from(v: Viewport) -> Self {
        SavedView { x: v.x, y: v.y, zoom: v.zoom }
    }
}

impl From<SavedView> for Viewport {
    fn from(v: SavedView) -> Self {
        Viewport { x: v.x, y: v.y, zoom: v.zoom.clamp(crate::canvas::ZOOM_MIN, crate::canvas::ZOOM_MAX) }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedMonitor {
    pub name: String,
    pub viewports: Vec<SavedView>,
    pub bookmarks: Vec<Vec<Option<SavedView>>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub version: u32,
    pub tagset: u32,
    /// Per-monitor views. Sessions written before it existed have top-level `viewports` and
    /// `bookmarks` instead, which load onto the first monitor.
    #[serde(default)]
    pub monitors: Vec<SavedMonitor>,
    #[serde(default)]
    pub viewports: Vec<SavedView>,
    #[serde(default)]
    pub bookmarks: Vec<Vec<Option<SavedView>>>,
    pub windows: Vec<SavedWindow>,
}

pub fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("uwc/session.json"))
}

/// The entry for a window that is opening, taken out of `pool`: same app-id and title, else same app-id.
pub fn take(pool: &mut Vec<SavedWindow>, app_id: &str, title: &str) -> Option<SavedWindow> {
    if app_id.is_empty() {
        return None;
    }
    let i = pool
        .iter()
        .position(|w| w.app_id == app_id && w.title == title)
        .or_else(|| pool.iter().position(|w| w.app_id == app_id))?;
    Some(pool.remove(i))
}

impl SavedWindow {
    /// What restoring this entry does to a window that is opening.
    pub fn apply(&self, open: &mut Open) {
        open.position = Some((self.x, self.y));
        open.size = Some((self.width, self.height));
        open.tags = Some(self.tags).filter(|t| *t != 0);
        open.pinned = self.pinned;
        open.scale = Some(self.scale);
        open.minimized = self.minimized;
        open.fullscreen = self.fullscreen;
        open.pin_mon = self.pin_mon;
        open.fs_mon = self.fs_mon;
    }
}

impl Uwc {
    /// The session as it is now: views, and the windows that are open plus the entries no window claimed.
    pub fn session_snapshot(&self) -> Session {
        let mut windows: Vec<SavedWindow> = self
            .clients
            .iter()
            // A window that hid itself (unmapped but it was here) keeps its place in the session.
            .filter(|c| (c.mapped || c.ever_mapped) && !c.app_id().is_empty())
            .map(|c| {
                let (x, y) = match c.pin {
                    Some(p) => (p.x, p.y),
                    None => (c.canvas.x, c.canvas.y),
                };
                // A fullscreen window is saved with the size it goes back to.
                let size = if c.fullscreen { c.prev_size.unwrap_or(c.size()) } else { c.size() };
                SavedWindow {
                    app_id: c.app_id(),
                    title: c.title(),
                    x,
                    y,
                    width: size.w,
                    height: size.h,
                    tags: c.tags,
                    pinned: c.pin.is_some(),
                    scale: c.scale,
                    minimized: c.minimized != 0,
                    fullscreen: c.fullscreen,
                    pin_mon: c.pin_mon,
                    fs_mon: c.fs_mon,
                }
            })
            .collect();
        windows.extend(self.session_pool.iter().cloned());
        if windows.len() > MAX_WINDOWS {
            windows.truncate(MAX_WINDOWS);
        }
        Session {
            version: VERSION,
            tagset: self.tagset,
            monitors: self
                .monitors
                .iter()
                .map(|m| SavedMonitor {
                    name: m.output.name().to_string(),
                    viewports: (0..TAGCOUNT).map(|i| m.view_of(i, self.tagset).into()).collect(),
                    bookmarks: m.bookmarks.iter().map(|b| b.iter().map(|v| v.map(Into::into)).collect()).collect(),
                })
                .collect(),
            viewports: Vec::new(),
            bookmarks: Vec::new(),
            windows,
        }
    }

    /// Write the session if it changed since the last write (or `force`).
    pub fn session_save(&mut self, force: bool) -> anyhow::Result<()> {
        if !self.config.session_restore {
            return Ok(());
        }
        let Some(path) = path() else { return Ok(()) };
        let text = serde_json::to_string_pretty(&self.session_snapshot())?;
        if !force && self.session_written.as_deref() == Some(text.as_str()) {
            return Ok(());
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Write then rename, so a crash mid-write cannot leave half a file. The file lists window titles
        // (document names, page titles): readable by its owner only.
        let tmp = path.with_extension("json.tmp");
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let _ = std::fs::remove_file(&tmp);
            let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
            f.write_all(text.as_bytes())?;
        }
        std::fs::rename(&tmp, &path)?;
        self.session_written = Some(text);
        Ok(())
    }

    /// At startup: put the views back and keep the window entries for when their apps open.
    pub fn session_load(&mut self) {
        if !self.config.session_restore {
            return;
        }
        let Some(path) = path() else { return };
        let Ok(text) = std::fs::read_to_string(&path) else { return };
        let s: Session = match serde_json::from_str(&text) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("ignoring {}: {e}", path.display());
                return;
            }
        };
        if s.version != VERSION {
            return;
        }
        // Old sessions name no monitors: their views load onto the first one.
        let saved: Vec<SavedMonitor> = if s.monitors.is_empty() {
            vec![SavedMonitor { name: String::new(), viewports: s.viewports.clone(), bookmarks: s.bookmarks.clone() }]
        } else {
            s.monitors.clone()
        };
        for m in &mut self.monitors {
            let sm = saved.iter().find(|sm| sm.name == m.output.name()).or_else(|| saved.first());
            let Some(sm) = sm else { continue };
            for (i, v) in sm.viewports.iter().take(TAGCOUNT).enumerate() {
                m.vp[i] = (*v).into();
            }
            for (i, bs) in sm.bookmarks.iter().take(TAGCOUNT).enumerate() {
                for (n, b) in bs.iter().take(9).enumerate() {
                    m.bookmarks[i][n] = b.map(Into::into);
                }
            }
        }
        if s.tagset & crate::canvas::ALLTAGS != 0 {
            self.tagset = s.tagset & crate::canvas::ALLTAGS;
        }
        tracing::info!("session: {} remembered windows from {}", s.windows.len(), path.display());
        self.session_pool = s.windows;
        self.session_written = Some(text);
        self.damage();
    }

    /// Forget everything remembered (IPC `session forget`).
    pub fn session_forget(&mut self) {
        self.session_pool.clear();
        self.session_written = None;
        if let Some(p) = path() {
            let _ = std::fs::remove_file(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(app: &str, title: &str) -> SavedWindow {
        SavedWindow {
            app_id: app.into(),
            title: title.into(),
            x: 1.0,
            y: 2.0,
            width: 300,
            height: 200,
            tags: 1,
            pinned: false,
            scale: 1.0,
            minimized: false,
            fullscreen: false,
            pin_mon: 0,
            fs_mon: 0,
        }
    }

    #[test]
    fn matching_prefers_the_title_and_uses_each_entry_once() {
        let mut pool = vec![w("foot", "build"), w("foot", "shell"), w("mpv", "film")];
        assert_eq!(take(&mut pool, "foot", "shell").unwrap().title, "shell");
        // No title match: the first with the app-id.
        assert_eq!(take(&mut pool, "foot", "something else").unwrap().title, "build");
        assert!(take(&mut pool, "foot", "shell").is_none(), "used up");
        assert!(take(&mut pool, "", "film").is_none(), "no app-id, no match");
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn round_trips_through_json() {
        let s = Session {
            version: VERSION,
            tagset: 0b101,
            monitors: vec![SavedMonitor {
                name: "DP-1".into(),
                viewports: vec![SavedView { x: 1.0, y: 2.0, zoom: 0.5 }],
                bookmarks: vec![vec![None, Some(SavedView { x: 3.0, y: 4.0, zoom: 2.0 })]],
            }],
            viewports: Vec::new(),
            bookmarks: Vec::new(),
            windows: vec![w("foot", "a")],
        };
        let back: Session = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn a_v1_session_loads_onto_the_first_monitor() {
        let old = "{\"version\":1,\"tagset\":1,\"viewports\":[{\"x\":1.0,\"y\":2.0,\"zoom\":0.5}],\"bookmarks\":[[null]],\"windows\":[]}";
        let s: Session = serde_json::from_str(old).unwrap();
        assert!(s.monitors.is_empty());
        assert_eq!(s.viewports.len(), 1);
    }
}
