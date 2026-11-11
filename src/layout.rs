//! Tiling layouts as pure maths: an area, a window count and some options in, one rect per window
//! out (in the order the windows are given). Nothing here knows about windows or the compositor;
//! `tiling.rs` applies the result.
//!
//! Gaps are exact: `outer` between the area's edge and the windows, `inner` between neighbouring
//! windows. Splits take the gap out of the space first and share what is left, so there is no
//! rounding drift and odd gaps stay odd.

use smithay::utils::{Logical, Rectangle};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// `masters` windows in a column on the left, the rest stacked on the right.
    MasterStack,
    /// Each window takes the left, then the top half of what is left, alternating.
    Dwindle,
    /// Like dwindle, but the side the window takes turns right, bottom, left, top: a spiral inward.
    Spiral,
    /// Equal columns side by side.
    Columns,
}

impl Layout {
    pub const ALL: [Layout; 4] = [Layout::MasterStack, Layout::Dwindle, Layout::Spiral, Layout::Columns];

    pub fn name(self) -> &'static str {
        match self {
            Layout::MasterStack => "master-stack",
            Layout::Dwindle => "dwindle",
            Layout::Spiral => "spiral",
            Layout::Columns => "columns",
        }
    }

    pub fn parse(s: &str) -> Option<Layout> {
        Layout::ALL.into_iter().find(|l| l.name() == s)
    }

    /// The names, for error messages.
    pub fn names() -> String {
        Layout::ALL.map(Layout::name).join(", ")
    }

    /// The layout after this one, `None` after the last (tiling off).
    pub fn next(self) -> Option<Layout> {
        let i = Layout::ALL.iter().position(|l| *l == self).unwrap_or(0);
        Layout::ALL.get(i + 1).copied()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Opts {
    /// Between the area's edge and the windows, px.
    pub outer: i32,
    /// Between neighbouring windows, px.
    pub inner: i32,
    /// The share of the space the first window (master column) of each split takes, 0.1 to 0.9.
    pub ratio: f64,
    /// Master-stack: windows in the master column (at least 1).
    pub masters: usize,
}

impl Default for Opts {
    fn default() -> Self {
        Opts { outer: 0, inner: 0, ratio: 0.5, masters: 1 }
    }
}

type Rect = Rectangle<i32, Logical>;

fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
    Rectangle::new((x, y).into(), (w, h).into())
}

/// Which side of a rect a window takes in a split.
#[derive(Clone, Copy)]
enum Side {
    Left,
    Top,
    Right,
    Bottom,
}

/// Cut `r` in two with `gap` between them; the window takes `side` and `ratio` of the space. Returns
/// (the window's part, what is left). Too small to cut: both are `r` (the caller overlaps rather
/// than panics; areas this small are not real).
fn split(r: Rect, side: Side, ratio: f64, gap: i32) -> (Rect, Rect) {
    let horizontal = matches!(side, Side::Left | Side::Right);
    let (len, start) = if horizontal { (r.size.w, r.loc.x) } else { (r.size.h, r.loc.y) };
    let space = len - gap;
    if space < 2 {
        return (r, r);
    }
    let first = ((space as f64 * ratio).round() as i32).clamp(1, space - 1);
    let rest = space - first;
    // The window's part and the remainder along the axis, as (offset, length).
    let (mine, other) = match side {
        Side::Left | Side::Top => ((start, first), (start + first + gap, rest)),
        Side::Right | Side::Bottom => ((start + rest + gap, first), (start, rest)),
    };
    let cut = |(at, n): (i32, i32)| {
        if horizontal {
            rect(at, r.loc.y, n, r.size.h)
        } else {
            rect(r.loc.x, at, r.size.w, n)
        }
    };
    (cut(mine), cut(other))
}

