//! Rendering. The screen shows raw source lines with a gutter; structure is
//! expressed purely through highlighting, so there is no rendered->source
//! mapping to get wrong.
//!
//! Selections can start and end mid-line, so each rendered line is cut into
//! styled segments at the union of the selection and cursor boundaries. All
//! cuts land on character boundaries — the columns coming out of comrak are
//! byte offsets, and slicing a multi-byte character in half panics.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::{Anchor, App, Mode};
use crate::wrap::{cells, wrap, Piece};

pub fn draw(f: &mut Frame, app: &mut App, scroll: &mut Anchor) {
    // The comment box grows with the comment, up to a point.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "clamp(1, 8) + 2 is 3..=10, which fits u16. `expect` rather than \
                  `allow` so the build breaks if this stops being the only lossy \
                  cast in the crate."
    )]
    let input_h = if app.mode == Mode::Input {
        (app.editor.rows().len().clamp(1, 8) + 2) as u16
    } else {
        0
    };
    let chunks = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(input_h),
        Constraint::Length(6),
        Constraint::Length(1),
    ])
    .split(f.area());

    draw_source(f, chunks[0], app, scroll);
    if app.mode == Mode::Input {
        draw_input(f, chunks[1], app);
    }
    draw_annotations(f, chunks[2], app);
    draw_footer(f, chunks[3], app);
    if app.peek {
        draw_peek(f, chunks[0], app);
    }
}

/// Colours for the markdown syntax tags produced by `highlight`.
fn syntax_style(tag: &str) -> Style {
    let s = Style::default();
    match tag {
        "heading" => s.fg(Color::Cyan).add_modifier(Modifier::BOLD),
        "heading-marker" => s.fg(Color::Blue).add_modifier(Modifier::DIM),
        "list-marker" => s.fg(Color::Blue),
        "code" => s.fg(Color::Indexed(108)),
        "code-span" => s.fg(Color::Indexed(180)),
        "link" => s.fg(Color::Blue).add_modifier(Modifier::UNDERLINED),
        "image" => s.fg(Color::Magenta),
        "strong" => s.add_modifier(Modifier::BOLD),
        "emph" => s.add_modifier(Modifier::ITALIC),
        "strike" => s.add_modifier(Modifier::CROSSED_OUT),
        "html" => s.fg(Color::DarkGray),
        "hr" => s.fg(Color::DarkGray),
        "quote" => s.fg(Color::Indexed(245)).add_modifier(Modifier::ITALIC),
        "table" => s.fg(Color::DarkGray),
        "cell" => s.fg(Color::Reset),
        _ => s,
    }
}

/// Cut `text` into styled segments. Later marks win over earlier ones, and
/// every cut is forced onto a character boundary.
fn segments(text: &str, marks: &[(usize, usize, Style)], base: Style) -> Vec<Span<'static>> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut bounds = vec![0usize, text.len()];
    for (a, b, _) in marks {
        bounds.push(*a);
        bounds.push(*b);
    }
    bounds.retain(|&b| b <= text.len() && text.is_char_boundary(b));
    bounds.sort_unstable();
    bounds.dedup();

    let mut out = Vec::new();
    for w in bounds.windows(2) {
        let (a, b) = (w[0], w[1]);
        if a >= b {
            continue;
        }
        let style = marks
            .iter()
            .rev()
            .find(|(ma, mb, _)| *ma <= a && b <= *mb)
            .map_or(base, |(_, _, s)| *s);
        out.push(Span::styled(text[a..b].to_string(), style));
    }
    out
}

/// Line-number field floor. The gutter is this plus two cells: the annotation
/// dot and the selection bar.
const LINENO_MIN: usize = 4;

