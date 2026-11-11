//! Tiling on a canvas where everything floats. A tag can have a layout (`tile "master-stack"`): its
//! ordinary windows are then kept in the slots `layout.rs` works out for the tag's *tile area*, a
//! rect of canvas px taken from the view when the layout was turned on (so panning never moves or
//! resizes anything; run `tile` again to re-anchor to where you are). Windows are re-placed when
//! one maps, closes, is minimized, pinned, floated or sent to another tag, and when the layout
//! or its options change. `tile-once` arranges the windows one time and leaves it at that.
//!
//! Tiled: mapped, on the tag, not pinned, not fullscreen, not minimized, not a dialog (it has a
//! parent), not floating. A window is floating when a rule says `open { float }`, `toggle-float`
//! (or IPC `float ID`) says so, or it was moved or resized by hand: dragging a tile out of the
//! layout is how you take it out, and the others close up. Windows keep their order by id (the
//! order they opened in), so focusing and raising never shuffles the layout.
//!
//! Nothing is saved with the session: a restart brings the windows back where they were, floating.

use smithay::utils::{Logical, Rectangle, Size};

use crate::{
    canvas::{lowest_tag, TAGCOUNT},
    layout::{self, Layout},
    state::Uwc,
};

/// What one tag's tiling is doing.
#[derive(Debug, Clone, PartialEq)]
pub struct TagTiling {
    /// None = nothing is tiled on this tag.
    pub layout: Option<Layout>,
    /// Canvas px.
    pub area: Rectangle<i32, Logical>,
    /// Set by `tile-ratio` / `tile-masters`; None follows the options.
    pub ratio: Option<f64>,
    pub masters: Option<usize>,
}

impl Default for TagTiling {
    fn default() -> Self {
        TagTiling { layout: None, area: Rectangle::from_size((0, 0).into()), ratio: None, masters: None }
    }
}

pub struct Tiling {
    pub tags: [TagTiling; TAGCOUNT],
}

impl Default for Tiling {
    fn default() -> Self {
        Tiling { tags: std::array::from_fn(|_| TagTiling::default()) }
    }
}

pub const MIN_RATIO: f64 = 0.1;
pub const MAX_RATIO: f64 = 0.9;
pub const MAX_MASTERS: usize = 16;

impl Uwc {
    /// The tag whose layout places client `i`, if it is tiled.
    pub fn tile_tag(&self, i: usize) -> Option<usize> {
        let c = &self.clients[i];
        if !c.mapped || c.floating || c.minimized != 0 || c.pin.is_some() || c.fullscreen {
            return None;
        }
        let t = (0..TAGCOUNT).find(|&t| c.tags & (1 << t) != 0 && self.tiling.tags[t].layout.is_some())?;
        // A dialog goes over its parent, not into the layout.
        self.parent_of(i).is_none().then_some(t)
    }

    /// The options a tag's layout is worked out with.
    pub fn tile_opts(&self, tag: usize) -> layout::Opts {
        let t = &self.tiling.tags[tag];
        layout::Opts {
            outer: self.config.tile_gap,
            inner: self.config.tile_inner_gap,
            ratio: t.ratio.unwrap_or(self.config.tile_ratio),
            masters: t.masters.unwrap_or(self.config.tile_masters as usize),
        }
    }

    /// The part of the canvas the current monitor shows (minus panels), in canvas px.
    fn visible_area(&self) -> Rectangle<i32, Logical> {
        let (ax, ay, aw, ah) = self.work_area();
        let vp = self.target_viewport_of(self.cur);
        let (x, y) = vp.screen_to_canvas((ax, ay));
        let z = vp.zoom.max(1e-6);
        Rectangle::new(
            (x.round() as i32, y.round() as i32).into(),
            (((aw / z).round() as i32).max(1), ((ah / z).round() as i32).max(1)).into(),
        )
    }

