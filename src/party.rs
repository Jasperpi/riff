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
    /// The colours of the cover playing: its accent, and a deep shade of it.
    pub accent: Rgb,
    pub shade: Rgb,
}

impl Music<'_> {
    /// The spectrum at `t`, 0 for the lowest band and 1 for the highest.
    fn band(&self, t: f32) -> f32 {
        let at = t.clamp(0.0, 1.0) * (BANDS - 1) as f32;
        let (i, f) = (at as usize, at.fract());
        let next = (i + 1).min(BANDS - 1);
        self.bars[i] * (1.0 - f) + self.bars[next] * f
    }
}

/// What a scene puts at one point: how much light, which of the two
/// colours it is in (0 the cover's own, 1 its complement), and for an
/// empty cell, perhaps a glyph.
#[derive(Clone, Copy, Default)]
struct Light {
    level: f32,
    tint: f32,
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

/// The light of `scene` at pixel `px`, `py` of a screen `w` pixels wide and
/// `h` tall (two pixels to a cell, one above the other, about square).
fn light(scene: Scene, px: f32, py: f32, w: f32, h: f32, m: &Music) -> Light {
    let (x, y) = (px / w, py / h);
    // Distances in pixels, which are about as tall as they are wide.
    let (dx, dy) = (px - w / 2.0, py - h / 2.0);
    let unit = w.min(h) / 2.0;
    let tau = std::f32::consts::TAU;
    match scene {
        Scene::Edges => {
            // Bars reach in from all four edges: lows in the middle of each
            // edge, highs towards the corners. `across` and `down` run from 0
            // on the centre line to 1 at the edge. The side bars stop sooner,
            // so as to look as deep as the top and bottom ones.
            let across = (x - 0.5).abs() * 2.0;
            let down = (y - 0.5).abs() * 2.0;
            // Separate bars, with a gap between each: 24 from the middle of
            // the top and bottom edges out to the corners, 10 on the sides.
            let bar_of = |t: f32, n: f32| {
                let k = (t * n).floor();
                ((t * n).fract() < 0.7).then_some(((k + 0.5) / n).min(1.0))
            };
            let (tall, wide) = (bar_of(across, 24.0).map_or(0.0, |t| m.band(t)), bar_of(down, 10.0).map_or(0.0, |t| m.band(t)));
            let (under, bar) = {
                let (a, b) = (tall * 0.9 - (1.0 - down), wide * 0.5 - (1.0 - across));
                if a > b { (a, tall) } else { (b, wide) }
            };
            // Brightest at the tip of each bar, with a glow beyond it; a bar
            // with nothing in it has no tip to glow.
            let strength = (bar * 5.0).min(1.0);
            let level = if under > 0.0 { 0.35 + 0.55 * (-under * 5.0).exp() } else { 0.75 * (under * 28.0).exp() } * strength;
            Light { level, tint: across.max(down).powi(2), glyph: None }
        }
        Scene::Tunnel => {
            // Rounded rectangles of light rushing outward, leaping on each beat.
            let (nx, ny) = (dx / (w / 2.0), dy / (h / 2.0));
            let r = (nx.powi(4) + ny.powi(4)).powf(0.25);
            let phase = r * 6.0 - m.clock * 1.4 - m.beats as f32 * 0.5 - (1.0 - m.beat) * 0.5;
            let off = (phase - phase.round()).abs();
            let ring = (-(off / 0.07).powi(2)).exp();
            let level = ring * (0.25 + 0.75 * r.min(1.0)) + 0.06 * r + m.band(r.min(1.0)) * 0.1;
            Light { level, tint: (phase.round() as i64).rem_euclid(2) as f32, glyph: None }
        }
        Scene::Plasma => {
            // Slow waves running into each other, like a lava lamp.
            let (u, v) = (dx / unit, dy / unit);
            let t = m.clock * 0.9;
            let r = (u * u + v * v).sqrt();
            let wave = (u * 2.2 + t).sin() + (v * 1.8 - t * 1.3).sin() + ((u + v) * 1.6 + t * 0.7).sin() + (r * (3.0 + 1.5 * m.bass) - t * 2.0).sin();
            let v = wave / 8.0 + 0.5;
            Light { level: 0.08 + 1.05 * v * v * v + 0.15 * m.beat * v, tint: ((v * tau + t * 0.3).sin() * 0.5 + 0.5), glyph: None }
        }
        Scene::Lasers => {
            // Thin beams from two points on the floor, sweeping back and
            // forth through a haze, each with a soft halo.
            let mut level = 0.04 + 0.03 * hash((px / 6.0) as u32, (py / 6.0) as u32 + (m.clock * 2.0) as u32);
            let mut tint = 0.0;
            let mut strongest = 0.0;
            for k in 0..6 {
                let from = if k % 2 == 0 { w * 0.22 } else { w * 0.78 };
                let (bx, by) = (px - from, py - h * 1.02);
                let angle = by.atan2(bx);
                let aim = -tau / 4.0 + (m.clock * 0.6 + k as f32 * 1.7).sin() * 0.85;
                let off = ((angle - aim + tau / 2.0).rem_euclid(tau) - tau / 2.0).abs();
                let core = (-(off / (0.006 + 0.004 * m.beat)).powi(2)).exp();
                let halo = (-(off / 0.05).powi(2)).exp() * 0.22;
                let beam = (core * 0.9 + halo) * (0.6 + 0.4 * m.beat);
                level += beam;
                if beam > strongest {
                    strongest = beam;
                    tint = (k % 2) as f32;
                }
            }
            Light { level, tint, glyph: None }
        }
        Scene::Rain => {
            // About half the columns carry a stream falling at the pace of
            // its part of the spectrum, glowing behind its head.
            let col = (px.max(0.0)) as u32;
            let band = m.band(x);
            if hash(col, 5) > 0.5 {
                return Light { level: 0.03, tint: 0.0, glyph: None };
            }
            let speed = 0.25 + 0.9 * band;
            let head = (hash(col, 7) + m.clock * speed * 0.25).rem_euclid(1.0) * 1.5 - 0.25;
            let behind = head - y;
            let trail = 0.3 + 0.2 * band;
            if !(0.0..trail).contains(&behind) {
                return Light { level: 0.03, tint: 0.0, glyph: None };
            }
            let fade = 1.0 - behind / trail;
            let at_head = behind < 0.03;
            let pick = (hash(col * 131 + (py / 2.0) as u32, (m.clock * 6.0) as u32) * RAIN.len() as f32) as usize;
            Light {
                level: if at_head { 1.0 } else { 0.08 + 0.55 * fade * fade },
                tint: band,
                glyph: (fade > 0.35).then(|| RAIN[pick.min(RAIN.len() - 1)]),
            }
        }
        Scene::Warp => {
            // Stars on spokes from the middle, flying outward, each with a
            // streak behind it that lengthens as it speeds up.
            let r = (dx * dx + dy * dy).sqrt() / unit;
            let spokes = 260.0;
            let spoke = ((dy.atan2(dx) / tau + 0.5) * spokes) as u32;
            let speed = 0.15 + 0.35 * m.bass + 0.25 * m.beat;
            let mut best = Light { level: 0.05 * (1.0 - r * 0.5).max(0.0), tint: 0.0, glyph: None };
            for k in 0..2 {
                if hash(spoke, 20 + k) > 0.85 {
                    continue;
                }
                let at = (hash(spoke, 3 + k) + m.clock * speed * (0.8 + 0.4 * hash(spoke, 9 + k))).rem_euclid(1.0);
                let star = at * at * 1.6;
                let streak = 0.04 + 0.35 * speed * star;
                let behind = star - r;
                if (0.0..streak).contains(&behind) {
                    let level = 0.3 + 0.95 * (1.0 - behind / streak).powi(2) * (0.3 + star).min(1.0);
                    if level > best.level {
                        let head = behind < streak * 0.15 && star > 0.5;
                        best = Light { level, tint: hash(spoke, 30), glyph: head.then_some(if star > 1.0 { '✦' } else { '•' }) };
                    }
                }
            }
            best
        }
    }
}

/// From darkness through the colour to white: light turned into colour.
struct Ramp {
    stops: [[f32; 3]; 2],
    dark: [f32; 3],
}

impl Ramp {
    /// The cover's accent and its complement, over a deep shade of the cover.
    fn new(m: &Music, swapped: bool) -> Self {
        let (h, s, l) = art::hsl(m.accent);
        let (h, s) = (h, s.max(0.55));
        let main = art::hsl_to_rgb(h, s, l.clamp(0.5, 0.62));
        // The second colour drifts slowly round the wheel as the beats go by.
        let other = art::hsl_to_rgb(h + 150.0 + (m.beats as f32 * 1.5) % 60.0, s, l.clamp(0.5, 0.62));
        let f = |c: Rgb| [c.0 as f32, c.1 as f32, c.2 as f32];
        let dark = art::mix(m.shade, (0, 0, 0), 0.35);
        let stops = if swapped { [f(other), f(main)] } else { [f(main), f(other)] };
        Self { stops, dark: f(dark) }
    }

