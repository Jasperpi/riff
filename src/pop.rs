//! Pop art for the four-square mode. Each square is printed like a poster
//! in its own loud colours: a halftone screen fading in from the outer
//! corner, the cover big and turned in towards the middle with a heavy
//! outline and a hard shadow so it stands off the page, and the square's
//! number in big chunky print. The cover is screen-printed in the square's
//! inks until it is the one being heard, when it comes up in its own
//! colours, pumped up, with a spotlight behind it, and jumps on the beat.

use image::RgbImage;

use crate::art::{Cell, Rendered, Rgb, mix};

/// The inks one square is printed in.
#[derive(Clone, Copy)]
pub struct Scheme {
    /// The page behind everything.
    pub paper: Rgb,
    /// The dots and rays printed over the page.
    pub dots: Rgb,
    /// Outlines, shadows and the caption box.
    pub ink: Rgb,
    /// The cover, printed in four tones from dark to light.
    pub tones: [Rgb; 4],
    /// Lettering on the ink.
    pub letter: Rgb,
}

/// One scheme per square: hot pink, yellow, cyan, orange.
pub const SCHEMES: [Scheme; 4] = [
    Scheme {
        paper: (255, 46, 136),
        dots: (255, 140, 196),
        ink: (24, 6, 46),
        tones: [(27, 6, 56), (0, 190, 255), (255, 214, 63), (255, 255, 255)],
        letter: (255, 226, 70),
    },
    Scheme {
        paper: (255, 222, 26),
        dots: (255, 168, 0),
        ink: (12, 20, 92),
        tones: [(10, 31, 122), (255, 46, 136), (46, 196, 255), (255, 248, 222)],
        letter: (255, 255, 255),
    },
    Scheme {
        paper: (25, 211, 255),
        dots: (140, 238, 255),
        ink: (20, 0, 52),
        tones: [(18, 0, 46), (123, 44, 255), (255, 61, 203), (236, 255, 249)],
        letter: (255, 226, 70),
    },
    Scheme {
        paper: (255, 107, 26),
        dots: (255, 170, 102),
        ink: (0, 26, 51),
        tones: [(0, 26, 51), (0, 199, 129), (255, 225, 26), (255, 255, 255)],
        letter: (255, 255, 255),
    },
];

/// How a square should look this instant.
#[derive(Clone, Copy, Default)]
pub struct Look {
    pub scheme: usize,
    /// How much of the square is being heard, 0..1: its cover goes from
    /// print to its own colours, and grows.
    pub heard: f32,
    /// 1.0 on a beat, falling back to 0. Only the heard square jumps.
    pub beat: f32,
    /// Low-end energy, 0..1: the dots swell with it.
    pub bass: f32,
    /// Free-running time in seconds.
    pub time: f32,
    /// Beats heard so far: the rays jump round a notch on each.
    pub beats: u64,
    /// The square is about to come in: it shakes, harder as the moment nears.
    pub coming: f32,
    /// The square is the one the arrows are on.
    pub picked: bool,
}

/// Where everything goes in a square. Pixels for what is drawn (two to a
/// cell, one above the other), cells for the words laid over it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layout {
    /// The cover's centre and half its side, in pixels.
    pub cx: f32,
    pub cy: f32,
    pub half: f32,
    /// Words beside the cover, or (in a narrow square) under it.
    pub beside: bool,
    /// The cells the words may use: from column `text_x`, `text_w` wide,
    /// rows `text_top` to `text_bottom` (exclusive).
    pub text_x: u16,
    pub text_w: u16,
    pub text_top: u16,
    pub text_bottom: u16,
    /// The square's big number: top-left corner in pixels and the size of
    /// one of its dots.
    pub numeral: Option<(f32, f32, f32)>,
}

/// Columns kept clear between the edge of a square and anything in it.
const MARGIN: f32 = 3.0;

