# marginal

Annotate a document in a terminal at whatever granularity you actually mean —
an inline code span, a table row, a range of list items, a whole section — and
emit the result as prose an agent can act on. Markdown gets all of that; any
other text file gets paragraphs and lines, through the plain backend below.

POC scope: open a file, navigate, select, comment, emit JSON + feedback
markdown. No gate semantics yet; two launchers exist, one per host style — see
`STATUS.md` for what exists, what does not, and why.

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
marginal FILE.md [--result PATH] [--label NAME] [--format NAME] [--raw]
marginal --dump-blocks FILE.md     # headless: print the navigation units
marginal --help                    # exits 0 (2 if an argument is not UTF-8)
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

## File formats

Two backends. Which one runs is decided by the extension, once, before either
the TUI or `--dump-blocks` sees a unit:

| extension | backend | what you get |
|---|---|---|
| `.md`, `.markdown` | **markdown**, via comrak | everything below: headings, list items, table rows, the inline hierarchy, syntax colour |
| anything else | **plain** | paragraphs, lines, comments, JSON |

`--format markdown` and `--format plain` override the guess in either direction.
It is the answer for a file with no extension, and for the `.txt` that is really
markdown. An unknown name is refused before the session starts, because the
wrong backend is otherwise silent — a `.tex` file parsed as markdown does not
fail, it builds units out of `#` and `_` that mean nothing there and files every
comment against the wrong lines.

The plain backend knows exactly one rule: **a paragraph is a run of consecutive
non-blank lines.** That is enough for `.tex`, `.rst`, `.org`, `.txt` and anything
else to navigate by paragraph, narrow to a single line with `-`, select ranges,
comment, and emit the same result JSON and feedback markdown as markdown does —
the output format never follows the input format. What it does not give you is
syntax colour (there is nothing to tag), table alignment (there are no table
rows), or any structure a blank line does not reveal: a `\section{Intro}` on the
line above its first sentence is one unit with that sentence, and no amount of
staring at the bytes says otherwise.

It is also, unlike the markdown backend, *universally* gapless: the units are a
partition of the lines, so every non-blank line is in exactly one of them
whatever the input. `blocks.rs` lists four shapes where markdown does not manage
that.

## Output

**Use `--result PATH`.** The TUI needs a real tty, so it refuses to start when
stdout is redirected — which means the feedback markdown it prints on exit can
only ever land on the screen, never in a pipe. The result JSON is the only
machine-readable route, and it carries the same markdown in
`feedbackMarkdown`.

Exit codes: `0` nothing was annotated, `1` something was, `2` tool failure
(unreadable file, no tty, bad flag, an argument that is not valid UTF-8).

Argv is decoded before a single flag is read, so an argument that is not valid
UTF-8 is a tool failure whatever else is on the command line — `--help`
included. A Linux filename is an arbitrary byte string, so this is reachable
from an ordinary glob; the alternative, printing help while quietly dropping the
one argument nobody could read, is worse than exiting `2` and saying so.

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

## Launchers: annotating an agent's reply

Two hosts, two ways onto a tty. pi is a terminal program and can lend marginal
the one it already holds; Claude Code runs the launcher from a tool call with no
tty anywhere in reach, so one has to be borrowed from tmux. The document model —
which messages go in, how they are headed, what `--label` they carry — is the
same in both, deliberately.

### pi — `/annotate`

