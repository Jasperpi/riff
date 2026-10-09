//! Everything drawn on screen. Pure function of `App`, apart from recording
//! where clickable things ended up and caching rendered artwork.

use std::borrow::Cow;
use std::sync::Arc;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, Focus, Hit, LyricState, Overlay, Page, Screen};
use crate::art::{self, ArtMode, Rgb};
use crate::engine::Conn;
use crate::model::*;
use crate::viz::BANDS;

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
    let mode = if app.art_mode == ArtMode::Off { ArtMode::Blocks } else { app.art_mode };
    let key = (url.to_string(), area.width, area.height, mode);
    let rendered = match app.art_cache.get(&key) {
        Some(r) => r.clone(),
        None => {
            let r = Arc::new(art::render(&image, area.width, area.height, mode));
            if app.art_cache.len() > 48 {
                app.art_cache.clear();
            }
            app.art_cache.insert(key, r.clone());
            r
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
    let buf = f.buffer_mut();
    fill(buf, area, Style::default().bg(rgb(th.bg)).fg(th.text));

    if area.width < 36 || area.height < 8 {
        put_centered(buf, area, area.y + area.height / 2, "riff needs a bigger window", Style::default().fg(th.dim));
        return;
    }

    let bar_h: u16 = if area.height >= 18 { 5 } else { 2 };
    let body = Rect { height: area.height - bar_h - 1, ..area };
    let bar = Rect { y: body.bottom(), height: bar_h, ..area };
    let hints = Rect { y: bar.bottom(), height: 1, ..area };

    match app.screen {
        Screen::Browse => draw_browse(buf, app, &th, body),
        Screen::NowPlaying => draw_now_playing(buf, app, &th, body),
    }
    draw_player(buf, app, &th, bar);
    draw_hints(buf, app, &th, hints);
    draw_overlay(buf, app, &th, area);
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
    draw_list(buf, &mut app.hits, th, list, page, focused, frame, &playing_uri, is_playing, eq, liked);
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
                if current && is_playing {
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

    let show_art = app.art_mode != ArtMode::Off && top.height >= 9 && top.width >= 50;
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

// ---- player bar -----------------------------------------------------------

fn draw_player(buf: &mut Buffer, app: &mut App, th: &Theme, bar: Rect) {
    fill(buf, bar, Style::default().bg(rgb(th.panel)));
    let compact = bar.height < 5;
    let track = app.pb.track.clone();
    let pos = app.pb.position();
    let dur = track.as_ref().map(|t| t.duration_ms).unwrap_or(0);

    // Left: artwork tile (also a button to open the now-playing screen).
    let mut x = bar.x + 2;
    if !compact && app.art_mode != ArtMode::Off && app.screen == Screen::Browse {
        let art_rect = Rect { x: bar.x + 2, y: bar.y + 1, width: 6, height: 3 };
        let url = track.as_ref().and_then(|t| t.image.clone());
        draw_art(buf, app, th, art_rect, url.as_deref());
        app.hits.push((art_rect, Hit::NowPlayingArt));
        x = art_rect.right() + 2;
    }

    let right_w: u16 = if bar.width >= 96 { 30 } else if bar.width >= 72 { 18 } else { 0 };
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
            put(buf, sx, sy, 9, rep, on(app.pb.repeat != Repeat::Off));
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

    let hints: &[(&str, &str)] = match (&app.overlay, app.screen, app.focus) {
        (Overlay::Queue { .. }, _, _) => &[("↑↓", "browse"), ("esc", "close")],
        (Overlay::Devices { .. }, _, _) => &[("enter", "play there"), ("esc", "close")],
        (_, Screen::NowPlaying, _) => {
            &[("space", "pause"), (",.", "seek"), ("n p", "skip"), ("f", "like"), ("c", "cover style"), ("↑↓", "lyrics"), ("esc", "back")]
        }
        (_, _, Focus::Sidebar) => &[("enter", "open"), ("→", "tracks"), ("/", "search"), ("v", "now playing"), ("?", "all keys")],
        _ => &[("enter", "play"), ("a", "queue"), ("f", "like"), ("/", "search"), ("o", "album"), ("v", "now playing"), ("?", "all keys")],
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
            (",  .", "seek 5 seconds"),
            ("<  >", "seek 30 seconds"),
            ("-  +", "volume"),
            ("s", "shuffle"),
            ("r", "repeat: off, all, one"),
            ("d", "choose device"),
            ("u", "up next"),
        ],
    ),
    (
        "Browse",
        &[
            ("↑↓ j k", "move"),
            ("← →", "sidebar / list"),
            ("enter", "play or open"),
            ("tab", "next tab"),
            ("esc", "back"),
            ("/", "search Spotify"),
            ("ctrl-f", "filter this list"),
            ("g  G", "top / bottom"),
            ("R", "refresh"),
        ],
    ),
    (
        "Track",
        &[
            ("a", "add to queue"),
            ("f", "like / unlike"),
            ("F", "like what's playing"),
            ("o", "go to album"),
            ("A", "go to artist"),
        ],
    ),
    (
        "View",
        &[
            ("v", "now playing + lyrics"),
            ("c", "cover style"),
            ("?", "this help"),
            ("q", "quit"),
        ],
    ),
];

fn draw_overlay(buf: &mut Buffer, app: &mut App, th: &Theme, screen: Rect) {
    match &app.overlay {
        Overlay::None | Overlay::Filter => {}
        Overlay::Help => {
            let two_cols = screen.width >= 84;
            let (w, h) = if two_cols { (80, 26) } else { (44, screen.height.saturating_sub(2)) };
            let inner = popup(buf, th, screen, w, h, None, "Keys");
            if !app.web_api() && inner.height > 3 {
                put(
                    buf,
                    inner.x + 1,
                    inner.bottom() - 1,
                    inner.width.saturating_sub(2),
                    "`riff setup` adds playlist search, Recently Played, remote control",
                    Style::default().fg(th.faint),
                );
            }
            let col_w = if two_cols { inner.width / 2 } else { inner.width };
            let mut col = 0u16;
            let mut y = inner.y + 1;
            for (n, (section, keys)) in HELP.iter().enumerate() {
                if two_cols && n == 2 {
                    col = 1;
                    y = inner.y + 1;
                }
                let x = inner.x + 1 + col * col_w;
                if y >= inner.bottom() {
                    break;
                }
                put(buf, x, y, col_w, section, Style::default().fg(th.accent).add_modifier(Modifier::BOLD));
                y += 1;
                for (key, what) in *keys {
                    if y >= inner.bottom() {
                        break;
                    }
                    put(buf, x, y, 9, key, Style::default().fg(th.text));
                    put(buf, x + 10, y, col_w.saturating_sub(12), what, Style::default().fg(th.dim));
                    y += 1;
                }
                y += 1;
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
            let h = (app.queue.len() as u16 + 4).clamp(6, screen.height.saturating_sub(4));
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
            let h = (app.pb.devices.len() as u16 + 4).clamp(6, screen.height.saturating_sub(4));
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