    fn colour(&self, level: f32, tint: f32) -> Rgb {
        let tint = tint.clamp(0.0, 1.0);
        let c: [f32; 3] = std::array::from_fn(|i| self.stops[0][i] + (self.stops[1][i] - self.stops[0][i]) * tint);
        let level = level.clamp(0.0, 1.3);
        let lerp = |a: [f32; 3], b: [f32; 3], t: f32| -> [f32; 3] { std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t.clamp(0.0, 1.0)) };
        let deep: [f32; 3] = std::array::from_fn(|i| c[i] * 0.32);
        let out = if level < 0.4 {
            lerp(self.dark, deep, level / 0.4)
        } else if level < 0.75 {
            lerp(deep, c, (level - 0.4) / 0.35)
        } else {
            // Past full colour, towards white: the hot centre of the light.
            lerp(c, [255.0; 3], (level - 0.75) / 0.55 * 0.8)
        };
        // A few steps per channel, so cells that haven't really changed
        // are not sent to the terminal again on every frame.
        let q = |v: f32| ((v.clamp(0.0, 255.0) / 4.0).round() * 4.0).min(255.0) as u8;
        (q(out[0]), q(out[1]), q(out[2]))
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

/// Paint party mode over the finished frame in `area`. Empty cells become
/// two pixels of the scene each; cells with something in them keep it,
/// lit clearly, over a darkened band so it stays readable. Cover art in
/// `art` keeps its own colours and gets a frame of light.
pub fn paint(buf: &mut Buffer, area: Rect, art: &[Rect], rave: &Rave, m: &Music) {
    let (w, h) = (area.width as usize, area.height as usize);
    if w == 0 || h == 0 {
        return;
    }
    let ramp = Ramp::new(m, rave.drops % 2 == 1);
    let (pw, ph) = (w as f32, h as f32 * 2.0);
    let flash = rave.drop * rave.drop * 0.5;
    let lift = m.beat * 0.06 + flash;
    let mixing = rave.change < 1.0;
    let at = |px: f32, py: f32| {
        let mut l = light(rave.scene, px, py, pw, ph, m);
        if mixing {
            // Fading from the last scene into this one.
            let was = light(rave.last, px, py, pw, ph, m);
            let t = rave.change;
            l = Light { level: was.level + (l.level - was.level) * t, tint: was.tint + (l.tint - was.tint) * t, glyph: if t > 0.5 { l.glyph } else { was.glyph } };
        }
        Light { level: l.level + lift, ..l }
    };
    let in_art = |col: u16, row: u16| art.iter().any(|r| col >= r.x && col < r.right() && row >= r.y && row < r.bottom());
    let by_art = |col: u16, row: u16| art.iter().any(|r| col + 1 >= r.x && col <= r.right() && row + 1 >= r.y && row <= r.bottom());
    // Which cells had nothing in them, before any were changed: a scene's
    // glyphs go only where both neighbours are empty too, so they never sit
    // between two words, and never on the bottom line, with the key hints.
    let blank: Vec<bool> = (0..w * h)
        .map(|i| buf.cell((area.x + (i % w) as u16, area.y + (i / w) as u16)).is_none_or(|c| c.symbol() == " "))
        .collect();
    // Empty, with nothing either side: open sky, where the scene shows. A
    // space beside writing (between two words, say) belongs to the writing.
    let sky = |x: usize, y: usize| blank[y * w + x] && (x == 0 || blank[y * w + x - 1]) && (x + 1 == w || blank[y * w + x + 1]);
    let open = |x: usize, y: usize| y + 1 < h && sky(x, y);
    let main = ramp.colour(0.75, 0.0);
    let text_tint = |strength: f32| -> Rgb {
        if strength > 0.8 {
            art::mix((255, 255, 255), main, 0.1)
        } else if strength > 0.42 {
            art::mix((255, 255, 255), main, 0.4)
        } else {
            art::mix((200, 200, 200), main, 0.6)
        }
    };

    for y in 0..h {
        let row = area.y + y as u16;
        for x in 0..w {
            let col = area.x + x as u16;
            if in_art(col, row) {
                continue;
            }
            let (px, top_y) = (x as f32 + 0.5, y as f32 * 2.0 + 0.5);
            let (top, bottom) = (at(px, top_y), at(px, top_y + 1.0));
            let Some(cell) = buf.cell_mut((col, row)) else { continue };
            if by_art(col, row) {
                // The frame round the cover: a band of light, brightest on the beat.
                let c = ramp.colour(0.85 + 0.35 * m.beat, 0.0);
                if cell.symbol() == " " {
                    cell.set_char(' ').set_bg(rgb(c));
                } else {
                    cell.set_bg(rgb(c)).set_fg(rgb((10, 10, 10)));
                }
                continue;
            }
            if sky(x, y) {
                match top.glyph.or(bottom.glyph).filter(|_| open(x, y)) {
                    Some(glyph) => {
                        let bright = top.level.max(bottom.level);
                        cell.set_char(glyph)
                            .set_fg(rgb(ramp.colour(bright.max(0.9), top.tint)))
                            .set_bg(rgb(ramp.colour(bright * 0.35, top.tint)));
                    }
                    None => {
                        cell.set_char('▀').set_fg(rgb(ramp.colour(top.level, top.tint))).set_bg(rgb(ramp.colour(bottom.level, bottom.tint)));
                    }
                }
            } else {
                // Something is written here: a darkened band behind it, and
                // the writing itself clear and light.
                let strength = luma(cell.fg, 0.9);
                // Never brighter than a dim glow, however bright the scene.
                let behind = ramp.colour(((top.level + bottom.level) * 0.5 * 0.45).min(0.3), top.tint);
                cell.set_bg(rgb(behind)).set_fg(rgb(text_tint(strength)));
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

    // On a strong beat, rows give a little nudge sideways, in a wave down
    // the screen. The bottom line stays put so the key hints remain readable.
    if m.beat > 0.6 {
        for y in 0..h.saturating_sub(1) {
            let shift = ((m.clock * 2.4 + y as f32 * 0.55).sin() * 1.4 * m.beat).round() as isize;
            shift_row(buf, y, shift);
        }
    }

    // A drop glitches a few rows sideways, by a few cells, for a moment.
    if rave.drop > 0.6 {
        let seed = rave.drops * 977 + (rave.drop * 12.0) as u32;
        for k in 0..3 {
            let y = (hash(seed, k) * h.saturating_sub(1) as f32) as usize;
            let shift = ((hash(seed, k + 10) - 0.5) * 16.0) as isize;
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

    fn lit(scene: Scene, accent: Rgb, text: &str) -> Buffer {
        let bars = [0.6; BANDS];
        let m = Music { bars: &bars, beat: 0.0, beats: 3, bass: 0.5, clock: 4.0, accent, shade: (20, 8, 24) };
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 24));
        for row in [3, 8, 12, 20] {
            buf.set_string(10, row, text, ratatui::style::Style::default().fg(Color::Rgb(150, 150, 150)));
        }
        let rave = Rave { scene, last: scene, change: 1.0, ..Default::default() };
        paint(&mut buf, Rect::new(0, 0, 80, 24), &[], &rave, &m);
        buf
    }

    #[test]
    fn writing_stays_readable_in_every_scene() {
        for scene in Scene::ORDER {
            let buf = lit(scene, (230, 80, 160), "Every signal turning green in this town");
            for cell in buf.content.iter().filter(|c| c.symbol() != " " && c.symbol() != "▀") {
                let (fg, bg) = (luma(cell.fg, 0.0), luma(cell.bg, 1.0));
                assert!(fg - bg > 0.45, "{scene:?}: '{}' at {fg:.2} on {bg:.2}", cell.symbol());
            }
        }
    }

    #[test]
    fn the_colours_follow_the_cover() {
        // A scene's light at full strength, in its first colour, is the
        // cover's own hue; its second colour is well round the wheel from it.
        for accent in [(220, 40, 40), (40, 80, 230), (60, 200, 90), (230, 180, 40)] {
            let bars = [0.0; BANDS];
            let m = Music { bars: &bars, beat: 0.0, beats: 0, bass: 0.0, clock: 0.0, accent, shade: (20, 8, 24) };
            let ramp = Ramp::new(&m, false);
            let (want, _, _) = art::hsl(accent);
            let (got, _, _) = art::hsl(ramp.colour(0.75, 0.0));
            let apart = |a: f32, b: f32| ((a - b).rem_euclid(360.0)).min((b - a).rem_euclid(360.0));
            assert!(apart(want, got) < 12.0, "{accent:?} lit at hue {got}, not {want}");
            let (other, _, _) = art::hsl(ramp.colour(0.75, 1.0));
            assert!(apart(want, other) > 90.0, "{accent:?}: its second colour is too close");
            // The dark is dark, and the brightest nearly white.
            assert!(luma(rgb(ramp.colour(0.0, 0.0)), 1.0) < 0.1);
            assert!(luma(rgb(ramp.colour(1.3, 0.0)), 0.0) > 0.85);
            // A drop swaps the two round.
            assert_eq!(Ramp::new(&m, true).colour(0.75, 0.0), ramp.colour(0.75, 1.0));
        }
    }

    #[test]
    fn every_scene_draws_something_and_covers_nothing() {
        let bars = [0.5; BANDS];
        let m = Music { bars: &bars, beat: 0.6, beats: 3, bass: 0.5, clock: 4.0, accent: (230, 80, 160), shade: (20, 8, 24) };
        for scene in Scene::ORDER {
            let (w, h) = (80, 24);
            let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
            buf.set_string(10, 5, "Midnight Drive", ratatui::style::Style::default().fg(Color::Rgb(230, 230, 230)));
            let rave = Rave { scene, last: scene, change: 1.0, ..Default::default() };
            paint(&mut buf, Rect::new(0, 0, w, h), &[], &rave, &Music { beat: 0.0, ..m });
            let lights: std::collections::HashSet<_> = buf.content.iter().map(|c| c.bg).collect();
            assert!(lights.len() > 4, "{scene:?} is flat");
            let text: String = (10..24).map(|x| buf[(x, 5)].symbol().to_string()).collect();
            assert_eq!(text, "Midnight Drive", "{scene:?} covered the text");
        }
    }
}
