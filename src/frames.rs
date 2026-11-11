//! Frames: labelled regions of a tag's canvas, drawn by uwc behind the windows. A frame is how you
//! lay a canvas out into places ("mail", "build", "notes"): move it by its title strip and the
//! windows inside go along, resize it by its bottom-right corner, jump the view to it
//! (`frame-fit`, `frame-next`), name it over IPC. A window belongs to a frame while its centre is
//! inside it; that is worked out when the frame moves, so dragging a window in or out is all it
//! takes to change what goes with it.
//!
//! Frames are saved with the session. They have no client: the fill, border, strip and label are
//! drawn here (the label with the same pixel font as the jump hints).

use std::collections::HashMap;

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            element::{
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
                solid::SolidColorRenderElement,
                Id, Kind,
            },
            gles::GlesRenderer,
        },
    },
    utils::{Logical, Physical, Point, Rectangle, Transform},
};

use crate::{hints::label_pixels, render::UwcElement, state::Uwc};

pub struct Frame {
    pub id: u32,
    /// Canvas px.
    pub rect: Rectangle<f64, Logical>,
    pub label: String,
    /// The tags it is on.
    pub tags: u32,
    /// Fill, four borders, strip.
    ids: [Id; 6],
}

#[derive(Default)]
pub struct Frames {
    pub list: Vec<Frame>,
    next: u32,
    /// The windows a drag is carrying, and where they were: (window id, canvas position).
    pub carrying: Vec<(u32, Point<f64, Logical>)>,
    cache: HashMap<(u32, String, i32), (MemoryRenderBuffer, i32, i32)>,
}

/// Which part of a frame the pointer is over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameHit {
    Strip,
    Corner,
}

const MIN_SIZE: f64 = 80.0;
/// More than anyone lays out by hand; stops a script from filling memory.
pub const MAX_FRAMES: usize = 500;

impl Uwc {
    fn frame_index(&self, id: u32) -> Option<usize> {
        self.frames.list.iter().position(|f| f.id == id)
    }

    pub fn frame_add(&mut self, rect: Rectangle<f64, Logical>, label: &str, tags: u32) -> u32 {
        self.frames.next += 1;
        let id = self.frames.next;
        let rect = Rectangle::new(
            (rect.loc.x.round(), rect.loc.y.round()).into(),
            (rect.size.w.max(MIN_SIZE).round(), rect.size.h.max(MIN_SIZE).round()).into(),
        );
        self.frames.list.push(Frame {
            id,
            rect,
            label: label.chars().take(60).collect(),
            tags,
            ids: std::array::from_fn(|_| Id::new()),
        });
        self.frames_changed();
        id
    }

    /// Frames look different without moving: new colours, a new label.
    fn frames_changed(&mut self) {
        self.deco_commit.increment();
        self.damage();
    }

    /// A frame around window `id`, with room to spare, on the window's tags.
    pub fn frame_around(&mut self, id: u32) -> Option<u32> {
        let i = self.client_index(id)?;
        let pad = 40.0;
        let r = self.clients[i].rect();
        let strip = self.frame_strip_height() / self.viewport().zoom.max(0.05);
        let rect = Rectangle::new(
            (r.loc.x - pad, r.loc.y - pad - strip).into(),
            (r.size.w + 2.0 * pad, r.size.h + 2.0 * pad + strip).into(),
        );
        let tags = if self.clients[i].pin.is_some() { self.tagset } else { self.clients[i].tags };
        Some(self.frame_add(rect, "", tags))
    }

    /// The windows inside frame `id`: mapped canvas windows on its tags whose centre is in it.
    pub fn frame_members(&self, id: u32) -> Vec<u32> {
        let Some(f) = self.frame_index(id).map(|i| &self.frames.list[i]) else { return Vec::new() };
        self.clients
            .iter()
            .filter(|c| c.mapped && c.minimized == 0 && c.pin.is_none() && !c.fullscreen && c.tags & f.tags != 0)
            .filter(|c| {
                let r = c.rect();
                let centre = (r.loc.x + r.size.w / 2.0, r.loc.y + r.size.h / 2.0);
                centre.0 >= f.rect.loc.x
                    && centre.0 < f.rect.loc.x + f.rect.size.w
                    && centre.1 >= f.rect.loc.y
                    && centre.1 < f.rect.loc.y + f.rect.size.h
            })
            .map(|c| c.id)
            .collect()
    }

