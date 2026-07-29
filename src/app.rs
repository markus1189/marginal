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

use serde::Serialize;

use crate::blocks::{self, Block, Pos, Span, TreeNode};

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
    pub lines: Vec<String>,
    pub blocks: Vec<Block>,
    pub tree: TreeNode,
    pub cursor: Pos,
    pub sel: Sel,
    pub annotations: Vec<Annotation>,
    pub mode: Mode,
    pub input: String,
    pub status: String,
    pub quit: bool,
    next_id: usize,
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
        let lines: Vec<String> = src.lines().map(|l| l.to_string()).collect();
        let blocks = blocks::parse(src);
        let tree = blocks::parse_tree(src);
        let cursor = blocks
            .first()
            .map(|b| b.span.start)
            .unwrap_or(Pos::new(1, 1));
        App {
            path,
            lines,
            blocks,
            tree,
            cursor,
            sel: Sel::Here,
            annotations: Vec::new(),
            mode: Mode::Normal,
            input: String::new(),
            status: String::new(),
            quit: false,
            next_id: 1,
        }
    }

    pub fn line_count(&self) -> usize {
        self.lines.len().max(1)
    }

    pub fn line_text(&self, line: usize) -> &str {
        self.lines
            .get(line.saturating_sub(1))
            .map(|s| s.as_str())
            .unwrap_or("")
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
                let (a, b) = (
                    anchor.min(self.cursor.line),
                    anchor.max(self.cursor.line),
                );
                Some(Span {
                    start: Pos::new(a, 1),
                    end: Pos::new(b, self.line_len(b).max(1)),
                })
            }
            Sel::Region { depth } => {
                let stack = self.stack();
                stack.get(depth.min(stack.len().saturating_sub(1))).map(|(_, s)| *s)
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
        self.sel = match self.sel {
            Sel::Blocks { .. } => {
                self.status = "selection cleared".into();
                Sel::Here
            }
            _ => {
                self.status = "block selection — J/K to extend".into();
                Sel::Blocks {
                    anchor: self.current_block().unwrap_or(0),
                }
            }
        };
    }

    pub fn toggle_lines(&mut self) {
        self.sel = match self.sel {
            Sel::Lines { .. } => {
                self.status = "selection cleared".into();
                Sel::Here
            }
            _ => {
                self.status = "line selection — j/k to extend".into();
                Sel::Lines {
                    anchor: self.cursor.line,
                }
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
            format!("{} L{}:{}-{}", kind, span.start.line, span.start.col, span.end.col)
        } else {
            format!("{} L{}-{}", kind, span.start.line, span.end.line)
        };
    }

    // ---- movement -------------------------------------------------------

    /// Moving the cursor abandons a hierarchy selection — the stack it was an
    /// index into no longer applies. Block- and line-wise selections are
    /// anchored, so movement extends them instead.
    fn drop_region(&mut self) {
        if matches!(self.sel, Sel::Region { .. }) {
            self.sel = Sel::Here;
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
        let Some(cur) = self.current_block() else { return };
        let target = (cur as isize + delta).clamp(0, self.blocks.len() as isize - 1) as usize;
        self.cursor = self.blocks[target].span.start;
        self.snap();
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

    // ---- annotating -----------------------------------------------------

    pub fn begin_comment(&mut self) {
        if self.selection().is_none() {
            self.status = "nothing to annotate".into();
            return;
        }
        self.mode = Mode::Input;
        self.input.clear();
    }

    pub fn commit_comment(&mut self) {
        let text = self.input.trim().to_string();
        self.mode = Mode::Normal;
        self.input.clear();
        if text.is_empty() {
            self.status = "empty comment discarded".into();
            return;
        }
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
        self.input.clear();
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
    fn slice(&self, span: Span) -> String {
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

    fn loc(&self, a: &Annotation) -> String {
        if a.whole_lines {
            if a.start_line == a.end_line {
                format!("{}:{}", self.path, a.start_line)
            } else {
                format!("{}:{}-{}", self.path, a.start_line, a.end_line)
            }
        } else if a.start_line == a.end_line {
            format!(
                "{}:{}:{}-{}",
                self.path, a.start_line, a.start_col, a.end_col
            )
        } else {
            format!(
                "{}:{}:{}-{}:{}",
                self.path, a.start_line, a.start_col, a.end_line, a.end_col
            )
        }
    }

    pub fn feedback_markdown(&self) -> String {
        if self.annotations.is_empty() {
            return String::new();
        }
        let mut out = format!("# Review feedback: {}\n", self.path);
        for a in &self.annotations {
            out.push_str(&format!("\n## {} · {}\n", self.loc(a), a.block_kind));
            for l in a.original_text.lines() {
                out.push_str(&format!("> {}\n", l));
            }
            out.push_str(&format!("\n{}\n", a.text));
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
        a.input = text.into();
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
        let kinds: Vec<_> = a.blocks.iter().map(|b| (b.kind, b.start(), b.end())).collect();
        assert_eq!(
            kinds,
            vec![("table-row", 1, 2), ("table-row", 3, 3), ("table-row", 4, 4)]
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
        a.input = "half typed".into();
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
}
