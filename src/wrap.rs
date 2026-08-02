//! Line wrapping.
//!
//! `wrap_line` is the primitive and reports **byte ranges**, not strings,
//! because both callers need different things from the same break decisions:
//! the peek overlay wants text, and the source view wants to rebase its
//! syntax and selection marks — which are byte ranges within a line — onto
//! each row. Rebasing is only correct if a row is a byte window in the first
//! place, so ranges are the shared truth and strings are derived from them.
//!
//! Hand-rolled rather than `Paragraph::wrap` because callers need the exact
//! row count to clamp a scroll and ratatui exposes no stable way to ask;
//! `Wrap { trim: true }` also eats the indentation that makes nested
//! markdown readable.
//!
//! # Two measures of width, and the rule for picking one
//!
//! ratatui measures a string with two functions that do not agree, and every
//! width in this crate is one of them. Which one is not a matter of taste:
//!
//! - [`cells_claimed`] is `Span::width()`, which is plain `unicode-width`. It
//!   is what `Line::width()` sums, so it is the number ratatui uses when a
//!   string's own width **decides how big an area is**.
//! - [`cells_drawn`] is `str::cell_width`, which is `unicode-width` plus one
//!   column per halfwidth katakana dakuten or handakuten (`U+FF9E`/`U+FF9F`).
//!   `unicode-width` calls those zero-width and terminals draw them as a column
//!   of their own. It is what `LineTruncator` and `WordWrapper` advance by and
//!   what `Buffer::diff` reads a cell's occupancy with, so it is the number
//!   that says **what reaches the screen**.
//!
//! `ｶﾞ` (`U+FF76 U+FF9E`) is the whole of the disagreement: one grapheme
//! cluster, `cells_claimed` 1, `cells_drawn` 2, and two columns on the glass.
//!
//! A new call site picks between them by asking one question — **where does the
//! width on the other side of the comparison come from?**
//!
//! - **From outside the string**: a pane, a table column, a `Rect` that was
//!   already fixed when the string arrived. ratatui will lay the string into
//!   that area and cut what does not fit, and it cuts by [`cells_drawn`]. Any
//!   other measure over-fills the area, and the overflow is dropped silently —
//!   there is no mark and no error, the characters simply never appear.
//! - **From the string itself**: a block title, whose `Rect`
//!   `Block::render_left_titles` sizes at `Line::width()`. Then
//!   [`cells_claimed`] is the *outer* of the two measures and the one that
//!   decides whether the string is given an area at all. Being stricter than it
//!   buys nothing, because the rect the string is handed is derived the same
//!   short way whatever budget the caller picked. `452b2c8` established this
//!   for `shorten_path`; it is not an oversight to be tidied up.
//!
//! The short form: **fit it to a pane with [`cells_drawn`], size a title with
//! [`cells_claimed`]**.

use ratatui::buffer::CellWidth as _;
use ratatui::text::Span;

/// Cells `s` *claims*: `Span::width()`, the number `Line::width()` sums and the
/// one a block title's `Rect` is sized from. See the module doc for when this is
/// the measure a call site wants — most of the time it is not.
pub fn cells_claimed(s: &str) -> usize {
    Span::raw(s).width()
}

/// Cells `s` is *drawn* in: `str::cell_width`, the number ratatui lays out and
/// truncates by. The measure for anything being fitted into a pane whose width
/// was decided elsewhere. See the module doc.
pub fn cells_drawn(s: &str) -> usize {
    // `str::cell_width` fast-paths a one-byte string to 1, behind a
    // `debug_assert!` that the byte is not an ASCII control — a guard for
    // callers who were meant to have filtered controls out already. The callers
    // here cannot: they measure the file under review one character at a time,
    // `source_lines` ends a line only at `\r` or `\n`, and `display_line`
    // substitutes only `\t`, so the other thirty ASCII control bytes reach this
    // function intact. Taking the fast path here is the same answer `cell_width`
    // gives — 1 for any one-byte string, control or not — without the panic a
    // debug build would otherwise take on a file with a stray `\x0c` in it.
    if s.len() == 1 {
        return 1;
    }
    usize::from(s.cell_width())
}

