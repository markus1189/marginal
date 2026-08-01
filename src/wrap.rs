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

use ratatui::text::Span;

/// Display width of `s` in terminal cells.
pub fn cells(s: &str) -> usize {
    Span::raw(s).width()
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

/// The rows one source line occupies at `width` cells — byte ranges into the
/// line — with the hanging indent its continuation rows are padded by.
///
/// `width == 0` means "do not wrap": one row covering the whole line, which is
/// what the source view renders when wrapping is off and what keeps every
/// row-addressed motion working unchanged in that mode.
pub fn wrap_source(line: &str, width: usize) -> (Vec<(usize, usize)>, usize) {
    if width == 0 {
        return (vec![(0, line.len())], 0);
    }
    let indent = hang_indent(line).min(width / 2);
    (wrap_line(line, width, width - indent), indent)
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

/// Byte offsets inside `word` where a break is acceptable without a space:
/// after a URL or path separator, and between two adjacent wide characters,
/// which is the only break CJK offers since it is written without spaces.
fn break_points(word: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut buf = [0u8; 4];
    let mut prev_wide = false;
    for (i, ch) in word.char_indices() {
        let wide = cells(ch.encode_utf8(&mut buf)) > 1;
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
    // A break with nothing placed yet would emit a row of pure indentation.
    macro_rules! brk {
        ($at:expr) => {
            if cur_end > start {
                rows.push((start, cur_end));
                limit = rest;
            }
            start = $at;
            cur_end = $at;
            w = 0;
        };
    }

    for (s, e, sp) in units {
        let ww = cells(&line[s..e]);
        if w > 0 && w + ww > limit {
            brk!(s);
        }
        if ww > limit {
            // Wider than the pane on its own. Prefer its own break points; only
            // cut mid-token when even those leave something too wide.
            let mut prev = s;
            let stops = break_points(&line[s..e])
                .into_iter()
                .map(|b| s + b)
                .chain(std::iter::once(e));
            for bp in stops {
                let sw = cells(&line[prev..bp]);
                if w > 0 && w + sw > limit {
                    brk!(prev);
                }
                if sw > limit {
                    let mut j = prev;
                    let mut buf = [0u8; 4];
                    for ch in line[prev..bp].chars() {
                        let cw = cells(ch.encode_utf8(&mut buf));
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
                assert!(cells(r) <= width || width == 0, "row over width: {r:?}");
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
            assert!(cells(r) <= 20, "{r:?}");
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
            assert!(cells(r) <= 12, "{r:?}");
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
                let mut prev = 0;
                for &(s, e) in &rows {
                    assert!(s <= e && e <= line.len(), "{line:?} {width} {s}..{e}");
                    assert!(s >= prev, "rows go backwards in {line:?} at {width}");
                    assert!(line.is_char_boundary(s) && line.is_char_boundary(e));
                    // A character wider than the whole pane cannot be split, so
                    // it goes out alone and overflows by design.
                    assert!(
                        cells(&line[s..e]) <= width || line[s..e].chars().count() == 1,
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

    #[test]
    fn wrap_hard_cuts_a_word_wider_than_the_pane() {
        let rows = wrap("see https://example.dev/a/very/long/path/indeed here", 12);
        assert!(rows.len() > 3, "{rows:?}");
        for r in &rows {
            assert!(cells(r) <= 12, "{r:?}");
        }
    }

    #[test]
    fn wrap_survives_multibyte_and_a_zero_width_pane() {
        assert!(wrap("Prüfen köde — ✓ fertig", 0).is_empty());
        let rows = wrap("Prüfen köde — ✓ fertig", 7);
        for r in &rows {
            assert!(cells(r) <= 7, "{r:?}");
        }
    }
}
