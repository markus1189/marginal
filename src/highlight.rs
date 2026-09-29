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
        "front-matter" => "front-matter",
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

/// Byte length of a list item's marker: bullet or ordinal, trailing whitespace,
/// and a task checkbox if present. Measured from `from` within `line`.
///
/// The fallback only — an item whose content begins on its start line is
/// measured against comrak's own idea of where that content begins, in `walk`.
/// This runs for the items that have no content there at all: `-` on its own
/// line, or `- [x]` with nothing after it. Nothing but a checkbox can follow the
/// marker in that case, since any other content would be a child node on this
/// very line, so the rule below is never asked a question it can get wrong.
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
    while b.get(i).is_some_and(|c| matches!(c, b' ' | b'\t')) {
        i += 1;
    }
    // Task checkbox. The middle byte has to be a real task marker, and the
    // bracket pair has to be followed by whitespace or the end of the line --
    // both are what comrak's tasklist scanner demands, and it is the second that
    // makes this a checkbox rather than the opening of ordinary text.
    //
    // Byte-safe: `[`, the marker and `]` are all ASCII, and no byte of a
    // multi-byte sequence is ever below 0x80, so `i += 3` cannot land
    // mid-character.
    if b.get(i) == Some(&b'[')
        && b.get(i + 1)
            .is_some_and(|c| matches!(c, b' ' | b'x' | b'X'))
        && b.get(i + 2) == Some(&b']')
        && b.get(i + 3).is_none_or(|c| matches!(c, b' ' | b'\t'))
    {
        i += 3;
        while b.get(i).is_some_and(|c| matches!(c, b' ' | b'\t')) {
            i += 1;
        }
    }
    i
}

/// Bytes at the head of a continuation line that belong to the containers a
/// node sits in rather than to the node: quote markers and indentation, and no
/// more of them than the `lead` bytes in front of the node on its first line.
///
/// A span's continuation lines start at column 1 as comrak reports them, so a
/// fence inside a quote painted each `> ` in code colour on top of the quote's
/// own, and a link wrapped inside a list item underlined the item's
/// indentation. The node starts where its first line says it does, and its
/// later lines repeat that line's chrome — the list marker, footnote label or
/// quote marker there becomes indentation or a marker here, never content.
///
/// Capped by `lead` so a deeper continuation keeps its relative indentation —
/// a code line indented past its fence is code — and a lazy continuation, which
/// repeats no chrome at all, is left whole. Unlike `App::slice`, which trims
/// only what the lead's quote markers and spaces literally repeat, this also
/// skips the indentation that stands in for a list marker: whitespace is
/// invisible to `slice`'s quoted text but not to an underline.
fn chrome_len(text: &str, lead: usize) -> usize {
    text.bytes()
        .take(lead)
        .take_while(|b| matches!(b, b'>' | b' ' | b'\t'))
        .count()
}

/// The fence character and run length opening `line` at byte `from`, if a
/// fence opens there: three or more backticks or tildes.
fn fence_at(line: &str, from: usize) -> Option<(u8, usize)> {
    let rest = line.get(from..)?.as_bytes();
    let c = *rest.first().filter(|c| matches!(c, b'`' | b'~'))?;
    let n = rest.iter().take_while(|b| **b == c).count();
    (n >= 3).then_some((c, n))
}

/// Whether a fence opened `(c, n)` names a unified diff: its info string's
/// first word is `diff` or `patch`, in any case.
fn is_diff_fence(line: &str, from: usize, (_, n): (u8, usize)) -> bool {
    line.get(from + n..)
        .and_then(|info| info.split_whitespace().next())
        .is_some_and(|w| w.eq_ignore_ascii_case("diff") || w.eq_ignore_ascii_case("patch"))
}

/// `@@ -a,b +c,d @@` → the line counts `(b, d)` its body holds. A count
/// left out is 1, as in the unified format. `None` for a header this cannot
/// read, which the caller treats as a hunk with no known end.
fn hunk_counts(header: &str) -> Option<(usize, usize)> {
    let mut it = header.split_whitespace().skip(1);
    let count = |s: &str, sign: char| -> Option<usize> {
        let s = s.strip_prefix(sign)?;
        s.split_once(',').map_or(Some(1), |(_, n)| n.parse().ok())
    };
    Some((count(it.next()?, '-')?, count(it.next()?, '+')?))
}