    /// Move frame `id` to canvas `(x, y)` with its windows.
    pub fn frame_move(&mut self, id: u32, x: f64, y: f64) {
        let members = self.frame_members(id);
        let Some(i) = self.frame_index(id) else { return };
        let (dx, dy) = (x.round() - self.frames.list[i].rect.loc.x, y.round() - self.frames.list[i].rect.loc.y);
        self.frames.list[i].rect.loc = (x.round(), y.round()).into();
        for w in members {
            if let Some(c) = self.client_index(w).map(|c| self.clients[c].canvas) {
                self.move_client(w, c.x + dx, c.y + dy);
            }
        }
        self.frames_changed();
    }

    pub fn frame_resize(&mut self, id: u32, w: f64, h: f64) {
        if let Some(i) = self.frame_index(id) {
            self.frames.list[i].rect.size = (w.max(MIN_SIZE).round(), h.max(MIN_SIZE).round()).into();
            self.frames_changed();
        }
    }

    pub fn frame_set_label(&mut self, id: u32, label: &str) {
        if let Some(i) = self.frame_index(id) {
            self.frames.list[i].label = label.chars().take(60).collect();
            self.frames_changed();
        }
    }

    pub fn frame_remove(&mut self, id: u32) -> bool {
        let before = self.frames.list.len();
        self.frames.list.retain(|f| f.id != id);
        self.frames.cache.retain(|k, _| k.0 != id);
        let removed = self.frames.list.len() != before;
        if removed {
            self.frames_changed();
        }
        removed
    }

    /// Fit the view to frame `id` (eased).
    pub fn frame_fit(&mut self, id: u32) {
        let Some(i) = self.frame_index(id) else { return };
        let r = self.frames.list[i].rect;
        self.with_viewport_eased(|v, area| v.fit((r.loc.x, r.loc.y, r.size.w, r.size.h), area, 30.0));
    }

    /// The frame (on the current tags) the focused window is in, else the one under the view's centre.
    pub fn frame_of_focus(&self) -> Option<u32> {
        let visible = |f: &&Frame| f.tags & self.tagset != 0;
        if let Some(c) = self.focus.and_then(|id| self.client_index(id)).map(|i| &self.clients[i]) {
            let r = c.rect();
            let centre = (r.loc.x + r.size.w / 2.0, r.loc.y + r.size.h / 2.0);
            // The smallest one that holds it, so a nested frame wins.
            let inside = |f: &&Frame| {
                centre.0 >= f.rect.loc.x
                    && centre.0 < f.rect.loc.x + f.rect.size.w
                    && centre.1 >= f.rect.loc.y
                    && centre.1 < f.rect.loc.y + f.rect.size.h
            };
            if let Some(f) = self
                .frames
                .list
                .iter()
                .filter(visible)
                .filter(inside)
                .min_by(|a, b| (a.rect.size.w * a.rect.size.h).total_cmp(&(b.rect.size.w * b.rect.size.h)))
            {
                return Some(f.id);
            }
        }
        None
    }

