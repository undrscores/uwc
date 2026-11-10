//! Jumping to a window on an unbounded canvas. Two ways in:
//!
//! - **Hint mode** (`jump-hints`, Mod+slash): the view zooms out to show every window and each gets a
//!   short label from the home row (`hint-keys`), drawn over its top-left corner. Typing a label
//!   focuses that window and glides the view to it at the zoom you came from; Escape puts everything
//!   back. While it is up every key goes to it, not to clients or binds.
//! - **`jump QUERY`** over IPC (and `examples/uwc-jump`, which feeds it from a dmenu-style launcher):
//!   fuzzy-matches the app-id and title, views the window's tag, restores it if minimized, and
//!   focuses and reveals it.
//!
//! Labels are tiny textures made from a 5x7 pixel font, so uwc needs no font stack.

use std::collections::HashMap;

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            element::{memory::MemoryRenderBuffer, memory::MemoryRenderBufferRenderElement, Kind},
            gles::GlesRenderer,
        },
    },
    utils::{Physical, Point, Transform},
};

use crate::{render::UwcElement, state::Uwc};

/// Rows of 5 bits, most significant bit on the left.
fn glyph(c: char) -> [u8; 7] {
    match c.to_ascii_uppercase() {
        'A' => [0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'B' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110],
        'C' => [0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110],
        'D' => [0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110],
        'E' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111],
        'F' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000],
        'G' => [0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01111],
        'H' => [0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'I' => [0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        'J' => [0b00111, 0b00010, 0b00010, 0b00010, 0b00010, 0b10010, 0b01100],
        'K' => [0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001],
        'L' => [0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111],
        'M' => [0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001],
        'N' => [0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001],
        'O' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'P' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000],
        'Q' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101],
        'R' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001],
        'S' => [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110],
        'T' => [0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
        'U' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'V' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100],
        'W' => [0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b10101, 0b01010],
        'X' => [0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001],
        'Y' => [0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100],
        'Z' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111],
        '0' => [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
        '1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        '2' => [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
        '3' => [0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110],
        '4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
        '5' => [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
        '6' => [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
        '7' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
        '8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
        '9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
        _ => [0; 7],
    }
}

/// RGBA pixels of `label` (its first `typed` characters dimmed) on a `bg` badge, each font pixel
/// `px` device pixels square. Returns (pixels, width, height).
pub fn label_pixels(label: &str, typed: usize, px: i32, bg: [f32; 4], fg: [f32; 4]) -> (Vec<u8>, i32, i32) {
    let n = label.chars().count() as i32;
    let pad = 2;
    // 5 columns per glyph and one between glyphs, `pad` around everything.
    let (gw, gh) = (n * 6 - 1 + 2 * pad, 7 + 2 * pad);
    let (w, h) = (gw * px, gh * px);
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    let mut out = vec![0u8; (w * h * 4) as usize];
    let put = |out: &mut Vec<u8>, x: i32, y: i32, c: [f32; 4], mix: f32| {
        let i = ((y * w + x) * 4) as usize;
        // Dimmed text is the text colour mixed towards the badge.
        let m = |a: f32, b: f32| a * mix + b * (1.0 - mix);
        out[i..i + 4].copy_from_slice(&[byte(m(c[0], bg[0])), byte(m(c[1], bg[1])), byte(m(c[2], bg[2])), 255]);
    };
    for y in 0..h {
        for x in 0..w {
            put(&mut out, x, y, bg, 1.0);
        }
    }
    for (k, ch) in label.chars().enumerate() {
        let mix = if k < typed { 0.35 } else { 1.0 };
        for (row, bits) in glyph(ch).iter().enumerate() {
            for col in 0..5 {
                if bits >> (4 - col) & 1 == 1 {
                    let (gx, gy) = ((pad + k as i32 * 6 + col) * px, (pad + row as i32) * px);
                    for dy in 0..px {
                        for dx in 0..px {
                            put(&mut out, gx + dx, gy + dy, fg, mix);
                        }
                    }
                }
            }
        }
    }
    (out, w, h)
}

/// Labels for `n` windows from `keys`: single characters while they suffice, else all pairs
/// (`keys` squared), in key order so the first windows get the easiest ones.
pub fn make_labels(keys: &[char], n: usize) -> Vec<String> {
    let k = keys.len().max(1);
    if n <= k {
        return keys.iter().take(n).map(|c| c.to_string()).collect();
    }
    // With pairs, no label may be the prefix of another, so every one has two characters.
    keys.iter().flat_map(|a| keys.iter().map(move |b| format!("{a}{b}"))).take(n).collect()
}

/// How well `query` matches `text` as a case-insensitive subsequence: None if it does not, else a
/// score where higher is better (consecutive characters, word starts and an early start score).
pub fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
    let q: Vec<char> = query.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
    if q.is_empty() {
        return Some(0);
    }
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut qi, mut score, mut prev) = (0usize, 0i32, None::<usize>);
    for (i, &c) in t.iter().enumerate() {
        if qi < q.len() && c == q[qi] {
            let word_start = i == 0 || !t[i - 1].is_alphanumeric();
            score += 10;
            if prev == Some(i.wrapping_sub(1)) && i > 0 {
                score += 15;
            }
            if word_start {
                score += 12;
            }
            if qi == 0 {
                score -= i.min(20) as i32;
            }
            prev = Some(i);
            qi += 1;
        }
    }
    (qi == q.len()).then_some(score)
}