/// Per-line tags for the body of a ```` ```diff ```` or ```` ```patch ````
/// fence: `diff-add` for a `+` line, `diff-del` for a `-` line, `diff-hunk`
/// for an `@@` header and `diff-meta` for the `---`/`+++` file header pair.
/// Emitted after the fence's own `code` mark, so they win over it.
///
/// The one ambiguity is a removed line whose text starts `--` (a SQL or Lua
/// comment, a flag): as a diff line it reads `---`, exactly like a file
/// header. Two rules settle it the way `git apply` does. Inside a hunk whose
/// header gave line counts, a line is a body line until those counts run out,
/// whatever it starts with. Outside one, `---` is a header only when a `+++`
/// line follows it — which is also the only way the `/marginal-diff` launcher's
/// fences, one hunk body each with the `@@` line lifted into a heading, can
/// hold a `---` at all.
fn diff_marks(code: &TreeNode, lines: &[&str], out: &mut [LineMarks]) {
    let span = code.span;
    let lead = span.start.col - 1;
    let opener = lines.get(span.start.line - 1).copied().unwrap_or("");
    let Some(fence) = fence_at(opener, lead) else {
        return; // an indented code block has no info string
    };
    if !is_diff_fence(opener, lead, fence) {
        return;
    }
    // The last line is a closing fence unless the block ran to the end of
    // its container unclosed.
    let body_of = |l: usize| {
        let text = lines.get(l - 1).copied().unwrap_or("");
        let a = chrome_len(text, lead);
        (text, a)
    };
    let mut last = span.end.line;
    if last > span.start.line {
        let (text, a) = body_of(last);
        let t = text[a..].trim_matches([' ', '\t']);
        if t.len() >= fence.1 && t.bytes().all(|b| b == fence.0) {
            last -= 1;
        }
    }

    // Lines left in the current hunk, old side and new side; `None` for a
    // hunk whose header did not say.
    let mut hunk: Option<Option<(usize, usize)>> = None;
    let mut l = span.start.line + 1;
    while l <= last {
        let (text, a) = body_of(l);
        let body = &text[a..];
        let len = text.len();
        let next = (l < last).then(|| {
            let (t, b) = body_of(l + 1);
            &t[b..]
        });
        let tag = if body.starts_with("@@") {
            hunk = Some(hunk_counts(body));
            "diff-hunk"
        } else if let Some(Some((o, n))) = hunk
            .as_mut()
            .filter(|h| matches!(h, Some((o, n)) if *o + *n > 0))
        {
            match body.as_bytes().first() {
                Some(b'+') => {
                    *n = n.saturating_sub(1);
                    "diff-add"
                }
                Some(b'-') => {
                    *o = o.saturating_sub(1);
                    "diff-del"
                }
                Some(b'\\') => "",
                _ => {
                    *o = o.saturating_sub(1);
                    *n = n.saturating_sub(1);
                    ""
                }
            }
        } else if body.starts_with("---") && next.is_some_and(|n| n.starts_with("+++")) {
            let (t, b) = body_of(l + 1);
            add(out, l, a, len, "diff-meta");
            add(out, l + 1, b, t.len(), "diff-meta");
            hunk = None;
            l += 2;
            continue;
        } else if body.starts_with('+') {
            "diff-add"
        } else if body.starts_with('-') {
            "diff-del"
        } else {
            ""
        };
        if !tag.is_empty() {
            add(out, l, a, len, tag);
        }
        l += 1;
    }
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
            let lead = span.start.col - 1;
            for line in span.start.line..=span.end.line {
                let text = lines.get(line - 1).copied().unwrap_or("");
                if let Some((a, b)) = span.byte_range_on(line, text.len()) {
                    let a = if line == span.start.line {
                        a
                    } else {
                        chrome_len(text, lead)
                    };
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
            "code" => diff_marks(child, lines, out),
            "list-item" => {
                // The marker ends where the item's content begins, and comrak
                // already reports that: the first child's start column is placed
                // after the bullet, its padding *and* the task checkbox, because
                // the tasklist extension moves that paragraph's start column
                // past the checkbox it consumed. Measuring the marker instead
                // meant re-deciding whether a bracket pair was a checkbox, and
                // the lexical rule disagreed with comrak four ways: `- [x]and`
                // is a plain item whose text is `[x]and` (no whitespace after
                // the `]`, so no checkbox) and lost five bytes to list chrome;
                // `-\t[x] a` and `- [x]\ttab` are task items whose checkbox the
                // space-only skip never reached; and `-     [x] a` is an item
                // holding an indented code block whose first line is `[x] a`,
                // dimmed as a marker. `- [x](b) t` was wrong too, and only
                // looked right because the `link` mark lands on top of it.
                //
                // Knowing merely *that* comrak parsed a task item would not
                // settle any of these — the question is where the marker ends,
                // and the child's column answers it directly.
                let l = lines.get(span.start.line - 1).copied().unwrap_or("");
                let from = span.start.col - 1;
                let to = child
                    .children
                    .first()
                    .filter(|c| c.span.start.line == span.start.line)
                    .map_or_else(
                        // No content on this line to end the marker: `-` alone,
                        // or `- [x]` with nothing after it.
                        || from + list_marker_len(l, from),
                        |c| c.span.start.col - 1,
                    )
                    .min(l.len());
                add(out, span.start.line, from, to, "list-marker");
            }
            _ => {}
        }

        walk(child, lines, out);
    }
}

