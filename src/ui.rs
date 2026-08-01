//! Rendering. The screen shows raw source lines with a gutter; structure is
//! expressed purely through highlighting, so there is no rendered->source
//! mapping to get wrong.
//!
//! Selections can start and end mid-line, so each rendered line is cut into
//! styled segments at the union of the selection and cursor boundaries. All
//! cuts land on character boundaries — the columns coming out of comrak are
//! byte offsets, and slicing a multi-byte character in half panics.

use ratatui::buffer::CellWidth as _;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use unicode_segmentation::UnicodeSegmentation as _;

use crate::app::{Anchor, App, Mode};
use crate::wrap::{cells_claimed, cells_drawn, wrap, Piece, Row};

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

/// Cells the terminal will draw `row` in.
///
/// Deliberately not `Line::width()`, which is the sum of `Span::width()` and so
/// is `cells_claimed` — the measure that decides how big an *area* is. A row
/// gets no say in the size of the body pane; it is laid into it by
/// `LineTruncator`, which advances by `cell_width` a grapheme at a time. Summing
/// per span is the same arithmetic the truncator does, and it stays the same
/// number when a cursor or selection mark splits a cluster into two spans: the
/// column `cell_width` adds is charged per halfwidth sound mark, not per pair.
fn row_cells_drawn(row: &Line) -> usize {
    row.spans.iter().map(|s| cells_drawn(&s.content)).sum()
}

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

/// The nearest byte the rows actually render, at or before `b`, falling forward
/// when nothing precedes it — **except when the rows render nothing at all**,
/// where it returns `b` unchanged and the promise in that first sentence is not
/// kept. See the last paragraph: that case is covered elsewhere, not here.
///
/// Rows are windows into the line but not a cover of it: `wrap_line` leaves the
/// space it broke at, and any trailing run of spaces, outside every row so they
/// overhang the edge instead of taking a row of their own. A mark on one of
/// those bytes clips against every piece and survives on none. For syntax that
/// is invisible and harmless — the byte is a space. For the cursor it means no
/// cell on the screen carries the cursor style at all, and neither overflow
/// safety net fires, because the row is neither empty nor over-width.
///
/// **The empty line is the hole in that, and so is the all-space line in pretty
/// mode.** Both come back as one row whose only piece is the zero-width
/// `Src(0, 0)`: an empty line has nothing to render, and pretty mode's wrapper
/// leaves `"   "` entirely outside its rows, the same way it leaves any other
/// trailing run of spaces. (Raw mode gives `Src(0, 3)` for that line, so only
/// the empty case is a hole there.) Nothing then contains `b`, but `e <= b`
/// *does* match — `0 <= 0` — so the middle branch takes `end = 0`, steps back to
/// **byte 0**, and returns a byte no row renders. So does the fall-forward, and
/// so would `unwrap_or(b)`: every route out is uncovered, and the first line of
/// this doc is a promise this function cannot keep here.
///
/// A caller reading only this function would conclude the cursor vanishes on
/// every blank line. It does not, and the reason is not in this function: it is
/// the `spans.is_empty() && on_cursor_line` net in `draw_source`, ~200 lines
/// down, which paints one cursor cell whenever a row produced no spans at all.
/// That net is the entire guarantee for blank lines — narrow it or move it and
/// the cursor disappears on every one of them, with nothing here to say so.
fn snap_to_rendered(rows: &[Row], text: &str, b: usize) -> usize {
    let srcs = || {
        rows.iter().flat_map(|r| r.iter()).filter_map(|p| match *p {
            Piece::Src(s, e) => Some((s, e)),
            Piece::Pad { .. } => None,
        })
    };
    if srcs().any(|(s, e)| s <= b && b < e) {
        return b;
    }
    if let Some(end) = srcs().filter(|&(_, e)| e <= b).map(|(_, e)| e).max() {
        // `end` is one past the last rendered byte; step back onto its character
        // so the cursor shows on the last thing the row draws.
        let mut i = end.saturating_sub(1);
        while i > 0 && !text.is_char_boundary(i) {
            i -= 1;
        }
        return i;
    }
    srcs()
        .filter(|&(s, _)| s > b)
        .map(|(s, _)| s)
        .min()
        .unwrap_or(b)
}

/// Syntax first, then selection, then the cursor: later marks win, so
/// highlighting never hides where you are or what you have chosen.
fn line_marks(
    app: &App,
    lineno: usize,
    text: &str,
    rows: &[Row],
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
        let c0 = snap_to_rendered(rows, text, cursor_byte(app, text));
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
        let (rows, indent) = app.line_rows(lineno);
        let marks = line_marks(app, lineno, &text, &rows, sel_style, cur_style);
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
            // `row_cells_drawn`, not `Line::width()`: both of the decisions
            // below are about what the pane will hold, and the pane is cut by
            // `LineTruncator`, which advances by `cell_width`. `Line::width()`
            // is `cells_claimed` — see the rule in `wrap`'s module doc.
            if row_cells_drawn(&row) > body_w {
                // A cursor past the edge leaves no cursor cell on screen at all.
                // The marker takes the cursor's colour in that case, so the screen
                // still says where you are — `w`/`b`, `0` and `z` get you back to it.
                let hidden =
                    on_cursor_line && cells_drawn(&text[..cursor_byte(app, &text)]) >= body_w;
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
                let buf = f.buffer_mut();
                // A double-width grapheme covering the last two body columns
                // lives at `x - 1`, with `x` as its continuation cell. Writing
                // the marker into `x` alone leaves a wide cell followed by a
                // non-blank one — the state ratatui documents as ill-formed —
                // and `Buffer::diff` skips whatever follows a wide symbol, so
                // the marker would reach the front buffer and never the
                // terminal. Blank the lead cell and both survive.
                //
                // It is `Cell::cell_width` that has to answer "is this wide?",
                // because that is the function `Buffer::diff` itself asks. It
                // is not `wrap::cells_claimed`: `cells_claimed` is plain `unicode-width`, while
                // `cell_width` adds one column per halfwidth katakana dakuten
                // (U+FF9E/U+FF9F), which `unicode-width` calls zero-width and
                // terminals draw as a column of its own. `cells_claimed("ｶﾞ")` is 1 and
                // `"ｶﾞ".cell_width()` is 2, so the old guard left every such
                // line unblanked and the marker was dropped by `diff`. Asking
                // the `Cell` rather than its symbol also honours a
                // `CellDiffOption::ForcedWidth`, which is what `diff` skips by
                // when one is set.
                //
                // Note what the blanking costs: the character it replaces
                // *fitted*. Two columns cannot hold a two-cell glyph and a
                // one-cell marker both, so raw mode's promise that every body
                // cell is a byte of the file gives way here to saying that the
                // line continues. Moving the marker to `x - 1` instead would
                // destroy the same character and pull the marker off the edge.
                if x > body_area.x && buf[(x - 1, y)].cell_width() > 1 {
                    buf[(x - 1, y)].set_symbol(" ");
                }
                buf[(x, y)].set_symbol("›").set_style(style);
            }
        }
    }
}

