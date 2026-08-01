//! Markdown syntax highlighting, taken from the AST we already have.
//!
//! No second parser and no regexes: `blocks::parse_tree` already knows where
//! every heading, fence, link and emphasis run is, to the byte. This turns that
//! into per-line byte ranges tagged with a style name, which `ui` maps to
//! actual colours — keeping ratatui out of the model.
//!
//! Marks are emitted parent-before-child, and the renderer lets later marks
//! win, so a `strong` run inside a blockquote overrides the quote's styling.

use crate::blocks::TreeNode;

/// `(start_byte, end_byte, tag)` ranges for one line.
pub type LineMarks = Vec<(usize, usize, &'static str)>;

fn tag_of(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "heading" => "heading",
        "code" => "code",
        "code-span" => "code-span",
        "link" => "link",
        "image" => "image",
        "strong" => "strong",
        "emph" => "emph",
        "strike" => "strike",
        "html" | "html-inline" => "html",
        "hr" => "hr",
        "blockquote" => "quote",
        "table-row" => "table",
        "table-cell" => "cell",
        // Paragraphs, lists, items and text runs carry no styling of their own;
        // tagging them would flatten everything nested inside.
        _ => return None,
    })
}

/// Byte length of an ATX heading's leading `#` run plus the spaces after it.
///
/// Only sound for a heading comrak parsed as ATX, where the run at the
/// heading's own start column *is* the marker. Asking the same question of a
/// setext heading measures its text.
fn heading_marker_len(line: &str) -> usize {
    let hashes = line.bytes().take_while(|b| *b == b'#').count();
    if hashes == 0 {
        return 0;
    }
    let rest = &line[hashes..];
    hashes + rest.len() - rest.trim_start_matches(' ').len()
}

/// Byte length of a list item's marker: bullet or ordinal, trailing spaces, and
/// a task checkbox if present. Measured from `from` within `line`.
fn list_marker_len(line: &str, from: usize) -> usize {
    let Some(rest) = line.get(from..) else {
        return 0;
    };
    let mut i = 0;
    let b = rest.as_bytes();
    if b.first().is_some_and(|c| matches!(c, b'-' | b'*' | b'+')) {
        i = 1;
    } else {
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == 0 || !b.get(i).is_some_and(|c| matches!(c, b'.' | b')')) {
            return 0;
        }
        i += 1;
    }
    while b.get(i) == Some(&b' ') {
        i += 1;
    }
    // Task checkbox. The middle byte has to be a real task marker: without that
    // test any one-character bracket run matched, so `- [a] text` -- a shortcut
    // reference label, which produces no link node to override it -- was tagged
    // as list chrome up to the closing bracket.
    //
    // Byte-safe: `[`, the marker and `]` are all ASCII, and no byte of a
    // multi-byte sequence is ever below 0x80, so `i += 3` cannot land
    // mid-character.
    if b.get(i) == Some(&b'[')
        && b.get(i + 1)
            .is_some_and(|c| matches!(c, b' ' | b'x' | b'X'))
        && b.get(i + 2) == Some(&b']')
    {
        i += 3;
        while b.get(i) == Some(&b' ') {
            i += 1;
        }
    }
    i
}

fn add(out: &mut [LineMarks], line: usize, a: usize, b: usize, tag: &'static str) {
    if b > a {
        if let Some(row) = out.get_mut(line.saturating_sub(1)) {
            row.push((a, b, tag));
        }
    }
}

fn walk(node: &TreeNode, lines: &[&str], out: &mut Vec<LineMarks>) {
    for child in &node.children {
        let span = child.span;

        if let Some(tag) = tag_of(child.kind) {
            for line in span.start.line..=span.end.line {
                let len = lines.get(line - 1).map_or(0, |l| l.len());
                if let Some((a, b)) = span.byte_range_on(line, len) {
                    add(out, line, a, b, tag);
                }
            }
        }

        // Markers get their own, dimmer tag. Pushed after the node's own mark
        // so they win, and before the children so real content still wins.
        match child.kind {
            // ATX only. A setext heading is underlined, not opened, so its first
            // line is ordinary text — and a `#` run measured there is part of
            // that text: `#hashtag` over `===` had its `#` dimmed as chrome, and
            // `####### seven` (too many hashes to be ATX at all) lost eight
            // bytes. comrak already knows which of the two it parsed; the `#`
            // run alone cannot tell them apart.
            "heading" if !child.setext => {
                // From the heading's own start, not from byte 0. A heading is
                // not always flush left — up to three leading spaces are still
                // one, and headings inside blockquotes and list items are
                // ordinary markdown — and measuring the `#` run from byte 0
                // yielded 0 for every one of them, so `add` dropped the mark and
                // the marker rendered in the heading's own colour. The adjacent
                // list-item arm already does exactly this.
                let l = lines.get(span.start.line - 1).copied().unwrap_or("");
                let from = span.start.col - 1;
                let n = l.get(from..).map_or(0, heading_marker_len);
                add(out, span.start.line, from, from + n, "heading-marker");
            }
            "list-item" => {
                let l = lines.get(span.start.line - 1).copied().unwrap_or("");
                let from = span.start.col - 1;
                let n = list_marker_len(l, from);
                add(out, span.start.line, from, from + n, "list-marker");
            }
            _ => {}
        }

        walk(child, lines, out);
    }
}

