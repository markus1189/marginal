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

/// Byte length of a heading's leading `#` run plus the space after it.
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
    let rest = match line.get(from..) {
        Some(r) => r,
        None => return 0,
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
    // task checkbox
    if b.get(i) == Some(&b'[') && b.get(i + 2) == Some(&b']') {
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
                let len = lines.get(line - 1).map(|l| l.len()).unwrap_or(0);
                if let Some((a, b)) = span.byte_range_on(line, len) {
                    add(out, line, a, b, tag);
                }
            }
        }

        // Markers get their own, dimmer tag. Pushed after the node's own mark
        // so they win, and before the children so real content still wins.
        match child.kind {
            "heading" => {
                let l = lines.get(span.start.line - 1).copied().unwrap_or("");
                let n = heading_marker_len(l);
                add(out, span.start.line, 0, n, "heading-marker");
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

    fn tags(src: &str, line: usize) -> Vec<(usize, usize, &'static str)> {
        marks(&parse_tree(src), src)
            .get(line - 1)
            .cloned()
            .unwrap_or_default()
    }

    #[test]
    fn heading_is_tagged_and_its_hashes_are_separate() {
        let src = "## Steps here\n";
        let m = tags(src, 1);
        assert!(m.contains(&(0, 13, "heading")));
        assert!(m.contains(&(0, 3, "heading-marker")));
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
        assert!(m.iter().any(|(a, b, t)| *t == "code-span" && &src[*a..*b] == "`code`"));
        assert!(m.iter().any(|(a, b, t)| *t == "strong" && &src[*a..*b] == "**bold**"));
        assert!(m.iter().any(|(a, b, t)| *t == "emph" && &src[*a..*b] == "*soft*"));
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
            assert!(src.is_char_boundary(a) && src.is_char_boundary(b), "{a}..{b}");
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
