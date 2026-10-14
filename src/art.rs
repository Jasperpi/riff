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
    /// Beats heard so far: bars and phrases are counted from these.
    pub beats: u64,
    /// How far through the current beat, 0..1, judged from the last few.
    pub through: f32,
    /// High-end energy, 0..1: the sparkle.
    pub highs: f32,
    /// How many motes drift through a cover with depth, 0..1. None by default.
    pub motes: f32,
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
    /// Which way the surface faces at each spot: how fast it comes nearer
    /// across and down, in nearness per width of the cover. The light
    /// falls on it by this.
    slope: Vec<[f32; 2]>,
    /// How near the subject is: where the focus rests.
    subject: f32,
    /// The cover softened, for what is out of focus, and the colour of the
    /// haze in the distance: made from the cover the first time it is drawn.
    soft: std::sync::OnceLock<Soft>,
}

/// A cover, blurred twice over, and the colour of the air in front of it.
struct Soft {
    some: RgbImage,
    much: RgbImage,
    haze: [f32; 3],
}

impl Soft {
    fn of(img: &RgbImage) -> Self {
        let some = image::imageops::resize(img, 40, 40, FilterType::Triangle);
        let much = image::imageops::resize(&some, 16, 16, FilterType::Triangle);
        let mut mean = [0f32; 3];
        for p in much.pixels() {
            for c in 0..3 {
                mean[c] += p.0[c] as f32 / 256.0;
            }
        }
        // The cover's own colour, as a dim, slightly cool mist.
        let (h, s, _) = rgb_to_hsl((mean[0] as u8, mean[1] as u8, mean[2] as u8));
        let haze = hsl_to_rgb(h - 12.0, (s * 0.8).min(0.55), 0.2);
        Self { some, much, haze: [haze.0 as f32, haze.1 as f32, haze.2 as f32] }
    }
}

impl DepthMap {
    pub const SIDE: u32 = 252;
    /// How far anything moves, in map cells, and then some: the camera's
    /// lean, its swing to each bar's viewpoint, the dolly and the punch.
    const REACH: usize = 40;

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

        // Slopes measured three cells either way, which smooths over the
        // steps a map of whole numbers has.
        const GAP: usize = 3;
        let at = |x: usize, y: usize| near[y.min(side - 1) * side + x.min(side - 1)] as f32 / 255.0;
        let per_width = side as f32 / (2 * GAP) as f32;
        let slope = (0..side * side)
            .map(|i| {
                let (x, y) = (i % side, i / side);
                [
                    (at(x + GAP, y) - at(x.saturating_sub(GAP), y)) * per_width,
                    (at(x, y + GAP) - at(x, y.saturating_sub(GAP))) * per_width,
                ]
            })
            .collect();

