//! The `?` overlay: every Normal-mode key, grouped, over the source view.
//!
//! `KEYS` below is the one list of bindings the program shows, and the README's
//! Keys table is the one list it documents. A test reads the README and fails
//! when the two name different keys, so adding a binding to one and not the
//! other fails `cargo test` rather than rotting the documentation. The
//! descriptions may be shorter than the README's; the key cells may not differ.
//!
//! Kept out of `ui.rs` on purpose: the overlay shares nothing with the source
//! view but `Frame`, and a module of its own keeps the table and its drift test
//! next to each other.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::wrap::{cells_claimed, wrap};

/// `(group, [(key, what it does)])`, in the order the overlay shows them. The
/// key strings are the README's Keys column with the backticks taken out.
pub const KEYS: [(&str, &[(&str, &str)]); 5] = [
    (
        "move",
        &[
            ("h/j/k/l, arrows", "move by character / line"),
            ("C-d / C-u", "half page down / up"),
            ("C-f / C-b, PgDn/PgUp", "full page down / up"),
            ("0 / $, Home/End", "start / end of line"),
            ("J / K", "move by navigation unit"),
            ("w / b", "move by inline node: code span, link, emphasis"),
            ("g / G", "first / last line"),
            ("C-n / C-p", "move by screen row inside a wrapped line"),
            ("] / [", "next / previous mark: annotation, question, step"),
        ],
    ),
    (
        "select",
        &[
            ("v", "select a range of units; J/K extends"),
            ("V", "select whole lines; j/k extends"),
            (
                "+ / - (also = / _)",
                "widen / narrow along the markdown hierarchy",
            ),
            ("Esc", "drop the selection"),
        ],
    ),
    (
        "annotate",
        &[
            ("Enter (also c)", "comment on the selection"),
            ("C", "general comment on the whole document"),
            (
                "y / n",
                "annotate the selection yes / no; flips the other answer",
            ),
            ("x", "remove the newest annotation on this line"),
            (
                "e",
                "edit the newest annotation on this line; empty removes it",
            ),
            ("E", "edit the newest general comment; empty removes it"),
        ],
    ),
    (
        "view",
        &[
            ("P", "pretty on / off: soft wrap, aligned tables"),
            ("z", "peek at the selection, wrapped"),
            ("#", "numbered-list steps in / out of the mark ring"),
            ("?", "this list; j/k scroll, ? / Esc / q close"),
        ],
    ),
    ("quit", &[("q, C-c", "quit")]),
];

/// Widest key cell, which is where every description starts.
fn key_width() -> usize {
    KEYS.iter()
        .flat_map(|(_, keys)| keys.iter())
        .map(|(k, _)| cells_claimed(k))
        .max()
        .unwrap_or(0)
}

/// The overlay's rows at `width` inner cells.
///
/// Two columns where a description gets at least 16 cells beside the widest
/// key; below that each key is on a row of its own and its description on the
/// rows under it, because a description squeezed into six cells beside its key
/// is a column of word fragments.
fn rows(width: usize) -> Vec<Line<'static>> {
    let kw = key_width();
    let key_style = Style::default().fg(Color::Magenta);
    let head_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let side_by_side = width >= 2 + kw + 2 + 16;
    let mut out = Vec::new();
    for (i, (group, keys)) in KEYS.iter().enumerate() {
        if i > 0 {
            out.push(Line::raw(""));
        }
        out.push(Line::styled((*group).to_string(), head_style));
        for (key, what) in *keys {
            if side_by_side {
                let indent = 2 + kw + 2;
                for (j, row) in wrap(what, width - indent).into_iter().enumerate() {
                    let lead = if j == 0 {
                        Span::styled(format!("  {key:<kw$}  "), key_style)
                    } else {
                        Span::raw(" ".repeat(indent))
                    };
                    out.push(Line::from(vec![lead, Span::raw(row)]));
                }
            } else {
                out.push(Line::styled(format!(" {key}"), key_style));
                for row in wrap(what, width.saturating_sub(3).max(1)) {
                    out.push(Line::raw(format!("   {row}")));
                }
            }
        }
    }
    out
}

/// The first title in `choices` that fits a border `width` cells wide, or none.
fn fitting(choices: &[String], width: usize) -> String {
    choices
        .iter()
        .find(|t| cells_claimed(t) <= width.saturating_sub(2))
        .cloned()
        .unwrap_or_default()
}

