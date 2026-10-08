//! Box-drawing (U+2500–257F) and block (U+2580–259F) characters drawn as
//! shapes on the cell grid, like Ghostty, kitty and Alacritty do. Font glyphs
//! for these only meet when the line height matches the font's; drawn, table
//! borders and agent UIs join seamlessly at any font and spacing.

use gpui::{Bounds, Hsla, Path, PathBuilder, Pixels, Point, point, px, size};

pub(super) enum Shape {
    Rect(Bounds<Pixels>, Hsla),
    Path(Path<Pixels>, Hsla),
}

/// Line weights per direction: 0 none, 1 light, 2 heavy, 3 double.
#[derive(Clone, Copy)]
struct Arms {
    up: u8,
    right: u8,
    down: u8,
    left: u8,
}

const fn a(up: u8, right: u8, down: u8, left: u8) -> Arms {
    Arms { up, right, down, left }
}

/// U+2500..=U+257F; dashed lines are drawn solid, diagonals are left to the font.
#[rustfmt::skip]
const LINES: [Option<Arms>; 128] = {
    let n = None;
    [
        // 2500 ─ ━ │ ┃ ┄ ┅ ┆ ┇ ┈ ┉ ┊ ┋ ┌ ┍ ┎ ┏
        Some(a(0,1,0,1)), Some(a(0,2,0,2)), Some(a(1,0,1,0)), Some(a(2,0,2,0)),
        Some(a(0,1,0,1)), Some(a(0,2,0,2)), Some(a(1,0,1,0)), Some(a(2,0,2,0)),
        Some(a(0,1,0,1)), Some(a(0,2,0,2)), Some(a(1,0,1,0)), Some(a(2,0,2,0)),
        Some(a(0,1,1,0)), Some(a(0,2,1,0)), Some(a(0,1,2,0)), Some(a(0,2,2,0)),
        // 2510 ┐ ┑ ┒ ┓ └ ┕ ┖ ┗ ┘ ┙ ┚ ┛ ├ ┝ ┞ ┟
        Some(a(0,0,1,1)), Some(a(0,0,1,2)), Some(a(0,0,2,1)), Some(a(0,0,2,2)),
        Some(a(1,1,0,0)), Some(a(1,2,0,0)), Some(a(2,1,0,0)), Some(a(2,2,0,0)),
        Some(a(1,0,0,1)), Some(a(1,0,0,2)), Some(a(2,0,0,1)), Some(a(2,0,0,2)),
        Some(a(1,1,1,0)), Some(a(1,2,1,0)), Some(a(2,1,1,0)), Some(a(1,1,2,0)),
        // 2520 ┠ ┡ ┢ ┣ ┤ ┥ ┦ ┧ ┨ ┩ ┪ ┫ ┬ ┭ ┮ ┯
        Some(a(2,1,2,0)), Some(a(2,2,1,0)), Some(a(1,2,2,0)), Some(a(2,2,2,0)),
        Some(a(1,0,1,1)), Some(a(1,0,1,2)), Some(a(2,0,1,1)), Some(a(1,0,2,1)),
        Some(a(2,0,2,1)), Some(a(2,0,1,2)), Some(a(1,0,2,2)), Some(a(2,0,2,2)),
        Some(a(0,1,1,1)), Some(a(0,1,1,2)), Some(a(0,2,1,1)), Some(a(0,2,1,2)),
        // 2530 ┰ ┱ ┲ ┳ ┴ ┵ ┶ ┷ ┸ ┹ ┺ ┻ ┼ ┽ ┾ ┿
        Some(a(0,1,2,1)), Some(a(0,1,2,2)), Some(a(0,2,2,1)), Some(a(0,2,2,2)),
        Some(a(1,1,0,1)), Some(a(1,1,0,2)), Some(a(1,2,0,1)), Some(a(1,2,0,2)),
        Some(a(2,1,0,1)), Some(a(2,1,0,2)), Some(a(2,2,0,1)), Some(a(2,2,0,2)),
        Some(a(1,1,1,1)), Some(a(1,1,1,2)), Some(a(1,2,1,1)), Some(a(1,2,1,2)),
        // 2540 ╀ ╁ ╂ ╃ ╄ ╅ ╆ ╇ ╈ ╉ ╊ ╋ ╌ ╍ ╎ ╏
        Some(a(2,1,1,1)), Some(a(1,1,2,1)), Some(a(2,1,2,1)), Some(a(2,1,1,2)),
        Some(a(2,2,1,1)), Some(a(1,1,2,2)), Some(a(1,2,2,1)), Some(a(2,2,1,2)),
        Some(a(1,2,2,2)), Some(a(2,1,2,2)), Some(a(2,2,2,1)), Some(a(2,2,2,2)),
        Some(a(0,1,0,1)), Some(a(0,2,0,2)), Some(a(1,0,1,0)), Some(a(2,0,2,0)),
        // 2550 ═ ║ ╒ ╓ ╔ ╕ ╖ ╗ ╘ ╙ ╚ ╛ ╜ ╝ ╞ ╟
        Some(a(0,3,0,3)), Some(a(3,0,3,0)), Some(a(0,3,1,0)), Some(a(0,1,3,0)),
        Some(a(0,3,3,0)), Some(a(0,0,1,3)), Some(a(0,0,3,1)), Some(a(0,0,3,3)),
        Some(a(1,3,0,0)), Some(a(3,1,0,0)), Some(a(3,3,0,0)), Some(a(1,0,0,3)),
        Some(a(3,0,0,1)), Some(a(3,0,0,3)), Some(a(1,3,1,0)), Some(a(3,1,3,0)),
        // 2560 ╠ ╡ ╢ ╣ ╤ ╥ ╦ ╧ ╨ ╩ ╪ ╫ ╬ ╭ ╮ ╯
        Some(a(3,3,3,0)), Some(a(1,0,1,3)), Some(a(3,0,3,1)), Some(a(3,0,3,3)),
        Some(a(0,3,1,3)), Some(a(0,1,3,1)), Some(a(0,3,3,3)), Some(a(1,3,0,3)),
        Some(a(3,1,0,1)), Some(a(3,3,0,3)), Some(a(1,3,1,3)), Some(a(3,1,3,1)),
        Some(a(3,3,3,3)), Some(a(0,1,1,0)), Some(a(0,0,1,1)), Some(a(1,0,0,1)),
        // 2570 ╰ ╱ ╲ ╳ ╴ ╵ ╶ ╷ ╸ ╹ ╺ ╻ ╼ ╽ ╾ ╿
        Some(a(1,1,0,0)), n, n, n,
        Some(a(0,0,0,1)), Some(a(1,0,0,0)), Some(a(0,1,0,0)), Some(a(0,0,1,0)),
        Some(a(0,0,0,2)), Some(a(2,0,0,0)), Some(a(0,2,0,0)), Some(a(0,0,2,0)),
        Some(a(0,2,0,1)), Some(a(1,0,2,0)), Some(a(0,1,0,2)), Some(a(2,0,1,0)),
    ]
};

