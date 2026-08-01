//! Table alignment: the padding that makes a markdown table's columns line up.
//!
//! **Insertion only.** Every pad is spaces — or dashes, in the delimiter row —
//! added *between* source bytes. Nothing is moved, hidden or rewritten, so a
//! rendered row is still the source line in order and the byte ranges that
//! address a selection survive untouched. That is the whole reason this is
//! padding rather than a reflow: `draw_source` rebases syntax, selection and
//! cursor marks by clipping them into a row, and clipping is exact only while
//! the row *is* the line.
//!
//! Two consequences worth knowing:
//!
//! * Aligning a table makes it **wider**. A table whose aligned width does not
//!   fit the pane is left ragged, because a grid sheared by a line wrap is
//!   worse to read than a ragged one. So an aligned row never wraps and a
//!   wrapped row is never padded — the two never have to compose.
//! * Every other tool that aligns markdown tables — prettier, mdformat,
//!   vim-table-mode — does it by editing the file. marginal cannot: the file is
//!   the thing under review.

use crate::blocks::Block;
use crate::wrap::{cells, Piece, Row};

/// Cells the screen shows that no byte of the file accounts for: `n` of `fill`,
/// spliced in at byte `at` of the source line.
///
/// `anchor` is the byte it takes its style from, and it is **the nearest byte
/// of the cell the gap was opened in** — the cell's first byte for the opening
/// gap, its last for the closing one. Not a detail the renderer can work out:
/// a pad sits *at* a byte boundary rather than on a byte, so nothing local
/// distinguishes "a selection starts here, the gap is inside it" from "a
/// selection ends here, the gap is outside it". Only the code that opened the
/// gap knows which cell it belongs to.
///
/// Both anchors landing inside the cell and outside its content is what makes
/// a selected *cell* highlight whole while a selection *within* a cell — a
/// word, a code span — does not swell to the width of the column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pad {
    at: usize,
    n: usize,
    fill: char,
    anchor: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Align {
    Left,
    Right,
    Center,
}

/// One cell of one row, as byte offsets into its line: the run of leading
/// spaces, the trimmed content, the run of trailing spaces. `closed` says
/// whether a `|` follows it — the last cell of a row that ends without one has
/// no pipe to line up against, so its trailing run is left alone.
#[derive(Debug, Clone, Copy)]
struct Cell {
    lead: usize,
    cs: usize,
    ce: usize,
    trail: usize,
    closed: bool,
}

/// The column grid one table settles on.
///
/// A cell is a gap, its content, and a gap. Only insertion is available, so
/// both gaps can grow and neither can shrink: `width` is what the cell must
/// reach, and `lead`/`trail` are the gap the column agrees on for whichever
/// side its alignment pins the content to.
///
/// `width` is not simply the widest content. It has to be reachable *for every
/// row* given that row's own spacing, or a cell would need negative padding —
/// so each alignment derives it from the constraint it actually imposes:
///
/// * left: every content starts at `lead`, so `lead + max(content + trail)`
/// * right: every content ends `trail` from the pipe, so the mirror of that
/// * centre: nothing is pinned, so the widest cell as it stands
///
/// Widening the content alone would line the pipes up and leave the text
/// ragged inside them whenever rows disagree about separator spacing — `|a|`
/// against `| bb |`. Pinning a gap is what lines up both.
struct Grid {
    aligns: Vec<Align>,
    lead: Vec<usize>,
    trail: Vec<usize>,
    width: Vec<usize>,
}

#[derive(Clone)]
struct Padding {
    /// Cells the whole table occupies once padded. Below this the table is left
    /// alone — see the module header.
    width: usize,
    pads: Vec<Pad>,
}

/// Every table in the document, flattened to per-line padding.
///
/// Computed once, in `App::new`: the column widths depend on all of a table's
/// rows but on nothing about the terminal, so the only thing the pane width
/// decides is whether to use them.
pub struct Tables {
    /// Indexed by `line - 1`. `None` for every line that is not a table row
    /// needing padding.
    lines: Vec<Option<Padding>>,
}

impl Tables {
    pub fn new(lines: &[String], blocks: &[Block]) -> Self {
        let mut out = vec![None; lines.len()];
        for (from, to) in extents(blocks) {
            // Tabs become one space each before anything measures a column, the
            // same substitution `App::display_line` makes and for the same
            // reason: one byte for one cell keeps byte offsets and screen
            // columns the same number.
            let texts: Vec<String> = (from..=to)
                .filter_map(|l| lines.get(l - 1))
                .map(|t| t.replace('\t', " "))
                .collect();
            if texts.len() != to + 1 - from {
                continue;
            }
            let Some(padding) = align(&texts) else {
                continue;
            };
            for (i, p) in padding.into_iter().enumerate() {
                out[from - 1 + i] = p;
            }
        }
        Self { lines: out }
    }

