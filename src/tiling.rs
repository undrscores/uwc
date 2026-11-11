//! Tiling on a canvas where everything floats. A *region* can have a layout, and its ordinary
//! windows are then kept in the slots `layout.rs` works out for it:
//!
//! - a tag (`tile "master-stack"`): the region is the *tile area*, a rect of canvas px taken from
//!   the view when the layout was turned on (so panning never moves or resizes anything; run `tile`
//!   again to re-anchor to where you are);
//!
//! Windows are re-placed when one maps, closes, is minimized, pinned, floated or sent to another
//! tag, and when the layout or its options change. `tile-once`
//! arranges the windows one time and leaves it at that.
//!
//! Tiled: mapped, not pinned, not fullscreen, not minimized, not a dialog (it has a parent), not a
//! fixed-size window (its minimum and maximum size are the same), not floating. A window is
//! floating when a rule says `open { float }`, `toggle-float` (or IPC `float ID`) says so, or it was
//! moved or resized with the keys or IPC. With the mouse a tile is *lifted*: let go over another
//! tile of its region and the two swap places; inside its own region and it goes back; anywhere else
//! it floats. Resizing a tile with the mouse moves the main
//! split (the ratio) with the pointer's horizontal travel.
//!
//! Windows keep an order per region (the order they opened in, until swapped), so focusing and
//! raising never shuffles a layout. A window that will not get as small as its slot (a minimum size
//! it announces, or one it shows by drawing bigger than it was told) gets the room it needs and the
//! others share the rest.
//!
//! The session keeps each tag's layout, split and tile area (not the window order, and not which
//! windows were floated): a restart brings the layout back and windows tile into it as their apps open.

use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::{
    canvas::{lowest_tag, TAGCOUNT},
    layout::{self, Layout},
    state::{Drag, Shell, Uwc},
};

/// Something that has a layout: a tag's tile area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    Tag(usize),
}

/// What a region is doing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TileState {
    /// None = nothing is tiled here.
    pub layout: Option<Layout>,
    /// Set by `tile-ratio` / `tile-masters` / resizing a tile; None follows the options.
    pub ratio: Option<f64>,
    pub masters: Option<usize>,
    /// The windows in layout order, first = master. Windows not listed go after, in the order they
    /// opened.
    pub order: Vec<u32>,
}

/// A tag's tiling: the state and the canvas rect it works in.
#[derive(Debug, Clone, PartialEq)]
pub struct TagTiling {
    pub state: TileState,
    /// Canvas px.
    pub area: Rectangle<i32, Logical>,
}

impl Default for TagTiling {
    fn default() -> Self {
        TagTiling { state: TileState::default(), area: Rectangle::from_size((0, 0).into()) }
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

/// Whether windows glide to their slots (`move-ms`) or jump there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glide {
    Ease,
    /// Dragging and resizing: the windows must keep up with the pointer.
    Instant,
    /// A window that just opened goes straight to its slot; the others glide.
    InstantFor(u32),
}

pub const MIN_RATIO: f64 = 0.1;
pub const MAX_RATIO: f64 = 0.9;
pub const MAX_MASTERS: usize = 16;

/// A window's smallest and largest size as it announced them (0 = none), in its own px.
pub type Limits = ((i32, i32), (i32, i32));

fn contains(r: Rectangle<f64, Logical>, p: Point<f64, Logical>) -> bool {
    p.x >= r.loc.x && p.x < r.loc.x + r.size.w && p.y >= r.loc.y && p.y < r.loc.y + r.size.h
}

impl Uwc {
    // ---- who is where ------------------------------------------------------------------------

    pub fn tile_state(&self, r: Region) -> Option<&TileState> {
        match r {
            Region::Tag(t) => self.tiling.tags.get(t).map(|t| &t.state),
        }
    }

    fn tile_state_mut(&mut self, r: Region) -> Option<&mut TileState> {
        match r {
            Region::Tag(t) => self.tiling.tags.get_mut(t).map(|t| &mut t.state),
        }
    }

    /// The canvas rect a region lays out in.
    pub fn tile_area(&self, r: Region) -> Option<Rectangle<i32, Logical>> {
        match r {
            Region::Tag(t) => self.tiling.tags.get(t).map(|t| t.area),
        }
    }

