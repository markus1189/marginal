# Status

## In this POC

- comrak-based structure extraction in two views: a flat list of navigation
  units, and the full containment hierarchy down to inline nodes
- `(line, column)` spans throughout, byte-safe on multibyte input
- source view with gutter, cursor, partial-line selection highlighting,
  scrolling
- three selection mechanisms: unit ranges (`v`), line ranges (`V`), and
  expand/contract along the hierarchy (`+`/`-`)
- comments on any selection, removal, result JSON + feedback markdown,
  exit `0` / `1` / `2`
- 58 tests, none requiring a terminal (UI covered via ratatui `TestBackend`)

### Granularity tiers, as scoped

- **tier 1** — tables navigate by row, blockquotes by inner block, plus
  line-wise selection. Done.
- **tier 3** — columns in the cursor, the selection, the rendering, the JSON
  and the feedback locations. Done.
- **tier 4** — expand/contract on the AST. Done.
- **tier 2** (heading-scoped sections) and **tier 5** (semantic `w`/`b`
  motions between inline nodes) — not built.

## Not built yet

- **the launcher** (`bin/annot`) — the piece that relocates the TUI onto a tty
  the agent does not own. Deliberately absent: see the open question below.
- `--gate`, `--stdin`, `$EDITOR` escalation, deletion annotations, global
  comments, approve-with-notes
- the TUI has never been run by a human. Everything was verified through
  `--dump-blocks`, unit tests and `TestBackend` renders, because the agent that
  wrote it has no controlling tty.

## Verified, not assumed

| | |
|---|---|
| flake build | `nix build .#annot-tui` → store path |
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
