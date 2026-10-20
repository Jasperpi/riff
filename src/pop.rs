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
    /// Keep the cover where it is: no bobbing, no shaking. For a real
    /// picture, which is only sent again when something really changes.
    pub still: bool,
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
    let moves = if look.still { 0.0 } else { 1.0 };
    let bob = (look.time * 1.7 + look.scheme as f32 * 1.9).sin() * (1.0 - look.heard) * hf * 0.012 * moves;
    let shake = (look.time * 47.0).sin() * look.coming * wf * 0.01 * moves;
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
    // The words keep clear of the cover at its biggest, outline, shadow and all.
    let (left, right) = cover_reach(cx, full * 0.5);
    let (text_x, text_end) = if inward_right { (MARGIN, left.floor() - 2.0) } else { (right.ceil() + 2.0, wf - MARGIN) };
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

/// The furthest left and right a cover centred at `cx`, `full_half` pixels
/// from middle to edge when heard, ever reaches: on the beat, with its
/// outline and its longest shadow.
fn cover_reach(cx: f32, full_half: f32) -> (f32, f32) {
    let half = full_half * 1.06;
    let ring = if half > 12.0 { 2.0 } else { 1.0 };
    (cx - half - ring, cx + half + ring + shadow_for(half) * 1.4)
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
    let painter = Painter::new(cover, w as u16, (h / 2) as u16, *look);
    let mut out = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            out.push(painter.paint(x as f32 + 0.5, y as f32 + 0.5, false));
        }
    }
    out
}

/// Everything about a square worked out once, to paint it at any point:
/// on the grid of half-block pixels, or finer, for a real picture.
pub struct Painter<'a> {
    s: &'static Scheme,
    look: Look,
    at: Layout,
    shadow: f32,
    outline: f32,
    print: Option<Print<'a>>,
    w: f32,
    h: f32,
    /// The corner nearest the middle of the screen: the halftone fades out
    /// towards it and grows towards the outer edges.
    inner: (f32, f32),
    reach: f32,
    spin: f32,
    dots: Rgb,
    frame: Option<Rgb>,
    thick: f32,
}

impl<'a> Painter<'a> {
    /// The square of `w` by `h` cells. A finer picture of the cover should
    /// be given when there is one: only the cover is drawn finer.
    pub fn new(cover: Option<&'a RgbImage>, w: u16, h: u16, look: Look) -> Self {
        let s = &SCHEMES[look.scheme % SCHEMES.len()];
        let at = layout(w, h, &look);
        let (wf, hf) = (w as f32, h as f32 * 2.0);
        // A spotlight behind the cover being heard: rays that turn slowly and
        // kick round a notch on each beat.
        let notch = std::f32::consts::TAU / RAYS * 0.5;
        let kick = (look.beats % 2) as f32 + (1.0 - look.beat).powi(3);
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
        Self {
            s,
            look,
            at,
            shadow: shadow_for(at.half) * (1.0 + 0.4 * look.beat * look.heard),
            outline: if at.half > 12.0 { 2.0 } else { 1.0 },
            print: cover.map(Print::new),
            w: wf,
            h: hf,
            inner: (if look.scheme % 2 == 0 { wf } else { 0.0 }, if look.scheme < 2 { hf } else { 0.0 }),
            reach: (wf * wf + hf * hf).sqrt(),
            spin: look.time * 0.05 + notch * kick,
            dots: mix(s.paper, s.dots, 0.75),
            frame,
            thick: if look.heard > 0.5 || look.coming > 0.0 { 2.0 } else { 1.0 },
        }
    }

