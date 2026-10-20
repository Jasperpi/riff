//! Real pictures, for terminals that can show them: those that speak the
//! kitty graphics protocol (kitty, Konsole, Ghostty, WezTerm). A picture is
//! sent packed and scaled by the terminal to cover a block of cells; it sits
//! on top of whatever text is there.
//!
//! The protocol's packed format is PNG. Some terminals (Konsole) hand the
//! data to a general picture loader that takes JPEG as well, and a JPEG of a
//! cover is a quarter the size. A terminal reads its input far more slowly
//! than it unpacks a picture, so for a cover that moves, size is what
//! counts: the terminal is asked which it accepts and given the smaller.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use ratatui::layout::Rect;

/// Pictures are numbered so a new frame replaces the one before it. The
/// numbers only have to be riff's own within this terminal.
const FIRST_ID: u32 = 7_433;

/// Whether this terminal has been found to draw pictures.
static FOUND: AtomicBool = AtomicBool::new(false);

/// A terminal that draws pictures.
pub struct Screen {
    /// The size of one character cell, in the terminal's pixels.
    pub cell: (u16, u16),
    /// It takes a JPEG where the protocol says PNG.
    jpeg: bool,
    /// How many pictures are up.
    shown: usize,
    /// Running totals for the log: frames, time drawing them, time sending them.
    sent: (u32, Duration, Duration),
    /// What each slot is known to be showing, so a picture that hasn't
    /// changed need not be sent again.
    keys: Vec<Option<u64>>,
}

/// A terminal that draws pictures, with characters `cell` pixels in size,
/// that isn't there: for `--snapshot`, which saves the pictures to files.
pub fn pretend(cell: (u16, u16)) -> Screen {
    Screen { cell, jpeg: false, shown: 0, sent: (0, Duration::ZERO, Duration::ZERO), keys: Vec::new() }
}

/// Ask the terminal whether it draws pictures. For raw mode, before anything
/// else is reading the keyboard: the answer comes back as input.
pub fn detect() -> Option<Screen> {
    // A one-pixel picture that is checked but not shown, then a question
    // every terminal answers, so there is an end to wait for either way.
    if !ask(b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\").windows(9).any(|w| w == b"_Gi=31;OK") {
        return None;
    }
    // And whether a JPEG will do where a PNG is expected.
    let mut tiny = Vec::new();
    let jpeg = image::codecs::jpeg::JpegEncoder::new(&mut tiny)
        .encode(&[128u8; 8 * 8 * 3], 8, 8, image::ExtendedColorType::Rgb8)
        .is_ok()
        && {
            let mut question = b"\x1b_Gi=32,a=q,f=100;".to_vec();
            question.extend(base64::engine::general_purpose::STANDARD.encode(&tiny).bytes());
            question.extend(b"\x1b\\");
            ask(&question).windows(9).any(|w| w == b"_Gi=32;OK")
        };
    let mut screen = Screen { cell: (0, 0), jpeg, shown: 0, sent: (0, Duration::ZERO, Duration::ZERO), keys: Vec::new() };
    if !screen.measure() {
        return None;
    }
    FOUND.store(true, Ordering::Relaxed);
    Some(screen)
}

/// Send the terminal a question and gather what it says back, up to its
/// answer to a second question that every terminal answers.
fn ask(question: &[u8]) -> Vec<u8> {
    let mut reply = Vec::new();
    let mut out = std::io::stdout();
    if out.write_all(question).and_then(|_| out.write_all(b"\x1b[c")).and_then(|_| out.flush()).is_err() {
        return reply;
    }
    let started = Instant::now();
    let patience = Duration::from_millis(400);
    while started.elapsed() < patience {
        let mut fd = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
        let wait = patience.saturating_sub(started.elapsed()).as_millis() as i32;
        // SAFETY: one valid pollfd, and a buffer of the length given.
        if unsafe { libc::poll(&mut fd, 1, wait) } <= 0 {
            break;
        }
        let mut buf = [0u8; 256];
        let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            break;
        }
        reply.extend_from_slice(&buf[..n as usize]);
        // The second answer looks like ESC [ ? … c.
        if let Some(at) = reply.windows(3).position(|w| w == b"\x1b[?") {
            if reply[at..].contains(&b'c') {
                break;
            }
        }
    }
    reply
}

impl Screen {
    /// Read the size of a character cell again (the window or its font changed).
    /// False if the terminal won't say, in which case pictures can't be placed.
    pub fn measure(&mut self) -> bool {
        // Pictures are sized in pixels: all of them want sending again.
        self.keys.clear();
        match ratatui::crossterm::terminal::window_size() {
            Ok(size) if size.width > 0 && size.height > 0 && size.columns > 0 && size.rows > 0 => {
                self.cell = (size.width / size.columns, size.height / size.rows);
                self.cell.0 > 0 && self.cell.1 > 0
            }
            _ => false,
        }
    }

    /// What slot `slot` was last marked as showing.
    pub fn showing(&self, slot: usize) -> Option<u64> {
        self.keys.get(slot).copied().flatten()
    }

    /// Note what slot `slot` now shows, for `showing`.
    pub fn mark(&mut self, slot: usize, key: u64) {
        if self.keys.len() <= slot {
            self.keys.resize(slot + 1, None);
        }
        self.keys[slot] = Some(key);
    }

