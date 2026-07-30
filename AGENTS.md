# marginal — agent notes

## Build & check

```sh
nix develop --command ./check    # everything CI would run, cheapest-first
cargo test                       # enough for a tight loop; no test needs a tty
```

## Invariants that break silently

- **Columns are 1-based *byte* offsets**, from comrak sourcepos. Every slice and
  every render cut goes through `floor_boundary` / `ceil_boundary`. A raw
  `&s[a..b]` on user text is a panic waiting for the first umlaut.
- `unsafe_code = "forbid"`. Not negotiable.
- Navigation units stay flat, gapless and non-overlapping. `blocks.rs` has a
  test asserting it across three fixtures; keep it passing.
- `app.rs` holds no ratatui types, so it stays testable without a terminal.
  Colours belong in `ui.rs`, tags in `highlight.rs`.

## You probably have no tty

`main.rs` refuses to start unless stdout is a terminal, so plain
`cargo run -- FILE` from an agent session exits `2`. Three headless routes:

- `--dump-blocks FILE` — the flat unit table
- unit tests in `app.rs` / `blocks.rs` / `editor.rs`
- ratatui `TestBackend` renders in `ui.rs`, which assert on real screen text
  and can be given any width, so layout regressions are testable headlessly

Whichever you used, **say which one** — never report the TUI as "run" or
"working" without naming the evidence.

## Integration testing through tmux

If a tmux server is running, a pane is a real tty and the whole thing works.
This is the only way to exercise `main.rs::handle_key`, which has no unit tests
at all — crossterm's key decoding, the `ctrl`/`alt` guards and the mode
dispatch are otherwise entirely unverified.

Spawn your own window rather than asking for one. `-d` keeps it off the user's
screen:

```sh
PANE=$(tmux new-window -d -P -F '#{pane_id}' -c /path/to/workdir 'exec zsh')
tmux resize-window -t "$PANE" -x 90 -y 30        # width is a real variable

# stdout cannot be captured (the tty guard), so ask for JSON and stash $?
tmux send-keys -t "$PANE" './target/release/marginal --result out.json PLAN.md; echo rc=$? > rc.txt' Enter
sleep 1.5                                         # the loop blocks on event::read()

tmux capture-pane -p    -t "$PANE"                # screen as plain text
tmux capture-pane -p -e -t "$PANE"                # …with SGR escapes, to assert on colour

tmux send-keys -t "$PANE" 'JJ'; sleep 0.3         # drive it
tmux send-keys -t "$PANE" -- '-'; sleep 0.3       # `--` guards keys that start with a dash
tmux send-keys -t "$PANE" 'c'; sleep 0.3
tmux send-keys -t "$PANE" 'the comment text'; sleep 0.3
tmux send-keys -t "$PANE" Enter; sleep 0.3
tmux send-keys -t "$PANE" 'q'; sleep 1

cat rc.txt out.json
tmux kill-window -t "$PANE"                       # always clean up
```

Things that will bite:

- **Sleep between every step.** The event loop blocks on `event::read()` and
  only redraws after a key arrives; capturing too early gets the previous frame.
- **`send-keys -- '-'`.** Without the `--`, tmux reads `-` as a flag.
- **Width is a variable, not a constant.** Both layout bugs found on
  2026-07-30 only appear below ~110 columns. Test at 80, 95 and 120.
- **Exit codes need a wrapper.** `; echo rc=$? > file` inside the pane, since
  you are not the process's parent.
- **Never run against the user's real files.** Build a fixture in the
  scratchpad first.

Two bugs shipped past a full green suite because nothing ever drove the real
thing: a stale status line surviving a dropped selection, and a title bar whose
path pushed every other field off the border. Both are now covered by tests —
but only after a human pressed a key.

## Lint policy

Clippy runs `pedantic` + `nursery`. Exceptions live in `[lints.clippy]` in
`Cargo.toml`, **each with a comment saying why**. Do not add a module-scope
`#![allow]` — it disarms the lint for hundreds of lines. Prefer a site-local
`#[expect]`, which breaks the build once it stops being needed.

## Layout

| file | job |
|---|---|
| `blocks.rs` | comrak → flat units + containment tree; the sourcepos fixups |
| `highlight.rs` | same tree → per-line tagged byte ranges; no second parser |
| `app.rs` | cursor, selection, annotations, JSON + markdown output |
| `ui.rs` | ratatui only; tags → colours, composites syntax < selection < cursor |
| `editor.rs` | readline comment buffer |
| `main.rs` | args, tty guard, key dispatch, exit codes |

## Numbers in the docs

Re-measure before quoting one. The test count, crate count and binary size in
`STATUS.md` had all drifted from reality by 2026-07-30. Prefer stating the
property and the command over pasting a figure that rots.
