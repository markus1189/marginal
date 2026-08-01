//! Markdown structure extraction.
//!
//! Two views of the same parse, for two different jobs:
//!
//! * [`parse`] — a flat, gapless, non-overlapping list of *navigation units*.
//!   This is what `J`/`K` steps through and what block-wise selection joins.
//! * [`parse_tree`] — the full containment hierarchy, block **and** inline,
//!   which powers expand/contract selection.
//!
//! Positions come from comrak's sourcepos: 1-based line, 1-based **byte**
//! column, end inclusive. Byte, not character — verified against the line
//!
//!     Prüfen `köde`
//!
//! where the backtick is character 8 but comrak reports column 9. Every
//! consumer of `col` therefore has to respect char boundaries.

use comrak::nodes::{AstNode, NodeValue};
use comrak::{parse_document, Arena, Options};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub line: usize,
    /// 1-based byte column.
    pub col: usize,
}

impl Pos {
    pub const fn new(line: usize, col: usize) -> Self {
        Self { line, col }
    }
}

/// An inclusive range of source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: Pos,
    pub end: Pos,
}

impl Span {
    pub fn contains(&self, p: Pos) -> bool {
        p >= self.start && p <= self.end
    }

    pub const fn touches_line(&self, line: usize) -> bool {
        line >= self.start.line && line <= self.end.line
    }

    /// Byte range `[start, end)` of this span within `line`, given that line's
    /// byte length. `None` when the span does not reach the line.
    pub fn byte_range_on(&self, line: usize, line_len: usize) -> Option<(usize, usize)> {
        if !self.touches_line(line) {
            return None;
        }
        let s = if line == self.start.line {
            self.start.col.saturating_sub(1)
        } else {
            0
        };
        let e = if line == self.end.line {
            self.end.col
        } else {
            line_len
        };
        Some((s.min(line_len), e.min(line_len)))
    }

    pub fn union(self, other: Self) -> Self {
        Self {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub id: usize,
    pub kind: &'static str,
    pub span: Span,
    /// Heading level, or list nesting depth. 0 when not applicable.
    pub level: usize,
}

impl Block {
    pub const fn start(&self) -> usize {
        self.span.start.line
    }
    pub const fn end(&self) -> usize {
        self.span.end.line
    }
    pub const fn contains_line(&self, line: usize) -> bool {
        self.span.touches_line(line)
    }
}

/// A node in the containment hierarchy — block level and inline level alike.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeNode {
    pub kind: &'static str,
    pub span: Span,
    pub children: Vec<Self>,
}

/// The parser configuration every view of a document goes through. Shared with
/// `app`'s tests, which re-parse generated feedback markdown and must see it the
/// way the rest of the crate does rather than through a second set of options.
pub fn options() -> Options<'static> {
    let mut o = Options::default();
    o.extension.table = true;
    o.extension.tasklist = true;
    o.extension.strikethrough = true;
    o.extension.autolink = true;
    o.extension.footnotes = true;
    o
}

fn line_len(lines: &[&str], line: usize) -> usize {
    lines.get(line.saturating_sub(1)).map_or(0, |l| l.len())
}

/// comrak reports `line:0` as an end position when a block is terminated by the
/// start of the next line rather than by its own last byte. Pull that back onto
/// the last line the block actually occupies, and never run past the file.
fn norm(sp: comrak::nodes::Sourcepos, lines: &[&str]) -> Span {
    let total = lines.len().max(1);
    let start = Pos::new(sp.start.line.clamp(1, total), sp.start.column.max(1));
    let end = if sp.end.column == 0 && sp.end.line > sp.start.line {
        let l = (sp.end.line - 1).clamp(start.line, total);
        Pos::new(l, line_len(lines, l).max(1))
    } else {
        let l = sp.end.line.clamp(start.line, total);
        Pos::new(l, sp.end.column.max(1).min(line_len(lines, l).max(1)))
    };
    Span {
        start,
        end: end.max(start),
    }
}

