//! Album art for a character grid: three renderers plus colour extraction.

use image::{RgbImage, imageops::FilterType};

pub type Rgb = (u8, u8, u8);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ArtMode {
    /// Half-block cells in true colour: two pixels per character.
    Blocks,
    /// Classic density-ramp ASCII, tinted with the cover's colours.
    Ascii,
    /// Dithered braille dots: eight sub-pixels per character.
    Braille,
    Off,
}

impl ArtMode {
    pub fn parse(s: &str) -> Self {
        match s {
            "ascii" => Self::Ascii,
            "braille" => Self::Braille,
            "off" => Self::Off,
            _ => Self::Blocks,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Blocks => "blocks",
            Self::Ascii => "ascii",
            Self::Braille => "braille",
            Self::Off => "off",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Blocks => Self::Ascii,
            Self::Ascii => Self::Braille,
            Self::Braille => Self::Off,
            Self::Off => Self::Blocks,
        }
    }
}

#[derive(Clone, Copy)]
pub struct Cell {
    pub ch: char,
    pub fg: Rgb,
    pub bg: Option<Rgb>,
}

pub struct Rendered {
    pub w: u16,
    pub h: u16,
    pub cells: Vec<Cell>,
}

const RAMP: &[u8] = b" .'`^\",:;Il!i><~+_-?][}{1)(|/tfjrxnuvczXYUJCLQ0OZmwqpdbkhao*#MW&8%B@$";

fn luma(p: &[u8]) -> f32 {
    (0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32) / 255.0
}

/// Brighten a colour so glyphs drawn in it stay readable on a dark background.
fn lift(p: &[u8], floor: f32) -> Rgb {
    let max = p[0].max(p[1]).max(p[2]) as f32 / 255.0;
    if max < 0.02 {
        let g = (floor * 255.0) as u8;
        return (g, g, g);
    }
    let k = (floor + (1.0 - floor) * max) / max;
    let ch = |c: u8| (c as f32 * k).min(255.0) as u8;
    (ch(p[0]), ch(p[1]), ch(p[2]))
}

/// Luminance of every pixel, stretched to use the full 0..1 range so dark or
/// washed-out covers still show their structure.
fn stretched(img: &RgbImage) -> Vec<f32> {
    let mut l: Vec<f32> = img.pixels().map(|p| luma(&p.0)).collect();
    let (lo, hi) = l.iter().fold((1f32, 0f32), |(lo, hi), &v| (lo.min(v), hi.max(v)));
    let span = (hi - lo).max(0.08);
    for v in &mut l {
        *v = ((*v - lo) / span).clamp(0.0, 1.0);
    }
    l
}

pub fn render(img: &RgbImage, w: u16, h: u16, mode: ArtMode) -> Rendered {
    let (cw, chh) = (w.max(1) as u32, h.max(1) as u32);
    let mut cells = Vec::with_capacity((cw * chh) as usize);
    match mode {
        ArtMode::Off => {}
        ArtMode::Blocks => {
            let small = image::imageops::resize(img, cw, chh * 2, FilterType::CatmullRom);
            for y in 0..chh {
                for x in 0..cw {
                    let top = small.get_pixel(x, y * 2).0;
                    let bot = small.get_pixel(x, y * 2 + 1).0;
                    cells.push(Cell {
                        ch: '▀',
                        fg: (top[0], top[1], top[2]),
                        bg: Some((bot[0], bot[1], bot[2])),
                    });
                }
            }
        }
        ArtMode::Ascii => {
            let small = image::imageops::resize(img, cw, chh, FilterType::CatmullRom);
            let l = stretched(&small);
            for (i, p) in small.pixels().enumerate() {
                let idx = (l[i].powf(0.9) * (RAMP.len() - 1) as f32).round() as usize;
                cells.push(Cell {
                    ch: RAMP[idx.min(RAMP.len() - 1)] as char,
                    fg: lift(&p.0, 0.45),
                    bg: None,
                });
            }
        }
        ArtMode::Braille => {
            let (pw, ph) = (cw * 2, chh * 4);
            let small = image::imageops::resize(img, pw, ph, FilterType::CatmullRom);
            let mut l = stretched(&small);
            // Floyd–Steinberg error diffusion down to one bit per dot.
            let at = |x: u32, y: u32| (y * pw + x) as usize;
            let mut on = vec![false; l.len()];
            for y in 0..ph {
                for x in 0..pw {
                    let old = l[at(x, y)];
                    let lit = old > 0.5;
                    on[at(x, y)] = lit;
                    let e = old - if lit { 1.0 } else { 0.0 };
                    if x + 1 < pw {
                        l[at(x + 1, y)] += e * 7.0 / 16.0;
                    }
                    if y + 1 < ph {
                        if x > 0 {
                            l[at(x - 1, y + 1)] += e * 3.0 / 16.0;
                        }
                        l[at(x, y + 1)] += e * 5.0 / 16.0;
                        if x + 1 < pw {
                            l[at(x + 1, y + 1)] += e / 16.0;
                        }
                    }
                }
            }
            const DOTS: [[u32; 2]; 4] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];
            for cy in 0..chh {
                for cx in 0..cw {
                    let mut bits = 0u32;
                    let mut sum = [0u32; 3];
                    for (dy, row) in DOTS.iter().enumerate() {
                        for (dx, bit) in row.iter().enumerate() {
                            let (x, y) = (cx * 2 + dx as u32, cy * 4 + dy as u32);
                            if on[at(x, y)] {
                                bits |= bit;
                            }
                            let p = small.get_pixel(x, y).0;
                            for c in 0..3 {
                                sum[c] += p[c] as u32;
                            }
                        }
                    }
                    let avg = [(sum[0] / 8) as u8, (sum[1] / 8) as u8, (sum[2] / 8) as u8];
                    cells.push(Cell {
                        ch: char::from_u32(0x2800 + bits).unwrap_or(' '),
                        fg: lift(&avg, 0.55),
                        bg: None,
                    });
                }
            }
        }
    }
    Rendered { w, h, cells }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    /// Vivid colour for highlights, readable on a dark background.
    pub accent: Rgb,
    /// Very dark tint of the cover, used as a backdrop.
    pub shade: Rgb,
}

