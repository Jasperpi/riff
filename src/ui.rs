//! Everything drawn on screen. Pure function of `App`, apart from recording
//! where clickable things ended up and caching rendered artwork.

use std::borrow::Cow;
use std::sync::Arc;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, Focus, Hit, LyricState, Overlay, Page, PixelJob, Screen};
use crate::art::{self, ArtMode, Rgb};
use crate::engine::Conn;
use crate::model::*;
use crate::audio::BANDS;

struct Theme {
    bg: Rgb,
    panel: Rgb,
    text: Color,
    dim: Color,
    faint: Color,
    accent: Color,
    accent_rgb: Rgb,
    sel: Color,
    sel_idle: Color,
    error: Color,
}

fn rgb(c: Rgb) -> Color {
    Color::Rgb(c.0, c.1, c.2)
}

fn theme(app: &App) -> Theme {
    let p = app.palette;
    let bg = art::mix((11, 12, 12), p.shade, 0.8);
    Theme {
        bg,
        panel: art::mix(bg, (255, 255, 255), 0.045),
        text: Color::Rgb(233, 236, 234),
        dim: Color::Rgb(152, 158, 154),
        faint: Color::Rgb(86, 92, 88),
        accent: rgb(p.accent),
        accent_rgb: p.accent,
        sel: rgb(art::mix(bg, p.accent, 0.24)),
        sel_idle: rgb(art::mix(bg, (255, 255, 255), 0.075)),
        error: Color::Rgb(255, 122, 110),
    }
}

// ---- drawing primitives ---------------------------------------------------

/// Truncate to `max` terminal cells, ending in an ellipsis when cut.
fn fit(s: &str, max: usize) -> Cow<'_, str> {
    let clean = !s.chars().any(char::is_control);
    if clean && s.width() <= max {
        return Cow::Borrowed(s);
    }
    let mut out = String::new();
    let mut used = 0;
    let total: usize = s.chars().map(|c| if c.is_control() { 1 } else { c.width().unwrap_or(0) }).sum();
    let budget = if total > max { max.saturating_sub(1) } else { max };
    for ch in s.chars() {
        let ch = if ch.is_control() { ' ' } else { ch };
        let w = ch.width().unwrap_or(0);
        if used + w > budget {
            break;
        }
        out.push(ch);
        used += w;
    }
    if total > max && max > 0 {
        out.push('…');
    }
    Cow::Owned(out)
}

/// Draw text clipped to `max` cells. Returns the cells used.
fn put(buf: &mut Buffer, x: u16, y: u16, max: u16, s: &str, style: Style) -> u16 {
    let area = buf.area;
    if max == 0 || y < area.y || y >= area.bottom() || x < area.x || x >= area.right() {
        return 0;
    }
    let max = max.min(area.right() - x);
    let t = fit(s, max as usize);
    buf.set_stringn(x, y, t.as_ref(), max as usize, style);
    t.width() as u16
}

/// Draw text so that it ends just before column `right`.
fn put_right(buf: &mut Buffer, right: u16, y: u16, max: u16, s: &str, style: Style) -> u16 {
    let t = fit(s, max as usize);
    let w = t.width() as u16;
    put(buf, right.saturating_sub(w), y, w, t.as_ref(), style);
    w
}

fn put_centered(buf: &mut Buffer, area: Rect, y: u16, s: &str, style: Style) {
    let t = fit(s, area.width as usize);
    let w = t.width() as u16;
    put(buf, area.x + (area.width.saturating_sub(w)) / 2, y, w, t.as_ref(), style);
}

fn fill(buf: &mut Buffer, area: Rect, style: Style) {
    buf.set_style(area.intersection(buf.area), style);
}

fn set(buf: &mut Buffer, x: u16, y: u16, ch: char, style: Style) {
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.set_char(ch).set_style(style);
    }
}

fn spinner(frame: u64) -> char {
    const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    FRAMES[(frame / 3 % 10) as usize]
}

fn bar_glyph(v: f32) -> char {
    const LEVELS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    LEVELS[(v.clamp(0.0, 1.0) * 8.0).round() as usize]
}

/// Paint cover art into `area`, fetching the image if it isn't loaded yet.
fn draw_art(buf: &mut Buffer, app: &mut App, th: &Theme, area: Rect, url: Option<&str>) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let image = url.and_then(|u| app.images.get(u).cloned());
    let (Some(url), Some(image)) = (url, image) else {
        if let Some(u) = url {
            let u = u.to_string();
            app.want_image(&u);
        }
        // Placeholder: a quiet tile with a note in the middle.
        fill(buf, area, Style::default().bg(rgb(art::mix(th.bg, (255, 255, 255), 0.07))));
        set(
            buf,
            area.x + area.width / 2,
            area.y + area.height / 2,
            '♪',
            Style::default().fg(th.faint),
        );
        return;
    };
    app.art_rects.push(area);
    let mode = app.art_mode;
    let rendered = if mode == ArtMode::Pulse {
        // Redrawn every frame from a small copy, shaped by the music right now.
        let source = app.thumbs.get(url).cloned().unwrap_or(image);
        Arc::new(art::render_pulse(&source, area.width, area.height, app.art_fx()))
    } else if let (ArtMode::Depth, true, Some(depth)) = (mode, area.width >= 12, app.depths.get(url).cloned()) {
        // Likewise redrawn every frame. Covers too small to show it stay still.
        // A pop-up would be hidden behind a picture, so those wait for it to close.
        let clear = matches!(app.overlay, Overlay::None | Overlay::Filter);
        if let (Some(screen), true) = (&app.pixels, clear && app.cfg.pictures) {
            // As a real picture: the main loop sends it once the characters
            // are drawn. The cells under it are left empty. A cover is square,
            // which in cells depends on the shape of this terminal's font.
            let (cw, ch) = (screen.cell.0 as f32, screen.cell.1 as f32);
            let side = (area.width as f32 * cw).min(area.height as f32 * ch);
            let cols = ((side / cw).round() as u16).clamp(1, area.width);
            let rows = ((side / ch).round() as u16).clamp(1, area.height);
            let cells = Rect {
                x: area.x + (area.width - cols) / 2,
                y: area.y + (area.height - rows) / 2,
                width: cols,
                height: rows,
            };
            for y in area.y..area.bottom() {
                for x in area.x..area.right() {
                    set(buf, x, y, ' ', Style::default().bg(rgb(th.bg)));
                }
            }
            // The largest version of the cover is worth having at this size.
            if let Some(larger) = art::larger(url) {
                app.want_image(&larger);
            }
            app.pixel_jobs.push((cells, PixelJob::Depth(url.to_string())));
            return;
        }
        let source = app.thumbs.get(url).cloned().unwrap_or(image);
        Arc::new(art::render_depth(&source, &depth, area.width, area.height, app.art_fx()))
    } else {
        if mode == ArtMode::Depth && area.width >= 12 {
            // Shown plain until its depth has been worked out.
            let wanted = url.to_string();
            app.want_depth(&wanted);
        }
        let key = (url.to_string(), area.width, area.height, mode);
        match app.art_cache.get(&key) {
            Some(r) => r.clone(),
            None => {
                let r = Arc::new(art::render(&image, area.width, area.height, mode));
                if app.art_cache.len() > 48 {
                    app.art_cache.clear();
                }
                app.art_cache.insert(key, r.clone());
                r
            }
        }
    };
    for row in 0..rendered.h {
        for col in 0..rendered.w {
            let c = rendered.cells[(row as usize) * rendered.w as usize + col as usize];
            if let Some(cell) = buf.cell_mut((area.x + col, area.y + row)) {
                cell.set_char(c.ch).set_fg(rgb(c.fg));
                if let Some(bg) = c.bg {
                    cell.set_bg(rgb(bg));
                }
            }
        }
    }
}

/// Spectrum bars filling `area`, brighter towards the top.
fn draw_spectrum(buf: &mut Buffer, app: &App, th: &Theme, area: Rect, gap: u16) {
    if area.width < 4 || area.height == 0 {
        return;
    }
    let step = 1 + gap;
    let count = ((area.width + gap) / step).min(BANDS as u16 * 2) as usize;
    let used = count as u16 * step - gap;
    let x0 = area.x + (area.width - used) / 2;
    for i in 0..count {
        // Resample the analyser's bands across however many bars fit.
        let pos = i as f32 / count.max(1) as f32 * BANDS as f32;
        let a = pos.floor() as usize;
        let b = (a + 1).min(BANDS - 1);
        let v = app.bars[a.min(BANDS - 1)] * (1.0 - pos.fract()) + app.bars[b] * pos.fract();
        let cells = v * area.height as f32;
        for row in 0..area.height {
            let level = cells - (area.height - 1 - row) as f32;
            if level <= 0.0 {
                continue;
            }
            let height_frac = 1.0 - row as f32 / area.height.max(1) as f32;
            let color = art::mix(
                art::mix(th.bg, th.accent_rgb, 0.45),
                art::mix(th.accent_rgb, (255, 255, 255), 0.25),
                height_frac,
            );
            set(buf, x0 + i as u16 * step, area.y + row, bar_glyph(level), Style::default().fg(rgb(color)));
        }
    }
}

// ---- top level ------------------------------------------------------------

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let th = theme(app);
    app.hits.clear();
    app.art_rects.clear();
    app.pixel_jobs.clear();
    app.drawn_squares.clear();
    let buf = f.buffer_mut();
    fill(buf, area, Style::default().bg(rgb(th.bg)).fg(th.text));

    if area.width < 36 || area.height < 10 {
        put_centered(buf, area, area.y + area.height / 2, "riff needs a bigger window", Style::default().fg(th.dim));
        return;
    }

    // The squares take the whole screen, bar the line of key hints.
    if app.screen == Screen::Quad {
        let hints = Rect { y: area.bottom() - 1, height: 1, ..area };
        draw_quad(buf, app, &th, Rect { height: area.height - 1, ..area });
        draw_hints(buf, app, &th, hints);
        draw_overlay(buf, app, &th, area);
        return;
    }

    let bar_h: u16 = if area.height >= 18 { 5 } else { 2 };
    let body = Rect { height: area.height - bar_h - 1, ..area };
    let bar = Rect { y: body.bottom(), height: bar_h, ..area };
    let hints = Rect { y: bar.bottom(), height: 1, ..area };

    match app.screen {
        Screen::Browse => draw_browse(buf, app, &th, body),
        Screen::NowPlaying => draw_now_playing(buf, app, &th, body),
        Screen::Decks => draw_decks(buf, app, &th, body),
        Screen::Quad => {}
    }
    if app.quad_on() {
        draw_quad_bar(buf, app, &th, bar);
    } else if app.decks_on() {
        draw_decks_bar(buf, app, &th, bar);
    } else {
        draw_player(buf, app, &th, bar);
    }
    draw_hints(buf, app, &th, hints);
    draw_overlay(buf, app, &th, area);
    if app.party {
        let music = crate::party::Music {
            bars: &app.bars,
            beat: app.beat,
            beats: app.beats,
            bass: app.bass,
            clock: app.clock,
            accent: app.palette.accent,
            shade: app.palette.shade,
        };
        crate::party::paint(buf, area, &app.art_rects, &app.rave, &music);
    }
}

