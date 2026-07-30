# marginal

Annotate markdown in a terminal at whatever granularity you actually mean —
an inline code span, a table row, a range of list items, a whole section — and
emit the result as prose an agent can act on.

POC scope: open a file, navigate, select, comment, emit JSON + feedback
markdown. No launcher, no tmux popup, no gate semantics yet — see `STATUS.md`.

## Run

```sh
nix develop                      # rust toolchain + the checkers behind ./check
cargo run --release -- PLAN.md
```

or without the dev shell:

```sh
nix run . -- PLAN.md
```

## Usage

```
marginal FILE.md [--result PATH]
marginal --dump-blocks FILE.md     # headless: print the navigation units
```

`--result PATH` writes the result JSON. Feedback markdown goes to stdout on
exit. Exit code is `0` when nothing was annotated, `1` when something was, `2`
on tool failure (unreadable file, no tty).

## Keys

| Key | Action |
|---|---|
| `h`/`j`/`k`/`l`, arrows | move by character / line |
| `C-d` / `C-u` | half page down / up |
| `C-f` / `C-b`, PgDn/PgUp | full page down / up (two lines of overlap) |
| `0` / `$` | start / end of line |
| `J` / `K` | move by navigation unit |
| `g` / `G` | first / last line |
| `v` | select a range of units — `J`/`K` extends |
| `V` | select whole lines, ignoring unit boundaries — `j`/`k` extends |
| `+` / `-` | widen / narrow along the markdown hierarchy |
| `c` | comment on the selection |
| `x` | remove the annotation under the cursor |
| `Esc` | drop the selection |
| `q` | quit |

### Writing a comment

Readline bindings, because that is what fingers expect. `Enter` saves, `Esc`
cancels, and **`C-j` inserts a newline** — comments can be several lines.

| Key | Action |
|---|---|
| `C-a` / `C-e` | start / end of line (line-local in a multi-line comment) |
| `C-b` / `C-f`, arrows | back / forward one character |
| `M-b` / `M-f` | back / forward one word |
| `C-d`, Delete | delete character forward |
| `C-h`, Backspace | delete character back |
| `C-w` | delete word back, whitespace-delimited — takes punctuation with it |
| `M-DEL` | delete word back, stopping at punctuation |
| `M-d` | delete word forward |
| `C-k` | kill to end of line; at the end, joins the next line |
| `C-u` | kill to start of line, keeping anything after the cursor |
| `C-p` / `C-n`, Up/Down | recall earlier comments from this session |

There is no kill ring: killed text is gone.

## Selection

Three mechanisms, all resolving to one `(line, column)` span.

**`v` — units.** `J`/`K` steps through *navigation units*: headings,
paragraphs, list items, code blocks, **table rows**, and the inner blocks of a
blockquote. The unit list is flat, gapless and non-overlapping, which is what
makes stepping through it predictable.

**`V` — lines.** Ignores structure entirely. This is what you want for four
lines in the middle of a forty-line fence.

**`+` / `-` — the hierarchy.** Put the cursor anywhere and press `-` to narrow
onto the innermost thing under it, or `+` to widen:

```
text run  →  link  →  paragraph  →  list item  →  list  →  document
```

You never have to point precisely — land near the thing and adjust. `-` from a
paragraph gets you the code span inside it; `+` from a table row gets you the
whole table. Runs of nodes with identical spans collapse, so every press
visibly moves.

## Syntax highlighting

Headings, fences, inline code, links, emphasis, blockquotes, table pipes and
list markers are coloured from **the AST that already exists** — no second
parser, no regexes, no extra dependency. `blocks::parse_tree` knows where every
node is to the byte, so `highlight` turns that into per-line tagged ranges and
`ui` maps tags to colours.

The renderer composites overlapping ranges with later marks winning, so the
layering is: syntax, then selection, then cursor. Highlighting can never hide
where you are or what you have chosen, and `**bold**` inside a blockquote wins
over the quote's own styling.

Code *inside* a fence is not highlighted by language — that would need syntect
or a tree-sitter grammar set. Deliberately out of scope; see `STATUS.md`.

## How positions work

Annotations attach to source spans, and the screen shows **raw source lines** —
so there is no rendered→source mapping layer, because every node already *is* a
span.

Spans come from [comrak](https://github.com/kivikakk/comrak)'s `sourcepos`,
chosen over the alternatives after measuring all of them on one fixture.
Container nodes carry real ranges, markers and fences sit *inside* the range,
ranges are contiguous, and it reaches all the way down to inline nodes:

```
Paragraph   L1:1  - L2:37
  Code      L1:5  - L1:20     ← includes the backticks
  Link      L1:30 - L1:66     ← includes [label](url)
    Text    L1:31 - L1:41     ← just the label
TableRow    L4:1  - L4:9
  TableCell L4:2  - L4:4
```

Four normalisations sit on top, each pinned by a test:

- comrak reports `line:0` as an end position when a block is terminated by the
  next line; that is pulled back onto the last line the block occupies.
- a list item's range spans its nested sublist, so the item is trimmed to end
  where the sublist begins — item and sublist never overlap.
- comrak emits no node for a table's `|---|---|` delimiter row, so each row is
  stretched to just before the next one. The delimiter rides with the header,
  and the unit list stays gapless.
- **columns are 1-based *byte* offsets, not characters.** In `Prüfen \`köde\``
  the backtick is character 8 and comrak calls it column 9. Every slice and
  every render cut is forced onto a character boundary; there is a test that
  deliberately asks for a cut inside `ü` and checks the line survives.

`--dump-blocks` prints the unit table for any file, which is how the parser
gets checked without a terminal.

## Tests and checks

```sh
cargo test
nix develop --command ./check    # everything CI would run
```

`./check` runs, cheapest-first: `cargo fmt --check`, `taplo` on the TOML,
`cargo clippy -D warnings`, the test suite, `typos`, `cargo machete` (unused
deps) and `cargo deny check` (advisories, licenses, source provenance).

Clippy runs with `pedantic` and `nursery` enabled. The exceptions live in
`[lints.clippy]` in `Cargo.toml`, each with a comment saying why — that list is
meant to be argued with, not grown silently.

Two tools are in the dev shell but deliberately out of `./check`, being far too
slow for a pre-commit loop:

```sh
cargo mutants     # do the tests actually catch anything?
bacon             # watch loop
```

95 tests, none requiring a terminal. The UI is covered through ratatui's
`TestBackend`, which renders into an in-memory buffer, so the gutter, partial
line highlighting, multibyte segmentation and scrolling are asserted on rather
than assumed.