/// One piece of a rendered row.
///
/// A row used to be a single byte range. It is a sequence now because pretty
/// mode aligns tables by *inserting* padding — the one thing on screen that no
/// byte of the source accounts for. Everything else is unchanged: the `Src`
/// pieces of a row still concatenate to a window of the line, in order,
/// dropping nothing, which is what lets `draw_source` rebase a mark by clipping
/// it and `cursor_row` find a byte's row by scanning starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Piece {
    /// Bytes `[start, end)` of the line, rendered as they are.
    Src(usize, usize),
    /// `n` cells of `fill`, from nowhere. Having no byte of its own it has no
    /// style of its own either, so it names the byte to borrow one from:
    /// whichever side of the gap the padding belongs to.
    Pad { n: usize, fill: char, anchor: usize },
}

/// One screen line. Without pretty mode every row is exactly one `Src`, which
/// is the degenerate case the wrapper alone produces.
pub type Row = Vec<Piece>;

/// First source byte the row shows.
pub fn row_start(row: &Row) -> usize {
    row.iter()
        .find_map(|p| match *p {
            Piece::Src(s, _) => Some(s),
            Piece::Pad { .. } => None,
        })
        .unwrap_or(0)
}

/// One past the last source byte the row shows.
pub fn row_end(row: &Row) -> usize {
    row.iter()
        .rev()
        .find_map(|p| match *p {
            Piece::Src(_, e) => Some(e),
            Piece::Pad { .. } => None,
        })
        .unwrap_or(0)
}

/// Word-wrap to `width` cells, preserving each source line's own break and its
/// leading indentation. Hand-rolled rather than `Paragraph::wrap` because the
/// overlay needs the exact row count to clamp its scroll, and ratatui exposes
/// no stable way to ask; `Wrap { trim: true }` also eats the indentation that
/// makes nested markdown readable.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for line in text.split('\n') {
        // Half the pane is the cap: past that a deep indent costs more than the
        // alignment it buys, and `rest` must stay >= 1 for any width.
        let indent = hang_indent(line).min(width / 2);
        let pad = " ".repeat(indent);
        for (i, (s, e)) in wrap_line(line, width, width - indent)
            .into_iter()
            .enumerate()
        {
            if i == 0 {
                out.push(line[s..e].to_string());
            } else {
                out.push(format!("{pad}{}", &line[s..e]));
            }
        }
    }
    out
}

/// The rows one source line occupies at `width` cells, with the hanging indent
/// its continuation rows are padded by.
///
/// `width == 0` means "do not wrap": one row covering the whole line, which is
/// what the source view renders when pretty mode is off and what keeps every
/// row-addressed motion working unchanged in that mode.
pub fn wrap_source(line: &str, width: usize) -> (Vec<Row>, usize) {
    if width == 0 {
        return (vec![vec![Piece::Src(0, line.len())]], 0);
    }
    let indent = hang_indent(line).min(width / 2);
    let rows = wrap_line(line, width, width - indent)
        .into_iter()
        .map(|(s, e)| vec![Piece::Src(s, e)])
        .collect();
    (rows, indent)
}

/// Cells a continuation row is padded by, so a wrapped line keeps the shape of
/// the construct it belongs to: the source indentation, plus the width of a
/// list marker or blockquote prefix so text lines up under text rather than
/// under the bullet. Display only — `wrap_line` reports byte ranges that never
/// contain padding, so nothing derived from them can drift.
pub fn hang_indent(line: &str) -> usize {
    // Spaces and `>` are one byte and one cell each, so bytes are cells here.
    let n = line.len() - line.trim_start_matches([' ', '>']).len();
    let rest = &line[n..];
    let spaces = |s: &str| s.len() - s.trim_start_matches(' ').len();

    if let Some(r) = rest.strip_prefix(['-', '*', '+']) {
        if r.starts_with(' ') {
            return n + 1 + spaces(r);
        }
    }
    let d = rest.chars().take_while(char::is_ascii_digit).count();
    if d > 0 {
        if let Some(r) = rest[d..].strip_prefix(['.', ')']) {
            if r.starts_with(' ') {
                return n + d + 1 + spaces(r);
            }
        }
    }
    n
}