// ---- browse ---------------------------------------------------------------

fn draw_browse(buf: &mut Buffer, app: &mut App, th: &Theme, body: Rect) {
    let side_w = if body.width >= 64 { (body.width / 4).clamp(22, 32) } else { 0 };
    if side_w == 0 && app.focus == Focus::Sidebar {
        // Too narrow for both panes: the focused one takes the whole width.
        return draw_sidebar(buf, app, th, body);
    }
    if side_w > 0 {
        let side = Rect { width: side_w, ..body };
        draw_sidebar(buf, app, th, side);
        for y in body.y..body.bottom() {
            set(buf, side.right(), y, '│', Style::default().fg(rgb(art::mix(th.bg, (255, 255, 255), 0.09))));
        }
    }
    let offset = if side_w > 0 { side_w + 1 } else { 0 };
    let main = Rect { x: body.x + offset, width: body.width - offset, ..body };
    app.hits.push((main, Hit::MainPane));
    draw_main(buf, app, th, main);
}

fn draw_sidebar(buf: &mut Buffer, app: &mut App, th: &Theme, area: Rect) {
    app.hits.push((area, Hit::SidePane));
    let focused = app.focus == Focus::Sidebar && app.screen == Screen::Browse;

    // Display rows: entries, with a blank line ahead of each later section header.
    let mut rows: Vec<Option<usize>> = Vec::with_capacity(app.sidebar.len() + 4);
    for (i, e) in app.sidebar.iter().enumerate() {
        if matches!(e, SideEntry::Header(_)) && i > 0 {
            rows.push(None);
        }
        rows.push(Some(i));
    }
    let view_h = area.height.saturating_sub(1) as usize;
    let sel_row = rows.iter().position(|r| *r == Some(app.side_sel)).unwrap_or(0);
    if app.side_follow {
        if sel_row < app.side_top + 1 {
            app.side_top = sel_row.saturating_sub(1);
        } else if sel_row >= app.side_top + view_h {
            app.side_top = sel_row + 1 - view_h;
        }
    }
    app.side_top = app.side_top.min(rows.len().saturating_sub(view_h.max(1)));

    let open_spec = app.pages.first().map(|p| p.spec.clone());
    let inner_w = area.width.saturating_sub(2);
    for (line, row) in rows.iter().skip(app.side_top).take(view_h).enumerate() {
        let y = area.y + 1 + line as u16;
        let Some(i) = *row else { continue };
        let entry = &app.sidebar[i];
        let selected = i == app.side_sel;
        let (icon, label, depth, spec): (&str, String, u8, Option<PageSpec>) = match entry {
            SideEntry::Header(h) => {
                put(buf, area.x + 2, y, inner_w, &h.to_uppercase(), Style::default().fg(th.faint).add_modifier(Modifier::BOLD));
                continue;
            }
            SideEntry::Folder { name, depth } => {
                put(
                    buf,
                    area.x + 2 + *depth as u16 * 2,
                    y,
                    inner_w.saturating_sub(*depth as u16 * 2),
                    &format!("▾ {name}"),
                    Style::default().fg(th.dim),
                );
                continue;
            }
            SideEntry::Liked => ("♥", "Liked Songs".into(), 0, Some(PageSpec::Liked)),
            SideEntry::Albums => ("◆", "Albums".into(), 0, Some(PageSpec::SavedAlbums)),
            SideEntry::Artists => ("●", "Artists".into(), 0, Some(PageSpec::FollowedArtists)),
            SideEntry::Recent => ("↺", "Recently Played".into(), 0, Some(PageSpec::Recent)),
            SideEntry::Top => ("★", "On Repeat".into(), 0, Some(PageSpec::Top)),
            SideEntry::Playlist { playlist, depth } => {
                ("", playlist.name.clone(), *depth, Some(PageSpec::Playlist(playlist.id.clone())))
            }
        };
        let is_open = spec.is_some() && spec == open_spec;
        // Mark where the music is coming from: our own choice when playing here,
        // otherwise whatever context the active device reports.
        let playing_here = app.pb.track.is_some()
            && if app.pb.local {
                spec.is_some() && spec == app.pb.source
            } else {
                matches!(entry, SideEntry::Playlist { playlist, .. }
                    if app.pb.context_uri.ends_with(&format!(":playlist:{}", playlist.id)))
            };
        let row_rect = Rect { x: area.x, y, width: area.width, height: 1 };
        app.hits.push((row_rect, Hit::Side(i)));
        if selected {
            fill(buf, row_rect, Style::default().bg(if focused { th.sel } else { th.sel_idle }));
        }
        let mut style = Style::default().fg(if selected || is_open { th.text } else { th.dim });
        if is_open {
            style = style.fg(th.accent).add_modifier(Modifier::BOLD);
        }
        let mut x = area.x + 2 + depth as u16 * 2;
        if !icon.is_empty() {
            put(buf, x, y, 2, icon, Style::default().fg(if is_open { th.accent } else { th.faint }));
            x += 2;
        }
        let room = (area.right().saturating_sub(x + 1)).saturating_sub(if playing_here { 2 } else { 0 });
        put(buf, x, y, room, &label, style);
        if playing_here {
            put(buf, area.right() - 2, y, 1, "♪", Style::default().fg(th.accent));
        }
    }
}

fn draw_main(buf: &mut Buffer, app: &mut App, th: &Theme, area: Rect) {
    let focused = app.focus == Focus::Main;
    let frame = app.frame;
    let depth = app.pages.len();
    let playing_uri = app.pb.uri().to_string();
    let is_playing = app.pb.playing;
    let eq = [app.bars[3], app.bars[14], app.bars[28]];
    let liked = &app.liked;
    let marked: std::collections::HashSet<&str> = app.basket.iter().map(|t| t.uri.as_str()).collect();
    let Some(page) = app.pages.last_mut() else { return };

    let x = area.x + 2;
    let w = area.width.saturating_sub(4);
    let mut y = area.y + 1;

    // Title block.
    let title = if page.head.title.is_empty() { "…" } else { &page.head.title };
    let mut tx = x;
    if depth > 1 {
        tx += put(buf, tx, y, 2, "‹ ", Style::default().fg(th.faint));
    }
    put(buf, tx, y, w.saturating_sub(tx - x), title, Style::default().fg(th.text).add_modifier(Modifier::BOLD));
    y += 1;
    let tab = &page.tabs[page.tab];
    let subtitle: Cow<str> = if !tab.filter.is_empty() {
        Cow::Owned(format!("{} of {} matching “{}”", tab.view.len(), tab.items.len(), tab.filter))
    } else {
        Cow::Borrowed(page.head.subtitle.as_str())
    };
    put(
        buf,
        x,
        y,
        w,
        &subtitle,
        Style::default().fg(if tab.filter.is_empty() { th.dim } else { th.accent }),
    );
    y += 2;

    // Tabs.
    if page.tabs.len() > 1 {
        let mut tx = x;
        for (i, t) in page.tabs.iter().enumerate() {
            let active = i == page.tab;
            let label = if t.items.is_empty() || t.loading {
                t.label.to_string()
            } else {
                format!("{} {}", t.label, t.items.len())
            };
            let style = if active {
                Style::default().fg(th.accent).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                Style::default().fg(th.dim)
            };
            let used = put(buf, tx, y, area.right().saturating_sub(tx + 1), &label, style);
            app.hits.push((Rect { x: tx, y, width: used, height: 1 }, Hit::Tab(i)));
            tx += used + 3;
        }
        y += 2;
    }

    let list = Rect { x: area.x, y, width: area.width, height: area.bottom().saturating_sub(y) };
    draw_list(buf, &mut app.hits, th, list, page, focused, frame, &playing_uri, is_playing, eq, liked, &marked);
}

