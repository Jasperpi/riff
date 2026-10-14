//! Pop art for the four-square mode. Each square is printed like a poster
//! in its own loud colours: a sunburst and Ben-Day dots behind, the cover
//! in front with a heavy outline and a hard shadow so it stands off the
//! page. The cover is screen-printed in the square's inks until it is the
//! one being heard, when it comes up in its own colours, pumped up, and
//! jumps on the beat.

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

/// Where the cover went, in pixels of the picture: centre and half its side.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placed {
    pub cx: f32,
    pub cy: f32,
    pub half: f32,
}

/// Lay out a square `w` pixels wide and `h` high: where its cover goes.
pub fn place(w: u32, h: u32, look: &Look) -> Placed {
    let (w, h) = (w as f32, h as f32);
    // From just under the top edge to clear of the title, artist and
    // progress lines underneath (four and a half rows), with room for the
    // shadow and for the jump on the beat.
    let (top, bottom) = (2.0, (h - 9.0).max(3.0));
    let room = ((bottom - top) / 1.3).min(w * 0.56);
    let side = room * (0.84 + 0.16 * look.heard) * (1.0 + 0.09 * look.beat * look.heard);
    let bob = (look.time * 1.7 + look.scheme as f32 * 1.9).sin() * (1.0 - look.heard) * h * 0.02;
    let shake = (look.time * 47.0).sin() * look.coming * w * 0.012;
    Placed { cx: w * 0.5 + shake, cy: top + (bottom - top) * 0.45 + bob, half: (side * 0.5).max(1.0) }
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

/// The square as pixels, row by row.
pub fn picture(cover: Option<&RgbImage>, w: u32, h: u32, look: &Look) -> Vec<Rgb> {
    let s = &SCHEMES[look.scheme % SCHEMES.len()];
    let at = place(w, h, look);
    let shadow = (at.half * 0.2).max(2.0) * (1.0 + 0.5 * look.beat * look.heard);
    let outline = if at.half > 14.0 { 2.0 } else { 1.0 };
    // The rays turn slowly, and for the square being heard they also kick
    // round a notch on every beat.
    let rays = 14.0;
    let notch = std::f32::consts::TAU / rays * 0.5;
    let kick = (look.beats % 2) as f32 + (1.0 - look.beat).powi(3);
    let spin = look.time * 0.06 + look.scheme as f32 + look.heard * notch * kick;
    let print = cover.map(|c| Print::new(c, s));

    let mut out = vec![s.paper; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let (dx, dy) = (fx - at.cx, fy - at.cy);
            // Position on the cover's own square: -1..1 inside.
            let local = |ox: f32, oy: f32| ((dx - ox) / at.half, (dy - oy) / at.half);
            let inside = |(u, v): (f32, f32), grow: f32| u.abs() <= 1.0 + grow && v.abs() <= 1.0 + grow;

            let (u, v) = local(0.0, 0.0);
            let colour = if cover.is_some() && inside((u, v), 0.0) {
                print.as_ref().map_or(s.paper, |p| p.colour(u, v, look.heard, s))
            } else if cover.is_some() && inside((u, v), outline / at.half) {
                s.ink
            } else if cover.is_some() && inside(local(shadow, shadow), outline / at.half) {
                mix(s.ink, s.paper, 0.12)
            } else {
                // The page: a sunburst from behind the cover, and dots over it.
                let angle = dy.atan2(dx) + spin;
                let ray = ((angle / std::f32::consts::TAU * rays).rem_euclid(1.0) < 0.5) as u8 as f32;
                let mut page = mix(s.paper, s.dots, ray * (0.3 + 0.35 * look.heard));
                // Ben-Day dots, in rows offset from each other, swelling with the bass.
                let spacing = 7.0;
                let row = (fy / spacing).floor();
                let shift = if row as i32 % 2 == 0 { 0.0 } else { spacing / 2.0 };
                let (gx, gy) = ((fx + shift).rem_euclid(spacing) - spacing / 2.0, fy.rem_euclid(spacing) - spacing / 2.0);
                let radius = 1.75 + 0.6 * look.bass * look.heard;
                if gx * gx + gy * gy <= radius * radius {
                    page = s.dots;
                }
                page
            };
            out[(y * w + x) as usize] = colour;
        }
    }

    // A frame round the square: ink, or for the one heard, light that flashes on the beat.
    let frame = if look.heard > 0.5 {
        Some(mix((255, 255, 255), s.letter, 0.5 - 0.5 * look.beat))
    } else if look.coming > 0.0 && (look.time * 6.0).fract() < 0.5 {
        Some(s.letter)
    } else if look.picked {
        Some(s.letter)
    } else {
        None
    };
    for y in 0..h {
        for x in 0..w {
            if x == 0 || x + 1 == w || y < 1 || y + 1 >= h {
                out[(y * w + x) as usize] = frame.unwrap_or(s.ink);
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
        assert!(place(60, 40, &hit).half > place(60, 40, &still).half);
        // And something heard is bigger than something waiting.
        assert!(place(60, 40, &still).half > place(60, 40, &Look::default()).half);
    }

    #[test]
    fn the_frame_lights_only_for_the_heard_square() {
        let edge = |look: Look| square(Some(&cover()), 40, 12, look).cells[0].fg;
        assert_eq!(edge(Look::default()), SCHEMES[0].ink);
        assert_ne!(edge(Look { heard: 1.0, ..Default::default() }), SCHEMES[0].ink);
    }
}
