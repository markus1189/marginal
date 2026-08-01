# Line wrapping

Two halves. The first is built and passing `./check`: the wrapper the peek
overlay uses, rewritten to stop losing rows to whitespace and to break where a
reader would. The second is a design, not code: soft wrap in the source view,
which STATUS.md defers because `lineno = scroll + idx + 1` in `draw_source`
stops being invertible.

The first half was written so the second half has something to stand on —
`wrap_line` returns byte ranges precisely because that is what mark rebasing
needs.

## Part 1 — the shared wrapper (built)

### What was wrong

Probed against the old implementation, width in parentheses:

| Input | Old | Now |
|---|---|---|
| `"abcde fghij"` (5) | `["abcde", " ", "fghij"]` | `["abcde", "fghij"]` |
| `"hello "` (5) | `["hello", " "]` | `["hello"]` |
| `"    - a nested item with several words"` (20) | `["    - a nested item ", "with several words"]` | `["    - a nested item", "      with several", "      words"]` |
| `"see https://example.dev/a/very/long/path here"` (12) | `["see ", "https://exam", "ple.dev/a/ve", "ry/long/path", " here"]` | `["see", "https://", "example.dev/", "a/very/long/", "path here"]` |

Three distinct defects, one root cause each:

- **A row spent on the space it broke at.** `split_inclusive(' ')` welds the
  trailing space onto the word, so a word that exactly fills the pane measures
  one cell over, falls into the "wider than the pane" branch, and orphans its
  own space onto the next row. At width 5 a five-letter word produced a blank
  line in the middle of a sentence. This fires at every width — the trigger is
  a word whose length equals the pane, which is common, not exotic.
- **Phantom trailing rows.** Same cause at end of line: `"hello "` produced a
  second row holding one space. `peek_rows` counted it, so `scroll_peek`
  clamped one row too high and `z` would scroll to a blank screen. Markdown's
  hard line break is two trailing spaces, so this was reachable from ordinary
  documents.
- **Continuation at column zero.** The old test asserts `rows[0]` keeps its
  indent and says nothing about the rest, so a wrapped nested list item
  continued at column 0 and read as a new top-level item.

### What replaced it

`wrap_line(line, first, rest) -> Vec<(usize, usize)>` — greedy packing that
reports **byte ranges into the line**, not strings. Three properties:

- Trailing spaces sit outside every range, so they overhang the right edge
  instead of taking a row. Spaces still count toward the width of the *next*
  word, which is what makes the break land in the right place.
- An over-wide word is split at its own break points first — after `/ - _ . ,
  ; : ? & = #`, and between two adjacent wide characters, which is the only
  break CJK offers since it is written without spaces. Cutting mid-token is
  the fallback, not the first move.
- A break with nothing yet placed does not emit a row, so a deep indent in a
  narrow pane no longer opens with a blank line.

`hang_indent(line)` pads continuation rows by the source indentation plus the
width of a list marker or blockquote prefix, capped at half the pane. It lives
in `wrap`, the display layer — **never** in `wrap_line`. Byte ranges that
contained synthetic padding would be useless for part 2.

Ranges, not strings, is the whole point: the source view's syntax and selection
marks are byte ranges within a line, and rebasing a mark onto a row is only
correct if the row *is* a byte window. One primitive, both callers.

### Tests added

Five, all in `ui::tests`:

- `a_break_never_spends_a_row_on_the_space_it_broke_at`
- `trailing_space_does_not_invent_a_row`
- `continuation_rows_line_up_under_the_text_not_the_bullet`
- `an_over_wide_word_breaks_at_its_own_separators_first`
- `wrap_line_ranges_are_ordered_windows_into_the_source_line` — the property
  part 2 stands on: rows are an ascending partition of the line into
  character-aligned byte windows that drop no non-space byte. A mark at byte
  `n` therefore belongs to exactly one row and rebases by subtraction.

132 tests pass; `./check` is green (fmt, taplo, clippy pedantic, typos,
machete, deny).

One deliberate exception, asserted rather than hidden: a character wider than
the entire pane cannot be split, so it goes out alone and overflows by a cell.
The old code emitted an empty row *and* the overflowing character; this emits
only the character.

### Behaviour change to be aware of

Trailing whitespace no longer appears in peek output. It was invisible before
too — two trailing spaces render as two blanks at the end of a row — so this
costs nothing legible, and it is what removes the phantom row. The JSON and
feedback contract is untouched: `peek_text` still returns the exact selection,
and only the *display* of it changed.