#[allow(clippy::too_many_arguments)]
fn draw_list(
    buf: &mut Buffer,
    hits: &mut Vec<(Rect, Hit)>,
    th: &Theme,
    area: Rect,
    page: &mut Page,
    focused: bool,
    frame: u64,
    playing_uri: &str,
    is_playing: bool,
    eq: [f32; 3],
    liked: &std::collections::HashSet<String>,
    marked: &std::collections::HashSet<&str>,
) {
    let tab = page.cur_mut();
    if area.height < 2 {
        return;
    }
    let x = area.x + 2;
    let w = area.width.saturating_sub(4);

    if tab.view.is_empty() {
        let y = area.y + (area.height / 3).min(4);
        if let Some(err) = &tab.error {
            put_centered(buf, area, y, err, Style::default().fg(th.error));
            put_centered(buf, area, y + 2, "press R to retry", Style::default().fg(th.faint));
        } else if tab.loading {
            put_centered(buf, area, y, &format!("{} Loading", spinner(frame)), Style::default().fg(th.dim));
        } else if !tab.filter.is_empty() {
            put_centered(buf, area, y, "No matches", Style::default().fg(th.dim));
        } else {
            put_centered(buf, area, y, "Nothing here yet", Style::default().fg(th.dim));
        }
        return;
    }

    let has_tracks = matches!(tab.items.first(), Some(Item::Track(_)));
    // Column plan for track rows: index, heart, title, artist, album, time.
    let flex = w.saturating_sub(4 + 2 + 7);
    let (title_w, artist_w, album_w) = if flex >= 78 {
        (flex * 40 / 100, flex * 28 / 100, flex - flex * 40 / 100 - flex * 28 / 100)
    } else if flex >= 44 {
        (flex * 58 / 100, flex - flex * 58 / 100, 0)
    } else {
        (flex, 0, 0)
    };

    let mut y = area.y;
    if has_tracks {
        let head = Style::default().fg(th.faint);
        put_right(buf, x + 3, y, 3, "#", head);
        let mut cx = x + 4 + 2;
        put(buf, cx, y, title_w, "TITLE", head);
        cx += title_w;
        if artist_w > 0 {
            put(buf, cx, y, artist_w, "ARTIST", head);
            cx += artist_w;
        }
        if album_w > 0 {
            put(buf, cx, y, album_w, "ALBUM", head);
        }
        put_right(buf, x + w, y, 6, "TIME", head);
        y += 1;
    }

    let rows = area.bottom().saturating_sub(y) as usize;
    if rows == 0 {
        return;
    }
    // Keep the selection on screen with a little context around it.
    let margin = 2.min(rows.saturating_sub(1) / 2);
    if tab.sel < tab.top + margin {
        tab.top = tab.sel.saturating_sub(margin);
    } else if tab.sel + margin >= tab.top + rows {
        tab.top = tab.sel + margin + 1 - rows;
    }
    tab.top = tab.top.min(tab.view.len().saturating_sub(rows));

    for (line, vi) in (tab.top..tab.view.len()).take(rows).enumerate() {
        let ry = y + line as u16;
        let item = &tab.items[tab.view[vi]];
        let selected = vi == tab.sel;
        let row_rect = Rect { x: area.x, y: ry, width: area.width.saturating_sub(1), height: 1 };
        hits.push((row_rect, Hit::Row(vi)));
        if selected {
            fill(buf, row_rect, Style::default().bg(if focused { th.sel } else { th.sel_idle }));
        }
        match item {
            Item::Track(t) => {
                let current = !playing_uri.is_empty() && t.uri == playing_uri;
                let base = if !t.playable {
                    th.faint
                } else if current {
                    th.accent
                } else {
                    th.text
                };
                let second = if t.playable { th.dim } else { th.faint };
                let is_marked = marked.contains(t.uri.as_str());
                if is_marked {
                    // Selected for the queue: a tinted row with a tick in the margin.
                    if !selected {
                        fill(buf, row_rect, Style::default().bg(rgb(art::mix(th.bg, th.accent_rgb, 0.11))));
                    }
                    put_right(buf, x + 3, ry, 3, "✓", Style::default().fg(th.accent).add_modifier(Modifier::BOLD));
                } else if current && is_playing {
                    for (i, v) in eq.iter().enumerate() {
                        set(buf, x + 1 + i as u16, ry, bar_glyph(v.max(0.12)), Style::default().fg(th.accent));
                    }
                } else if current {
                    put_right(buf, x + 3, ry, 3, "▶", Style::default().fg(th.accent));
                } else {
                    put_right(buf, x + 3, ry, 3, &(vi + 1).to_string(), Style::default().fg(th.faint));
                }
                if liked.contains(&t.uri) {
                    put(buf, x + 4, ry, 1, "♥", Style::default().fg(th.accent));
                }
                let mut cx = x + 6;
                let mut title_style = Style::default().fg(base);
                if current {
                    title_style = title_style.add_modifier(Modifier::BOLD);
                }
                let gutter = |cw: u16| cw.saturating_sub(2);
                if artist_w == 0 {
                    // Narrow layout: one column, "Title · Artist".
                    let used = put(buf, cx, ry, gutter(title_w), &t.name, title_style);
                    let rest = gutter(title_w).saturating_sub(used + 3);
                    if rest > 4 {
                        put(buf, cx + used, ry, rest + 3, &format!(" · {}", t.artist_line()), Style::default().fg(second));
                    }
                } else {
                    put(buf, cx, ry, gutter(title_w), &t.name, title_style);
                    cx += title_w;
                    put(buf, cx, ry, gutter(artist_w), &t.artist_line(), Style::default().fg(second));
                    cx += artist_w;
                    if album_w > 0 {
                        put(buf, cx, ry, gutter(album_w), &t.album, Style::default().fg(second));
                    }
                }
                if t.duration_ms > 0 {
                    put_right(buf, x + w, ry, 7, &fmt_ms(t.duration_ms), Style::default().fg(second));
                }
            }
            Item::Album(a) => {
                let name_w = w * 48 / 100;
                let artist_w = w * 30 / 100;
                put(buf, x, ry, name_w.saturating_sub(2), &a.name, Style::default().fg(th.text));
                put(buf, x + name_w, ry, artist_w.saturating_sub(2), &a.artist_line(), Style::default().fg(th.dim));
                let kind = match a.kind.as_str() {
                    "single" => "Single",
                    "ep" => "EP",
                    "compilation" => "Compilation",
                    _ => "Album",
                };
                put(buf, x + name_w + artist_w, ry, 12, kind, Style::default().fg(th.faint));
                put_right(buf, x + w, ry, 5, &a.year, Style::default().fg(th.dim));
            }
            Item::Artist(a) => {
                put(buf, x, ry, w, &a.name, Style::default().fg(th.text));
            }
            Item::Playlist(p) => {
                let name_w = w * 55 / 100;
                put(buf, x, ry, name_w.saturating_sub(2), &p.name, Style::default().fg(th.text));
                if !p.owner.is_empty() {
                    put(buf, x + name_w, ry, (w - name_w).saturating_sub(14), &format!("by {}", p.owner), Style::default().fg(th.dim));
                }
                if p.len > 0 {
                    put_right(buf, x + w, ry, 12, &format!("{} tracks", p.len), Style::default().fg(th.faint));
                }
            }
        }
    }

    // Scrollbar, only when the list overflows.
    let total = tab.view.len();
    if total > rows {
        let sx = area.right() - 1;
        let thumb = ((rows * rows) / total).max(1);
        let start = (tab.top * (rows - thumb)) / (total - rows).max(1);
        for i in 0..rows {
            let on = i >= start && i < start + thumb;
            set(buf, sx, y + i as u16, if on { '┃' } else { '│' }, Style::default().fg(if on { th.dim } else { rgb(art::mix(th.bg, (255, 255, 255), 0.08)) }));
        }
    }
    // More results are on the way.
    if tab.loading && !tab.items.is_empty() {
        put_right(buf, x + w, area.y.saturating_sub(if has_tracks { 0 } else { 1 }), 3, &spinner(frame).to_string(), Style::default().fg(th.dim));
    }
}

// ---- now playing ----------------------------------------------------------

fn draw_now_playing(buf: &mut Buffer, app: &mut App, th: &Theme, body: Rect) {
    let Some(track) = app.pb.track.clone() else {
        put_centered(buf, body, body.y + body.height / 2 - 1, "Nothing playing", Style::default().fg(th.dim));
        put_centered(buf, body, body.y + body.height / 2 + 1, "esc to go back, then pick a track and press enter", Style::default().fg(th.faint));
        return;
    };

    let viz_h = if app.cfg.visualizer && body.height >= 20 { (body.height / 5).clamp(3, 8) } else { 0 };
    let top = Rect { height: body.height - viz_h, ..body };
    let pad = 3u16;

    let show_art = top.height >= 9 && top.width >= 50;
    let art_h = if show_art {
        (top.height.saturating_sub(2)).min(top.width * 45 / 100 / 2).min(40)
    } else {
        0
    };
    let art_w = art_h * 2;
    let art_rect = Rect {
        x: top.x + pad,
        y: top.y + 1 + (top.height.saturating_sub(2 + art_h)) / 2,
        width: art_w,
        height: art_h,
    };
    if show_art {
        draw_art(buf, app, th, art_rect, track.image.as_deref());
    }

    let col_x = if show_art { art_rect.right() + 4 } else { top.x + pad };
    let col_w = top.right().saturating_sub(col_x + pad);
    let mut y = top.y + 1;
    let liked = app.is_liked(&track.uri);
    let used = put(buf, col_x, y, col_w.saturating_sub(2), &track.name, Style::default().fg(th.text).add_modifier(Modifier::BOLD));
    if liked {
        put(buf, col_x + used + 1, y, 1, "♥", Style::default().fg(th.accent));
    }
    y += 1;
    put(buf, col_x, y, col_w, &track.artist_line(), Style::default().fg(th.accent));
    y += 1;
    put(buf, col_x, y, col_w, &track.album, Style::default().fg(th.dim));
    y += 2;

    let pane = Rect { x: col_x, y, width: col_w, height: top.bottom().saturating_sub(y + 1) };
    draw_lyrics(buf, app, th, pane);

    if viz_h > 0 {
        let viz = Rect { x: body.x + pad, y: top.bottom(), width: body.width - pad * 2, height: viz_h - 1 };
        draw_spectrum(buf, app, th, viz, 1);
    }
}

/// Word-wrap `text` to `width` cells.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let extra = if cur.is_empty() { 0 } else { 1 };
        if cur.width() + extra + word.width() > width && !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }
    lines
}

fn draw_lyrics(buf: &mut Buffer, app: &mut App, th: &Theme, pane: Rect) {
    if pane.height < 2 || pane.width < 12 {
        return;
    }
    let lyrics = match &app.lyrics {
        LyricState::Ready(l) => l,
        LyricState::Loading => {
            put(buf, pane.x, pane.y, pane.width, &format!("{} finding lyrics", spinner(app.frame)), Style::default().fg(th.faint));
            return;
        }
        LyricState::Idle | LyricState::Missing => {
            return draw_up_next(buf, app, th, pane, matches!(app.lyrics, LyricState::Missing));
        }
    };

    // Flatten to display rows, remembering which lyric line each row belongs to.
    let width = pane.width as usize;
    let mut rows: Vec<(usize, String)> = Vec::new();
    for (i, line) in lyrics.lines.iter().enumerate() {
        let text = if line.text.trim().is_empty() { "♪" } else { line.text.as_str() };
        for piece in wrap(text, width) {
            rows.push((i, piece));
        }
    }
    let current = app.lyric_line();
    let focus_line = app.lyrics_scroll.or(current).unwrap_or(0);
    let focus_row = rows.iter().position(|(i, _)| *i == focus_line).unwrap_or(0);
    let body_h = pane.height.saturating_sub(1) as usize;
    // Hold the active line a third of the way down so upcoming lines are visible.
    let top = focus_row.saturating_sub(body_h / 3).min(rows.len().saturating_sub(body_h));

    for (n, (i, text)) in rows.iter().skip(top).take(body_h).enumerate() {
        let style = match current {
            Some(c) if *i == c => Style::default().fg(th.text).add_modifier(Modifier::BOLD),
            Some(c) if *i < c => Style::default().fg(th.faint),
            Some(c) if *i <= c + 2 => Style::default().fg(th.dim),
            Some(_) => Style::default().fg(th.faint),
            None if lyrics.synced => Style::default().fg(th.dim),
            None => Style::default().fg(th.dim),
        };
        put(buf, pane.x, pane.y + n as u16, pane.width, text, style);
    }
    let credit = if lyrics.synced {
        format!("lyrics · {}", lyrics.source)
    } else {
        format!("lyrics · {} · not time-synced, ↑↓ to scroll", lyrics.source)
    };
    put(buf, pane.x, pane.bottom() - 1, pane.width, &credit, Style::default().fg(th.faint));
}