    /// Put every tiled window of `tag` in its slot.
    pub fn retile(&mut self, tag: usize) {
        let Some(layout) = self.tiling.tags[tag].layout else { return };
        let mut ids: Vec<u32> =
            (0..self.clients.len()).filter(|&i| self.tile_tag(i) == Some(tag)).map(|i| self.clients[i].id).collect();
        ids.sort_unstable();
        let slots = layout::arrange(layout, self.tiling.tags[tag].area, ids.len(), self.tile_opts(tag));
        for (id, slot) in ids.into_iter().zip(slots) {
            let Some(i) = self.client_index(id) else { continue };
            // The slot is the window's footprint, which is its size times its own scale.
            let s = self.clients[i].scale.max(0.01);
            let want =
                Size::from((((slot.size.w as f64) / s).round() as i32, ((slot.size.h as f64) / s).round() as i32));
            if self.clients[i].size() != want {
                self.resize_client(id, want.w, want.h);
            }
            let at = (slot.loc.x as f64, slot.loc.y as f64);
            if (self.clients[i].canvas.x, self.clients[i].canvas.y) != at {
                self.move_client_eased(id, at.0, at.1);
            }
        }
    }

    /// Re-place the windows of every tag that has a layout; cheap when none has. Called wherever a
    /// window comes, goes or changes in a way that decides whether it is tiled.
    pub fn retile_all(&mut self) {
        for t in 0..TAGCOUNT {
            if self.tiling.tags[t].layout.is_some() {
                self.retile(t);
            }
        }
    }

    /// The tag the tile actions work on: the one in use.
    pub fn tile_target(&self) -> usize {
        lowest_tag(self.tagset())
    }

    /// Turn a layout on for the tag in use (or off), over what the current monitor shows now.
    pub fn tile_set(&mut self, layout: Option<Layout>) {
        let tag = self.tile_target();
        let area = self.visible_area();
        let t = &mut self.tiling.tags[tag];
        t.layout = layout;
        if layout.is_some() {
            t.area = area;
        } else {
            t.ratio = None;
            t.masters = None;
        }
        self.retile(tag);
        self.tiling_changed();
    }

    /// Arrange the tag in use once and stay out of its way afterwards.
    pub fn tile_once(&mut self, layout: Layout) {
        let tag = self.tile_target();
        let area = self.visible_area();
        let t = &mut self.tiling.tags[tag];
        t.layout = Some(layout);
        t.area = area;
        self.retile(tag);
        self.tiling.tags[tag] = TagTiling::default();
        self.tiling_changed();
    }

    /// The next layout, and off after the last.
    pub fn tile_cycle(&mut self) {
        let next = match self.tiling.tags[self.tile_target()].layout {
            None => Some(Layout::ALL[0]),
            Some(l) => l.next(),
        };
        self.tile_set(next);
    }

    pub fn tile_ratio(&mut self, delta: f64) {
        let tag = self.tile_target();
        let now = self.tile_opts(tag).ratio;
        self.tile_set_ratio(now + delta);
    }

    pub fn tile_set_ratio(&mut self, ratio: f64) {
        let tag = self.tile_target();
        self.tiling.tags[tag].ratio = Some(ratio.clamp(MIN_RATIO, MAX_RATIO));
        self.retile(tag);
        self.tiling_changed();
    }

    pub fn tile_masters(&mut self, delta: i32) {
        let tag = self.tile_target();
        let now = self.tile_opts(tag).masters as i32;
        self.tile_set_masters((now + delta).max(1) as usize);
    }

    pub fn tile_set_masters(&mut self, n: usize) {
        let tag = self.tile_target();
        self.tiling.tags[tag].masters = Some(n.clamp(1, MAX_MASTERS));
        self.retile(tag);
        self.tiling_changed();
    }

    /// Keep a window out of (or put it back into) the layouts.
    pub fn set_floating(&mut self, id: u32, on: bool) {
        let Some(i) = self.client_index(id) else { return };
        if self.clients[i].floating == on {
            return;
        }
        self.clients[i].floating = on;
        self.retile_all();
        self.tiling_changed();
    }

    /// A window the user moves or resizes by hand leaves its layout, so the layout does not fight
    /// the hand and the others close up around the gap.
    pub fn release_from_tiling(&mut self, id: u32) {
        if self.client_index(id).is_some_and(|i| self.tile_tag(i).is_some()) {
            self.set_floating(id, true);
        }
    }

    /// Something about tiling changed: the snapshot is out of date and a frame is due.
    pub fn tiling_changed(&mut self) {
        self.ipc.dirty = true;
        self.damage();
    }
}
