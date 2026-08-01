# Aligned tables

Wrapping made a long line readable. It did nothing for a table, which is the
other thing in a markdown document that is unreadable as source — and 22% of
the over-wide lines in the corpus measured for `docs/wrapping.md` are table
rows.

This is the design that shipped, the four it beat, and the two defects a real
terminal found that `TestBackend` could not.

## The constraint everything falls out of

`draw_source`:

```rust
.filter(|&&(a, b, _)| b > s && a < e)
.map(|&(a, b, st)| (a.max(s) - s, b.min(e) - s, st))
```

A row **is** a byte window `[s, e)` into `display_line`, and syntax marks, the
selection and the cursor are all byte ranges that clip into it and shift down.
`wrap_line_ranges_are_ordered_windows_into_the_source_line` is what makes that
exact. Aligning a table means putting cells on screen that are in no file,
which is the one thing a byte window cannot express.

Two things make it less dire than it sounds.

- `display_line` already rewrites the source — `\t` becomes one space
  (`app.rs`) — and survives only because it is one byte for one cell. The
  precedent for a display transform exists; the bar it has to clear is that
  byte offsets stay screen columns.
- **No inverse map is needed.** There is no mouse handling and no `set_cursor`
  anywhere in `src/`; the cursor is drawn as a *mark*, which is a byte range.
  Nothing in this program ever asks "which byte is at screen column 37". That
  removes the harder half of the problem before it starts.

Worth naming: prettier, mdformat, `vim-table-mode` and `column -t` all align
markdown tables by **editing the file**. marginal cannot — the file is the
thing under review — so there is nothing to copy and the design is genuinely
its own.

## The design space

### A — piece list, insertion only (**shipped**)

A row stops being `(usize, usize)` and becomes a sequence:

```rust
enum Piece {
    Src(usize, usize),
    Pad { n: usize, fill: char, anchor: usize },
}
```

Pads go only where markdown already ignores whitespace: inside a cell, against
its `|`.

| | |
|---|---|
| invariant | `concat(Src pieces) == line` — a strict generalisation; today is the zero-pad case |
| monotone? | yes |
| `cursor_row` | scan for the row whose first `Src` starts ≤ `b` — unchanged in shape |
| touches | `line_rows`, the clip-shift block, the `›` width check |
| does **not** touch | `blocks.rs`, `line_marks`, `selected_bytes_on`, `App::snap`, the JSON |

### B — substitution map, virtual text

Render a derived string per row and carry `Vec<(src_range, screen_range)>`.
Buys box-drawing borders and a `─` rule; costs a bidirectional map and screen
cells that map to no byte. Strictly more capable, strictly more machinery, and
unless the goal is glyphs it is A with extra bookkeeping.

### C — cell-space navigation

Inside a table the cursor becomes a **cell index** rather than a column.
`blocks.rs` already emits `table-cell` nodes with source spans, so a selection
is a cell's span and the renderer is unconstrained: box drawing, wrapped cells,
hidden pipes. The only design where "wrap a table" has a good answer — cell
text wraps inside its column.

The cost is a mode discontinuity: `cursor.col` stops meaning a screen position
inside tables, and `w`/`b`/`+`/`-` become cell-wise. Defensible, arguably
better in a table, but a different program.

### D — pre-align into a shadow document with a source map

Feels clean, is how source maps work, and is the one to say no to. Wrapping
already made rows width-dependent and rebuilt per frame, so this invalidates a
position map on every resize while every function in `app.rs` silently belongs
to one of two coordinate spaces.

### E — pretty only in peek

Preserves everything absolutely and answers nothing. A fine complement to a
truncating grid; not an answer to "I want to read the table".

## Why the wrapping interaction picks A

Aligning a table **makes it wider**: `Σ max cell + 3·cols + 1`. The fixture in
`AGENTS.md` already has a 92-column table row against an 87-column body, so
pretty can turn a table that fit into one that does not.

| | keeps grid | keeps monotonicity | needs |
|---|---|---|---|
| **align only if it fits**, else plain wrap unaligned | when it matters | ✓ | A |
| overflow with `›`, never wrap a table row | ✓ | ✓ | A |
| column-group folding (stack groups, repeat header) | ✓ | ✗ | B/C |
| cell wrapping inside column budgets | ✓ | **✗** | C |