fn draw_up_next(buf: &mut Buffer, app: &App, th: &Theme, pane: Rect, lyrics_missing: bool) {
    let mut y = pane.y;
    if lyrics_missing {
        put(buf, pane.x, y, pane.width, "No lyrics for this one", Style::default().fg(th.faint));
        y += 2;
    }
    if app.queue.is_empty() || y + 2 >= pane.bottom() {
        return;
    }
    put(buf, pane.x, y, pane.width, "UP NEXT", Style::default().fg(th.faint).add_modifier(Modifier::BOLD));
    y += 1;
    for t in app.queue.iter().take(pane.bottom().saturating_sub(y) as usize) {
        let used = put(buf, pane.x, y, pane.width, &t.name, Style::default().fg(th.dim));
        let rest = pane.width.saturating_sub(used + 3);
        if rest > 6 {
            put(buf, pane.x + used, y, rest + 3, &format!(" · {}", t.artist_line()), Style::default().fg(th.faint));
        }
        y += 1;
    }
}

// ---- DJ decks -------------------------------------------------------------

/// The crossfader: a track with a handle, A on the left and B on the right.
fn draw_fader(buf: &mut Buffer, th: &Theme, x: u16, y: u16, width: u16, value: f32) {
    if width < 8 {
        return;
    }
    let track = width - 4;
    put(buf, x, y, 1, "A", Style::default().fg(if value < 0.5 { th.accent } else { th.faint }).add_modifier(Modifier::BOLD));
    put(buf, x + width - 1, y, 1, "B", Style::default().fg(if value > 0.5 { th.accent } else { th.faint }).add_modifier(Modifier::BOLD));
    let handle = (value.clamp(0.0, 1.0) * (track - 1) as f32).round() as u16;
    for i in 0..track {
        let (ch, color) = if i == handle { ('●', th.text) } else if i == track / 2 { ('┼', th.dim) } else { ('━', th.faint) };
        set(buf, x + 2 + i, y, ch, Style::default().fg(color));
    }
}

fn draw_deck(buf: &mut Buffer, app: &mut App, th: &Theme, area: Rect, d: usize) {
    let deck = app.decks.decks[d].clone();
    let focused = app.deck_focus == d;
    let heard = if d == 0 { 1.0 - app.decks.fader } else { app.decks.fader };
    let x = area.x + 2;
    let w = area.width.saturating_sub(4);
    let mut y = area.y + 1;

    // Header: which deck, a light that flashes on its beat, and its state.
    let label = format!("{} DECK {}", if focused { "▸" } else { " " }, crate::dj::deck_name(d));
    let used = put(buf, x, y, w, &label, Style::default().fg(if focused { th.accent } else { th.dim }).add_modifier(Modifier::BOLD));
    let on_beat = deck.playing && deck.meter.phase.is_some_and(|p| p < 0.18);
    put(buf, x + used + 1, y, 1, "●", Style::default().fg(if on_beat { th.text } else { th.faint }));
    let state = if deck.failed {
        "can't be played"
    } else if deck.loading {
        "loading"
    } else if deck.track.is_none() {
        "empty"
    } else if deck.ended {
        "finished"
    } else if deck.playing && heard < 0.02 {
        "playing, silent"
    } else if deck.playing {
        "playing"
    } else {
        "cued"
    };
    put_right(buf, x + w, y, 16, state, Style::default().fg(if deck.playing { th.accent } else { th.faint }));
    y += 2;

    let Some(track) = deck.track.clone() else {
        put(buf, x, y, w, &format!("Press {} on a song to load it here", d + 1), Style::default().fg(th.faint));
        return;
    };

    // Cover, with the song beside it.
    let art_h = (area.height.saturating_sub(8)).clamp(3, 7).min(w / 4);
    let art = Rect { x, y, width: art_h * 2, height: art_h };
    if area.height >= 12 && w >= 30 {
        draw_art(buf, app, th, art, track.image.as_deref());
    }
    let tx = if area.height >= 12 && w >= 30 { art.right() + 2 } else { x };
    let tw = (x + w).saturating_sub(tx);
    put(buf, tx, y, tw, &track.name, Style::default().fg(th.text).add_modifier(Modifier::BOLD));
    put(buf, tx, y + 1, tw, &track.artist_line(), Style::default().fg(th.dim));
    // Tempo as it is being heard, and how far it has been bent.
    let tempo = match deck.meter.bpm {
        Some(bpm) => format!("{bpm:.1} BPM"),
        None if deck.playing => "finding the beat…".to_string(),
        None => "– BPM".to_string(),
    };
    let used = put(buf, tx, y + 3, tw, &tempo, Style::default().fg(th.text));
    if deck.bend.abs() > 0.0005 {
        put(buf, tx + used + 2, y + 3, 8, &format!("{:+.1}%", deck.bend * 100.0), Style::default().fg(th.accent));
    }
    if app.decks.locked == Some(d) {
        put_right(buf, x + w, y + 3, 10, "on beat", Style::default().fg(th.accent));
    }
    y += art_h.max(4) + 1;

    // Level meter and progress.
    if y + 2 < area.bottom() {
        let cells = (deck.meter.level.clamp(0.0, 1.0).sqrt() * w as f32) as u16;
        for i in 0..w {
            set(buf, x + i, y, if i < cells { '▮' } else { '▯' }, Style::default().fg(if i < cells { th.accent } else { th.faint }));
        }
        y += 1;
        let glyph = if deck.loading { spinner(app.frame).to_string() } else if deck.playing { "❚❚".into() } else { "▶".into() };
        put(buf, x, y, 2, &glyph, Style::default().fg(th.accent).add_modifier(Modifier::BOLD));
        let ew = put(buf, x + 3, y, 7, &fmt_ms(deck.pos_ms), Style::default().fg(th.dim));
        let tw = put_right(buf, x + w, y, 7, &fmt_ms(track.duration_ms), Style::default().fg(th.dim));
        let bx = x + 4 + ew;
        let bw = (x + w).saturating_sub(tw + 1).saturating_sub(bx);
        if bw >= 4 && track.duration_ms > 0 {
            let filled = ((deck.pos_ms as f64 / track.duration_ms as f64).min(1.0) * bw as f64).round() as u16;
            for i in 0..bw {
                let (ch, color) = if i < filled { ('━', th.accent) } else { ('─', th.faint) };
                set(buf, bx + i, y, ch, Style::default().fg(color));
            }
            set(buf, bx + filled.min(bw - 1), y, '●', Style::default().fg(th.text));
        }
        y += 2;
    }

    if y < area.bottom() {
        draw_eq(buf, th, x, y, w, deck.eq, deck.meter.bands, focused);
    }
}

/// A deck's three EQ knobs, each beside a meter of what that band is putting out.
fn draw_eq(buf: &mut Buffer, th: &Theme, x: u16, y: u16, w: u16, eq: [i8; 3], bands: [f32; 3], focused: bool) {
    use crate::dj::{EQ_KILL, EQ_MAX};
    let slots = (EQ_MAX - EQ_KILL + 1) as u16;
    // Label, meter and knob track when there is room; otherwise the setting in words.
    let full = w >= 3 * (6 + slots) + 4;
    let each = if full { 6 + slots + 2 } else { 12 };
    for band in 0..3u16 {
        let bx = x + band * each;
        if bx + if full { 6 + slots } else { 10 } > x + w {
            break;
        }
        let step = eq[band as usize];
        let killed = step == EQ_KILL;
        let name = ["LOW", "MID", "HI"][band as usize];
        let tone = if killed { th.error } else if focused { th.text } else { th.dim };
        put(buf, bx, y, 3, name, Style::default().fg(tone).add_modifier(Modifier::BOLD));
        let level = bands[band as usize].clamp(0.0, 1.0).sqrt();
        set(buf, bx + 4, y, bar_glyph(level), Style::default().fg(if killed { th.faint } else { th.accent }));
        if !full {
            put(buf, bx + 6, y, 6, crate::dj::eq_label(step).trim_end_matches(" dB"), Style::default().fg(tone));
            continue;
        }
        let at = (step - EQ_KILL) as u16;
        for i in 0..slots {
            let (ch, color) = if i == at {
                (if killed { '✕' } else { '●' }, tone)
            } else if i < at {
                ('━', if focused { th.accent } else { th.dim })
            } else if i == (-EQ_KILL) as u16 {
                // Where flat is, when the knob is below it.
                ('┆', th.faint)
            } else {
                ('─', th.faint)
            };
            set(buf, bx + 6 + i, y, ch, Style::default().fg(color));
        }
    }
}

fn draw_decks(buf: &mut Buffer, app: &mut App, th: &Theme, body: Rect) {
    let viz_h = if app.cfg.visualizer && body.height >= 24 { (body.height / 6).clamp(3, 6) } else { 0 };
    let fader_h = 4;
    let decks_h = body.height.saturating_sub(viz_h + fader_h);
    let half = body.width / 2;
    draw_deck(buf, app, th, Rect { x: body.x, y: body.y, width: half, height: decks_h }, 0);
    draw_deck(buf, app, th, Rect { x: body.x + half, y: body.y, width: body.width - half, height: decks_h }, 1);
    for y in body.y + 1..body.y + decks_h {
        set(buf, body.x + half, y, '│', Style::default().fg(rgb(art::mix(th.bg, (255, 255, 255), 0.09))));
    }

    // Crossfader, with what a transition in progress is up to.
    let fy = body.y + decks_h + 1;
    let fw = (body.width * 3 / 5).clamp(20, 70).min(body.width.saturating_sub(4));
    draw_fader(buf, th, body.x + (body.width - fw) / 2, fy, fw, app.decks.fader);
    let status = match app.decks.auto {
        Some((to, _, true)) => format!("listening to deck {} to find its beat…", crate::dj::deck_name(to)),
        Some((to, progress, false)) => format!("mixing in deck {} · {:.0}%", crate::dj::deck_name(to), progress * 100.0),
        None if app.decks.locked.is_some() => "decks are beat-matched".to_string(),
        None => String::new(),
    };
    put_centered(buf, body, fy + 1, &status, Style::default().fg(th.accent));

    if viz_h > 0 {
        let viz = Rect { x: body.x + 3, y: body.bottom() - viz_h, width: body.width.saturating_sub(6), height: viz_h - 1 };
        draw_spectrum(buf, app, th, viz, 1);
    }
}