    /// The options a region's layout is worked out with.
    pub fn tile_opts(&self, r: Region) -> layout::Opts {
        let st = self.tile_state(r);
        layout::Opts {
            outer: self.config.tile_gap,
            inner: self.config.tile_inner_gap,
            ratio: st.and_then(|s| s.ratio).unwrap_or(self.config.tile_ratio),
            masters: st.and_then(|s| s.masters).unwrap_or(self.config.tile_masters as usize),
        }
    }

    /// The region whose layout places client `i`, if it is tiled.
    pub fn tile_region(&self, i: usize) -> Option<Region> {
        let c = &self.clients[i];
        if !c.mapped || c.floating || c.minimized != 0 || c.pin.is_some() || c.fullscreen {
            return None;
        }
        // A window with one fixed size cannot be laid out.
        if c.limits.0 .0 > 0 && c.limits.0 .1 > 0 && c.limits.0 == c.limits.1 {
            return None;
        }
        // A dialog goes over its parent, not into the layout.
        if self.parent_of(i).is_some() {
            return None;
        }
        (0..TAGCOUNT).find(|&t| c.tags & (1 << t) != 0 && self.tiling.tags[t].state.layout.is_some()).map(Region::Tag)
    }

    /// The region of the focused window if it is tiled, else the tag in use: what `tile-ratio` and
    /// `tile-masters` change.
    pub fn tile_focus_region(&self) -> Region {
        self.focus
            .and_then(|id| self.client_index(id))
            .and_then(|i| self.tile_region(i))
            .unwrap_or(Region::Tag(self.tile_target()))
    }

    /// The tag the tile actions work on: the one in use.
    pub fn tile_target(&self) -> usize {
        lowest_tag(self.tagset())
    }

