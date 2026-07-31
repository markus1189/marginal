# Status

## In this POC

- comrak-based structure extraction in two views: a flat list of navigation
  units, and the full containment hierarchy down to inline nodes
- `(line, column)` spans throughout, byte-safe on multibyte input
- source view with gutter, cursor, partial-line selection highlighting,
  scrolling
- three selection mechanisms: unit ranges (`v`), line ranges (`V`), and
  expand/contract along the hierarchy (`+`/`-`)
- inline motions (`w`/`b`) and a wrapped read-only peek overlay (`z`), which
  together make a line wider than the pane navigable without a sideways scroll
- markdown syntax highlighting driven by the same AST, no extra dependency
- multi-line comment editor with readline bindings and per-session history
- paging keys (`C-d`/`C-u`/`C-f`/`C-b`, PgDn/PgUp)
- comments on any selection, removal, result JSON + feedback markdown,
  exit `0` / `1` / `2`
- no test requires a terminal (UI covered via ratatui `TestBackend`)

## Dependency diet

`comrak`'s `default = ["cli", "syntect-onig", "bon"]` pulled an entire
command-line app (clap, shell-words, xdg, fmt2io) plus syntect and the
oniguruma **C** library — none of it used. Dropping it with
`default-features = false` is worth roughly a third of the dependency graph and
half the binary.

Measured 2026-07-30 — `cargo tree --prefix none | awk '{print $1}' | sort -u |
wc -l` reports **93** crates, and the release binary is **2,079,608 bytes**.
The pre-diet figures recorded on 2026-07-29 (142 crates, 4.01 MB) are kept only
as the reason for the change; they are not reproducible from this tree.

## Highlighting code inside fences

Not done, and it is the one thing the AST cannot provide — comrak sees a fence
body as opaque text. Options, ranked, if it ever matters:

1. **syntect** — ~100 syntaxes and themes. Costs back the 38 crates and ~1.9 MB
   above, plus oniguruma; the `regex-fancy` feature trades speed for pure Rust.
   comrak's `CodeBlock` carries the info string, so each fence can be
   highlighted independently and syntect's cross-line parser state — normally
   the painful part in a scrolling viewport — never comes up.
2. **two-face** — syntect plus bat's extra syntaxes/themes. Only worth it after
   syntect proves it lacks a language you need.
3. **inkjet / syntastica** (tree-sitter) — best quality, one grammar crate per
   language and a build that dwarfs the rest of this program.

Put it behind a cargo feature so the lean build stays the default.

### Granularity tiers, as scoped

- **tier 1** — tables navigate by row, blockquotes by inner block, plus
  line-wise selection. Done.
- **tier 3** — columns in the cursor, the selection, the rendering, the JSON
  and the feedback locations. Done.
- **tier 4** — expand/contract on the AST. Done.
- **tier 5** — semantic `w`/`b` motions between inline nodes. Done, and it
  turned out to be the answer to long lines rather than a nicety: see below.
- **tier 2** (heading-scoped sections) — not built.

## Not built yet

- **the general launcher** — the piece that relocates the TUI onto a tty the
  agent does not own. The pi extension (`.pi/extensions/marginal-annotate.ts`,
  see the README) is not one: it suspends a host TUI that already holds the
  terminal. The Claude Code launcher (`launchers/claude-code/marginal-last`) is
  half of one: it borrows a tty from tmux, from a caller that has none, which is
  the hard half — but it borrows from *this* human's tmux server. An agent with
  no tmux and no `DISPLAY` still has nowhere to put the screen — see the open
  question below.
- `--gate`, `--stdin`, `$EDITOR` escalation, deletion annotations, global
  comments, approve-with-notes
- **horizontal scrolling** — deliberately, now. See below.
- **soft wrap.** The obvious alternative, and the real fix for prose. It costs
  an explicit rendered-row → `(line, byte offset)` map, because `lineno =
  scroll + idx + 1` in `draw_source` stops being invertible. Worth doing behind
  a toggle if the peek overlay turns out not to be enough; `keep_cursor_visible`
  then works unchanged, on rows instead of lines, and there is still only one
  scrolling axis. (tui-textarea has refused word wrap for three years for
  exactly this reason: *"it assumes height is equal to number of lines"*.)