/// A word and the run of spaces that follows it, as byte offsets:
/// `(word start, word end, cells of trailing space)`.
fn units(line: &str) -> Vec<(usize, usize, usize)> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < line.len() {
        let ws = i;
        while i < line.len() && b[i] != b' ' {
            i += 1;
            while i < line.len() && !line.is_char_boundary(i) {
                i += 1;
            }
        }
        let we = i;
        while i < line.len() && b[i] == b' ' {
            i += 1;
        }
        out.push((ws, we, i - we));
    }
    out
}

/// Byte length of the longest prefix of `s` that fits in `limit` cells of pane.
/// Always a char boundary: it is accumulated a character at a time, never by
/// slicing at a computed byte.
///
/// `limit` is a pane, so the measure is [`cells_drawn`]. Charging it per `char`
/// is exact for the pair the two measures disagree about — [`cells_drawn`] adds
/// its column per *occurrence* of `U+FF9E`/`U+FF9F`, so a halfwidth katakana and
/// its dakuten cost 1 apiece whether they are measured together or apart, and
/// the pane is not over-filled either way. It is **not** exact for a sequence whose
/// width is a property of the sequence: `"✔\u{FE0F}"` is two cells whole and one
/// summed per char, the under-count `452b2c8` removed from `shorten_path`. That
/// gap survives here and is not this function's to close on its own.
fn prefix_cells(s: &str, limit: usize) -> usize {
    let mut buf = [0u8; 4];
    let (mut n, mut w) = (0usize, 0usize);
    for ch in s.chars() {
        let cw = cells_drawn(ch.encode_utf8(&mut buf));
        if w + cw > limit {
            break;
        }
        w += cw;
        n += ch.len_utf8();
    }
    n
}