    /// Put up picture number `slot`: `rgb` is `w` by `h` pixels, and the
    /// terminal scales it to cover `cells` exactly. One already up in that
    /// slot is replaced. `above` puts it over any others it overlaps.
    pub fn show(&mut self, slot: usize, cells: Rect, w: u32, h: u32, rgb: &[u8], drawing: Duration, above: bool) -> std::io::Result<()> {
        use image::ImageEncoder;
        use image::codecs::jpeg::JpegEncoder;
        use image::codecs::png::{CompressionType, FilterType, PngEncoder};
        let started = Instant::now();
        let mut packed = Vec::with_capacity(rgb.len() / 4);
        if self.jpeg {
            JpegEncoder::new_with_quality(&mut packed, 90)
                .encode(rgb, w, h, image::ExtendedColorType::Rgb8)
                .map_err(std::io::Error::other)?;
        } else {
            // Packed quickly rather than tightly: there is another along in a moment.
            PngEncoder::new_with_quality(&mut packed, CompressionType::Fast, FilterType::Sub)
                .write_image(rgb, w, h, image::ExtendedColorType::Rgb8)
                .map_err(std::io::Error::other)?;
        }
        let mut out = std::io::stdout().lock();
        if let Some(key) = self.keys.get_mut(slot) {
            *key = None;
        }
        out.write_all(&command(slot, cells, &packed, above))?;
        out.flush()?;
        self.shown = self.shown.max(slot + 1);

        // Now and then, note in the log what this is costing.
        self.sent = (self.sent.0 + 1, self.sent.1 + drawing, self.sent.2 + started.elapsed());
        if self.sent.0 == 200 {
            let per = |total: Duration| total.as_secs_f32() * 1000.0 / 200.0;
            log::info!(
                "pictures: {w}x{h} as {}, {:.1} ms to draw and {:.1} ms to pack and send, each",
                if self.jpeg { "JPEG" } else { "PNG" },
                per(self.sent.1),
                per(self.sent.2)
            );
            self.sent = (0, Duration::ZERO, Duration::ZERO);
        }
        Ok(())
    }

    /// Take down every picture after the first `keep`.
    pub fn keep(&mut self, keep: usize) {
        if self.shown <= keep {
            return;
        }
        let mut out = std::io::stdout().lock();
        for slot in keep..self.shown {
            let _ = write!(out, "\x1b_Ga=d,d=I,i={},q=2\x1b\\", FIRST_ID + slot as u32);
        }
        let _ = out.flush();
        self.shown = keep;
        self.keys.truncate(keep);
    }
}

/// What to send the terminal to put a packed picture in `slot`, over `cells`.
fn command(slot: usize, cells: Rect, packed: &[u8], above: bool) -> Vec<u8> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(packed);
    let mut out = Vec::with_capacity(encoded.len() + encoded.len() / 256 + 96);
    let _ = write!(out, "\x1b[{};{}H", cells.y + 1, cells.x + 1);
    let id = FIRST_ID + slot as u32;
    // In pieces: terminals put a limit on the length of one command.
    let mut pieces = encoded.as_bytes().chunks(4096).peekable();
    let mut first = true;
    while let Some(piece) = pieces.next() {
        let more = pieces.peek().is_some() as u8;
        if std::mem::take(&mut first) {
            // Shown as soon as it arrives (a=T), without moving the cursor
            // (C=1) and without an answer coming back as input (q=2).
            let z = if above { ",z=1" } else { "" };
            let _ = write!(out, "\x1b_Ga=T,q=2,C=1,f=100,i={id},p=1,c={},r={}{z},m={more};", cells.width, cells.height);
        } else {
            let _ = write!(out, "\x1b_Gm={more};");
        }
        out.extend_from_slice(piece);
        out.extend_from_slice(b"\x1b\\");
    }
    out
}

/// Take down everything riff has put up, on the way out.
pub fn clear() {
    if !FOUND.load(Ordering::Relaxed) {
        return;
    }
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(b"\x1b_Ga=d,d=A,q=2\x1b\\");
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_picture_goes_out_whole_in_pieces_the_terminal_accepts() {
        // Big enough to need several pieces, and not a multiple of anything.
        let packed: Vec<u8> = (0..20_011u32).map(|i| (i * 31 % 251) as u8).collect();
        let sent = command(1, Rect { x: 4, y: 2, width: 36, height: 16 }, &packed, false);
        let text = String::from_utf8(sent).unwrap();

        // The cursor goes to the top-left cell first (counted from one).
        let (place, rest) = text.split_once("\x1b_G").unwrap();
        assert_eq!(place, "\x1b[3;5H");
        let pieces: Vec<&str> = rest.split("\x1b_G").map(|p| p.strip_suffix("\x1b\\").unwrap()).collect();
        assert!(pieces.len() > 1);

        let mut payload = String::new();
        for (n, piece) in pieces.iter().enumerate() {
            let (keys, data) = piece.split_once(';').unwrap();
            assert!(data.len() <= 4096);
            // Every piece but the last says more is coming.
            let more = if n + 1 < pieces.len() { "m=1" } else { "m=0" };
            assert!(keys.ends_with(more), "piece {n} has {keys}");
            if n == 0 {
                assert!(keys.contains("a=T") && keys.contains("c=36,r=16") && keys.contains(&format!("i={}", FIRST_ID + 1)));
            } else {
                assert_eq!(keys, more);
            }
            payload.push_str(data);
        }
        assert_eq!(base64::engine::general_purpose::STANDARD.decode(payload).unwrap(), packed);
    }
}