- the annotations pane is a fixed six rows and does not scroll; the comment
  editor caps at eight rows and does not scroll either, so the caret vanishes
  in a longer comment.

## Why long lines are not solved by scrolling sideways

Measured over 243,887 markdown lines in `~/Stuff`: **75.7% are ≤87 columns**,
the width of one body column in a 95-column pane. Of the 24.3% that are longer,
73% are prose, 22% are tables, 3% URLs. Set a horizontal offset to 87 and three
quarters of the screen becomes line numbers over blank space; the longest line
in the corpus is 215,364 columns, so the axis has no sensible maximum either.

Markdown *table source* is not column-aligned, so the header row is useless as a
ruler even while it is still on screen — panning right cuts diagonally through
the table. And `blocks.rs` already makes a table navigate by **row**.

The deciding argument is that the column axis is irrelevant to three of the four
selection modes: `Sel::Here`, `Sel::Blocks` and `Sel::Lines` never read
`cursor.col`. Only `Sel::Region` (`+`/`-`) does, and what it needs is the cursor
*inside* a node — which is a motion, not a viewport offset. Hence `w`/`b`.

Every implementation surveyed (ratatui, Helix, Kakoune, edtui, less, lazygit)
makes soft wrap and horizontal scroll mutually exclusive; none ships both.

If it is built anyway: split the gutter into its own layout column (done) and
use `Paragraph::scroll((0, x))` rather than slicing by hand — it skips by
`cell_width` over already-styled graphemes, so no mark rebasing is needed. Its
one defect is that odd offsets round down *per line* with wide characters, so
two rows can show different source columns at screen column 0.

## Tabs render as one space each

Found while checking the above, and it had nothing to do with long lines.
ratatui's `Span::styled_graphemes` filters control characters, so a tab was
*deleted* rather than expanded: `a\tb\tc END` — 9 bytes — rendered as `abc END`,
and every column past a tab was off by one on screen while the JSON reported the
true byte column. Silent, and precisely the kind of error this tool exists not to
make.

`draw_source` now substitutes one space per tab. Not a tab stop: one byte for
one cell keeps the column on screen identical to the column in the output, which
is worth more here than visually correct indentation. Covered by a test.

## Found by running it

On 2026-07-30 the TUI was driven for the first time, in a 95×54 tmux pane via
`tmux send-keys`. A full green suite had missed four things, three now fixed:

- **stale status line** — `drop_region` cleared the selection but not the
  status, so after moving away the footer kept advertising `code L65-102` for a
  selection that no longer existed. No test moved the cursor *after* a region
  selection and re-rendered. Fixed; `app.rs` now covers it.
- **title bar was 100% path** — the path came first and unbounded, pushing the
  line/unit/annotation counts and the live selection readout past the border.
  The only field that changes on every keypress was the first casualty. Fixed:
  selection first, path last and elided from the left.
- **footer truncated mid-word** — the ~105-column hint line was cut at 95,
  losing `c comment · x remove · q quit`. Fixed: five hint variants, narrowest
  keeps `q quit`, and the status column yields before the keys do.
- **`C-c` did nothing** — raw mode delivers it as a keystroke, no `SIGINT` is
  raised, and the `if ctrl` arm matched only `d`/`u`/`f`/`b`. `q` was the sole
  exit. Now bound to quit, and to cancel in the comment editor.

Also confirmed on a real terminal rather than a `TestBackend` buffer: syntax
highlighting emits the expected SGR (dim blue `##`, bold cyan heading text),
and the `--result` JSON round-trips a column-precise annotation
(`code-span`, `L5:5-20`, `wholeLines: false`).

Driven again on 2026-07-30 for the long-line work, 90×30 and 80×20, on a fixture
with a 248-column paragraph and a 92-column table row. Three defects were already
in the suite by then; the run found one more:

- **orphaned truncation markers under the peek overlay** — the overlay is inset
  by two columns, so the `›` markers were painted into the strip of source still
  visible beside it, detached from any line. Exactly the "is this scrolled or is
  this short" ambiguity the marker exists to remove. Suppressed while peeking;
  `ui.rs` now covers it.