/// `k` equal parts of `r` along an axis with `gap` between them; the remainder goes to the first ones.
fn stack(r: Rect, k: usize, gap: i32, vertical: bool) -> Vec<Rect> {
    let k = k.max(1);
    let len = if vertical { r.size.h } else { r.size.w };
    let space = (len - gap * (k as i32 - 1)).max(k as i32);
    let (base, extra) = (space / k as i32, (space % k as i32) as usize);
    let mut at = if vertical { r.loc.y } else { r.loc.x };
    (0..k)
        .map(|i| {
            let n = base + i32::from(i < extra);
            let out = if vertical { rect(r.loc.x, at, r.size.w, n) } else { rect(at, r.loc.y, n, r.size.h) };
            at += n + gap;
            out
        })
        .collect()
}

/// Where each of `count` windows goes inside `area`.
pub fn arrange(layout: Layout, area: Rect, count: usize, o: Opts) -> Vec<Rect> {
    if count == 0 {
        return Vec::new();
    }
    let outer = o.outer.max(0);
    let gap = o.inner.max(0);
    let ratio = o.ratio.clamp(0.1, 0.9);
    let area = rect(
        area.loc.x + outer,
        area.loc.y + outer,
        (area.size.w - 2 * outer).max(1),
        (area.size.h - 2 * outer).max(1),
    );
    match layout {
        Layout::Columns => stack(area, count, gap, false),
        Layout::MasterStack => {
            let masters = o.masters.clamp(1, count);
            if masters == count {
                return stack(area, count, gap, true);
            }
            let (left, right) = split(area, Side::Left, ratio, gap);
            let mut out = stack(left, masters, gap, true);
            out.extend(stack(right, count - masters, gap, true));
            out
        }
        Layout::Dwindle | Layout::Spiral => {
            let mut out = Vec::with_capacity(count);
            let mut left = area;
            for i in 0..count - 1 {
                let side = if layout == Layout::Dwindle {
                    if i % 2 == 0 {
                        Side::Left
                    } else {
                        Side::Top
                    }
                } else {
                    [Side::Left, Side::Top, Side::Right, Side::Bottom][i % 4]
                };
                let (mine, rest) = split(left, side, ratio, gap);
                out.push(mine);
                left = rest;
            }
            out.push(left);
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overlap(a: Rect, b: Rect) -> bool {
        a.loc.x < b.loc.x + b.size.w
            && b.loc.x < a.loc.x + a.size.w
            && a.loc.y < b.loc.y + b.size.h
            && b.loc.y < a.loc.y + a.size.h
    }

    fn inside(r: Rect, area: Rect) -> bool {
        r.loc.x >= area.loc.x
            && r.loc.y >= area.loc.y
            && r.loc.x + r.size.w <= area.loc.x + area.size.w
            && r.loc.y + r.size.h <= area.loc.y + area.size.h
    }

    const AREA: (i32, i32, i32, i32) = (500, -300, 1921, 1081);

    fn area() -> Rect {
        rect(AREA.0, AREA.1, AREA.2, AREA.3)
    }

    #[test]
    fn windows_stay_inside_and_never_overlap() {
        for layout in Layout::ALL {
            for (outer, inner) in [(0, 0), (16, 12), (7, 5), (0, 9)] {
                for masters in 1..4 {
                    // Dwindle and spiral halve the space with every window: 13 would be a few px each.
                    let most = if matches!(layout, Layout::Dwindle | Layout::Spiral) { 8 } else { 14 };
                    for count in 1..=most {
                        let o = Opts { outer, inner, ratio: 0.55, masters };
                        let rects = arrange(layout, area(), count, o);
                        assert_eq!(rects.len(), count);
                        let bounds = rect(AREA.0 + outer, AREA.1 + outer, AREA.2 - 2 * outer, AREA.3 - 2 * outer);
                        for (i, a) in rects.iter().enumerate() {
                            assert!(a.size.w > 0 && a.size.h > 0, "{layout:?} {count}: empty {a:?}");
                            assert!(inside(*a, bounds), "{layout:?} {count} {o:?}: {a:?} leaves {bounds:?}");
                            for b in &rects[i + 1..] {
                                assert!(!overlap(*a, *b), "{layout:?} {count} {o:?}: {a:?} overlaps {b:?}");
                            }
                        }
                    }
                }
            }
        }
    }

    /// With no gaps the windows are the area exactly: nothing lost to rounding.
    #[test]
    fn without_gaps_the_windows_cover_the_area() {
        for layout in Layout::ALL {
            for count in 1..=14 {
                let rects = arrange(layout, area(), count, Opts::default());
                let covered: i64 = rects.iter().map(|r| r.size.w as i64 * r.size.h as i64).sum();
                assert_eq!(covered, AREA.2 as i64 * AREA.3 as i64, "{layout:?} with {count} windows");
            }
        }
    }

    #[test]
    fn the_gaps_are_exact() {
        // One window: the outer gap on every side, no inner gap.
        let one = arrange(Layout::Dwindle, rect(0, 0, 1000, 600), 1, Opts { outer: 16, inner: 12, ..Opts::default() });
        assert_eq!(one, vec![rect(16, 16, 968, 568)]);
        // Two side by side: exactly `inner` between them, `outer` to the edges (not outer + inner / 2).
        let two =
            arrange(Layout::MasterStack, rect(0, 0, 1000, 600), 2, Opts { outer: 16, inner: 11, ..Opts::default() });
        assert_eq!(two[0].loc.x, 16);
        assert_eq!(two[1].loc.x - (two[0].loc.x + two[0].size.w), 11);
        assert_eq!(two[1].loc.x + two[1].size.w, 1000 - 16);
    }

    #[test]
    fn master_stack_shape() {
        let o = Opts { ratio: 0.6, ..Opts::default() };
        let r = arrange(Layout::MasterStack, rect(0, 0, 1000, 600), 3, o);
        assert_eq!(r[0], rect(0, 0, 600, 600));
        assert_eq!(r[1], rect(600, 0, 400, 300));
        assert_eq!(r[2], rect(600, 300, 400, 300));
        // Two masters share the left column; the rest stack on the right.
        let r = arrange(Layout::MasterStack, rect(0, 0, 1000, 600), 3, Opts { masters: 2, ..o });
        assert_eq!((r[0].size.w, r[1].size.w, r[2].size.w), (600, 600, 400));
        assert_eq!(r[2].size.h, 600);
        // As many masters as windows: no stack column.
        let r = arrange(Layout::MasterStack, rect(0, 0, 1000, 600), 2, Opts { masters: 5, ..o });
        assert_eq!((r[0].size.w, r[1].size.w), (1000, 1000));
    }

    #[test]
    fn spiral_turns_inward() {
        let r = arrange(Layout::Spiral, rect(0, 0, 1000, 1000), 5, Opts::default());
        assert_eq!(r[0], rect(0, 0, 500, 1000)); // left
        assert_eq!(r[1], rect(500, 0, 500, 500)); // top of the right half
        assert_eq!(r[2], rect(750, 500, 250, 500)); // right of the bottom right
        assert_eq!(r[3], rect(500, 750, 250, 250)); // bottom of what is left
    }

    #[test]
    fn odd_sizes_and_silly_options_do_not_panic() {
        for layout in Layout::ALL {
            for count in [0, 1, 2, 9, 40] {
                for (w, h) in [(0, 0), (1, 1), (3, 2), (100, 100)] {
                    let o = Opts { outer: 50, inner: 80, ratio: 7.0, masters: 0 };
                    assert_eq!(arrange(layout, rect(0, 0, w, h), count, o).len(), count);
                    let o = Opts { outer: -5, inner: -5, ratio: f64::NAN, masters: 99 };
                    assert_eq!(arrange(layout, rect(0, 0, w, h), count, o).len(), count);
                }
            }
        }
    }

    #[test]
    fn names_round_trip_and_cycle_ends_in_off() {
        for l in Layout::ALL {
            assert_eq!(Layout::parse(l.name()), Some(l));
        }
        assert_eq!(Layout::parse("nope"), None);
        let mut l = Layout::ALL[0];
        let mut n = 1;
        while let Some(next) = l.next() {
            l = next;
            n += 1;
        }
        assert_eq!(n, Layout::ALL.len());
    }
}