/// One entry per source line, in order.
pub fn marks(tree: &TreeNode, src: &str) -> Vec<LineMarks> {
    let lines: Vec<&str> = src.lines().collect();
    let mut out = vec![LineMarks::new(); lines.len()];
    walk(tree, &lines, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::parse_tree;

    fn tags(src: &str, line: usize) -> LineMarks {
        marks(&parse_tree(src), src)
            .get(line - 1)
            .cloned()
            .unwrap_or_default()
    }

    /// Every case in a table, against the *whole* mark list for its first line.
    /// Whole, because `contains` cannot see a mark that should not be there —
    /// which is what a marker run over ordinary text is. Every case, because a
    /// bug in this file is usually a class, and one run should show all of it
    /// rather than the first member.
    fn every_case(cases: &[(&str, LineMarks)]) {
        let bad: Vec<String> = cases
            .iter()
            .filter(|(src, want)| tags(src, 1) != *want)
            .map(|(src, want)| format!("{src:?}\n    want {want:?}\n     got {:?}", tags(src, 1)))
            .collect();
        assert!(bad.is_empty(), "\n{}", bad.join("\n"));
    }

    #[test]
    fn heading_is_tagged_and_its_hashes_are_separate() {
        assert_eq!(
            tags("## Steps here\n", 1),
            vec![(0, 13, "heading"), (0, 3, "heading-marker")]
        );
    }

    /// The marker was measured from byte 0 of the line and emitted there, so
    /// any heading not literally starting with `#` produced a run of length 0
    /// and `add` dropped the mark — the `#` then rendered in the heading's own
    /// colour rather than the dimmer marker one. Up to three leading spaces
    /// still make a heading, and headings inside blockquotes and list items are
    /// ordinary markdown.
    ///
    /// Whole mark lists, not `contains`: the indent boundary is only half
    /// checked if a fourth space may quietly keep producing heading marks on
    /// what is by then an indented code block.
    #[test]
    fn a_heading_that_is_not_flush_left_still_has_a_marker() {
        every_case(&[
            (
                "## Steps\n",
                vec![(0, 8, "heading"), (0, 3, "heading-marker")],
            ),
            (
                "  # Indented\n",
                vec![(2, 12, "heading"), (2, 4, "heading-marker")],
            ),
            (
                "   # three spaces\n",
                vec![(3, 17, "heading"), (3, 5, "heading-marker")],
            ),
            // Four is one too many: an indented code block, and nothing about
            // it is a heading.
            ("    # four spaces\n", vec![(4, 17, "code")]),
            (
                "> # Quoted\n",
                vec![
                    (0, 10, "quote"),
                    (2, 10, "heading"),
                    (2, 4, "heading-marker"),
                ],
            ),
            (
                "- # In a list item\n",
                vec![
                    (0, 2, "list-marker"),
                    (2, 18, "heading"),
                    (2, 4, "heading-marker"),
                ],
            ),
        ]);
    }

    /// A setext heading is *underlined*, not opened, so its first line is
    /// ordinary text — and the `#` run was measured there all the same. The
    /// marker mark then covered real words: `#hashtag` lost its `#` to the
    /// dimmer marker colour mid-word, and `####### seven` — seven hashes, too
    /// many to open an ATX heading at all — lost eight bytes. comrak reports
    /// which of the two forms it parsed; the `#` run alone cannot tell them
    /// apart, since a valid ATX opener can never begin a setext heading's text.
    ///
    /// `contains` could not have caught this: the defect is a mark that should
    /// not be there, and every mark the old assertions asked for was present.
    #[test]
    fn a_setext_heading_has_no_hashes_to_dim() {
        every_case(&[
            ("Setext\n===\n", vec![(0, 6, "heading")]),
            // Flush left misbehaved before the marker was ever moved off byte
            // 0 — the container cases only widened the class.
            ("#hashtag\n===\n", vec![(0, 8, "heading")]),
            ("  #not-atx\n  ---\n", vec![(2, 10, "heading")]),
            (
                "> #hashtag\n> ===\n",
                vec![(0, 10, "quote"), (2, 10, "heading")],
            ),
            (
                "> ####### seven\n> ===\n",
                vec![(0, 15, "quote"), (2, 15, "heading")],
            ),
            (
                "- #hashtag\n  ===\n",
                vec![(0, 2, "list-marker"), (2, 10, "heading")],
            ),
        ]);
    }

    #[test]
    fn list_marker_is_separated_from_the_item_text() {
        let src = "- item a\n";
        assert!(tags(src, 1).contains(&(0, 2, "list-marker")));
    }

    #[test]
    fn task_checkbox_counts_as_part_of_the_marker() {
        let src = "- [ ] Add validation\n";
        assert!(tags(src, 1).contains(&(0, 6, "list-marker")));
        for done in ["- [x] Done\n", "- [X] Done\n"] {
            assert!(tags(done, 1).contains(&(0, 6, "list-marker")), "{done:?}");
        }
    }

    /// The rule asked only for a bracket pair with one byte between, never that
    /// the byte was a task marker — so a shortcut reference label was swallowed
    /// as list chrome. `- [a](b)` survived only because the later `link` mark
    /// happened to win; an unresolved shortcut reference produces no link node,
    /// so nothing overrode it.
    #[test]
    fn a_one_character_link_label_is_not_a_checkbox() {
        assert_eq!(tags("- [a] text\n", 1), vec![(0, 2, "list-marker")]);
        // A two-character label was already rejected, which is what showed the
        // rule was matching on length rather than on content.
        assert_eq!(tags("- [ab] text\n", 1), vec![(0, 2, "list-marker")]);
    }

    #[test]
    fn ordered_list_markers_are_recognised() {
        assert!(tags("1. first\n", 1).contains(&(0, 3, "list-marker")));
        assert!(tags("12) twelfth\n", 1).contains(&(0, 4, "list-marker")));
    }

    #[test]
    fn nested_item_marker_starts_at_its_own_indent() {
        let src = "- outer\n  - inner\n";
        let m = tags(src, 2);
        assert!(m.contains(&(2, 4, "list-marker")), "{m:?}");
    }

    #[test]
    fn inline_spans_are_tagged_with_their_delimiters() {
        let src = "Use `code` and **bold** and *soft*.\n";
        let m = tags(src, 1);
        assert!(m
            .iter()
            .any(|(a, b, t)| *t == "code-span" && &src[*a..*b] == "`code`"));
        assert!(m
            .iter()
            .any(|(a, b, t)| *t == "strong" && &src[*a..*b] == "**bold**"));
        assert!(m
            .iter()
            .any(|(a, b, t)| *t == "emph" && &src[*a..*b] == "*soft*"));
    }

    #[test]
    fn links_are_tagged_across_label_and_target() {
        let src = "See [the docs](https://example.com) now.\n";
        let m = tags(src, 1);
        assert!(m
            .iter()
            .any(|(a, b, t)| *t == "link" && &src[*a..*b] == "[the docs](https://example.com)"));
    }

    #[test]
    fn a_fence_is_tagged_on_every_line_including_both_delimiters() {
        let src = "```go\nfmt.Println()\n```\n";
        for line in 1..=3 {
            assert!(
                tags(src, line).iter().any(|(_, _, t)| *t == "code"),
                "line {line} untagged"
            );
        }
    }

    #[test]
    fn a_heading_inside_a_fence_is_code_not_heading() {
        let src = "```\n# not a heading\n```\n";
        let m = tags(src, 2);
        assert!(m.iter().any(|(_, _, t)| *t == "code"));
        assert!(!m.iter().any(|(_, _, t)| t.starts_with("heading")));
    }

    #[test]
    fn nested_emphasis_is_emitted_after_its_container_so_it_wins() {
        let src = "> quoted **bold** text\n";
        let m = tags(src, 1);
        let quote = m.iter().position(|(_, _, t)| *t == "quote").unwrap();
        let strong = m.iter().position(|(_, _, t)| *t == "strong").unwrap();
        assert!(quote < strong, "container must come first: {m:?}");
    }

    #[test]
    fn table_cells_are_emitted_after_their_row() {
        let src = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let m = tags(src, 1);
        let row = m.iter().position(|(_, _, t)| *t == "table").unwrap();
        let cell = m.iter().position(|(_, _, t)| *t == "cell").unwrap();
        assert!(row < cell, "{m:?}");
    }

    #[test]
    fn multibyte_lines_produce_valid_byte_ranges() {
        let src = "Prüfen `köde` — ✓ fertig.\n";
        for (a, b, _) in tags(src, 1) {
            assert!(
                src.is_char_boundary(a) && src.is_char_boundary(b),
                "{a}..{b}"
            );
        }
    }

    #[test]
    fn every_mark_stays_within_its_line() {
        let src = "# H\n\nPara with `code`.\n\n- item\n\n> quote\n";
        let lines: Vec<&str> = src.lines().collect();
        for (i, row) in marks(&parse_tree(src), src).iter().enumerate() {
            for (a, b, t) in row {
                assert!(a <= b, "inverted {t} on line {}", i + 1);
                assert!(*b <= lines[i].len(), "{t} overruns line {}", i + 1);
            }
        }
    }

    #[test]
    fn empty_input_yields_no_marks() {
        assert!(marks(&parse_tree(""), "").is_empty());
    }
}