/// With the decks on, the bottom bar shows both of them at a glance.
fn draw_decks_bar(buf: &mut Buffer, app: &mut App, th: &Theme, bar: Rect) {
    fill(buf, bar, Style::default().bg(rgb(th.panel)));
    let x = bar.x + 2;
    let w = bar.width.saturating_sub(4);
    let compact = bar.height < 5;
    for d in 0..2 {
        let deck = &app.decks.decks[d];
        let y = if compact { bar.y + d as u16 } else { bar.y + 1 + d as u16 };
        let glyph = if deck.playing { "▶" } else { "‖" };
        let mut cx = x + put(buf, x, y, 4, &format!("{} {} ", crate::dj::deck_name(d), glyph), Style::default().fg(if deck.playing { th.accent } else { th.faint }).add_modifier(Modifier::BOLD));
        match &deck.track {
            Some(t) => {
                cx += put(buf, cx, y, w.saturating_sub(34), &t.name, Style::default().fg(if deck.playing { th.text } else { th.dim }));
                put(buf, cx, y, (x + w).saturating_sub(cx + 22), &format!(" · {}", t.artist_line()), Style::default().fg(th.faint));
                let right = format!("{}  {}", deck.meter.bpm.map(|b| format!("{b:.0} BPM")).unwrap_or_default(), fmt_ms(deck.pos_ms));
                put_right(buf, x + w, y, 20, &right, Style::default().fg(th.dim));
            }
            None => {
                put(buf, cx, y, w, &format!("empty: press {} on a song", d + 1), Style::default().fg(th.faint));
            }
        }
    }
    if !compact {
        let fw = (w / 2).clamp(16, 44);
        draw_fader(buf, th, x, bar.y + 3, fw, app.decks.fader);
        if app.screen != Screen::Decks {
            put_right(buf, x + w, bar.y + 3, 24, "D opens the decks", Style::default().fg(th.faint));
        }
    }
}

// ---- four squares ---------------------------------------------------------

/// The four squares, filling `area`: 1 2 above, 3 4 below.
fn draw_quad(buf: &mut Buffer, app: &mut App, th: &Theme, area: Rect) {
    let (half_w, half_h) = (area.width / 2, area.height / 2);
    for d in 0..crate::quad::SQUARES {
        let (col, row) = ((d % 2) as u16, (d / 2) as u16);
        let rect = Rect {
            x: area.x + col * half_w,
            y: area.y + row * half_h,
            width: if col == 0 { half_w } else { area.width - half_w },
            height: if row == 0 { half_h } else { area.height - half_h },
        };
        app.hits.push((rect, Hit::Square(d)));
        draw_square(buf, app, th, rect, d);
    }
    // Only when the covers gather round the middle, beside their words.
    let beside = crate::pop::layout(half_w, half_h, &crate::pop::Look::default()).beside;
    if beside && app.squares.squares.iter().any(|s| s.track.is_some()) {
        if pictures(app) {
            let (cx, cy) = (area.x as f32 + half_w as f32, (area.y + half_h) as f32);
            let radius = hub_radius(area, app.beat);
            // The disc's box, in cells: a pixel is half a row.
            let (rx, ry) = (radius.ceil() as u16 + 1, (radius / 2.0).ceil() as u16 + 1);
            let cells = Rect { x: (cx as u16).saturating_sub(rx), y: (cy as u16).saturating_sub(ry), width: rx * 2, height: ry * 2 };
            let to = app.squares.next.map(|n| n.to);
            app.pixel_jobs.push((cells, PixelJob::Hub { centre: (cx, cy), radius, to }));
        } else {
            draw_hub(buf, app, area);
        }
    }
}

/// Whether the squares are drawn with real pictures: in a terminal that
/// shows them, with nothing popped up that a picture would hide.
fn pictures(app: &App) -> bool {
    app.pixels.is_some() && app.cfg.pictures && matches!(app.overlay, Overlay::None | Overlay::Filter)
}

/// How big the disc where the squares meet is, in pixels (half a row each).
fn hub_radius(area: Rect, beat: f32) -> f32 {
    (area.height as f32 * 0.17).clamp(3.5, 6.0) * (1.0 + 0.08 * beat)
}

fn draw_square(buf: &mut Buffer, app: &mut App, th: &Theme, rect: Rect, d: usize) {
    use crate::pop::{self, Look, SCHEMES};
    use crate::quad::Sync;
    if rect.width < 4 || rect.height < 2 {
        return;
    }
    let sq = app.squares.squares[d].clone();
    let next = app.squares.next.filter(|n| n.to == d);
    let live = app.squares.live == d;
    let heard = sq.meter.heard.clamp(0.0, 1.0);
    // Shaking harder as the moment it comes in nears.
    let coming = match next {
        Some(n) if n.progress.is_none() => (1.0 - n.beats_left / 4.0).clamp(0.15, 1.0),
        _ => 0.0,
    };
    let look = Look {
        scheme: d,
        heard,
        beat: app.beat * heard,
        bass: app.bass,
        time: app.clock,
        beats: app.beats,
        coming,
        picked: app.squares.target() == d,
        still: pictures(app),
    };
    let cover = sq.track.as_ref().and_then(|t| t.image.clone());
    let image = cover.as_deref().and_then(|u| app.thumbs.get(u).cloned());
    if let (Some(url), None) = (&cover, &image) {
        app.want_image(&url.clone());
    }
    let drawn = pop::square(image.as_deref().filter(|_| sq.track.is_some()), rect.width, rect.height, look);
    for row in 0..drawn.h {
        for col in 0..drawn.w {
            let c = drawn.cells[row as usize * drawn.w as usize + col as usize];
            if let Some(cell) = buf.cell_mut((rect.x + col, rect.y + row)) {
                cell.set_char(c.ch).set_fg(rgb(c.fg));
                if let Some(bg) = c.bg {
                    cell.set_bg(rgb(bg));
                }
            }
        }
    }
    app.drawn_squares.push((rect, look, cover.clone().filter(|_| sq.track.is_some())));
    // Where the terminal shows pictures, the cover is one, sharp, sent once
    // the characters are drawn. The cells under it are left plain and
    // unchanging, so the terminal never has a reason to redraw them.
    if look.still && image.is_some() && sq.track.is_some() {
        let (col, row, width, height) = pop::cover_cells(rect.width, rect.height, &look);
        let cells = Rect { x: rect.x + col, y: rect.y + row, width, height };
        let s = &crate::pop::SCHEMES[d];
        fill(buf, cells, Style::default().bg(rgb(s.paper)).fg(rgb(s.paper)));
        for y in cells.y..cells.bottom() {
            for x in cells.x..cells.right() {
                set(buf, x, y, ' ', Style::default().bg(rgb(s.paper)));
            }
        }
        if let Some(larger) = cover.as_deref().and_then(art::larger) {
            app.want_image(&larger);
        }
        app.pixel_jobs.push((cells, PixelJob::Square));
    }

    let s = &SCHEMES[d];
    let sticker = Style::default().bg(rgb(s.ink)).fg(rgb(s.letter)).add_modifier(Modifier::BOLD);
    let plain = Style::default().bg(rgb(s.paper)).fg(rgb(s.ink));
    let at = pop::layout(rect.width, rect.height, &look);
    let (tx, tw) = (rect.x + at.text_x, at.text_w);
    // Beside the cover the words line up against it, so the four covers and
    // their titles make one picture; under it, they are centred.
    let towards_right = at.beside && d % 2 == 0;
    let line = |buf: &mut Buffer, y: u16, text: &str, style: Style| {
        let text = fit(text, tw as usize);
        let used = text.width() as u16;
        let x = if !at.beside { tx + tw.saturating_sub(used) / 2 } else if towards_right { tx + tw - used } else { tx };
        put(buf, x, y, used, &text, style);
    };

    let Some(track) = sq.track.clone() else {
        let mid = rect.y + rect.height / 2;
        put_centered(buf, rect, mid.saturating_sub(1), " EMPTY ", sticker);
        put_centered(buf, rect, mid + 1, " esc, then enter on a song ", plain);
        return;
    };

    // What it is up to. Beside the cover the big number says which square it is.
    let number = if at.numeral.is_some() { String::new() } else { format!("{} ", d + 1) };
    let state = match (next, live) {
        (Some(n), _) if n.progress.is_some() => format!(" {number}▶▶ {} ", n.mv.name().to_uppercase()),
        (Some(n), _) => {
            let left = n.beats_left.ceil().clamp(1.0, 8.0) as usize;
            format!(" {number}NEXT {} ", "●".repeat(left))
        }
        (None, true) if app.squares.paused => format!(" {number}❚❚ PAUSED "),
        (None, true) if app.squares.stuttering => format!(" {number}♪ STUTTER "),
        (None, true) => format!(" {number}♪ ON AIR "),
        _ if sq.loading => format!(" {number}{} ", spinner(app.frame)),
        _ if number.is_empty() => String::new(),
        _ => format!(" {}", number),
    };
    // Every square's sticker blinks on its own beat: in time, they blink together.
    let flash = sq.meter.phase.is_some_and(|p| p < 0.15) && !app.squares.paused && next.is_none();
    let state_style = if flash { Style::default().bg(rgb(s.letter)).fg(rgb(s.ink)).add_modifier(Modifier::BOLD) } else { sticker };

    // The tempo it is running at, whether it is in time, and for the one
    // playing, which beat of the bar it is on: the one is coming.
    let sync = match sq.meter.sync {
        _ if live => "",
        Sync::Locked => " ⇄ in time",
        Sync::Listening => " … listening",
        Sync::Apart => " ≠ too far apart",
    };
    let count = match sq.meter.bar {
        Some(bar) if live => {
            let at = (bar * 4.0) as usize;
            format!("  {}", (0..4).map(|k| if k == at { '●' } else { '○' }).collect::<String>())
        }
        _ => String::new(),
    };
    let tempo = sq.meter.bpm.map(|bpm| {
        let full = format!(" {bpm:.0} BPM{sync}{count} ");
        // Short of room, the sign alone says it.
        if full.width() > tw as usize { format!(" {bpm:.0} BPM{}{count} ", sync.split(' ').take(2).collect::<Vec<_>>().join(" ")) } else { full }
    });

    // The title as a headline: capitals, broken over up to three strips.
    let room = at.text_bottom.saturating_sub(at.text_top) as usize;
    let mut title = wrap(&track.name.to_uppercase(), tw.saturating_sub(2).max(4) as usize);
    let most = room.saturating_sub(3 + !state.is_empty() as usize).clamp(1, 3);
    if title.len() > most {
        title.truncate(most);
        if let Some(last) = title.last_mut() {
            last.push('…');
        }
    }
    let progress = live && track.duration_ms > 0;
    let lines = !state.is_empty() as usize * 2 + title.len() + 1 + tempo.is_some() as usize + progress as usize;
    let mut y = rect.y + at.text_top + (room.saturating_sub(lines) / 2) as u16;
    if !at.beside {
        // Under the cover the state sits in the corner, out of the way.
        put(buf, rect.x + 2, rect.y + 1, tw, &state, state_style);
    } else if !state.is_empty() {
        line(buf, y, &state, state_style);
        y += 2;
    }
    let strip = if live { sticker } else { Style::default().bg(rgb(s.ink)).fg(rgb(s.paper)).add_modifier(Modifier::BOLD) };
    for words in &title {
        if y >= rect.bottom().saturating_sub(1) {
            break;
        }
        line(buf, y, &format!(" {words} "), strip);
        y += 1;
    }
    if y < rect.bottom().saturating_sub(1) {
        line(buf, y, &format!(" {} ", track.artist_line()), plain.add_modifier(Modifier::BOLD));
        y += 1;
    }
    if let (Some(tempo), true) = (tempo, y < rect.bottom().saturating_sub(1)) {
        line(buf, y, &tempo, plain);
        y += 1;
    }

    // How far through the song: a rule as wide as the words.
    if progress && y < rect.bottom().saturating_sub(1) {
        let bw = tw.min(if at.beside { tw } else { rect.width.saturating_sub(6) });
        let bx = if at.beside && towards_right { tx + tw - bw } else if at.beside { tx } else { rect.x + (rect.width - bw) / 2 };
        let filled = ((sq.pos_ms as f64 / track.duration_ms as f64).min(1.0) * bw as f64) as u16;
        for i in 0..bw {
            let (ch, color) = if i < filled { ('━', s.ink) } else { ('─', art::mix(s.paper, s.ink, 0.35)) };
            if let Some(cell) = buf.cell_mut((bx + i, y)) {
                cell.set_char(ch).set_fg(rgb(color)).set_bg(rgb(s.paper));
            }
        }
    }
    let _ = th;
}