    /// Fit the next (or previous) frame of this tag set, in reading order, starting from the one
    /// the view is on.
    pub fn frame_cycle(&mut self, forward: bool) {
        let mut ids: Vec<(u32, f64, f64)> = self
            .frames
            .list
            .iter()
            .filter(|f| f.tags & self.tagset != 0)
            .map(|f| (f.id, f.rect.loc.x, f.rect.loc.y))
            .collect();
        if ids.is_empty() {
            return;
        }
        ids.sort_by(|a, b| ((a.2 / 200.0).round(), a.1).partial_cmp(&((b.2 / 200.0).round(), b.1)).unwrap());
        let vp = self.target_viewport_of(self.cur);
        let (ax, ay, aw, ah) = self.work_area();
        let centre = vp.screen_to_canvas((ax + aw / 2.0, ay + ah / 2.0));
        // The frame the view is on: the one whose centre the view's centre is closest to.
        let here = ids
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                let d = |f: &&(u32, f64, f64)| {
                    let r = self.frames.list.iter().find(|x| x.id == f.0).unwrap().rect;
                    let c = (r.loc.x + r.size.w / 2.0, r.loc.y + r.size.h / 2.0);
                    (c.0 - centre.0).hypot(c.1 - centre.1)
                };
                d(a).total_cmp(&d(b))
            })
            .map(|(i, _)| i)
            .unwrap_or(0);
        let n = ids.len();
        let next = if forward { (here + 1) % n } else { (here + n - 1) % n };
        self.frame_fit(ids[next].0);
    }

    /// Height of the title strip in screen px.
    pub fn frame_strip_height(&self) -> f64 {
        11.0 * self.config.frame_label_scale
    }

    /// What of a frame is at screen point `p` on the pointer's monitor: the title strip (drag to
    /// move the frame and its windows) or the bottom-right corner (drag to resize). Topmost frame
    /// first.
    pub fn frame_hit(&self, p: Point<f64, Logical>) -> Option<(u32, FrameHit)> {
        let m = self.mon_at(p);
        let local = p - self.origin_of(m);
        let vp = self.viewport_of(m);
        let strip = self.frame_strip_height();
        let corner = (strip * 1.5).max(14.0);
        for f in self.frames.list.iter().rev().filter(|f| f.tags & self.tagset != 0) {
            let (x, y) = vp.canvas_to_screen((f.rect.loc.x, f.rect.loc.y));
            let (w, h) = (f.rect.size.w * vp.zoom, f.rect.size.h * vp.zoom);
            let inside = local.x >= x && local.x < x + w && local.y >= y && local.y < y + h;
            if !inside {
                continue;
            }
            if local.x >= x + w - corner && local.y >= y + h - corner {
                return Some((f.id, FrameHit::Corner));
            }
            if local.y < y + strip.min(h) {
                return Some((f.id, FrameHit::Strip));
            }
        }
        None
    }

    /// Start dragging frame `id` (its windows are remembered now, so a window the frame passes
    /// over on the way does not join).
    pub fn frame_begin_move(&mut self, id: u32) {
        let Some(i) = self.frame_index(id) else { return };
        let loc = self.frames.list[i].rect.loc;
        let members = self.frame_members(id);
        self.frames.carrying =
            members.into_iter().filter_map(|w| self.client_index(w).map(|c| (w, self.clients[c].canvas))).collect();
        self.drag = Some(crate::state::Drag::FrameMove { id, start: self.cursor, orig: loc });
    }

    pub fn frame_begin_resize(&mut self, id: u32) {
        let Some(i) = self.frame_index(id) else { return };
        let size = self.frames.list[i].rect.size;
        self.drag = Some(crate::state::Drag::FrameResize { id, start: self.cursor, orig: size });
    }

    /// The drag moved: place the frame and everything it carries relative to where they started.
    pub fn frame_drag_to(&mut self, id: u32, d: (f64, f64), orig: Point<f64, Logical>) {
        let Some(i) = self.frame_index(id) else { return };
        let to = (orig.x + d.0, orig.y + d.1);
        self.frames.list[i].rect.loc = (to.0.round(), to.1.round()).into();
        let carried = self.frames.carrying.clone();
        for (w, at) in carried {
            self.move_client(w, at.x + d.0, at.y + d.1);
        }
        self.frames_changed();
    }

    /// The frames as render elements (screen order: label, edges, strip, fill), above the
    /// wallpaper and below every window.
    pub fn region_elements(&mut self, renderer: &mut GlesRenderer, m: usize, scale: f64) -> Vec<UwcElement> {
        if self.frames.list.is_empty() {
            return Vec::new();
        }
        let vp = self.viewport_of(m);
        let strip = self.frame_strip_height();
        let color = self.config.frame_color;
        let lum = 0.2126 * color[0] + 0.7152 * color[1] + 0.0722 * color[2];
        let fg = if lum > 0.55 { [0.05, 0.05, 0.08, 1.0] } else { [1.0, 1.0, 1.0, 1.0] };
        let px = ((self.config.frame_label_scale * scale).round() as i32).max(1);
        let bw = ((self.config.frame_border_width * scale).round() as i32).max(0);
        let commit = self.deco_commit;
        let mut out = Vec::new();
        // Premultiplied colours at an opacity.
        let pm = |a: f32| [color[0] * a, color[1] * a, color[2] * a, a];
        let clamp = |v: f64| v.clamp(-1.0e6, 1.0e6).round() as i32;
        let (mut cache, tagset) = (std::mem::take(&mut self.frames.cache), self.tagset);
        for f in self.frames.list.iter().rev().filter(|f| f.tags & tagset != 0) {
            let (x, y) = vp.canvas_to_screen((f.rect.loc.x, f.rect.loc.y));
            let (w, h) = (f.rect.size.w * vp.zoom, f.rect.size.h * vp.zoom);
            let (x0, y0, x1, y1) = (clamp(x * scale), clamp(y * scale), clamp((x + w) * scale), clamp((y + h) * scale));
            if x1 <= 0 && y1 <= 0 {
                continue;
            }
            let rect =
                |a: i32, b: i32, c: i32, d: i32| Rectangle::<i32, Physical>::new((a, b).into(), (c - a, d - b).into());
            let solid = |id: &Id, r: Rectangle<i32, Physical>, c: [f32; 4]| {
                UwcElement::from(SolidColorRenderElement::new(id.clone(), r, commit, c, Kind::Unspecified))
            };
            let strip_px = ((strip * scale).round() as i32).min(y1 - y0).max(0);
            // Label first: it is on top.
            if !f.label.is_empty() {
                let key = (f.id, f.label.clone(), px);
                let (buf, bwid, bhei) = cache.entry(key).or_insert_with(|| {
                    let (pix, w, h) = label_pixels(&f.label, 0, px, color, fg);
                    (MemoryRenderBuffer::from_slice(&pix, Fourcc::Abgr8888, (w, h), 1, Transform::Normal, None), w, h)
                });
                // The badge is the strip's height: clip nothing, but keep it off tiny frames.
                if *bhei <= strip_px.max(*bhei) && *bwid < (x1 - x0).max(1) {
                    let at = Point::<f64, Physical>::from((x0 as f64, y0 as f64));
                    if let Ok(e) = MemoryRenderBufferRenderElement::from_buffer(
                        renderer,
                        at,
                        &*buf,
                        None,
                        None,
                        None,
                        Kind::Unspecified,
                    ) {
                        out.push(UwcElement::from(e));
                    }
                }
            }
            if bw > 0 {
                let edge = pm(0.9);
                let b = bw.min((x1 - x0) / 2).min((y1 - y0) / 2).max(0);
                if b > 0 {
                    out.push(solid(&f.ids[1], rect(x0, y0, x1, y0 + b), edge));
                    out.push(solid(&f.ids[2], rect(x0, y1 - b, x1, y1), edge));
                    out.push(solid(&f.ids[3], rect(x0, y0 + b, x0 + b, y1 - b), edge));
                    out.push(solid(&f.ids[4], rect(x1 - b, y0 + b, x1, y1 - b), edge));
                }
            }
            out.push(solid(&f.ids[5], rect(x0, y0, x1, y0 + strip_px), pm(0.35)));
            out.push(solid(&f.ids[0], rect(x0, y0, x1, y1), pm(self.config.frame_opacity as f32)));
        }
        self.frames.cache = cache;
        out
    }
}