    /// The colour at `x`, `y`, in half-block pixels from the square's top
    /// left. The frame, the number and the page behind are drawn in whole
    /// pixels wherever they are asked for, so a fine picture matches the
    /// half-blocks round it exactly; the cover, its outline and its shadow
    /// are drawn as finely as they are asked for (`fine` also reads the
    /// cover between its pixels).
    pub fn paint(&self, x: f32, y: f32, fine: bool) -> Rgb {
        let (s, look, at) = (self.s, &self.look, &self.at);
        let (px, py) = (x.floor(), y.floor());
        // The frame: the outermost line always ink, so neighbouring frames
        // never run together.
        let edge = px < self.thick || px + self.thick >= self.w || py < self.thick || py + self.thick >= self.h;
        if edge {
            let rim = px < 1.0 || px + 1.0 >= self.w || py < 1.0 || py + 1.0 >= self.h;
            return if rim { s.ink } else { self.frame.unwrap_or(s.ink) };
        }
        // The big number, with a hard shadow like the cover's.
        if let Some((nx, ny, dot)) = at.numeral {
            let (cx, cy) = (px + 0.5, py + 0.5);
            let ink_at = |qx: f32, qy: f32| digit_dot(look.scheme, ((qx - nx) / dot).floor() as i32, ((qy - ny) / dot).floor() as i32);
            let fall = (dot * 0.5).max(1.0);
            if ink_at(cx, cy) {
                return if look.heard > 0.5 { s.letter } else { s.tones[3] };
            }
            if ink_at(cx - fall, cy - fall) {
                return s.ink;
            }
        }
        // The cover, its outline and its shadow.
        let (fx, fy) = if fine { (x, y) } else { (px + 0.5, py + 0.5) };
        if let Some(print) = &self.print {
            let (dx, dy) = (fx - at.cx, fy - at.cy);
            let (u, v) = (dx / at.half, dy / at.half);
            let ring = self.outline / at.half;
            if u.abs() <= 1.0 && v.abs() <= 1.0 {
                return print.colour(u, v, look.heard, s, fine);
            }
            let (su, sv) = ((dx - self.shadow) / at.half, (dy - self.shadow) / at.half);
            let within = |u: f32, v: f32| u.abs() <= 1.0 + ring && v.abs() <= 1.0 + ring;
            if within(u, v) || within(su, sv) {
                return s.ink;
            }
        }
        self.page(px, py)
    }

    /// The page behind everything at whole pixel `px`, `py`.
    fn page(&self, px: f32, py: f32) -> Rgb {
        let (s, look, at) = (self.s, &self.look, &self.at);
        let (cx, cy) = (px + 0.5, py + 0.5);
        let mut page = s.paper;
        let spot = look.heard;
        if spot > 0.0 {
            let angle = (cy - at.cy).atan2(cx - at.cx) + self.spin;
            if (angle / std::f32::consts::TAU * RAYS).rem_euclid(1.0) < 0.5 {
                page = mix(page, s.letter, 0.3 * spot);
            }
        }
        // A halftone screen: dots on a regular grid, every other row
        // shifted, none near the middle of the screen and growing a size at
        // a time towards the outer corner (and with the bass, while heard).
        // Whole pixels, so the screen stays crisp.
        let (gx, gy) = (px as i32, py as i32);
        let shift = if gy.div_euclid(4) % 2 == 0 { 0 } else { 2 };
        let (ox, oy) = ((gx + shift).rem_euclid(4), gy.rem_euclid(4));
        let far = ((cx - self.inner.0).powi(2) + (cy - self.inner.1).powi(2)).sqrt() / self.reach;
        let size = ((far * 1.5 - 0.35 + 0.15 * look.bass * spot) * 3.0).clamp(0.0, 3.0) as i32;
        let on = match size {
            1 => ox == 1 && oy == 1,
            2 => (1..=2).contains(&ox) && oy == 1,
            3 => (1..=2).contains(&ox) && (1..=2).contains(&oy),
            _ => false,
        };
        if on { self.dots } else { page }
    }
}

/// Rays in the spotlight behind the cover being heard.
const RAYS: f32 = 16.0;

/// The cells a square's cover takes up, with its outline and its shadow, at
/// the most it ever grows (when heard, on the beat): where a real picture of
/// it goes. Relative to the square: column, row, width, height.
pub fn cover_cells(w: u16, h: u16, look: &Look) -> (u16, u16, u16, u16) {
    let biggest = Look { heard: 1.0, beat: 1.0, ..*look };
    let at = layout(w, h, &biggest);
    let reach = at.half + if at.half > 12.0 { 2.0 } else { 1.0 };
    let shadow = shadow_for(at.half) * 1.4;
    let (left, right) = ((at.cx - reach).floor().max(1.0), (at.cx + reach + shadow).ceil().min(w as f32 - 1.0));
    let (top, bottom) = (((at.cy - reach) / 2.0).floor().max(1.0), ((at.cy + reach + shadow) / 2.0).ceil().min(h as f32 - 1.0));
    (left as u16, top as u16, (right - left).max(0.0) as u16, (bottom - top).max(0.0) as u16)
}

/// The disc where the squares meet, for a real picture of it.
#[derive(Clone, Copy, Debug)]
pub struct Hub {
    /// Its middle in cells: columns across, rows down.
    pub centre: (f32, f32),
    /// Its radius in half-block pixels (a column wide, half a row tall).
    pub radius: f32,
    /// The square the music is heading for, to point at.
    pub to: Option<usize>,
    pub beat: f32,
}

impl Hub {
    pub const INK: Rgb = (14, 10, 24);

    pub fn ring(&self) -> Rgb {
        mix((255, 255, 255), (255, 226, 70), self.beat)
    }