The last row is the fault line. With cell wrapping, screen row 1 holds bytes
from cells 1, 2 and 3 and screen row 2 holds *later bytes of cell 1 again* — so
the byte→row map stops ascending, and `cursor_row`, `step_row`/`walk_rows` and
clip-then-shift all quietly assume it does. Folding breaks it too, since a
repeated header is bytes from another line.

**Monotonicity, not padding, is the real constraint.** A and B stay on the safe
side of it. C crosses it deliberately and pays with a new cursor model.

So: **align only if it fits.** A table that fits is aligned and never wraps; a
table that does not is left ragged and wraps like anything else. The two
transforms never compose on one line, which is asserted by
`a_padded_row_is_always_exactly_one_row`.

## What shipped, and where it differs

### The delimiter row needed no substitution after all

The plan expected `|---|` to be the one place insertion fails — you cannot
*pad* `---` to width, you have to rewrite it — and leaned on `blocks.rs:192`
(comrak emits no node for the delimiter row, so nothing can point at it) to
make that a sanctioned exception.

Giving `Pad` a fill character removed the exception entirely. The delimiter row
grows its own rule, dashes inserted after the last `-` so a `:` stays on the
end it marks, and the design is 100% insertion-only with no special case at
all. It also takes no space padding, which is what keeps it from widening the
whole table to make room for gaps it does not want.

### Three independent maxima do not compose

The first `Grid` took, per column, the widest lead / widest content / widest
trail and summed them. That over-pads whenever two rows reach the same width
differently — `| a  |` (content 1, trail 2) against `| -- |` (content 2, trail
1) both occupy 4, but the three maxima demand 5. Symptom: an already-aligned
table got padded, caught by `an_already_aligned_table_is_not_touched`.

The width a column needs has to be *reachable for every row given that row's
own spacing*, or a cell needs negative padding. Each alignment imposes a
different constraint, so each derives its width differently:

- left: every content starts at `lead` → `lead + max(content + trail)`
- right: the mirror → `trail + max(lead + content)`
- centre: nothing is pinned → `max(lead + content + trail)`

Then each cell's opening gap is `want.clamp(own_lead, width - content - own_trail)`,
which is exact centring where it fits and a graceful fallback where it does not.

### A pad has no style, and cannot work out whose to borrow

Both defects found by running it in a tmux pane; neither visible to a
`TestBackend` string, which throws styles away.

A pad sits *at* a byte boundary, not on a byte, so nothing local distinguishes
"a selection starts here, the gap is inside it" from "a selection ends here,
the gap is outside it".

- Taking the style of the byte **before** put the gap in front of a centred
  cell in the pipe's style — a selected cell highlighted on its right half
  only.
- Taking the byte **after the content** swept the closing gap into a selection
  *inside* the cell, so annotating one word lit up the whole column.

`Pad` carries an explicit `anchor`, set to **the nearest byte of its own cell**:
the cell's first byte for the opening gap, its last for the closing one. Both
land inside the cell and outside its content, which is exactly what makes a
selected cell highlight whole while a word inside it does not swell.
`padding_is_highlighted_with_its_cell_and_only_with_its_cell` covers both.

### One switch, not two

`W`/`--no-wrap` became `P`/`--raw`, and wrapping is no longer separately
toggleable. The question the toggle answers is "am I looking at the bytes or at
something readable"; a half-pretty third mode is a state nobody asked for. Off
is also the debugging view — when a column looks wrong, `P` gives back a screen
where every cell is a byte.

## What this does not change

The output contract. Columns are 1-based byte offsets into the *source*, and
`alignment_does_not_reach_the_output_contract` renders the same document with
pretty on and off and asserts `result()` is byte-identical — with the selection
made inside a padded cell, which is where a pad-aware column would be off by
exactly the padding.

Verified in a real tty at 80 and 95 columns: a table aligns at 95, falls back to
ragged-and-wrapped at 80, `P` returns the literal source, and a word rendered
twelve cells right of where it lives still emits `"startCol": 7`.

## Still open

- **The peek overlay does not align.** It calls `wrap()`, which knows nothing
  about tables, so peeking a table row shows the raw row. The layout is
  already computed and per-line; wiring it in is small.
- **Alignment is all-or-nothing per table.** A table one cell too wide loses
  the grid entirely. Dropping to a single space around the pipes would reclaim
  `2·(cols−1)` and is a knob, not a design — but nothing measures how often it
  would help.
- **Column-group folding** is the answer for tables that genuinely cannot fit,
  and it needs B or C. Nothing here forecloses either.