Confirmed working on the real terminal, none of it visible to a `TestBackend`
string: the marker takes SGR `1;38;5;0;48;5;3` — bold black on yellow, the
cursor's own style — when the cursor is off the right edge; `w` walks from
column 1 to the code span at column 176 with `code-span L5:176` in the status;
`-` then narrows to `[code-span L5:176-186]` and the emitted JSON carries
`"originalText": "`code_span`"` for a span 90 columns past the edge of the pane.

## Verified, not assumed

| | |
|---|---|
| flake build | `nix build .#marginal` → store path |
| 5500-line file, 2500 units | parsed in 4 ms |
| real plan file | 89 units (66 before tables and quotes were split) |
| no tty | exits `2` with a message, no panic |
| unreadable file | exits `2` |
| `--dump-blocks \| head` | no broken-pipe panic |
| clippy | clean at `--all-targets` |

## Design notes worth keeping

- **Columns are bytes.** comrak reports column 9 for a backtick that is
  character 8 in `Prüfen \`köde\``. Every consumer floors or ceils to a
  character boundary; a test asks for a cut inside `ü` on purpose.
- **Identical spans collapse, outermost label wins.** In a one-paragraph file
  the text run, the paragraph and the document share a span. Collapsing keeps
  expansion visible; keeping the *outer* label stops that span being reported
  as a "text run" when it is the whole document.
- **Moving the cursor drops a hierarchy selection.** `Sel::Region` is an index
  into a stack computed at the cursor, so it stops meaning anything once the
  cursor moves. Unit and line selections are anchored, so movement extends them
  instead.

## Open question this POC does not answer

Whether it should exist. A GUI Emacs frame reached via `emacsclient -c` is a
review surface this machine already has, it needs no tty relocation, and the
approve/reject gate is already bound in the user's config (`C-c C-c` /
`C-c C-k` via `with-editor`). Measured facts that bear on it:

- `emacsclient -c FILE` blocks a no-tty caller and returns when the client's
  last buffer is done. Needs `DISPLAY`; `emacsclient -t` cannot work without a
  tty at all.
- Emacs can only produce exit `0` or `1` — `2` must come from a wrapper.
- `tmux display-popup` allows one popup per client; a second concurrent caller
  gets `rc=0` having never run. Any launcher must treat `0` as approved *only*
  when a well-formed result file exists.

What the finer selection work does change: `+`/`-` on the markdown hierarchy is
something an Emacs buffer does not give you for free. That is now a real
argument for the terminal version rather than a hypothetical one.

One data point since: the pi extension needs none of the above. A host that is
itself a terminal program can hand its tty over and take it back, and a result
file read after the fact makes the exit-code ambiguity moot. It answers nothing
about the headless case, which is the hard one.

A second data point, which does bear on it: the Claude Code launcher runs from a
tool call with captured stdio, no tty and no way to suspend anything, and the
tmux popup carries it anyway. Measured, on that path:

- `display-popup -E` blocks the caller for the full run (3 s sleep → 3.02 s
  wall) and propagates the inner exit status unchanged — `0`/`1`/`2`/`3` all
  arrive intact, so the wrapper the Emacs route needs is unnecessary here.
- `$TMUX` and `$TMUX_PANE` survive into the tool call, so the popup lands on the
  client already showing the agent — no client hunting.
- A session with **no attached client** still runs the command, with a pty, and
  still reports its exit status. That is the failure this design has to catch by
  hand (`list-clients` before launching), and it is worse than the concurrent-
  popup case: nothing is wrong, nobody can see it, and the caller waits.
- The exit status is not portable across transports the way the popup makes it
  look. `alacritty -e sh -c 'exit 7'` returns **0** — the terminal blocks, but
  the child's verdict is gone. So the rule the concurrent-popup case already
  forces (`0` means approved only when a well-formed result file exists) is not
  a tmux quirk to work around; it is the only thing a launcher can rely on, and
  reading the exit code at all is a portability trap.

So the answer for a headless agent may be less "relocate the screen" than "find
the human's multiplexer and prove somebody is attached to it". What is still
unanswered is the agent with no human terminal anywhere — SSH-less CI, a cloud
session — where the browser Plannotator opens is a real advantage and a tmux
popup has nothing to attach to.
