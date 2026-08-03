//! Markdown structure extraction.
//!
//! Two views of the same parse, for two different jobs:
//!
//! * [`parse`] — a flat list of *navigation units*, ordered by position. This
//!   is what `J`/`K` steps through and what block-wise selection joins.
//! * [`parse_tree`] — the full containment hierarchy, block **and** inline,
//!   which powers expand/contract selection.
//!
//! # What the flat list guarantees, and what it does not
//!
//! Flat and ordered, always. Gapless and non-overlapping is the *goal* and the
//! common case, and two tests pin it —
//! `navigation_units_never_overlap` and
//! `navigation_units_cover_every_non_blank_line_exactly_once` — but they pin it
//! **over their fixtures**, not universally, and it is not universally true.
//! Known counterexamples, all reproduced with `--dump-blocks` and all left
//! standing here on purpose (each is its own fix, with its own test):
//!
//! * **Link reference definitions.** comrak builds no AST node for them, so
//!   `[a]: http://example.com` on its own line is in no unit. `# T` / `See [a].`
//!   / `[a]: …` / `Another.` yields units for L1, L3 and L7 — L5 is uncovered.
//! * **Indented code in a list item, after a sublist.** comrak's sourcepos for
//!   the item is short, and the guard in `walk_child` compares only
//!   `start.line`: a code block that *starts* on the item's last covered line
//!   but runs past it is neither inside the trimmed span nor walked. In
//!   `- item one` / `  - sub` / two lines of eight-space code / `- item two`,
//!   the second code line is uncovered.
//! * **Unreferenced footnote definitions.** `footnotes` is on in [`options`],
//!   and a definition nothing links to does not survive into the tree at all —
//!   so a document that is nothing but `[^fn]: …` parses to **zero** units, and
//!   one sitting between two paragraphs leaves its own line uncovered. A
//!   *referenced* definition's inner blocks come through normally.
//! * **A sublist opening on its parent's own line.** `- - a` gives two
//!   `list-item` units both spanning `L1-L1`: the trim in `walk_child` can only
//!   cut the parent back to `nested_start - 1`, and here that is the parent's
//!   own start line, so the two **overlap**.
//!
//! What the callers actually rely on is weaker and does hold: the list is flat,
//! ordered, and [`block_at`] never returns `None` for a non-empty list — a
//! cursor in a hole resolves to the nearest unit above it. That fallback is
//! why an uncovered line is quiet rather than fatal, and also why it is worth
//! knowing about: a comment typed on one is filed against a *different* unit's
//! lines and text.
//!
//! Positions come from comrak's sourcepos: 1-based line, 1-based **byte**
//! column, end inclusive. Byte, not character — verified against the line
//!
//!     Prüfen `köde`
//!
//! where the backtick is character 8 but comrak reports column 9. Every
//! consumer of `col` therefore has to respect char boundaries.
//!
//! They are not all *source* columns as comrak hands them over, though: inside
//! a table the extension rewrites the text before the inline parser sees it,
//! and the columns it then reports index the rewrite. [`PipeShift`] maps them
//! back, once per parse, so everything downstream of [`parse_tree`] — slices,
//! marks, the containment tree — indexes the file.

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
    /// True only for a setext heading — one underlined with `===` or `---`
    /// rather than opened by a `#` run, so its first line is ordinary text with
    /// no marker on it. `kind` cannot carry this: `kind_of` also labels the
    /// navigation units, and those labels reach `--dump-blocks`, the annotation
    /// JSON and the status line.
    pub setext: bool,
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

/// The document split into lines the way markdown ends one — and therefore
/// the way comrak numbers `sourcepos`. **Every vector that a `sourcepos.line`
/// indexes has to come from here**, or the two disagree about which line is
/// which.
///
/// `str::lines` is not that split. It breaks on `\n` alone, discarding a `\r`
/// only when one immediately precedes the newline. The spec ends a line on a
/// line feed, a carriage return, *or* a carriage-return/line-feed pair, so a
/// document holding a lone `\r` — one that is not part of a `\r\n` — has one
/// more line than `str::lines` reports, and every line after it shifts up by
/// one per such byte. Verified against comrak 0.54: it puts the paragraph of
/// `"# H\rpara\r"` at line 2, where `str::lines` yields a single line.
///
/// The terminator belongs to no line, so column 1 is the line's first byte in
/// both views and a byte column keeps meaning the same thing. It also means no
/// line yielded here can contain a `\r`, which is what lets everything
/// downstream keep splitting its own `\n`-joined text on `\n`.
pub fn source_lines(src: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = src;
    while !rest.is_empty() {
        let Some(i) = rest.find(['\r', '\n']) else {
            out.push(rest);
            break;
        };
        out.push(&rest[..i]);
        let term = if rest[i..].starts_with("\r\n") { 2 } else { 1 };
        rest = &rest[i + term..];
    }
    out
}