    /// Its colour `dx`, `dy` half-block pixels from its middle, if it covers that point.
    fn paint(&self, dx: f32, dy: f32) -> Option<Rgb> {
        let r = (dx * dx + dy * dy).sqrt();
        let (outer, band) = (self.radius, 1.5);
        if r > outer + 1.0 {
            return None;
        }
        if r > outer || r > outer - band {
            return Some(if r > outer { Self::INK } else { self.ring() });
        }
        // Inside: an arrow to the square coming in, or a dot that pulses.
        let inside = match self.to {
            Some(to) => {
                let dir = [(-1.0f32, -1.0f32), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)][to % 4];
                let (ux, uy) = (dir.0 * std::f32::consts::FRAC_1_SQRT_2, dir.1 * std::f32::consts::FRAC_1_SQRT_2);
                let along = dx * ux + dy * uy;
                let across = (dx * uy - dy * ux).abs();
                let size = outer - band;
                let head = (0.0..size * 0.6).contains(&along) && across <= (size * 0.6 - along) * 0.75;
                let shaft = (-size * 0.55..0.05).contains(&along) && across <= size * 0.14;
                head || shaft
            }
            None => r <= (outer - band) * (0.25 + 0.15 * self.beat),
        };
        Some(if inside { self.ring() } else { Self::INK })
    }
}

/// A real picture of part of the four squares: the cells `cells`, at `w` by
/// `h` pixels of RGB, with the disc where they meet over them if given.
/// Each point is painted by whichever square it falls in.
pub fn scene(squares: &[(ratatui::layout::Rect, Painter)], hub: Option<&Hub>, cells: ratatui::layout::Rect, w: u32, h: u32) -> Vec<u8> {
    let mut out = vec![0u8; (w * h * 3) as usize];
    let paint_row = |y: usize, line: &mut [u8]| {
        let row = cells.y as f32 + (y as f32 + 0.5) / h as f32 * cells.height as f32;
        for (x, pixel) in line.chunks_exact_mut(3).enumerate() {
            let col = cells.x as f32 + (x as f32 + 0.5) / w as f32 * cells.width as f32;
            let disc = hub.and_then(|hub| hub.paint(col - hub.centre.0, (row - hub.centre.1) * 2.0));
            let colour = disc.or_else(|| {
                squares
                    .iter()
                    .find(|(r, _)| col >= r.x as f32 && col < r.right() as f32 && row >= r.y as f32 && row < r.bottom() as f32)
                    .map(|(r, painter)| painter.paint(col - r.x as f32, (row - r.y as f32) * 2.0, true))
            });
            let c = colour.unwrap_or(Hub::INK);
            pixel.copy_from_slice(&[c.0, c.1, c.2]);
        }
    };
    // Rows dealt out between a few threads in turn.
    let threads = std::thread::available_parallelism().map_or(2, |n| n.get()).clamp(1, 4);
    let mut hands: Vec<Vec<(usize, &mut [u8])>> = (0..threads).map(|_| Vec::new()).collect();
    for (y, line) in out.chunks_mut((w * 3) as usize).enumerate() {
        hands[y % threads].push((y, line));
    }
    std::thread::scope(|scope| {
        for hand in hands {
            let paint_row = &paint_row;
            scope.spawn(move || {
                for (y, line) in hand {
                    paint_row(y, line);
                }
            });
        }
    });
    out
}

/// What a square's picture depends on, in steps coarse enough that it is
/// sent again only when the change would show.
pub fn look_key(look: &Look) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let step = |v: f32, n: f32| (v * n).round() as i32;
    look.scheme.hash(&mut h);
    step(look.heard, 10.0).hash(&mut h);
    look.picked.hash(&mut h);
    (look.coming > 0.0 && (look.time * 4.0).fract() < 0.5).hash(&mut h);
    (look.coming > 0.0).hash(&mut h);
    if look.heard > 0.0 {
        // The one heard jumps on the beat, and its spotlight turns.
        step(look.beat * look.heard, 6.0).hash(&mut h);
        step(look.bass * look.heard, 4.0).hash(&mut h);
        (look.beats % 2).hash(&mut h);
        step(look.time, 8.0).hash(&mut h);
    }
    h.finish()
}

/// A cover ready to print: its colours, and how its lightness spreads, so
/// even a dark cover uses all four tones.
struct Print<'a> {
    img: &'a RgbImage,
    lo: f32,
    span: f32,
}

