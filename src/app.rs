//! Editor state: cursor, selection, annotations, and the two output formats.
//! Deliberately free of any ratatui types so it can be unit-tested without a
//! terminal — which matters, because the agent building this cannot run a TUI.
//!
//! Selection has four shapes, all resolving to a single `Span`:
//!
//! * whole block under the cursor (the default)
//! * `v` — a range of navigation units
//! * `V` — a range of whole lines, ignoring block boundaries
//! * `+`/`-` — a node from the markdown containment hierarchy, from an inline
//!   code span up to the whole document

use std::fmt::Write as _;

use serde::Serialize;

use crate::blocks::{self, Block, Pos, Span, TreeNode};
use crate::editor::Editor;
use crate::highlight::{self, LineMarks};
use crate::table::{self, Tables};
use crate::wrap::{self, Row};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Input,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sel {
    /// Just the navigation unit under the cursor.
    Here,
    /// Block-wise, anchored at a block index.
    Blocks { anchor: usize },
    /// Line-wise, anchored at a line.
    Lines { anchor: usize },
    /// A node of the containment hierarchy; `depth` indexes the stack at the
    /// cursor, 0 being innermost.
    Region { depth: usize },
}

#[derive(Debug, Clone, Serialize)]
pub struct Annotation {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    #[serde(rename = "blockKind")]
    pub block_kind: String,
    #[serde(rename = "startLine")]
    pub start_line: usize,
    #[serde(rename = "startCol")]
    pub start_col: usize,
    #[serde(rename = "endLine")]
    pub end_line: usize,
    #[serde(rename = "endCol")]
    pub end_col: usize,
    /// True when the span covers its lines entirely, so consumers can quote
    /// whole lines instead of a fragment.
    #[serde(rename = "wholeLines")]
    pub whole_lines: bool,
    #[serde(rename = "originalText")]
    pub original_text: String,
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct Source {
    pub path: String,
    /// Display name a launcher asked for. `path` stays the real file, so
    /// provenance survives even when the file is a temporary one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub lines: usize,
}

#[derive(Debug, Serialize)]
pub struct Outcome {
    pub version: u32,
    pub decision: &'static str,
    pub source: Source,
    pub annotations: Vec<Annotation>,
    #[serde(rename = "feedbackMarkdown")]
    pub feedback_markdown: String,
}

pub struct App {
    pub path: String,
    /// Overrides `path` everywhere a human reads it. A launcher that opens a
    /// temp file names the thing being reviewed instead of where it landed.
    pub label: Option<String>,
    pub lines: Vec<String>,
    pub blocks: Vec<Block>,
    pub tree: TreeNode,
    /// Syntax highlighting marks, one entry per source line. Computed once.
    pub marks: Vec<LineMarks>,
    pub cursor: Pos,
    pub sel: Sel,
    pub annotations: Vec<Annotation>,
    pub mode: Mode,
    pub editor: Editor,
    pub status: String,
    pub quit: bool,
    /// Source rows currently visible. Owned by the renderer, which is the only
    /// thing that knows the terminal size; paging keys read it.
    pub viewport: usize,
    /// Peek overlay: the selection, wrapped, over the source view. Read-only —
    /// it exists to answer "what did I actually select", not to edit.
    pub peek: bool,
    /// Wrapped-row offset inside the peek overlay.
    pub peek_scroll: usize,
    /// Rows the peeked text wraps to at the current width. Published by the
    /// renderer for the same reason `viewport` is: only it knows the geometry.
    pub peek_rows: usize,
    /// Render the source legibly rather than literally: soft wrap, and tables
    /// aligned by padding. Off means the old behaviour — lines run past the
    /// right edge with a marker to say so, and every cell on screen is a byte
    /// of the file. That is the mode to turn to when a column looks wrong.
    ///
    /// One switch, not two. The question it answers is "am I looking at the
    /// bytes or at something readable", and a half-pretty third mode would be
    /// a state nobody asked for.
    pub pretty: bool,
    /// Cells the body column is wide, published by the renderer. Zero until the
    /// first frame, and zero is also how `wrap_source` is told not to wrap and
    /// how `Tables::pads` is told not to align — so every row-addressed motion
    /// degrades to line-addressed before any geometry is known, which is what
    /// the headless tests exercise.
    pub body_width: usize,
    /// Alignment padding per line. Depends on every row of a table but on
    /// nothing about the terminal, so it is computed once here and the width
    /// only decides whether to use it.
    tables: Tables,
    next_id: usize,
}

/// Top of the source viewport: a line, and how many of its wrapped rows sit
/// above the fold. `row` is 0 for any line short enough not to wrap.
///
/// An anchor rather than a document-wide row index: only the visible rows are
/// ever wrapped, so a width change costs nothing and the 2,476-row line in the
/// corpus costs nothing until you are inside it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Anchor {
    pub line: usize,
    pub row: usize,
}

impl Default for Anchor {
    fn default() -> Self {
        Self { line: 1, row: 0 }
    }
}