    /// The padding that aligns `line` in a body `width` cells wide, or `None`
    /// when the line is not in a table, the table is already aligned, or
    /// aligning it would push the table past the right edge.
    pub fn pads(&self, line: usize, width: usize) -> Option<&[Pad]> {
        let p = self.lines.get(line.checked_sub(1)?)?.as_ref()?;
        (width > 0 && p.width <= width).then_some(p.pads.as_slice())
    }
}

/// The one row a padded line renders to: source bytes with the alignment
/// padding spliced in. The `Src` pieces still concatenate to the whole line, in
/// order, dropping nothing — which is the property `draw_source` rebases marks
/// against and `pretty_rows_concatenate_to_the_source_line` asserts.
pub fn row(len: usize, pads: &[Pad]) -> Row {
    let mut out = Vec::with_capacity(pads.len() * 2 + 1);
    let mut at = 0;
    for p in pads {
        if p.at > at {
            out.push(Piece::Src(at, p.at));
        }
        out.push(Piece::Pad {
            n: p.n,
            fill: p.fill,
            anchor: p.anchor,
        });
        at = p.at;
    }
    if at < len || out.is_empty() {
        out.push(Piece::Src(at, len));
    }
    out
}

/// Line ranges of the tables in the document, from the flat block list rather
/// than by looking for `|---|`: a delimiter row inside a fenced code block is
/// text, and comrak is the only thing that knows the difference.
///
/// A table's rows are line-contiguous — `blocks.rs` stretches the header row
/// over the delimiter row precisely so there is no gap — so a break in the
/// numbering is a break between two tables.
fn extents(blocks: &[Block]) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    for b in blocks.iter().filter(|b| b.kind == "table-row") {
        match out.last_mut() {
            Some(last) if last.1 + 1 == b.start() => last.1 = b.end(),
            _ => out.push((b.start(), b.end())),
        }
    }
    out
}

/// Byte offsets of the `|` that separate cells. A backslash escapes the next
/// character, which is the only way GFM lets a pipe be content.
///
/// Scanning bytes is safe here: `\` and `|` are ASCII, and no byte of a
/// multi-byte UTF-8 sequence is ever below `0x80`.
fn bars(line: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut esc = false;
    for (i, c) in line.bytes().enumerate() {
        if esc {
            esc = false;
        } else if c == b'\\' {
            esc = true;
        } else if c == b'|' {
            out.push(i);
        }
    }
    out
}

fn cell(line: &str, a: usize, b: usize, closed: bool) -> Cell {
    let inner = &line[a..b];
    let end = inner.trim_end_matches(' ');
    // Spaces after the last cell of a row that never closes are trailing
    // whitespace on the line, not a gap before a pipe. Nothing lines up against
    // them, so they are not part of any column.
    let trail = |n: usize| if closed { n } else { 0 };
    if end.is_empty() {
        // All spaces: no content to sit between two gaps, so it is all one gap.
        // Which one is arbitrary — the cell reaches the same width either way.
        return Cell {
            lead: 0,
            cs: a,
            ce: a,
            trail: trail(b - a),
            closed,
        };
    }
    let lead = inner.len() - inner.trim_start_matches(' ').len();
    Cell {
        lead,
        cs: a + lead,
        ce: a + end.len(),
        trail: trail(b - (a + end.len())),
        closed,
    }
}

fn row_cells(line: &str) -> Vec<Cell> {
    let bars = bars(line);
    let Some((&first, &last)) = bars.first().zip(bars.last()) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(bars.len() + 1);
    // Text before the first bar is a column only when the row opens without
    // one. `  | a |` has indentation, not a first cell.
    if !line[..first].trim().is_empty() {
        out.push(cell(line, 0, first, true));
    }
    for w in bars.windows(2) {
        out.push(cell(line, w[0] + 1, w[1], true));
    }
    if !line[last + 1..].trim().is_empty() {
        out.push(cell(line, last + 1, line.len(), false));
    }
    out
}

/// `---`, `:---`, `---:` or `:---:`, and nothing else.
fn delim_align(text: &str) -> Option<Align> {
    let s = text.trim();
    let left = s.starts_with(':');
    let s = s.strip_prefix(':').unwrap_or(s);
    let right = s.ends_with(':');
    let s = s.strip_suffix(':').unwrap_or(s);
    if s.is_empty() || !s.bytes().all(|b| b == b'-') {
        return None;
    }
    Some(match (left, right) {
        (true, true) => Align::Center,
        (false, true) => Align::Right,
        _ => Align::Left,
    })
}

fn is_delimiter(text: &str, row: &[Cell]) -> bool {
    !row.is_empty() && row.iter().all(|c| delim_align(&text[c.cs..c.ce]).is_some())
}

