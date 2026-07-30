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
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::{App, Mode};

pub fn draw(f: &mut Frame, app: &mut App, scroll: &mut usize) {
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

fn draw_source(f: &mut Frame, area: Rect, app: &mut App, scroll: &mut usize) {
    let viewport = area.height.saturating_sub(2) as usize;
    app.viewport = viewport;
    keep_cursor_visible(app, scroll, viewport);

    let current = app.current_block();
    let sel_style = Style::default().bg(Color::Indexed(238)).fg(Color::White);
    let cur_style = Style::default()
        .bg(Color::Yellow)
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD);

    let mut rows: Vec<Line> = Vec::with_capacity(viewport);

    for idx in 0..viewport {
        let lineno = *scroll + idx + 1;
        if lineno > app.lines.len() {
            break;
        }
        let text = app.line_text(lineno).to_string();
        let on_cursor_line = lineno == app.cursor.line;
        let in_current = current.is_some_and(|c| app.blocks[c].contains_line(lineno));
        let n = app.annotations_on(lineno);

        // Syntax first, then selection, then the cursor: later marks win, so
        // highlighting never hides where you are or what you have chosen.
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
        if on_cursor_line {
            let c0 = app.cursor.col.saturating_sub(1).min(text.len());
            let c1 = text[c0..]
                .char_indices()
                .nth(1)
                .map_or(text.len(), |(i, _)| c0 + i);
            if c1 > c0 {
                marks.push((c0, c1, cur_style));
            }
        }

        let selected_here = app.line_selected(lineno);
        let bar = if in_current || selected_here {
            "▍"
        } else {
            " "
        };

        let mut spans = vec![
            Span::styled(
                format!("{lineno:>4} "),
                Style::default().fg(if on_cursor_line {
                    Color::Yellow
                } else {
                    Color::DarkGray
                }),
            ),
            Span::styled(
                bar,
                Style::default().fg(if selected_here {
                    Color::Yellow
                } else {
                    Color::DarkGray
                }),
            ),
        ];

        let body = segments(&text, &marks, Style::default());
        if body.is_empty() && on_cursor_line {
            spans.push(Span::styled(" ", cur_style));
        } else {
            spans.extend(body);
        }

        if n > 0 {
            spans.push(Span::styled(
                format!("  ●{n}"),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        rows.push(Line::from(spans));
    }

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

    // The selection readout goes first: it is the only field that changes on
    // every keypress, and a long path used to push it off the end of the border
    // entirely. The path is last and shortened to whatever is left, because a
    // truncated path is still recognisable and a missing selection is not.
    let rest = format!(
        " [{}] · {} lines · {} units · {} annotations · ",
        sel,
        app.lines.len(),
        app.blocks.len(),
        app.annotations.len(),
    );
    let budget = usize::from(area.width).saturating_sub(rest.chars().count() + 3);
    let title = format!("{rest}{} ", shorten_path(&app.path, budget));

    f.render_widget(
        Paragraph::new(rows).block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
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

fn keep_cursor_visible(app: &App, scroll: &mut usize, viewport: usize) {
    if viewport == 0 {
        return;
    }
    let cursor0 = app.cursor.line.saturating_sub(1);
    if cursor0 < *scroll {
        *scroll = cursor0;
    } else if cursor0 >= *scroll + viewport {
        *scroll = cursor0 + 1 - viewport;
    }
    let max_scroll = app.lines.len().saturating_sub(viewport);
    *scroll = (*scroll).min(max_scroll);
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
                    Span::raw(a.text.clone()),
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
const KEYS: [&str; 5] = [
    "hjkl move · ^d/^u/^f/^b page · J/K unit · v units · V lines · +/- widen/narrow · c comment · x remove · q quit",
    "hjkl · J/K unit · v/V select · +/- widen · c comment · x remove · q quit",
    "hjkl · v/V · +/- · c comment · x remove · q quit",
    "c comment · x remove · q quit",
    "q quit",
];
const INPUT_KEYS: [&str; 2] = ["Enter save · Esc cancel", "Enter · Esc"];

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
    fn render(app: &mut App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut scroll = 0usize;
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
        assert!(screen.contains("●1"));
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
