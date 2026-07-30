# marginal

Annotate markdown in a terminal at whatever granularity you actually mean —
an inline code span, a table row, a range of list items, a whole section — and
emit the result as prose an agent can act on.

POC scope: open a file, navigate, select, comment, emit JSON + feedback
markdown. No tmux popup and no gate semantics yet; the only launcher is the pi
extension below — see `STATUS.md` for what exists, what does not, and why.

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
marginal FILE.md [--result PATH] [--label NAME]
marginal --dump-blocks FILE.md     # headless: print the navigation units
marginal --help                    # exits 0
```

`--label NAME` replaces the path everywhere a human reads it — the title bar and
every location in the feedback markdown. It exists for launchers, which open a
temp file whose absolute path is noise the consumer cannot use. `source.path` in
the result JSON still records where the bytes came from; `source.label` carries
the name, and is omitted entirely when no label was given.

`--dump-blocks` prints the flat navigation-unit table, which is how the parser
gets checked without a terminal:

```
  0  heading      L1-1  level=1
  1  list-item    L3-3
  2  paragraph    L5-5
```

## Output

**Use `--result PATH`.** The TUI needs a real tty, so it refuses to start when
stdout is redirected — which means the feedback markdown it prints on exit can
only ever land on the screen, never in a pipe. The result JSON is the only
machine-readable route, and it carries the same markdown in
`feedbackMarkdown`.

Exit codes: `0` nothing was annotated, `1` something was, `2` tool failure
(unreadable file, no tty, bad flag).

A real run, annotating the inline code span in
``Use `parse_document` and the [comrak docs](https://docs.rs) here.``:

```json
{
  "version": 1,
  "decision": "changes-requested",
  "source": { "path": "PLAN.md", "lines": 5 },
  "annotations": [
    {
      "id": "a1",
      "type": "comment",
      "blockKind": "code-span",
      "startLine": 5,
      "startCol": 5,
      "endLine": 5,
      "endCol": 20,
      "wholeLines": false,
      "originalText": "`parse_document`",
      "text": "parse once and reuse the arena"
    }
  ],
  "feedbackMarkdown": "# Review feedback: PLAN.md\n\n## PLAN.md:5:5-20 · code-span\n> `parse_document`\n\nparse once and reuse the arena\n"
}
```

`wholeLines` says whether the span covers its lines entirely, so a consumer
knows to quote whole lines rather than a fragment. It also picks the location
format: `PLAN.md:5` for whole lines, `PLAN.md:5:5-20` for a fragment.

## Launcher: annotating an agent's reply

`.pi/extensions/marginal-annotate.ts` registers `/annotate` in
[pi](https://github.com/badlogic/pi-mono). It takes the agent's last message,
writes it to a temp `.md`, suspends pi's TUI, runs marginal over it, and sends
`feedbackMarkdown` back as the next prompt. Exit `0` sends nothing.

The binary is looked up as `$MARGINAL_BIN`, then `target/release/marginal`, then
`marginal` on `PATH` — the repo build wins, so you review with what you just
compiled. Project-local extensions load only in a trusted project, so start pi
with `-a` or trust the project once.

`--label assistant-message` is what makes it readable: the temp path is noise,
so every location reads `assistant-message:29 · list-item` instead.

No tty relocation is involved. pi already owns the terminal, so the extension
stops the host TUI and starts it again in a `finally`. That is the whole trick,
and it only works for a host that is itself a terminal program.

## Keys

| Key | Action |
|---|---|
| `h`/`j`/`k`/`l`, arrows | move by character / line |
| `C-d` / `C-u` | half page down / up |
| `C-f` / `C-b`, PgDn/PgUp | full page down / up (two lines of overlap) |
| `0` / `$`, Home/End | start / end of line |
| `J` / `K` | move by navigation unit |
| `g` / `G` | first / last line |
| `v` | select a range of units — `J`/`K` extends |
| `V` | select whole lines, ignoring unit boundaries — `j`/`k` extends |
| `+` / `-` (also `=` / `_`) | widen / narrow along the markdown hierarchy |
| `c` | comment on the selection |
| `x` | remove an annotation on the cursor's **line** — the most recent one, if several overlap |
| `Esc` | drop the selection |
| `q`, `C-c` | quit |

Raw mode delivers `C-c` as a keystroke and no `SIGINT` is ever raised, so it is
bound explicitly. Without that binding there is no way out but `q`.

### Writing a comment

Readline bindings, because that is what fingers expect. `Enter` saves, `Esc` or
`C-c` cancels, and **`C-j` inserts a newline** — comments can be several lines.

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

Three mechanisms, all resolving to one `(line, column)` span — and the screen
shows **raw source lines**, so there is no rendered→source mapping layer to get
wrong. Every node already *is* a span.

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

Note that the cursor must actually sit *inside* a node to reach it: with the
cursor on the `#` of a heading, `-` has nothing to narrow to, because the text
run starts two columns later.

Positions come from [comrak](https://github.com/kivikakk/comrak)'s `sourcepos`.
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

**Columns are 1-based *byte* offsets, not characters.** In ``Prüfen `köde` ``
the backtick is character 8 and comrak calls it column 9. Every slice and every
render cut is forced onto a character boundary. The four normalisations layered
on top of `sourcepos`, each pinned by a test, are documented in `STATUS.md`.

## Syntax highlighting

Headings, fences, inline code, links, emphasis, blockquotes, table pipes and
list markers are coloured from **the AST that already exists** — no second
parser, no regexes, no extra dependency. `blocks::parse_tree` knows where every
node is to the byte, `highlight` turns that into per-line tagged ranges, and
`ui` maps tags to colours.

The renderer composites overlapping ranges with later marks winning, so the
layering is: syntax, then selection, then cursor. Highlighting can never hide
where you are or what you have chosen, and `**bold**` inside a blockquote wins
over the quote's own styling.

Code *inside* a fence is not highlighted by language. Deliberately out of
scope — see `STATUS.md` for the options if it ever matters.

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

**No test requires a terminal.** The UI is covered through ratatui's
`TestBackend`, which renders into an in-memory buffer, so the gutter, partial
line highlighting, multibyte segmentation, narrow-pane layout and scrolling are
asserted on rather than assumed.

What that leaves uncovered is `main.rs::handle_key` — crossterm's key decoding
and the mode dispatch. Driving the real binary in a tmux pane is the way to
exercise it; `AGENTS.md` has the recipe.