        // The subject is whatever is nearest, give or take: the nearest
        // twentieth of the picture.
        let mut sorted = near.clone();
        sorted.sort_unstable();
        let subject = sorted[sorted.len() * 19 / 20] as f32 / 255.0;
        Some(Self { near, ceiling, slope, subject, soft: Default::default() })
    }

    #[cfg_attr(not(feature = "depth"), allow(dead_code))]
    pub fn from_bytes(near: Vec<u8>) -> Option<Self> {
        Self::new(near)
    }

    fn soft(&self, img: &RgbImage) -> &Soft {
        self.soft.get_or_init(|| Soft::of(img))
    }

    /// Map cell under a point, and how far between cells it is.
    fn cell(u: f32, v: f32) -> (usize, usize, usize, usize, f32, f32) {
        let side = Self::SIDE as f32;
        let x = ((u + 0.5) * side - 0.5).clamp(0.0, side - 1.0);
        let y = ((v + 0.5) * side - 0.5).clamp(0.0, side - 1.0);
        let (x0, y0) = (x as usize, y as usize);
        let (x1, y1) = ((x0 + 1).min(Self::SIDE as usize - 1), (y0 + 1).min(Self::SIDE as usize - 1));
        (x0, y0, x1, y1, x - x0 as f32, y - y0 as f32)
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
        let (x0, y0, x1, y1, fx, fy) = Self::cell(u, v);
        // Asked millions of times a second for a picture: kept to the bare sums.
        let p = |x: usize, y: usize| self.near[y * Self::SIDE as usize + x] as f32;
        let top = p(x0, y0) + (p(x1, y0) - p(x0, y0)) * fx;
        let bottom = p(x0, y1) + (p(x1, y1) - p(x0, y1)) * fx;
        (top + (bottom - top) * fy) * (1.0 / 255.0)
    }

    /// Which way the surface slopes at a point: see `slope`.
    fn slope_at(&self, u: f32, v: f32) -> [f32; 2] {
        let (x0, y0, x1, y1, fx, fy) = Self::cell(u, v);
        let p = |x: usize, y: usize| self.slope[y * Self::SIDE as usize + x];
        let (a, b, c, d) = (p(x0, y0), p(x1, y0), p(x0, y1), p(x1, y1));
        std::array::from_fn(|i| {
            let top = a[i] + (b[i] - a[i]) * fx;
            let bottom = c[i] + (d[i] - c[i]) * fx;
            top + (bottom - top) * fy
        })
    }
}

/// The viewpoints the camera swings between, one per bar: round the
/// cover, never twice the same in a row.
const VIEWS: [(f32, f32); 8] = [(-1.0, -0.5), (0.9, 0.4), (0.2, 1.0), (-0.7, 0.8), (1.0, -0.8), (-0.3, -1.0), (0.6, 0.0), (-1.0, 0.2)];