const fn kind_of(v: &NodeValue) -> Option<&'static str> {
    Some(match v {
        // block level
        NodeValue::Paragraph => "paragraph",
        NodeValue::Heading(_) => "heading",
        NodeValue::CodeBlock(_) => "code",
        NodeValue::HtmlBlock(_) => "html",
        NodeValue::ThematicBreak => "hr",
        NodeValue::Table(_) => "table",
        NodeValue::TableRow(_) => "table-row",
        NodeValue::TableCell => "table-cell",
        NodeValue::BlockQuote => "blockquote",
        NodeValue::FootnoteDefinition(_) => "footnote",
        NodeValue::Item(_) | NodeValue::TaskItem(_) => "list-item",
        NodeValue::List(_) => "list",
        // inline level
        NodeValue::Code(_) => "code-span",
        NodeValue::Link(_) => "link",
        NodeValue::Image(_) => "image",
        NodeValue::Strong => "strong",
        NodeValue::Emph => "emph",
        NodeValue::Strikethrough => "strike",
        NodeValue::HtmlInline(_) => "html-inline",
        NodeValue::Text(_) => "text",
        _ => return None,
    })
}

const fn is_list(v: &NodeValue) -> bool {
    matches!(v, NodeValue::List(_) | NodeValue::DescriptionList)
}

// ---------------------------------------------------------------------------
// Flat navigation units
// ---------------------------------------------------------------------------

fn walk_flat<'a>(node: &'a AstNode<'a>, lines: &[&str], depth: usize, out: &mut Vec<Block>) {
    for child in node.children() {
        walk_child(child, lines, depth, out);
    }
}

/// One child of a container, as navigation units. Split out of `walk_flat` so a
/// list item can put the blocks that follow its sublist through the same rules,
/// rather than a second, thinner copy of them.
fn walk_child<'a>(child: &'a AstNode<'a>, lines: &[&str], depth: usize, out: &mut Vec<Block>) {
    let value = child.data.borrow().value.clone();
    let span = norm(child.data.borrow().sourcepos, lines);

    // Containers that are navigated *through*, not annotated as a unit. The
    // container itself stays reachable via expand-selection.
    if is_list(&value)
        || matches!(
            value,
            NodeValue::BlockQuote | NodeValue::FootnoteDefinition(_)
        )
    {
        walk_flat(child, lines, depth, out);
        return;
    }

    // A table navigates by row. comrak emits no node for the delimiter row,
    // so stretch each row to just before the next one; that keeps the block
    // list gapless and puts `|---|---|` inside the header row, which is
    // where a reader expects it.
    if matches!(value, NodeValue::Table(_)) {
        let rows: Vec<_> = child.children().collect();
        for (j, row) in rows.iter().enumerate() {
            let mut rs = norm(row.data.borrow().sourcepos, lines);
            if let Some(next) = rows.get(j + 1) {
                let next_line = norm(next.data.borrow().sourcepos, lines).start.line;
                if next_line > rs.end.line + 1 {
                    let l = next_line - 1;
                    rs.end = Pos::new(l, line_len(lines, l).max(1));
                }
            } else if span.end.line > rs.end.line {
                // No next row to stretch towards. A header-only table still has
                // a delimiter row under it, and it is inside the table's own
                // span even though comrak gives it no node of its own — so the
                // last row takes it, exactly as any other row would.
                rs.end = Pos::new(span.end.line, line_len(lines, span.end.line).max(1));
            }
            push(out, "table-row", rs, 0);
        }
        return;
    }

    let Some(kind) = kind_of(&value) else {
        return;
    };

    if kind == "list-item" {
        // The item's range spans its nested sublist; trim it so item and
        // sublist never overlap, then walk the children in order. Descending
        // into sublists *only* used to drop everything after one: a follow-up
        // paragraph, a code fence or a second table inside the same item is
        // neither within the trimmed span nor reached by the recursion, so it
        // belonged to no unit at all. `block_at` then resolved a cursor on
        // those lines to the last unit above it -- the sublist's final leaf --
        // and the annotation was written against that block's lines and text.
        let nested = child
            .children()
            .find(|c| is_list(&c.data.borrow().value))
            .map(|c| norm(c.data.borrow().sourcepos, lines).start.line);

        let mut s = span;
        if let Some(ns) = nested {
            let l = ns.saturating_sub(1).max(s.start.line);
            s.end = Pos::new(l, line_len(lines, l).max(1));
        }
        push(out, kind, s, depth);
        for grandchild in child.children() {
            if is_list(&grandchild.data.borrow().value) {
                walk_flat(grandchild, lines, depth + 1, out);
            } else if norm(grandchild.data.borrow().sourcepos, lines).start.line > s.end.line {
                // Content the trimmed span no longer covers.
                walk_child(grandchild, lines, depth, out);
            }
        }
        return;
    }

    let level = match &value {
        NodeValue::Heading(h) => h.level as usize,
        _ => 0,
    };
    push(out, kind, span, level);
}