/// Where the four squares meet: a disc that pulses on the beat, with an
/// arrow to the square the music is heading for.
fn draw_hub(buf: &mut Buffer, app: &App, area: Rect) {
    let (cx, cy) = (area.x as f32 + (area.width / 2) as f32, (area.y + area.height / 2) as f32 * 2.0);
    let radius = hub_radius(area, app.beat);
    let ink = (14, 10, 24);
    let ring = art::mix((255, 255, 255), (255, 226, 70), app.beat);
    // Each cell is two pixels: its top in the foreground colour of `▀`, its
    // bottom in the background. Squares are all drawn that way, so a pixel
    // can be read back off them and changed alone.
    let pixel = |x: f32, y: f32| -> Option<art::Rgb> {
        let r = ((x - cx) * (x - cx) + (y - cy) * (y - cy)).sqrt();
        if r <= radius - 1.5 {
            Some(ink)
        } else if r <= radius {
            Some(ring)
        } else if r <= radius + 1.0 {
            Some(ink)
        } else {
            None
        }
    };
    let reach = radius as i32 + 2;
    for row in (cy / 2.0) as i32 - reach / 2 - 1..=(cy / 2.0) as i32 + reach / 2 + 1 {
        for col in cx as i32 - reach..=cx as i32 + reach {
            let (Ok(x), Ok(y)) = (u16::try_from(col), u16::try_from(row)) else { continue };
            let (top, bottom) = (pixel(col as f32 + 0.5, row as f32 * 2.0 + 0.5), pixel(col as f32 + 0.5, row as f32 * 2.0 + 1.5));
            if top.is_none() && bottom.is_none() {
                continue;
            }
            let Some(cell) = buf.cell_mut((x, y)) else { continue };
            let read = |c: Color| match c {
                Color::Rgb(r, g, b) => (r, g, b),
                _ => ink,
            };
            let (was_top, was_bottom) = (read(cell.fg), read(cell.bg));
            cell.set_char('▀').set_fg(rgb(top.unwrap_or(was_top))).set_bg(rgb(bottom.unwrap_or(was_bottom)));
        }
    }
    // In the middle: where the music is going, or a note on the beat.
    let arrow = match app.squares.next.map(|n| n.to) {
        Some(0) => "↖",
        Some(1) => "↗",
        Some(2) => "↙",
        Some(3) => "↘",
        _ if app.beat > 0.4 => "♪",
        _ => "·",
    };
    let style = Style::default().bg(rgb(ink)).fg(rgb(ring)).add_modifier(Modifier::BOLD);
    put(buf, cx as u16, (cy / 2.0) as u16, 1, arrow, style);
}

/// With the squares on and the songs on screen, the bottom bar shows all four.
fn draw_quad_bar(buf: &mut Buffer, app: &mut App, th: &Theme, bar: Rect) {
    use crate::pop::SCHEMES;
    fill(buf, bar, Style::default().bg(rgb(th.panel)));
    let compact = bar.height < 5;
    let x = bar.x + 2;
    let w = bar.width.saturating_sub(4);
    let live = app.squares.live;
    let y = if compact { bar.y } else { bar.y + 1 };
    if let Some(track) = app.squares.squares[live].track.clone() {
        let s = &SCHEMES[live];
        let badge = format!(" {} ♪ ", live + 1);
        let mut cx = x + put(buf, x, y, 8, &badge, Style::default().bg(rgb(s.paper)).fg(rgb(s.ink)).add_modifier(Modifier::BOLD)) + 1;
        cx += put(buf, cx, y, w.saturating_sub(40), &track.name, Style::default().fg(th.text).add_modifier(Modifier::BOLD));
        put(buf, cx, y, (x + w).saturating_sub(cx + 24), &format!(" · {}", track.artist_line()), Style::default().fg(th.dim));
        let bpm = app.squares.squares[live].meter.bpm.map(|b| format!("{b:.0} BPM  ")).unwrap_or_default();
        put_right(buf, x + w, y, 22, &format!("{bpm}{}", if app.squares.paused { "paused" } else { "playing" }), Style::default().fg(th.dim));
    }
    let y = y + 1 + (!compact) as u16;
    if y >= bar.bottom() {
        return;
    }
    let each = w / crate::quad::SQUARES as u16;
    for d in 0..crate::quad::SQUARES {
        let s = &SCHEMES[d];
        let sq = &app.squares.squares[d];
        let cx = x + d as u16 * each;
        let style = if d == live { Style::default().bg(rgb(s.paper)).fg(rgb(s.ink)) } else { Style::default().fg(rgb(s.paper)) };
        let coming = app.squares.next.is_some_and(|n| n.to == d);
        let name = sq.track.as_ref().map_or("empty", |t| t.name.as_str());
        let label = format!(" {}{} {} ", d + 1, if coming { "▸" } else { "" }, name);
        put(buf, cx, y, each.saturating_sub(1), &label, style.add_modifier(Modifier::BOLD));
    }
}

// ---- player bar -----------------------------------------------------------

fn draw_player(buf: &mut Buffer, app: &mut App, th: &Theme, bar: Rect) {
    fill(buf, bar, Style::default().bg(rgb(th.panel)));
    let compact = bar.height < 5;
    let track = app.pb.track.clone();
    let pos = app.position();
    let dur = track.as_ref().map(|t| t.duration_ms).unwrap_or(0);

    // Left: artwork tile (also a button to open the now-playing screen).
    let mut x = bar.x + 2;
    if !compact && app.screen == Screen::Browse {
        let art_rect = Rect { x: bar.x + 2, y: bar.y + 1, width: 6, height: 3 };
        let url = track.as_ref().and_then(|t| t.image.clone());
        draw_art(buf, app, th, art_rect, url.as_deref());
        app.hits.push((art_rect, Hit::NowPlayingArt));
        x = art_rect.right() + 2;
    }

    let right_w: u16 = if bar.width >= 100 { 38 } else if bar.width >= 72 { 18 } else { 0 };
    let text_w = bar.right().saturating_sub(x + right_w + 3);
    let title_y = if compact { bar.y } else { bar.y + 1 };

    match &track {
        Some(t) => {
            let used = put(buf, x, title_y, text_w.saturating_sub(2), &t.name, Style::default().fg(th.text).add_modifier(Modifier::BOLD));
            if app.is_liked(&t.uri) {
                put(buf, x + used + 1, title_y, 1, "♥", Style::default().fg(th.accent));
            }
            if compact {
                let rest = text_w.saturating_sub(used + 5);
                if rest > 6 {
                    put(buf, x + used + 3, title_y, rest, &format!("· {}", t.artist_line()), Style::default().fg(th.dim));
                }
            } else {
                let line = if t.album.is_empty() { t.artist_line() } else { format!("{} · {}", t.artist_line(), t.album) };
                put(buf, x, title_y + 1, text_w, &line, Style::default().fg(th.dim));
            }
        }
        None => {
            let msg = match app.conn {
                Conn::Connecting => "Connecting to Spotify…",
                Conn::Reconnecting => "Reconnecting…",
                Conn::LoginRejected => "Spotify login was rejected. Quit and run: riff login",
                Conn::Online => "Ready. Pick something and press enter",
            };
            put(buf, x, title_y, text_w + right_w, msg, Style::default().fg(th.dim));
        }
    }

    // Right: live spectrum above the playback switches.
    if right_w > 0 && !compact {
        let rx = bar.right() - right_w - 2;
        if app.cfg.visualizer && right_w >= 30 && app.screen == Screen::Browse {
            draw_spectrum(buf, app, th, Rect { x: rx, y: bar.y + 1, width: right_w, height: 2 }, 0);
        }
        let on = |active: bool| Style::default().fg(if active { th.accent } else { th.faint });
        let mut sx = rx;
        let sy = bar.y + 3;
        if right_w >= 30 {
            sx += put(buf, sx, sy, 10, "⇄ shuffle", on(app.pb.shuffle)) + 2;
            let rep = match app.pb.repeat {
                Repeat::Off => "↻ repeat",
                Repeat::Context => "↻ all",
                Repeat::Track => "↻ one",
            };
            sx += put(buf, sx, sy, 9, rep, on(app.pb.repeat != Repeat::Off)) + 2;
            put(buf, sx, sy, 6, "≈ fade", on(app.cfg.mix));
        }
        let vol = format!("vol {:>3}%", app.pb.volume);
        put_right(buf, bar.right() - 2, sy, 9, &vol, Style::default().fg(th.dim));
    }

    // Bottom: transport and progress.
    let py = if compact { bar.y + 1 } else { bar.y + 3 };
    let glyph = if app.pb.loading {
        spinner(app.frame).to_string()
    } else if app.pb.playing {
        "❚❚".to_string()
    } else {
        "▶".to_string()
    };
    put(buf, x, py, 2, &glyph, Style::default().fg(th.accent).add_modifier(Modifier::BOLD));
    app.hits.push((Rect { x: x.saturating_sub(1), y: py, width: 4, height: 1 }, Hit::PlayPause));

    let end = if compact || right_w == 0 { bar.right() - 2 } else { bar.right() - right_w - 4 };
    let elapsed = fmt_ms(pos);
    let total = fmt_ms(dur);
    let ex = x + 3;
    let ew = put(buf, ex, py, 8, &elapsed, Style::default().fg(th.dim));
    let tw = put_right(buf, end, py, 8, &total, Style::default().fg(th.dim));
    let bx = ex + ew + 1;
    let bw = end.saturating_sub(tw + 1).saturating_sub(bx);
    if bw >= 4 {
        let frac = if dur > 0 { pos as f64 / dur as f64 } else { 0.0 };
        let filled = (frac * bw as f64).round() as u16;
        for i in 0..bw {
            let (ch, color) = if i < filled { ('━', th.accent) } else { ('─', th.faint) };
            set(buf, bx + i, py, ch, Style::default().fg(color));
        }
        if dur > 0 {
            set(buf, bx + filled.min(bw - 1), py, '●', Style::default().fg(th.text));
        }
        app.hits.push((Rect { x: bx, y: py, width: bw, height: 1 }, Hit::Seek));
    }

    // Where the sound is coming out, when it isn't here.
    if !compact {
        if let Some(name) = &app.pb.remote_name {
            put(buf, x, bar.y + 4, text_w, &format!("▸ playing on {name}  (d to move it)"), Style::default().fg(th.accent));
        }
    }
}