/// Padding for one line, given the grid its table settled on.
///
/// `Grid::width` is built so every subtraction below stays non-negative for
/// every row of the table. `saturating_sub` anyway: if a future edit gets the
/// two out of step, a column renders wrong rather than panicking in the middle
/// of someone's review.
fn pads_for(text: &str, cells_of: &[Cell], delim: bool, g: &Grid) -> Vec<Pad> {
    let mut out = Vec::new();
    let mut push = |at: usize, n: usize, fill: char, anchor: usize| {
        if n > 0 {
            out.push(Pad {
                at,
                n,
                fill,
                anchor,
            });
        }
    };
    for (j, c) in cells_of.iter().enumerate().take(g.aligns.len()) {
        let (w, cw) = (g.width[j], cells(&text[c.cs..c.ce]));
        // The two ends of the cell, which is where both of its gaps take their
        // style from — see `Pad`.
        let (head, tail) = (c.cs - c.lead, (c.ce + c.trail).saturating_sub(1));
        if delim {
            // The delimiter row takes no spaces: it grows its own rule, so the
            // dash run extends and a `:` stays on the end it marks. Filling it
            // with the column's gaps instead would break the table's one
            // horizontal line, and would widen the whole table to make room.
            let at = text[c.cs..c.ce].rfind('-').map_or(c.ce, |i| c.cs + i + 1);
            let n = w.saturating_sub(c.lead + cw + c.trail);
            push(at, n, '-', at.saturating_sub(1));
            continue;
        }
        // Where the content would ideally start, then pulled back inside what
        // this row's own spacing already commits to: a gap can only grow.
        let want = match g.aligns[j] {
            Align::Left => g.lead[j],
            Align::Right => w.saturating_sub(cw + g.trail[j]),
            Align::Center => w.saturating_sub(cw) / 2,
        };
        let lead = want.clamp(c.lead, w.saturating_sub(cw + c.trail));
        push(head, lead - c.lead, ' ', head);
        if c.closed {
            push(c.ce, w.saturating_sub(cw + lead + c.trail), ' ', tail);
        }
    }
    out
}