/// One entry per source line, in order.
pub fn marks(tree: &TreeNode, src: &str) -> Vec<LineMarks> {
    let lines = crate::blocks::source_lines(src);
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
        every_case(&[
            ("- [ ] Add validation\n", vec![(0, 6, "list-marker")]),
            ("- [x] Done\n", vec![(0, 6, "list-marker")]),
            ("- [X] Done\n", vec![(0, 6, "list-marker")]),
            ("1. [x] ordered\n", vec![(0, 7, "list-marker")]),
            (
                "> - [x] quoted\n",
                vec![(0, 14, "quote"), (2, 8, "list-marker")],
            ),
        ]);
        // A sublist item measures from its own indent, and its checkbox with it.
        assert_eq!(
            tags("- outer\n  - [x] inner\n", 2),
            vec![(2, 8, "list-marker")]
        );
    }

    /// A bracket pair is only a checkbox if whitespace or the end of the line
    /// follows the `]` — comrak's tasklist scanner demands it, and the rule here
    /// did not. `- [x]and` is therefore a plain item whose text is `[x]and`, and
    /// all five bytes of `- [x]` were painted as list chrome over the front of a
    /// word. `- [x](b) t` was tagged just as wrongly and only looked right
    /// because the `link` mark lands on top of it.
    ///
    /// The measurement now ends where comrak says the item's content begins,
    /// which settles three more disagreements the byte scan could not: the two
    /// tab cases, where the space-only skip never reached the checkbox at all,
    /// and `-     [x] a`, which is five spaces of padding — one past the point
    /// where the rest becomes an indented code block, so `[x] a` is *code* and
    /// was being dimmed as a marker.
    ///
    /// Whole mark lists, not `contains`: every one of these is a marker that
    /// reaches too far or not far enough, and both are invisible to a `contains`
    /// asking for a mark that is present either way.
    #[test]
    fn a_checkbox_needs_whitespace_after_it_to_be_one() {
        every_case(&[
            // Not checkboxes: comrak parsed every one of these as a plain item
            // whose text starts at the `[`.
            ("- [x]and\n", vec![(0, 2, "list-marker")]),
            ("- [a] text\n", vec![(0, 2, "list-marker")]),
            ("- [x](b) t\n", vec![(0, 2, "list-marker"), (2, 8, "link")]),
            // Five spaces after the bullet: an indented code block, not a task.
            (
                "-     [x] a\n",
                vec![(0, 6, "list-marker"), (6, 11, "code")],
            ),
            // Real task items whose checkbox is reached across a tab.
            ("-\t[x] a\n", vec![(0, 6, "list-marker")]),
            ("- [x]\ttab after\n", vec![(0, 6, "list-marker")]),
            // comrak's scanner consumes exactly one space after the `]`; the
            // second belongs to the paragraph, whose text is " a".
            ("-  [x]  a\n", vec![(0, 7, "list-marker")]),
            // No content at all on the line, so there is no child column to
            // measure against and the byte scan answers instead.
            ("- [x]\n", vec![(0, 5, "list-marker")]),
            ("-\n", vec![(0, 1, "list-marker")]),
        ]);
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
        let srcs = [
            "# H\n\nPara with `code`.\n\n- item\n\n> quote\n",
            // A bare `\r` is a line ending to comrak but not to `str::lines`,
            // so a line vector built from the latter runs short and a mark
            // addressed by comrak's line number overruns — or worse, does not,
            // and paints the wrong text in silence.
            "intro\rrest\n\n# H\n\n| a | b |\r|---|---|\n\n- item\n",
        ];
        for src in srcs {
            let lines = crate::blocks::source_lines(src);
            for (i, row) in marks(&parse_tree(src), src).iter().enumerate() {
                for (a, b, t) in row {
                    assert!(a <= b, "inverted {t} on line {} of {src:?}", i + 1);
                    assert!(
                        *b <= lines[i].len(),
                        "{t} overruns line {} of {src:?}",
                        i + 1
                    );
                }
            }
        }
    }

    /// A mark is a byte range into a line, addressed by comrak's line number.
    /// A line vector out of step with comrak does not lose the mark — it paints
    /// it onto whatever text sits at that index instead, which is how a
    /// `list-marker` came to be reported over `| ` on a table row and over
    /// `The` in a paragraph. Assert on what each marker mark *covers*, since
    /// that is the only thing that says it landed on the right line.
    #[test]
    fn a_marker_mark_covers_the_marker_it_was_measured_from() {
        let src = "intro\rrest\n\n## Heading here\n\n- item one\n\n3. ordinal\n";
        let lines = crate::blocks::source_lines(src);
        assert_eq!(lines.len(), 8, "{lines:?}");
        let mut found: Vec<(&str, &str)> = Vec::new();
        for (i, row) in marks(&parse_tree(src), src).iter().enumerate() {
            for (a, b, t) in row {
                if t.ends_with("marker") {
                    found.push((t, &lines[i][*a..*b]));
                }
            }
        }
        assert_eq!(
            found,
            vec![
                ("heading-marker", "## "),
                ("list-marker", "- "),
                ("list-marker", "3. ")
            ]
        );
    }

    /// comrak's continuation lines start at column 1, so a fence in a quote
    /// painted every `> ` in code colour over the quote's own, and a link
    /// wrapped in a list item underlined the item's indentation. A node's later
    /// lines now start past the chrome its first line stood behind — and no
    /// further, so indentation that belongs to the code stays code.
    #[test]
    fn continuation_lines_leave_the_container_chrome_to_the_container() {
        let q = "> ```\n> code\n>   indented\n> ```\n";
        assert_eq!(tags(q, 2), vec![(0, 6, "quote"), (2, 6, "code")]);
        assert_eq!(tags(q, 3), vec![(0, 12, "quote"), (2, 12, "code")]);
        assert_eq!(tags(q, 4), vec![(0, 5, "quote"), (2, 5, "code")]);
        // Doubly quoted: both markers are chrome.
        assert_eq!(
            tags("> > ```\n> > x\n> > ```\n", 2),
            vec![(0, 5, "quote"), (2, 5, "quote"), (4, 5, "code")]
        );
        // Code content that itself opens with `>` is still code: the cap is
        // what the first line's lead held.
        assert_eq!(
            tags("> ```\n> > not a quote\n> ```\n", 2),
            vec![(0, 15, "quote"), (2, 15, "code")]
        );
        // A list item: the marker becomes indentation on the next line.
        let l = "- see [the\n  docs](u) now\n";
        assert_eq!(tags(l, 2), vec![(2, 10, "link")]);
        // A setext underline indented under a list marker.
        assert_eq!(tags("- title\n  ===\n", 2), vec![(2, 5, "heading")]);
        // A lazy continuation repeats no chrome and loses nothing.
        assert_eq!(
            tags("> para [a\nlazy](u)\n", 2),
            vec![(0, 8, "quote"), (0, 8, "link")]
        );
    }

    /// Only the diff tags of each line, so the fence's own `code` mark under
    /// them does not have to be spelled out in every case.
    fn diff_tags(src: &str) -> Vec<Vec<(usize, usize, &'static str)>> {
        marks(&parse_tree(src), src)
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .filter(|m| m.2.starts_with("diff-"))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn a_diff_fence_colours_added_removed_hunk_and_header_lines() {
        let src = "```diff\n--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n ctx\n-old\n+new\n```\n";
        assert_eq!(
            diff_tags(src),
            vec![
                vec![],
                vec![(0, 7, "diff-meta")],
                vec![(0, 7, "diff-meta")],
                vec![(0, 15, "diff-hunk")],
                vec![],
                vec![(0, 4, "diff-del")],
                vec![(0, 4, "diff-add")],
                vec![],
            ]
        );
        // `patch` too, any case, with more words after it, and tilde fences.
        for open in ["```patch", "```DIFF", "``` diff title=x", "~~~diff"] {
            let close = if open.starts_with('~') { "~~~" } else { "```" };
            let src = format!("{open}\n+a\n-b\n{close}\n");
            let t = diff_tags(&src);
            assert_eq!(t[1], vec![(0, 2, "diff-add")], "{open}");
            assert_eq!(t[2], vec![(0, 2, "diff-del")], "{open}");
            assert!(t[3].is_empty(), "closing fence tagged: {open}");
        }
    }

    /// Nothing outside a diff fence is a diff: another language, an indented
    /// code block, and a list of `-` items are all left alone.
    #[test]
    fn only_a_diff_or_patch_fence_is_coloured_as_a_diff() {
        for src in [
            "```sh\n+a\n-b\n```\n",
            "```\n+a\n-b\n```\n",
            "    +a\n    -b\n",
            "- a\n- b\n",
            "```diffx\n+a\n```\n",
        ] {
            assert!(diff_tags(src).iter().all(Vec::is_empty), "{src:?}");
        }
    }

    /// A removed line whose text starts `--` reads `---` in a diff, just like
    /// a file header. Inside a counted hunk it is a body line; outside one it
    /// is a header only with a `+++` under it — the `/marginal-diff` launcher
    /// lifts every `@@` into a heading, so its fences are bare hunk bodies.
    #[test]
    fn a_removed_line_starting_with_dashes_is_not_a_file_header() {
        let counted = "```diff\n@@ -1,2 +1,1 @@\n--- a SQL comment\n+++ not a header\n```\n";
        let t = diff_tags(counted);
        assert_eq!(t[2], vec![(0, 17, "diff-del")]);
        assert_eq!(t[3], vec![(0, 16, "diff-add")]);
        // The launcher's shape: a hunk body and nothing else.
        let bare = "```diff\n context\n--- gone\n+kept\n```\n";
        let t = diff_tags(bare);
        assert_eq!(t[2], vec![(0, 8, "diff-del")]);
        assert_eq!(t[3], vec![(0, 5, "diff-add")]);
        // Once a hunk's counts run out, a header pair is a header again.
        let two = "```diff\n@@ -1 +1 @@\n-a\n+b\n--- a/y\n+++ b/y\n@@ -3 +3 @@\n-c\n```\n";
        let t = diff_tags(two);
        assert_eq!(t[4], vec![(0, 7, "diff-meta")]);
        assert_eq!(t[5], vec![(0, 7, "diff-meta")]);
        assert_eq!(t[6], vec![(0, 11, "diff-hunk")]);
        assert_eq!(t[7], vec![(0, 2, "diff-del")]);
    }

    /// In a container the tags start where the fence's content does, past the
    /// quote marker or list indentation — the same chrome rule as every other
    /// continuation line.
    #[test]
    fn a_diff_fence_in_a_container_is_tagged_past_its_chrome() {
        let t = diff_tags("> ```diff\n> +a\n> -b\n> ```\n");
        assert_eq!(t[1], vec![(2, 4, "diff-add")]);
        assert_eq!(t[2], vec![(2, 4, "diff-del")]);
        let t = diff_tags("- item\n\n  ```diff\n  +a\n  ```\n");
        assert_eq!(t[3], vec![(2, 4, "diff-add")]);
        // Unclosed: runs to the end of the document, and the last line is body.
        let t = diff_tags("```diff\n+a\n-b\n");
        assert_eq!(t[2], vec![(0, 2, "diff-del")]);
    }

    /// Front matter is metadata: one dim tag over every line of it, delimiters
    /// included, and none of the heading and rule marks it used to get.
    #[test]
    fn front_matter_is_tagged_as_one_block() {
        let src = "---\ntitle: x\n---\n\nbody\n";
        assert_eq!(tags(src, 1), vec![(0, 3, "front-matter")]);
        assert_eq!(tags(src, 2), vec![(0, 8, "front-matter")]);
        assert_eq!(tags(src, 3), vec![(0, 3, "front-matter")]);
    }

    #[test]
    fn empty_input_yields_no_marks() {
        assert!(marks(&parse_tree(""), "").is_empty());
    }
}