/// Documents whose line endings disagree with each other, and one real shape
/// per fixture: a heading, a table, a lazy blockquote continuation, both kinds
/// of code block, a nested list with a tail paragraph, and a multi-byte line.
///
/// Here rather than in the test module below because `app` sweeps the same
/// corpus through `App`, which is where a line vector out of step with comrak
/// actually reaches the deliverable.
#[cfg(test)]
pub const MIXED_ENDINGS: &[&str] = &[
    "# H\rpara\r",
    "intro\rrest\n\n# Heading\n\n- item one\n- item two\n",
    "| a | b |\r|---|---|\r| 1 | 2 |\n\npara\n",
    "> quote line\r   lazy continuation\n\n- item\r\n- second\r",
    "```\ncode a\rcode b\n```\n\nafter the fence\n",
    "    indented\rnot indented any more\n",
    "- one\r  - nested\r\n\n  tail para\n",
    "prüfen `köde`\rzweite Zeile mit `Umlaut ö`\n",
    "text\r\r\nblank line between\n",
];

/// What `source_lines` must produce, computed the other way round: rewrite
/// every ending to `\n` first, then split on that. Deliberately a second
/// implementation and not a second call, so a test comparing the two cannot
/// inherit a mistake from the one it is checking.
#[cfg(test)]
pub fn normalised_lines(src: &str) -> Vec<String> {
    if src.is_empty() {
        return Vec::new();
    }
    let flat = src.replace("\r\n", "\n").replace('\r', "\n");
    flat.strip_suffix('\n')
        .unwrap_or(&flat)
        .split('\n')
        .map(ToString::to_string)
        .collect()
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

// ---------------------------------------------------------------------------
// The escaped-pipe column shift
// ---------------------------------------------------------------------------

/// One run of text comrak's table extension unescaped before it parsed the
/// inlines inside it, and the source columns of the backslashes that cost it.
#[derive(Debug)]
struct Unescaped {
    line: usize,
    /// 1-based byte column of the run's first byte, in the source.
    start: usize,
    /// 1-based byte column of its last byte, in the source.
    end: usize,
    /// Source columns of the dropped backslashes, ascending. Never empty.
    dropped: Vec<usize>,
}

/// The map from the inline columns comrak reports back to source columns, for
/// the runs its table extension unescapes. Empty for any document without a
/// table, which is why it is built once per parse and consulted per node.
///
/// # What comrak does
///
/// `\|` is how GFM puts a literal pipe in a table cell, and comrak's
/// `parser::table::unescape_pipes` takes the backslash out of the text
/// **before** that text is handed to the inline parser. Every position the
/// inline parser then reports is measured against the shortened string, while
/// the cell's own `sourcepos` — and the row's, and the table's — stays in
/// source coordinates. So an inline column inside such a run is short by one
/// byte per `\|` dropped ahead of it, and `App::slice` quotes the wrong bytes:
/// on
///
/// ```text
/// | Meldung (`"ERROR" \| "TIMEOUT"`) per E-Mail |
/// ```
///
/// the text run comrak parsed as `) per E-Mail` is reported at columns 33..44,
/// which slices to `` `) per E-Mai `` — one byte left of the truth at both
/// ends, so the selection opens on the code span's closing backtick and stops
/// a letter short. The code span above it loses that backtick for the same
/// reason.
///
/// # Ground truth, taken from comrak 0.54 one case at a time
///
/// * The shift is **per run, not per line**: it starts over in each cell, so a
///   `\|` in an earlier cell of the same row shifts nothing in a later one.
///   Within a cell it accumulates, one byte per escape.
/// * Only the **inline** columns move. `table`, `table-row` and `table-cell`
///   spans are all built from source offsets and stay correct, which is what
///   lets a cell's own span anchor the correction.
/// * `\|` inside a code span shifts exactly as it does in plain text — the
///   unescaping happens before anything knows what a code span is.
/// * `\\|` — an escaped backslash before a pipe — drops nothing, matching
///   `unescape_pipes`'s own state machine, which clears its flag on the second
///   backslash. Neither does a `\|` that comrak's cell scanner never saw,
///   because the row was not a table.
/// * No other escape shifts anything. `\*`, `\_`, `\\` and `\[` in a cell all
///   report source columns, because the inline parser handles those itself and
///   tracks its own position while doing it. `\|` is the odd one out precisely
///   because it is resolved by the *block* parser, behind the inline parser's
///   back. The same `\|` in a paragraph, a heading or a list item — anywhere
///   outside a table — shifts nothing either.
/// * A **paragraph absorbed above a table's header row** goes through the same
///   `unescape_pipes` call (in `try_inserting_table_header_paragraph`) and its
///   inlines shift the same way, even though it is not a table row and holds
///   no cells. It is a sibling case in the literal sense: same function, same
///   corruption, a different node kind. The run there is a whole line, and the
///   shift does not cross a line break because a newline clears the escape.
#[derive(Debug)]
struct PipeShift {
    /// Sorted by `(line, start)`, and never overlapping: table cells are
    /// disjoint by construction and a preface line is never a row.
    runs: Vec<Unescaped>,
}

impl PipeShift {
    /// The source column that a column comrak reported on `line` names.
    ///
    /// Identity outside an unescaped run, and identity for the `column: 0`
    /// end-of-block sentinel `norm` still has to recognise.
    fn source_column(&self, line: usize, col: usize) -> usize {
        let upto = self
            .runs
            .partition_point(|r| (r.line, r.start) <= (line, col));
        let Some(run) = self.runs[..upto]
            .last()
            .filter(|r| r.line == line && col <= r.end)
        else {
            return col;
        };
        // Each dropped backslash at or before the position built so far is a
        // byte the inline parser never counted, so the answer moves right past
        // it -- and past whatever that uncovers.
        let mut out = col;
        for &d in &run.dropped {
            if d > out {
                break;
            }
            out += 1;
        }
        out
    }
}

/// The backslashes `unescape_pipes` drops from `line`, as 1-based source byte
/// columns. A transcription of comrak's own loop: the flag is set only by a
/// backslash that no pending backslash precedes, so `\\|` keeps both.
///
/// Scanning the whole line rather than each run is sound because a `|` with a
/// backslash in front of it is never a cell delimiter — comrak's cell scanner
/// treats it as escaped whether or not `unescape_pipes` agrees — so the flag is
/// always clear where one run ends and the next begins.
fn dropped_backslashes(line: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut after_backslash = false;
    for (i, c) in line.char_indices() {
        if after_backslash {
            if c == '|' {
                // The backslash is the byte before, so `i` is its 1-based column.
                out.push(i);
            }
            after_backslash = false;
        } else if c == '\\' {
            after_backslash = true;
        }
    }
    out
}

/// True for a paragraph comrak split off the top of a table's own container —
/// the `preface` of `try_inserting_table_header_paragraph`, which is unescaped
/// with the cells. A table cannot otherwise interrupt a paragraph, so a table
/// starting on the line after a paragraph ends *is* that split.
fn is_table_preface<'a>(para: &'a AstNode<'a>) -> bool {
    let end = para.data.borrow().sourcepos.end.line;
    para.next_sibling().is_some_and(|next| {
        let d = next.data.borrow();
        matches!(d.value, NodeValue::Table(_)) && d.sourcepos.start.line == end + 1
    })
}