/// The selection readout goes first: it is the only field that changes on every
/// keypress, and a long path used to push it off the end of the border
/// entirely. The path is last and shortened to whatever is left, because a
/// truncated path is still recognisable and a missing selection is not.
///
/// The title's slot is `width - 2`, the run of top border between the two
/// corners — never `width`. What the path may spend is that slot less the fixed
/// prefix and less the trailing pad space, which is the `+ 3` below.
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
    // Two of the three are the border corners, the third is the pad space that
    // `format!` appends below.
    let budget = usize::from(width).saturating_sub(cells_claimed(&rest) + 3);
    format!("{rest}{} ", shorten_path(app.display_name(), budget))
}

/// Keep the tail of an over-long path — the file name is what identifies it,
/// the directory prefix is what makes it too long.
///
/// `max` is terminal cells, so the path is measured in cells too. Counting
/// characters under-counts a wide-character path by one per wide character, and
/// the title then overran the border and was truncated by ratatui from the
/// *right* — cutting away the file name this function exists to keep, with no
/// ellipsis to say it had happened.
///
/// The cut walks **grapheme clusters** and measures the retained tail **whole**,
/// with the same `cells_claimed` the caller applies to the finished title. Neither half
/// of that is decoration:
///
/// - `cells_claimed` reads sequences, so a per-character sum is a different number.
///   `"✔\u{FE0F}"` and `"1\u{FE0F}\u{20E3}"` are two cells each as a string and
///   one summed per char; `"👩‍👩‍👧‍👦"` is two as a string and eight summed. The
///   under-counting kind is the same overrun this function exists to prevent,
///   reached through VS16 instead of through East-Asian-Wide.
/// - a `char` boundary inside a cluster is not a place to cut. Splitting
///   `"日\u{301}ab"` after `日` leaves the combining acute to attach to the
///   ellipsis that gets prepended, so the cut mark comes out as `…́`.
///
/// `cells_claimed`, not `str::cell_width`, because a block title is measured twice and
/// the *outer* of the two is `cells_claimed`. `Block::render_left_titles` sizes the
/// title's `Rect` at `Line::width()` — plain `unicode-width`, the same function
/// `cells_claimed` is — and only inside that rect does the span loop advance by
/// `cell_width`. So `cells_claimed` is what decides whether the whole title gets a rect
/// to live in, and being stricter than it buys nothing: the one string where
/// the two disagree, halfwidth katakana plus dakuten, is clipped by ratatui
/// whatever budget it is given, because the rect it is handed is derived from
/// the title itself and is a column short per pair.
fn shorten_path(path: &str, max: usize) -> String {
    if cells_claimed(path) <= max {
        return path.to_string();
    }
    // One column goes to the ellipsis that marks the cut. With no column at all
    // there is nothing to say: an ellipsis at `max == 0` is one cell more than
    // the slot holds, and what it would cost is the title's trailing pad space.
    if max == 0 {
        return String::new();
    }
    let budget = max - 1;
    // Longest suffix that fits, measured as a string at every candidate cut.
    // `cells_claimed` never shrinks as the suffix grows, so the first cut that does not
    // fit ends the search.
    let start = path
        .grapheme_indices(true)
        .map(|(i, _)| i)
        .rev()
        .take_while(|&i| cells_claimed(&path[i..]) <= budget)
        .last()
        .unwrap_or(path.len());
    format!("…{}", &path[start..])
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

/// The comment editor's first-row prompt. Continuation rows draw two spaces, so
/// two cells is what every row of the box spends before its text either way.
const PROMPT: &str = "> ";

/// The horizontal scroll that leaves the caret grapheme wholly on screen.
///
/// `prompt` and `before` are everything the row draws to the left of the caret;
/// `caret` is the grapheme under it, empty when the caret sits past the end of
/// the line and a space is drawn instead. Two properties of ratatui's
/// `LineTruncator` decide the answer, and neither of them is about the caret's
/// left cell:
///
/// - a grapheme that does not fit whole is not half-drawn. The truncator
///   `break`s out of the line, so a two-cell caret placed in the last column is
///   not clipped to one column — it is dropped, along with everything after it.
///   The caret's *right* cell has to be inside the pane too.
/// - `trim_offset` skips whole graphemes, so an offset landing inside a wide one
///   is rounded **down** and the row is drawn one cell to the right of where it
///   was asked for. Rounding **up** to an offset the truncator can reach is what
///   keeps the arithmetic here and the arithmetic on screen the same one.
///
/// Widths come from `str::cell_width`, not `wrap::cells_claimed`, for the reason
/// `cac34c1` gives: they disagree by one column per halfwidth katakana dakuten,
/// and only the former is what ratatui lays the row out with.
fn caret_hscroll(prompt: &str, before: &str, caret: &str, inner_w: u16) -> u16 {
    // Each cumulative grapheme width is an offset `trim_offset` can land on
    // exactly, and nothing between two of them is.
    let mut x = 0u16;
    let stops: Vec<u16> = std::iter::once(0)
        .chain(
            prompt
                .graphemes(true)
                .chain(before.graphemes(true))
                .map(|g| {
                    x = x.saturating_add(g.cell_width());
                    x
                }),
        )
        .collect();
    // `x` is now the caret's left cell, and the caret claims `caret_w` from it.
    // An empty `caret` is the end-of-line space, one cell like any other.
    let caret_w = caret.cell_width().max(1);
    let need = x.saturating_add(caret_w).saturating_sub(inner_w);
    // `x` is itself a stop, so this only fails when the caret is wider than the
    // whole pane — where scrolling it flush left is the best there is.
    stops.into_iter().find(|&s| s >= need).unwrap_or(x)
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

    // `Editor::rows` splits on `\n` only, so a comment typed without `C-j` is
    // always exactly one row however long it grows. With no wrap and no scroll
    // the paragraph was simply clipped at the border, and the caret — a styled
    // cell like any other — was clipped with it: everything typed past the edge
    // happened blind, with nothing on screen to say where the insertion point
    // was. The source view has a marker for exactly this; the comment box had
    // nothing. Scrolling rather than wrapping because the box's height is
    // computed from `rows().len()`, so a wrapped line would overflow it
    // vertically and be clipped again, one axis over.
    let inner_w = area.width.saturating_sub(2);
    let all_rows = app.editor.rows();
    let cur = all_rows.get(crow).copied().unwrap_or("");
    let ccol = ccol.min(cur.len());
    let hscroll = caret_hscroll(
        PROMPT,
        &cur[..ccol],
        cur[ccol..].graphemes(true).next().unwrap_or(""),
        inner_w,
    );

    // The same argument one axis over, and the axis the box loses rows on for
    // two independent reasons: `draw` caps its height at eight rows, and
    // `Layout::vertical` squeezes it below even that when the pane is short —
    // seven rows already do not fit an eighteen-row terminal. Either way
    // `rows()` outruns `area`, and the row that goes missing is the last one,
    // which is the one being typed on. `area.height` rather than the cap,
    // because only the area knows which of the two did the cutting.
    let inner_h = area.height.saturating_sub(2);
    let vscroll = u16::try_from(crow.saturating_sub(usize::from(inner_h.saturating_sub(1))))
        .unwrap_or(u16::MAX);

    let rows: Vec<Line> = app
        .editor
        .rows()
        .iter()
        .enumerate()
        .map(|(i, text)| {
            let prompt = Span::styled(
                if i == 0 { PROMPT } else { "  " },
                Style::default().fg(Color::Yellow),
            );
            let mut spans = vec![prompt];
            if i == crow {
                // Draw the caret as a cell so it is visible on any terminal,
                // including one sitting past the end of the line. One *grapheme
                // cluster*, not one char: a char stops after the first codepoint
                // of a ZWJ sequence or of a base plus a combining mark, which
                // both styles half a glyph and — because a span is segmented on
                // its own — hands ratatui two clusters where the text has one,
                // changing what the row is as wide as.
                let c1 = text[ccol..]
                    .graphemes(true)
                    .next()
                    .map_or(text.len(), |g| ccol + g.len());
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
        Paragraph::new(rows).scroll((vscroll, hscroll)).block(
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
///
/// The table also sets where the status field starts appearing: `status_floor`
/// reads the floor off these widths, and for `KEYS` it is the third rung that
/// decides it — 50 cells, so 80 columns. Widen that rung by one cell and an
/// 80-column terminal loses the only confirmation `x remove` gives. Under that
/// budget it documents movement by naming `[/] marks` and not `hjkl`, `w/b`,
/// `v/V` or `+/-`. Every one of those four moves the cursor somewhere the
/// reader can already see; `[` and `]` are the only keys that reach an
/// annotation off screen, which is the whole point of a fringe you work down.
/// They also answer with `mark 2/5` in the status field this rung has just
/// bought, so the nine cells they cost name a position the pane could not
/// otherwise report — the argument that used to buy `v/V` its three cells
/// here, and the rung below still spends them on `w/b`.
///
/// `c comment`, not `Enter comment`, from this rung down: `c` stays bound
/// (see the dispatch in `main.rs`) and spells the same action four cells
/// cheaper, which is `[/] marks` nearly half paid for.
const KEYS: [&str; 6] = [
    "hjkl move · ^d/^u/^f/^b page · J/K unit · w/b inline · v units · V lines · +/- widen/narrow · z peek · [/] marks · Enter comment · x remove · q quit",
    "hjkl · J/K unit · w/b inline · v/V select · +/- widen · z peek · [/] marks · Enter comment · x remove · q quit",
    "z peek · [/] marks · c comment · x remove · q quit",
    "w/b · z peek · c comment · x remove · q quit",
    "c comment · x remove · q quit",
    "q quit",
];
const INPUT_KEYS: [&str; 2] = ["Enter save · Esc cancel", "Enter · Esc"];
const PEEK_KEYS: [&str; 2] = ["j/k scroll · z or Esc closes", "z closes"];

const STATUS_W: u16 = 28;

/// What a rung costs on its own: one column of leading space, then the hints.
///
/// `cells_drawn` because the comparison this feeds is against the footer pane,
/// which a `Paragraph` truncates. It is the only site in the crate where the
/// choice cannot be observed: the three tables above are compile-time constants
/// of ASCII plus `·` and `—`, and the two measures agree on every character of
/// them — `the_two_measures_agree_on_every_key_hint` says so, and would fail if
/// a rung ever grew a character where they do not. The rule still decides it,
/// because "it does not matter here" is a fact about today's strings and not
/// about the function.
fn hints_w(keys: &str) -> usize {
    1 + cells_drawn(keys)
}

/// …and what it costs with the status field beside it, one column between the
/// two so they never abut.
fn hints_and_status_w(keys: &str) -> usize {
    hints_w(keys) + 1 + usize::from(STATUS_W)
}

/// The narrowest pane that shows the status field, read off the rung table.
///
/// Rung `i` is what the hints alone would pick for every width from
/// `hints_w(table[i])` up to `hints_w(table[i - 1]) - 1`; wider than that and
/// the fuller rung above takes over. The field is free — costs no rung —
/// exactly where `hints_and_status_w(table[i])` still lands inside rung `i`'s
/// own band, and the floor is the narrowest such width in the table. The first
/// rung's band has no ceiling, so a candidate always exists.
///
/// That floor is also the lowest one the two monotonicities allow: one column
/// below it the field only fits beside a rung barer than the one that pane
/// already shows, so buying it there would take hints away from a *widening*
/// pane. For `KEYS` the floor is 80: at 80 columns the rung the hints alone
/// would pick is the 50-cell one, and 50 + 30 is exactly the room an 80-column
/// pane has, so the field costs that pane nothing. At 79 it would cost a rung.
///
/// Derived, never written down. A hard-coded 80 is right for `KEYS` today and
/// wrong the moment a hint string is edited — and `INPUT_KEYS` and `PEEK_KEYS`
/// have floors of their own, 53 and 58.
fn status_floor(table: &[&str]) -> usize {
    let mut floor = usize::MAX;
    let mut ceiling = usize::MAX; // nothing is fuller than the first rung
    for keys in table {
        if hints_and_status_w(keys) <= ceiling {
            floor = floor.min(hints_and_status_w(keys));
        }
        ceiling = hints_w(keys) - 1;
    }
    floor
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let width = usize::from(area.width);
    let table: &[&str] = if app.mode == Mode::Input {
        &INPUT_KEYS
    } else if app.peek {
        &PEEK_KEYS
    } else {
        &KEYS
    };

    // Below `status_floor` the key hints come first: they are the only
    // documentation on screen, and the status field is a luxury bought only
    // where it costs no rung.
    //
    // Deciding the status first — and against the *barest* rung, `q quit` —
    // made the footer non-monotonic in width: from 36 to 57 columns the
    // 28-column status field was affordable next to `q quit` and so was always
    // taken, forcing the barest rung even though dropping the status would have
    // fitted two rungs more. Widening the terminal from 35 to 36 columns
    // therefore *removed* the key hints.
    //
    // Testing affordability against the rung *just chosen* brought the same
    // fault back on the other axis: `cells_drawn(keys)` jumps a whole rung gap at each
    // boundary, so the right-hand side of that test outran the left and the
    // field vanished by *widening* — on at 94 columns, gone at 95. At and above
    // the floor the field is therefore reserved outright and the rung is picked
    // from what is left, which is monotone in width on both axes.
    let status_w = if width >= status_floor(table) {
        STATUS_W
    } else {
        0
    };
    let keys = table
        .iter()
        .copied()
        .find(|k| {
            if status_w == 0 {
                hints_w(k) <= width
            } else {
                hints_and_status_w(k) <= width
            }
        })
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
        // Nothing, not an ellipsis: a zero-cell budget cannot pay for the one
        // cell an ellipsis costs, and the cell it used to take was the pad
        // space that keeps the title off the corner.
        assert_eq!(shorten_path("/a/b/c/long.md", 0), "");
        // multibyte must not be sliced apart
        assert_eq!(shorten_path("/tmp/Prüfen/köde.md", 8), "…köde.md");
        // …and neither must a grapheme cluster. Cutting after `日` would leave
        // the combining acute to attach itself to the ellipsis.
        assert_eq!(shorten_path("日\u{301}ab", 3), "…ab");
    }

    /// Rows of the comment box. It is the only block with a yellow border, so
    /// its left edge names them — top border, content, bottom border.
    fn comment_box_rows(buf: &ratatui::buffer::Buffer) -> Vec<u16> {
        let rows: Vec<u16> = (0..buf.area.height)
            .filter(|&y| buf[(0, y)].style().fg == Some(Color::Yellow))
            .collect();
        assert!(rows.len() >= 3, "comment box not found: {rows:?}");
        rows
    }

    /// The caret inside the comment box, as `(x, y, symbol)`: the first cell
    /// painted in the caret's colours. `None` when it never reached the buffer,
    /// which is the whole failure mode under test — a `LineTruncator` that
    /// cannot fit a grapheme does not clip it, it drops the rest of the line.
    fn caret_cell(buf: &ratatui::buffer::Buffer) -> Option<(u16, u16, String)> {
        comment_box_rows(buf).into_iter().find_map(|y| {
            (0..buf.area.width)
                .find(|&x| buf[(x, y)].style().bg == Some(Color::Yellow))
                .map(|x| (x, y, buf[(x, y)].symbol().to_string()))
        })
    }

    /// Comment texts whose caret arithmetic differs. `ｶﾞ` is here because
    /// `wrap::cells_claimed` and ratatui's `cell_width` disagree about it by one column
    /// (see `cac34c1`), so a scroll measured with the former is short by a
    /// column per pair.
    const CARET_TEXTS: [(&str, &str); 5] = [
        (
            "ascii",
            "the quick brown fox jumps over the lazy dog again and again",
        ),
        (
            "cjk",
            "漢漢漢漢漢漢漢漢漢漢漢漢漢漢漢漢漢漢漢漢漢漢漢漢漢漢漢漢漢漢",
        ),
        ("emoji", "👨‍👩‍👧‍👦❤️👍🏽👨‍👩‍👧‍👦❤️👍🏽👨‍👩‍👧‍👦❤️👍🏽👨‍👩‍👧‍👦❤️👍🏽"),
        ("mixed", "ab漢cd👍🏽ef漢gh❤️ij漢kl👨‍👩‍👧‍👦mn漢op qr漢st uv漢wx"),
        ("dakuten", "ｶﾞｶﾞｶﾞｶﾞｶﾞｶﾞｶﾞｶﾞｶﾞｶﾞｶﾞｶﾞｶﾞｶﾞｶﾞｶﾞｶﾞｶﾞｶﾞｶﾞ"),
    ];

    /// A named caret position, as the motion that reaches it from the end.
    type CaretPos = (&'static str, fn(&mut crate::editor::Editor));

    /// Caret positions reachable by a binding, `set` having left it at the end.
    /// `left` is the interesting one: it is the only one that parks the caret
    /// *on* the last grapheme rather than on the space past it, and a space is
    /// one cell whatever the text is made of.
    const CARET_POSITIONS: [CaretPos; 6] = [
        ("end", |_| {}),
        ("home", crate::editor::Editor::home),
        ("word_left", crate::editor::Editor::word_left),
        ("word_left*2", |e| {
            e.word_left();
            e.word_left();
        }),
        ("home+word_right", |e| {
            e.home();
            e.word_right();
        }),
        ("end-1", crate::editor::Editor::left),
    ];

    /// `Editor::rows` splits on `\n` only, so a comment typed without `C-j` is
    /// one row however long. With no wrap and no scroll the paragraph was
    /// clipped at the border and the caret — a styled cell like any other — was
    /// clipped with it, so everything typed past the edge happened blind. The
    /// source view protects the same invariant with a marker; the comment box
    /// had nothing.
    ///
    /// The scroll that fixed that was a cell count taken against the caret's
    /// *left* cell, which is right for ASCII and wrong for everything wider.
    /// Hence the sweep: 5 texts × 6 caret positions × every width from 4 to 120.
    #[test]
    fn the_comment_caret_stays_on_screen_however_long_the_comment() {
        for (tname, text) in CARET_TEXTS {
            for (pname, pos) in CARET_POSITIONS {
                for w in 4u16..=120 {
                    let mut app = App::new("PLAN.md".into(), DOC);
                    app.begin_comment();
                    app.editor.set(text);
                    pos(&mut app.editor);
                    let buf = render_buf(&mut app, w, 20);
                    assert!(
                        caret_cell(&buf).is_some(),
                        "no caret cell at width {w}, text {tname}, caret at {pname}"
                    );
                }
            }
        }
    }

    /// The caret cell must hold the whole cluster the caret sits on. It was cut
    /// with `char_indices().nth(1)`, which is the first *codepoint*: for a ZWJ
    /// family or a skin-tone modifier that styles a fragment and — because a
    /// span is segmented on its own — hands ratatui several clusters where the
    /// text has one, so the row is laid out at a width the scroll never
    /// accounted for.
    #[test]
    fn the_caret_covers_the_whole_grapheme_cluster_it_sits_on() {
        for g in ["a", "漢", "👨‍👩‍👧‍👦", "❤️", "👍🏽", "ｶﾞ"] {
            let text = g.repeat(20);
            for w in 6u16..=60 {
                let mut app = App::new("PLAN.md".into(), DOC);
                app.begin_comment();
                app.editor.set(&text);
                app.editor.home();
                let buf = render_buf(&mut app, w, 20);
                let (_, _, sym) = caret_cell(&buf)
                    .unwrap_or_else(|| panic!("no caret cell at width {w} on {g:?}"));
                assert_eq!(sym, g, "caret cut mid-cluster at width {w} on {g:?}");
            }
        }
    }

    /// A comment of `n` rows, each row a word no other row contains, with the
    /// caret left at the end of the last one — where typing would put it.
    fn comment_of_rows(n: usize) -> String {
        (1..=n)
            .map(|i| format!("row{i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The comment box clipped vertically without scrolling: `draw` caps its
    /// height at eight rows, and `Layout::vertical` takes more off it than that
    /// on a short pane — seven rows of comment do not fit an eighteen-row
    /// terminal. Both cut the *last* row, which is the one being typed on, so
    /// the caret, the text under it and any marker went missing together.
    ///
    /// The sweep starts at nine rows of terminal because below that the box has
    /// no content row at all — the annotations pane keeps its six whatever else
    /// is starved — which is a layout-priority defect and not this one.
    #[test]
    fn the_row_being_typed_on_is_visible_however_many_rows_the_comment_has() {
        for h in 9u16..=30 {
            for n in 1usize..=12 {
                let mut app = App::new("PLAN.md".into(), DOC);
                app.begin_comment();
                app.editor.set(&comment_of_rows(n));
                let buf = render_buf(&mut app, 40, h);
                let (_, y, _) = caret_cell(&buf)
                    .unwrap_or_else(|| panic!("no caret cell at {h} rows, {n} rows of comment"));
                let line: String = (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect();
                assert!(
                    line.contains(&format!("row{n}")),
                    "the caret is not on the row being typed on at {h} rows, {n} rows of comment: {line:?}"
                );
            }
        }
    }

    /// Scrolling down must not cost the rows above it: the box shows a window
    /// ending on the row being typed on, not just that row.
    #[test]
    fn a_scrolled_comment_box_still_fills_itself_with_the_rows_above() {
        let mut app = App::new("PLAN.md".into(), DOC);
        app.begin_comment();
        app.editor.set(&comment_of_rows(12));
        let buf = render_buf(&mut app, 40, 30);
        let rows = comment_box_rows(&buf);
        // Eight content rows between the two borders, showing rows 5 to 12.
        assert_eq!(rows.len(), 10, "{rows:?}");
        let shown: Vec<String> = rows[1..rows.len() - 1]
            .iter()
            .map(|&y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim()
                    .trim_start_matches(['│', '>', ' '])
                    .trim_end_matches('│')
                    .trim()
                    .to_string()
            })
            .collect();
        assert_eq!(
            shown,
            (5..=12).map(|i| format!("row{i}")).collect::<Vec<_>>(),
            "{shown:?}"
        );
    }

    /// A caret past the end of the line has no grapheme of its own, so it is
    /// drawn as a space — one cell wide however wide the text before it is.
    #[test]
    fn the_caret_past_the_end_of_a_line_is_a_space() {
        for g in ["a", "漢", "👨‍👩‍👧‍👦"] {
            let mut app = App::new("PLAN.md".into(), DOC);
            app.begin_comment();
            app.editor.set(&g.repeat(20));
            let buf = render_buf(&mut app, 30, 20);
            let (_, _, sym) = caret_cell(&buf).expect("no caret cell");
            assert_eq!(sym, " ", "end-of-line caret on {g:?}");
        }
    }

    /// `caret_hscroll` never scrolls the caret off the left edge to get it off
    /// the right one, and never asks for an offset ratatui would round down —
    /// every answer is a cumulative grapheme width of what precedes the caret.
    #[test]
    fn caret_hscroll_lands_on_a_reachable_offset_left_of_the_caret() {
        for before in ["", "ab", "漢漢漢", "a漢b👍🏽c", "ｶﾞｶﾞｶﾞ"] {
            let mut stops = vec![0u16];
            let mut x = 0u16;
            for g in PROMPT.graphemes(true).chain(before.graphemes(true)) {
                x += g.cell_width();
                stops.push(x);
            }
            for caret in ["a", "漢", ""] {
                for inner_w in 1u16..=40 {
                    let h = caret_hscroll(PROMPT, before, caret, inner_w);
                    assert!(stops.contains(&h), "{h} is not a reachable offset");
                    assert!(h <= x, "scrolled past the caret at inner width {inner_w}");
                    let caret_w = caret.cell_width().max(1);
                    assert!(
                        caret_w > inner_w || x - h + caret_w <= inner_w,
                        "caret {caret:?} after {before:?} overflows inner width {inner_w}"
                    );
                }
            }
        }
    }

    /// A status text no rung contains, so finding it on the footer row means
    /// the status field itself reached the terminal.
    const STATUS_PROBE: &str = "annotation removed";

    /// The footer as rendered at width `w`: which rung of `table` is on the last
    /// row, and whether the status field came with it. `usize::MAX` for the rung
    /// when none of them is there at all — barer than any real rung, which is
    /// what a pane too narrow for even `q quit` deserves.
    fn footer_at(setup: fn(&mut App), table: &[&str], w: u16) -> (usize, bool) {
        let mut app = App::new("PLAN.md".into(), DOC);
        setup(&mut app);
        app.status = STATUS_PROBE.into();
        let buf = render_buf(&mut app, w, 14);
        let y = buf.area.height - 1;
        let row: String = (0..buf.area.width)
            .map(|x| buf[(x, y)].symbol().to_string())
            .collect();
        // `position` finds the richest rung on the row: a rung can only contain
        // rungs shorter than itself, so every spurious match is a barer one.
        let rung = table
            .iter()
            .position(|k| row.contains(k))
            .unwrap_or(usize::MAX);
        (rung, row.contains(STATUS_PROBE))
    }

    /// Widening the pane must take nothing away — neither hints nor status.
    ///
    /// Hints: the status field was once decided first and against the *barest*
    /// rung, so from 36 to 57 columns the 28-column field was always affordable
    /// next to `q quit` and therefore always taken, forcing the barest rung
    /// even though dropping it would have fitted two rungs more. Going from 35
    /// to 36 columns removed `c comment · x remove` from the screen.
    ///
    /// Status: testing the field against the rung just chosen moved the same
    /// fault to the other axis. `cells_drawn(keys)` jumps 31 and 38 cells at the
    /// `KEYS` rung boundaries, so the field was on at 94 columns and gone at 95,
    /// on at 132 and gone at 133 — absent at 80 and 140, the two commonest
    /// terminal widths, and it is the only confirmation `x remove` gives.
    ///
    /// Both axes, all three tables, and out to 250 columns: `INPUT_KEYS` and
    /// `PEEK_KEYS` escaped the second fault only because their rung gaps happen
    /// to be under the width of the status field, which nothing recorded.
    #[test]
    fn the_footer_never_loses_hints_or_status_as_the_pane_widens() {
        fn sweep(name: &str, setup: fn(&mut App), table: &[&str]) {
            let mut prev_rung = usize::MAX;
            let mut had_status = false;
            for w in 8u16..=250 {
                let (rung, status) = footer_at(setup, table, w);
                assert!(
                    rung <= prev_rung,
                    "{name}: width {w} shows a barer rung than width {}",
                    w - 1
                );
                assert!(
                    status || !had_status,
                    "{name}: width {w} dropped the status field a narrower pane had"
                );
                prev_rung = rung;
                had_status |= status;
            }
            assert!(had_status, "{name}: the status field never showed at all");
        }
        sweep("KEYS", |_| {}, &KEYS);
        sweep("INPUT_KEYS", App::begin_comment, &INPUT_KEYS);
        sweep("PEEK_KEYS", |a| a.peek = true, &PEEK_KEYS);

        // The widths that motivated the reserve: `x remove` has no undo and no
        // prompt, and the field used to be missing at both of the commonest
        // terminal sizes. 80 is an anchor on the rung table, not just on the
        // reserve — the floor is derived, so a third rung one cell wider than
        // its 50-cell budget puts the field out of reach of an 80-column pane
        // again without touching a line of the footer's arithmetic.
        assert!(footer_at(|_| {}, &KEYS, 80).1, "no status at 80 columns");
        assert!(footer_at(|_| {}, &KEYS, 140).1, "no status at 140 columns");
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

    /// File names whose cell width is neither their character count nor the sum
    /// of their characters' widths. `cells_claimed` reads sequences: `✔\u{FE0F}` and the
    /// keycap `1\u{FE0F}\u{20E3}` are two cells whole and one summed per char,
    /// and the ZWJ family is two whole and eight summed. `日` is the plain
    /// East-Asian-Wide case where char and cluster coincide; `ｶﾞ` is the case
    /// where `cells_claimed` and ratatui's own `cell_width` disagree (see `cac34c1`);
    /// ASCII is the control that must not regress.
    const TITLE_NAMES: [(&str, &str); 6] = [
        ("ascii", "a-perfectly-ordinary-but-quite-long-file-name.md"),
        ("cjk", "日本語日本語日本語日本語日本語日本語.md"),
        ("vs16", "yyyyyyyyyy✔\u{FE0F}✔\u{FE0F}.md"),
        ("keycap", "yyyyyyyyyy1\u{FE0F}\u{20E3}2\u{FE0F}\u{20E3}.md"),
        ("zwj", "yyyyyyyyyy👩‍👩‍👧‍👦👩‍👩‍👧‍👦.md"),
        ("dakuten", "ｶﾞｶﾞｶﾞｶﾞｶﾞｶﾞｶﾞｶﾞｶﾞｶﾞｶﾞｶﾞ.md"),
    ];

    /// The screen's top row, symbol by symbol. The title lives on the source
    /// block's top border, which is row 0 of the frame.
    fn top_row(buf: &ratatui::buffer::Buffer) -> String {
        (0..buf.area.width)
            .map(|x| buf[(x, 0)].symbol())
            .collect::<String>()
    }

    /// The narrowest width whose title slot holds the fixed fields at all.
    /// Below it the prefix alone overruns, which is a different problem:
    /// `source_title` shortens the path and nothing else.
    fn narrowest_usable_width() -> u16 {
        let mut bare = App::new("x.md".into(), DOC);
        bare.label = Some(String::new());
        let fixed = cells_claimed(&source_title(&bare, u16::MAX));
        u16::try_from(fixed + 2).unwrap()
    }

    /// `width` is terminal cells but the budget and the cut counted characters,
    /// so a wide-character path was under-counted by one per wide character. The
    /// title overran the border and ratatui truncated it from the right, taking
    /// the file name and extension — the very thing `shorten_path` keeps — with
    /// no ellipsis to show it had happened.
    ///
    /// Counting *cells* per char fixed the East-Asian-Wide half and left the
    /// other half in place, because `cells_claimed` is a measure of a string and the cut
    /// summed it over chars. Two different answers to "how wide is this?" cannot
    /// both be the budget.
    ///
    /// The slot is `width - 2`, the border between the corners. The old
    /// assertion here said `width`, which is two cells of slack — enough for the
    /// VS16 case to pass while the `d` of `.md` was cut off the screen. So the
    /// budget is checked against the slot, and the *buffer* is checked for the
    /// name the title claims to end with.
    #[test]
    fn the_title_fits_its_slot_and_keeps_the_file_name_on_screen() {
        // The cut itself, in cells rather than characters.
        assert_eq!(shorten_path("日本語日本語日本.md", 9), "…日本.md");
        assert_eq!(shorten_path("仕様/設計メモ.md", 20), "仕様/設計メモ.md");
        assert!(cells_claimed(&shorten_path("日本語日本語日本.md", 9)) <= 9);
        // …and in cells of the sequence, not cells summed over its chars. Both
        // directions: summing gives one cell per VS16 sequence where the string
        // is two, so this one used to come back a cell over budget —
        // "…✔\u{FE0F}✔\u{FE0F}.md" is 8 cells in a slot of 7. And eight per ZWJ
        // family where the string is two, so this one used to come back as
        // "….md" with room to spare.
        assert_eq!(
            shorten_path("yyyy✔\u{FE0F}✔\u{FE0F}.md", 7),
            "…✔\u{FE0F}.md"
        );
        assert_eq!(shorten_path("yy👩‍👩‍👧‍👦👩‍👩‍👧‍👦.md", 8), "…👩‍👩‍👧‍👦👩‍👩‍👧‍👦.md");

        for (kind, name) in TITLE_NAMES {
            // Widths at which the title claims to end in the file name, and the
            // buffer is therefore worth looking at. Counted so that a
            // `shorten_path` which gave up and returned "…" everywhere would
            // fail here rather than skip its way to green.
            let mut checked = 0usize;
            for w in narrowest_usable_width()..=160 {
                let mut app = App::new("x.md".into(), DOC);
                app.label = Some(name.into());
                let title = source_title(&app, w);
                let slot = usize::from(w) - 2;
                assert!(
                    cells_claimed(&title) <= slot,
                    "{kind} title is {} cells in a {slot}-cell slot at width {w}: {title:?}",
                    cells_claimed(&title)
                );

                // What the string says has to be what the screen shows. It was
                // the gap between the two that hid this: the title ended in
                // ".md " and the border ended in ".m".
                if !title.ends_with(".md ") {
                    continue;
                }
                let buf = render_buf(&mut app, w, 24);
                let row = top_row(&buf);
                // ratatui measures the title twice: `Block` sizes its rect at
                // `Line::width()` — plain `unicode-width`, the function `cells_claimed`
                // is — and then fills the rect with a loop that advances by
                // `cell_width`. Halfwidth katakana sound marks are the only
                // characters where the two disagree (`cac34c1`), and the
                // difference comes off the right-hand end. The rect is derived
                // from the title, so no budget this function could pick would
                // widen it. `overhang` is 0 for every other name here, which
                // leaves the plain assertion "the name the title claims is on
                // the border"; for `ｶﾞ` it says how much is unreachable.
                let overhang = title
                    .chars()
                    .filter(|c| matches!(*c, '\u{FF9E}' | '\u{FF9F}'))
                    .count();
                assert_eq!(
                    row.contains(".md"),
                    overhang <= 1, // the trailing pad space absorbs one cell
                    "{kind} name at width {w}, {overhang} cells of overhang: {row:?}"
                );
                checked += 1;
            }
            assert!(
                checked >= 80,
                "{kind} reached the buffer at {checked} widths"
            );
        }
    }

    /// The wrapped half of the same invariant. Rows are windows into the line
    /// but not a cover of it: the space a line breaks at, and any trailing run
    /// of spaces, sit outside every row so they overhang rather than take a row
    /// of their own. A cursor mark on one of those bytes clipped against every
    /// piece and survived on none, so no cell on screen carried the cursor —
    /// and neither overflow net fires, because the row is neither empty nor
    /// over-width. A markdown hard break is the everyday way to land there.
    #[test]
    fn the_cursor_survives_every_column_of_a_wrapped_line() {
        let doc = "aaaa bbbb cccc dddd eeee ffff gggg hhhh\nline one  \n";
        for (line, text) in doc.lines().enumerate() {
            for col in 1..=text.len() {
                let mut app = App::new("w.md".into(), doc);
                app.cursor = Pos::new(line + 1, col);
                let buf = render_buf(&mut app, 40, 10);
                assert!(
                    has_cursor(&buf),
                    "cursor vanished at L{}:{col} of {text:?}",
                    line + 1
                );
            }
        }
    }

    /// The marker is poked straight into the last body cell. Where a
    /// double-width grapheme covers the last two columns, that cell is its
    /// continuation half: the buffer goes ill-formed and `Buffer::diff` skips
    /// the cell after a wide symbol, so the marker reached the front buffer and
    /// never the backend. `render_buf` returns the backend buffer, which is the
    /// only place the difference shows.
    ///
    /// Both characters here are two cells on screen, but only one of them is
    /// two cells according to `wrap::cells_claimed`. `cells_claimed` is plain `unicode-width`,
    /// which calls the halfwidth dakuten U+FF9E zero-width; ratatui adds the
    /// column back, so `cells_claimed("ｶﾞ")` is 1 where `"ｶﾞ".cell_width()` is 2. The
    /// guard asked `cells_claimed`, saw a narrow cell, blanked nothing, and left the
    /// original bug untouched on every dakuten line at every even width. Hence
    /// the sweep: one width is not a test of a layout rule, and it is the even
    /// ones that land the wide grapheme on the last two columns. `日` stays in
    /// the sweep so this remains a test of the marker rather than a pin on one
    /// Unicode quirk.
    #[test]
    fn the_overflow_marker_survives_a_wide_last_column() {
        for ch in ["日", "ｶﾞ"] {
            let doc = format!("{}\n", ch.repeat(200));
            for w in 10u16..=120 {
                let mut app = App::new("cjk.md".into(), &doc);
                app.pretty = false;
                app.cursor = Pos::new(1, 1);
                let buf = render_buf(&mut app, w, 6);
                assert_eq!(
                    buf[(w - 2, 1)].symbol(),
                    "›",
                    "marker missing at width {w} on a line of {ch:?}"
                );
            }
        }
    }

    /// And the second thing it costs: the marker is what says where the cursor
    /// is when the cursor itself is past the edge. On a CJK line both used to
    /// vanish, leaving no indication anywhere on the row.
    #[test]
    fn a_cursor_past_the_edge_is_still_marked_on_a_wide_line() {
        let doc = format!("{}\n", "日".repeat(200));
        for w in 20u16..=40 {
            let mut app = App::new("cjk.md".into(), &doc);
            app.pretty = false;
            app.cursor = Pos::new(1, 150);
            let buf = render_buf(&mut app, w, 6);
            assert!(has_cursor(&buf), "no cursor indication at width {w}");
        }
    }

    /// Both of the overflow marker's decisions, on the character class where the
    /// two measures disagree.
    ///
    /// *Whether* it fires was `Line::width() > body_w`. `Line::width()` sums
    /// `Span::width()`, which is `cells_claimed`, so a row of halfwidth katakana
    /// and dakuten reported half the columns it needs: 34 clusters in a 35-cell
    /// body measured 34, so the row was judged to fit and ratatui then cut 33
    /// of its 68 cells off the end with nothing on screen to say so. The band
    /// swept is exactly the one where the two answers differ — wide enough for
    /// the claimed width, too narrow for the drawn one.
    ///
    /// *What colour* it fires in was the same mistake one function along:
    /// `cells_claimed` of the text before the cursor, against the same `body_w`.
    /// The marker takes the cursor's colour when the cursor is off screen, and
    /// on these lines the cursor was off screen from half the column the guard
    /// thought. `9f17460` and `cac34c1` both left this one standing.
    ///
    /// Backend buffer for both, since a marker that never survives `Buffer::diff`
    /// is present in the front buffer and absent from the terminal.
    #[test]
    fn the_overflow_marker_fires_and_takes_its_colour_by_the_drawn_width() {
        // 34 clusters: 34 claimed cells, 68 drawn ones, and six bytes apiece.
        // Bodies from 35 to 65 cells are the band where only the drawn measure
        // overflows, and where the last cluster is wholly off screen.
        let doc = format!("{}\n", "ｶﾞ".repeat(34));
        for w in 43u16..=73 {
            let body_w = w - 8; // two border columns and the six-cell gutter
            let mut app = App::new("kana.md".into(), &doc);
            app.pretty = false;
            app.cursor = Pos::new(1, 1);
            let buf = render_buf(&mut app, w, 6);
            assert_eq!(
                buf[(w - 2, 1)].symbol(),
                "›",
                "width {w}: a {body_w}-cell body holding 68 cells of line is not marked"
            );

            // …and with the cursor past the last column the body can draw, the
            // marker is the only thing left that can say where it is. The widest
            // body here draws 32 clusters and the first half of the 33rd, so the
            // 34th — six bytes to a cluster — is off screen at every width swept
            // and on screen at none of them.
            app.cursor = Pos::new(1, 33 * 6 + 1);
            let buf = render_buf(&mut app, w, 6);
            assert!(
                (7..w - 2).all(|x| buf[(x, 1)].style().bg != Some(Color::Yellow)),
                "width {w}: the cursor is drawn in the body, so this proves nothing"
            );
            assert_eq!(
                buf[(w - 2, 1)].style().bg,
                Some(Color::Yellow),
                "width {w}: the cursor is off screen and the marker is not wearing its colour"
            );
        }
    }

    /// The footer measures its key hints with `cells_drawn` because they are cut
    /// to a pane, but every rung is a compile-time constant that both measures
    /// agree on — so the choice is invisible today. This is what makes that
    /// sentence true rather than hopeful: a rung that grows a halfwidth dakuten,
    /// or any other character the two count differently, fails here.
    #[test]
    fn the_two_measures_agree_on_every_key_hint() {
        for table in [&KEYS[..], &INPUT_KEYS[..], &PEEK_KEYS[..]] {
            for keys in table {
                assert_eq!(
                    cells_claimed(keys),
                    cells_drawn(keys),
                    "the footer's rung {keys:?} measures differently by the two rules, so \
                     `hints_w` now has a visible choice to justify"
                );
            }
        }
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

    /// Wrapping is the mode whose whole promise is that nothing runs off the
    /// edge, and on a halfwidth-katakana line it broke that promise silently.
    /// `wrap_line` packed rows to `cells_claimed`, which calls `U+FF9E`
    /// zero-width; ratatui's `LineTruncator` cut them at `cell_width`, which does
    /// not. Every row went to the pane at twice its budget and came back halved,
    /// with no `›` to say so — the marker's own guard is `Line::width()`, the
    /// same short measure, so it agreed the row fitted.
    ///
    /// The assertion is on the backend buffer and it is a *census*: read the
    /// body columns back out of the buffer and require them to spell the head of
    /// the file, character for character, with no gap. Asserting "no `›`" would
    /// pass on the bug, and asserting one row's width would pass on a row that
    /// silently dropped its tail.
    ///
    /// The line is one word with no break point in it — no separator, and
    /// halfwidth katakana is not wide — so it goes to `wrap_line`'s hard cut,
    /// which packs a character at a time. `ｶ`, `ﾞ` and a digit are one drawn cell
    /// each, so a correct row is exactly `body_w` characters and the count is
    /// exact rather than a bound. That a cluster may be split across two rows is
    /// the price of a pane with an odd number of columns, and it costs no cell.
    ///
    /// The counter between the clusters is not decoration. A line of one
    /// repeated cluster cannot fail `starts_with`: every window of it is a
    /// prefix, so a screen that skipped half of every row would still spell a
    /// prefix of the file — and at an even body width it spells one of exactly
    /// the right length too. Only text that says where it came from can tell
    /// "the next `n` characters" from "some later `n` characters".
    #[test]
    fn every_cluster_of_a_wrapped_halfwidth_katakana_line_reaches_the_screen() {
        // Long enough to fill the viewport at the widest pane swept, so a short
        // count is always dropped text and never the end of the document.
        let line: String = (0..500).map(|i| format!("ｶﾞ{i}")).collect();
        let doc = format!("{line}\n");
        // `draw` gives the annotations pane six rows and the footer one, and the
        // source block spends two more of what is left on its border.
        let h = 20u16;
        let last_body_row = h - 6 - 1 - 2;
        for w in 20u16..=80 {
            let mut app = App::new("kana.md".into(), &doc);
            app.pretty = true;
            app.cursor = Pos::new(1, 1);
            let buf = render_buf(&mut app, w, h);
            let body_x = 7u16; // one border column, then the six-cell gutter
            let body_w = w - body_x - 1;
            // A wide grapheme's continuation cell is a space, and this document
            // has no space of its own, so dropping them leaves only file bytes.
            let shown: String = (1..=last_body_row)
                .flat_map(|y| (body_x..body_x + body_w).map(move |x| (x, y)))
                .map(|p| buf[p].symbol())
                .collect::<String>()
                .replace(' ', "");
            assert!(
                line.starts_with(&shown),
                "width {w}: the screen is not a prefix of the line: {shown:?}"
            );
            assert_eq!(
                shown.chars().count(),
                usize::from(body_w) * usize::from(last_body_row),
                "width {w}: {} characters on {last_body_row} rows of a {body_w}-cell body — \
                 a short count is text that never reached the terminal",
                shown.chars().count()
            );
            // Wrapping's promise: nothing runs off the edge, so nothing is
            // marked as running off it either.
            assert!(
                (1..=last_body_row).all(|y| buf[(w - 2, y)].symbol() != "›"),
                "width {w}: a wrapped row is marked truncated"
            );
        }
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

    /// `table.rs` asserts the columns it computed; this asserts the columns the
    /// terminal ended up with, which is the only place the claim "the pipes line
    /// up" is true or false. Reading them back off the backend buffer also means
    /// the assertion cannot be satisfied by the same arithmetic that produced
    /// the padding — the bug was exactly an arithmetic that disagreed with the
    /// renderer.
    #[test]
    fn a_padded_table_puts_every_rows_pipes_in_the_same_screen_column() {
        let doc = "\
| id | name | ok |
|---|---|---|
| 1 | ｶﾞｶﾞｶﾞ | y |
| 22 | plain | n |
";
        let mut app = App::new("t.md".into(), doc);
        app.pretty = true;
        let (w, h) = (60u16, 16u16);
        let buf = render_buf(&mut app, w, h);
        let pipes = |y: u16| -> Vec<u16> {
            (7..w - 1)
                .filter(|&x| buf[(x, y)].symbol() == "|")
                .collect()
        };
        let want = pipes(1);
        assert_eq!(want.len(), 4, "the header row did not render four pipes");
        for y in 2..=4u16 {
            assert_eq!(
                pipes(y),
                want,
                "screen row {y} puts its pipes in different columns from the header's"
            );
        }
    }

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
