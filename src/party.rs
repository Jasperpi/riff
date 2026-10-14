//! Party mode: a pass over each finished frame that makes all of it move
//! with the music.
//!
//! Behind everything runs one of six scenes, and every four bars it gives
//! way to the next, fading across a beat: a spectrum closing in from the
//! edges, a tunnel of rings that leap outward on the beat, a slow plasma,
//! laser beams sweeping up from the floor, rain of glyphs falling at the
//! speed of their part of the spectrum, and stars streaking past.
//!
//! It also listens for the drop: the music coming back hard after a lull.
//! Then the screen flashes once, shakes, a few rows glitch sideways, and
//! the scene jumps to a hotter one. Colours wheel round and jump on each
//! beat throughout, rows of text sway, and cover art gets a frame of light.
//! Nothing strobes: the one flash is for a drop, and fades.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

use crate::art::{self, Rgb};
use crate::audio::BANDS;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Scene {
    /// The spectrum, reaching in from all four edges.
    #[default]
    Edges,
    /// Rings rushing outward from the middle, a leap on each beat.
    Tunnel,
    /// Slow interference of waves, warping with the bass.
    Plasma,
    /// Beams sweeping up from the floor.
    Lasers,
    /// Glyphs falling down every column, each at its band's pace.
    Rain,
    /// Stars streaking out from the middle, faster with the bass.
    Warp,
}

impl Scene {
    const ORDER: [Scene; 6] = [Scene::Edges, Scene::Tunnel, Scene::Plasma, Scene::Lasers, Scene::Rain, Scene::Warp];

    fn next(self) -> Self {
        let i = Self::ORDER.iter().position(|s| *s == self).unwrap_or(0);
        Self::ORDER[(i + 1) % Self::ORDER.len()]
    }
}

/// What party mode remembers from one frame to the next.
#[derive(Clone, Debug, Default)]
pub struct Rave {
    pub scene: Scene,
    /// The scene being left, and how far the change has got (1 when done).
    pub last: Scene,
    pub change: f32,
    /// Loudness, followed quickly and slowly. A drop is the quick one coming
    /// back well above the slow one after a while below it.
    fast: f32,
    slow: f32,
    /// Seconds the music has spent in a lull.
    lull: f32,
    /// 1.0 at a drop, falling away over a second or so.
    pub drop: f32,
    pub drops: u32,
    /// The beat a scene last changed on.
    since: u64,
}

impl Rave {
    /// Move on by `dt` seconds. `level` is how loud the music is, 0..1;
    /// `beat` whether a beat just landed; `beats` how many there have been.
    pub fn tick(&mut self, dt: f32, level: f32, beat: bool, beats: u64) {
        self.fast += (level - self.fast) * (dt * 7.0).min(1.0);
        self.slow += (level - self.slow) * (dt * 0.25).min(1.0);
        if self.fast < self.slow * 0.7 {
            self.lull += dt;
        } else if self.fast > self.slow * 0.9 {
            self.lull = (self.lull - dt * 0.5).max(0.0);
        }
        self.drop = (self.drop - dt * 0.8).max(0.0);
        self.change = (self.change + dt * 2.0).min(1.0);
        if !beat {
            return;
        }
        if self.lull > 1.5 && self.fast > self.slow * 1.2 && self.fast > 0.15 {
            // The drop: straight to something hot.
            self.drop = 1.0;
            self.drops += 1;
            self.lull = 0.0;
            let hot = if self.scene == Scene::Warp { Scene::Tunnel } else { Scene::Warp };
            self.go(hot, beats);
        } else if beats >= self.since + 16 && beats % 16 == 0 {
            // A new phrase, a new scene.
            self.go(self.scene.next(), beats);
        }
    }

    fn go(&mut self, scene: Scene, beats: u64) {
        self.last = self.scene;
        self.scene = scene;
        self.change = 0.0;
        self.since = beats;
    }
}