/// Source ranges of every run comrak unescaped, as `(line, start, end)` in
/// 1-based byte columns.
fn unescaped_runs<'a>(node: &'a AstNode<'a>, lines: &[&str], out: &mut Vec<(usize, usize, usize)>) {
    for child in node.children() {
        {
            let data = child.data.borrow();
            let sp = data.sourcepos;
            match &data.value {
                // A cell is one line by construction; the guard is here so a
                // future comrak that changes that degrades to no correction
                // rather than to a wrong one.
                NodeValue::TableCell if sp.start.line == sp.end.line => {
                    out.push((sp.start.line, sp.start.column, sp.end.column));
                }
                NodeValue::Paragraph if is_table_preface(child) => {
                    for l in sp.start.line..=sp.end.line {
                        out.push((l, 1, line_len(lines, l)));
                    }
                }
                _ => {}
            }
        }
        unescaped_runs(child, lines, out);
    }
}

fn pipe_shift<'a>(root: &'a AstNode<'a>, lines: &[&str]) -> PipeShift {
    let mut ranges = Vec::new();
    unescaped_runs(root, lines, &mut ranges);
    ranges.sort_unstable();

    let mut runs: Vec<Unescaped> = Vec::new();
    let mut scanned: Option<(usize, Vec<usize>)> = None;
    for (line, start, end) in ranges {
        let Some(text) = line.checked_sub(1).and_then(|i| lines.get(i)) else {
            continue;
        };
        if !matches!(&scanned, Some((l, _)) if *l == line) {
            scanned = Some((line, dropped_backslashes(text)));
        }
        let Some((_, all)) = &scanned else { continue };
        let dropped: Vec<usize> = all
            .iter()
            .copied()
            .filter(|c| *c >= start && *c <= end)
            .collect();
        if !dropped.is_empty() {
            runs.push(Unescaped {
                line,
                start,
                end,
                dropped,
            });
        }
    }
    PipeShift { runs }
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
    let lines = source_lines(src);
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