/// The padding for every line of one table, or `None` when there is nothing to
/// align — no delimiter row, or a table already laid out by hand.
fn align(texts: &[String]) -> Option<Vec<Option<Padding>>> {
    let rows: Vec<Vec<Cell>> = texts.iter().map(|t| row_cells(t)).collect();
    let delim = rows
        .iter()
        .enumerate()
        .position(|(i, r)| is_delimiter(&texts[i], r))?;

    let aligns: Vec<Align> = rows[delim]
        .iter()
        .map(|c| delim_align(&texts[delim][c.cs..c.ce]).unwrap_or(Align::Left))
        .collect();
    if aligns.is_empty() {
        return None;
    }

    let mut g = Grid {
        lead: vec![0; aligns.len()],
        trail: vec![0; aligns.len()],
        width: vec![0; aligns.len()],
        aligns,
    };
    // Two passes: the pinned gap is a maximum over the column, and the width
    // every cell must reach depends on it.
    for (i, r) in rows.iter().enumerate() {
        for (j, c) in r.iter().enumerate().take(g.aligns.len()) {
            g.lead[j] = g.lead[j].max(c.lead);
            g.trail[j] = g.trail[j].max(c.trail);
            let cw = cells(&texts[i][c.cs..c.ce]);
            g.width[j] = g.width[j].max(match g.aligns[j] {
                Align::Left => cw + c.trail,
                Align::Right => c.lead + cw,
                Align::Center => c.lead + cw + c.trail,
            });
        }
    }
    for (j, a) in g.aligns.iter().enumerate() {
        g.width[j] += match a {
            Align::Left => g.lead[j],
            Align::Right => g.trail[j],
            Align::Center => 0,
        };
    }

    let pads: Vec<Vec<Pad>> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| pads_for(&texts[i], r, i == delim, &g))
        .collect();
    if pads.iter().all(Vec::is_empty) {
        return None;
    }

    // One width for the whole table: a row is only aligned if every row is, so
    // the decision cannot be taken a row at a time.
    let width = pads
        .iter()
        .zip(texts)
        .map(|(p, t)| cells(t.trim_end()) + p.iter().map(|p| p.n).sum::<usize>())
        .max()
        .unwrap_or(0);

    Some(
        pads.into_iter()
            .map(|pads| (!pads.is_empty()).then_some(Padding { width, pads }))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks;

    /// Render a document's tables the way the source view would, at `width`.
    fn render(src: &str, width: usize) -> Vec<String> {
        let lines: Vec<String> = src.lines().map(ToString::to_string).collect();
        let tables = Tables::new(&lines, &blocks::parse(src));
        lines
            .iter()
            .enumerate()
            .map(|(i, text)| match tables.pads(i + 1, width) {
                None => text.clone(),
                Some(pads) => row(text.len(), pads)
                    .iter()
                    .map(|p| match *p {
                        Piece::Src(s, e) => text[s..e].to_string(),
                        Piece::Pad { n, fill, .. } => fill.to_string().repeat(n),
                    })
                    .collect(),
            })
            .collect()
    }

    const RAGGED: &str = "\
| id | description | ok |
|---|:---:|---|
| 1 | short | y |
| 22 | a much longer description | n |
";

    #[test]
    fn a_table_that_fits_lines_its_pipes_up() {
        let out = render(RAGGED, 80);
        let bars: Vec<Vec<usize>> = out
            .iter()
            .filter(|l| l.contains('|'))
            .map(|l| super::bars(l))
            .collect();
        assert!(bars.windows(2).all(|w| w[0] == w[1]), "{out:#?}");
    }

    /// Aligning widens a table. Past the pane it would have to wrap, and a
    /// grid cut by a wrap is worse than a ragged one — so it is left alone.
    #[test]
    fn a_table_too_wide_to_align_is_left_ragged() {
        assert_eq!(render(RAGGED, 20), RAGGED.lines().collect::<Vec<_>>());
        // The same table one cell under its aligned width is still left alone.
        let need = render(RAGGED, 80)[0].chars().count();
        assert_eq!(render(RAGGED, need - 1), RAGGED.lines().collect::<Vec<_>>());
        assert_ne!(render(RAGGED, need), RAGGED.lines().collect::<Vec<_>>());
    }

    /// Spaces in the rule would read as a hole in the table's one horizontal
    /// line, so the delimiter row grows dashes instead.
    #[test]
    fn the_delimiter_row_is_padded_with_its_own_rule() {
        let rule = &render(RAGGED, 80)[1];
        assert!(!rule.contains("- "), "{rule}");
        assert!(rule.contains(":---"), "{rule}");
        assert!(rule.ends_with("---|"), "{rule}");
    }

    /// Padding to the widest *content* alone lines the pipes up and leaves the
    /// text ragged inside them. Each of the three runs gets its own maximum.
    #[test]
    fn rows_that_disagree_about_separator_spacing_still_line_up() {
        let out = render("|a|bb|\n|---|---|\n| ccc | d |\n", 40);
        let at = |l: &String, s: &str| l.find(s).unwrap();
        assert_eq!(at(&out[0], "a"), at(&out[2], "ccc"));
        assert_eq!(at(&out[0], "bb"), at(&out[2], "d"));
    }

    #[test]
    fn the_delimiter_row_decides_which_way_a_column_is_padded() {
        let out = render("| a | a | a |\n|---|--:|:-:|\n| xxx | xxx | xxx |\n", 40);
        // left: content at the left edge; right: at the right; centre: neither.
        assert!(out[0].starts_with("| a   |"), "{}", out[0]);
        assert!(out[0].contains("|   a |"), "{}", out[0]);
        assert!(out[0].contains("|  a  |"), "{}", out[0]);
    }

    /// A table laid out by hand needs no padding at all, and must not be given
    /// a padded row that says otherwise.
    #[test]
    fn an_already_aligned_table_is_not_touched() {
        let src = "| a  | bb |\n| -- | -- |\n| cc | d  |\n";
        let lines: Vec<String> = src.lines().map(ToString::to_string).collect();
        let tables = Tables::new(&lines, &blocks::parse(src));
        assert!((1..=3).all(|l| tables.pads(l, 80).is_none()));
    }

    /// `|---|` inside a fence is text. The extents come from comrak, which is
    /// the only thing that knows the difference.
    #[test]
    fn a_delimiter_row_inside_a_code_fence_is_not_a_table() {
        let src = "```\n| a | b |\n|---|---|\n| c | dddddd |\n```\n";
        let lines: Vec<String> = src.lines().map(ToString::to_string).collect();
        let tables = Tables::new(&lines, &blocks::parse(src));
        assert!((1..=5).all(|l| tables.pads(l, 80).is_none()));
    }

    /// An escaped pipe is content, not a column boundary.
    #[test]
    fn an_escaped_pipe_does_not_open_a_column() {
        assert_eq!(bars(r"| a \| b | c |"), [0, 9, 13]);
    }

    /// Rows are allowed to be short, and a short row must not drag the columns
    /// it does not reach out of line.
    #[test]
    fn a_row_with_too_few_cells_is_padded_as_far_as_it_goes() {
        let out = render("| a | b |\n|---|---|\n| cccc |\n| e | ffff |\n", 40);
        assert_eq!(super::bars(&out[2]), super::bars(&out[3])[..2]);
    }
}