fn push(out: &mut Vec<Block>, kind: &'static str, span: Span, level: usize) {
    out.push(Block {
        id: out.len(),
        kind,
        span,
        level,
    });
}

/// The flat list of navigation units, ordered by position, ids renumbered.
pub fn parse(src: &str) -> Vec<Block> {
    let lines: Vec<&str> = src.lines().collect();
    let arena = Arena::new();
    let root = parse_document(&arena, src, &options());

    let mut out = Vec::new();
    walk_flat(root, &lines, 0, &mut out);
    out.sort_by_key(|b| (b.span.start, b.span.end));
    for (i, b) in out.iter_mut().enumerate() {
        b.id = i;
    }
    out
}

/// Index of the block containing `line`, else the nearest one above it, so the
/// cursor never falls into a hole between blocks.
pub fn block_at(blocks: &[Block], line: usize) -> Option<usize> {
    if blocks.is_empty() {
        return None;
    }
    if let Some(i) = blocks.iter().position(|b| b.contains_line(line)) {
        return Some(i);
    }
    blocks
        .iter()
        .enumerate()
        .filter(|(_, b)| b.end() < line)
        .map(|(i, _)| i)
        .next_back()
        .or(Some(0))
}

// ---------------------------------------------------------------------------
// Containment hierarchy
// ---------------------------------------------------------------------------

fn walk_tree<'a>(node: &'a AstNode<'a>, lines: &[&str]) -> Vec<TreeNode> {
    let mut out = Vec::new();
    for child in node.children() {
        let value = child.data.borrow().value.clone();
        let span = norm(child.data.borrow().sourcepos, lines);
        let kids = walk_tree(child, lines);
        match kind_of(&value) {
            Some(kind) => out.push(TreeNode {
                kind,
                span,
                children: kids,
            }),
            // Soft/hard breaks and similar: splice their children in rather
            // than inventing a level of hierarchy nobody asked for.
            None => out.extend(kids),
        }
    }
    out
}

/// The whole document as a containment hierarchy.
pub fn parse_tree(src: &str) -> TreeNode {
    let lines: Vec<&str> = src.lines().collect();
    let total = lines.len().max(1);
    let arena = Arena::new();
    let root = parse_document(&arena, src, &options());
    TreeNode {
        kind: "document",
        span: Span {
            start: Pos::new(1, 1),
            end: Pos::new(total, line_len(&lines, total).max(1)),
        },
        children: walk_tree(root, &lines),
    }
}