pub struct Hints {
    /// Whether hint mode zoomed the view out itself (so cancelling zooms back).
    entered_overview: bool,
    /// (window id, label).
    labels: Vec<(u32, String)>,
    typed: String,
    cache: HashMap<(String, usize, i32), (MemoryRenderBuffer, i32, i32)>,
}

/// What a key means to hint mode.
#[derive(Debug, Clone, PartialEq)]
pub enum HintKey {
    Char(char),
    Backspace,
    Cancel,
    /// A modifier or other key with no meaning here: swallowed.
    Ignore,
}

impl Uwc {
    pub fn hints_active(&self) -> bool {
        self.hints.is_some()
    }

    pub fn hints_toggle(&mut self) {
        if self.hints.is_some() {
            return self.hints_cancel();
        }
        if self.is_locked() {
            return;
        }
        let cur = self.cur;
        // Reading order: rows from the top, left to right, as laid out on the screen.
        let mut wins: Vec<(u32, f64, f64)> = Vec::new();
        let entered_overview = self.monitors.get(cur).is_some_and(|m| m.overview.is_none());
        if entered_overview {
            self.toggle_overview();
        }
        for c in self.clients.iter().filter(|c| self.visible_on(cur, c)) {
            let p = self.place_on(cur, c, &crate::state::Fx::REST);
            wins.push((c.id, p.origin.x, p.origin.y));
        }
        if wins.is_empty() {
            if entered_overview {
                self.toggle_overview();
            }
            return;
        }
        wins.sort_by(|a, b| ((a.2 / 60.0).round(), a.1).partial_cmp(&((b.2 / 60.0).round(), b.1)).unwrap());
        let keys: Vec<char> = self.config.hint_keys.chars().filter(|c| glyph(*c) != [0; 7]).collect();
        let keys = if keys.is_empty() { "asdfghjkl".chars().collect() } else { keys };
        let labels = make_labels(&keys, wins.len()).into_iter().zip(wins).map(|(l, w)| (w.0, l)).collect();
        self.hints = Some(Hints { entered_overview, labels, typed: String::new(), cache: HashMap::new() });
        self.damage();
    }

    /// Back to where you were.
    pub fn hints_cancel(&mut self) {
        let Some(h) = self.hints.take() else { return };
        if h.entered_overview {
            self.toggle_overview();
        }
        self.damage();
    }

