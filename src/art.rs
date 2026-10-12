//! Album art for a character grid: the renderers plus colour extraction.

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
    /// The cover itself moves with the music: it punches in on each beat.
    Pulse,
    /// The cover given depth: near things and far things drift apart, and
    /// what is nearest jumps forward on the beat.
    Depth,
}

impl ArtMode {
    pub fn parse(s: &str) -> Self {
        match s {
            "ascii" => Self::Ascii,
            "braille" => Self::Braille,
            "pulse" => Self::Pulse,
            "depth" if cfg!(feature = "depth") => Self::Depth,
            _ => Self::Blocks,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Blocks => "blocks",
            Self::Ascii => "ascii",
            Self::Braille => "braille",
            Self::Pulse => "pulse",
            Self::Depth => "depth",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Blocks => Self::Ascii,
            Self::Ascii => Self::Braille,
            Self::Braille => Self::Pulse,
            // Only where the depth model was built in.
            Self::Pulse if cfg!(feature = "depth") => Self::Depth,
            Self::Pulse | Self::Depth => Self::Blocks,
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
        ArtMode::Blocks | ArtMode::Pulse | ArtMode::Depth => {
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

/// How hard the music is hitting the cover right now.
#[derive(Clone, Copy, Default)]
pub struct Fx {
    /// 1.0 at the instant of a beat, falling back to 0.
    pub beat: f32,
    /// Low-end energy, 0..1.
    pub bass: f32,
    /// Free-running animation time in seconds.
    pub time: f32,
}

/// Draw the cover as half-blocks, alive: it zooms in on the beat, its colour
/// channels split apart for an instant, a ripple runs out from the centre and
/// the whole image flashes brighter. With no music it is simply the cover.
pub fn render_pulse(img: &RgbImage, w: u16, h: u16, fx: Fx) -> Rendered {
    let (cw, ch) = (w.max(1) as u32, h.max(1) as u32);
    let (iw, ih) = (img.width() as f32, img.height() as f32);
    let zoom = 1.0 + 0.11 * fx.beat + 0.04 * fx.bass;
    let split = 0.022 * fx.beat;
    let gain = 1.0 + 0.45 * fx.beat + 0.10 * fx.bass;
    let lift = 26.0 * fx.beat;

    // Bilinear sample of one channel at normalised coordinates (centre = 0,0).
    let sample = |u: f32, v: f32, c: usize| -> f32 {
        let x = ((u + 0.5) * iw - 0.5).clamp(0.0, iw - 1.0);
        let y = ((v + 0.5) * ih - 0.5).clamp(0.0, ih - 1.0);
        let (x0, y0) = (x.floor() as u32, y.floor() as u32);
        let (x1, y1) = ((x0 + 1).min(img.width() - 1), (y0 + 1).min(img.height() - 1));
        let (fx_, fy) = (x - x0 as f32, y - y0 as f32);
        let p = |x: u32, y: u32| img.get_pixel(x, y).0[c] as f32;
        let top = p(x0, y0) * (1.0 - fx_) + p(x1, y0) * fx_;
        let bottom = p(x0, y1) * (1.0 - fx_) + p(x1, y1) * fx_;
        top * (1.0 - fy) + bottom * fy
    };
    let pixel = |px: u32, py: u32| -> Rgb {
        let u = (px as f32 + 0.5) / cw as f32 - 0.5;
        let v = (py as f32 + 0.5) / (ch * 2) as f32 - 0.5;
        let r = (u * u + v * v).sqrt().max(1e-4);
        // A ring travelling outward, strongest right after the beat.
        let ripple = (r * 22.0 - fx.time * 9.0).sin() * 0.014 * fx.beat;
        let (u, v) = ((u + u / r * ripple) / zoom, (v + v / r * ripple) / zoom);
        let tone = |value: f32| (value * gain + lift).clamp(0.0, 255.0) as u8;
        (tone(sample(u + split, v, 0)), tone(sample(u, v, 1)), tone(sample(u - split, v, 2)))
    };

    let mut cells = Vec::with_capacity((cw * ch) as usize);
    for y in 0..ch {
        for x in 0..cw {
            cells.push(Cell { ch: '▀', fg: pixel(x, y * 2), bg: Some(pixel(x, y * 2 + 1)) });
        }
    }
    Rendered { w, h, cells }
}

/// How near each part of a cover is: 0 for the far distance, 255 for whatever
/// is closest. A square, `SIDE` to a side, row by row.
pub struct DepthMap {
    pub near: Vec<u8>,
    /// For each spot, the nearest thing anywhere within `REACH` of it: the
    /// most that could ever slide across it, so the search for what shows
    /// there can start from that instead of from the very front.
    ceiling: Vec<u8>,
}

impl DepthMap {
    pub const SIDE: u32 = 252;
    /// How far anything moves in a real picture, in map cells, and then some.
    const REACH: usize = 26;

    /// `None` unless `near` is a full map.
    pub fn new(near: Vec<u8>) -> Option<Self> {
        let side = Self::SIDE as usize;
        if near.len() != side * side {
            return None;
        }
        // The largest value in a square around each cell: along the rows, then down the columns.
        let spread = |from: &[u8], step: usize, line: usize| -> Vec<u8> {
            let mut out = vec![0u8; from.len()];
            for start in 0..side {
                for i in 0..side {
                    let (lo, hi) = (i.saturating_sub(Self::REACH), (i + Self::REACH).min(side - 1));
                    out[start * line + i * step] =
                        (lo..=hi).map(|j| from[start * line + j * step]).max().unwrap_or(0);
                }
            }
            out
        };
        let across = spread(&near, 1, side);
        let ceiling = spread(&across, side, 1);
        Some(Self { near, ceiling })
    }

    #[cfg_attr(not(feature = "depth"), allow(dead_code))]
    pub fn from_bytes(near: Vec<u8>) -> Option<Self> {
        Self::new(near)
    }

    /// The nearest thing anywhere around a point, 0..1.
    fn ceiling_at(&self, u: f32, v: f32) -> f32 {
        let side = Self::SIDE as f32;
        let x = ((u + 0.5) * side).clamp(0.0, side - 1.0) as usize;
        let y = ((v + 0.5) * side).clamp(0.0, side - 1.0) as usize;
        self.ceiling[y * Self::SIDE as usize + x] as f32 * (1.0 / 255.0)
    }

    /// Nearness, 0..1, at normalised coordinates (the centre is 0,0).
    fn at(&self, u: f32, v: f32) -> f32 {
        let side = Self::SIDE as f32;
        let x = ((u + 0.5) * side - 0.5).clamp(0.0, side - 1.0);
        let y = ((v + 0.5) * side - 0.5).clamp(0.0, side - 1.0);
        let (x0, y0) = (x as usize, y as usize);
        let (x1, y1) = ((x0 + 1).min(Self::SIDE as usize - 1), (y0 + 1).min(Self::SIDE as usize - 1));
        let (fx, fy) = (x - x0 as f32, y - y0 as f32);
        // Asked millions of times a second for a picture: kept to the bare sums.
        let p = |x: usize, y: usize| self.near[y * Self::SIDE as usize + x] as f32;
        let top = p(x0, y0) + (p(x1, y0) - p(x0, y0)) * fx;
        let bottom = p(x0, y1) + (p(x1, y1) - p(x0, y1)) * fx;
        (top + (bottom - top) * fy) * (1.0 / 255.0)
    }
}

/// The depth style's point of view at one instant. It drifts in a slow loop,
/// further when the bass is heavy, so near things slide across far ones; on
/// each beat whatever is nearest jumps towards the viewer and catches the light.
struct DepthView {
    lean_x: f32,
    lean_y: f32,
    punch: f32,
    fx: Fx,
    fine: bool,
}

impl DepthView {
    /// Drawn slightly enlarged, so sliding rarely brings the cover's edge into view.
    const MARGIN: f32 = 1.05;
    /// How near something has to be to stay where it is while the rest
    /// slides. Set towards the front: what is in front is usually the
    /// subject, or the title, and should not wander out of the picture.
    const STILL: f32 = 0.6;

    /// `fine` is for a real picture, where a small movement shows; in
    /// half-blocks it has to be bigger to be seen at all.
    fn new(fx: Fx, fine: bool) -> Self {
        let lean = if fine { 0.028 + 0.02 * fx.bass } else { 0.045 + 0.03 * fx.bass };
        Self {
            lean_x: (fx.time * 0.55).sin() * lean,
            lean_y: (fx.time * 0.37).cos() * lean * 0.6,
            punch: 0.13 * fx.beat,
            fx,
            fine,
        }
    }

    /// What shows at a point of the picture (the centre is 0,0 and the edges
    /// are at half): where on the cover it comes from, and how brightly lit.
    fn see(&self, depth: &DepthMap, u: f32, v: f32) -> (f32, f32, f32) {
        let (u, v) = (u / Self::MARGIN, v / Self::MARGIN);
        // Where on the cover a point this near would have been brought here from.
        let from = |near: f32| {
            let slide = (near - Self::STILL) * 2.0;
            // On the beat the front comes forward and the back falls away.
            let grow = 1.0 + self.punch * (near - 0.4);
            ((u + self.lean_x * slide) / grow, (v + self.lean_y * slide) / grow)
        };
        // Step back from the nearest layer until the cover turns out to be at
        // least that near: that is the surface seen here, in front of
        // anything further off. (Asking only "how near is this spot" leaves
        // ghosts of every edge behind as things slide.)
        const LAYERS: usize = 16;
        // The cover getting this much nearer from one step to the next is the
        // side of something, met edge-on across the gap it left when it slid.
        const EDGE_ON: f32 = 0.15;
        let (mut su, mut sv, mut near) = (u, v, 0.0);
        // The last two places looked at that turned out to be open air.
        let mut trail = [(u, v); 2];
        let mut before: Option<(f32, f32)> = None;
        // Nothing nearer than this is close enough to slide over this spot,
        // so the layers in front of it needn't be looked through. (Only where
        // the movement is small enough for the map's reach, and where there
        // are enough pixels for the saving to matter.)
        let first = if self.fine { ((1.0 - depth.ceiling_at(u, v)) * LAYERS as f32) as usize } else { 0 };
        for layer in first..=LAYERS {
            let guess = 1.0 - layer as f32 / LAYERS as f32;
            let (gu, gv) = from(guess);
            let short = guess - depth.at(gu, gv);
            if short <= 0.0 {
                match before {
                    // The gap shows what is behind it, from just clear of the
                    // edge, not the thing itself stretched to cover it.
                    Some((last, was_short)) if (guess - short) - (last - was_short) > EDGE_ON => {
                        (su, sv) = trail[0];
                        near = depth.at(su, sv);
                    }
                    // Between the last layer and this one; settle where they cross.
                    Some((last, was_short)) => {
                        near = last + (guess - last) * was_short / (was_short - short);
                        (su, sv) = from(near);
                    }
                    None => (su, sv, near) = (gu, gv, guess),
                }
                break;
            }
            before = Some((guess, short));
            trail = [if layer == first { (gu, gv) } else { trail[1] }, (gu, gv)];
            (su, sv, near) = (gu, gv, guess);
        }
        // The far distance sits back a little; the beat lights what is close.
        let gain = 0.84 + 0.16 * near + self.fx.beat * 0.28 * near * near + 0.06 * self.fx.bass;
        (su, sv, gain)
    }

    /// The colour of the picture at a point, read off `img`.
    fn colour(&self, img: &RgbImage, depth: &DepthMap, u: f32, v: f32) -> Rgb {
        let (su, sv, gain) = self.see(depth, u, v);
        Self::read(img, su, sv, gain)
    }

    /// The cover's colour at a point of it, lit by `gain`.
    fn read(img: &RgbImage, su: f32, sv: f32, gain: f32) -> Rgb {
        let (iw, ih) = (img.width() as f32, img.height() as f32);
        // Past the edge of the cover, carry on with its mirror image: a
        // background continues, where repeating the last pixel would streak.
        let fold = |t: f32| if t < -0.5 { -1.0 - t } else if t > 0.5 { 1.0 - t } else { t };
        let x = ((fold(su) + 0.5) * iw - 0.5).clamp(0.0, iw - 1.0);
        let y = ((fold(sv) + 0.5) * ih - 0.5).clamp(0.0, ih - 1.0);
        let (x0, y0) = (x.floor() as u32, y.floor() as u32);
        let (x1, y1) = ((x0 + 1).min(img.width() - 1), (y0 + 1).min(img.height() - 1));
        let (tx, ty) = (x - x0 as f32, y - y0 as f32);
        let (a, b, c, d) = (img.get_pixel(x0, y0).0, img.get_pixel(x1, y0).0, img.get_pixel(x0, y1).0, img.get_pixel(x1, y1).0);
        let tone = |i: usize| {
            let top = a[i] as f32 * (1.0 - tx) + b[i] as f32 * tx;
            let bottom = c[i] as f32 * (1.0 - tx) + d[i] as f32 * tx;
            ((top * (1.0 - ty) + bottom * ty) * gain).clamp(0.0, 255.0) as u8
        };
        (tone(0), tone(1), tone(2))
    }
}

/// Draw the cover with depth as half-blocks: two pixels to a character.
pub fn render_depth(img: &RgbImage, depth: &DepthMap, w: u16, h: u16, fx: Fx) -> Rendered {
    let (cw, ch) = (w.max(1) as u32, h.max(1) as u32);
    let view = DepthView::new(fx, false);
    let pixel = |px: u32, py: u32| {
        view.colour(img, depth, (px as f32 + 0.5) / cw as f32 - 0.5, (py as f32 + 0.5) / (ch * 2) as f32 - 0.5)
    };
    let mut cells = Vec::with_capacity((cw * ch) as usize);
    for y in 0..ch {
        for x in 0..cw {
            cells.push(Cell { ch: '▀', fg: pixel(x, y * 2), bg: Some(pixel(x, y * 2 + 1)) });
        }
    }
    Rendered { w, h, cells }
}

/// Run `work` on every row of `rows`, shared out between a few threads. The
/// rows are dealt out in turn rather than in blocks, because some parts of a
/// picture take far longer than others and a block of those would hold
/// everything up.
fn share_rows<T: Send>(rows: Vec<&mut [T]>, work: impl Fn(usize, &mut [T]) + Sync) {
    let threads = std::thread::available_parallelism().map_or(2, |n| n.get()).clamp(1, 6);
    let mut hands: Vec<Vec<(usize, &mut [T])>> = (0..threads).map(|_| Vec::new()).collect();
    for (n, row) in rows.into_iter().enumerate() {
        hands[n % threads].push((n, row));
    }
    std::thread::scope(|scope| {
        for hand in hands {
            let work = &work;
            scope.spawn(move || {
                for (n, row) in hand {
                    work(n, row);
                }
            });
        }
    });
}

/// Draw the cover with depth as a real picture, `w` by `h` pixels of RGB, for
/// terminals that can show one. This is thousands of times the pixels of the
/// half-block version, so two things keep it quick: the rows are shared out
/// between threads, and where each point comes from on the cover (the slow
/// part) is worked out for every other pixel each way and filled in between.
/// The colours themselves are read for every pixel, so nothing gets blurred.
pub fn render_depth_rgb(img: &RgbImage, depth: &DepthMap, w: u32, h: u32, fx: Fx) -> Vec<u8> {
    const EVERY: usize = 2;
    let (w, h) = (w.max(1) as usize, h.max(1) as usize);
    let view = DepthView::new(fx, true);

    // Where from, and how lit, on a grid: one more point than needed each
    // way, so every pixel sits between four of them.
    let (gw, gh) = ((w - 1) / EVERY + 2, (h - 1) / EVERY + 2);
    let mut grid = vec![(0f32, 0f32, 0f32); gw * gh];
    share_rows(grid.chunks_mut(gw).collect(), |gy, row| {
        let v = ((gy * EVERY) as f32 + 0.5) / h as f32 - 0.5;
        for (gx, point) in row.iter_mut().enumerate() {
            *point = view.see(depth, ((gx * EVERY) as f32 + 0.5) / w as f32 - 0.5, v);
        }
    });

    let mut out = vec![0u8; w * h * 3];
    let grid = &grid;
    share_rows(out.chunks_mut(w * 3).collect(), |y, row| {
        let (gy, ty) = (y / EVERY, (y % EVERY) as f32 / EVERY as f32);
        for (x, pixel) in row.chunks_exact_mut(3).enumerate() {
            let (gx, tx) = (x / EVERY, (x % EVERY) as f32 / EVERY as f32);
            let (a, b) = (grid[gy * gw + gx], grid[gy * gw + gx + 1]);
            let (c, d) = (grid[(gy + 1) * gw + gx], grid[(gy + 1) * gw + gx + 1]);
            let mix = |a: f32, b: f32, c: f32, d: f32| {
                let (top, bottom) = (a + (b - a) * tx, c + (d - c) * tx);
                top + (bottom - top) * ty
            };
            let rgb = DepthView::read(img, mix(a.0, b.0, c.0, d.0), mix(a.1, b.1, c.1, d.1), mix(a.2, b.2, c.2, d.2));
            pixel.copy_from_slice(&[rgb.0, rgb.1, rgb.2]);
        }
    });
    out
}

/// Spotify serves each cover in a few sizes, named in the address. Given the
/// one riff normally loads (300 pixels), the address of the largest (640).
pub fn larger(url: &str) -> Option<String> {
    url.contains("ab67616d00001e02").then(|| url.replace("ab67616d00001e02", "ab67616d0000b273"))
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
        assert_eq!(render(&img, 20, 10, ArtMode::Pulse).cells.len(), 200);
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
    fn pulse_at_rest_is_the_plain_cover() {
        let img = RgbImage::from_fn(64, 64, |x, y| image::Rgb([x as u8 * 4, y as u8 * 4, 128]));
        let still = render_pulse(&img, 16, 8, Fx::default());
        let plain = render(&img, 16, 8, ArtMode::Blocks);
        // Same picture, give or take resampling.
        let far_off = still
            .cells
            .iter()
            .zip(&plain.cells)
            .filter(|(a, b)| a.fg.0.abs_diff(b.fg.0) > 24 || a.fg.1.abs_diff(b.fg.1) > 24)
            .count();
        assert!(far_off < 8, "{far_off} cells differ");
        // On a beat it brightens and moves.
        let hit = render_pulse(&img, 16, 8, Fx { beat: 1.0, bass: 0.5, time: 1.0 });
        let sum = |r: &Rendered| r.cells.iter().map(|c| c.fg.0 as u32 + c.fg.1 as u32 + c.fg.2 as u32).sum::<u32>();
        assert!(sum(&hit) > sum(&still));
    }

    /// A cover with a bright square in the middle, and a depth map that says
    /// the square is near and everything else far.
    fn square_in_front() -> (RgbImage, DepthMap) {
        let inside = |x: u32, y: u32, side: u32| (side * 3 / 8..side * 5 / 8).contains(&x) && (side * 3 / 8..side * 5 / 8).contains(&y);
        let cover = RgbImage::from_fn(96, 96, |x, y| image::Rgb(if inside(x, y, 96) { [250, 250, 250] } else { [10, 10, 10] }));
        let side = DepthMap::SIDE;
        let near = (0..side * side).map(|i| if inside(i % side, i / side, side) { 255 } else { 0 }).collect();
        (cover, DepthMap::new(near).unwrap())
    }

    /// Columns of row `row` of cells that are fully lit.
    fn lit(frame: &Rendered, row: usize) -> Vec<usize> {
        (0..frame.w as usize).filter(|&x| frame.cells[row * frame.w as usize + x].fg.0 > 200).collect()
    }

    #[test]
    fn depth_slides_near_things_and_leaves_far_ones() {
        let (cover, depth) = square_in_front();
        // Leaning fully one way, then the other (the lean follows a sine).
        let quarter = std::f32::consts::FRAC_PI_2 / 0.55;
        let one = render_depth(&cover, &depth, 64, 32, Fx { time: quarter, ..Default::default() });
        let other = render_depth(&cover, &depth, 64, 32, Fx { time: quarter * 3.0, ..Default::default() });
        let (a, b) = (lit(&one, 16), lit(&other, 16));
        assert!(!a.is_empty() && !b.is_empty());
        let shift = (a[0] as i32 - b[0] as i32).abs();
        assert!(shift >= 3, "the near square only moved {shift} cells");
        // It moved as a whole: no smear, no second copy.
        assert!((a.len() as i32 - b.len() as i32).abs() <= 2);
        assert_eq!(a.last().unwrap() - a[0] + 1, a.len(), "the square broke up");
    }

    #[test]
    fn depth_brings_near_things_forward_on_the_beat() {
        let (cover, depth) = square_in_front();
        let still = render_depth(&cover, &depth, 64, 32, Fx::default());
        let hit = render_depth(&cover, &depth, 64, 32, Fx { beat: 1.0, ..Default::default() });
        assert!(lit(&hit, 16).len() > lit(&still, 16).len(), "the square didn't grow on the beat");
        // The far background doesn't flash with it.
        assert!(hit.cells[0].fg.0 < 40);
    }

    #[test]
    fn the_picture_is_whole_however_the_rows_are_shared_out() {
        // A height that doesn't divide evenly between the threads drawing it.
        let (cover, depth) = square_in_front();
        let fx = Fx { time: 2.0, beat: 0.5, bass: 0.3 };
        let (w, h) = (40usize, 37usize);
        let picture = render_depth_rgb(&cover, &depth, w as u32, h as u32, fx);
        assert_eq!(picture.len(), w * h * 3);
        // Every other pixel each way is worked out in full; check those.
        let view = DepthView::new(fx, true);
        for y in (0..h).step_by(2) {
            for x in (0..w).step_by(2) {
                let want = view.colour(&cover, &depth, (x as f32 + 0.5) / w as f32 - 0.5, (y as f32 + 0.5) / h as f32 - 0.5);
                let got = &picture[(y * w + x) * 3..][..3];
                assert_eq!((got[0], got[1], got[2]), want, "at {x},{y}");
            }
        }
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