/// Byte offsets inside `word` where a break is acceptable without a space:
/// after a URL or path separator, and between two adjacent wide characters,
/// which is the only break CJK offers since it is written without spaces.
///
/// "Wide" is [`cells_drawn`] because the question is about the screen, and the
/// answer has to be the one the row-packing loop below will act on. Halfwidth
/// katakana is not wide under either measure, so a dakuten run offers no break
/// point of its own and falls to the hard cut — which is the path that has to
/// charge the right number of cells.
fn break_points(word: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut buf = [0u8; 4];
    let mut prev_wide = false;
    for (i, ch) in word.char_indices() {
        let wide = cells_drawn(ch.encode_utf8(&mut buf)) > 1;
        if wide && prev_wide && i > 0 {
            out.push(i);
        }
        prev_wide = wide;
        if matches!(
            ch,
            '/' | '-' | '_' | '.' | ',' | ';' | ':' | '?' | '&' | '=' | '#'
        ) {
            let after = i + ch.len_utf8();
            if after < word.len() {
                out.push(after);
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Greedy-pack one source line into rows of at most `first` cells for the first
/// row and `rest` for the others, reported as byte ranges into `line`.
///
/// Ranges, not strings, because this is also the shape the source view needs:
/// its syntax and selection marks are byte ranges within the line, and rebasing
/// them onto a row is only correct if the row is a byte window in the first
/// place. Trailing spaces are left outside every range so they overhang the
/// edge instead of taking a row of their own.
pub fn wrap_line(line: &str, first: usize, rest: usize) -> Vec<(usize, usize)> {
    if first == 0 || rest == 0 {
        return Vec::new();
    }
    let units = units(line);
    if units.is_empty() {
        return vec![(0, 0)];
    }

    let mut rows: Vec<(usize, usize)> = Vec::new();
    let (mut start, mut cur_end, mut w, mut limit) = (0usize, 0usize, 0usize, first);
    macro_rules! brk {
        ($at:expr) => {
            if cur_end > start {
                rows.push((start, cur_end));
                limit = rest;
                start = $at;
                cur_end = $at;
                w = 0;
            } else {
                // Nothing placed yet. Only the leading-indentation unit has zero
                // width, so `start..$at` is this line's indentation. A row of
                // pure indentation is not worth a row — but moving `start` past
                // those bytes leaves them outside every row, and the source view
                // rebases a mark by clipping it into a row's window, so an
                // uncovered byte is a mark that renders nowhere and a cursor
                // that cannot be found. Keep them at the head of the row that
                // follows instead, and count their cells so it still fits.
                // Indentation wide enough to fill the pane on its own cannot
                // share, so it spills into rows of its own first; only the
                // remainder rides along with the content.
                let mut p = start;
                while cells_drawn(&line[p..$at]) >= limit {
                    let n = prefix_cells(&line[p..$at], limit);
                    if n == 0 {
                        break;
                    }
                    rows.push((p, p + n));
                    limit = rest;
                    p += n;
                }
                start = p;
                cur_end = p;
                w = cells_drawn(&line[p..$at]);
            }
        };
    }

    for (s, e, sp) in units {
        let ww = cells_drawn(&line[s..e]);
        if w > 0 && w + ww > limit {
            brk!(s);
        }
        // `w` is not always 0 here: a break that kept the indentation carries its
        // cells forward, and the split below has to account for them.
        if w + ww > limit {
            // Wider than the pane on its own. Prefer its own break points; only
            // cut mid-token when even those leave something too wide.
            let mut prev = s;
            let stops = break_points(&line[s..e])
                .into_iter()
                .map(|b| s + b)
                .chain(std::iter::once(e));
            for bp in stops {
                let sw = cells_drawn(&line[prev..bp]);
                if w > 0 && w + sw > limit {
                    brk!(prev);
                }
                // `w + sw`, not `sw`: the `brk!` above is a no-op when nothing
                // has been placed, so `w` still holds the indentation it kept.
                // Testing the segment alone let `sw <= limit < w + sw` fall to
                // the `else` and pack `indent + sw` cells into a `limit` pane.
                if w + sw > limit {
                    let mut j = prev;
                    let mut buf = [0u8; 4];
                    for ch in line[prev..bp].chars() {
                        let cw = cells_drawn(ch.encode_utf8(&mut buf));
                        // Not `brk!`: the loop sets `cur_end` itself on every
                        // character, so seeding it here would be a dead store.
                        if w > 0 && w + cw > limit {
                            if j > start {
                                rows.push((start, j));
                                limit = rest;
                            }
                            start = j;
                            w = 0;
                        }
                        w += cw;
                        j += ch.len_utf8();
                        cur_end = j;
                    }
                } else {
                    w += sw;
                    cur_end = bp;
                }
                prev = bp;
            }
        } else {
            w += ww;
            cur_end = e;
        }
        w += sp;
    }
    rows.push((start, cur_end));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two measures, and the single character class that separates them.
    /// Anything that makes them agree everywhere has deleted the distinction the
    /// module doc is about.
    #[test]
    fn the_two_measures_part_company_at_the_halfwidth_sound_marks() {
        // Agreement is the common case: ASCII, East-Asian-Wide, combining
        // marks, and the *combining* dakuten U+3099, which ratatui leaves alone.
        for s in [
            "",
            "abc",
            "日本語",
            "e\u{301}",
            "ｶ\u{3099}",
            "ガ",
            "✔\u{FE0F}",
        ] {
            assert_eq!(
                cells_claimed(s),
                cells_drawn(s),
                "the two measures should agree on {s:?}"
            );
        }
        // And the exception, in both spellings ratatui adjusts for.
        for (s, claimed, drawn) in [("ｶﾞ", 1, 2), ("ﾊﾟ", 1, 2), ("あﾞ", 2, 3)] {
            assert_eq!(cells_claimed(s), claimed, "claimed width of {s:?}");
            assert_eq!(cells_drawn(s), drawn, "drawn width of {s:?}");
        }
    }

    /// `cells_drawn` is `str::cell_width` with its one-byte fast path taken
    /// here instead, because that path is guarded by a `debug_assert!` against
    /// ASCII control bytes and this crate cannot promise there are none: the
    /// file under review is whatever the user opened, `source_lines` ends a line
    /// only at `\r` or `\n`, and `display_line` substitutes only `\t`. Every
    /// other control byte arrives intact at the per-character loops in
    /// `prefix_cells`, `break_points` and `wrap_line`'s hard cut.
    ///
    /// So: the whole of ASCII, one byte at a time, must answer rather than
    /// panic — and must answer the same 1 the fast path would have.
    #[test]
    fn cells_drawn_answers_for_a_control_byte_rather_than_asserting_on_it() {
        for b in 0u8..=0x7f {
            let mut buf = [0u8; 4];
            let s = char::from(b).encode_utf8(&mut buf);
            assert_eq!(cells_drawn(s), 1, "one-byte {b:#04x}");
        }
        // The same bytes reaching the wrapper, which is how they get here.
        let line: String = (0u8..0x20)
            .filter(|&b| b != b'\r' && b != b'\n')
            .map(char::from)
            .collect();
        let rows = wrap_line(&line, 4, 4);
        let kept: String = rows.iter().map(|&(s, e)| &line[s..e]).collect();
        assert_eq!(kept, line, "control bytes dropped by the wrapper");
    }

    /// Half of every row of a halfwidth-katakana line never reached the screen.
    /// `wrap_line` measured with `unicode-width`, which calls `U+FF9E`
    /// zero-width, so it packed 34 clusters into a 34-cell pane — 68 drawn
    /// cells — and ratatui's `LineTruncator`, which advances by `cell_width`,
    /// stopped at 34. The other 17 clusters were dropped with no marker, and the
    /// row count was halved with them, so the anchor, the cursor row and the
    /// paging step were all short on such a line too.
    #[test]
    fn a_wrapped_row_fits_the_pane_in_the_cells_the_terminal_draws() {
        let line = "ｶﾞ".repeat(200);
        // The headline number first: 200 clusters at two cells each is 400
        // cells, which is 12 rows of 34 — not the 6 the claimed measure asked
        // for, and `row_count` is what the anchor, the cursor row and the paging
        // step are all counted in.
        assert_eq!(
            wrap_line(&line, 34, 34).len(),
            12,
            "200 two-cell clusters do not fit in 6 rows of 34 cells"
        );
        for width in 1usize..=60 {
            let rows = wrap_line(&line, width, width);
            for &(s, e) in &rows {
                let seg = &line[s..e];
                assert!(
                    cells_drawn(seg) <= width || seg.chars().count() == 1,
                    "width {width}: a row of {} drawn cells in a {width}-cell pane",
                    cells_drawn(seg)
                );
            }
            let kept: String = rows.iter().map(|&(s, e)| &line[s..e]).collect();
            assert_eq!(kept, line, "dropped bytes at width {width}");
        }
    }

    #[test]
    fn wrap_preserves_every_character_and_the_indentation() {
        for width in [1usize, 3, 12, 40] {
            let text = "    - a nested item with several words\nand a second line";
            let rows = wrap(text, width);
            let rebuilt: String = rows.join("").replace(' ', "");
            assert_eq!(rebuilt, text.replace([' ', '\n'], ""), "width {width}");
            assert!(
                rows[0].starts_with(' ') || width < 4,
                "lost indent at {width}"
            );
            for r in &rows {
                assert!(
                    cells_drawn(r) <= width || width == 0,
                    "row over width: {r:?}"
                );
            }
        }
    }

    /// The word before the break used to consume the whole pane, which sent its
    /// own trailing space onto a row by itself — a blank line in the middle of
    /// a sentence.
    #[test]
    fn a_break_never_spends_a_row_on_the_space_it_broke_at() {
        assert_eq!(wrap("abcde fghij", 5), ["abcde", "fghij"]);
        assert!(!wrap("the quick brown fox jumps over the lazy dog", 9)
            .iter()
            .any(|r| r.trim().is_empty()));
    }

    /// Trailing whitespace overhangs the edge rather than taking a row, so the
    /// row count the overlay scrolls against matches the rows it can show.
    #[test]
    fn trailing_space_does_not_invent_a_row() {
        assert_eq!(wrap("hello ", 5), ["hello"]);
        assert_eq!(wrap("ab  ", 5), ["ab"]);
        assert_eq!(wrap("a\n\nb", 5), ["a", "", "b"]);
    }

    /// A wrapped list item used to continue at column 0, which read as a new
    /// top-level item rather than the rest of the one above it.
    #[test]
    fn continuation_rows_line_up_under_the_text_not_the_bullet() {
        let rows = wrap("    - a nested item with several words", 20);
        assert_eq!(rows[0], "    - a nested item");
        for r in &rows[1..] {
            assert!(r.starts_with("      "), "{r:?}");
            assert!(cells_drawn(r) <= 20, "{r:?}");
        }
        assert_eq!(wrap("12. ordered item here", 12)[1], "    item");
        assert_eq!(wrap("> quoted text that runs on", 12)[1], "  text that");
    }

    /// A URL is one word, so it can only break inside itself. At a separator it
    /// stays readable; mid-token it does not.
    #[test]
    fn an_over_wide_word_breaks_at_its_own_separators_first() {
        let rows = wrap("see https://example.dev/a/very/long/path here", 12);
        assert!(rows.iter().any(|r| r.ends_with('/')), "{rows:?}");
        for r in &rows {
            assert!(cells_drawn(r) <= 12, "{r:?}");
        }
        // No separator to break at: cutting mid-token is still the fallback.
        assert!(wrap(&"x".repeat(30), 10).len() >= 3);
    }

    /// The property the source view will stand on: rows are a partition of the
    /// line into ascending byte windows, so a mark at byte `n` belongs to
    /// exactly one row and rebases onto it by subtraction.
    #[test]
    fn wrap_line_ranges_are_ordered_windows_into_the_source_line() {
        for line in [
            "    - a nested item with several words",
            "see https://example.dev/a/very/long/path here",
            "日本語のテキストはここで折り返す",
            "trailing spaces   ",
            "",
        ] {
            for width in [1usize, 4, 12, 40] {
                let rows = wrap_line(line, width, width);
                // Every byte before the first row's start is a byte in no row.
                assert_eq!(rows[0].0, 0, "uncovered head in {line:?} at {width}");
                let mut prev = 0;
                for &(s, e) in &rows {
                    assert!(s <= e && e <= line.len(), "{line:?} {width} {s}..{e}");
                    assert!(s >= prev, "rows go backwards in {line:?} at {width}");
                    assert!(line.is_char_boundary(s) && line.is_char_boundary(e));
                    // A character wider than the whole pane cannot be split, so
                    // it goes out alone and overflows by design.
                    assert!(
                        cells_drawn(&line[s..e]) <= width || line[s..e].chars().count() == 1,
                        "over width: {:?}",
                        &line[s..e]
                    );
                    prev = e;
                }
                // Every non-space byte lands in exactly one row.
                let kept: String = rows.iter().map(|&(s, e)| &line[s..e]).collect();
                assert_eq!(
                    kept.replace(' ', ""),
                    line.replace(' ', ""),
                    "dropped text in {line:?} at {width}"
                );
            }
        }
    }

    /// A break with nothing placed yet used to move `start` past the line's
    /// leading indentation without emitting a row for it, so those bytes were in
    /// no row at all. Three things read that gap: the pretty view drew a nested
    /// continuation flush against the gutter, `draw_source` rebased a mark on
    /// those bytes onto nothing, and `move_row` subtracted a row start larger
    /// than the cursor byte.
    #[test]
    fn an_over_wide_first_word_keeps_the_lines_indentation() {
        // Every byte lands in a row, and every row fits its own pane: `first`
        // for row 0, `rest` below it.
        let fits = |line: &str, first: usize, rest: usize| {
            let rows = wrap_line(line, first, rest);
            assert_eq!(
                rows[0].0, 0,
                "uncovered indent, {line:?} at {first}: {rows:?}"
            );
            let kept: String = rows.iter().map(|&(s, e)| &line[s..e]).collect();
            assert_eq!(kept, line, "dropped bytes, {line:?} at {first}");
            for (i, &(s, e)) in rows.iter().enumerate() {
                let limit = if i == 0 { first } else { rest };
                assert!(
                    cells_drawn(&line[s..e]) <= limit,
                    "over width, {line:?} at {first}/{rest}: row {i} is {:?}",
                    &line[s..e]
                );
            }
        };

        let line = "        https://example.com/some/long/path";
        // 9 to 13 is the window the wider widths hide. The break that kept the
        // eight cells of indentation carries them in `w`, and `https:` is six
        // cells: the segment fits the pane on its own, the sum does not. Below
        // 9 the indentation spills into rows of its own and stops being
        // carried, so only this band exercises the sum.
        for width in [9usize, 10, 11, 12, 13, 20, 34, 42, 48] {
            fits(line, width, width);
        }
        assert!(wrap(line, 48)[0].starts_with("        "), "lost the indent");

        // A word with no break point of its own: the hard cut has to charge the
        // carried indentation too, or it puts thirteen cells in an eight pane.
        fits("     abcdefgh", 8, 8);
        // And it charges cells, not characters: the wide char that will not fit
        // behind `  x` starts a row instead of overflowing one.
        fits("  x本", 3, 2);
    }

    /// The price of charging the carried indentation: when not even the first
    /// character fits behind it, the indentation goes out on a row of its own —
    /// the row `brk!` calls "not worth a row". It is still worth more than the
    /// alternatives, which are an over-wide row or bytes in no row at all, and
    /// the spill loop already emits rows of this shape for a wide indent.
    #[test]
    fn indentation_that_cannot_share_with_even_one_character_takes_a_row() {
        assert_eq!(wrap_line("  本", 3, 3), [(0, 2), (2, 5)]);
    }

    /// Indentation too wide to share a row still has to land in one.
    #[test]
    fn indentation_wider_than_the_pane_spills_into_its_own_rows() {
        let line = "          x";
        let rows = wrap_line(line, 4, 4);
        assert_eq!(rows[0].0, 0);
        let kept: String = rows.iter().map(|&(s, e)| &line[s..e]).collect();
        assert_eq!(kept, line);
        for &(s, e) in &rows {
            assert!(
                cells_drawn(&line[s..e]) <= 4,
                "over width: {:?}",
                &line[s..e]
            );
        }
    }

    #[test]
    fn wrap_hard_cuts_a_word_wider_than_the_pane() {
        let rows = wrap("see https://example.dev/a/very/long/path/indeed here", 12);
        assert!(rows.len() > 3, "{rows:?}");
        for r in &rows {
            assert!(cells_drawn(r) <= 12, "{r:?}");
        }
    }

    #[test]
    fn wrap_survives_multibyte_and_a_zero_width_pane() {
        assert!(wrap("Prüfen köde — ✓ fertig", 0).is_empty());
        let rows = wrap("Prüfen köde — ✓ fertig", 7);
        for r in &rows {
            assert!(cells_drawn(r) <= 7, "{r:?}");
        }
    }
}