    pub fn hint_key(&mut self, key: HintKey) {
        let Some(h) = self.hints.as_mut() else { return };
        match key {
            HintKey::Ignore => {}
            HintKey::Cancel => self.hints_cancel(),
            HintKey::Backspace => {
                h.typed.pop();
                self.damage();
            }
            HintKey::Char(c) => {
                let c = c.to_ascii_lowercase();
                let mut next = h.typed.clone();
                next.push(c);
                let hits: Vec<(u32, String)> = h.labels.iter().filter(|(_, l)| l.starts_with(&next)).cloned().collect();
                match hits.as_slice() {
                    // No such label: the key is ignored.
                    [] => {}
                    [(id, l)] if *l == next => {
                        let id = *id;
                        self.hints = None;
                        self.hints_jump(id);
                    }
                    _ => {
                        h.typed = next;
                        self.damage();
                    }
                }
            }
        }
    }

    /// Leave the overview for window `id`: focus it and glide to it at the zoom the view had before.
    fn hints_jump(&mut self, id: u32) {
        let cur = self.cur;
        let tagset = self.tagset;
        let before_zoom = self.monitors.get(cur).and_then(|m| m.overview).map(|v| v.zoom);
        self.focus_client(Some(id));
        let Some(i) = self.client_index(id) else { return };
        if !self.place_on(cur, &self.clients[i], &crate::state::Fx::REST).on_canvas {
            // Pinned and fullscreen windows are on screen wherever the view is: go back and stay.
            if let Some(m) = self.monitors.get_mut(cur) {
                if let Some(prev) = m.overview.take() {
                    *m.viewport_mut(tagset) = prev;
                }
            }
            return self.damage();
        }
        let r = self.clients[i].rect();
        let c = (r.loc.x + r.size.w / 2.0, r.loc.y + r.size.h / 2.0);
        self.with_viewport_eased_on(cur, |v, area| {
            if let Some(z) = before_zoom {
                v.zoom = z;
            }
            v.center_on(c, area);
        });
    }

    /// The labels as render elements over their windows (topmost, so drawn first in the list).
    pub fn hint_elements(&mut self, renderer: &mut GlesRenderer, m: usize, scale: f64) -> Vec<UwcElement> {
        let Some(mut h) = self.hints.take() else { return Vec::new() };
        let mut out = Vec::new();
        let bg = self.config.deco.focus_color;
        let lum = 0.2126 * bg[0] + 0.7152 * bg[1] + 0.0722 * bg[2];
        let fg = if lum > 0.55 { [0.05, 0.05, 0.08, 1.0] } else { [1.0, 1.0, 1.0, 1.0] };
        let px = ((self.config.hint_scale * scale).round() as i32).max(1);
        let (ax, ay, aw, ah) = self.work_area_of(m);
        for (id, label) in &h.labels {
            let Some(i) = self.client_index(*id) else { continue };
            if !label.starts_with(&h.typed) || !self.visible_on(m, &self.clients[i]) {
                continue;
            }
            let key = (label.clone(), h.typed.len(), px);
            let (buf, bw, bh) = h.cache.entry(key).or_insert_with(|| {
                let (pix, w, hh) = label_pixels(label, h.typed.len(), px, bg, fg);
                (MemoryRenderBuffer::from_slice(&pix, Fourcc::Abgr8888, (w, hh), 1, Transform::Normal, None), w, hh)
            });
            let (bw, bh) = (*bw as f64, *bh as f64);
            let p = self.place_on(m, &self.clients[i], &crate::state::Fx::REST);
            let mut at = Point::<f64, Physical>::from(((p.origin.x + 6.0) * scale, (p.origin.y + 6.0) * scale));
            // Kept on the screen (inside the work area) even for a window that is partly off it.
            at.x = at.x.clamp(ax * scale, ((ax + aw) * scale - bw).max(ax * scale));
            at.y = at.y.clamp(ay * scale, ((ay + ah) * scale - bh).max(ay * scale));
            if let Ok(e) =
                MemoryRenderBufferRenderElement::from_buffer(renderer, at, &*buf, None, None, None, Kind::Unspecified)
            {
                out.push(UwcElement::from(e));
            }
        }
        self.hints = Some(h);
        out
    }