fn walk_tree<'a>(node: &'a AstNode<'a>, lines: &[&str], shift: &PipeShift) -> Vec<TreeNode> {
    let mut out = Vec::new();
    for child in node.children() {
        let value = child.data.borrow().value.clone();
        let kind = kind_of(&value);
        let mut sp = child.data.borrow().sourcepos;
        // Only the inline columns are measured against unescaped text; the
        // block spans a run is anchored to would be broken by the same move.
        if kind.is_some_and(is_inline) {
            sp.start.column = shift.source_column(sp.start.line, sp.start.column);
            sp.end.column = shift.source_column(sp.end.line, sp.end.column);
        }
        let span = norm(sp, lines);
        let kids = walk_tree(child, lines, shift);
        match kind {
            Some(kind) => out.push(TreeNode {
                kind,
                span,
                setext: matches!(&value, NodeValue::Heading(h) if h.setext),
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
    let lines = source_lines(src);
    let total = lines.len().max(1);
    let arena = Arena::new();
    let root = parse_document(&arena, src, &options());
    let shift = pipe_shift(root, &lines);
    TreeNode {
        kind: "document",
        span: Span {
            start: Pos::new(1, 1),
            end: Pos::new(total, line_len(&lines, total).max(1)),
        },
        setext: false,
        children: walk_tree(root, &lines, &shift),
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

// ---------------------------------------------------------------------------
// Questions
// ---------------------------------------------------------------------------

/// Kinds whose interior is not prose. A `?` inside one is punctuation in some
/// other language — a glob, a regex, a ternary, a query string — and stopping
/// on it is how a jump key loses the reader's trust.
fn is_verbatim(kind: &str) -> bool {
    matches!(kind, "code" | "code-span" | "html" | "html-inline")
}

fn verbatim_spans(n: &TreeNode, out: &mut Vec<Span>) {
    if is_verbatim(n.kind) {
        out.push(n.span);
        return;
    }
    for c in &n.children {
        verbatim_spans(c, out);
    }
}

/// Characters allowed between a `?` and the end of the sentence. `Is it?)` and
/// `*Really?*` are questions; the run has to end at whitespace all the same.
const CLOSERS: [char; 12] = [')', ']', '}', '"', '\'', '»', '”', '’', '*', '_', '~', '`'];

/// Every `?` that ends a sentence, in document order.
///
/// The rule is `?` followed by whitespace or end-of-line, optionally through a
/// run of closing punctuation — deliberately *not* `?\b`, which matches a `?`
/// followed by a word character and so selects `example.com?q=1`, the exact
/// case worth excluding. Verbatim spans are skipped outright.
///
/// Takes the already-parsed tree rather than re-parsing, as `highlight::marks`
/// does: one comrak pass per document, no second parser to disagree with the
/// first.
///
/// The line vector comes from [`source_lines`], like every other one addressed
/// by a `sourcepos` line number. `str::lines` was the obvious thing to reach for
/// and is the wrong split: it ends a line on `\n` alone, so a document holding a
/// bare `\r` yields fewer lines than comrak counted and everything below that
/// byte is numbered one short per `\r` — while the `\r` stays *inside* its line,
/// pushing every column after it right as well. Both axes wrong at once, and the
/// resulting position can name a column past the end of the line it names.
pub fn questions(root: &TreeNode, src: &str) -> Vec<Pos> {
    let mut skip = Vec::new();
    verbatim_spans(root, &mut skip);

    let mut out = Vec::new();
    for (i, text) in source_lines(src).into_iter().enumerate() {
        // '?' is one byte, so `b + 1` is always a character boundary.
        for (b, _) in text.char_indices().filter(|&(_, c)| c == '?') {
            let pos = Pos::new(i + 1, b + 1);
            if skip.iter().any(|s: &Span| s.contains(pos)) {
                continue;
            }
            let tail = text[b + 1..].trim_start_matches(CLOSERS);
            if tail.is_empty() || tail.starts_with(char::is_whitespace) {
                out.push(pos);
            }
        }
    }
    out
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

    /// Markdown ends a line on `\n`, on `\r\n`, **or on a bare `\r`**, and
    /// comrak numbers `sourcepos` accordingly. `str::lines` only knows the
    /// first two, so a document holding a lone `\r` used to yield a line vector
    /// one entry short from that byte on — and every span below it named a line
    /// that held someone else's text.
    #[test]
    fn source_lines_ends_a_line_where_commonmark_does() {
        let cases: &[(&str, &[&str])] = &[
            ("", &[]),
            ("a", &["a"]),
            ("a\n", &["a"]),
            ("a\n\n", &["a", ""]),
            ("a\nb\n", &["a", "b"]),
            ("a\r\nb\r\n", &["a", "b"]),
            ("a\rb\r", &["a", "b"]),
            // A trailing lone `\r` ends the line it is on; it is not content,
            // and `str::lines` leaves it in the last line as if it were.
            ("para\r", &["para"]),
            // `\r\r\n` is two endings — a bare `\r`, then a `\r\n` — so the
            // line between them is empty. Pairing greedily would lose it.
            ("a\r\r\nb\n", &["a", "", "b"]),
            ("a\n\rb\n", &["a", "", "b"]),
            // The terminator belongs to no line, so a byte column still counts
            // from the line's own first byte whichever ending precedes it.
            ("prüfen\rköde\n", &["prüfen", "köde"]),
        ];
        for (src, want) in cases {
            assert_eq!(source_lines(src), *want, "{src:?}");
        }
    }

    #[test]
    fn a_lone_carriage_return_ends_a_line_the_way_a_newline_does() {
        assert_eq!(
            flat("# H\rpara\r"),
            vec![("heading", 1, 1), ("paragraph", 2, 2)]
        );
        // …and all three endings in one document, which is what a file edited
        // on two machines actually looks like.
        assert_eq!(
            flat("# H\r\n\r\npara\rtail\n"),
            vec![("heading", 1, 1), ("paragraph", 3, 4)]
        );
    }

    /// The same split, arrived at the other way round: normalise all three
    /// endings to `\n`, *then* split on that. A second implementation rather
    /// than a second call, so the corpus is held to something that cannot share
    /// a mistake with `source_lines` — and it is exactly the normalise-at-the-
    /// boundary design this fix rejected, kept as the oracle it makes a good one
    /// of. `App` holds its own line vector to the same standard.
    #[test]
    fn source_lines_agrees_with_normalising_the_endings_first() {
        for src in MIXED_ENDINGS {
            assert_eq!(source_lines(src), normalised_lines(src), "{src:?}");
        }
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

    // ---- tier 4b: the escaped-pipe column shift ----------------------------

    /// Every inline node of `src` in document order, as `(kind, the bytes its
    /// span names)`. The slice is the whole point of these tests: a column that
    /// has drifted off its own text reads as a plausible number and as obvious
    /// nonsense here. Slicing raw is deliberate too — a corrected column that
    /// landed inside a character would panic rather than round quietly.
    fn inline_slices(src: &str) -> Vec<(&'static str, String)> {
        fn go(n: &TreeNode, lines: &[&str], out: &mut Vec<(&'static str, String)>) {
            if is_inline(n.kind) && n.span.start.line == n.span.end.line {
                let line = lines[n.span.start.line - 1];
                out.push((
                    n.kind,
                    line[n.span.start.col - 1..n.span.end.col].to_string(),
                ));
            }
            for c in &n.children {
                go(c, lines, out);
            }
        }
        let lines = source_lines(src);
        let mut out = Vec::new();
        go(&parse_tree(src), &lines, &mut out);
        out
    }

    fn slices(src: &str) -> Vec<String> {
        inline_slices(src).into_iter().map(|(_, s)| s).collect()
    }

    /// comrak's table extension takes the backslash out of `\|` before it parses
    /// a cell's inlines, so every inline column after one is short by a byte —
    /// and `App::slice` then quotes text the reviewer never selected. The cases
    /// here are the ground truth this was fixed against, each read off comrak
    /// 0.54 rather than assumed.
    #[test]
    fn an_escaped_pipe_does_not_shift_what_an_inline_span_quotes() {
        // One escape: the run holding it, and everything after it in the cell.
        assert_eq!(
            slices("| a \\| b | c *em* d |\n|---|---|\n"),
            ["a \\| b", "c ", "*em*", "em", " d"]
        );
        // Two: the shift accumulates, one byte per escape.
        assert_eq!(
            slices("| a \\| b \\| c *em* tail | z |\n|---|---|\n"),
            ["a \\| b \\| c ", "*em*", "em", " tail", "z"]
        );
        // A code span is unescaped with everything else — the rewrite happens
        // before anything knows a code span is there.
        assert_eq!(
            slices("| `x \\| y` then *em* z |\n|---|\n"),
            ["`x \\| y`", " then ", "*em*", "em", " z"]
        );
        // An escape as the last thing in the cell still moves its own run's end.
        assert_eq!(
            slices("| a *em* b \\| |\n|---|\n"),
            ["a ", "*em*", "em", " b \\|"]
        );
        // A cell that is nothing but the escape: the run is the pipe alone.
        assert_eq!(
            slices("| \\| | *em* b |\n|---|---|\n"),
            ["|", "*em*", "em", " b"]
        );
        // Inside a container, where the columns start further right.
        assert_eq!(
            slices("> | a \\| b *em* c |\n> |---|\n"),
            ["a \\| b ", "*em*", "em", " c"]
        );
    }

    /// The shift starts over in every cell. comrak anchors each cell's inlines
    /// at that cell's own start column, so an escape in an earlier cell of the
    /// same row moves nothing in a later one — and a fix that counted a row's
    /// escapes would push every later cell right by them.
    #[test]
    fn the_shift_stops_at_the_edge_of_the_cell_that_causes_it() {
        // Escape in cell one only: cell two was never wrong and stays right.
        assert_eq!(
            slices("| a \\| b | c *em* d |\n|---|---|\n"),
            ["a \\| b", "c ", "*em*", "em", " d"]
        );
        // One in each: cell two moves by its own escape, not by both.
        assert_eq!(
            slices("| a \\| b | c \\| d *em* e |\n|---|---|\n"),
            ["a \\| b", "c \\| d ", "*em*", "em", " e"]
        );
        // And it does not carry into the next row either.
        assert_eq!(
            slices("| h |\n|---|\n| a \\| b *em* c |\n| p *em* q |\n"),
            ["h", "a \\| b ", "*em*", "em", " c", "p ", "*em*", "em", " q"]
        );
    }

    /// `\|` is the odd one out because the *block* parser resolves it, behind
    /// the inline parser's back. Every escape the inline parser handles itself
    /// reports source columns, in a cell as anywhere else — so the correction
    /// must fire for this one and for nothing else.
    #[test]
    fn only_the_pipe_escape_shifts_a_column() {
        assert_eq!(
            slices("| a \\* b \\_ c \\\\ d \\[ e *em* f |\n|---|\n"),
            ["a \\* b \\_ c \\\\ d \\[ e ", "*em*", "em", " f"]
        );
        // `\\|` is an escaped *backslash* and then a pipe. comrak's own state
        // machine clears its flag on the second backslash and drops nothing.
        assert_eq!(
            slices("| a \\\\| b *em* c |\n|---|\n"),
            ["a \\\\| b ", "*em*", "em", " c"]
        );
        // The same escape outside a table is never unescaped at all.
        for src in [
            "para a \\| b *em* c\n",
            "# head a \\| b *em* c\n",
            "- item a \\| b *em* c\n",
        ] {
            let got = slices(src);
            assert!(got.contains(&"*em*".to_string()), "{src:?} -> {got:?}");
            assert!(
                got.iter().any(|s| s.ends_with("a \\| b ")),
                "{src:?} -> {got:?}"
            );
        }
    }

    /// The sibling case, and the reason this is not a table-cell fix. comrak
    /// splits the paragraph lines above a header row off the table's container
    /// and runs them through the *same* `unescape_pipes` call, so a `\|` there
    /// shifts a paragraph that holds no cells at all. The escape does not carry
    /// across the line break, because the newline clears the escape flag.
    #[test]
    fn a_paragraph_a_table_absorbed_shifts_the_same_way_a_cell_does() {
        assert_eq!(
            slices("intro \\| text *em* tail\n| a | b |\n|---|---|\n"),
            ["intro \\| text ", "*em*", "em", " tail", "a", "b"]
        );
        // Two preface lines, the escape on the first: the second is untouched.
        assert_eq!(
            slices("first \\| line *em* one\nsecond *em* line\n| a | b |\n|---|---|\n"),
            [
                "first \\| line ",
                "*em*",
                "em",
                " one",
                "second ",
                "*em*",
                "em",
                " line",
                "a",
                "b"
            ]
        );
        // …and on the second, where the first is the one that must not move.
        assert_eq!(
            slices("first *em* line\nsecond \\| line *em* two\n| a | b |\n|---|---|\n"),
            [
                "first ",
                "*em*",
                "em",
                " line",
                "second \\| line ",
                "*em*",
                "em",
                " two",
                "a",
                "b"
            ]
        );
        // A paragraph with a blank line under it is not a preface — the table
        // never touched its text, and correcting it would break it.
        assert_eq!(
            slices("intro \\| text *em* tail\n\n| a | b |\n|---|---|\n"),
            ["intro \\| text ", "*em*", "em", " tail", "a", "b"]
        );
    }

    /// The corrected column is still a *byte* offset, and still lands on a
    /// character boundary — `inline_slices` slices raw, so anything else is a
    /// panic here rather than a silent round in `ceil_boundary`.
    #[test]
    fn a_corrected_column_is_still_a_byte_offset_on_a_boundary() {
        assert_eq!(
            slices("| Prüfen \\| köde *em* x |\n|---|\n"),
            ["Prüfen \\| köde ", "*em*", "em", " x"]
        );
        // The escape immediately before a multi-byte character, so the shift
        // has to step over the whole of it.
        assert_eq!(
            slices("| \\| ö *em* ü |\n|---|\n"),
            ["| ö ", "*em*", "em", " ü"]
        );
    }

    /// The oracle this defect was found with, as a test.
    ///
    /// A cell's inlines have to land on the same source bytes as the identical
    /// text parsed *outside* a table, where comrak unescapes nothing and its
    /// columns are known good. The reference is comrak's own behaviour on the
    /// other side of the table extension, not a second copy of the correction,
    /// so this cannot pass by agreeing with the code it checks.
    #[test]
    fn a_cell_quotes_what_the_same_text_quotes_outside_a_table() {
        const BODIES: [&str; 8] = [
            "plain text",
            "a *em* b",
            "a **strong** b",
            "a `code` b",
            "see [lbl](http://x) end",
            "a ~~gone~~ b",
            "Prüfen `köde` ü",
            "a <b>x</b> c",
        ];
        // Both escapes that move a column and every neighbouring shape that
        // must not: an escaped backslash, escapes the inline parser owns.
        const ESCAPES: [&str; 6] = ["", " \\| ", " \\* ", " \\\\| ", " \\\\ ", " \\[ "];

        let mut checked = 0;
        for body in BODIES {
            for head in ESCAPES {
                for tail in ESCAPES {
                    let cell = format!("{head}{body}{tail}");
                    let reference = cell.trim_matches(|c: char| c == ' ' || c == '\t');
                    let doc = format!("|{cell}| second *em* cell |\n|---|---|\n");
                    let want = slices(reference);
                    let got = slices(&doc);
                    assert!(
                        got.starts_with(&want),
                        "cell {cell:?}\n  in a table: {got:?}\n  standalone: {want:?}"
                    );
                    checked += 1;
                }
            }
        }
        assert_eq!(checked, BODIES.len() * ESCAPES.len() * ESCAPES.len());
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

    // ---- questions ---------------------------------------------------------

    const QDOC: &str = "\
Should I add validation?

Two on one line? Or not? Yes.

A glob `ls *.rs?` is quiet, and https://docs.rs/?q=1 too.

```sh
test -f x && echo ?
```

Is it worth it (really?) and \"is it?\" too.

Really??
";

    fn qlines(src: &str) -> Vec<usize> {
        questions(&parse_tree(src), src)
            .iter()
            .map(|p| p.line)
            .collect()
    }

    /// One fixture, every class at once — the false positives are the reason
    /// this feature is worth having rather than a plain scan for `?`.
    #[test]
    fn questions_are_sentence_terminators_not_every_question_mark() {
        assert_eq!(qlines(QDOC), vec![1, 3, 3, 11, 11, 13]);
    }

    #[test]
    fn a_question_mark_inside_a_code_span_or_block_is_not_a_question() {
        assert!(!qlines(QDOC).contains(&5), "code span on line 5");
        assert!(!qlines(QDOC).contains(&8), "fenced block on line 8");
    }

    /// `?\\b` — the obvious first guess — matches exactly this and nothing that
    /// belongs, which is why the rule is the inverse.
    #[test]
    fn a_query_string_is_not_a_question() {
        assert!(qlines("See https://docs.rs/?q=1 for more.\n").is_empty());
    }

    #[test]
    fn closing_punctuation_may_sit_between_the_mark_and_the_space() {
        assert_eq!(qlines("Is it (really?) so?\n"), vec![1, 1]);
        assert_eq!(qlines("She asked \"why?\" twice.\n"), vec![1]);
        assert_eq!(qlines("*Really?* he said.\n"), vec![1]);
    }

    /// Only the last mark of a run terminates the sentence, so `??` is one
    /// stop, not two on the same word.
    #[test]
    fn a_run_of_question_marks_yields_one_stop() {
        let q = questions(&parse_tree("Really??\n"), "Really??\n");
        assert_eq!(q, vec![Pos::new(1, 8)]);
    }

    #[test]
    fn columns_are_byte_offsets_so_umlauts_do_not_shift_them() {
        let src = "Wieso Ünicode?\n";
        let q = questions(&parse_tree(src), src);
        assert_eq!(q, vec![Pos::new(1, src.find('?').unwrap() + 1)]);
    }

    /// A question position is addressed by a `sourcepos` line number, so it has
    /// to be measured against `source_lines` — and it was measured against
    /// `str::lines`, which ends a line only on `\n`. On `"x\ry? yes\n"` that
    /// reported `1:4`: one line short, because the `\r` did not end a line, and
    /// three columns right, because it stayed inside one. Line 1 is `"x"`, so
    /// the position named a column past the end of the line it named — the
    /// gutter marked the wrong row and `]` jumped the cursor off the end of it.
    #[test]
    fn a_question_after_a_bare_carriage_return_is_on_the_line_it_is_on() {
        let src = "x\ry? yes\n";
        assert_eq!(questions(&parse_tree(src), src), vec![Pos::new(2, 2)]);
        // All three endings in one document, which is what a file edited on two
        // machines looks like.
        let src = "# Heading\r\n\r\nIs it?\rAnd this one?\n";
        assert_eq!(
            questions(&parse_tree(src), src),
            vec![Pos::new(3, 6), Pos::new(4, 13)]
        );
    }

    /// The oracle the fix was found with, and the shape of check that would have
    /// caught it: every position handed back has to *be* a `?` in the line
    /// vector the rest of the crate indexes. A position measured against a
    /// different split fails this without anyone having to predict which line it
    /// would land on.
    #[test]
    fn every_reported_position_names_a_question_mark_in_the_line_it_names() {
        let corpus: Vec<String> = MIXED_ENDINGS
            .iter()
            .map(|s| format!("{s}\nAnd a trailing question?\n"))
            .chain(
                [
                    QDOC,
                    "x\ry? yes\n",
                    "Is it?\rReally?\r\nOr not?\n",
                    "prüfen — köde?\rzweite Zeile ö?\n",
                    "no questions here at all\r\n",
                ]
                .iter()
                .map(ToString::to_string),
            )
            .collect();

        for src in &corpus {
            let lines = source_lines(src);
            for p in questions(&parse_tree(src), src) {
                let line = lines
                    .get(p.line - 1)
                    .unwrap_or_else(|| panic!("{src:?}: {p:?} names no line"));
                assert_eq!(
                    line.as_bytes().get(p.col - 1).copied(),
                    Some(b'?'),
                    "{src:?}: {p:?} names {line:?}"
                );
            }
        }
    }
}