/// Byte index of the cursor within `text`, floored onto a character boundary.
/// `App::snap` normally guarantees this, but tests assign `cursor` directly and
/// slicing mid-character panics.
fn cursor_byte(app: &App, text: &str) -> usize {
    let mut i = app.cursor.col.saturating_sub(1).min(text.len());
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// The style of the byte at `byte`, by the same later-wins rule `segments`
/// uses. Alignment padding is in no file and so has no style of its own; it
/// takes the one from the byte it was inserted after, which is what keeps a
/// selected cell one continuous block instead of stitching at every column.
fn style_at(marks: &[(usize, usize, Style)], byte: usize) -> Style {
    marks
        .iter()
        .rev()
        .find(|&&(a, b, _)| byte >= a && byte < b)
        .map_or_else(Style::default, |&(_, _, st)| st)
}

/// Syntax first, then selection, then the cursor: later marks win, so
/// highlighting never hides where you are or what you have chosen.
fn line_marks(
    app: &App,
    lineno: usize,
    text: &str,
    sel_style: Style,
    cur_style: Style,
) -> Vec<(usize, usize, Style)> {
    let mut marks: Vec<(usize, usize, Style)> = app
        .marks
        .get(lineno - 1)
        .map(|row| {
            row.iter()
                .map(|(a, b, tag)| (*a, *b, syntax_style(tag)))
                .collect()
        })
        .unwrap_or_default();

    if let Some((a, b)) = app.selected_bytes_on(lineno) {
        marks.push((a, b, sel_style));
    }
    if lineno == app.cursor.line {
        let c0 = cursor_byte(app, text);
        let c1 = text[c0..]
            .char_indices()
            .nth(1)
            .map_or(text.len(), |(i, _)| c0 + i);
        if c1 > c0 {
            marks.push((c0, c1, cur_style));
        }
    }
    marks
}

fn draw_source(f: &mut Frame, area: Rect, app: &mut App, scroll: &mut Anchor) {
    let viewport = area.height.saturating_sub(2) as usize;
    app.viewport = viewport;

    let current = app.current_block();
    let sel_style = Style::default().bg(Color::Indexed(238)).fg(Color::White);
    let cur_style = Style::default()
        .bg(Color::Yellow)
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(source_title(app, area.width));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // The gutter is its own column. Nothing that happens to the body — running
    // off the edge, or one day scrolling sideways — can push the line number,
    // the annotation dot or the selection bar off the screen with it.
    let lineno_w = app.lines.len().to_string().len().max(LINENO_MIN);
    let gutter_w = u16::try_from(lineno_w + 2)
        .unwrap_or(6)
        .min(inner.width.saturating_sub(1));
    let cols = Layout::horizontal([Constraint::Length(gutter_w), Constraint::Min(0)]).split(inner);
    let (gutter_area, body_area) = (cols[0], cols[1]);
    let body_w = usize::from(body_area.width);

    // Published before anything reads row space: `line_rows` wraps to this, so
    // the anchor, the cursor row and the paging step are all computed against
    // the width actually on screen this frame.
    app.body_width = body_w;
    keep_cursor_visible(app, scroll, viewport);

    let mut gutter: Vec<Line> = Vec::with_capacity(viewport);
    let mut body: Vec<Line> = Vec::with_capacity(viewport);
    // Rows whose line runs past the right edge, and the colour to say so in.
    // Always empty while wrapping: nothing runs past the edge.
    let mut overflow: Vec<(usize, Style)> = Vec::new();

    let mut lineno = scroll.line;
    let mut idx = 0usize;
    while idx < viewport && lineno <= app.lines.len() {
        // One space per tab, not a tab stop. ratatui drops control characters
        // outright, so a tab used to vanish and shift every column after it left
        // by one. A single space is one byte for one cell, which keeps the byte
        // column the screen shows identical to the byte column that goes in the
        // JSON — worth more here than visually correct indentation.
        let text = app.display_line(lineno);
        let on_cursor_line = lineno == app.cursor.line;
        let in_current = current.is_some_and(|c| app.blocks[c].contains_line(lineno));
        let selected_here = app.line_selected(lineno);
        let n = app.annotations_on(lineno);
        let marks = line_marks(app, lineno, &text, sel_style, cur_style);
        let (rows, indent) = app.line_rows(lineno);
        let pad = " ".repeat(indent);
        let first_row = if lineno == scroll.line { scroll.row } else { 0 };

        for (k, pieces) in rows.iter().enumerate().skip(first_row) {
            if idx == viewport {
                break;
            }
            // The number and the dot mark the line, so they go on its first row
            // only — repeated, they would read as separate lines. The selection
            // bar marks every row: the line is still selected halfway down it.
            let head = k == 0;
            gutter.push(Line::from(vec![
                Span::styled(
                    if head {
                        format!("{lineno:>lineno_w$}")
                    } else {
                        " ".repeat(lineno_w)
                    },
                    Style::default().fg(if on_cursor_line {
                        Color::Yellow
                    } else {
                        Color::DarkGray
                    }),
                ),
                // The dot used to trail the text, which made it the first thing
                // truncated — an annotation you could not see you had made.
                Span::styled(
                    if n > 0 && head { "●" } else { " " },
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    if in_current || selected_here {
                        "▍"
                    } else {
                        " "
                    },
                    Style::default().fg(if selected_here {
                        Color::Yellow
                    } else {
                        Color::DarkGray
                    }),
                ),
            ]));

            let mut spans = if k == 0 || pad.is_empty() {
                Vec::new()
            } else {
                vec![Span::raw(pad.clone())]
            };
            // Alignment padding is the only thing here that is not in the file,
            // and it is inserted between bytes rather than over them — so the
            // source pieces are still windows into the line and marks still
            // rebase by clipping.
            for piece in pieces {
                match *piece {
                    Piece::Src(s, e) => {
                        // Marks are byte ranges into the whole line; a piece is
                        // a byte window into it. Clip to the window, then shift
                        // into it — the partition property is what makes this
                        // exact.
                        let row_marks: Vec<(usize, usize, Style)> = marks
                            .iter()
                            .filter(|&&(a, b, _)| b > s && a < e)
                            .map(|&(a, b, st)| (a.max(s) - s, b.min(e) - s, st))
                            .collect();
                        spans.extend(segments(&text[s..e], &row_marks, Style::default()));
                    }
                    Piece::Pad { n, fill, anchor } => spans.push(Span::styled(
                        fill.to_string().repeat(n),
                        style_at(&marks, anchor),
                    )),
                }
            }
            if spans.is_empty() && on_cursor_line {
                spans.push(Span::styled(" ", cur_style));
            }
            let row = Line::from(spans);
            if row.width() > body_w {
                // A cursor past the edge leaves no cursor cell on screen at all.
                // The marker takes the cursor's colour in that case, so the screen
                // still says where you are — `w`/`b`, `0` and `z` get you back to it.
                let hidden = on_cursor_line && cells(&text[..cursor_byte(app, &text)]) >= body_w;
                let dim = Style::default().fg(Color::DarkGray);
                overflow.push((idx, if hidden { cur_style } else { dim }));
            }
            body.push(row);
            idx += 1;
        }
        lineno += 1;
    }

    f.render_widget(Paragraph::new(gutter), gutter_area);
    f.render_widget(Paragraph::new(body), body_area);

    // Overlaid afterwards: the marker replaces whatever the truncated line left
    // in the last cell, which is precisely the character it is warning about.
    //
    // Not while the peek overlay is up. The overlay is inset, so the markers
    // land beside it in the sliver of source still showing — a `›` with no line
    // attached to it, which is the exact ambiguity the marker exists to remove.
    if body_w > 0 && !app.peek {
        let x = body_area.right() - 1;
        for (idx, style) in overflow {
            let Ok(dy) = u16::try_from(idx) else { continue };
            let y = body_area.y + dy;
            if y < body_area.bottom() {
                f.buffer_mut()[(x, y)].set_symbol("›").set_style(style);
            }
        }
    }
}

/// The selection readout goes first: it is the only field that changes on every
/// keypress, and a long path used to push it off the end of the border
/// entirely. The path is last and shortened to whatever is left, because a
/// truncated path is still recognisable and a missing selection is not.
fn source_title(app: &App, width: u16) -> String {
    let sel = match app.selection() {
        Some(s) if s.start.line == s.end.line => format!(
            "{} L{}:{}-{}",
            app.selection_kind(),
            s.start.line,
            s.start.col,
            s.end.col
        ),
        Some(s) => format!("{} L{}-{}", app.selection_kind(), s.start.line, s.end.line),
        None => "—".into(),
    };
    // The cursor column is on screen even when the cursor itself is not.
    let rest = format!(
        " [{}] · L{}:{} · {} lines · {} units · {} annotations · ",
        sel,
        app.cursor.line,
        app.cursor.col,
        app.lines.len(),
        app.blocks.len(),
        app.annotations.len(),
    );
    let budget = usize::from(width).saturating_sub(rest.chars().count() + 3);
    format!("{rest}{} ", shorten_path(app.display_name(), budget))
}

/// Keep the tail of an over-long path — the file name is what identifies it,
/// the directory prefix is what makes it too long.
fn shorten_path(path: &str, max: usize) -> String {
    let n = path.chars().count();
    if n <= max {
        return path.to_string();
    }
    // One column goes to the ellipsis that marks the cut.
    let tail: String = path.chars().skip(n + 1 - max.max(1)).collect();
    format!("…{tail}")
}

/// Scroll so the cursor's *row* is on screen. Every path is O(viewport): the
/// anchor is walked, never indexed, so no part of this is proportional to the
/// document or to the height of the line the cursor happens to be on.
fn keep_cursor_visible(app: &App, scroll: &mut Anchor, viewport: usize) {
    if viewport == 0 {
        return;
    }
    let cur = Anchor {
        line: app.cursor.line,
        row: app.cursor_row(),
    };

    if (cur.line, cur.row) < (scroll.line, scroll.row) {
        // Above the fold: the cursor's row becomes the top row.
        *scroll = cur;
    } else if cur.line >= scroll.line + viewport {
        // Far below. Counting rows down from the old anchor could be a million
        // of them after `G`; walking back up from the cursor is bounded.
        *scroll = app.walk_rows(cur, viewport - 1, false);
    } else {
        // Near enough to count, but a single tall line between the two can
        // still be thousands of rows — so stop counting once it cannot matter.
        let mut n = 0usize;
        let mut a = *scroll;
        while a != cur && n <= viewport {
            match app.step_row(a, true) {
                Some(next) => a = next,
                None => break,
            }
            n += 1;
        }
        if n >= viewport {
            *scroll = app.walk_rows(cur, viewport - 1, false);
        }
    }

    // No blank rows under the last line: the top can go no further than a
    // viewport short of the document's final row.
    let end = app.line_count();
    let last = Anchor {
        line: end,
        row: app.row_count(end) - 1,
    };
    let max_top = app.walk_rows(last, viewport - 1, false);
    if (scroll.line, scroll.row) > (max_top.line, max_top.row) {
        *scroll = max_top;
    }
}

fn draw_input(f: &mut Frame, area: Rect, app: &App) {
    let range = match app.selection() {
        Some(s) if s.start.line == s.end.line && s.start.col == 1 => format!("L{}", s.start.line),
        Some(s) if s.start.line == s.end.line => {
            format!("L{}:{}-{}", s.start.line, s.start.col, s.end.col)
        }
        Some(s) => format!("L{}-{}", s.start.line, s.end.line),
        None => String::new(),
    };
    let title = format!(
        " comment on {} {} — Enter saves · C-j newline · Esc cancels ",
        app.selection_kind(),
        range
    );

    let caret = Style::default().bg(Color::Yellow).fg(Color::Black);
    let (crow, ccol) = app.editor.row_col();
    let rows: Vec<Line> = app
        .editor
        .rows()
        .iter()
        .enumerate()
        .map(|(i, text)| {
            let prompt = Span::styled(
                if i == 0 { "> " } else { "  " },
                Style::default().fg(Color::Yellow),
            );
            let mut spans = vec![prompt];
            if i == crow {
                // Draw the caret as a cell so it is visible on any terminal,
                // including one sitting past the end of the line.
                let c1 = text[ccol..]
                    .char_indices()
                    .nth(1)
                    .map_or(text.len(), |(n, _)| ccol + n);
                spans.extend(segments(
                    text,
                    &[(ccol, c1.max(ccol), caret)],
                    Style::default(),
                ));
                if ccol >= text.len() {
                    spans.push(Span::styled(" ", caret));
                }
            } else {
                spans.push(Span::raw((*text).to_string()));
            }
            Line::from(spans)
        })
        .collect();

    f.render_widget(
        Paragraph::new(rows).block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(Color::Yellow)),
        ),
        area,
    );
}