    /// `jump QUERY`: the window whose app-id and title match best (`#N` is window id N), brought
    /// into view wherever it is: its tag viewed, restored if minimized, focused and revealed.
    pub fn jump_to(&mut self, query: &str) -> Result<u32, String> {
        let query = query.trim();
        let id = if let Some(n) = query.strip_prefix('#') {
            let id: u32 = n.trim().parse().map_err(|_| "`#` needs a window id".to_string())?;
            self.client_index(id).map(|_| id).ok_or_else(|| format!("no window {id}"))?
        } else {
            // Later in `clients` is higher in the stack: it wins ties.
            let mut best: Option<(i32, usize)> = None;
            for (i, c) in self.clients.iter().enumerate().filter(|(_, c)| c.mapped || c.minimized != 0) {
                let text = format!("{} {}", c.app_id(), c.title());
                if let Some(s) = fuzzy_score(query, &text) {
                    if best.is_none_or(|(b, _)| s >= b) {
                        best = Some((s, i));
                    }
                }
            }
            best.map(|(_, i)| self.clients[i].id).ok_or_else(|| format!("no window matches `{query}`"))?
        };
        let i = self.client_index(id).ok_or("the window is gone")?;
        if self.clients[i].minimized != 0 {
            self.set_minimized(id, false);
        } else if !self.visible(&self.clients[i]) {
            let tags = self.clients[i].tags;
            self.set_tagset(1 << tags.trailing_zeros());
        }
        self.hints_cancel();
        self.focus_and_reveal(id);
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_single_keys_then_pairs_without_prefixes() {
        let k: Vec<char> = "asdf".chars().collect();
        assert_eq!(make_labels(&k, 3), ["a", "s", "d"]);
        assert_eq!(make_labels(&k, 4), ["a", "s", "d", "f"]);
        let many = make_labels(&k, 6);
        assert_eq!(many, ["aa", "as", "ad", "af", "sa", "ss"]);
        // Nothing is the start of another.
        for a in &many {
            assert!(many.iter().all(|b| a == b || !b.starts_with(a.as_str())));
        }
    }

    #[test]
    fn fuzzy_matching_prefers_word_starts_and_runs() {
        let s = |q, t| fuzzy_score(q, t);
        assert!(s("ff", "firefox Mozilla Firefox").is_some());
        assert!(s("xyz", "firefox").is_none());
        assert_eq!(s("", "anything"), Some(0));
        // A word-start run beats a scattered match.
        assert!(s("term", "foot Terminal") > s("term", "the empty room"));
        // The start of the text beats the middle.
        assert!(s("fire", "firefox") > s("fire", "a very long title with fire in it"));
        assert!(s("FOX", "firefox").is_some(), "case does not matter");
    }

    #[test]
    fn a_label_is_drawn_with_a_badge_and_dimmed_typed_part() {
        let bg = [0.0, 0.0, 1.0, 1.0];
        let fg = [1.0, 1.0, 1.0, 1.0];
        let (px, w, h) = label_pixels("as", 0, 2, bg, fg);
        // Two glyphs: 2*6-1 columns plus 2 padding on each side, 7 rows plus padding, at 2 px each.
        assert_eq!((w, h), ((11 + 4) * 2, (7 + 4) * 2));
        assert_eq!(px.len(), (w * h * 4) as usize);
        // The corner is badge blue; somewhere there is full-white text.
        assert_eq!(&px[0..4], &[0, 0, 255, 255]);
        assert!(px.chunks(4).any(|p| p == [255, 255, 255, 255]));
        // With the first character typed, its pixels are mixed towards the badge, never white.
        let (dim, ..) = label_pixels("a", 1, 2, bg, fg);
        assert!(!dim.chunks(4).any(|p| p == [255, 255, 255, 255]));
        assert!(dim.chunks(4).any(|p| p[0] > 0 && p[0] < 255));
    }
}
