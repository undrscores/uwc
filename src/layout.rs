//! Tiling layouts as pure maths: an area, a window count and some options in, one rect per window
//! out (in the order the windows are given). Nothing here knows about windows or the compositor;
//! `tiling.rs` applies the result.
//!
//! A window can have a smallest size (`mins`, one per window in the same order, 0 = none): its slot is
//! never made smaller than that when the area has room, and the space is shared among the others. When
//! the minimums cannot all fit, the windows keep them and run past the area rather than overlap.
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

/// A window's smallest size in px; 0 = no limit.
pub type Min = (i32, i32);

fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
    Rectangle::new((x, y).into(), (w, h).into())
}

fn min_of(mins: &[Min], i: usize) -> Min {
    mins.get(i).copied().unwrap_or((0, 0))
}

/// The largest minimum along one axis among `mins` (a lower bound for the space they need).
fn need(mins: &[Min], vertical: bool) -> i32 {
    mins.iter().map(|m| if vertical { m.1 } else { m.0 }).max().unwrap_or(0).max(0)
}

/// Which side of a rect a window takes in a split.
#[derive(Clone, Copy)]
enum Side {
    Left,
    Top,
    Right,
    Bottom,
}

/// Cut `r` in two with `gap` between them; the window takes `side` and `ratio` of the space, but at
/// least `mine_min` of it and leaving at least `rest_min`. Returns (the window's part, what is left).
/// Too small to cut: both are `r` (the caller overlaps rather than panics; areas this small are not
/// real).
fn split(r: Rect, side: Side, ratio: f64, gap: i32, mine_min: i32, rest_min: i32) -> (Rect, Rect) {
    let horizontal = matches!(side, Side::Left | Side::Right);
    let (len, start) = if horizontal { (r.size.w, r.loc.x) } else { (r.size.h, r.loc.y) };
    let space = len - gap;
    if space < 2 {
        return (r, r);
    }
    let (lo, rest_min) = (mine_min.max(1), rest_min.max(1));
    let want = (space as f64 * ratio).round() as i32;
    let first = if lo + rest_min <= space {
        want.clamp(lo, space - rest_min)
    } else {
        // Both cannot have their minimum: share the shortfall in proportion.
        (space as f64 * lo as f64 / (lo + rest_min) as f64).round() as i32
    }
    .clamp(1, space - 1);
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

/// `mins.len()` parts of `r` along an axis with `gap` between them, as equal as the minimums allow
/// (a part that needs more than its share gets that and the rest share what is left); the
/// remainder goes to the first ones.
fn stack(r: Rect, mins: &[i32], gap: i32, vertical: bool) -> Vec<Rect> {
    let k = mins.len().max(1);
    let min = |i: usize| mins.get(i).copied().unwrap_or(0).max(1);
    let len = if vertical { r.size.h } else { r.size.w };
    let space = (len - gap * (k as i32 - 1)).max(k as i32);
    let mut size = vec![0; k];
    let mut fixed = vec![false; k];
    loop {
        let free: Vec<usize> = (0..k).filter(|&i| !fixed[i]).collect();
        if free.is_empty() {
            break;
        }
        let left = space - (0..k).filter(|&i| fixed[i]).map(|i| size[i]).sum::<i32>();
        let share = left / free.len() as i32;
        let mut changed = false;
        for &i in &free {
            if min(i) > share {
                size[i] = min(i);
                fixed[i] = true;
                changed = true;
            }
        }
        if !changed {
            let (base, extra) = (left / free.len() as i32, (left % free.len() as i32) as usize);
            for (j, &i) in free.iter().enumerate() {
                size[i] = base + i32::from(j < extra);
            }
            break;
        }
    }
    let mut at = if vertical { r.loc.y } else { r.loc.x };
    size.into_iter()
        .map(|n| {
            let out = if vertical { rect(r.loc.x, at, r.size.w, n) } else { rect(at, r.loc.y, n, r.size.h) };
            at += n + gap;
            out
        })
        .collect()
}

/// Where each of `count` windows goes inside `area`.
pub fn arrange(layout: Layout, area: Rect, count: usize, o: Opts, mins: &[Min]) -> Vec<Rect> {
    if count == 0 {
        return Vec::new();
    }
    let outer = o.outer.max(0);
    let gap = o.inner.max(0);
    let ratio = if o.ratio.is_nan() { 0.5 } else { o.ratio.clamp(0.1, 0.9) };
    let area = rect(
        area.loc.x + outer,
        area.loc.y + outer,
        (area.size.w - 2 * outer).max(1),
        (area.size.h - 2 * outer).max(1),
    );
    let mins: Vec<Min> = (0..count).map(|i| min_of(mins, i)).collect();
    let along =
        |ms: &[Min], vertical: bool| -> Vec<i32> { ms.iter().map(|m| if vertical { m.1 } else { m.0 }).collect() };
    match layout {
        Layout::Columns => stack(area, &along(&mins, false), gap, false),
        Layout::MasterStack => {
            let masters = o.masters.clamp(1, count);
            if masters == count {
                return stack(area, &along(&mins, true), gap, true);
            }
            let (m, rest) = mins.split_at(masters);
            let (left, right) = split(area, Side::Left, ratio, gap, need(m, false), need(rest, false));
            let mut out = stack(left, &along(m, true), gap, true);
            out.extend(stack(right, &along(rest, true), gap, true));
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
                let vertical = matches!(side, Side::Top | Side::Bottom);
                let own = if vertical { mins[i].1 } else { mins[i].0 };
                let (mine, rest) = split(left, side, ratio, gap, own, need(&mins[i + 1..], vertical));
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
                        let rects = arrange(layout, area(), count, o, &[]);
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
                let rects = arrange(layout, area(), count, Opts::default(), &[]);
                let covered: i64 = rects.iter().map(|r| r.size.w as i64 * r.size.h as i64).sum();
                assert_eq!(covered, AREA.2 as i64 * AREA.3 as i64, "{layout:?} with {count} windows");
            }
        }
    }

    #[test]
    fn the_gaps_are_exact() {
        // One window: the outer gap on every side, no inner gap.
        let one =
            arrange(Layout::Dwindle, rect(0, 0, 1000, 600), 1, Opts { outer: 16, inner: 12, ..Opts::default() }, &[]);
        assert_eq!(one, vec![rect(16, 16, 968, 568)]);
        // Two side by side: exactly `inner` between them, `outer` to the edges (not outer + inner / 2).
        let two = arrange(
            Layout::MasterStack,
            rect(0, 0, 1000, 600),
            2,
            Opts { outer: 16, inner: 11, ..Opts::default() },
            &[],
        );
        assert_eq!(two[0].loc.x, 16);
        assert_eq!(two[1].loc.x - (two[0].loc.x + two[0].size.w), 11);
        assert_eq!(two[1].loc.x + two[1].size.w, 1000 - 16);
    }

    #[test]
    fn master_stack_shape() {
        let o = Opts { ratio: 0.6, ..Opts::default() };
        let r = arrange(Layout::MasterStack, rect(0, 0, 1000, 600), 3, o, &[]);
        assert_eq!(r[0], rect(0, 0, 600, 600));
        assert_eq!(r[1], rect(600, 0, 400, 300));
        assert_eq!(r[2], rect(600, 300, 400, 300));
        // Two masters share the left column; the rest stack on the right.
        let r = arrange(Layout::MasterStack, rect(0, 0, 1000, 600), 3, Opts { masters: 2, ..o }, &[]);
        assert_eq!((r[0].size.w, r[1].size.w, r[2].size.w), (600, 600, 400));
        assert_eq!(r[2].size.h, 600);
        // As many masters as windows: no stack column.
        let r = arrange(Layout::MasterStack, rect(0, 0, 1000, 600), 2, Opts { masters: 5, ..o }, &[]);
        assert_eq!((r[0].size.w, r[1].size.w), (1000, 1000));
    }

    #[test]
    fn spiral_turns_inward() {
        let r = arrange(Layout::Spiral, rect(0, 0, 1000, 1000), 5, Opts::default(), &[]);
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
                    assert_eq!(arrange(layout, rect(0, 0, w, h), count, o, &[]).len(), count);
                    let o = Opts { outer: -5, inner: -5, ratio: f64::NAN, masters: 99 };
                    assert_eq!(arrange(layout, rect(0, 0, w, h), count, o, &[]).len(), count);
                }
            }
        }
    }

    #[test]
    fn a_window_that_will_not_shrink_gets_the_room_it_needs() {
        let o = Opts { ratio: 0.5, ..Opts::default() };
        let a = rect(0, 0, 1000, 600);
        // Master-stack: a master that needs 700 px of width makes the master column wider than the ratio.
        let r = arrange(Layout::MasterStack, a, 3, o, &[(700, 0)]);
        assert_eq!((r[0].size.w, r[1].size.w), (700, 300));
        // A stacked window that needs 400 px of height takes it from the others.
        let r = arrange(Layout::MasterStack, a, 4, o, &[(0, 0), (0, 400)]);
        assert_eq!((r[1].size.h, r[2].size.h, r[3].size.h), (400, 100, 100));
        // Columns: the others share what is left.
        let r = arrange(Layout::Columns, a, 3, o, &[(0, 0), (600, 0), (0, 0)]);
        assert_eq!((r[0].size.w, r[1].size.w, r[2].size.w), (200, 600, 200));
        // Dwindle: the first window needs more than half.
        let r = arrange(Layout::Dwindle, a, 3, o, &[(800, 0)]);
        assert_eq!(r[0].size.w, 800);
        // Nothing overlaps while it fits, and the others keep a pixel at least.
        for layout in Layout::ALL {
            let r = arrange(layout, a, 4, Opts { inner: 10, ..o }, &[(300, 150), (0, 0), (250, 100), (0, 0)]);
            for (i, x) in r.iter().enumerate() {
                assert!(x.size.w >= 1 && x.size.h >= 1);
                for y in &r[i + 1..] {
                    assert!(!overlap(*x, *y), "{layout:?}: {x:?} overlaps {y:?}");
                }
            }
            assert!(r[0].size.w >= 300 && r[0].size.h >= 150 || layout == Layout::Columns, "{layout:?}: {:?}", r[0]);
        }
    }

    #[test]
    fn minimums_that_cannot_fit_run_past_the_area_without_overlapping() {
        let a = rect(0, 0, 1000, 600);
        let r = arrange(Layout::Columns, a, 3, Opts::default(), &[(600, 0), (600, 0), (600, 0)]);
        assert!(r.iter().all(|x| x.size.w == 600));
        assert_eq!((r[1].loc.x, r[2].loc.x), (600, 1200));
        // Never a panic, whatever is asked for.
        for layout in Layout::ALL {
            let r =
                arrange(layout, rect(0, 0, 50, 50), 5, Opts { inner: 9, ..Opts::default() }, &[(i32::MAX / 8, -4); 5]);
            assert_eq!(r.len(), 5);
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