/// Lay out a square `w` cells wide and `h` high. Beside the words, the cover
/// sits on the side towards the middle of the screen, so the four covers
/// gather together and the words go round the outside.
pub fn layout(w: u16, h: u16, look: &Look) -> Layout {
    let (wf, hf) = (w as f32, h as f32 * 2.0);
    // The cover as big as the height allows, with room round it to breathe
    // and for its shadow.
    let full = (hf * 0.78 - 2.0).min(wf * 0.55).max(2.0);
    let shadow = shadow_for(full * 0.5);
    let beside = wf - full - shadow - MARGIN * 3.0 >= 16.0 && h >= 6;
    // It grows when heard and jumps on the beat; the words stay put.
    let side = full * (0.9 + 0.1 * look.heard) * (1.0 + 0.06 * look.beat * look.heard);
    let bob = (look.time * 1.7 + look.scheme as f32 * 1.9).sin() * (1.0 - look.heard) * hf * 0.012;
    let shake = (look.time * 47.0).sin() * look.coming * wf * 0.01;
    if !beside {
        // Too narrow: the cover above, the words under it.
        // Clear of the title and artist underneath, and the frame.
        let full = (hf - 13.0).min(wf * 0.6).max(2.0);
        let side = full * (0.9 + 0.1 * look.heard) * (1.0 + 0.06 * look.beat * look.heard);
        let under = ((3.0 + full + shadow_for(full * 0.5) + 2.0) / 2.0).ceil() as u16;
        return Layout {
            cx: wf * 0.5 + shake,
            cy: 3.0 + full * 0.5 + bob,
            half: (side * 0.5).max(1.0),
            beside: false,
            text_x: 2,
            text_w: w.saturating_sub(4),
            text_top: under.min(h.saturating_sub(3)),
            text_bottom: h.saturating_sub(1),
            numeral: None,
        };
    }
    let inward_right = look.scheme % 2 == 0;
    let cx = if inward_right { wf - MARGIN - shadow - full * 0.5 } else { MARGIN + full * 0.5 };
    let (text_x, text_end) = if inward_right {
        (MARGIN, wf - MARGIN * 2.0 - shadow - full)
    } else {
        (MARGIN * 2.0 + full + shadow, wf - MARGIN)
    };
    // The number, five dots tall, filling a third of the height, in the
    // outer bottom corner.
    let dot = (hf * 0.3 / 5.0).floor().max(1.0);
    let number_x = if inward_right { text_x } else { text_end - dot * 3.5 };
    let number_y = hf - 4.0 - dot * 5.5;
    let room_above = (number_y / 2.0).floor() as u16;
    let numeral = (room_above >= 4).then_some((number_x, number_y, dot));
    Layout {
        cx: cx + shake,
        cy: hf * 0.5 - shadow * 0.4 + bob,
        half: (side * 0.5).max(1.0),
        beside: true,
        text_x: text_x as u16,
        text_w: (text_end - text_x).max(0.0) as u16,
        text_top: 1,
        text_bottom: if numeral.is_some() { room_above } else { h.saturating_sub(1) },
        numeral,
    }
}

/// How far a cover's shadow falls, for a cover `half` pixels from middle to edge.
fn shadow_for(half: f32) -> f32 {
    (half * 0.14).clamp(2.0, 5.0)
}

/// The digits 1 to 4, three dots wide and five tall, a row to a byte (high bit left).
const DIGITS: [[u8; 5]; 4] = [
    [0b110, 0b010, 0b010, 0b010, 0b111],
    [0b111, 0b001, 0b111, 0b100, 0b111],
    [0b111, 0b001, 0b011, 0b001, 0b111],
    [0b101, 0b101, 0b111, 0b001, 0b001],
];

/// Whether the dot of digit `n` (0 for 1) at column `x`, row `y` is inked.
fn digit_dot(n: usize, x: i32, y: i32) -> bool {
    (0..3).contains(&x) && (0..5).contains(&y) && DIGITS[n % 4][y as usize] >> (2 - x) & 1 == 1
}

/// The square as half-block cells, `w` by `h` of them. `cover` is drawn in
/// the middle when there is one.
pub fn square(cover: Option<&RgbImage>, w: u16, h: u16, look: Look) -> Rendered {
    let (pw, ph) = (w.max(1) as u32, h.max(1) as u32 * 2);
    let px = picture(cover, pw, ph, &look);
    let mut cells = Vec::with_capacity(w as usize * h as usize);
    for y in 0..h as u32 {
        for x in 0..pw {
            let top = px[(y * 2 * pw + x) as usize];
            let bottom = px[((y * 2 + 1) * pw + x) as usize];
            cells.push(Cell { ch: '▀', fg: top, bg: Some(bottom) });
        }
    }
    Rendered { w, h, cells }
}

