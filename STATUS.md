# Status

## In this POC

- comrak-based structure extraction in two views: a flat list of navigation
  units, and the full containment hierarchy down to inline nodes
- `(line, column)` spans throughout, byte-safe on multibyte input
- source view with gutter, cursor, partial-line selection highlighting,
  scrolling
- three selection mechanisms: unit ranges (`v`), line ranges (`V`), and
  expand/contract along the hierarchy (`+`/`-`)
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
- **tier 2** (heading-scoped sections) and **tier 5** (semantic `w`/`b`
  motions between inline nodes) — not built.

## Not built yet

- **the launcher** — the piece that relocates the TUI onto a tty
  the agent does not own. Deliberately absent: see the open question below.
- `--gate`, `--stdin`, `$EDITOR` escalation, deletion annotations, global
  comments, approve-with-notes
- **horizontal scrolling.** `draw_source` renders each line from column 1 and
  lets ratatui truncate, so on a 95-column pane anything past roughly column 88
  cannot be seen — awkward for a tool whose selling point is column-precise
  selection.
- the annotations pane is a fixed six rows and does not scroll; the comment
  editor caps at eight rows and does not scroll either, so the caret vanishes
  in a longer comment.

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