/// Everything the scenes draw from, for one frame.
pub struct Music<'a> {
    pub bars: &'a [f32; BANDS],
    /// 1.0 at a beat, falling to 0.
    pub beat: f32,
    pub beats: u64,
    pub bass: f32,
    /// Animation time, running faster when the music is louder.
    pub clock: f32,
}

impl Music<'_> {
    /// The spectrum at `t`, 0 for the lowest band and 1 for the highest.
    fn band(&self, t: f32) -> f32 {
        self.bars[((t.clamp(0.0, 1.0) * (BANDS - 1) as f32) as usize).min(BANDS - 1)]
    }
}

/// What a scene puts at one cell: a shift of hue, extra light, and maybe a
/// glyph for an empty cell.
#[derive(Clone, Copy, Default)]
struct Glow {
    hue: f32,
    light: f32,
    glyph: Option<char>,
}

fn hash(a: u32, b: u32) -> f32 {
    let mut h = a.wrapping_mul(0x9E37_79B9) ^ b.wrapping_mul(0x85EB_CA6B);
    h ^= h >> 16;
    h = h.wrapping_mul(0x7FEB_352D);
    h ^= h >> 15;
    h = h.wrapping_mul(0x846C_A68B);
    h ^= h >> 16;
    (h >> 8) as f32 / 16_777_216.0
}

/// Characters that look like falling code, and that every monospace font has.
const RAIN: &[char] = &['0', '1', '2', '5', '7', '9', 'Z', 'Ξ', 'Σ', 'Λ', 'Δ', 'Π', 'Ω', 'λ', 'ψ', '+', '*', '=', ':', '¦', '╎'];