fn smooth(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Where a point of the picture looks onto the cover, and how near what it sees is.
#[derive(Clone, Copy, Default)]
struct Seen {
    su: f32,
    sv: f32,
    near: f32,
    /// How brightly the light falls there, and how much it catches an edge.
    /// These change smoothly, so they are worked out where `see` is.
    lit: f32,
    rim: f32,
}

/// The depth style's camera and lights at one instant.
///
/// The camera drifts in a slow loop, further when the bass is heavy, and
/// on every bar swings round to a new viewpoint, so near things slide
/// across far ones. Over eight bars it dollies in and out while holding
/// the subject's size, so the background breathes behind it. On each beat
/// whatever is nearest jumps towards the viewer.
///
/// A coloured light circles the cover and falls on it by the way each part
/// faces; on the beat it catches the edges. The focus rests on the subject
/// and pulls back to the distance at the end of each phrase. The distance
/// sits in a haze of the cover's own colour, and on the first beat of each
/// bar a line of light sweeps through the scene from front to back.
struct DepthView {
    lean_x: f32,
    lean_y: f32,
    punch: f32,
    dolly: f32,
    focus: f32,
    aperture: f32,
    light: [f32; 3],
    tint: [f32; 3],
    /// The sweep: how near the line of light is, and how bright.
    scan: (f32, f32),
    scan_tint: [f32; 3],
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
    /// How deep the scene is, against its width, for the light.
    const RELIEF: f32 = 0.3;

    /// `fine` is for a real picture, where a small movement shows; in
    /// half-blocks it has to be bigger to be seen at all.
    fn new(fx: Fx, fine: bool, depth: &DepthMap) -> Self {
        let lean = if fine { 0.028 + 0.02 * fx.bass } else { 0.045 + 0.03 * fx.bass };
        // Each bar's viewpoint, eased into across its first beat.
        let bar = fx.beats / 4;
        let view = |b: u64| VIEWS[(b.wrapping_mul(5) % VIEWS.len() as u64) as usize];
        let ease = if fx.beats % 4 == 0 { smooth(fx.through) } else { 1.0 };
        let (from, to) = (view(bar.wrapping_sub(1)), view(bar));
        let swing = if fine { 0.02 } else { 0.03 };
        let (vx, vy) = (from.0 + (to.0 - from.0) * ease, from.1 + (to.1 - from.1) * ease);
        // Counted continuously, beat by beat, for the moves that span bars.
        let count = fx.beats as f32 + fx.through;

        // The focus pulls back to the distance over the last bar of every four.
        let rack = match fx.beats % 16 {
            11 => smooth(fx.through),
            12..=14 => 1.0,
            15 => 1.0 - smooth(fx.through),
            _ => 0.0,
        };
        let focus = depth.subject + (0.08 - depth.subject) * rack;

        // The light: round the cover every eight seconds or so, a little
        // way each beat, and coloured to wheel slowly through the hues.
        let angle = fx.time * 0.45 + fx.beats as f32 * 0.35;
        let light = normal([angle.cos() * 0.75, angle.sin() * 0.55, 0.6]);
        let tint = rgb_f(hsl_to_rgb(fx.beats as f32 * 23.0 + fx.time * 9.0, 0.85, 0.62));
        let scan_tint = rgb_f(hsl_to_rgb(fx.beats as f32 * 23.0 + 160.0, 0.95, 0.64));
        // On the first beat of each bar, a line runs from the front to the back.
        let scan = if fx.beats % 4 == 0 { (0.05 + 0.95 * fx.beat, fx.beat) } else { (0.0, 0.0) };

        Self {
            lean_x: (fx.time * 0.55).sin() * lean + vx * swing,
            lean_y: (fx.time * 0.37).cos() * lean * 0.6 + vy * swing * 0.6,
            punch: 0.13 * fx.beat,
            dolly: 0.05 * (count / 32.0 * std::f32::consts::TAU).sin(),
            focus,
            aperture: if fine { 0.5 } else { 0.3 },
            light,
            tint,
            scan,
            scan_tint,
            fx,
            fine,
        }
    }

    /// Where on the cover a point of the picture a point this near would
    /// have been brought here from.
    fn from(&self, u: f32, v: f32, near: f32) -> (f32, f32) {
        let slide = (near - Self::STILL) * 2.0;
        // On the beat the front comes forward and the back falls away; the
        // dolly does the same, slowly, about the point in focus.
        let grow = 1.0 + self.punch * (near - 0.4) + self.dolly * (near - self.focus);
        ((u + self.lean_x * slide) / grow, (v + self.lean_y * slide) / grow)
    }

    /// What shows at a point of the picture (the centre is 0,0 and the edges
    /// are at half): where on the cover it comes from, and how near that is.
    fn see(&self, depth: &DepthMap, u: f32, v: f32) -> Seen {
        let (u, v) = (u / Self::MARGIN, v / Self::MARGIN);
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
            let (gu, gv) = self.from(u, v, guess);
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
                        (su, sv) = self.from(u, v, near);
                    }
                    None => (su, sv, near) = (gu, gv, guess),
                }
                break;
            }
            before = Some((guess, short));
            trail = [if layer == first { (gu, gv) } else { trail[1] }, (gu, gv)];
            (su, sv, near) = (gu, gv, guess);
        }
        self.lighting(depth, Seen { su, sv, near, lit: 1.0, rim: 0.0 })
    }

    /// How the light falls where `seen` looks: brighter where the surface
    /// turns towards it, darker where it turns away, as it was where it faces
    /// straight out. Edges met side-on catch it, more so on the beat.
    fn lighting(&self, depth: &DepthMap, seen: Seen) -> Seen {
        let [gx, gy] = depth.slope_at(seen.su, seen.sv);
        let n = normal([-gx * Self::RELIEF, -gy * Self::RELIEF, 1.0]);
        let facing = n[0] * self.light[0] + n[1] * self.light[1] + n[2] * self.light[2];
        let lit = (1.0 + 0.7 * (facing - self.light[2])).clamp(0.6, 1.4);
        let side = 1.0 - n[2];
        let rim = side * side.sqrt() * (0.04 + 0.5 * self.fx.beat) * 110.0;
        Seen { lit, rim, ..seen }
    }

    /// The colour of the picture at a point, read off `img`.
    #[cfg(test)]
    fn colour(&self, img: &RgbImage, depth: &DepthMap, u: f32, v: f32) -> Rgb {
        self.shade(img, depth, self.see(depth, u, v))
    }

    /// The colour of what is seen: the cover there, in or out of focus, lit
    /// by the light going round, hazed by distance, and caught by the sweep.
    fn shade(&self, img: &RgbImage, depth: &DepthMap, seen: Seen) -> Rgb {
        let Seen { su, sv, near, lit, rim } = seen;
        let fx = &self.fx;
        let soft = depth.soft(img);

        // Never so far gone the subject dissolves.
        let blur = (self.aperture * (near - self.focus).abs() * 1.8).min(0.6);
        let c = if blur <= 0.02 {
            read(img, su, sv)
        } else if blur < 0.5 {
            lerp3(read(img, su, sv), read(&soft.some, su, sv), blur * 2.0)
        } else {
            lerp3(read(&soft.some, su, sv), read(&soft.much, su, sv), blur * 2.0 - 1.0)
        };

        // The far distance sits back a little; the beat lights what is close.
        let gain = (0.84 + 0.16 * near + fx.beat * 0.28 * near * near + 0.06 * fx.bass) * lit;
        let fog = (1.0 - near) * (1.0 - near) * (0.28 + 0.1 * fx.bass);
        let (centre, strength) = self.scan;
        let sweep = if strength > 0.01 { strength * (-((near - centre) / 0.025).powi(2)).exp() * 60.0 } else { 0.0 };
        let tone = |i: usize| {
            let v = c[i] * gain + self.tint[i] * rim / 255.0;
            let v = v + (soft.haze[i] - v) * fog + self.scan_tint[i] * sweep / 255.0;
            v.clamp(0.0, 255.0) as u8
        };
        (tone(0), tone(1), tone(2))
    }

    /// Motes of dust drifting through the scene at every depth. Each is
    /// hidden by whatever stands in front of it, and those far from the
    /// focus spread into soft discs. Added to `out`, `w` by `h` RGB, whose
    /// nearness at each pixel is `nearness`.
    fn motes(&self, out: &mut [u8], nearness: &[f32], w: usize, h: usize) {
        let fx = &self.fx;
        if fx.motes <= 0.0 {
            return;
        }
        let count = (40.0 * fx.motes) as usize;
        let bright = 0.35 + 0.5 * fx.highs + 0.45 * fx.beat;
        let unit = w.min(h) as f32;
        for i in 0..count {
            let hash = |k: u32| {
                let mut h = (i as u32).wrapping_mul(0x9E37_79B9) ^ k.wrapping_mul(0x85EB_CA6B);
                h ^= h >> 16;
                h = h.wrapping_mul(0x7FEB_352D);
                h ^= h >> 15;
                h = h.wrapping_mul(0x846C_A68B);
                h ^= h >> 16;
                (h >> 8) as f32 / 16_777_216.0
            };
            let z = 0.12 + 0.88 * hash(1);
            // Rising slowly, nearer ones faster, swaying as they go.
            let rise = fx.time * (0.012 + 0.03 * z);
            let my = (hash(2) - rise).rem_euclid(1.2) - 0.6;
            let mx = hash(3) - 0.5 + (fx.time * 0.4 + i as f32).sin() * 0.03;
            // Where it shows: the camera's move, run the other way.
            let slide = (z - Self::STILL) * 2.0;
            let grow = 1.0 + self.punch * (z - 0.4) + self.dolly * (z - self.focus);
            let (u, v) = ((mx * grow - self.lean_x * slide) * Self::MARGIN, (my * grow - self.lean_y * slide) * Self::MARGIN);
            let (cx, cy) = ((u + 0.5) * w as f32, (v + 0.5) * h as f32);
            let spread = (self.aperture * (z - self.focus).abs() * 2.5).min(1.0);
            let radius = ((0.0015 + 0.0055 * z) * unit * (1.0 + 3.0 * spread)).max(0.6);
            let glow = bright * (1.0 - 0.6 * spread) * 180.0;
            let tint = lerp3([255.0, 244.0, 222.0], self.tint, 0.35);
            let (x0, x1) = ((cx - radius).floor().max(0.0) as usize, ((cx + radius).ceil().max(0.0) as usize).min(w));
            let (y0, y1) = ((cy - radius).floor().max(0.0) as usize, ((cy + radius).ceil().max(0.0) as usize).min(h));
            for y in y0..y1 {
                for x in x0..x1 {
                    // Behind the surface here: hidden.
                    if nearness[y * w + x] > z + 0.02 {
                        continue;
                    }
                    let d = (((x as f32 + 0.5 - cx) / radius).powi(2) + ((y as f32 + 0.5 - cy) / radius).powi(2)).min(1.0);
                    let fall = (1.0 - d).powi(2) * glow / 255.0;
                    let px = &mut out[(y * w + x) * 3..][..3];
                    for c in 0..3 {
                        px[c] = (px[c] as f32 + tint[c] * fall).min(255.0) as u8;
                    }
                }
            }
        }
    }
}