`.pi/extensions/marginal-annotate.ts` registers `/annotate` in
[pi](https://github.com/badlogic/pi-mono). It takes the agent's last message,
writes it to a temp `.md`, suspends pi's TUI, runs marginal over it, and sends
`feedbackMarkdown` back as the next prompt. Exit `0` sends nothing.

| Command | Document handed to marginal |
|---|---|
| `/annotate` | the last assistant message, bare |
| `/annotate 3` | the last 3 assistant messages, plus the prompts between them |
| `/annotate all` | the whole branch |

One message goes in bare, so its line numbers are its own. Anything wider is
assembled with a `## you [n]` / `## agent [n]` heading per message — otherwise a
comment cannot say which message it means. Thinking blocks, tool calls and tool
results are dropped; the numbering counts what survived.

The binary is looked up as `$MARGINAL_BIN`, then `target/release/marginal`, then
`marginal` on `PATH` — the repo build wins, so you review with what you just
compiled. Project-local extensions load only in a trusted project, so start pi
with `-a` or trust the project once.

`--label` is what makes it readable: the temp path is noise, so every location
reads `assistant-message:29 · list-item`, or `conversation:64 · paragraph` for
the wider views.

No tty relocation is involved. pi already owns the terminal, so the extension
stops the host TUI and starts it again in a `finally`. That is the whole trick,
and it only works for a host that is itself a terminal program.

The document builder — which messages go in, how they are headed — has its own
tests, since it is the part that can be wrong quietly:

```sh
node --test .pi/extensions/marginal-annotate.test.mjs
```

`./check` runs them too when `node` is on `PATH`, and skips them when it is not.

### Claude Code — `/marginal-last`

`launchers/claude-code/marginal-last` is a bash script and `SKILL.md` beside it
is the slash command, which does nothing but run the script and read its stdout.
Both live here; installing is one symlink, so the two copies cannot drift:

```sh
ln -s "$PWD/launchers/claude-code" ~/.claude/skills/marginal-last
```

Same three views (`/marginal-last`, `/marginal-last 3`, `/marginal-last all`),
same labels, same headers on the feedback.

The transcript is read from `~/.claude/projects/*/$CLAUDE_CODE_SESSION_ID.jsonl`
rather than from a session API. `isSidechain` entries (subagents) are dropped
along with thinking, tool calls, tool results and `isMeta` entries — the last of
those matters, since the injected skill body itself arrives as a user message.

There is deliberately **no fallback** when the session id is missing. A project
directory accumulates hundreds of past sessions and two windows on one repo
share it, so picking by mtime would hand over a conversation the human was never
in — silently, and with nothing in the document to give it away.

**The transcript is cut at the most recent user prompt before anything is
assembled.** That prompt is the `/marginal-last` invocation itself, so what is
offered for review is the message the human had actually read when they asked —
not the half-finished turn that is running the launcher. This has no pi
equivalent, where commands run outside the agent loop.

Which means "user prompt" has to be exact. Claude Code writes machine traffic as
ordinary user messages — task notifications, `!command` input and its output —
and the two kinds are not interchangeable: a slash-command stub records that the
human invoked something and is a valid place to cut, while a notification that
landed mid-turn is not, and cutting there would leave the in-flight turn in the
document. Everything tag-wrapped is kept out of the document either way.

`--dump` prints the assembled document and launches nothing, which is how the
builder gets checked without a terminal — the same trick as `--dump-blocks`:

```sh
launchers/claude-code/marginal-last 3 --dump
```

The tty comes from `tmux display-popup -E`, which blocks the caller for exactly
as long as the TUI runs and propagates its exit status unchanged (0/1/2/3 all
survive, measured). Without `$TMUX` it falls back to `alacritty -e`. With
neither, the launcher fails — a gate that cannot reach the human must say so
rather than answer for them.

**The result file is the verdict; the exit status is only a diagnostic.**
marginal writes it in `finish` — after the last keypress, before it picks its own
exit code — on every run that reached the TUI. That is not a stylistic
preference: `alacritty -e sh -c 'exit 7'` returns **0**, so a launcher that reads
the exit status for the annotated/clean split announces "no annotations" over
real feedback on that path. Terminals that fork outright (ghostty,
gnome-terminal) are not used, because they would not block either.

**A missing result file does not mean the TUI never ran.** The write can fail
*after* a full session — the directory went away, the disk filled, the path was
replaced — and `finish` answers that by exiting `2` and printing the review to
stdout anyway, because at that point the markdown is the only copy of the
annotations there is. Driven in a 90×30 tmux pane to check it: annotate the code
span on line 3, delete the `--result` directory mid-session, press `q`. Result:
no file, `marginal: cannot write …: No such file or directory` on stderr, the
complete `# Review feedback` block on stdout, `rc=2`. Under the
`tmux display-popup -E` this launcher uses, that pane closes when the process
exits — so by the time the launcher looks, the rescued review is already gone.

The only thing a consumer may conclude from a missing result file is therefore
**that no verdict is available**: not that nothing happened, and not that the
human did not comment. The launcher's response is right in shape — refuse, exit
`2`, never answer for the human — and it now says "no verdict" rather than "it
never ran". What it still cannot do is *recover* the review in the lost-write
case, because nothing captures the popup's stdout. That is a code change and has
not been made.

Four behaviours the code has to defend against, all measured:

| Behaviour | Consequence |
|---|---|
| A tmux session with no attached client still runs the command and reports its status | A TUI nobody can see, waiting for a keypress nobody can send. Refused up front via `list-clients`. |
| One popup per client — a second concurrent caller gets `rc=0` having never run | Handled by the same rule as above: no result file, no verdict, exit `2`. |
| The popup inherits the tmux server's environment, not the caller's | The binary is passed by absolute path, and `LANG`/`LC_ALL`/`COLORTERM` are forwarded explicitly. |
| `alacritty` does not forward its child's exit status | Nothing reads `$rc` as a verdict; it appears only in the error message when no result file exists. |

marginal's `0`/`1` split is deliberately not propagated to the agent: a tool call
that exits non-zero reads as a broken command, and "the human commented" is not a
failure. The launcher exits `0` for both and puts the distinction in stdout,
which is what the agent actually reads; `2` is every failure, jq's included.

## Keys

| Key | Action |
|---|---|
| `h`/`j`/`k`/`l`, arrows | move by character / line |
| `C-d` / `C-u` | half page down / up |
| `C-f` / `C-b`, PgDn/PgUp | full page down / up (two lines of overlap) |
| `0` / `$`, Home/End | start / end of line |
| `J` / `K` | move by navigation unit |
| `w` / `b` | move by **inline node** — the next/previous code span, link, emphasis or text run |
| `g` / `G` | first / last line |
| `C-n` / `C-p` | move by **screen row** — reaches the middle of a line that wraps to many rows |
| `v` | select a range of units — `J`/`K` extends |
| `V` | select whole lines, ignoring unit boundaries — `j`/`k` extends |
| `+` / `-` (also `=` / `_`) | widen / narrow along the markdown hierarchy |
| `P` | pretty on / off — soft wrap and aligned tables (on by default; `--raw` starts off) |
| `z` | peek: the selection, wrapped, over the source view — `j`/`k` scroll, `z`/`Esc`/`q` close |
| `Enter` (also `c`) | comment on the selection |
| `x` | remove an annotation on the cursor's **line** — the most recent one, if several overlap |
| `]` / `[` | next / previous **mark**, in document order, wrapping at both ends |
| `Esc` | drop the selection |
| `q`, `C-c` | quit |

A **mark** is an annotation you have written, or a question the document asks
that you have not answered yet. The gutter cell between the line number and the
selection bar shows which:

| cell | meaning |
|---|---|
| `?` cyan | an unanswered question |
| `●` magenta | an annotation — including one you just wrote on a question |

Commenting on a question turns its `?` into `●` and drops it from the ring, so
the fringe is a worklist that drains as you work down the message. The status
field reports where you are in the ring — `question 2/5` — which is also how you
notice the detector has found more than you expected.

A question is a `?` followed by whitespace or end-of-line, optionally through a
run of closing punctuation, and not inside a code block, code span or raw HTML.
So `Is it (really?) so?` is two, and `docs.rs/?q=1`, `ls *.rs?` and a `?:` in a
fenced block are none. Note that this is the inverse of `?\b`, which matches a
`?` followed by a word character — precisely the cases worth excluding.

Questions are derived from the source, not authored by you: they never appear in
`--result` JSON, and a document full of them with no comments is still an
`approved` decision.

Raw mode delivers `C-c` as a keystroke and no `SIGINT` is ever raised, so it is
bound explicitly. Without that binding there is no way out but `q`.

### Lines wider than the pane

The view soft-wraps: a line longer than the pane continues on the rows below it.
The view still does not scroll sideways, and never will — every implementation
surveyed makes the two mutually exclusive.

- **One line, one number.** The line number and the annotation dot sit on a
  line's first row only; a repeated number would read as a repeated line. The
  selection bar marks every row, because the line is still selected halfway
  down it.
- **Continuation rows line up under the text.** A wrapped list item continues
  under its own text rather than under the bullet, and a blockquote keeps its
  indent — otherwise the rest of an item reads as a new top-level one. The
  padding is display only: the byte columns in the JSON never see it.
- **An over-wide word breaks at its own separators.** A URL breaks after a `/`,
  not mid-token. CJK, which has no spaces at all, breaks between characters.
- **`j`/`k` move a source line, `C-n`/`C-p` a screen row.** One `j` clears a
  line however tall it wraps; `C-n` reaches the middle of one. `V` keeps
  extending by line either way.

### Tables

Markdown table source is not column-aligned, and reading a review diff of one
that is not is miserable. Pretty mode aligns it on screen by **inserting
padding** — spaces inside a cell, and dashes in the delimiter row so the rule
stays a rule. Nothing is moved, hidden or rewritten:

```
| id | description | ok |            | id |        description        | ok |
|---|:---:|---|                      |----|:-------------------------:|----|
| 1 | short | y |            ───►    | 1  |           short           | y  |
| 22 | a much longer… | n |          | 22 | a much longer…            | n  |
```

- **The columns are still bytes of the file.** Padding is inserted between
  bytes rather than over them, so a selection is addressed exactly as before
  and the JSON cannot tell the difference. `short` above is `L3:7-11` whether
  it is drawn at column 7 or column 19.
- **A table that does not fit is left ragged.** Aligning widens a table, and a
  grid cut in half by a wrap is worse to read than a ragged one — so below the
  width it needs, a table is left alone and soft-wraps like anything else. An
  aligned row therefore never wraps.
- **Rows may disagree about spacing.** `|a|` and `| bb |` in the same table
  still line up: each of a cell's two gaps grows to what its column needs.
- **`:---:` is obeyed**, and a column is padded left, right or centred to match.
- **The padding belongs to the cell.** Select a cell and the whole padded width
  highlights; select a word inside it and only the word does.

Every other tool that aligns markdown tables — prettier, mdformat,
vim-table-mode — does it by editing the file. marginal cannot: the file is the
thing under review.

### Turning it off

`P` turns pretty off, and `--raw` starts that way. Off, every cell on screen is
a byte of the file — which is the mode to reach for when a column looks wrong.
A long line is cut at the right edge with a dim `›` in the last column, and
three things make that survivable rather than a trap:

- **`w`/`b` reach what you cannot see.** The interesting columns on a long line
  are its inline node starts, and there are a handful of them, not two hundred.
  `w` lands *inside* the code span at column 176 so `-` can narrow onto it; the
  status line and the title both report where the cursor went.
- **The cursor is never silently gone.** When it sits past the right edge the
  `›` on that row takes the cursor's own colour, and the title carries
  `L{line}:{col}` regardless.
- **`z` shows the whole thing.** The peek overlay wraps the current selection,
  read-only, whichever mode the source view is in.

Wrapping is a rendering decision and reaches nothing else: `(line, col)` in the
JSON and the feedback markdown are computed from the source, and a test asserts
the output is byte-identical with wrapping on and off.

The gutter is a separate column, so the line number, the annotation dot and the
selection bar cannot be pushed off screen by anything the body does.

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
blockquote. The unit list is flat and ordered, and on ordinary prose it is also
gapless and non-overlapping, which is what makes stepping through it
predictable. There are shapes where it is not — a link reference definition, an
unreferenced footnote definition, `- - a` — and the `blocks.rs` module doc names
them; the cursor resolves to the nearest unit above rather than falling out of
the list.

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