/// Cell `(cx, cy)` of a `w` by `h` screen, in `scene`.
fn glow(scene: Scene, cx: usize, cy: usize, w: usize, h: usize, m: &Music) -> Glow {
    let (x, y) = ((cx as f32 + 0.5) / w as f32, (cy as f32 + 0.5) / h as f32);
    // Cells are about twice as tall as they are wide: distances in widths.
    let aspect = w as f32 / h as f32 / 2.0;
    let (dx, dy) = ((x - 0.5) * aspect, y - 0.5);
    let tau = std::f32::consts::TAU;
    match scene {
        Scene::Edges => {
            // Bars reach in from all four edges: lows in the middle of each
            // edge, highs towards the corners. `across` and `down` run from 0
            // on the centre line to 1 at the edge. The side bars stop sooner,
            // so as to look as deep as the top and bottom ones.
            const REACH_TALL: f32 = 0.9;
            const REACH_WIDE: f32 = 0.5;
            let across = (x - 0.5).abs() * 2.0;
            let down = (y - 0.5).abs() * 2.0;
            let under = (m.band(across) * REACH_TALL - (1.0 - down)).max(m.band(down) * REACH_WIDE - (1.0 - across));
            let light = if under > 0.0 { 0.07 + 0.11 * (1.0 - under.min(1.0)) } else { 0.0 };
            Glow { hue: across.max(down) * 110.0 + across.min(down) * 50.0, light, glyph: None }
        }
        Scene::Tunnel => {
            // Rectangles shaped like the screen, rushing outward, leaping on each beat.
            let r = ((x - 0.5).abs() * 2.0).max((y - 0.5).abs() * 2.0);
            let phase = r * 6.0 - m.clock * 1.4 - m.beats as f32 * 0.5 - (1.0 - m.beat) * 0.5;
            let ring = phase.rem_euclid(1.0);
            let lit = if ring < 0.38 { 1.0 - ring / 0.38 } else { 0.0 };
            Glow { hue: phase.floor() * 47.0, light: lit * (0.06 + 0.13 * r) + m.band(r) * 0.05, glyph: None }
        }
        Scene::Plasma => {
            let t = m.clock * 0.9;
            let r = (dx * dx + dy * dy).sqrt();
            let v = (dx * 9.0 + t).sin()
                + (dy * 7.0 - t * 1.3).sin()
                + ((dx + dy) * 6.0 + t * 0.7).sin()
                + (r * (12.0 + 6.0 * m.bass) - t * 2.0).sin();
            let v = v / 8.0 + 0.5;
            Glow { hue: v * 160.0, light: 0.02 + 0.16 * v * v + 0.05 * m.beat * v, glyph: None }
        }
        Scene::Lasers => {
            // From two points on the floor, beams sweeping back and forth.
            let mut best = Glow::default();
            for k in 0..6 {
                let from = if k % 2 == 0 { 0.25 } else { 0.75 };
                let (bx, by) = ((x - from) * aspect, y - 1.05);
                let angle = by.atan2(bx);
                let aim = -tau / 4.0 + (m.clock * 0.6 + k as f32 * 1.7).sin() * 0.85;
                let off = (angle - aim + tau / 2.0).rem_euclid(tau) - tau / 2.0;
                let width = 0.018 + 0.02 * m.beat;
                let lit = (-(off / width).powi(2)).exp() * (0.12 + 0.12 * m.beat);
                if lit > best.light {
                    best = Glow { hue: k as f32 * 60.0, light: lit, glyph: None };
                }
            }
            // A little smoke for them to cut through.
            best.light += 0.03 * hash(cx as u32 / 3, cy as u32 + (m.clock * 2.0) as u32);
            best
        }
        Scene::Rain => {
            // Each column falls at the pace of its part of the spectrum; about
            // half the columns are falling at any time.
            let band = m.band(x);
            if hash(cx as u32, 5) > 0.55 {
                return Glow { hue: x * 120.0, light: 0.0, glyph: None };
            }
            let speed = 0.25 + 0.9 * band;
            let head = (hash(cx as u32, 7) + m.clock * speed * 0.25).rem_euclid(1.0) * 1.5 - 0.25;
            let behind = head - y;
            let trail = 0.3 + 0.2 * band;
            if !(0.0..trail).contains(&behind) {
                return Glow { hue: x * 120.0, light: 0.0, glyph: None };
            }
            let fade = 1.0 - behind / trail;
            let pick = (hash(cx as u32 * 131 + cy as u32, (m.clock * 6.0) as u32) * RAIN.len() as f32) as usize;
            Glow { hue: x * 120.0, light: 0.04 + 0.12 * fade * fade, glyph: Some(RAIN[pick.min(RAIN.len() - 1)]) }
        }
        Scene::Warp => {
            // Stars on spokes, flying outward; a streak behind each.
            let r = (dx * dx + dy * dy).sqrt() / (0.5 * aspect.max(1.0));
            let spokes = 200.0;
            let spoke = ((dy.atan2(dx) / tau + 0.5) * spokes) as u32;
            let speed = 0.15 + 0.35 * m.bass + 0.25 * m.beat;
            // Two stars to a spoke, at different points of their flight.
            let mut best = Glow { hue: r * 90.0 + 200.0, light: 0.03 * (1.0 - r).max(0.0), glyph: None };
            for k in 0..2 {
                let at = (hash(spoke, 3 + k) + m.clock * speed * (0.8 + 0.4 * hash(spoke, 9 + k))).rem_euclid(1.0);
                let star = at * at * 1.3;
                // The streak lengthens as the star speeds up towards the edge.
                let streak = 0.04 + 0.3 * speed * star;
                let behind = star - r;
                if (0.0..streak).contains(&behind) {
                    let head = behind < streak * 0.25;
                    let glyph = if !head { '·' } else if star > 0.7 { '✦' } else { '•' };
                    let light = 0.06 + 0.14 * (1.0 - behind / streak) * star.min(1.0);
                    if light > best.light {
                        best = Glow { hue: spoke as f32 * 3.0, light, glyph: Some(glyph) };
                    }
                }
            }
            best
        }
    }
}

fn luma(c: Color, fallback: f32) -> f32 {
    match c {
        Color::Rgb(r, g, b) => (0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32) / 255.0,
        _ => fallback,
    }
}

fn rgb(c: Rgb) -> Color {
    Color::Rgb(c.0, c.1, c.2)
}