    /// Whether any region has a layout (so there is something to keep in place).
    fn any_tiling(&self) -> bool {
        self.tiling.tags.iter().any(|t| t.state.layout.is_some())
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

    // ---- placing -----------------------------------------------------------------------------

    /// Put every tiled window of `region` in its slot.
    pub fn retile(&mut self, region: Region, glide: Glide) {
        let Some(layout) = self.tile_state(region).and_then(|s| s.layout) else { return };
        let Some(area) = self.tile_area(region) else { return };
        // The windows, in layout order: the remembered order first, then newcomers by id.
        let mut ids: Vec<u32> = (0..self.clients.len())
            .filter(|&i| self.tile_region(i) == Some(region))
            .map(|i| self.clients[i].id)
            .collect();
        ids.sort_unstable();
        // The remembered order keeps a window's place while it is floating or minimized for a while
        // and drops the ones that are gone; newcomers go last, in the order they opened.
        let known = self.tile_state(region).map(|s| s.order.clone()).unwrap_or_default();
        let mut stored: Vec<u32> = known.into_iter().filter(|&id| self.client_index(id).is_some()).collect();
        let newcomers: Vec<u32> = ids.iter().copied().filter(|id| !stored.contains(id)).collect();
        stored.extend(newcomers);
        let ordered: Vec<u32> = stored.iter().copied().filter(|id| ids.contains(id)).collect();
        if let Some(s) = self.tile_state_mut(region) {
            s.order = stored;
        }
        // What each window needs at least, in canvas px.
        let mins: Vec<layout::Min> = ordered
            .iter()
            .map(|&id| {
                let c = &self.clients[self.client_index(id).unwrap()];
                let s = c.scale.max(0.01);
                let px = |n: i32| (n as f64 * s).ceil() as i32;
                (px(c.limits.0 .0.max(c.tile_min.0)), px(c.limits.0 .1.max(c.tile_min.1)))
            })
            .collect();
        let slots = layout::arrange(layout, area, ordered.len(), self.tile_opts(region), &mins);
        // The window being carried by the mouse keeps its slot but is not put into it.
        let lifted = match self.drag {
            Some(Drag::Move { id, .. }) => Some(id),
            _ => None,
        };
        for (id, slot) in ordered.into_iter().zip(slots) {
            if lifted == Some(id) {
                continue;
            }
            let Some(i) = self.client_index(id) else { continue };
            // The slot is the window's footprint, which is its size times its own scale.
            let s = self.clients[i].scale.max(0.01);
            let want = Size::from((
                (((slot.size.w as f64) / s).round() as i32).max(50),
                (((slot.size.h as f64) / s).round() as i32).max(30),
            ));
            self.clients[i].tile_want = Some((want.w, want.h));
            if self.clients[i].size() != want {
                self.resize_client(id, want.w, want.h);
            }
            let at = (slot.loc.x as f64, slot.loc.y as f64);
            if (self.clients[i].canvas.x, self.clients[i].canvas.y) != at {
                let eased = match glide {
                    Glide::Ease => true,
                    Glide::Instant => false,
                    Glide::InstantFor(only) => only != id,
                };
                if eased {
                    self.move_client_eased(id, at.0, at.1);
                } else {
                    self.move_client(id, at.0, at.1);
                }
            }
        }
    }

    /// Re-place the windows of every region that has a layout; cheap when none has. Called wherever
    /// a window comes, goes or changes in a way that decides whether it is tiled.
    pub fn retile_all_with(&mut self, glide: Glide) {
        if !self.any_tiling() {
            return;
        }
        for t in 0..TAGCOUNT {
            self.retile(Region::Tag(t), glide);
        }
    }

    pub fn retile_all(&mut self) {
        self.retile_all_with(Glide::Ease);
    }

    // ---- turning layouts on and off ----------------------------------------------------------

    /// Turn a layout on for the tag in use (or off), over what the current monitor shows now.
    pub fn tile_set(&mut self, layout: Option<Layout>) {
        let tag = self.tile_target();
        let area = self.visible_area();
        let t = &mut self.tiling.tags[tag];
        t.state.layout = layout;
        if layout.is_some() {
            t.area = area;
        } else {
            t.state = TileState::default();
        }
        self.retile(Region::Tag(tag), Glide::Ease);
        self.tiling_changed();
    }

    /// Arrange the tag in use once and stay out of its way afterwards.
    pub fn tile_once(&mut self, layout: Layout) {
        let tag = self.tile_target();
        let area = self.visible_area();
        let t = &mut self.tiling.tags[tag];
        t.state.layout = Some(layout);
        t.area = area;
        self.retile(Region::Tag(tag), Glide::Ease);
        self.tiling.tags[tag] = TagTiling::default();
        self.tiling_changed();
    }

    /// The next layout, and off after the last.
    pub fn tile_cycle(&mut self) {
        let next = match self.tiling.tags[self.tile_target()].state.layout {
            None => Some(Layout::ALL[0]),
            Some(l) => l.next(),
        };
        self.tile_set(next);
    }

    pub fn tile_ratio(&mut self, delta: f64) {
        let r = self.tile_focus_region();
        let now = self.tile_opts(r).ratio;
        self.tile_region_set_ratio(r, now + delta, Glide::Ease);
    }

    pub fn tile_set_ratio(&mut self, ratio: f64) {
        let r = self.tile_focus_region();
        self.tile_region_set_ratio(r, ratio, Glide::Ease);
    }

    pub fn tile_region_set_ratio(&mut self, r: Region, ratio: f64, glide: Glide) {
        if let Some(s) = self.tile_state_mut(r) {
            s.ratio = Some(ratio.clamp(MIN_RATIO, MAX_RATIO));
        }
        self.retile(r, glide);
        self.tiling_changed();
    }

    pub fn tile_masters(&mut self, delta: i32) {
        let r = self.tile_focus_region();
        let now = self.tile_opts(r).masters as i32;
        self.tile_region_set_masters(r, (now + delta).max(1) as usize);
    }

    pub fn tile_set_masters(&mut self, n: usize) {
        let r = self.tile_focus_region();
        self.tile_region_set_masters(r, n);
    }

    fn tile_region_set_masters(&mut self, r: Region, n: usize) {
        if let Some(s) = self.tile_state_mut(r) {
            s.masters = Some(n.clamp(1, MAX_MASTERS));
        }
        self.retile(r, Glide::Ease);
        self.tiling_changed();
    }

    // ---- floating, dragging ------------------------------------------------------------------

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

    /// A window moved or resized with the keys or IPC leaves its layout, so the layout does not
    /// fight it and the others close up around the gap.
    pub fn release_from_tiling(&mut self, id: u32) {
        if self.client_index(id).is_some_and(|i| self.tile_region(i).is_some()) {
            self.set_floating(id, true);
        }
    }

    /// The canvas point under the pointer, on the monitor it is on.
    fn cursor_on_canvas(&self) -> Point<f64, Logical> {
        let m = self.mon_at(self.cursor);
        let local = self.cursor - self.origin_of(m);
        self.viewport_of(m).screen_to_canvas((local.x, local.y)).into()
    }

    /// The mouse let go of a window it was moving. A tile was lifted out of its slot: it goes where
    /// it was dropped (see the module docs).
    pub fn tile_drop(&mut self, id: u32) {
        let Some(i) = self.client_index(id) else { return };
        let Some(region) = self.tile_region(i) else { return };
        let p = self.cursor_on_canvas();
        // Over another tile of the same region: swap places.
        let other = (0..self.clients.len())
            .filter(|&j| j != i && self.tile_region(j) == Some(region))
            .find(|&j| contains(self.clients[j].rect(), p))
            .map(|j| self.clients[j].id);
        if let Some(other) = other {
            if let Some(s) = self.tile_state_mut(region) {
                let (a, b) = (s.order.iter().position(|&x| x == id), s.order.iter().position(|&x| x == other));
                if let (Some(a), Some(b)) = (a, b) {
                    s.order.swap(a, b);
                }
            }
            self.retile(region, Glide::Ease);
            self.tiling_changed();
            return;
        }
        // Inside its own region: back into the slot. Anywhere else: floating where it was dropped.
        let own = self.tile_area(region).is_some_and(|a| {
            contains(
                Rectangle::new((a.loc.x as f64, a.loc.y as f64).into(), (a.size.w as f64, a.size.h as f64).into()),
                p,
            )
        });
        if own {
            self.retile(region, Glide::Ease);
            self.tiling_changed();
        } else {
            self.set_floating(id, true);
        }
    }

    /// Resizing a tile with the mouse: the pointer's horizontal travel (canvas px) moves the main
    /// split. Columns have no split to move, so the window is floated instead. Returns whether the
    /// resize became a tile resize (and its starting ratio).
    pub fn tile_begin_resize(&mut self, id: u32) -> Option<(Region, f64)> {
        let i = self.client_index(id)?;
        let region = self.tile_region(i)?;
        if self.tile_state(region)?.layout == Some(Layout::Columns) {
            self.set_floating(id, true);
            return None;
        }
        Some((region, self.tile_opts(region).ratio))
    }

    /// The ratio that follows a drag of `dx` canvas px from `ratio0`.
    pub fn tile_resize_to(&mut self, region: Region, ratio0: f64, dx: f64) {
        let Some(area) = self.tile_area(region) else { return };
        let o = self.tile_opts(region);
        let space = (area.size.w - 2 * o.outer - o.inner).max(1) as f64;
        self.tile_region_set_ratio(region, ratio0 + dx / space, Glide::Instant);
    }

    // ---- what clients tell us ----------------------------------------------------------------

    /// A client committed: its size limits may have changed, or it may be drawing bigger than the
    /// slot it was told (it cannot be that small), and then the layout gives it the room.
    pub fn note_tile_hints(&mut self, i: usize) {
        let Shell::Wayland(w) = &self.clients[i].shell else { return };
        let Some(t) = w.toplevel() else { return };
        let limits: Limits = smithay::wayland::compositor::with_states(t.wl_surface(), |s| {
            let mut g = s.cached_state.get::<smithay::wayland::shell::xdg::SurfaceCachedState>();
            let c = g.current();
            ((c.min_size.w.max(0), c.min_size.h.max(0)), (c.max_size.w.max(0), c.max_size.h.max(0)))
        });
        let acked = t.current_state().size;
        let size = self.clients[i].size();
        let c = &mut self.clients[i];
        let mut changed = c.limits != limits;
        c.limits = limits;
        if let Some((ww, wh)) = c.tile_want {
            // It has taken our size and still draws bigger: remember that as its minimum.
            if acked == Some(Size::from((ww, wh))) && (size.w > ww || size.h > wh) {
                let m = (c.tile_min.0.max(size.w.max(ww)), c.tile_min.1.max(size.h.max(wh)));
                if m != c.tile_min {
                    c.tile_min = m;
                    changed = true;
                }
            }
        }
        if changed && self.any_tiling() {
            self.retile_all_with(Glide::Ease);
        }
    }

    /// Something about tiling changed: the snapshot is out of date and a frame is due.
    pub fn tiling_changed(&mut self) {
        self.ipc.dirty = true;
        self.damage();
    }
}