/// Whether `c` is drawn here rather than by the font.
pub(super) fn handles(c: char) -> bool {
    match c as u32 {
        0x2500..=0x257F => LINES[(c as u32 - 0x2500) as usize].is_some(),
        0x2580..=0x259F => true,
        _ => false,
    }
}

/// Shapes for `c` in the cell at `o`, `w`×`h` pixels.
pub(super) fn draw(c: char, o: Point<Pixels>, w: f32, h: f32, color: Hsla, out: &mut Vec<Shape>) {
    let cp = c as u32;
    if (0x2580..=0x259F).contains(&cp) {
        block(cp, o, w, h, color, out);
        return;
    }
    let Some(arms) = LINES.get((cp.wrapping_sub(0x2500)) as usize).copied().flatten() else { return };
    // Light lines are about a twelfth of the font's height, at least a pixel.
    let light = (h / 14.).round().max(1.);
    let heavy = light * 2.;
    let gap = light; // between the two strokes of a double line
    let (x0, y0) = (f32::from(o.x), f32::from(o.y));
    let (cx, cy) = (x0 + (w / 2.).floor(), y0 + (h / 2.).floor());
    let rect = |x: f32, y: f32, rw: f32, rh: f32| Shape::Rect(Bounds::new(point(px(x), px(y)), size(px(rw), px(rh))), color);

    // Rounded corners: a quarter curve between the two arms.
    if (0x256D..=0x2570).contains(&cp) {
        let r = (w.min(h) / 2.).floor();
        let half = light / 2.;
        let (cxl, cyl) = (cx + half, cy + half);
        let (from, ctrl, to, edge_a, edge_b) = match cp {
            0x256D => (point(cxl + r, cyl), point(cxl, cyl), point(cxl, cyl + r), point(x0 + w, cyl), point(cxl, y0 + h)),
            0x256E => (point(cxl - r, cyl), point(cxl, cyl), point(cxl, cyl + r), point(x0, cyl), point(cxl, y0 + h)),
            0x256F => (point(cxl - r, cyl), point(cxl, cyl), point(cxl, cyl - r), point(x0, cyl), point(cxl, y0)),
            _ => (point(cxl + r, cyl), point(cxl, cyl), point(cxl, cyl - r), point(x0 + w, cyl), point(cxl, y0)),
        };
        let mut b = PathBuilder::stroke(px(light));
        b.move_to(point(px(edge_a.x), px(edge_a.y)));
        b.line_to(point(px(from.x), px(from.y)));
        b.curve_to(point(px(to.x), px(to.y)), point(px(ctrl.x), px(ctrl.y)));
        b.line_to(point(px(edge_b.x), px(edge_b.y)));
        if let Ok(path) = b.build() {
            out.push(Shape::Path(path, color));
        }
        return;
    }

    let thick = |weight: u8| if weight == 2 { heavy } else { light };
    // The span a line of `weight` covers across its own axis, around `c`.
    let span = |weight: u8, c: f32| -> (f32, f32) {
        match weight {
            3 => (c - gap - light, c + gap + light),
            wt => {
                let t = thick(wt);
                let lo = c - (t / 2.).floor();
                (lo, lo + t)
            }
        }
    };
    let v = arms.up.max(arms.down);
    let hz = arms.left.max(arms.right);
    let (vx0, vx1) = span(v.max(1), cx);
    let (hy0, hy1) = span(hz.max(1), cy);
    // Horizontal arms meet the vertical line's far edge; lone half-lines stop at the center.
    let left_end = if v > 0 { vx1 } else { cx + light / 2. };
    let right_start = if v > 0 { vx0 } else { cx - light / 2. };
    let up_end = if hz > 0 { hy1 } else { cy + light / 2. };
    let down_start = if hz > 0 { hy0 } else { cy - light / 2. };

    let hline = |weight: u8, from: f32, to: f32, out: &mut Vec<Shape>| match weight {
        0 => {}
        3 => {
            out.push(rect(from, cy - gap - light, to - from, light));
            out.push(rect(from, cy + gap, to - from, light));
        }
        wt => {
            let (y, y1) = span(wt, cy);
            out.push(rect(from, y, to - from, y1 - y));
        }
    };
    let vline = |weight: u8, from: f32, to: f32, out: &mut Vec<Shape>| match weight {
        0 => {}
        3 => {
            out.push(rect(cx - gap - light, from, light, to - from));
            out.push(rect(cx + gap, from, light, to - from));
        }
        wt => {
            let (x, x1) = span(wt, cx);
            out.push(rect(x, from, x1 - x, to - from));
        }
    };
    hline(arms.left, x0, left_end, out);
    hline(arms.right, right_start, x0 + w, out);
    vline(arms.up, y0, up_end, out);
    vline(arms.down, down_start, y0 + h, out);
}