/// Draw the overlay centred on `area`, clamping `app.help`'s scroll to what
/// the rows at this size allow. The scroll is clamped here and not in `App`
/// because only the renderer knows how many rows there are — the same reason
/// `peek_rows` is published by the peek overlay.
pub fn draw(f: &mut Frame, area: Rect, app: &mut App) {
    let Some(scroll) = app.help else { return };
    let w = area.width.saturating_sub(4).clamp(area.width.min(20), 76);
    let inner_w = usize::from(w.saturating_sub(2));
    let all = rows(inner_w);
    let want_h = u16::try_from(all.len() + 2).unwrap_or(u16::MAX);
    let h = want_h.min(area.height.saturating_sub(2).max(area.height.min(3)));
    let popup = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };

    let inner_h = usize::from(h.saturating_sub(2));
    let top = scroll.min(all.len().saturating_sub(inner_h));
    app.help = Some(top);
    let end = (top + inner_h).min(all.len());
    let shown: Vec<Line> = all[top..end].to_vec();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let w = usize::from(w);
    let block = if end - top < all.len() {
        let range = format!("{}-{end}/{}", top + 1, all.len());
        let title = fitting(
            &[
                format!(" keys {range} — j/k scroll · ? closes "),
                format!(" keys {range} "),
                " keys ".into(),
            ],
            w,
        );
        // The count says there is more; the bottom border says which way.
        let more = if end < all.len() && top > 0 {
            " ↑↓ more "
        } else if end < all.len() {
            " ↓ more "
        } else {
            " ↑ more "
        };
        block
            .title(title)
            .title_bottom(fitting(&[more.to_string()], w))
    } else {
        block.title(fitting(&[" keys — ? closes ".into(), " keys ".into()], w))
    };

    f.render_widget(Clear, popup);
    f.render_widget(Paragraph::new(shown).block(block), popup);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Anchor;
    use crate::format::Format;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// The key cells of the README's `## Keys` table, backticks removed.
    ///
    /// Read at run time, not with `include_str!`: the clippy, fmt and machete
    /// checks build from a fileset without the README, deliberately, so that a
    /// prose edit does not re-run them — and `include_str!` would fail to
    /// compile there. The `build` check, which runs this test, carries it.
    fn readme_keys() -> Vec<String> {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/README.md");
        let readme = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("{path}: {e} — the key table has nothing to check against"));
        let section = readme
            .split("\n## Keys\n")
            .nth(1)
            .expect("README has a `## Keys` section");
        section
            .lines()
            .skip_while(|l| !l.starts_with('|'))
            .take_while(|l| l.starts_with('|'))
            .skip(2) // header and delimiter row
            .map(|l| l.split('|').nth(1).unwrap_or("").trim().replace('`', ""))
            .collect()
    }

    fn overlay_keys() -> Vec<String> {
        KEYS.iter()
            .flat_map(|(_, keys)| keys.iter())
            .map(|(k, _)| (*k).to_string())
            .collect()
    }

    #[test]
    fn the_overlay_and_the_readme_list_the_same_keys() {
        let readme = readme_keys();
        let overlay = overlay_keys();
        assert!(readme.len() > 10, "README table not found: {readme:?}");
        for k in &readme {
            assert!(overlay.contains(k), "README documents {k:?}; `?` does not");
        }
        for k in &overlay {
            assert!(readme.contains(k), "`?` lists {k:?}; the README does not");
        }
        assert_eq!(readme.len(), overlay.len(), "a key listed twice");
    }

    fn render(app: &mut App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut scroll = Anchor::default();
        term.draw(|f| crate::ui::draw(f, app, &mut scroll)).unwrap();
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

    fn open() -> App {
        let mut app = App::open("PLAN.md".into(), "# Plan\n\ntext\n", Format::Markdown);
        app.toggle_help();
        app
    }

    /// Everything fits at 120x40: every group heading and every key, and no
    /// claim that there is more.
    #[test]
    fn at_120x40_every_key_is_on_screen() {
        let mut app = open();
        let screen = render(&mut app, 120, 40);
        for (group, keys) in KEYS {
            assert!(screen.contains(group), "{group}\n{screen}");
            for (key, _) in keys {
                assert!(screen.contains(key), "{key}\n{screen}");
            }
        }
        assert!(screen.contains(" keys — ? closes "), "{screen}");
        assert!(!screen.contains("more"), "{screen}");
    }

    /// 80x24 does not hold the whole list, so it says how much it shows and
    /// that there is more below — and `j` gets to the end of it.
    #[test]
    fn at_80x24_it_scrolls_and_says_so() {
        let mut app = open();
        let screen = render(&mut app, 80, 24);
        assert!(screen.contains("move"), "{screen}");
        assert!(screen.contains("h/j/k/l, arrows"), "{screen}");
        assert!(screen.contains("move by character / line"), "{screen}");
        assert!(screen.contains(" keys 1-"), "{screen}");
        assert!(screen.contains("↓ more"), "{screen}");

        for _ in 0..200 {
            app.scroll_help(1);
        }
        let screen = render(&mut app, 80, 24);
        assert!(screen.contains("q, C-c"), "{screen}");
        assert!(screen.contains("↑ more"), "{screen}");
        assert!(!screen.contains("↓"), "scrolled to the end: {screen}");

        // Clamped, so one `k` moves the view at once rather than paying back
        // the 200 presses past the end first.
        let before = app.help;
        app.scroll_help(-1);
        render(&mut app, 80, 24);
        assert_eq!(app.help, before.map(|t| t - 1));
    }

    /// 40x12: too narrow for two columns, so each key heads its own rows; the
    /// frame still says there is more, and nothing panics.
    #[test]
    fn at_40x12_it_stacks_and_still_says_there_is_more() {
        let mut app = open();
        let screen = render(&mut app, 40, 12);
        assert!(screen.contains(" h/j/k/l, arrows"), "{screen}");
        assert!(screen.contains("   move by character"), "{screen}");
        assert!(screen.contains("more"), "{screen}");
        for _ in 0..500 {
            app.scroll_help(1);
        }
        let screen = render(&mut app, 40, 12);
        assert!(screen.contains("q, C-c"), "{screen}");
    }

    /// Down to nothing at all: every size renders without a panic.
    #[test]
    fn no_terminal_size_panics() {
        for w in 1..=50 {
            for h in 1..=14 {
                let mut app = open();
                render(&mut app, w, h);
            }
        }
    }

    #[test]
    fn closing_it_leaves_the_source_view() {
        let mut app = open();
        app.toggle_help();
        assert_eq!(app.help, None);
        let screen = render(&mut app, 80, 24);
        assert!(!screen.contains(" keys"), "{screen}");
    }
}