/// Centre a rect inside `area`, inset by `dx`/`dy` on each side.
fn inset(area: Rect, dx: u16, dy: u16) -> Rect {
    let w = area.width.saturating_sub(dx * 2).max(1).min(area.width);
    let h = area.height.saturating_sub(dy * 2).max(1).min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

/// The selection, wrapped, over the source view. Read-only: it answers "what
/// did I actually select" for a block whose lines run off the edge, without
/// touching the cursor or the one-line-per-row mapping underneath.
fn draw_peek(f: &mut Frame, area: Rect, app: &mut App) {
    let popup = inset(area, 2, 1);
    let text = app.peek_text().replace('\t', " ");
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(popup);

    let rows = wrap(&text, usize::from(inner.width));
    app.peek_rows = rows.len();
    let top = app.peek_scroll.min(rows.len().saturating_sub(1));
    let height = usize::from(inner.height);
    let shown: Vec<Line> = rows
        .iter()
        .skip(top)
        .take(height)
        .map(|r| Line::raw(r.clone()))
        .collect();

    let more = if rows.len() > height {
        format!(
            " {}-{}/{}",
            top + 1,
            (top + height).min(rows.len()),
            rows.len()
        )
    } else {
        String::new()
    };
    let title = format!(
        " peek: {}{} — j/k scroll · z closes ",
        app.selection_kind(),
        more
    );

    f.render_widget(Clear, popup);
    f.render_widget(Paragraph::new(shown).block(block.title(title)), popup);
}

fn draw_annotations(f: &mut Frame, area: Rect, app: &App) {
    let rows: Vec<Line> = if app.annotations.is_empty() {
        vec![Line::from(Span::styled(
            "  no annotations yet — v/V select, +/- widen or narrow, c comment",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        app.annotations
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let loc = if a.whole_lines && a.start_line == a.end_line {
                    format!("L{}", a.start_line)
                } else if a.whole_lines {
                    format!("L{}-{}", a.start_line, a.end_line)
                } else if a.start_line == a.end_line {
                    format!("L{}:{}-{}", a.start_line, a.start_col, a.end_col)
                } else {
                    format!(
                        "L{}:{}-{}:{}",
                        a.start_line, a.start_col, a.end_line, a.end_col
                    )
                };
                let here = app.cursor.line >= a.start_line && app.cursor.line <= a.end_line;
                Line::from(vec![
                    Span::styled(
                        format!(" {} ", if here { "▸" } else { " " }),
                        Style::default().fg(Color::Yellow),
                    ),
                    Span::styled(
                        format!("{} {:<14}", i + 1, loc),
                        Style::default().fg(Color::Magenta),
                    ),
                    Span::styled(
                        format!("{:<14}", a.block_kind),
                        Style::default().fg(Color::DarkGray),
                    ),
                    // The editor advertises C-j, so a comment can be several
                    // lines; a raw span would silently swallow every break.
                    Span::raw(a.text.replace('\n', " ⏎ ")),
                ])
            })
            .collect()
    };

    f.render_widget(
        Paragraph::new(rows).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" annotations "),
        ),
        area,
    );
}