fn block(cp: u32, o: Point<Pixels>, w: f32, h: f32, color: Hsla, out: &mut Vec<Shape>) {
    let (x0, y0) = (f32::from(o.x), f32::from(o.y));
    let mut fill = |fx: f32, fy: f32, fw: f32, fh: f32, alpha: f32| {
        let c = Hsla { a: color.a * alpha, ..color };
        out.push(Shape::Rect(Bounds::new(point(px(x0 + fx * w), px(y0 + fy * h)), size(px(fw * w), px(fh * h))), c));
    };
    let eighth = |n: u32| n as f32 / 8.;
    match cp {
        0x2580 => fill(0., 0., 1., 0.5, 1.),
        0x2581..=0x2587 => {
            let part = eighth(cp - 0x2580);
            fill(0., 1. - part, 1., part, 1.)
        }
        0x2588 => fill(0., 0., 1., 1., 1.),
        0x2589..=0x258F => fill(0., 0., eighth(0x2590 - cp), 1., 1.),
        0x2590 => fill(0.5, 0., 0.5, 1., 1.),
        0x2591 => fill(0., 0., 1., 1., 0.25),
        0x2592 => fill(0., 0., 1., 1., 0.5),
        0x2593 => fill(0., 0., 1., 1., 0.75),
        0x2594 => fill(0., 0., 1., eighth(1), 1.),
        0x2595 => fill(1. - eighth(1), 0., eighth(1), 1., 1.),
        _ => {
            // Quadrants: upper-left, upper-right, lower-left, lower-right.
            let (ul, ur, ll, lr) = match cp {
                0x2596 => (false, false, true, false),
                0x2597 => (false, false, false, true),
                0x2598 => (true, false, false, false),
                0x2599 => (true, false, true, true),
                0x259A => (true, false, false, true),
                0x259B => (true, true, true, false),
                0x259C => (true, true, false, true),
                0x259D => (false, true, false, false),
                0x259E => (false, true, true, false),
                _ => (false, true, true, true),
            };
            for (on, fx, fy) in [(ul, 0., 0.), (ur, 0.5, 0.), (ll, 0., 0.5), (lr, 0.5, 0.5)] {
                if on {
                    fill(fx, fy, 0.5, 0.5, 1.);
                }
            }
        }
    }
}