fn rgb_f(c: Rgb) -> [f32; 3] {
    [c.0 as f32, c.1 as f32, c.2 as f32]
}

fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t)
}

fn normal(v: [f32; 3]) -> [f32; 3] {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-6);
    v.map(|c| c / len)
}

/// The cover's colour at a point of it (the centre is 0,0), between pixels.
fn read(img: &RgbImage, su: f32, sv: f32) -> [f32; 3] {
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
    std::array::from_fn(|i| {
        let top = a[i] as f32 * (1.0 - tx) + b[i] as f32 * tx;
        let bottom = c[i] as f32 * (1.0 - tx) + d[i] as f32 * tx;
        top * (1.0 - ty) + bottom * ty
    })
}

/// Draw the cover with depth as half-blocks: two pixels to a character.
pub fn render_depth(img: &RgbImage, depth: &DepthMap, w: u16, h: u16, fx: Fx) -> Rendered {
    let (cw, ch) = (w.max(1) as u32, h.max(1) as u32);
    let px = draw_depth(img, depth, cw as usize, ch as usize * 2, fx, false);
    let at = |x: u32, y: u32| {
        let p = &px[((y * cw + x) * 3) as usize..][..3];
        (p[0], p[1], p[2])
    };
    let mut cells = Vec::with_capacity((cw * ch) as usize);
    for y in 0..ch {
        for x in 0..cw {
            cells.push(Cell { ch: '▀', fg: at(x, y * 2), bg: Some(at(x, y * 2 + 1)) });
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
/// terminals that can show one.
pub fn render_depth_rgb(img: &RgbImage, depth: &DepthMap, w: u32, h: u32, fx: Fx) -> Vec<u8> {
    draw_depth(img, depth, w.max(1) as usize, h.max(1) as usize, fx, true)
}

/// Draw the cover with depth, `w` by `h` pixels of RGB. A real picture is
/// thousands of times the pixels of the half-block version, so two things
/// keep it quick: the rows are shared out between threads, and what each
/// point sees (the slow part) is worked out for every other pixel each way
/// and filled in between. The colours themselves are read for every pixel,
/// so nothing gets blurred that shouldn't be.
fn draw_depth(img: &RgbImage, depth: &DepthMap, w: usize, h: usize, fx: Fx, fine: bool) -> Vec<u8> {
    let every = if fine { 2 } else { 1 };
    let view = DepthView::new(fx, fine, depth);
    // Made before the threads start, so they all find it ready.
    depth.soft(img);

    // What is seen, on a grid: one more point than needed each way, so
    // every pixel sits between four of them.
    let (gw, gh) = ((w - 1) / every + 2, (h - 1) / every + 2);
    let mut grid = vec![Seen::default(); gw * gh];
    share_rows(grid.chunks_mut(gw).collect(), |gy, row| {
        let v = ((gy * every) as f32 + 0.5) / h as f32 - 0.5;
        for (gx, point) in row.iter_mut().enumerate() {
            *point = view.see(depth, ((gx * every) as f32 + 0.5) / w as f32 - 0.5, v);
        }
    });

    let mut out = vec![0u8; w * h * 3];
    let mut nearness = vec![0f32; w * h];
    let grid = &grid;
    let rows: Vec<(&mut [u8], &mut [f32])> = out.chunks_mut(w * 3).zip(nearness.chunks_mut(w)).collect();
    share_pairs(rows, |y, (row, near_row)| {
        let (gy, ty) = (y / every, (y % every) as f32 / every as f32);
        for (x, pixel) in row.chunks_exact_mut(3).enumerate() {
            let (gx, tx) = (x / every, (x % every) as f32 / every as f32);
            let (a, b) = (grid[gy * gw + gx], grid[gy * gw + gx + 1]);
            let (c, d) = (grid[(gy + 1) * gw + gx], grid[(gy + 1) * gw + gx + 1]);
            let mix = |a: f32, b: f32, c: f32, d: f32| {
                let (top, bottom) = (a + (b - a) * tx, c + (d - c) * tx);
                top + (bottom - top) * ty
            };
            let seen = Seen {
                su: mix(a.su, b.su, c.su, d.su),
                sv: mix(a.sv, b.sv, c.sv, d.sv),
                near: mix(a.near, b.near, c.near, d.near),
                lit: mix(a.lit, b.lit, c.lit, d.lit),
                rim: mix(a.rim, b.rim, c.rim, d.rim),
            };
            near_row[x] = seen.near;
            let rgb = view.shade(img, depth, seen);
            pixel.copy_from_slice(&[rgb.0, rgb.1, rgb.2]);
        }
    });
    view.motes(&mut out, &nearness, w, h);
    out
}

/// `share_rows` for rows that come in pairs: a row of colour and its row of nearness.
fn share_pairs(rows: Vec<(&mut [u8], &mut [f32])>, work: impl Fn(usize, (&mut [u8], &mut [f32])) + Sync) {
    let threads = std::thread::available_parallelism().map_or(2, |n| n.get()).clamp(1, 6);
    let mut hands: Vec<Vec<(usize, (&mut [u8], &mut [f32]))>> = (0..threads).map(|_| Vec::new()).collect();
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
        let hit = render_pulse(&img, 16, 8, Fx { beat: 1.0, bass: 0.5, time: 1.0, ..Default::default() });
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
        let fx = Fx { time: 2.0, beat: 0.5, bass: 0.3, beats: 13, through: 0.4, ..Default::default() };
        let (w, h) = (40usize, 37usize);
        let picture = render_depth_rgb(&cover, &depth, w as u32, h as u32, fx);
        assert_eq!(picture.len(), w * h * 3);
        // Every other pixel each way is worked out in full; check those.
        let view = DepthView::new(fx, true, &depth);
        for y in (0..h).step_by(2) {
            for x in (0..w).step_by(2) {
                let want = view.colour(&cover, &depth, (x as f32 + 0.5) / w as f32 - 0.5, (y as f32 + 0.5) / h as f32 - 0.5);
                let got = &picture[(y * w + x) * 3..][..3];
                assert_eq!((got[0], got[1], got[2]), want, "at {x},{y}");
            }
        }
    }

    #[test]
    fn depth_swings_to_a_new_viewpoint_each_bar() {
        let (cover, depth) = square_in_front();
        // Same instant of the drift, a bar apart, each well settled into its bar.
        let at = |beats| render_depth(&cover, &depth, 64, 32, Fx { beats, through: 1.0, ..Default::default() });
        let (one, two) = (at(5), at(9));
        let (a, b) = (lit(&one, 16), lit(&two, 16));
        assert!(!a.is_empty() && !b.is_empty());
        // Within a bar it holds still (the light moves on a notch each beat,
        // which can tip an edge cell over the threshold).
        let (c, d) = (lit(&at(5), 16), lit(&at(6), 16));
        assert!(c[0].abs_diff(d[0]) <= 1 && c.last().unwrap().abs_diff(*d.last().unwrap()) <= 1, "{c:?} {d:?}");
        assert!(a[0].abs_diff(b[0]) >= 2, "the bars' viewpoints are only {} cells apart", a[0].abs_diff(b[0]));
    }

    #[test]
    fn the_light_falls_on_the_side_facing_it() {
        // A dome, lit evenly grey: any difference between its sides is the light.
        let cover = RgbImage::from_pixel(96, 96, image::Rgb([150, 150, 150]));
        let side = DepthMap::SIDE as f32;
        let near = (0..DepthMap::SIDE * DepthMap::SIDE)
            .map(|i| {
                let (x, y) = ((i % DepthMap::SIDE) as f32 / side - 0.5, (i / DepthMap::SIDE) as f32 / side - 0.5);
                (255.0 * (1.0 - (x * x + y * y).sqrt() * 2.2).max(0.0)) as u8
            })
            .collect();
        let depth = DepthMap::new(near).unwrap();
        // At time 0 the light is off to the right.
        let frame = render_depth(&cover, &depth, 40, 20, Fx::default());
        let at = |x: usize| frame.cells[10 * 40 + x].fg.0;
        assert!(at(26) > at(14) + 10, "right {} vs left {}", at(26), at(14));
    }

    #[test]
    fn focus_pulls_back_at_the_end_of_a_phrase() {
        // A fine checkerboard far away, the square near.
        let inside = |x: u32, y: u32| (36..60).contains(&x) && (36..60).contains(&y);
        let cover = RgbImage::from_fn(96, 96, |x, y| {
            image::Rgb(if inside(x, y) { [250; 3] } else if (x / 3 + y / 3) % 2 == 0 { [200; 3] } else { [40; 3] })
        });
        let side = DepthMap::SIDE;
        let near = (0..side * side).map(|i| if inside(i % side * 96 / side, i / side * 96 / side) { 255 } else { 0 }).collect();
        let depth = DepthMap::new(near).unwrap();
        // How much the background's pixels differ from their neighbours: its sharpness.
        let sharpness = |beats| {
            let px = render_depth_rgb(&cover, &depth, 192, 192, Fx { beats, through: 0.5, ..Default::default() });
            (2..40).map(|x| (px[(20 * 192 + x) * 3] as i32 - px[(20 * 192 + x + 1) * 3] as i32).unsigned_abs()).sum::<u32>()
        };
        assert!(sharpness(13) > sharpness(1) * 2, "{} vs {}", sharpness(13), sharpness(1));
    }

    #[test]
    fn motes_hide_behind_what_is_in_front_of_them() {
        let cover = RgbImage::from_pixel(64, 64, image::Rgb([60, 60, 60]));
        let changed = |nearness: u8| {
            let depth = DepthMap::new(vec![nearness; (DepthMap::SIDE * DepthMap::SIDE) as usize]).unwrap();
            let fx = Fx { time: 3.0, highs: 1.0, ..Default::default() };
            let plain = render_depth_rgb(&cover, &depth, 200, 200, fx);
            let dusty = render_depth_rgb(&cover, &depth, 200, 200, Fx { motes: 1.0, ..fx });
            plain.iter().zip(&dusty).filter(|(a, b)| a != b).count()
        };
        let (open, walled) = (changed(0), changed(255));
        assert!(open > 300, "only {open} values changed with nothing in the way");
        assert!(walled * 10 < open, "{walled} changed behind a wall, {open} in the open");
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