/// The square as pixels, row by row: `w` by `h` of them, two to a cell.
pub fn picture(cover: Option<&RgbImage>, w: u32, h: u32, look: &Look) -> Vec<Rgb> {
    let s = &SCHEMES[look.scheme % SCHEMES.len()];
    let at = layout(w as u16, (h / 2) as u16, look);
    let shadow = shadow_for(at.half) * (1.0 + 0.4 * look.beat * look.heard);
    let outline = if at.half > 12.0 { 2.0 } else { 1.0 };
    let print = cover.map(|c| Print::new(c, s));
    let (wf, hf) = (w as f32, h as f32);

    // The corner nearest the middle of the screen: the halftone fades out
    // towards it and grows towards the outer edges.
    let inner = (if look.scheme % 2 == 0 { wf } else { 0.0 }, if look.scheme < 2 { hf } else { 0.0 });
    let reach = (wf * wf + hf * hf).sqrt();
    // A spotlight behind the cover being heard: rays that turn slowly and
    // kick round a notch on each beat.
    let rays = 16.0;
    let notch = std::f32::consts::TAU / rays * 0.5;
    let kick = (look.beats % 2) as f32 + (1.0 - look.beat).powi(3);
    let spin = look.time * 0.05 + notch * kick;
    let spot = look.heard;
    let dots = mix(s.paper, s.dots, 0.75);

    let mut out = vec![s.paper; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let (dx, dy) = (fx - at.cx, fy - at.cy);
            // Position on the cover's own square: -1..1 inside.
            let local = |ox: f32, oy: f32| ((dx - ox) / at.half, (dy - oy) / at.half);
            let inside = |(u, v): (f32, f32), grow: f32| u.abs() <= 1.0 + grow && v.abs() <= 1.0 + grow;
            let has_cover = cover.is_some();

            let (u, v) = local(0.0, 0.0);
            let colour = if has_cover && inside((u, v), 0.0) {
                print.as_ref().map_or(s.paper, |p| p.colour(u, v, look.heard, s))
            } else if has_cover && inside((u, v), outline / at.half) {
                s.ink
            } else if has_cover && inside(local(shadow, shadow), outline / at.half) {
                s.ink
            } else {
                let mut page = s.paper;
                if spot > 0.0 {
                    let angle = dy.atan2(dx) + spin;
                    if (angle / std::f32::consts::TAU * rays).rem_euclid(1.0) < 0.5 {
                        page = mix(page, s.letter, 0.3 * spot);
                    }
                }
                // A halftone screen: dots on a regular grid, every other row
                // shifted, none near the middle of the screen and growing a
                // size at a time towards the outer corner (and with the bass,
                // while heard). Whole pixels, so the screen stays crisp.
                let (gx, gy) = (x as i32, y as i32);
                let row = gy.div_euclid(4);
                let shift = if row % 2 == 0 { 0 } else { 2 };
                let (ox, oy) = ((gx + shift).rem_euclid(4), gy.rem_euclid(4));
                let far = ((fx - inner.0).powi(2) + (fy - inner.1).powi(2)).sqrt() / reach;
                let size = ((far * 1.5 - 0.35 + 0.15 * look.bass * spot) * 3.0).clamp(0.0, 3.0) as i32;
                let on = match size {
                    1 => ox == 1 && oy == 1,
                    2 => (1..=2).contains(&ox) && oy == 1,
                    3 => (1..=2).contains(&ox) && (1..=2).contains(&oy),
                    _ => false,
                };
                if on {
                    page = dots;
                }
                page
            };
            out[(y * w + x) as usize] = colour;
        }
    }

    // The big number, with a hard shadow like the cover's.
    if let Some((nx, ny, dot)) = at.numeral {
        let fill = if look.heard > 0.5 { s.letter } else { s.tones[3] };
        let n = look.scheme;
        let ink_at = |px: f32, py: f32| digit_dot(n, ((px - nx) / dot).floor() as i32, ((py - ny) / dot).floor() as i32);
        let fall = (dot * 0.5).max(1.0);
        for y in ny.max(0.0) as u32..((ny + dot * 5.0 + fall + 1.0) as u32).min(h) {
            for x in nx.max(0.0) as u32..((nx + dot * 3.0 + fall + 1.0) as u32).min(w) {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                if ink_at(px, py) {
                    out[(y * w + x) as usize] = fill;
                } else if ink_at(px - fall, py - fall) {
                    out[(y * w + x) as usize] = s.ink;
                }
            }
        }
    }

    // A frame round the square: ink, or for the one heard, light that
    // flashes on the beat; for one coming in, a blink on each half beat.
    let frame = if look.heard > 0.5 {
        Some(mix((255, 255, 255), s.letter, 0.5 - 0.5 * look.beat))
    } else if look.coming > 0.0 && (look.time * 4.0).fract() < 0.5 {
        Some(s.letter)
    } else if look.picked {
        Some(s.letter)
    } else {
        None
    };
    let thick = if look.heard > 0.5 || look.coming > 0.0 { 2 } else { 1 };
    for y in 0..h {
        for x in 0..w {
            let edge = x < thick || x + thick >= w || y < thick || y + thick >= h;
            // The outermost line stays ink, so neighbouring frames never run together.
            let rim = x == 0 || x + 1 == w || y == 0 || y + 1 == h;
            if edge {
                out[(y * w + x) as usize] = if rim { s.ink } else { frame.unwrap_or(s.ink) };
            }
        }
    }
    out
}

