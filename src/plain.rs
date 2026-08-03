//! The fallback backend: a document no format-specific parser owns.
//!
//! One rule — a *paragraph* is a maximal run of consecutive non-blank lines —
//! because that is the only structure every plain-text format agrees on. `.tex`,
//! `.rst`, `.org`, `.txt` and everything else get navigation, selection,
//! commenting and the result JSON out of it. What they do not get is syntax
//! highlighting (there is nothing to tag) or a hierarchy deeper than the three
//! levels below.
//!
//! # What it guarantees that the markdown backend does not
//!
//! `blocks`'s two coverage properties hold here **universally**, not merely over
//! a corpus: the units are a partition of the line vector, so every non-blank
//! line is in exactly one unit and no two units overlap, whatever the input. The
//! four counterexamples in `blocks`'s module doc have no analogue here — there
//! is no construct whose span can come out wrong, because there are no
//! constructs.
//!
//! # The hierarchy
//!
//! `document` > `paragraph` > `text`, one `text` node per line. That is what
//! `+`/`-` widens and narrows along, and — because `text` is an inline kind —
//! what `w`/`b` steps between, so an inline motion here moves to the start of
//! the next line. Word-level motion is deliberately not modelled: a word is not
//! a structure any parser reported, and inventing one would be a second,
//! lexical answer to a question the markdown backend answers from an AST.
//!
//! # Known limitations, all of them inherent to knowing nothing
//!
//! * A heading with no blank line under it joins the paragraph below it. In
//!   LaTeX, `\section{Intro}` on the line above its first sentence is one unit
//!   with that sentence, and there is no way to tell from the bytes alone that
//!   it should not be.
//! * `blocks::questions` skips verbatim spans, and there are none here, so a `?`
//!   inside a `\verb|x?|` or a `.. code-block::` is a stop that a
//!   format-specific backend would have suppressed.

use crate::blocks::{source_lines, Block, Pos, Span, TreeNode};
use crate::highlight::LineMarks;

/// The last column of `line`, as a `Span` end is measured: 1-based, inclusive,
/// a **byte** offset, and never 0 — an empty line still has a column 1 for a
/// span to end on. The same rule `blocks::norm` applies, so a plain span and a
/// markdown span mean the same thing to `App::slice` and to the result JSON.
fn end_col(lines: &[&str], line: usize) -> usize {
    lines
        .get(line.saturating_sub(1))
        .map_or(1, |l| l.len().max(1))
}

/// Maximal runs of consecutive non-blank lines, as 1-based inclusive
/// `(first, last)` pairs.
///
/// "Blank" is whitespace-only, which is the test `blocks`'s coverage property
/// uses to decide which lines a unit must account for. Using a different one
/// here would put a line in no unit and hand the cursor
/// `blocks::block_at`'s upward fallback — a comment filed against a unit the
/// reviewer was not looking at.
fn runs(lines: &[&str]) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let l = i + 1;
        match out.last_mut() {
            Some(last) if last.1 + 1 == l => last.1 = l,
            _ => out.push((l, l)),
        }
    }
    out
}

fn span_of(lines: &[&str], (first, last): (usize, usize)) -> Span {
    Span {
        start: Pos::new(first, 1),
        end: Pos::new(last, end_col(lines, last)),
    }
}

/// The flat list of navigation units: one per paragraph, in order.
pub fn parse(src: &str) -> Vec<Block> {
    let lines = source_lines(src);
    runs(&lines)
        .into_iter()
        .enumerate()
        .map(|(id, run)| Block {
            id,
            kind: "paragraph",
            span: span_of(&lines, run),
            level: 0,
        })
        .collect()
}

/// The whole document as a containment hierarchy.
pub fn parse_tree(src: &str) -> TreeNode {
    let lines = source_lines(src);
    let total = lines.len().max(1);
    TreeNode {
        kind: "document",
        span: Span {
            start: Pos::new(1, 1),
            end: Pos::new(total, end_col(&lines, total)),
        },
        setext: false,
        children: runs(&lines)
            .into_iter()
            .map(|run| TreeNode {
                kind: "paragraph",
                span: span_of(&lines, run),
                setext: false,
                children: (run.0..=run.1)
                    .map(|l| TreeNode {
                        kind: "text",
                        span: span_of(&lines, (l, l)),
                        setext: false,
                        children: Vec::new(),
                    })
                    .collect(),
            })
            .collect(),
    }
}