/// Every node containing `pos`, innermost first, ending at the document.
/// Consecutive nodes with identical spans are collapsed — expanding from a
/// paragraph to its only text run would otherwise look like nothing happened.
pub fn containment_stack(root: &TreeNode, pos: Pos) -> Vec<(&'static str, Span)> {
    fn go(n: &TreeNode, pos: Pos, out: &mut Vec<(&'static str, Span)>) {
        if !n.span.contains(pos) {
            return;
        }
        for c in &n.children {
            go(c, pos, out);
        }
        out.push((n.kind, n.span));
    }
    let mut raw = Vec::new();
    go(root, pos, &mut raw);

    // Collapse runs of identical spans, keeping the *outermost* label: when a
    // lone paragraph spans the whole document, that span is a document, not a
    // text run. Without this, expanding would appear to do nothing.
    let mut out: Vec<(&'static str, Span)> = Vec::with_capacity(raw.len());
    for entry in raw {
        match out.last_mut() {
            Some(last) if last.1 == entry.1 => *last = entry,
            _ => out.push(entry),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Inline motions
// ---------------------------------------------------------------------------

/// Inline node kinds — the things `+`/`-` can narrow onto, and therefore the
/// things worth stepping between.
fn is_inline(kind: &str) -> bool {
    matches!(
        kind,
        "code-span" | "link" | "image" | "strong" | "emph" | "strike" | "html-inline" | "text"
    )
}

/// Start of the next inline node strictly after `from`, in document order.
///
/// This is how a column past the right edge of the pane is reached without a
/// viewport offset: the interesting columns on a long line are exactly the
/// inline node starts, and there are a handful of them, not two hundred.
pub fn next_inline(root: &TreeNode, from: Pos) -> Option<Pos> {
    fold_inline(root, |p| p > from, Pos::min)
}

/// Start of the previous inline node strictly before `from`.
pub fn prev_inline(root: &TreeNode, from: Pos) -> Option<Pos> {
    fold_inline(root, |p| p < from, Pos::max)
}

fn fold_inline(
    root: &TreeNode,
    keep: impl Fn(Pos) -> bool,
    pick: impl Fn(Pos, Pos) -> Pos,
) -> Option<Pos> {
    fn go(
        n: &TreeNode,
        keep: &impl Fn(Pos) -> bool,
        pick: &impl Fn(Pos, Pos) -> Pos,
        best: &mut Option<Pos>,
    ) {
        if is_inline(n.kind) && keep(n.span.start) {
            *best = Some(best.map_or(n.span.start, |b| pick(b, n.span.start)));
        }
        for c in &n.children {
            go(c, keep, pick, best);
        }
    }
    let mut best = None;
    go(root, &keep, &pick, &mut best);
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "\
# Heading

Para one
still para.

- item a
- item b
  - nested

```go
# not a heading
```

| a | b |
|---|---|
| 1 | 2 |
";

    fn flat(src: &str) -> Vec<(&'static str, usize, usize)> {
        parse(src)
            .into_iter()
            .map(|b| (b.kind, b.start(), b.end()))
            .collect()
    }

    #[test]
    fn extracts_expected_navigation_units() {
        assert_eq!(
            flat(DOC),
            vec![
                ("heading", 1, 1),
                ("paragraph", 3, 4),
                ("list-item", 6, 6),
                ("list-item", 7, 7),
                ("list-item", 8, 8),
                ("code", 10, 12),
                ("table-row", 14, 15),
                ("table-row", 16, 16),
            ]
        );
    }

    #[test]
    fn tier1_table_navigates_by_row_not_as_one_lump() {
        let rows: Vec<_> = parse(DOC)
            .into_iter()
            .filter(|b| b.kind == "table-row")
            .collect();
        assert_eq!(rows.len(), 2);
        // the delimiter row has no node of its own; it rides with the header
        assert_eq!((rows[0].start(), rows[0].end()), (14, 15));
        assert_eq!((rows[1].start(), rows[1].end()), (16, 16));
        assert!(!parse(DOC).iter().any(|b| b.kind == "table"));
    }

    /// The delimiter row rode with the header only when a *next* row existed to
    /// stretch towards. A GFM table with a header and no body rows therefore
    /// left `|---|---|` in no unit: the cursor there reported the header's
    /// span, so a block selection silently omitted the line, and `extents`
    /// reported a one-line table, which `align` refuses — so it never aligned.
    #[test]
    fn a_header_only_table_still_owns_its_delimiter_row() {
        let src = "| name | description |\n|---|---|\n\n# H\n";
        assert_eq!(
            flat(src),
            vec![("table-row", 1, 2), ("heading", 4, 4)],
            "delimiter row left uncovered"
        );
    }

    #[test]
    fn tier1_blockquote_navigates_by_inner_block() {
        let src = "> First para.\n>\n> Second para.\n";
        assert_eq!(flat(src), vec![("paragraph", 1, 1), ("paragraph", 3, 3)]);
        assert!(!parse(src).iter().any(|b| b.kind == "blockquote"));
    }

    #[test]
    fn navigation_units_never_overlap() {
        for src in [
            DOC,
            "> quote\n\n| a |\n|---|\n| 1 |\n",
            "- a\n  - b\n    - c\n",
        ] {
            let bs = parse(src);
            for w in bs.windows(2) {
                assert!(
                    w[0].span.end < w[1].span.start,
                    "overlap: {:?} then {:?}",
                    w[0],
                    w[1]
                );
            }
        }
    }

    /// Non-overlap was asserted; gapless never was, and the fixtures above
    /// could not have caught it — none has a block *after* a nested sublist,
    /// which is the one shape `walk_flat` dropped. An uncovered line is worse
    /// than unreachable: `block_at` falls back to the last unit above it, so a
    /// comment typed there is filed against a different block's lines and text.
    #[test]
    fn navigation_units_cover_every_non_blank_line_exactly_once() {
        for src in [
            DOC,
            "> quote\n\n| a |\n|---|\n| 1 |\n",
            "- a\n  - b\n    - c\n",
            "1. Bump the version\n   - Cargo.toml\n   - flake.nix\n\n   Remember the lock file.\n\n2. Tag and push\n",
            "- item\n\n  - sub\n\n  ```\n  code\n  ```\n",
            "- item\n  - sub\n\n  tail para\n\n  | a | b |\n  |---|---|\n  | 1 | 2 |\n",
        ] {
            let bs = parse(src);
            for (i, line) in src.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let n = bs.iter().filter(|b| b.contains_line(i + 1)).count();
                assert_eq!(n, 1, "line {} {line:?} is in {n} units of {src:?}", i + 1);
            }
        }
    }

    #[test]
    fn fence_contents_are_not_headings() {
        assert!(!parse(DOC)
            .iter()
            .any(|b| b.kind == "heading" && b.start() == 11));
    }

    #[test]
    fn code_block_includes_both_fences() {
        let c = parse(DOC).into_iter().find(|b| b.kind == "code").unwrap();
        assert_eq!((c.start(), c.end()), (10, 12));
    }

    #[test]
    fn nested_item_does_not_overlap_its_parent() {
        let items: Vec<_> = parse(DOC)
            .into_iter()
            .filter(|b| b.kind == "list-item")
            .collect();
        assert_eq!(items.len(), 3);
        assert_eq!((items[1].start(), items[1].end()), (7, 7));
        assert_eq!((items[2].start(), items[2].end()), (8, 8));
    }

    #[test]
    fn task_items_are_list_items() {
        let src = "- [ ] Add validation\n- [x] Write tests\n";
        assert_eq!(flat(src), vec![("list-item", 1, 1), ("list-item", 2, 2)]);
    }

    #[test]
    fn blocks_never_run_past_end_of_file() {
        let b = parse("para with no trailing newline");
        assert_eq!(b.len(), 1);
        assert_eq!((b[0].start(), b[0].end()), (1, 1));
    }

    #[test]
    fn block_at_finds_container_and_falls_back_upward() {
        let b = parse(DOC);
        assert_eq!(b[block_at(&b, 1).unwrap()].kind, "heading");
        assert_eq!(b[block_at(&b, 11).unwrap()].kind, "code");
        assert_eq!(b[block_at(&b, 5).unwrap()].kind, "paragraph");
    }

    #[test]
    fn empty_document_has_no_blocks() {
        assert!(parse("").is_empty());
        assert_eq!(block_at(&[], 1), None);
    }

    #[test]
    fn crlf_line_endings_do_not_shift_ranges() {
        assert_eq!(
            flat("# H\r\n\r\npara\r\n"),
            vec![("heading", 1, 1), ("paragraph", 3, 3)]
        );
    }

    // ---- tier 4: hierarchy -------------------------------------------------

    // Two paragraphs, so the document strictly contains the first one. With a
    // single paragraph the two spans are identical and collapse into one entry.
    const INLINE: &str =
        "Use `parse_document` and the [comrak docs](https://docs.rs) now.\n\nSecond paragraph.\n";

    #[test]
    fn tree_reaches_inline_nodes_with_columns() {
        let t = parse_tree(INLINE);
        let stack = containment_stack(&t, Pos::new(1, 32)); // inside "comrak docs"
        let kinds: Vec<_> = stack.iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds, vec!["text", "link", "paragraph", "document"]);
    }

    #[test]
    fn expanding_from_a_link_label_widens_to_the_whole_link() {
        let t = parse_tree(INLINE);
        let stack = containment_stack(&t, Pos::new(1, 32));
        let (_, label) = stack[0];
        let (_, link) = stack[1];
        // the label excludes the brackets; the link includes them and the url
        assert!(link.start < label.start && link.end > label.end);
        assert_eq!(link.start.col, 30);
    }

    #[test]
    fn code_span_is_reachable_and_includes_its_backticks() {
        let t = parse_tree(INLINE);
        let stack = containment_stack(&t, Pos::new(1, 8));
        let (kind, span) = stack.iter().find(|(k, _)| *k == "code-span").unwrap();
        assert_eq!(*kind, "code-span");
        assert_eq!(span.start.col, 5);
        assert_eq!(
            &INLINE[span.start.col - 1..span.end.col],
            "`parse_document`"
        );
    }

    #[test]
    fn identical_spans_collapse_so_expansion_always_visibly_moves() {
        // A lone paragraph spans exactly what its text run and the document
        // span, so all three collapse — and the label kept is the outermost.
        let t = parse_tree("Just one paragraph.\n");
        let stack = containment_stack(&t, Pos::new(1, 3));
        let kinds: Vec<_> = stack.iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds, vec!["document"]);
    }

    #[test]
    fn columns_are_byte_offsets_not_character_offsets() {
        // The backtick is character 8 but byte 9. If this ever flips, every
        // slice in the app is off by one on non-ascii input.
        let src = "Prüfen `köde` hier.\n";
        let t = parse_tree(src);
        let stack = containment_stack(&t, Pos::new(1, 10));
        let (_, span) = stack.iter().find(|(k, _)| *k == "code-span").unwrap();
        assert_eq!(span.start.col, 9);
        assert_eq!(&src[span.start.col - 1..span.end.col], "`köde`");
    }

    #[test]
    fn table_cells_are_reachable_through_the_hierarchy() {
        let t = parse_tree(DOC);
        let kinds: Vec<_> = containment_stack(&t, Pos::new(14, 3))
            .iter()
            .map(|(k, _)| *k)
            .collect();
        assert!(kinds.contains(&"table-cell"));
        assert!(kinds.contains(&"table-row"));
        assert!(kinds.contains(&"table"));
    }

    #[test]
    fn stack_is_strictly_widening_and_ends_at_the_document() {
        let t = parse_tree(DOC);
        let stack = containment_stack(&t, Pos::new(3, 2));
        assert_eq!(stack.last().unwrap().0, "document");
        for w in stack.windows(2) {
            assert!(w[1].1.start <= w[0].1.start && w[1].1.end >= w[0].1.end);
            assert_ne!(w[0].1, w[1].1, "identical spans should have been collapsed");
        }
    }

    #[test]
    fn stack_outside_any_content_still_yields_the_document() {
        let t = parse_tree(DOC);
        let stack = containment_stack(&t, Pos::new(2, 1)); // the blank line
        assert_eq!(stack.len(), 1);
        assert_eq!(stack[0].0, "document");
    }

    #[test]
    fn byte_range_on_line_clips_to_the_line() {
        let s = Span {
            start: Pos::new(2, 3),
            end: Pos::new(4, 5),
        };
        assert_eq!(s.byte_range_on(1, 10), None);
        assert_eq!(s.byte_range_on(2, 10), Some((2, 10)));
        assert_eq!(s.byte_range_on(3, 10), Some((0, 10)));
        assert_eq!(s.byte_range_on(4, 10), Some((0, 5)));
        assert_eq!(s.byte_range_on(4, 3), Some((0, 3)));
        assert_eq!(s.byte_range_on(5, 10), None);
    }

    // ---- tier 5: inline motions --------------------------------------------

    #[test]
    fn inline_motions_step_through_the_nodes_of_a_line() {
        let t = parse_tree(INLINE);
        // "Use " text, `parse_document`, " and the " text, the link, its label…
        let mut p = Pos::new(1, 1);
        let mut cols = Vec::new();
        while let Some(next) = next_inline(&t, p) {
            cols.push((next.line, next.col));
            p = next;
        }
        assert!(cols.contains(&(1, 5)), "the code span: {cols:?}");
        assert!(cols.contains(&(1, 30)), "the link: {cols:?}");
        // and it walks off the end of the line into the next paragraph
        assert!(cols.iter().any(|(l, _)| *l == 3), "{cols:?}");
    }

    #[test]
    fn inline_motions_are_inverses_and_terminate() {
        let t = parse_tree(INLINE);
        let a = next_inline(&t, Pos::new(1, 1)).unwrap();
        let b = next_inline(&t, a).unwrap();
        assert_eq!(prev_inline(&t, b), Some(a));
        // nothing before the first node, nothing after the last
        assert_eq!(prev_inline(&t, Pos::new(1, 1)), None);
        let mut last = Pos::new(1, 1);
        while let Some(n) = next_inline(&t, last) {
            last = n;
        }
        assert_eq!(next_inline(&t, last), None);
    }

    #[test]
    fn inline_motions_reach_a_node_far_past_the_right_edge_of_any_pane() {
        // The case horizontal scrolling exists to serve: a link at column 200+.
        let filler = "word ".repeat(45);
        let src = format!("{filler}[label](https://example.dev) tail.\n");
        let t = parse_tree(&src);
        let link = next_inline(&t, Pos::new(1, 2)).unwrap();
        assert!(link.col > 200, "expected a far column, got {}", link.col);
        let stack = containment_stack(&t, link);
        assert!(
            stack.iter().any(|(k, _)| *k == "link"),
            "cursor should land inside the link: {stack:?}"
        );
    }

    #[test]
    fn inline_motions_on_a_file_without_inline_content_do_nothing() {
        let t = parse_tree("---\n");
        assert_eq!(next_inline(&t, Pos::new(1, 1)), None);
        assert_eq!(prev_inline(&t, Pos::new(9, 9)), None);
    }
}