// ---- hint line ------------------------------------------------------------

fn draw_hints(buf: &mut Buffer, app: &mut App, th: &Theme, area: Rect) {
    let y = area.y;
    let mut x = area.x + 2;

    if let Overlay::Filter = app.overlay {
        let filter = app.page().map(|p| p.cur().filter.as_str()).unwrap_or("");
        x += put(buf, x, y, 10, "filter ▸ ", Style::default().fg(th.accent));
        x += put(buf, x, y, area.width.saturating_sub(40), filter, Style::default().fg(th.text));
        put(buf, x, y, 1, "▏", Style::default().fg(th.accent));
        put_right(buf, area.right() - 2, y, 28, "enter keep · esc clear", Style::default().fg(th.faint));
        return;
    }

    // Right side first, so hints know how much room is left.
    let mut right_used = 0;
    if let Some(t) = &app.toast {
        let style = Style::default().fg(if t.error { th.error } else { th.accent });
        right_used = put_right(buf, area.right() - 2, y, area.width.saturating_sub(6).min(70), &t.text, style);
    } else {
        let status: Option<(String, Color)> = match app.conn {
            Conn::Connecting => Some((format!("{} connecting", spinner(app.frame)), th.dim)),
            Conn::Reconnecting => Some((format!("{} reconnecting", spinner(app.frame)), th.error)),
            Conn::LoginRejected => Some(("login rejected · run `riff login`".into(), th.error)),
            Conn::Online => app
                .backend
                .as_ref()
                .and_then(|b| b.api.as_ref())
                .and_then(|api| api.blocked_for())
                .map(|s| (format!("Spotify rate limit · {s}s"), th.error)),
        };
        if let Some((text, color)) = status {
            right_used = put_right(buf, area.right() - 2, y, 40, &text, Style::default().fg(color));
        }
    }

    // With songs selected, say so and say what can be done with them.
    if !app.basket.is_empty() && matches!(app.overlay, Overlay::None) {
        let n = app.basket.len();
        let label = format!("✓ {n} selected ");
        x += put(buf, x, y, 16, &label, Style::default().fg(th.accent).add_modifier(Modifier::BOLD)) + 1;
    }
    let selecting = !app.basket.is_empty();
    let hints: &[(&str, &str)] = match (&app.overlay, app.screen, app.focus) {
        (Overlay::Help { .. }, _, _) => &[("↑↓", "more keys"), ("esc", "close")],
        (Overlay::Search { .. }, _, _) => &[("enter", "search"), ("esc", "cancel")],
        (Overlay::Queue { .. }, _, _) => &[("↑↓", "browse"), ("esc", "close")],
        (Overlay::Devices { .. }, _, _) => &[("enter", "play there"), ("esc", "close")],
        (_, Screen::Quad, _) => &[
            ("← → ↑ ↓", "next song"),
            ("shift+arrow", "drop it"),
            ("enter", "stutter"),
            ("tab", "surprise me"),
            ("space", "pause"),
            ("esc", "songs"),
            ("4", "off"),
        ],
        (_, Screen::Browse, Focus::Main) if app.quad_on() => {
            &[("enter", "put in a square"), ("4", "the squares"), ("n p", "next square"), ("/", "search"), ("?", "all keys")]
        }
        (_, Screen::Decks, _) => {
            &[("a b", "play"), ("← →", "crossfade"), ("m", "mix in"), ("s", "sync"), ("uio jkl", "eq"), ("x", "bass here"), ("tab", "deck"), (",.", "seek"), ("[ ]", "tempo"), ("esc", "songs"), ("D", "off")]
        }
        (_, Screen::Browse, Focus::Main) if app.decks_on() => {
            &[("1 2", "load deck A / B"), ("enter", "load a deck"), ("m", "mix in"), ("D", "decks"), ("/", "search"), ("?", "all keys")]
        }
        (_, Screen::Browse, Focus::Main) if app.selecting_range() => {
            &[("↑↓", "extend the run"), ("V", "done"), ("a", "queue them"), ("enter", "play them")]
        }
        (_, Screen::NowPlaying, _) if app.decks_on() => {
            &[("space", "pause"), (",.", "seek"), ("m", "mix in"), ("↑↓", "lyrics"), ("D", "decks"), ("z", "party"), ("esc", "back")]
        }
        (_, Screen::NowPlaying, _) => {
            &[("space", "pause"), (",.", "seek"), ("n p", "skip"), ("f", "like"), ("c", "cover"), ("D", "decks"), ("z", "party"), ("esc", "back")]
        }
        (_, _, Focus::Sidebar) => &[("enter", "open"), ("→", "tracks"), ("/", "search"), ("v", "now playing"), ("?", "all keys")],
        _ if selecting => &[("enter", "play them"), ("a", "queue them"), ("x", "more"), ("V", "a run"), ("X", "clear"), ("/", "search")],
        _ => &[("enter", "play"), ("x", "select"), ("a", "queue"), ("f", "like"), ("/", "search"), ("v", "now playing"), ("D", "decks"), ("?", "all keys")],
    };
    let limit = area.right().saturating_sub(right_used + 4);
    for (key, label) in hints {
        let need = (key.width() + label.width() + 4) as u16;
        if x + need > limit {
            break;
        }
        x += put(buf, x, y, 12, key, Style::default().fg(th.dim).add_modifier(Modifier::BOLD));
        x += 1;
        x += put(buf, x, y, 16, label, Style::default().fg(th.faint));
        x += 3;
    }
}

// ---- overlays -------------------------------------------------------------

/// Draw a centred rounded box and return the area inside it.
fn popup(buf: &mut Buffer, th: &Theme, screen: Rect, w: u16, h: u16, y: Option<u16>, title: &str) -> Rect {
    let w = w.min(screen.width.saturating_sub(4));
    let h = h.min(screen.height.saturating_sub(2));
    let rect = Rect {
        x: screen.x + (screen.width - w) / 2,
        y: y.unwrap_or(screen.y + (screen.height - h) / 2),
        width: w,
        height: h,
    };
    let bg = art::mix(th.bg, (255, 255, 255), 0.07);
    let border = Style::default().fg(rgb(art::mix(th.bg, th.accent_rgb, 0.6))).bg(rgb(bg));
    for yy in rect.y..rect.bottom() {
        for xx in rect.x..rect.right() {
            if let Some(cell) = buf.cell_mut((xx, yy)) {
                cell.reset();
                cell.set_style(Style::default().bg(rgb(bg)).fg(th.text));
            }
        }
    }
    for xx in rect.x + 1..rect.right() - 1 {
        set(buf, xx, rect.y, '─', border);
        set(buf, xx, rect.bottom() - 1, '─', border);
    }
    for yy in rect.y + 1..rect.bottom() - 1 {
        set(buf, rect.x, yy, '│', border);
        set(buf, rect.right() - 1, yy, '│', border);
    }
    set(buf, rect.x, rect.y, '╭', border);
    set(buf, rect.right() - 1, rect.y, '╮', border);
    set(buf, rect.x, rect.bottom() - 1, '╰', border);
    set(buf, rect.right() - 1, rect.bottom() - 1, '╯', border);
    if !title.is_empty() {
        put(buf, rect.x + 2, rect.y, w.saturating_sub(4), &format!(" {title} "), Style::default().fg(th.accent).bg(rgb(bg)).add_modifier(Modifier::BOLD));
    }
    Rect { x: rect.x + 2, y: rect.y + 1, width: w.saturating_sub(4), height: h.saturating_sub(2) }
}