/// No syntax to highlight — but still one entry per source line, because that
/// is `highlight::marks`'s contract and `ui` indexes it by line number.
pub fn marks(src: &str) -> Vec<LineMarks> {
    vec![LineMarks::new(); source_lines(src).len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::{block_at, containment_stack, next_inline, prev_inline, MIXED_ENDINGS};

    /// A LaTeX fragment, because `.tex` is the format this backend was asked
    /// for. Nothing in it is parsed as LaTeX; that is the point.
    const TEX: &str = "\
\\section{Introduction}

This is a paragraph that runs
across two source lines.

\\begin{itemize}
  \\item first
  \\item second
\\end{itemize}
";

    fn flat(src: &str) -> Vec<(&'static str, usize, usize)> {
        parse(src)
            .into_iter()
            .map(|b| (b.kind, b.start(), b.end()))
            .collect()
    }

    #[test]
    fn a_paragraph_is_a_run_of_non_blank_lines() {
        assert_eq!(
            flat(TEX),
            vec![
                ("paragraph", 1, 1),
                ("paragraph", 3, 4),
                ("paragraph", 6, 9),
            ]
        );
    }

    /// The property `blocks` can only assert over a corpus. Here it is
    /// structural — the units partition the line vector — so the corpus is
    /// evidence rather than the whole claim, and it includes the mixed line
    /// endings that put `source_lines` and `str::lines` out of step.
    #[test]
    fn every_non_blank_line_is_in_exactly_one_unit() {
        for src in [
            TEX,
            "",
            "\n\n\n",
            "one line",
            "no trailing newline",
            "   \nleading blank\n",
            "a\r\rb\n",
            "tabs\t\n \t \nafter a whitespace-only line\n",
        ]
        .into_iter()
        .chain(MIXED_ENDINGS.iter().copied())
        {
            let bs = parse(src);
            for (i, line) in source_lines(src).into_iter().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let n = bs.iter().filter(|b| b.contains_line(i + 1)).count();
                assert_eq!(n, 1, "line {} {line:?} is in {n} units of {src:?}", i + 1);
            }
        }
    }

    #[test]
    fn units_never_overlap_and_stay_ordered() {
        for src in [TEX, "a\nb\n\nc\n"]
            .into_iter()
            .chain(MIXED_ENDINGS.iter().copied())
        {
            for w in parse(src).windows(2) {
                assert!(w[0].span.end < w[1].span.start, "{:?} {:?}", w[0], w[1]);
            }
        }
    }

    /// A span end is the line's **byte** length, so a line of multi-byte
    /// characters ends past its character count — and `App::slice` cuts on that
    /// number. Getting this wrong is a panic on the first umlaut, not a wrong
    /// answer.
    #[test]
    fn a_span_ends_on_the_last_byte_of_its_line_not_the_last_character() {
        let src = "prüfen\n";
        let b = parse(src);
        assert_eq!(b[0].span.end.col, 7, "6 characters, 7 bytes");
        assert_eq!(&src[b[0].span.start.col - 1..b[0].span.end.col], "prüfen");
    }

    #[test]
    fn a_document_with_nothing_in_it_has_no_units_but_still_has_a_tree() {
        for src in ["", "\n", "   \n\t\n"] {
            assert!(parse(src).is_empty(), "{src:?}");
            assert_eq!(block_at(&parse(src), 1), None, "{src:?}");
            let t = parse_tree(src);
            assert_eq!(t.kind, "document");
            assert!(t.children.is_empty(), "{src:?}");
            assert_eq!(t.span.start, Pos::new(1, 1), "{src:?}");
        }
    }

    #[test]
    fn contracting_narrows_from_the_paragraph_to_the_line_under_the_cursor() {
        let t = parse_tree(TEX);
        let kinds: Vec<_> = containment_stack(&t, Pos::new(4, 3))
            .iter()
            .map(|(k, _)| *k)
            .collect();
        assert_eq!(kinds, vec!["text", "paragraph", "document"]);
    }

    /// The same collapse `blocks` does: a one-line paragraph, its text node and
    /// a one-paragraph document all name the same span, so expanding would
    /// appear to do nothing and the outermost label is the honest one.
    #[test]
    fn identical_spans_collapse_so_expansion_always_visibly_moves() {
        let t = parse_tree("just one line\n");
        let kinds: Vec<_> = containment_stack(&t, Pos::new(1, 3))
            .iter()
            .map(|(k, _)| *k)
            .collect();
        assert_eq!(kinds, vec!["document"]);
    }

    #[test]
    fn a_cursor_on_a_blank_line_still_resolves_to_the_document() {
        let t = parse_tree(TEX);
        let stack = containment_stack(&t, Pos::new(2, 1));
        assert_eq!(stack.len(), 1);
        assert_eq!(stack[0].0, "document");
    }

    /// `text` is an inline kind, so `w`/`b` have something to walk. Line starts
    /// are all there is to walk to, and both motions have to agree about that
    /// or one of them strands the cursor.
    #[test]
    fn inline_motions_step_between_line_starts_and_terminate() {
        let t = parse_tree(TEX);
        let mut p = Pos::new(1, 1);
        let mut seen = Vec::new();
        while let Some(next) = next_inline(&t, p) {
            seen.push(next);
            p = next;
        }
        assert_eq!(
            seen,
            vec![
                Pos::new(3, 1),
                Pos::new(4, 1),
                Pos::new(6, 1),
                Pos::new(7, 1),
                Pos::new(8, 1),
                Pos::new(9, 1),
            ],
            "blank lines have no node, so they are stepped over"
        );
        assert_eq!(next_inline(&t, p), None);
        assert_eq!(prev_inline(&t, Pos::new(3, 1)), Some(Pos::new(1, 1)));
        assert_eq!(prev_inline(&t, Pos::new(1, 1)), None);
    }

    #[test]
    fn there_is_nothing_to_highlight_but_one_entry_per_line_all_the_same() {
        let m = marks(TEX);
        assert_eq!(m.len(), source_lines(TEX).len());
        assert!(m.iter().all(Vec::is_empty));
    }
}