impl<'a> Print<'a> {
    fn new(img: &'a RgbImage) -> Self {
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
    /// `fine` reads between the cover's pixels, for a real picture.
    fn colour(&self, u: f32, v: f32, heard: f32, s: &Scheme, fine: bool) -> Rgb {
        let (w, h) = (self.img.width(), self.img.height());
        let p = if fine {
            let x = ((u + 1.0) * 0.5 * w as f32 - 0.5).clamp(0.0, w as f32 - 1.0);
            let y = ((v + 1.0) * 0.5 * h as f32 - 0.5).clamp(0.0, h as f32 - 1.0);
            let (x0, y0) = (x as u32, y as u32);
            let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
            let (tx, ty) = (x - x0 as f32, y - y0 as f32);
            let (a, b, c, d) = (self.img.get_pixel(x0, y0).0, self.img.get_pixel(x1, y0).0, self.img.get_pixel(x0, y1).0, self.img.get_pixel(x1, y1).0);
            std::array::from_fn(|i| {
                let top = a[i] as f32 + (b[i] as f32 - a[i] as f32) * tx;
                let bottom = c[i] as f32 + (d[i] as f32 - c[i] as f32) * tx;
                (top + (bottom - top) * ty) as u8
            })
        } else {
            let x = (((u + 1.0) * 0.5 * w as f32) as u32).min(w - 1);
            let y = (((v + 1.0) * 0.5 * h as f32) as u32).min(h - 1);
            self.img.get_pixel(x, y).0
        };
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
                // Nor under the real picture of the cover, which allows for it at its biggest.
                let (col, _, width, _) = cover_cells(w, h, &look);
                let (pl, pr) = (col, col + width);
                assert!(at.text_x + at.text_w <= pl || at.text_x >= pr, "{w}x{h} #{scheme}: words {tx}..{te} under the picture {pl}..{pr}");
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
    fn a_fine_picture_matches_the_cells_round_it() {
        // Outside the cover's own box, painting finely gives just what the
        // half-block pixel there shows: the edge of a picture never shows.
        for look in [Look::default(), Look { scheme: 1, heard: 1.0, beat: 0.6, bass: 0.5, time: 3.0, beats: 5, ..Default::default() }] {
            let (w, h) = (60u16, 17u16);
            let img = cover();
            let painter = Painter::new(Some(&img), w, h, look);
            let (col, row, width, height) = cover_cells(w, h, &look);
            for py in 0..h as u32 * 2 {
                for px in 0..w as u32 {
                    let in_box = (col as u32..(col + width) as u32).contains(&px) && (row as u32 * 2..(row + height) as u32 * 2).contains(&py);
                    if in_box {
                        continue;
                    }
                    let coarse = painter.paint(px as f32 + 0.5, py as f32 + 0.5, false);
                    for (sx, sy) in [(0.05, 0.05), (0.95, 0.1), (0.3, 0.9), (0.99, 0.99)] {
                        let fine = painter.paint(px as f32 + sx, py as f32 + sy, true);
                        assert_eq!(fine, coarse, "at {px}+{sx}, {py}+{sy}");
                    }
                }
            }
        }
    }

    #[test]
    fn pictures_are_sent_again_only_when_they_change() {
        let idle = Look { scheme: 2, time: 1.0, bass: 0.7, beat: 0.9, ..Default::default() };
        // A square not being heard looks the same whatever the music does.
        assert_eq!(look_key(&idle), look_key(&Look { time: 9.3, bass: 0.1, beat: 0.0, beats: 7, ..idle }));
        // Being picked, or coming in (and its blink), shows.
        assert_ne!(look_key(&idle), look_key(&Look { picked: true, ..idle }));
        assert_ne!(look_key(&Look { coming: 0.5, time: 1.0, ..idle }), look_key(&Look { coming: 0.5, time: 1.15, ..idle }));
        // The one heard changes with the beat, but not with every twitch of it.
        let heard = Look { heard: 1.0, ..idle };
        assert_ne!(look_key(&heard), look_key(&Look { beat: 0.2, ..heard }));
        assert_eq!(look_key(&heard), look_key(&Look { beat: 0.91, ..heard }));
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

#[cfg(test)]
mod bench {
    use super::*;

    #[test]
    #[ignore]
    fn a_cover_picture_takes() {
        let img = RgbImage::from_fn(640, 640, |x, y| image::Rgb([(x / 3) as u8, (y / 3) as u8, 90]));
        let look = Look { heard: 1.0, beat: 0.5, still: true, ..Default::default() };
        let rect = ratatui::layout::Rect::new(0, 0, 80, 22);
        let painters = vec![(rect, Painter::new(Some(&img), 80, 22, look))];
        let (col, row, w, h) = cover_cells(80, 22, &look);
        let cells = ratatui::layout::Rect::new(col, row, w, h);
        let started = std::time::Instant::now();
        for _ in 0..20 {
            std::hint::black_box(scene(&painters, None, cells, w as u32 * 9, h as u32 * 18));
        }
        eprintln!("{}x{} cells at 9x18: {:.1} ms each", w, h, started.elapsed().as_secs_f32() * 1000.0 / 20.0);
    }
}