const HELP: &[(&str, &[(&str, &str)])] = &[
    (
        "Playback",
        &[
            ("space", "play / pause"),
            ("n  p", "next / previous"),
            (",  .", "seek 5 s (hold to scrub)"),
            ("<  >", "seek 30 seconds"),
            ("-  +", "volume"),
            ("s  r", "shuffle / repeat"),
            ("m  M", "crossfade songs / length"),
            ("d  u", "devices / up next"),
        ],
    ),
    (
        "Browse",
        &[
            ("↑↓ j k", "move"),
            ("← →", "sidebar / list"),
            ("enter", "play or open"),
            ("tab", "next tab"),
            ("[  ]", "previous / next tab"),
            ("pgup pgdn", "jump ten rows"),
            ("esc", "back"),
            ("/", "search Spotify"),
            ("ctrl-f", "filter this list"),
            ("g  G", "top / bottom"),
            ("R", "refresh"),
        ],
    ),
    (
        "Build a queue",
        &[
            ("x", "select (or ctrl-click)"),
            ("J  K", "select while moving"),
            ("V", "run: V, move, V again"),
            ("a", "queue selection or song"),
            ("enter", "play the selection now"),
            ("X", "clear the selection"),
        ],
    ),
    (
        "DJ decks",
        &[
            ("D", "decks (again: off)"),
            ("1  2", "load song on deck A / B"),
            ("a  b", "play / pause deck A / B"),
            ("← →", "crossfader (↓ centre)"),
            ("m  M", "mix in / mix length"),
            ("tab", "deck the keys act on"),
            ("u i o", "low / mid / high up"),
            ("j k l", "low / mid / high down"),
            ("J K L", "kill (U I O: flat)"),
            ("x", "bass to this deck"),
            ("s", "match tempo and beat"),
            ("[ ] 0", "tempo down / up / reset"),
            ("{ }", "nudge earlier / later"),
        ],
    ),
    (
        "Four squares",
        &[
            ("4", "four squares (again: off)"),
            ("← → ↑ ↓", "pick the next song"),
            ("shift ←→", "drop it: stutter, then in"),
            ("click", "that square next"),
            ("enter", "stutter the song playing"),
            ("tab", "a square chosen for you"),
            ("enter", "on a song: into a square"),
        ],
    ),
    (
        "Track & view",
        &[
            ("f  F", "like song / like playing"),
            ("o  A", "go to album / artist"),
            ("v", "now playing + lyrics"),
            ("c", "cover style (pulse, depth)"),
            ("z", "party mode"),
            ("?  q", "this help / quit"),
        ],
    ),
];

/// Lines a column of help sections takes: a heading and its keys each, with
/// a blank line between sections.
fn help_rows(sections: &[usize]) -> usize {
    sections.iter().map(|&n| HELP[n].1.len() + 1).sum::<usize>() + sections.len().saturating_sub(1)
}

/// Lay the help out as pages of `cols` columns, each `rows` lines tall. A
/// section is never split, so a short window gets more pages, not cut-off keys.
fn help_pages(cols: usize, rows: usize) -> Vec<Vec<Vec<usize>>> {
    let mut pages: Vec<Vec<Vec<usize>>> = vec![vec![Vec::new()]];
    for n in 0..HELP.len() {
        let page = pages.last_mut().unwrap();
        // The first column on this page with room for it.
        let fits = page.iter().position(|col| {
            let mut with = col.clone();
            with.push(n);
            col.is_empty() || help_rows(&with) <= rows
        });
        match fits {
            Some(c) => page[c].push(n),
            None if page.len() < cols => page.push(vec![n]),
            None => pages.push(vec![vec![n]]),
        }
    }
    pages
}

fn draw_overlay(buf: &mut Buffer, app: &mut App, th: &Theme, screen: Rect) {
    match &app.overlay {
        Overlay::None | Overlay::Filter => {}
        Overlay::Help { page } => {
            let cols: usize = if screen.width >= 126 { 3 } else if screen.width >= 80 { 2 } else { 1 };
            // Rows left for keys once the frame, a line of air and the footer are taken.
            let room = screen.height.saturating_sub(6).max(1) as usize;
            let pages = help_pages(cols, room);
            let tallest = pages.iter().flatten().map(|col| help_rows(col)).max().unwrap_or(0);
            let inner = popup(buf, th, screen, [48, 88, 122][cols - 1], tallest as u16 + 4, None, "Keys");
            let shown = *page % pages.len();
            let footer = inner.bottom().saturating_sub(1);
            let mut note_w = 0;
            if pages.len() > 1 {
                let note = format!("↓ more keys · {} of {}", shown + 1, pages.len());
                note_w = put_right(buf, inner.right(), footer, inner.width, &note, Style::default().fg(th.accent));
            }
            if !app.web_api() {
                put(
                    buf,
                    inner.x + 1,
                    footer,
                    inner.width.saturating_sub(note_w + 3),
                    "`riff setup` adds playlist search, Recently Played, remote control",
                    Style::default().fg(th.faint),
                );
            }
            // A page that doesn't need every column gives the rest of the room to the text.
            let col_w = inner.width / pages[shown].len().max(1) as u16;
            for (c, sections) in pages[shown].iter().enumerate() {
                let x = inner.x + 1 + c as u16 * col_w;
                let mut y = inner.y + 1;
                for &n in sections {
                    let (section, keys) = HELP[n];
                    if y >= footer {
                        break;
                    }
                    put(buf, x, y, col_w, section, Style::default().fg(th.accent).add_modifier(Modifier::BOLD));
                    y += 1;
                    for (key, what) in keys {
                        if y >= footer {
                            break;
                        }
                        put(buf, x, y, 10, key, Style::default().fg(th.text));
                        put(buf, x + 11, y, col_w.saturating_sub(12), what, Style::default().fg(th.dim));
                        y += 1;
                    }
                    y += 1;
                }
            }
        }
        Overlay::Search { input } => {
            let w = 64.min(screen.width.saturating_sub(6));
            let inner = popup(buf, th, screen, w, 3, Some(screen.y + screen.height / 5), "Search Spotify");
            // Show the tail of long queries so the cursor is always visible.
            let room = inner.width.saturating_sub(2) as usize;
            let mut shown: String = input.clone();
            while shown.width() > room {
                shown.remove(0);
            }
            let used = put(buf, inner.x, inner.y, inner.width, &shown, Style::default().fg(th.text));
            put(buf, inner.x + used, inner.y, 1, "▏", Style::default().fg(th.accent));
            if input.is_empty() {
                put(buf, inner.x + 1, inner.y, inner.width - 1, "songs, albums, artists, playlists", Style::default().fg(th.faint));
            }
        }
        Overlay::Queue { sel } => {
            let sel = *sel;
            let h = (app.queue.len() as u16 + 4).max(6).min(screen.height.saturating_sub(4));
            let inner = popup(buf, th, screen, 70, h, None, "Up next");
            if app.queue.is_empty() {
                let msg = if app.pb.shuffle && !app.web_api() {
                    "Shuffle is on, so the player picks the order as it goes."
                } else {
                    "Nothing up next. Press a on a track to queue it."
                };
                put(buf, inner.x + 1, inner.y + 1, inner.width, msg, Style::default().fg(th.dim));
                return;
            }
            let rows = inner.height as usize;
            let top = sel.saturating_sub(rows.saturating_sub(1));
            for (line, (i, t)) in app.queue.iter().enumerate().skip(top).take(rows).enumerate() {
                let y = inner.y + line as u16;
                let row = Rect { x: inner.x - 1, y, width: inner.width + 2, height: 1 };
                app.hits.push((row, Hit::OverlayRow(i)));
                if i == sel {
                    fill(buf, row, Style::default().bg(th.sel));
                }
                put_right(buf, inner.x + 3, y, 3, &(i + 1).to_string(), Style::default().fg(th.faint));
                let name_w = (inner.width - 12) * 55 / 100;
                put(buf, inner.x + 5, y, name_w, &t.name, Style::default().fg(th.text));
                put(buf, inner.x + 6 + name_w, y, inner.width.saturating_sub(name_w + 14), &t.artist_line(), Style::default().fg(th.dim));
                put_right(buf, inner.right(), y, 6, &fmt_ms(t.duration_ms), Style::default().fg(th.faint));
            }
        }
        Overlay::Devices { sel } => {
            let sel = *sel;
            let h = (app.pb.devices.len() as u16 + 4).max(6).min(screen.height.saturating_sub(4));
            let inner = popup(buf, th, screen, 56, h, None, "Play on");
            if app.pb.devices.is_empty() {
                put(buf, inner.x + 1, inner.y + 1, inner.width, "No devices yet. Still connecting, or nothing is open.", Style::default().fg(th.dim));
                return;
            }
            for (i, d) in app.pb.devices.iter().enumerate().take(inner.height as usize) {
                let y = inner.y + 1 + i as u16;
                if y >= inner.bottom() {
                    break;
                }
                let row = Rect { x: inner.x - 1, y, width: inner.width + 2, height: 1 };
                app.hits.push((row, Hit::OverlayRow(i)));
                if i == sel {
                    fill(buf, row, Style::default().bg(th.sel));
                }
                // Local events know first whether this terminal is the one playing.
                let active = if d.this { app.pb.local } else { d.active && !app.pb.local };
                put(buf, inner.x + 1, y, 1, if active { "●" } else { "○" }, Style::default().fg(if active { th.accent } else { th.faint }));
                let label = if d.this { format!("{} (this terminal)", d.name) } else { d.name.clone() };
                put(buf, inner.x + 3, y, inner.width.saturating_sub(18), &label, Style::default().fg(th.text));
                put_right(buf, inner.right(), y, 12, &d.kind, Style::default().fg(th.faint));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::demo;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn party_frame(level: f32) -> Buffer {
        let mut app = demo::app(Config::default());
        app.party = true;
        app.bars = [level; BANDS];
        // No beat, so rows stay where they are.
        app.beat = 0.0;
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn help_shows_every_section_whole_at_any_size() {
        for (cols, rows) in [(1, 6), (1, 18), (2, 18), (2, 30), (3, 20), (3, 4)] {
            let pages = help_pages(cols, rows);
            let mut seen: Vec<usize> = pages.iter().flatten().flatten().copied().collect();
            seen.sort();
            assert_eq!(seen, (0..HELP.len()).collect::<Vec<_>>(), "{cols} columns of {rows}");
            for column in pages.iter().flatten() {
                assert!(column.len() == 1 || help_rows(column) <= rows, "{cols} columns of {rows}: a column runs over");
            }
            assert!(pages.iter().all(|page| page.len() <= cols));
        }
    }

    #[test]
    fn pop_ups_survive_a_very_short_window() {
        use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        for height in 8..14 {
            for key in ['u', 'd', '?'] {
                let mut app = demo::app(Config::default());
                app.on_term(Event::Key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE)));
                let mut terminal = Terminal::new(TestBackend::new(40, height)).unwrap();
                terminal.draw(|f| draw(f, &mut app)).unwrap();
            }
        }
    }

    #[test]
    fn party_spectrum_comes_in_from_every_edge() {
        let (quiet, loud) = (party_frame(0.0), party_frame(0.5));
        let glow = |buf: &Buffer, x: u16, y: u16| match buf[(x, y)].bg {
            Color::Rgb(r, g, b) => r as u32 + g as u32 + b as u32,
            _ => 0,
        };
        for (name, x, y) in [("top", 60, 0), ("bottom", 60, 39), ("left", 0, 20), ("right", 119, 20)] {
            assert!(glow(&loud, x, y) > glow(&quiet, x, y), "nothing reaches in from the {name} edge");
        }
        assert_eq!(glow(&loud, 60, 20), glow(&quiet, 60, 20), "half-height bars should not reach the middle");
    }
}