## Part 2 — soft wrap in the source view (built)

> Built as designed below, with two simplifications found while building —
> paging moves the cursor rather than the anchor, and the wrapping primitives
> moved to `src/wrap.rs` so `app` could reach them. Both are recorded at the
> end. Verified in a real tty at 80, 95, 120 and 191 columns; the choices
> below are what shipped.


### The measurement

646,286 markdown lines across 3,364 files in `~/Stuff`, character counts, body
width 87 (one body column in a 95-column pane):

| | |
|---|---|
| lines ≤ 87 columns | 83.9% |
| rendered rows if every line wrapped | 844,501 — **1.307× amplification** |
| lines taking ≥ 2 rows | 16.11% |
| lines taking ≥ 4 rows | 2.41% |
| lines taking ≥ 8 rows | 0.47% |
| lines taking ≥ 30 rows | 0.0141% (91 lines) |
| worst single line | **2,476 rows** |

This does not reconcile with STATUS.md's 243,887 lines / 75.7% ≤ 87. Same
directory, different totals, and two days is not enough to add 400k lines —
so the two runs measured different file sets, and I have not worked out which.
Treat the shape as confirmed and the exact percentages as one measurement each.

Two numbers drive the design:

- **1.307× is cheap.** Wrapping does not meaningfully reduce how much document
  fits on screen. The objection to soft wrap is not density.
- **2,476 rows on one line is not cheap.** One line can exceed any viewport by
  two orders of magnitude. Any design where `scroll` remains a *line* index
  cannot scroll *within* that line, so it would be unreachable except through
  peek. This is the constraint that picks the design.

### What actually breaks

Smaller than it looks. Three places, and everything else is keyed by
`(lineno, byte range)` and survives untouched:

- `ui.rs:188` — `for idx in 0..viewport { let lineno = *scroll + idx + 1; }`.
  The only place the invariant *generates* rows.
- `ui.rs:316` — `keep_cursor_visible`, which compares `cursor.line` against
  `scroll` and clamps to `lines.len() - viewport`.
- `app.rs:385` — `App::page`, which moves the cursor by `viewport` lines.

Untouched: `line_marks`, `selected_bytes_on`, `annotations_on`,
`line_selected`, `segments`, `blocks.rs` entirely, and the JSON. Selections are
addressed in `(line, col)` and always will be — wrapping is a rendering
concern and must not reach the output contract. The overflow-marker machinery
(`ui.rs:186`, `241–271`) is deleted when wrap is on and kept when it is off.

### Axis A — what `scroll` becomes

1. **Flat row index over a document-wide row map.** Trivial arithmetic, gives a
   proportional scrollbar for free. Costs a full re-wrap of the document on
   every width change, and a 646k-line document is 844k rows — call it 7 MB of
   `(u32, u32)` and a visible hitch on resize.
2. **An anchor, `(line, subrow)`, wrapping only the viewport.** O(viewport) per
   frame — about 50 lines. Resize costs nothing. No document row total, so no
   proportional scrollbar and no "scroll to 60%".
3. **Anchor plus a lazily filled `Vec<u16>` of per-line row counts**, discarded
   on width change. Option 2 with totals when something asks.

**Recommend 2.** marginal has no scrollbar and no percentage jump; its long
motions are `g`, `G`, `J`/`K` and `w`/`b`, every one of which is addressed by
line or by AST node, not by row. The 2,476-row line costs nothing unless you
are inside it. And resize being free matters more here than in most TUIs: the
pi extension suspends a host TUI to borrow the terminal, and the Claude Code
launcher borrows a tty from tmux — both are ways for the width to change while
marginal is not drawing. Option 3 is the upgrade path if a scrollbar ever
appears; nothing in option 2 forecloses it.

```rust
/// Top of the viewport: a line, and how many of its wrapped rows are above the
/// fold. `row == 0` for every line short enough not to wrap.
struct Anchor { line: usize, row: usize }
```

Scrolling down walks forward wrapping one line at a time until `viewport` rows
are consumed; scrolling up walks backward doing the same. Both are O(rows
moved), never O(document).

### Axis B — what `j`/`k` mean

1. **Source lines** (today). One `j` can move the cursor 2,476 rows.
2. **Display rows.** Cursor stays inside the long line; column changes.
3. **Both**, on different keys.

**Recommend 3, `j`/`k` staying line-wise.** `V` extends a line selection with
`j`/`k`; if those moved by display row, `V` would silently stop tracking
selected lines. Keeping `j` line-wise also means a pathological line is never a
trap — one keypress leaves it regardless of how tall it is, which removes the
need for a per-line row cap.