/// Hint lines from fullest to barest. The full one is ~105 columns; in a
/// narrower pane it used to be cut mid-word, losing `c comment · x remove ·
/// q quit` — precisely the part a first-time reader needs. Whichever variant
/// fits the room available wins, and `q quit` survives to the very last.
const KEYS: [&str; 6] = [
    "hjkl move · ^d/^u/^f/^b page · J/K unit · w/b inline · v units · V lines · +/- widen/narrow · z peek · c comment · x remove · q quit",
    "hjkl · J/K unit · w/b inline · v/V select · +/- widen · z peek · c comment · x remove · q quit",
    "hjkl · w/b · v/V · +/- · z peek · c comment · x remove · q quit",
    "w/b · z peek · c comment · x remove · q quit",
    "c comment · x remove · q quit",
    "q quit",
];
const INPUT_KEYS: [&str; 2] = ["Enter save · Esc cancel", "Enter · Esc"];
const PEEK_KEYS: [&str; 2] = ["j/k scroll · z or Esc closes", "z closes"];

const STATUS_W: u16 = 28;

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let width = usize::from(area.width);
    // The status field is a luxury; the key hints are the only documentation
    // on screen. Below the width where both fit, the status goes.
    let bare = KEYS[KEYS.len() - 1].chars().count();
    let status_w = if width >= usize::from(STATUS_W) + bare + 2 {
        STATUS_W
    } else {
        0
    };
    // One column for the leading space.
    let room = width.saturating_sub(usize::from(status_w) + 1);
    let table: &[&str] = if app.mode == Mode::Input {
        &INPUT_KEYS
    } else if app.peek {
        &PEEK_KEYS
    } else {
        &KEYS
    };
    let keys = table
        .iter()
        .copied()
        .find(|k| k.chars().count() <= room)
        .unwrap_or("q");

    let cols = Layout::horizontal([Constraint::Min(0), Constraint::Length(status_w)]).split(area);
    f.render_widget(
        Paragraph::new(Span::styled(
            format!(" {keys}"),
            Style::default().fg(Color::DarkGray),
        )),
        cols[0],
    );
    f.render_widget(
        Paragraph::new(Span::styled(
            format!("{:>27} ", app.status),
            Style::default().fg(Color::Cyan),
        )),
        cols[1],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::blocks::Pos;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    const DOC: &str = "# Steps\n\n- [ ] Add validation to the login form\n- [ ] Write tests\n\nUse `parse_document` here.\n";

    /// Render into an in-memory backend and return the screen as text. The only
    /// way to exercise the drawing code without a tty.
    /// The raw cells, for the handful of assertions that are about *style* —
    /// the string harness below throws styles away, which is exactly how the
    /// vanishing cursor went unnoticed.
    fn render_buf(app: &mut App, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut scroll = Anchor::default();
        term.draw(|f| draw(f, app, &mut scroll)).unwrap();
        term.backend().buffer().clone()
    }

    /// Is there a cursor cell anywhere on screen? Yellow background, and only
    /// the cursor uses it.
    fn has_cursor(buf: &ratatui::buffer::Buffer) -> bool {
        (0..buf.area.height)
            .any(|y| (0..buf.area.width).any(|x| buf[(x, y)].style().bg == Some(Color::Yellow)))
    }

    fn render(app: &mut App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut scroll = Anchor::default();
        term.draw(|f| draw(f, app, &mut scroll)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_source_with_gutter_and_header() {
        let mut app = App::new("PLAN.md".into(), DOC);
        let screen = render(&mut app, 100, 24);
        assert!(screen.contains("6 lines · 4 units · 0 annotations · PLAN.md"));
        assert!(screen.contains("   1 ▍# Steps"));
        assert!(screen.contains("   3  - [ ] Add validation to the login form"));
        assert!(screen.contains("no annotations yet"));
        assert!(screen.contains("c comment"));
        assert!(screen.contains("q quit"));
    }

    #[test]
    fn renders_a_multi_block_selection_and_its_annotation() {
        let mut app = App::new("PLAN.md".into(), DOC);
        app.move_block(1);
        app.toggle_blocks();
        app.move_block(1);
        let screen = render(&mut app, 100, 24);
        assert!(screen.contains("   3 ▍- [ ] Add validation to the login form"));
        assert!(screen.contains("   4 ▍- [ ] Write tests"));

        app.begin_comment();
        app.editor.set("model layer");
        app.commit_comment();
        let screen = render(&mut app, 100, 24);
        // The dot sits in the gutter, pinned to the line number. It no longer
        // carries the count — `annotations_on` can exceed 1, and the pane below
        // lists each one; what the gutter owes you is "there is something here"
        // at a position no line length can push off the screen.
        // The dot is independent of the selection bar: committing collapsed the
        // selection back onto the cursor's block, so line 3 keeps its dot and
        // loses its bar.
        assert!(screen.contains("   3● "), "{screen}");
        assert!(screen.contains("   4●▍"), "{screen}");
        assert!(screen.contains("L3-4"));
        assert!(screen.contains("model layer"));
    }

    #[test]
    fn a_mid_line_selection_still_renders_the_whole_line() {
        let mut app = App::new("PLAN.md".into(), DOC);
        app.cursor = Pos::new(6, 6);
        app.contract(); // paragraph -> the code span, columns 5..20
        let screen = render(&mut app, 100, 24);
        // segmenting must not drop or duplicate any of the line's text
        assert!(screen.contains("Use `parse_document` here."));
        assert!(screen.contains("code-span L6:5-20"));
    }

    #[test]
    fn multibyte_lines_survive_segmentation() {
        let mut app = App::new("u.md".into(), "Prüfen `köde` hier — ✓ fertig.\n");
        app.cursor = Pos::new(1, 10);
        app.contract();
        let screen = render(&mut app, 100, 12);
        assert!(screen.contains("Prüfen `köde` hier — ✓ fertig."));
    }

    #[test]
    fn input_mode_shows_the_target_range() {
        let mut app = App::new("PLAN.md".into(), DOC);
        app.move_block(1);
        app.toggle_blocks();
        app.move_block(1);
        app.begin_comment();
        let screen = render(&mut app, 100, 24);
        assert!(screen.contains("comment on"));
        assert!(screen.contains("L3-4"));
        assert!(screen.contains("Enter saves"));
        assert!(screen.contains("C-j newline"));
    }

    #[test]
    fn a_multi_line_comment_renders_every_row() {
        let mut app = App::new("PLAN.md".into(), DOC);
        app.begin_comment();
        for c in "first".chars() {
            app.editor.insert(c);
        }
        app.editor.newline();
        for c in "second".chars() {
            app.editor.insert(c);
        }
        let screen = render(&mut app, 100, 24);
        assert!(screen.contains("> first"), "{screen}");
        assert!(screen.contains("  second"), "{screen}");
    }

    #[test]
    fn syntax_marks_do_not_disturb_the_rendered_text() {
        // Highlighting adds styles, never characters.
        let src = "## Steps\n\n- [ ] Use `parse_document` and **mind** the [docs](https://x.dev)\n";
        let mut app = App::new("PLAN.md".into(), src);
        let screen = render(&mut app, 100, 16);
        assert!(screen.contains("## Steps"));
        assert!(
            screen.contains("- [ ] Use `parse_document` and **mind** the [docs](https://x.dev)")
        );
    }

    #[test]
    fn scrolls_to_keep_the_cursor_visible_on_a_long_file() {
        let src: String = (1..=500).map(|i| format!("para {i}\n\n")).collect();
        let mut app = App::new("big.md".into(), &src);
        app.goto_last();
        let screen = render(&mut app, 80, 24);
        assert!(screen.contains("para 500"));
        assert!(!screen.contains("para 1\n"));
    }

    #[test]
    fn renders_an_empty_file_without_panicking() {
        let mut app = App::new("empty.md".into(), "");
        let screen = render(&mut app, 80, 24);
        assert!(screen.contains("0 units"));
    }

    #[test]
    fn survives_a_tiny_terminal() {
        let mut app = App::new("PLAN.md".into(), DOC);
        render(&mut app, 20, 6);
        render(&mut app, 10, 3);
    }

    /// Found by running the TUI in a 95-column pane: the path was first in the
    /// title and long enough to push every other field past the border, so the
    /// live selection readout — the only part that changes — was never visible.
    #[test]
    fn a_long_path_never_costs_the_selection_readout_in_the_title() {
        let long = "/tmp/claude-1000/-home-markus-Stuff-2026-07-27-scratch-marginal/\
                    7d415313-bed3-412e-8fed-f0dd60064d/scratchpad/review.md";
        let mut app = App::new(long.into(), DOC);
        app.cursor = Pos::new(6, 6);
        app.contract();
        let screen = render(&mut app, 95, 24);
        assert!(screen.contains("code-span L6:5-20"), "{screen}");
        assert!(screen.contains("6 lines"), "{screen}");
        assert!(
            screen.contains("review.md"),
            "path tail must survive: {screen}"
        );
        assert!(screen.contains('…'), "long path should be elided: {screen}");
    }

    #[test]
    fn a_short_path_is_left_alone() {
        let mut app = App::new("PLAN.md".into(), DOC);
        let screen = render(&mut app, 95, 24);
        assert!(screen.contains("PLAN.md"));
        assert!(!screen.contains('…'), "{screen}");
    }

    /// The same temp path, labelled: the title names what is being reviewed and
    /// needs no eliding at all, because the label is short by construction.
    #[test]
    fn a_label_replaces_the_path_in_the_title() {
        let long = "/tmp/claude-1000/-home-markus-Stuff-2026-07-27-scratch-marginal/\
                    7d415313-bed3-412e-8fed-f0dd60064d/scratchpad/review.md";
        let mut app = App::new(long.into(), DOC);
        app.label = Some("assistant-message".into());
        let screen = render(&mut app, 95, 24);
        assert!(screen.contains("assistant-message"), "{screen}");
        assert!(!screen.contains("scratchpad"), "{screen}");
        assert!(!screen.contains('…'), "{screen}");
    }

    #[test]
    fn shorten_path_keeps_the_tail_and_respects_the_budget() {
        assert_eq!(shorten_path("short.md", 20), "short.md");
        assert_eq!(shorten_path("/a/b/c/long.md", 8), "…long.md");
        assert_eq!(shorten_path("/a/b/c/long.md", 1), "…");
        assert_eq!(shorten_path("/a/b/c/long.md", 0), "…");
        // multibyte must not be sliced apart
        assert_eq!(shorten_path("/tmp/Prüfen/köde.md", 8), "…köde.md");
    }

    /// The full hint line is ~105 columns; in a 95-column pane it was cut
    /// mid-word, hiding `q quit` from the one reader who needs it most.
    #[test]
    fn narrow_footers_fall_back_to_shorter_key_hints() {
        for w in [120u16, 95, 60, 30, 12] {
            let mut app = App::new("PLAN.md".into(), DOC);
            let screen = render(&mut app, w, 12);
            assert!(
                screen.contains("q quit"),
                "width {w} lost the quit key:\n{screen}"
            );
        }
    }

    // ---- long lines --------------------------------------------------------

    /// 200 columns of prose in a 60-column pane.
    fn long_doc() -> String {
        format!("# H\n\n{}and `code_span` at the end.\n", "word ".repeat(34))
    }

    #[test]
    fn a_line_that_runs_past_the_edge_says_so() {
        let mut app = App::new("long.md".into(), &long_doc());
        app.pretty = false;
        let buf = render_buf(&mut app, 60, 12);
        let row: String = (0..buf.area.width).map(|x| buf[(x, 3)].symbol()).collect();
        assert!(row.contains("   3"), "expected line 3 on row 3: {row}");
        assert!(row.ends_with("›│"), "no truncation marker: {row}");

        // …and a short line does not claim to continue.
        let row1: String = (0..buf.area.width).map(|x| buf[(x, 1)].symbol()).collect();
        assert!(
            !row1.contains('›'),
            "short line marked as truncated: {row1}"
        );
    }

    /// Found by probing: `$` on a long line moved the cursor to a column the
    /// renderer never drew, so no cell on screen had the cursor style at all.
    #[test]
    fn the_cursor_is_never_silently_off_screen() {
        let mut app = App::new("long.md".into(), &long_doc());
        app.pretty = false;
        app.cursor = Pos::new(3, 1);
        app.goto_line_end();
        let buf = render_buf(&mut app, 60, 12);
        assert!(
            has_cursor(&buf),
            "cursor vanished at column {}",
            app.cursor.col
        );
        // it is the truncation marker that carries it
        let x = buf.area.width - 2;
        assert_eq!(buf[(x, 3)].symbol(), "›");
        assert_eq!(buf[(x, 3)].style().bg, Some(Color::Yellow));
    }

    #[test]
    fn the_title_reports_the_cursor_column_even_when_the_cursor_is_not_drawn() {
        let mut app = App::new("long.md".into(), &long_doc());
        app.cursor = Pos::new(3, 1);
        app.goto_line_end();
        let screen = render(&mut app, 60, 12);
        assert!(screen.contains("L3:197"), "{screen}");
    }

    /// The dot used to trail the body text, which made it the first thing
    /// truncated: an annotation you could not see you had made.
    #[test]
    fn an_annotation_on_a_long_line_is_still_visible() {
        let mut app = App::new("long.md".into(), &long_doc());
        app.cursor = Pos::new(3, 1);
        app.begin_comment();
        app.editor.set("too long");
        app.commit_comment();
        let screen = render(&mut app, 60, 12);
        assert!(screen.contains("   3●"), "{screen}");
    }

    #[test]
    fn the_gutter_widens_for_five_digit_line_numbers() {
        let src: String = (1..=10_050).map(|i| format!("p{i}\n\n")).collect();
        let mut app = App::new("huge.md".into(), &src);
        app.goto_last();
        let screen = render(&mut app, 60, 12);
        assert!(screen.contains("20099"), "{screen}");
    }

    // ---- peek --------------------------------------------------------------

    /// ratatui's `styled_graphemes` filters control characters, so a tab was
    /// deleted rather than expanded and every column after it drifted left by
    /// one — a silent off-by-one in a tool whose entire output is columns.
    #[test]
    fn a_tab_occupies_exactly_one_cell_so_columns_do_not_drift() {
        let mut app = App::new("t.md".into(), "a\tb\tc END\n");
        assert_eq!(app.line_len(1), 9);
        let buf = render_buf(&mut app, 40, 8);
        let row: String = (0..buf.area.width).map(|x| buf[(x, 1)].symbol()).collect();
        assert!(row.contains("a b c END"), "{row}");

        // and the cursor cell lands on the byte the model says it is on
        // bytes: a \t b \t c ' ' E N D — so the 'c' is byte 4, column 5
        app.cursor = Pos::new(1, 5);
        let buf = render_buf(&mut app, 40, 8);
        let x = (0..buf.area.width)
            .find(|&x| buf[(x, 1)].style().bg == Some(Color::Yellow))
            .expect("cursor on screen");
        assert_eq!(buf[(x, 1)].symbol(), "c");
    }

    #[test]
    fn peek_wraps_the_selection_over_the_source_view() {
        let mut app = App::new("long.md".into(), &long_doc());
        app.cursor = Pos::new(3, 1);
        app.toggle_peek();
        let screen = render(&mut app, 60, 16);
        assert!(screen.contains("peek: paragraph"), "{screen}");
        // the tail of the 200-column line is on screen, which is the point
        assert!(screen.contains("at the end."), "{screen}");
        assert!(app.peek_rows > 1, "text should have wrapped");
    }

    #[test]
    fn a_long_line_wraps_onto_the_rows_below_it() {
        let mut app = App::new("long.md".into(), &long_doc());
        let screen = render(&mut app, 60, 16);
        assert!(!screen.contains('›'), "nothing runs off the edge: {screen}");
        assert!(
            screen.contains("at the end."),
            "the tail of the line should be on screen: {screen}"
        );
    }

    /// A repeated line number reads as a repeated line. The number and the
    /// annotation dot mark the line, so they belong to its first row only.
    #[test]
    fn the_gutter_numbers_a_line_once_however_many_rows_it_takes() {
        let mut app = App::new("long.md".into(), &long_doc());
        let screen = render(&mut app, 60, 16);
        let field = |l: &str| l.chars().skip(1).take(4).collect::<String>();
        let numbered = screen.lines().filter(|l| field(l) == "   3").count();
        assert_eq!(numbered, 1, "line 3 numbered {numbered} times: {screen}");
        // …and it did take more than one row, or this proves nothing.
        assert!(app.row_count(3) > 1, "line 3 did not wrap");
    }

    /// With truncation the cursor past the edge was carried by the `›` marker.
    /// Wrapped, there is no edge to be past: it is drawn where it is.
    #[test]
    fn the_cursor_at_the_end_of_a_wrapped_line_is_drawn_on_its_own_row() {
        let mut app = App::new("long.md".into(), &long_doc());
        app.cursor = Pos::new(3, 1);
        app.goto_line_end();
        assert!(has_cursor(&render_buf(&mut app, 60, 16)));
        assert!(!render(&mut app, 60, 16).contains('›'));
    }

    /// Marks are byte ranges into a line and rows are byte windows into it, so
    /// rebasing one onto the other is where a wrapped selection would silently
    /// drift. The selected line is highlighted on every row it occupies.
    #[test]
    fn a_selection_stays_highlighted_across_the_rows_it_wraps_to() {
        let mut app = App::new("long.md".into(), &long_doc());
        app.cursor = Pos::new(3, 1);
        app.toggle_lines();
        let buf = render_buf(&mut app, 60, 16);
        let sel = Color::Indexed(238);
        let rows = (0..buf.area.height)
            .filter(|&y| (0..buf.area.width).any(|x| buf[(x, y)].style().bg == Some(sel)))
            .count();
        assert!(
            rows >= app.row_count(3) - 1,
            "selection covered {rows} rows of {}",
            app.row_count(3)
        );
    }

    /// Wrapping is a rendering decision. The JSON is computed from the source
    /// and must not notice.
    #[test]
    fn wrapping_does_not_reach_the_output_contract() {
        let mut app = App::new("long.md".into(), &long_doc());
        app.cursor = Pos::new(3, 1);
        app.toggle_lines();
        app.begin_comment();
        app.editor.set("note");
        app.commit_comment();

        render(&mut app, 60, 16);
        let wrapped = serde_json::to_string(&app.result()).unwrap();
        app.pretty = false;
        render(&mut app, 60, 16);
        assert_eq!(wrapped, serde_json::to_string(&app.result()).unwrap());
    }

    const TABLE_DOC: &str = "\
| id | description | ok |
|---|:---:|---|
| 1 | short | y |
| 22 | a much longer description | n |
";

    /// Aligning a table puts cells on the screen that no byte of the file
    /// accounts for. Columns are still bytes of the *source*, so the same
    /// selection must emit the same JSON with pretty on and off — and the
    /// selection to prove it on is one inside a padded cell, which is where a
    /// pad-aware column would be off by exactly the padding.
    #[test]
    fn alignment_does_not_reach_the_output_contract() {
        let mut app = App::new("t.md".into(), TABLE_DOC);
        // `-` narrows onto the cell under the cursor — the shortest one in the
        // widest column, so the one carrying the most padding.
        app.cursor = Pos::new(3, 7);
        app.contract();
        app.begin_comment();
        app.editor.set("note");
        app.commit_comment();
        assert_eq!(app.annotations[0].original_text, " short ");

        render(&mut app, 60, 16);
        let pretty = serde_json::to_string(&app.result()).unwrap();
        app.pretty = false;
        render(&mut app, 60, 16);
        assert_eq!(pretty, serde_json::to_string(&app.result()).unwrap());
    }

    /// Cells of line 3 carrying the selection or the cursor. The cursor sits
    /// inside the selection and paints its own colour, so it counts as part of
    /// it.
    fn highlighted(app: &mut App) -> Vec<u16> {
        let buf = render_buf(app, 60, 16);
        (0..buf.area.width)
            .filter(|&x| {
                matches!(
                    buf[(x, 3)].style().bg,
                    Some(Color::Indexed(238) | Color::Yellow)
                )
            })
            .collect()
    }

    /// Both found by running it in a real tty, both the same mistake: padding
    /// has no style of its own, so it has to borrow one, and *which* byte it
    /// borrows from is not something the renderer can work out locally.
    ///
    /// Taking the byte before it left the gap in front of a centred cell
    /// wearing the pipe's style — a selected cell highlighted on its right half
    /// only. Taking the byte after the content swept the closing gap into a
    /// selection *inside* the cell, so annotating one word lit up the whole
    /// column. Both gaps belong to the cell, and so anchor to its two ends.
    #[test]
    fn padding_is_highlighted_with_its_cell_and_only_with_its_cell() {
        let mut app = App::new("t.md".into(), TABLE_DOC);
        app.cursor = Pos::new(3, 8);

        app.contract();
        assert_eq!(app.selection_kind(), "table-cell");
        let lit = highlighted(&mut app);
        assert_eq!(
            usize::from(lit[lit.len() - 1] - lit[0]) + 1,
            lit.len(),
            "highlight has a hole: {lit:?}"
        );
        assert_eq!(lit.len(), 27, "not the whole padded cell: {lit:?}");

        // One step in, onto the word itself: the gaps are the cell's, not the
        // word's, so the highlight is the word.
        app.contract();
        assert_eq!(app.selection_kind(), "text");
        assert_eq!(highlighted(&mut app).len(), "short".len());
    }

    /// The whole point, on a real screen: the pipes land in one column.
    #[test]
    fn an_aligned_table_reaches_the_screen() {
        let mut app = App::new("t.md".into(), TABLE_DOC);
        let screen = render(&mut app, 60, 16);
        // Character positions, not byte offsets: the gutter's selection bar is
        // three bytes wide and one cell, so bytes would say the rows disagree.
        let bars: Vec<Vec<usize>> = screen
            .lines()
            .filter(|l| l.matches('|').count() > 2)
            .map(|l| {
                l.chars()
                    .enumerate()
                    .filter(|&(_, c)| c == '|')
                    .map(|(i, _)| i)
                    .collect()
            })
            .collect();
        assert_eq!(bars.len(), 4, "{screen}");
        assert!(bars.windows(2).all(|w| w[0] == w[1]), "{screen}");

        // …and off, the screen is byte for byte the source again.
        app.pretty = false;
        let raw = render(&mut app, 60, 16);
        assert!(raw.contains("| 1 | short | y |"), "{raw}");
    }

    /// Found by running it: the overlay is inset, so the markers landed in the
    /// strip of source still visible beside it — a `›` with no line attached.
    #[test]
    fn peek_suppresses_the_truncation_markers_underneath_it() {
        let mut app = App::new("long.md".into(), &long_doc());
        app.pretty = false;
        app.cursor = Pos::new(3, 1);
        assert!(render(&mut app, 60, 16).contains('›'));
        app.toggle_peek();
        let screen = render(&mut app, 60, 16);
        assert!(!screen.contains('›'), "{screen}");
    }

    #[test]
    fn peek_scroll_is_clamped_to_the_rows_it_has() {
        let mut app = App::new("long.md".into(), &long_doc());
        app.cursor = Pos::new(3, 1);
        app.toggle_peek();
        render(&mut app, 60, 16);
        for _ in 0..50 {
            app.scroll_peek(1);
        }
        assert_eq!(app.peek_scroll, app.peek_rows - 1);
        for _ in 0..50 {
            app.scroll_peek(-1);
        }
        assert_eq!(app.peek_scroll, 0);
    }

    #[test]
    fn peek_needs_something_to_peek_at() {
        let mut app = App::new("empty.md".into(), "");
        app.toggle_peek();
        assert!(!app.peek);
        render(&mut app, 60, 12);
    }

    /// The editor advertises `C-j newline`, and the pane used to render a
    /// two-line comment as `onetwo`.
    #[test]
    fn a_multi_line_comment_keeps_its_break_visible_in_the_pane() {
        let mut app = App::new("PLAN.md".into(), DOC);
        app.begin_comment();
        app.editor.set("one");
        app.editor.newline();
        for c in "two".chars() {
            app.editor.insert(c);
        }
        app.commit_comment();
        let screen = render(&mut app, 100, 24);
        assert!(screen.contains("one ⏎ two"), "{screen}");
    }

    #[test]
    fn segments_never_split_a_character() {
        let text = "Prüfen köde";
        // deliberately ask for a cut in the middle of "ü"
        let marks = [(3usize, 5usize, Style::default().bg(Color::Red))];
        let spans = segments(text, &marks, Style::default());
        let rebuilt: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(rebuilt, text);
    }

    #[test]
    fn segments_rebuild_the_line_exactly() {
        let text = "Use `parse_document` and more";
        for (a, b) in [(0, 3), (4, 20), (20, 29), (0, 29)] {
            let marks = [(a, b, Style::default().bg(Color::Blue))];
            let rebuilt: String = segments(text, &marks, Style::default())
                .iter()
                .map(|s| s.content.to_string())
                .collect();
            assert_eq!(rebuilt, text, "range {a}..{b}");
        }
    }
}