/// Largest byte index `<= i` that starts a character.
fn floor_boundary(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Smallest byte index `>= i` that starts a character.
fn ceil_boundary(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn strictly_contains(outer: Span, inner: Span) -> bool {
    outer.start <= inner.start && outer.end >= inner.end && outer != inner
}

impl App {
    pub fn new(path: String, src: &str) -> Self {
        let lines: Vec<String> = src.lines().map(std::string::ToString::to_string).collect();
        let blocks = blocks::parse(src);
        let tree = blocks::parse_tree(src);
        let marks = highlight::marks(&tree, src);
        let cursor = blocks.first().map_or(Pos::new(1, 1), |b| b.span.start);
        let tables = Tables::new(&lines, &blocks);
        Self {
            path,
            label: None,
            lines,
            blocks,
            tree,
            marks,
            cursor,
            sel: Sel::Here,
            annotations: Vec::new(),
            mode: Mode::Normal,
            editor: Editor::default(),
            status: String::new(),
            quit: false,
            viewport: 20,
            peek: false,
            peek_scroll: 0,
            peek_rows: 0,
            pretty: true,
            body_width: 0,
            tables,
            next_id: 1,
        }
    }

    // ---- row space ------------------------------------------------------
    //
    // A "row" is one screen line. With wrapping off it is a source line and
    // every function here is the identity; with it on, one source line is one
    // or more rows and the mapping runs through `wrap_source`.

    /// The line as the screen shows it. Tabs become one space each so a byte
    /// offset stays a cell offset — see the note in `draw_source`.
    pub fn display_line(&self, line: usize) -> String {
        self.line_text(line).replace('\t', " ")
    }

    /// Rows of line `line` — source bytes and any alignment padding, in screen
    /// order — plus the hanging indent its continuation rows carry.
    ///
    /// A padded table row is one row and never wraps: `Tables::pads` withholds
    /// the padding for any table whose aligned width does not fit, so the two
    /// transforms never have to compose on one line.
    pub fn line_rows(&self, line: usize) -> (Vec<Row>, usize) {
        let text = self.display_line(line);
        if self.pretty {
            if let Some(pads) = self.tables.pads(line, self.body_width) {
                return (vec![table::row(text.len(), pads)], 0);
            }
        }
        let width = if self.pretty { self.body_width } else { 0 };
        wrap::wrap_source(&text, width)
    }

    pub fn row_count(&self, line: usize) -> usize {
        self.line_rows(line).0.len().max(1)
    }

    /// Which of `line`'s rows holds byte `col - 1`. Padding cannot confuse this:
    /// it never changes the order of the source bytes, so row starts still
    /// ascend and the last one at or below `b` is the row `b` is in.
    pub fn cursor_row(&self) -> usize {
        let (rows, _) = self.line_rows(self.cursor.line);
        let b = self.cursor.col.saturating_sub(1);
        rows.iter()
            .rposition(|r| wrap::row_start(r) <= b)
            .unwrap_or(0)
    }

    /// One row up or down from `at`, or `None` at either end of the document.
    /// Walking, not indexing: nothing here is O(document).
    pub fn step_row(&self, at: Anchor, down: bool) -> Option<Anchor> {
        if down {
            if at.row + 1 < self.row_count(at.line) {
                return Some(Anchor {
                    line: at.line,
                    row: at.row + 1,
                });
            }
            (at.line < self.line_count()).then(|| Anchor {
                line: at.line + 1,
                row: 0,
            })
        } else if at.row > 0 {
            Some(Anchor {
                line: at.line,
                row: at.row - 1,
            })
        } else {
            (at.line > 1).then(|| Anchor {
                line: at.line - 1,
                row: self.row_count(at.line - 1) - 1,
            })
        }
    }

    /// `n` rows from `at`, stopping at the document edge.
    pub fn walk_rows(&self, at: Anchor, n: usize, down: bool) -> Anchor {
        let mut a = at;
        for _ in 0..n {
            match self.step_row(a, down) {
                Some(next) => a = next,
                None => break,
            }
        }
        a
    }

    /// Move the cursor one display row, keeping its offset within the row where
    /// the target is long enough. Line-wise `j`/`k` cannot reach the middle of a
    /// line that wraps to thousands of rows; this can.
    pub fn move_row(&mut self, dir: isize) {
        self.drop_region();
        let (rows, _) = self.line_rows(self.cursor.line);
        let cur = self.cursor_row();
        // Saturating, not bare: `cursor_row` falls back to row 0 when no row
        // starts at or below the cursor byte, and a bare subtraction then
        // underflows -- a panic in debug, and in release a wrapped offset that
        // the `min` below silently clamps to the end of the target row. Rows are
        // a partition again as of `wrap_line`'s indentation fix, so the fallback
        // should be unreachable; this keeps a wrong answer from becoming a crash
        // if some future row shape reopens the gap.
        let off = self
            .cursor
            .col
            .saturating_sub(1)
            .saturating_sub(wrap::row_start(&rows[cur]));

        let here = Anchor {
            line: self.cursor.line,
            row: cur,
        };
        let Some(to) = self.step_row(here, dir > 0) else {
            return;
        };
        let (target, _) = self.line_rows(to.line);
        let row = &target[to.row.min(target.len() - 1)];
        let (s, e) = (wrap::row_start(row), wrap::row_end(row));
        self.cursor.line = to.line;
        self.cursor.col = s + off.min(e.saturating_sub(s)) + 1;
        self.snap();
    }

    pub fn toggle_pretty(&mut self) {
        self.pretty = !self.pretty;
        self.status = if self.pretty {
            "pretty on".into()
        } else {
            "pretty off".into()
        };
    }

    pub fn line_count(&self) -> usize {
        self.lines.len().max(1)
    }

    pub fn line_text(&self, line: usize) -> &str {
        self.lines
            .get(line.saturating_sub(1))
            .map_or("", std::string::String::as_str)
    }

    pub fn line_len(&self, line: usize) -> usize {
        self.line_text(line).len()
    }

    pub fn current_block(&self) -> Option<usize> {
        blocks::block_at(&self.blocks, self.cursor.line)
    }

    pub fn stack(&self) -> Vec<(&'static str, Span)> {
        blocks::containment_stack(&self.tree, self.cursor)
    }

    // ---- selection ------------------------------------------------------

    pub fn selection(&self) -> Option<Span> {
        match self.sel {
            Sel::Here => self.current_block().map(|i| self.blocks[i].span),
            Sel::Blocks { anchor } => {
                let cur = self.current_block()?;
                let (a, b) = (anchor.min(cur), anchor.max(cur));
                Some(self.blocks[a].span.union(self.blocks[b].span))
            }
            Sel::Lines { anchor } => {
                let (a, b) = (anchor.min(self.cursor.line), anchor.max(self.cursor.line));
                Some(Span {
                    start: Pos::new(a, 1),
                    end: Pos::new(b, self.line_len(b).max(1)),
                })
            }
            Sel::Region { depth } => {
                let stack = self.stack();
                stack
                    .get(depth.min(stack.len().saturating_sub(1)))
                    .map(|(_, s)| *s)
            }
        }
    }

    /// What the current selection should be labelled as.
    pub fn selection_kind(&self) -> String {
        match self.sel {
            Sel::Here => self
                .current_block()
                .map(|i| self.blocks[i].kind.to_string())
                .unwrap_or_default(),
            Sel::Blocks { anchor } => match self.current_block() {
                Some(cur) if cur != anchor => {
                    let (a, b) = (anchor.min(cur), anchor.max(cur));
                    format!("{}..{}", self.blocks[a].kind, self.blocks[b].kind)
                }
                Some(cur) => self.blocks[cur].kind.to_string(),
                None => String::new(),
            },
            Sel::Lines { .. } => "lines".into(),
            Sel::Region { depth } => {
                let stack = self.stack();
                stack
                    .get(depth.min(stack.len().saturating_sub(1)))
                    .map(|(k, _)| (*k).to_string())
                    .unwrap_or_default()
            }
        }
    }

    /// Byte range `[start, end)` of the selection within `line`, for rendering.
    pub fn selected_bytes_on(&self, line: usize) -> Option<(usize, usize)> {
        let span = self.selection()?;
        let (a, b) = span.byte_range_on(line, self.line_len(line))?;
        let text = self.line_text(line);
        Some((floor_boundary(text, a), ceil_boundary(text, b)))
    }

    pub fn line_selected(&self, line: usize) -> bool {
        matches!(self.selection(), Some(s) if s.touches_line(line))
    }

    pub fn annotations_on(&self, line: usize) -> usize {
        self.annotations
            .iter()
            .filter(|a| line >= a.start_line && line <= a.end_line)
            .count()
    }

    pub fn toggle_blocks(&mut self) {
        self.sel = if let Sel::Blocks { .. } = self.sel {
            self.status = "selection cleared".into();
            Sel::Here
        } else {
            self.status = "block selection — J/K to extend".into();
            Sel::Blocks {
                anchor: self.current_block().unwrap_or(0),
            }
        };
    }

    pub fn toggle_lines(&mut self) {
        self.sel = if let Sel::Lines { .. } = self.sel {
            self.status = "selection cleared".into();
            Sel::Here
        } else {
            self.status = "line selection — j/k to extend".into();
            Sel::Lines {
                anchor: self.cursor.line,
            }
        };
    }

    /// Widen to the smallest hierarchy node strictly containing the selection.
    pub fn expand(&mut self) {
        let stack = self.stack();
        if stack.is_empty() {
            return;
        }
        let depth = match self.selection() {
            Some(cur) => stack
                .iter()
                .position(|(_, s)| strictly_contains(*s, cur))
                .unwrap_or(stack.len() - 1),
            None => 0,
        };
        self.sel = Sel::Region { depth };
        self.note_region(&stack, depth);
    }

    /// Narrow to the largest hierarchy node strictly inside the selection.
    pub fn contract(&mut self) {
        let stack = self.stack();
        if stack.is_empty() {
            return;
        }
        let depth = match self.selection() {
            Some(cur) => stack
                .iter()
                .rposition(|(_, s)| strictly_contains(cur, *s))
                .unwrap_or(0),
            None => 0,
        };
        self.sel = Sel::Region { depth };
        self.note_region(&stack, depth);
    }

    fn note_region(&mut self, stack: &[(&'static str, Span)], depth: usize) {
        let (kind, span) = stack[depth];
        self.status = if span.start.line == span.end.line {
            format!(
                "{} L{}:{}-{}",
                kind, span.start.line, span.start.col, span.end.col
            )
        } else {
            format!("{} L{}-{}", kind, span.start.line, span.end.line)
        };
    }

    // ---- movement -------------------------------------------------------

    /// Moving the cursor abandons a hierarchy selection — the stack it was an
    /// index into no longer applies. Block- and line-wise selections are
    /// anchored, so movement extends them instead.
    ///
    /// The status line is cleared with it: it was showing that region's span,
    /// and leaving it up means the footer keeps advertising a selection that no
    /// longer exists.
    fn drop_region(&mut self) {
        if matches!(self.sel, Sel::Region { .. }) {
            self.sel = Sel::Here;
            self.status.clear();
        }
    }

    fn snap(&mut self) {
        let len = self.line_len(self.cursor.line);
        let max = len.max(1);
        let text = self.line_text(self.cursor.line);
        let byte = floor_boundary(text, self.cursor.col.saturating_sub(1).min(max - 1));
        self.cursor.col = byte + 1;
    }

    pub fn move_line(&mut self, delta: isize) {
        self.drop_region();
        let target = self.cursor.line as isize + delta;
        self.cursor.line = target.clamp(1, self.line_count() as isize) as usize;
        self.snap();
    }

    pub fn move_char(&mut self, delta: isize) {
        self.drop_region();
        let text = self.line_text(self.cursor.line).to_string();
        let mut bounds: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
        if bounds.is_empty() {
            bounds.push(0);
        }
        let cur = self.cursor.col.saturating_sub(1);
        let idx = bounds.iter().rposition(|&b| b <= cur).unwrap_or(0);
        let next = (idx as isize + delta).clamp(0, bounds.len() as isize - 1) as usize;
        self.cursor.col = bounds[next] + 1;
    }

    pub fn move_block(&mut self, delta: isize) {
        self.drop_region();
        let Some(cur) = self.current_block() else {
            return;
        };
        let target = (cur as isize + delta).clamp(0, self.blocks.len() as isize - 1) as usize;
        self.cursor = self.blocks[target].span.start;
        self.snap();
    }

    /// Page the cursor. `dir` is +1 down / -1 up; a full page keeps two lines
    /// of overlap the way vim's C-f does, so you never lose your place.
    pub fn page(&mut self, dir: isize, half: bool) {
        let v = self.viewport.max(1);
        let step = if half {
            (v / 2).max(1)
        } else {
            v.saturating_sub(2).max(1)
        };
        // `viewport` counts rows, so a page is a page of rows. Paging by lines
        // while wrapping would overshoot by the amplification factor — 1.307x
        // across the corpus, and far more inside a table.
        if self.pretty && self.body_width > 0 {
            self.drop_region();
            let here = Anchor {
                line: self.cursor.line,
                row: self.cursor_row(),
            };
            let to = self.walk_rows(here, step, dir > 0);
            let (rows, _) = self.line_rows(to.line);
            self.cursor.line = to.line;
            self.cursor.col = wrap::row_start(&rows[to.row.min(rows.len() - 1)]) + 1;
            self.snap();
        } else {
            self.move_line(dir * step as isize);
        }
    }

    pub fn goto_first(&mut self) {
        self.drop_region();
        self.cursor = Pos::new(1, 1);
    }

    pub fn goto_last(&mut self) {
        self.drop_region();
        self.cursor = Pos::new(self.line_count(), 1);
        self.snap();
    }

    pub fn goto_line_start(&mut self) {
        self.drop_region();
        self.cursor.col = 1;
    }

    pub fn goto_line_end(&mut self) {
        self.drop_region();
        self.cursor.col = self.line_len(self.cursor.line).max(1);
        self.snap();
    }

    /// Step to the next/previous inline node start. On a line wider than the
    /// pane this is what puts the cursor *inside* the code span or link at
    /// column 200, which is the only thing `+`/`-` needs the column for — and
    /// it reports where it landed, because the target may be off screen.
    pub fn move_inline(&mut self, dir: isize) {
        self.drop_region();
        let target = if dir > 0 {
            blocks::next_inline(&self.tree, self.cursor)
        } else {
            blocks::prev_inline(&self.tree, self.cursor)
        };
        let Some(pos) = target else {
            self.status = "no further inline node".into();
            return;
        };
        self.cursor = pos;
        self.snap();
        let kind = self
            .stack()
            .first()
            .map_or_else(String::new, |(k, _)| (*k).to_string());
        self.status = format!("{kind} L{}:{}", self.cursor.line, self.cursor.col);
    }

    // ---- peek -----------------------------------------------------------

    pub fn toggle_peek(&mut self) {
        self.peek = !self.peek;
        self.peek_scroll = 0;
        if self.peek && self.selection().is_none() {
            self.peek = false;
            self.status = "nothing to peek at".into();
        }
    }

    pub fn scroll_peek(&mut self, delta: isize) {
        self.peek_scroll = self
            .peek_scroll
            .saturating_add_signed(delta)
            .min(self.peek_rows.saturating_sub(1));
    }

    /// The text the peek overlay shows: exactly what would be quoted.
    pub fn peek_text(&self) -> String {
        self.selection().map(|s| self.slice(s)).unwrap_or_default()
    }

    // ---- annotating -----------------------------------------------------

    pub fn begin_comment(&mut self) {
        if self.selection().is_none() {
            self.status = "nothing to annotate".into();
            return;
        }
        self.mode = Mode::Input;
        self.editor.start_fresh();
    }

    pub fn commit_comment(&mut self) {
        let text = self.editor.text().trim().to_string();
        self.mode = Mode::Normal;
        if text.is_empty() {
            self.editor.start_fresh();
            self.status = "empty comment discarded".into();
            return;
        }
        self.editor.submit();
        let Some(span) = self.selection() else { return };
        let kind = self.selection_kind();
        let quoted = self.slice(span);
        let id = format!("a{}", self.next_id);
        self.next_id += 1;
        self.annotations.push(Annotation {
            id,
            kind: "comment",
            block_kind: kind,
            start_line: span.start.line,
            start_col: span.start.col,
            end_line: span.end.line,
            end_col: span.end.col,
            whole_lines: self.is_whole_lines(span),
            original_text: quoted,
            text,
        });
        self.sel = Sel::Here;
        self.status = format!("{} annotation(s)", self.annotations.len());
    }

    pub fn cancel_input(&mut self) {
        self.mode = Mode::Normal;
        self.editor.start_fresh();
        self.status = "cancelled".into();
    }

    pub fn remove_at_cursor(&mut self) {
        let line = self.cursor.line;
        let hit = self
            .annotations
            .iter()
            .rposition(|a| line >= a.start_line && line <= a.end_line);
        match hit {
            Some(i) => {
                self.annotations.remove(i);
                self.status = "annotation removed".into();
            }
            None => self.status = "no annotation here".into(),
        }
    }

    fn is_whole_lines(&self, span: Span) -> bool {
        span.start.col == 1 && span.end.col >= self.line_len(span.end.line).max(1)
    }

    /// The exact source text a span covers.
    pub fn slice(&self, span: Span) -> String {
        let mut out = Vec::new();
        for line in span.start.line..=span.end.line {
            let text = self.line_text(line);
            let a = if line == span.start.line {
                floor_boundary(text, span.start.col.saturating_sub(1))
            } else {
                0
            };
            let b = if line == span.end.line {
                ceil_boundary(text, span.end.col)
            } else {
                text.len()
            };
            out.push(text[a.min(b)..b].to_string());
        }
        out.join("\n")
    }

    // ---- output ---------------------------------------------------------

    /// What a human should see this file called.
    pub fn display_name(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.path)
    }

    fn loc(&self, a: &Annotation) -> String {
        let name = self.display_name();
        if a.whole_lines {
            if a.start_line == a.end_line {
                format!("{}:{}", name, a.start_line)
            } else {
                format!("{}:{}-{}", name, a.start_line, a.end_line)
            }
        } else if a.start_line == a.end_line {
            format!("{}:{}:{}-{}", name, a.start_line, a.start_col, a.end_col)
        } else {
            format!(
                "{}:{}:{}-{}:{}",
                name, a.start_line, a.start_col, a.end_line, a.end_col
            )
        }
    }

    pub fn feedback_markdown(&self) -> String {
        if self.annotations.is_empty() {
            return String::new();
        }
        let mut out = format!("# Review feedback: {}\n", self.display_name());
        for a in &self.annotations {
            let _ = write!(out, "\n## {} · {}\n", self.loc(a), a.block_kind);
            for l in a.original_text.lines() {
                let _ = writeln!(out, "> {l}");
            }
            let _ = write!(out, "\n{}\n", a.text);
        }
        out
    }

    pub fn result(&self) -> Outcome {
        Outcome {
            version: 1,
            decision: if self.annotations.is_empty() {
                "approved"
            } else {
                "changes-requested"
            },
            source: Source {
                path: self.path.clone(),
                label: self.label.clone(),
                lines: self.lines.len(),
            },
            annotations: self.annotations.clone(),
            feedback_markdown: self.feedback_markdown(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "\
# Steps

- [ ] Add validation to the login form
- [ ] Write tests for the validation

Use `parse_document` and the [comrak docs](https://docs.rs) here.
";

    fn app() -> App {
        App::new("PLAN.md".into(), DOC)
    }

    fn commit(a: &mut App, text: &str) {
        a.begin_comment();
        a.editor.set(text);
        a.commit_comment();
    }

    #[test]
    fn starts_on_the_first_block() {
        let a = app();
        assert_eq!(a.cursor, Pos::new(1, 1));
        assert_eq!(a.blocks[a.current_block().unwrap()].kind, "heading");
    }

    #[test]
    fn block_movement_lands_on_block_starts() {
        let mut a = app();
        a.move_block(1);
        assert_eq!(a.cursor.line, 3);
        a.move_block(1);
        assert_eq!(a.cursor.line, 4);
        a.move_block(1);
        assert_eq!(a.cursor.line, 6);
    }

    /// The long-line workflow without a viewport offset: `w` walks onto the
    /// code span, `-` narrows to it, and the status says where it went — which
    /// matters precisely because the target may be off the right edge.
    #[test]
    fn inline_motion_lands_inside_a_node_so_contract_can_narrow_to_it() {
        let mut a = app();
        a.cursor = Pos::new(6, 1);
        a.move_inline(1);
        a.contract();
        assert_eq!(a.selection_kind(), "code-span");
        let s = a.selection().unwrap();
        assert_eq!(a.slice(s), "`parse_document`");
    }

    #[test]
    fn inline_motion_reports_where_it_landed_and_stops_at_the_ends() {
        let mut a = app();
        a.cursor = Pos::new(6, 1);
        a.move_inline(1);
        assert!(a.status.starts_with("code-span L6:5"), "{}", a.status);

        a.cursor = Pos::new(1, 1);
        a.move_inline(-1);
        assert_eq!(a.status, "no further inline node");
        assert_eq!(a.cursor, Pos::new(1, 1));
    }

    #[test]
    fn inline_motion_drops_a_hierarchy_selection_like_every_other_move() {
        let mut a = app();
        a.cursor = Pos::new(6, 6);
        a.contract();
        assert!(matches!(a.sel, Sel::Region { .. }));
        a.move_inline(1);
        assert!(matches!(a.sel, Sel::Here));
    }

    #[test]
    fn peek_shows_the_selection_and_refuses_when_there_is_none() {
        let mut a = app();
        a.cursor = Pos::new(6, 6);
        a.contract();
        a.toggle_peek();
        assert!(a.peek);
        assert_eq!(a.peek_text(), "`parse_document`");
        a.toggle_peek();
        assert!(!a.peek);

        let mut empty = App::new("empty.md".into(), "");
        empty.toggle_peek();
        assert!(!empty.peek);
        assert_eq!(empty.status, "nothing to peek at");
    }

    #[test]
    fn paging_moves_by_half_and_whole_viewports() {
        let src: String = (1..=200).map(|i| format!("line {i}\n")).collect();
        let mut a = App::new("big.md".into(), &src);
        a.viewport = 20;
        a.cursor = Pos::new(1, 1);

        a.page(1, true); // C-d
        assert_eq!(a.cursor.line, 11);
        a.page(1, false); // C-f, two lines of overlap
        assert_eq!(a.cursor.line, 29);
        a.page(-1, true); // C-u
        assert_eq!(a.cursor.line, 19);
        a.page(-1, false); // C-b
        assert_eq!(a.cursor.line, 1);
    }

    /// 100 columns per line, so every line wraps at any sane pane width.
    fn wide_doc(lines: usize) -> String {
        (0..lines)
            .map(|_| format!("{}\n", "word ".repeat(20)))
            .collect()
    }

    /// `j` moves a source line however tall it is; `C-n` moves one screen row,
    /// which is the only way to put the cursor in the middle of a line that
    /// wraps to more rows than the viewport has.
    #[test]
    fn move_row_walks_inside_a_line_while_move_line_steps_over_it() {
        let mut a = App::new("w.md".into(), &wide_doc(3));
        a.body_width = 20;
        let (rows, _) = a.line_rows(1);
        assert!(rows.len() >= 4, "line did not wrap: {} rows", rows.len());

        a.cursor = Pos::new(1, 1);
        a.move_row(1);
        assert_eq!(a.cursor.line, 1, "C-n left the line");
        assert_eq!(a.cursor.col, wrap::row_start(&rows[1]) + 1);
        a.move_row(-1);
        assert_eq!((a.cursor.line, a.cursor.col), (1, 1));

        a.move_line(1);
        assert_eq!(a.cursor.line, 2, "j should clear the whole wrapped line");
    }

    /// `move_row` subtracts the current row's start from the cursor byte, which
    /// is only sound while row 0 starts at byte 0. It did not for an indented
    /// line whose first word overflows the row: the wrapper stepped past the
    /// indentation, `cursor_row`'s `unwrap_or(0)` fallback fired, and the
    /// subtraction underflowed — a panic in debug, and in release a wrapped
    /// offset the `min` clamped to the end of the target row.
    #[test]
    fn move_row_survives_an_indented_line_whose_first_word_overflows() {
        let doc = "- item\n\n    https://example.dev/docs/reference/config/advanced#section-42\n";
        for width in [20usize, 42, 72] {
            let mut a = App::new("w.md".into(), doc);
            a.body_width = width;

            for line in 1..=a.line_count() {
                let (rows, _) = a.line_rows(line);
                assert_eq!(
                    wrap::row_start(&rows[0]),
                    0,
                    "line {line} at width {width} starts past byte 0"
                );
            }

            a.cursor = Pos::new(3, 1);
            a.move_row(1);
            let (rows, _) = a.line_rows(3);
            if rows.len() > 1 {
                assert_eq!(
                    a.cursor.col,
                    wrap::row_start(&rows[1]) + 1,
                    "C-n landed off the row start at width {width}"
                );
            }
        }
    }

    /// `viewport` counts rows, so paging must too — paging by lines while
    /// wrapping overshoots by however far the lines wrap.
    #[test]
    fn paging_counts_rows_not_lines_when_wrapped() {
        let mut a = App::new("w.md".into(), &wide_doc(50));
        a.body_width = 20;
        a.viewport = 20;
        a.cursor = Pos::new(1, 1);

        a.page(1, true); // half a viewport: 10 rows, not 10 lines
        assert!(a.cursor.line > 1, "paging stalled");
        assert!(
            a.cursor.line <= 3,
            "paged by lines, not rows: landed on line {}",
            a.cursor.line
        );

        // …and off it does the old thing, exactly.
        let mut b = App::new("w.md".into(), &wide_doc(50));
        b.pretty = false;
        b.viewport = 20;
        b.cursor = Pos::new(1, 1);
        b.page(1, true);
        assert_eq!(b.cursor.line, 11);
    }

    /// The walk is over rows, so a line taller than the document is long must
    /// not make it proportional to that height.
    #[test]
    fn row_walking_terminates_on_a_pathologically_tall_line() {
        let src = format!("short\n{}\nshort\n", "x ".repeat(20_000));
        let mut a = App::new("huge.md".into(), &src);
        a.body_width = 40;
        assert!(a.row_count(2) > 900, "expected a very tall line");

        // Off the top of it and back, without walking every row in between.
        a.cursor = Pos::new(2, 1);
        a.move_line(1);
        assert_eq!(a.cursor.line, 3);
        let top = a.walk_rows(Anchor { line: 3, row: 0 }, 5, false);
        assert_eq!(top.line, 2, "walking back should land inside the tall line");
        assert_eq!(top.row, a.row_count(2) - 5);
    }

    /// The property everything in row space stands on, generalised from wrap's
    /// ordered-windows test to cover alignment padding.
    ///
    /// Padding is inserted *between* source bytes, never over them, so the
    /// `Src` pieces of a line's rows are still an ascending partition of it:
    /// every non-space byte lands in exactly one row, at exactly one offset.
    /// That is what makes `draw_source` able to rebase a mark by clipping, and
    /// `cursor_row` able to find a byte's row by scanning starts.
    #[test]
    fn pretty_rows_concatenate_to_the_source_line() {
        let src = "\
# Table

| id | a very long description of the thing | ok |
|---|:---:|--:|
| 1 | short | y |
| 22 | a much longer description than the first one | n |
|日本|語のテキスト|x|

A paragraph long enough to wrap at any of the widths below, with a
https://example.dev/a/very/long/path in it as well.
";
        let mut a = App::new("t.md".into(), src);
        for width in [0usize, 12, 39, 60, 200] {
            a.body_width = width;
            for line in 1..=a.line_count() {
                let text = a.display_line(line);
                let (rows, _) = a.line_rows(line);
                let mut prev = 0;
                let mut kept = String::new();
                for r in &rows {
                    for p in r {
                        if let wrap::Piece::Src(s, e) = *p {
                            assert!(s >= prev, "L{line} w{width}: rows go backwards");
                            assert!(s <= e && e <= text.len(), "L{line} w{width}: {s}..{e}");
                            assert!(text.is_char_boundary(s) && text.is_char_boundary(e));
                            kept.push_str(&text[s..e]);
                            prev = e;
                        }
                    }
                }
                assert_eq!(
                    kept.replace(' ', ""),
                    text.replace(' ', ""),
                    "L{line} w{width}: dropped or duplicated text"
                );
            }
        }
    }

    /// Aligning widens a table, so a padded row that did not fit would have to
    /// wrap — and the padding is not wrap-aware. `Tables::pads` withholds the
    /// padding in exactly that case, which is what keeps the two from ever
    /// having to compose on one line.
    #[test]
    fn a_padded_row_is_always_exactly_one_row() {
        let src = "| id | description | ok |\n|---|---|---|\n| 1 | short | y |\n";
        let mut a = App::new("t.md".into(), src);
        for width in 1..=80 {
            a.body_width = width;
            for line in 1..=3 {
                let (rows, _) = a.line_rows(line);
                let padded = rows
                    .iter()
                    .flatten()
                    .any(|p| matches!(p, wrap::Piece::Pad { .. }));
                assert!(!padded || rows.len() == 1, "L{line} w{width}: {rows:?}");
            }
        }
    }

    #[test]
    fn paging_is_clamped_and_never_stalls() {
        let src: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        let mut a = App::new("big.md".into(), &src);
        a.cursor = Pos::new(1, 1);

        // a degenerate viewport must still advance by at least one line
        a.viewport = 0;
        a.page(1, true);
        assert_eq!(a.cursor.line, 2);
        a.page(1, false);
        assert_eq!(a.cursor.line, 3);

        a.viewport = 20;
        for _ in 0..10 {
            a.page(1, false);
        }
        assert_eq!(a.cursor.line, a.line_count());
        for _ in 0..10 {
            a.page(-1, false);
        }
        assert_eq!(a.cursor.line, 1);
    }

    #[test]
    fn movement_is_clamped_at_both_ends() {
        let mut a = app();
        a.move_block(-5);
        assert_eq!(a.cursor.line, 1);
        a.move_block(99);
        assert_eq!(a.cursor.line, 6);
        a.move_line(-99);
        assert_eq!(a.cursor.line, 1);
        a.move_line(999);
        assert_eq!(a.cursor.line, a.line_count());
    }

    // ---- tier 1 ---------------------------------------------------------

    #[test]
    fn tier1_table_rows_are_separate_navigation_units() {
        let src = "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n";
        let a = App::new("t.md".into(), src);
        let kinds: Vec<_> = a
            .blocks
            .iter()
            .map(|b| (b.kind, b.start(), b.end()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("table-row", 1, 2),
                ("table-row", 3, 3),
                ("table-row", 4, 4)
            ]
        );
    }

    #[test]
    fn tier1_line_selection_ignores_block_boundaries() {
        let mut a = app();
        a.cursor = Pos::new(3, 1);
        a.toggle_lines();
        a.move_line(1);
        a.move_line(1);
        let s = a.selection().unwrap();
        assert_eq!((s.start.line, s.end.line), (3, 5));
        // line 5 is blank and belongs to no block — impossible block-wise
        assert!(a.line_selected(5));
        assert_eq!(a.selection_kind(), "lines");
    }

    #[test]
    fn tier1_line_selection_extends_backwards() {
        let mut a = app();
        a.cursor = Pos::new(4, 1);
        a.toggle_lines();
        a.move_line(-2);
        let s = a.selection().unwrap();
        assert_eq!((s.start.line, s.end.line), (2, 4));
    }

    // ---- tier 3: columns ------------------------------------------------

    #[test]
    fn tier3_char_movement_steps_over_multibyte_characters() {
        let mut a = App::new("u.md".into(), "Prüfen köde\n");
        a.cursor = Pos::new(1, 1);
        a.move_char(1); // P -> r
        assert_eq!(a.cursor.col, 2);
        a.move_char(1); // r -> ü  (2 bytes)
        assert_eq!(a.cursor.col, 3);
        a.move_char(1); // ü -> f, skipping the continuation byte
        assert_eq!(a.cursor.col, 5);
        a.move_char(-1);
        assert_eq!(a.cursor.col, 3);
    }

    #[test]
    fn tier3_char_movement_is_clamped_to_the_line() {
        let mut a = app();
        a.cursor = Pos::new(1, 1);
        a.move_char(-5);
        assert_eq!(a.cursor.col, 1);
        a.move_char(500);
        assert_eq!(a.cursor.col, a.line_len(1));
    }

    #[test]
    fn tier3_selected_bytes_land_on_character_boundaries() {
        let mut a = App::new("u.md".into(), "Prüfen `köde` hier.\n");
        a.cursor = Pos::new(1, 10);
        a.contract(); // paragraph -> the code span
        let (s, e) = a.selected_bytes_on(1).unwrap();
        let text = a.line_text(1);
        assert!(text.is_char_boundary(s) && text.is_char_boundary(e));
        assert_eq!(&text[s..e], "`köde`");
    }

    #[test]
    fn tier3_annotation_records_columns_and_slices_exactly() {
        let mut a = app();
        a.cursor = Pos::new(6, 6); // inside `parse_document`
        a.contract(); // paragraph -> code span
        commit(&mut a, "call it once");
        let ann = &a.annotations[0];
        assert_eq!((ann.start_line, ann.end_line), (6, 6));
        assert_eq!(ann.original_text, "`parse_document`");
        assert!(!ann.whole_lines);
        assert_eq!(ann.block_kind, "code-span");
    }

    #[test]
    fn tier3_whole_line_selections_are_flagged_as_such() {
        let mut a = app();
        a.move_block(1);
        commit(&mut a, "model layer");
        assert!(a.annotations[0].whole_lines);
        assert_eq!(
            a.annotations[0].original_text,
            "- [ ] Add validation to the login form"
        );
    }

    // ---- tier 4: expand / contract --------------------------------------

    #[test]
    fn tier4_expand_widens_step_by_step_to_the_document() {
        let mut a = app();
        a.cursor = Pos::new(6, 32); // inside the link label
        a.contract(); // the default selection is the whole paragraph
        assert_eq!(a.selection_kind(), "link");
        let mut seen = Vec::new();
        for _ in 0..5 {
            a.expand();
            seen.push(a.selection_kind());
        }
        assert_eq!(seen[0], "paragraph");
        assert_eq!(seen[1], "document");
        // clamped at the top, never panics
        assert_eq!(seen[4], "document");
    }

    #[test]
    fn tier4_expansion_is_monotonic() {
        let mut a = app();
        a.cursor = Pos::new(6, 32);
        a.contract();
        a.contract();
        let mut prev = a.selection().unwrap();
        for _ in 0..4 {
            a.expand();
            let now = a.selection().unwrap();
            assert!(now.start <= prev.start && now.end >= prev.end);
            prev = now;
        }
    }

    #[test]
    fn tier4_contract_narrows_back_down() {
        let mut a = app();
        a.cursor = Pos::new(6, 32);
        assert_eq!(a.selection_kind(), "paragraph");
        a.contract();
        assert_eq!(a.selection_kind(), "link");
        a.contract();
        assert_eq!(a.selection_kind(), "text");
        a.contract(); // already innermost
        assert_eq!(a.selection_kind(), "text");
        a.expand();
        assert_eq!(a.selection_kind(), "link");
    }

    #[test]
    fn tier4_expand_from_a_block_selection_widens_past_the_block() {
        let mut a = app();
        a.move_block(1); // the first list item
        assert_eq!(a.selection_kind(), "list-item");
        a.expand();
        let s = a.selection().unwrap();
        // out to the list, which covers both items
        assert_eq!((s.start.line, s.end.line), (3, 4));
    }

    #[test]
    fn tier4_contract_from_a_whole_block_reaches_an_inline_node() {
        let mut a = app();
        a.cursor = Pos::new(6, 6);
        assert_eq!(a.selection_kind(), "paragraph");
        a.contract();
        assert_eq!(a.selection_kind(), "code-span");
    }

    #[test]
    fn tier4_moving_the_cursor_abandons_a_hierarchy_selection() {
        let mut a = app();
        a.cursor = Pos::new(6, 32);
        a.expand();
        assert!(matches!(a.sel, Sel::Region { .. }));
        a.move_line(-1);
        assert!(matches!(a.sel, Sel::Here));
    }

    /// Found by running the TUI: `g` back to the top left the footer still
    /// reading `code L65-102` for a selection that had already been dropped.
    #[test]
    fn moving_away_clears_the_status_left_by_a_region_selection() {
        let mut a = app();
        a.cursor = Pos::new(6, 6);
        a.contract();
        assert!(a.status.contains("code-span"), "{}", a.status);

        a.goto_first();
        assert!(matches!(a.sel, Sel::Here));
        assert!(a.status.is_empty(), "stale status: {}", a.status);
    }

    #[test]
    fn an_anchored_selection_keeps_its_status_while_being_extended() {
        let mut a = app();
        a.move_block(1);
        a.toggle_blocks();
        a.move_block(1);
        assert!(a.status.contains("J/K"), "{}", a.status);
    }

    #[test]
    fn tier4_expand_on_a_blank_line_gives_the_document_without_panicking() {
        let mut a = app();
        a.cursor = Pos::new(2, 1);
        a.expand();
        assert_eq!(a.selection_kind(), "document");
    }

    // ---- unchanged behaviour --------------------------------------------

    #[test]
    fn block_selection_extends_over_a_range() {
        let mut a = app();
        a.move_block(1);
        a.toggle_blocks();
        a.move_block(1);
        let s = a.selection().unwrap();
        assert_eq!((s.start.line, s.end.line), (3, 4));
        assert_eq!(a.selection_kind(), "list-item..list-item");
    }

    #[test]
    fn empty_comment_is_discarded() {
        let mut a = app();
        commit(&mut a, "   ");
        assert!(a.annotations.is_empty());
        assert_eq!(a.mode, Mode::Normal);
    }

    #[test]
    fn cancelling_input_leaves_no_annotation() {
        let mut a = app();
        a.begin_comment();
        a.editor.set("half typed");
        a.cancel_input();
        assert!(a.annotations.is_empty());
    }

    #[test]
    fn removal_targets_the_annotation_under_the_cursor() {
        let mut a = app();
        a.move_block(1);
        commit(&mut a, "one");
        a.move_block(1);
        commit(&mut a, "two");
        a.cursor = Pos::new(3, 1);
        a.remove_at_cursor();
        assert_eq!(a.annotations.len(), 1);
        assert_eq!(a.annotations[0].text, "two");
        a.cursor = Pos::new(1, 1);
        a.remove_at_cursor();
        assert_eq!(a.annotations.len(), 1);
    }

    #[test]
    fn decision_reflects_whether_anything_was_flagged() {
        let mut a = app();
        assert_eq!(a.result().decision, "approved");
        commit(&mut a, "nope");
        assert_eq!(a.result().decision, "changes-requested");
    }

    #[test]
    fn feedback_uses_line_locations_for_whole_lines() {
        let mut a = app();
        a.move_block(1);
        a.toggle_blocks();
        a.move_block(1);
        commit(&mut a, "model layer");
        let md = a.feedback_markdown();
        assert!(md.contains("## PLAN.md:3-4 · list-item..list-item"), "{md}");
        assert!(md.contains("> - [ ] Add validation to the login form"));
    }

    #[test]
    fn feedback_uses_column_locations_for_fragments() {
        let mut a = app();
        a.cursor = Pos::new(6, 6);
        a.contract();
        commit(&mut a, "call it once");
        let md = a.feedback_markdown();
        assert!(md.contains("## PLAN.md:6:5-20 · code-span"), "{md}");
        assert!(md.contains("> `parse_document`"));
    }

    /// A launcher opens a temp file whose absolute path is ~100 characters of
    /// noise the consumer cannot use. The label replaces it everywhere a human
    /// reads it, while `source.path` keeps saying where the bytes came from.
    #[test]
    fn a_label_replaces_the_path_in_feedback_but_not_in_provenance() {
        let mut a = app();
        a.label = Some("assistant-message".into());
        a.cursor = Pos::new(6, 6);
        a.contract();
        commit(&mut a, "call it once");

        let md = a.feedback_markdown();
        assert!(
            md.starts_with("# Review feedback: assistant-message\n"),
            "{md}"
        );
        assert!(
            md.contains("## assistant-message:6:5-20 · code-span"),
            "{md}"
        );
        assert!(
            !md.contains("PLAN.md"),
            "path must not leak into feedback: {md}"
        );

        let json = serde_json::to_string(&a.result()).unwrap();
        assert!(json.contains("\"path\":\"PLAN.md\""), "{json}");
        assert!(json.contains("\"label\":\"assistant-message\""), "{json}");
    }

    /// Without one, nothing about the output changes — including the absence
    /// of the key, so an unlabelled result stays byte-identical to before.
    #[test]
    fn no_label_leaves_the_path_and_omits_the_key() {
        let mut a = app();
        commit(&mut a, "x");
        assert_eq!(a.display_name(), "PLAN.md");
        assert!(a.feedback_markdown().contains("# Review feedback: PLAN.md"));
        let json = serde_json::to_string(&a.result()).unwrap();
        assert!(!json.contains("label"), "{json}");
    }

    #[test]
    fn result_serialises_with_columns() {
        let mut a = app();
        a.cursor = Pos::new(6, 6);
        a.contract();
        commit(&mut a, "x");
        let json = serde_json::to_string(&a.result()).unwrap();
        assert!(json.contains("\"startCol\":5"));
        assert!(json.contains("\"endCol\":20"));
        assert!(json.contains("\"wholeLines\":false"));
    }

    #[test]
    fn an_empty_file_does_not_panic() {
        let mut a = App::new("empty.md".into(), "");
        assert_eq!(a.current_block(), None);
        assert_eq!(a.selection(), None);
        a.begin_comment();
        assert_eq!(a.mode, Mode::Normal);
        a.move_block(1);
        a.move_char(1);
        a.expand();
        a.contract();
        a.remove_at_cursor();
        assert_eq!(a.result().decision, "approved");
    }

    /// `move_line` clamps into `1..=line_count()`, and `Ord::clamp` panics when
    /// min > max. On an empty file `lines` is empty, so the `.max(1)` in
    /// `line_count()` is the only thing keeping that range well-ordered.
    #[test]
    fn vertical_motion_on_an_empty_file_does_not_panic() {
        let mut a = App::new("empty.md".into(), "");
        assert_eq!(a.line_count(), 1, "the clamp upper bound must stay >= 1");

        a.move_line(1);
        a.move_line(-1);
        a.page(1, false);
        a.page(-1, true);
        a.goto_last();
        a.goto_first();
        assert_eq!(a.cursor, Pos::new(1, 1));
    }
}