The binding for display-row motion is the awkward part: `g` is already first
line, so Vim's `gj`/`gk` would require `g` to become a prefix and `gg` to
become first line — a breaking rebind. Options, in order of preference:

- **`C-n` / `C-p`** — unbound in normal mode today, no breaking change. Costs
  a keybinding that does not resemble the Vim it otherwise imitates.
- **Arrows** — split arrows (display rows) from `j`/`k` (source lines). Reads
  naturally: arrows are what the screen does, `j`/`k` are what the document
  does. Breaks the documented "arrows alias `j`/`k`".
- **`gg` migration** — most Vim-correct, most disruptive.

### Axis C — the gutter on continuation rows

The gutter is a separate layout column already (`ui.rs:179`), so this is purely
what to put in it. Line number on the first row of a line only, blank
after — a repeated number reads as a duplicate line. The selection bar `▍`
belongs on **every** row of a selected line: it marks the line as selected and
the line is still selected halfway down. The annotation dot `●` belongs on the
first row only, matching the existing reasoning at `ui.rs:213` that the dot
must not be the thing that gets lost.

### Axis D — mark rebasing

Solved by part 1. For a row spanning `[s, e)` of a line:

```rust
let row_marks: Vec<(usize, usize, Style)> = marks
    .iter()
    .filter(|(a, b, _)| *b > s && *a < e)
    .map(|(a, b, st)| (a.saturating_sub(s), (*b).min(e) - s, *st))
    .collect();
let spans = segments(&line[s..e], &row_marks, Style::default());
```

Clip, then shift. The ordered-windows test is what guarantees a mark cannot
land in two rows or none.

The cursor needs the inverse — given `cursor.col`, which row holds it — which
is a scan over the same ranges. `keep_cursor_visible` then works unchanged, on
rows instead of lines, exactly as STATUS.md predicted.

### Axis E — paging

With wrap on, `App::page` must move by rows or it overshoots by the
amplification factor. `page` currently moves the cursor and lets the renderer
follow. The clean version moves the *anchor* by `viewport` rows and puts the
cursor on the line that lands at the top. Note this inverts the existing
cursor-first relationship and will need
`paging_moves_by_half_and_whole_viewports` reworked — it is the one existing
test this design breaks.

### Axis F — toggle

`--wrap` plus a runtime key. `W` is free and mnemonic (`w` is the inline
motion, so the capital avoids the collision). Off by default for the POC:
truncation is the behaviour every existing test and screenshot assumes, and a
toggle lets the two be compared on the same document rather than argued about.

### What actually shipped, and where it differs

The plan above survived, with two changes:

- **Paging moves the cursor, not the anchor.** Axis E proposed moving the
  anchor by a viewport of rows and putting the cursor at the top, and predicted
  `paging_moves_by_half_and_whole_viewports` would need reworking. Walking the
  *cursor* by rows and letting `keep_cursor_visible` follow turned out simpler,
  kept the cursor-first relationship the rest of the program has, and left that
  test untouched — the headless tests run at `wrap_width == 0`, which is also
  how wrapping is switched off, so they exercise the unchanged path.
- **`src/wrap.rs`.** The primitives had to leave `ui.rs`: `App::move_row` and
  `App::page` need row space, and `app` deliberately holds no ratatui types
  beyond this one module boundary.

The toggle is `W` (`w` is the inline motion) and the flag is `--no-wrap`, since
wrapping ships **on**.

Verified in a real tty via the tmux recipe in AGENTS.md, at 80, 95, 120 and 191
columns: a 3,497-column line renders across 20 rows, `W` returns exactly the old
truncated screen, `k` clears a 20-row line in one press, `C-n` walks inside it,
and an annotation on that line reports `endCol: 3497` — the source column, from
a line no row of which is 3,497 cells wide.

### Still open

- The annotations pane is a fixed six rows and does not scroll; the comment
  editor caps at eight and does not scroll either. Neither wraps. Same
  primitive would serve both.
- No proportional scrollbar, by choice — Axis A option 3 is the upgrade path if
  one is ever wanted, and nothing here forecloses it.

### What this does not change

The output contract. Wrapping is a rendering decision; `(line, col)` byte
offsets in the JSON and the feedback markdown are computed from the source and
must stay identical with wrap on and off. Worth a test asserting exactly that:
same document, same selection, wrap toggled, byte-identical `result()`.
