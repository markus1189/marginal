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
    let input_h = if app.mode == Mode::Input { 3 } else { 0 };
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
            .map(|(_, _, s)| *s)
            .unwrap_or(base);
        out.push(Span::styled(text[a..b].to_string(), style));
    }
    out
}

fn draw_source(f: &mut Frame, area: Rect, app: &mut App, scroll: &mut usize) {
    let viewport = area.height.saturating_sub(2) as usize;
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
        let in_current = current
            .map(|c| app.blocks[c].contains_line(lineno))
            .unwrap_or(false);
        let n = app.annotations_on(lineno);

        let mut marks: Vec<(usize, usize, Style)> = Vec::new();
        if let Some((a, b)) = app.selected_bytes_on(lineno) {
            marks.push((a, b, sel_style));
        }
        if on_cursor_line {
            let c0 = app.cursor.col.saturating_sub(1).min(text.len());
            let c1 = text[c0..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| c0 + i)
                .unwrap_or(text.len());
            if c1 > c0 {
                marks.push((c0, c1, cur_style));
            }
        }

        let selected_here = app.line_selected(lineno);
        let bar = if in_current || selected_here { "▍" } else { " " };

        let mut spans = vec![
            Span::styled(
                format!("{:>4} ", lineno),
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
                format!("  ●{}", n),
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

    let title = format!(
        " {} · {} lines · {} units · {} annotations · [{}] ",
        app.path,
        app.lines.len(),
        app.blocks.len(),
        app.annotations.len(),
        sel
    );

    f.render_widget(
        Paragraph::new(rows).block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
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
        " comment on {} {} — Enter to save, Esc to cancel ",
        app.selection_kind(),
        range
    );
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Yellow)),
            Span::raw(app.input.clone()),
            Span::styled("█", Style::default().fg(Color::Yellow)),
        ]))
        .block(
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
                    format!("L{}:{}-{}:{}", a.start_line, a.start_col, a.end_line, a.end_col)
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
        Paragraph::new(rows)
            .block(Block::default().borders(Borders::ALL).title(" annotations ")),
        area,
    );
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let keys = if app.mode == Mode::Input {
        "Enter save · Esc cancel"
    } else {
        "hjkl move · J/K unit · v units · V lines · +/- widen/narrow · c comment · x remove · q quit"
    };
    let cols = Layout::horizontal([Constraint::Min(10), Constraint::Length(28)]).split(area);
    f.render_widget(
        Paragraph::new(Span::styled(
            format!(" {}", keys),
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
        assert!(screen.contains("PLAN.md · 6 lines · 4 units · 0 annotations"));
        assert!(screen.contains("   1 ▍# Steps"));
        assert!(screen.contains("   3  - [ ] Add validation to the login form"));
        assert!(screen.contains("no annotations yet"));
        assert!(screen.contains("hjkl move"));
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
        app.input = "model layer".into();
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
        assert!(screen.contains("Enter to save, Esc to cancel"));
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