/// A cover ready to print: its colours, and how its lightness spreads, so
/// even a dark cover uses all four tones.
struct Print<'a> {
    img: &'a RgbImage,
    lo: f32,
    span: f32,
}

impl<'a> Print<'a> {
    fn new(img: &'a RgbImage, _s: &Scheme) -> Self {
        // Every eighth pixel each way is plenty to find the range.
        let (mut lo, mut hi) = (1f32, 0f32);
        for y in (0..img.height()).step_by(8) {
            for x in (0..img.width()).step_by(8) {
                let l = lightness(img.get_pixel(x, y).0);
                lo = lo.min(l);
                hi = hi.max(l);
            }
        }
        Self { img, lo, span: (hi - lo).max(0.1) }
    }

    /// The cover's colour at `u`, `v` (-1..1 across it): printed in the
    /// scheme's inks, or as itself made louder, by how much it is `heard`.
    fn colour(&self, u: f32, v: f32, heard: f32, s: &Scheme) -> Rgb {
        let (w, h) = (self.img.width(), self.img.height());
        let x = (((u + 1.0) * 0.5 * w as f32) as u32).min(w - 1);
        let y = (((v + 1.0) * 0.5 * h as f32) as u32).min(h - 1);
        let p = self.img.get_pixel(x, y).0;
        let l = ((lightness(p) - self.lo) / self.span).clamp(0.0, 1.0);
        let printed = s.tones[((l * 4.0) as usize).min(3)];
        if heard <= 0.0 {
            return printed;
        }
        mix(printed, loud(p), heard.min(1.0))
    }
}

fn lightness(p: [u8; 3]) -> f32 {
    (0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32) / 255.0
}