pub const GREEN: Rgb = (30, 215, 96);

impl Default for Palette {
    fn default() -> Self {
        Self { accent: GREEN, shade: (16, 20, 18) }
    }
}

fn rgb_to_hsl((r, g, b): Rgb) -> (f32, f32, f32) {
    let (r, g, b) = (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let l = (max + min) / 2.0;
    let d = max - min;
    if d < 1e-5 {
        return (0.0, 0.0, l);
    }
    let s = d / (1.0 - (2.0 * l - 1.0).abs()).max(1e-5);
    let h = if max == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h * 60.0, s.min(1.0), l)
}

pub fn hsl_to_rgb(h: f32, s: f32, l: f32) -> Rgb {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h.rem_euclid(360.0) / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let ch = |v: f32| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    (ch(r), ch(g), ch(b))
}

/// Pick the cover's most characteristic colour: the hue that carries the most
/// saturated, visible pixels. Greyscale covers fall back to a soft white.
pub fn palette(img: &RgbImage) -> Palette {
    let small = image::imageops::resize(img, 40, 40, FilterType::Triangle);
    const BINS: usize = 24;
    let mut weight = [0f32; BINS];
    let mut sum = [[0f32; 3]; BINS];
    let mut mean = [0f32; 3];
    for p in small.pixels() {
        let (h, s, l) = rgb_to_hsl((p.0[0], p.0[1], p.0[2]));
        for c in 0..3 {
            mean[c] += p.0[c] as f32;
        }
        // Favour colourful mid-tones; near-black and near-white say little.
        let w = s * s * (1.0 - (2.0 * l - 1.0).abs()).max(0.0);
        if w < 0.02 {
            continue;
        }
        let bin = ((h / 360.0 * BINS as f32) as usize).min(BINS - 1);
        weight[bin] += w;
        for c in 0..3 {
            sum[bin][c] += p.0[c] as f32 * w;
        }
    }
    let n = (small.width() * small.height()) as f32;
    let mean = ((mean[0] / n) as u8, (mean[1] / n) as u8, (mean[2] / n) as u8);

    let best = (0..BINS).max_by(|&a, &b| weight[a].total_cmp(&weight[b])).unwrap_or(0);
    let accent = if weight[best] < n * 0.004 {
        (226, 230, 228)
    } else {
        let w = weight[best];
        let raw = ((sum[best][0] / w) as u8, (sum[best][1] / w) as u8, (sum[best][2] / w) as u8);
        let (h, s, l) = rgb_to_hsl(raw);
        hsl_to_rgb(h, s.clamp(0.5, 0.9), l.clamp(0.58, 0.72))
    };
    let (h, s, _) = rgb_to_hsl(mean);
    Palette { accent, shade: hsl_to_rgb(h, s.min(0.45), 0.075) }
}

pub fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    (f(a.0, b.0), f(a.1, b.1), f(a.2, b.2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_match_request() {
        let img = RgbImage::from_fn(64, 64, |x, y| image::Rgb([x as u8 * 4, y as u8 * 4, 128]));
        for mode in [ArtMode::Blocks, ArtMode::Ascii, ArtMode::Braille] {
            let r = render(&img, 20, 10, mode);
            assert_eq!(r.cells.len(), 200, "{mode:?}");
        }
        assert!(render(&img, 20, 10, ArtMode::Off).cells.is_empty());
    }

    #[test]
    fn accent_follows_the_cover() {
        let red = RgbImage::from_pixel(32, 32, image::Rgb([200, 30, 30]));
        let p = palette(&red);
        assert!(p.accent.0 > p.accent.1 && p.accent.0 > p.accent.2);
        let grey = RgbImage::from_pixel(32, 32, image::Rgb([90, 90, 90]));
        let g = palette(&grey).accent;
        assert!(g.0.abs_diff(g.1) < 12 && g.1.abs_diff(g.2) < 12);
    }

    #[test]
    fn hsl_round_trip() {
        for c in [(255, 0, 0), (12, 200, 99), (40, 40, 200), (128, 128, 128)] {
            let (h, s, l) = rgb_to_hsl(c);
            let back = hsl_to_rgb(h, s, l);
            assert!(c.0.abs_diff(back.0) <= 2 && c.1.abs_diff(back.1) <= 2 && c.2.abs_diff(back.2) <= 2);
        }
    }
}