/// Paint party mode over the finished frame in `area`. `base` is the
/// interface's own background, which tells raised panels from the page;
/// cover art in `art` keeps its own colours and gets a frame of light.
pub fn paint(buf: &mut Buffer, area: Rect, base: Rgb, art: &[Rect], rave: &Rave, m: &Music) {
    let (w, h) = (area.width as usize, area.height as usize);
    if w == 0 || h == 0 {
        return;
    }
    let beat = m.beat;
    let base_hue = m.beats as f32 * 47.0 + m.clock * 28.0;
    let base_luma = luma(rgb(base), 0.05);
    // Colours are snapped to steps so cells that haven't really changed are
    // not resent to the terminal on every frame.
    let step = |v: f32, n: f32| (v * n).round() / n;
    let in_art = |col: u16, row: u16| art.iter().any(|r| col >= r.x && col < r.right() && row >= r.y && row < r.bottom());
    // Around each cover, a ring of cells.
    let by_art = |col: u16, row: u16| {
        art.iter().any(|r| col + 1 >= r.x && col <= r.right() && row + 1 >= r.y && row <= r.bottom())
    };
    let mixing = rave.change < 1.0;
    let flash = rave.drop * rave.drop * 0.12;
    // Which cells were empty to begin with. A scene's glyphs go only where
    // both neighbours are empty too, so they never sit between two words.
    let blank: Vec<bool> = (0..w * h)
        .map(|i| buf.cell((area.x + (i % w) as u16, area.y + (i / w) as u16)).is_none_or(|c| c.symbol() == " "))
        .collect();
    let open = |x: usize, y: usize| {
        // Not on the bottom line, where the key hints are.
        y + 1 < h && blank[y * w + x] && (x == 0 || blank[y * w + x - 1]) && (x + 1 == w || blank[y * w + x + 1])
    };

    for y in 0..h {
        let row = area.y + y as u16;
        for x in 0..w {
            let col = area.x + x as u16;
            if in_art(col, row) {
                continue;
            }
            let Some(cell) = buf.cell_mut((col, row)) else { continue };
            let mut g = glow(rave.scene, x, y, w, h, m);
            if mixing {
                // Fading from the last scene into this one.
                let was = glow(rave.last, x, y, w, h, m);
                let t = rave.change;
                g = Glow { hue: was.hue + (g.hue - was.hue) * t, light: was.light + (g.light - was.light) * t, glyph: if t > 0.5 { g.glyph } else { was.glyph } };
            }
            let hue = step((base_hue + g.hue) / 360.0, 40.0) * 360.0;
            let raised = luma(cell.bg, base_luma) > base_luma + 0.02;
            let mut light = 0.05 + beat * 0.075 + g.light + flash;
            if raised {
                light += 0.07;
            }
            if by_art(col, row) {
                // The frame round the cover: brightest on the beat.
                light += 0.12 + 0.12 * beat;
            }
            let bg = art::hsl_to_rgb(hue, 0.78, step(light.min(0.4), 48.0));
            cell.set_bg(rgb(bg));

            if cell.symbol() != " " {
                let strength = luma(cell.fg, 0.9);
                let fg = if strength > 0.80 {
                    art::mix((255, 255, 255), art::hsl_to_rgb(hue + 40.0, 1.0, 0.8), 0.18)
                } else if strength > 0.42 {
                    art::hsl_to_rgb(hue + 160.0, 0.95, 0.74)
                } else {
                    art::hsl_to_rgb(hue + 160.0, 0.7, 0.58)
                };
                cell.set_fg(rgb(fg));
            } else if let Some(glyph) = g.glyph.filter(|_| open(x, y)) {
                // Only into empty cells: the scene never covers anything up.
                cell.set_char(glyph).set_fg(rgb(art::hsl_to_rgb(hue + 120.0, 0.9, 0.55 + g.light)));
            }
        }
    }

    let width = buf.area.width as usize;
    let shift_row = |buf: &mut Buffer, y: usize, shift: isize| {
        if shift == 0 || y >= h {
            return;
        }
        let start = (area.y as usize + y - buf.area.y as usize) * width;
        let cells = &mut buf.content[start..start + width];
        if shift > 0 {
            cells.rotate_right(shift as usize % width);
        } else {
            cells.rotate_left((-shift) as usize % width);
        }
    };

    // Sway: rows slide sideways in a travelling wave that kicks on the beat.
    // The bottom line stays put so the key hints remain readable.
    if beat > 0.05 {
        for y in 0..h.saturating_sub(1) {
            let shift = ((m.clock * 2.4 + y as f32 * 0.55).sin() * (0.2 + 2.3 * beat)).round() as isize;
            shift_row(buf, y, shift);
        }
    }

    // A drop glitches a few rows sideways, by a few cells, for a moment.
    if rave.drop > 0.55 {
        let seed = rave.drops * 977 + (rave.drop * 12.0) as u32;
        for k in 0..4 {
            let y = (hash(seed, k) * h.saturating_sub(1) as f32) as usize;
            let shift = ((hash(seed, k + 10) - 0.5) * 24.0) as isize;
            shift_row(buf, y, shift);
        }
    }

    // And shakes the whole screen, a cell either way, settling as it fades.
    if rave.drop > 0.3 {
        let by = if (m.clock * 30.0) as u32 % 2 == 0 { 1 } else { -1 };
        for y in 0..h.saturating_sub(1) {
            shift_row(buf, y, by);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_scene_every_four_bars() {
        let mut rave = Rave::default();
        let mut seen = vec![rave.scene];
        for beats in 1..=16 * 6 {
            rave.tick(0.5, 0.5, true, beats);
            if *seen.last().unwrap() != rave.scene {
                assert_eq!(beats % 16, 0, "changed scene on beat {beats}");
                seen.push(rave.scene);
            }
        }
        assert_eq!(seen.len(), 7, "{seen:?}");
        // Every scene came round.
        for scene in Scene::ORDER {
            assert!(seen.contains(&scene), "{scene:?} never showed");
        }
    }

    #[test]
    fn the_drop_is_found_after_a_lull() {
        let mut rave = Rave::default();
        let mut beats = 0;
        let mut run = |rave: &mut Rave, seconds: f32, level: f32| {
            for i in 0..(seconds * 30.0) as usize {
                // A beat every half second.
                let beat = i % 15 == 0;
                beats += beat as u64;
                rave.tick(1.0 / 30.0, level, beat, beats);
            }
        };
        // Steady music: no drop, however long.
        run(&mut rave, 20.0, 0.5);
        assert_eq!(rave.drops, 0);
        // A breakdown, then everything comes back in.
        run(&mut rave, 4.0, 0.1);
        assert_eq!(rave.drops, 0);
        run(&mut rave, 0.6, 0.8);
        assert_eq!(rave.drops, 1);
        assert!(rave.drop > 0.3);
        assert!(matches!(rave.scene, Scene::Warp | Scene::Tunnel));
        // And only the once.
        run(&mut rave, 5.0, 0.8);
        assert_eq!(rave.drops, 1);
    }

    #[test]
    fn every_scene_draws_something_and_covers_nothing() {
        let bars = [0.5; BANDS];
        let m = Music { bars: &bars, beat: 0.6, beats: 3, bass: 0.5, clock: 4.0 };
        for scene in Scene::ORDER {
            let (w, h) = (80, 24);
            let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
            buf.set_string(10, 5, "Midnight Drive", ratatui::style::Style::default().fg(Color::Rgb(230, 230, 230)));
            let rave = Rave { scene, last: scene, change: 1.0, ..Default::default() };
            paint(&mut buf, Rect::new(0, 0, w, h), (12, 12, 12), &[], &rave, &Music { beat: 0.0, ..m });
            let lights: std::collections::HashSet<_> = buf.content.iter().map(|c| c.bg).collect();
            assert!(lights.len() > 4, "{scene:?} is flat");
            let text: String = (10..24).map(|x| buf[(x, 5)].symbol().to_string()).collect();
            assert_eq!(text, "Midnight Drive", "{scene:?} covered the text");
        }
    }
}