/// A colour pushed towards poster paint: more saturated, more contrast,
/// and in a few flat steps rather than smooth shading.
fn loud(p: [u8; 3]) -> Rgb {
    let grey = (p[0] as f32 + p[1] as f32 + p[2] as f32) / 3.0;
    let push = |c: u8| {
        let c = grey + (c as f32 - grey) * 1.6;
        let c = (c - 128.0) * 1.25 + 128.0;
        // Five flat steps per channel.
        ((c.clamp(0.0, 255.0) / 255.0 * 4.0).round() / 4.0 * 255.0) as u8
    };
    (push(p[0]), push(p[1]), push(p[2]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cover() -> RgbImage {
        RgbImage::from_fn(64, 64, |x, y| image::Rgb([(x * 4) as u8, (y * 4) as u8, 90]))
    }

    fn colours(r: &Rendered) -> std::collections::HashSet<Rgb> {
        r.cells.iter().flat_map(|c| [c.fg, c.bg.unwrap()]).collect()
    }

    #[test]
    fn a_square_fills_the_space_it_is_given() {
        for (w, h) in [(1, 1), (7, 3), (60, 18), (81, 27)] {
            let r = square(Some(&cover()), w, h, Look::default());
            assert_eq!(r.cells.len(), w as usize * h as usize);
        }
        assert_eq!(square(None, 30, 10, Look::default()).cells.len(), 300);
    }

    #[test]
    fn unheard_squares_print_in_their_own_inks() {
        for scheme in 0..4 {
            let r = square(Some(&cover()), 60, 20, Look { scheme, ..Default::default() });
            let s = &SCHEMES[scheme];
            let ok = |c: &Rgb| {
                s.tones.contains(c)
                    || [s.paper, s.dots, s.ink].contains(c)
                    // Rays, dots on rays, and the shadow are tints between the inks.
                    || [s.paper, s.ink].iter().any(|a| (0..=20).any(|i| mix(*a, s.dots, i as f32 / 20.0) == *c || mix(s.ink, *a, i as f32 / 20.0) == *c))
            };
            let strays: Vec<Rgb> = colours(&r).into_iter().filter(|c| !ok(c)).collect();
            assert!(strays.len() < 6, "scheme {scheme}: {strays:?}");
            // All four tones of the print show on a cover with any range to it.
            for tone in s.tones {
                assert!(colours(&r).contains(&tone), "scheme {scheme} is missing {tone:?}");
            }
        }
    }

    #[test]
    fn the_heard_square_shows_the_cover_and_jumps() {
        let still = Look { heard: 1.0, ..Default::default() };
        let r = square(Some(&cover()), 60, 20, still);
        // Its own colours come through: a red-and-green ramp has colours no ink has.
        let s = &SCHEMES[0];
        assert!(colours(&r).iter().any(|c| !s.tones.contains(c) && c.0 > 150 && c.1 < 100));
        // On the beat the cover is bigger.
        let hit = Look { beat: 1.0, ..still };
        assert!(layout(60, 20, &hit).half > layout(60, 20, &still).half);
        // And something heard is bigger than something waiting.
        assert!(layout(60, 20, &still).half > layout(60, 20, &Look::default()).half);
    }

    #[test]
    fn covers_face_the_middle_and_words_keep_clear_of_them() {
        for (w, h) in [(50u16, 14u16), (60, 17), (80, 22), (120, 30)] {
            for scheme in 0..4 {
                let look = Look { scheme, heard: 1.0, beat: 1.0, ..Default::default() };
                let at = layout(w, h, &look);
                assert!(at.beside, "{w}x{h} should have room beside the cover");
                let (left, right) = (at.cx - at.half, at.cx + at.half);
                // Towards the middle of the screen: the right of a left-hand square.
                if scheme % 2 == 0 {
                    assert!(at.cx > w as f32 / 2.0);
                } else {
                    assert!(at.cx < w as f32 / 2.0);
                }
                // On the beat at its biggest, still inside the square, and clear of the words.
                assert!(left >= 1.0 && right <= w as f32 - 1.0, "{w}x{h} #{scheme}: cover {left}..{right}");
                assert!(at.cy - at.half >= 1.0 && at.cy + at.half <= h as f32 * 2.0 - 1.0, "{w}x{h} #{scheme}: cover off the top or bottom");
                let (tx, te) = (at.text_x as f32, (at.text_x + at.text_w) as f32);
                assert!(te <= left || tx >= right, "{w}x{h} #{scheme}: words {tx}..{te} over the cover {left}..{right}");
                assert!(at.text_w >= 14);
                if let Some((nx, ny, dot)) = at.numeral {
                    assert!(nx >= 1.0 && nx + dot * 4.0 <= w as f32 && ny >= 1.0 && ny + dot * 6.0 <= h as f32 * 2.0, "{w}x{h} #{scheme}: number off the square");
                    assert!((ny / 2.0) as u16 >= at.text_bottom, "the words run into the number");
                }
            }
        }
        // Too narrow for words beside: the cover above them.
        let at = layout(30, 14, &Look::default());
        assert!(!at.beside);
        assert!(at.cy + at.half <= at.text_top as f32 * 2.0, "the words run into the cover");
    }

    #[test]
    fn the_frame_lights_only_for_the_heard_square() {
        // The outermost line is always ink, so frames never run together;
        // inside it, the heard square's frame is lit.
        let at = |look: Look| {
            let r = square(Some(&cover()), 40, 12, look);
            (r.cells[0].fg, r.cells[1].bg.unwrap())
        };
        let light = |c: Rgb| c.0 as u32 + c.1 as u32 + c.2 as u32 > 600;
        let (rim, inside) = at(Look { heard: 1.0, ..Default::default() });
        assert_eq!(rim, SCHEMES[0].ink);
        assert!(light(inside), "{inside:?}");
        let (rim, inside) = at(Look::default());
        assert_eq!(rim, SCHEMES[0].ink);
        assert!(!light(inside), "{inside:?}");
    }
}
