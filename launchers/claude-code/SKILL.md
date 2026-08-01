---
name: marginal-last
description: Open the user's last-read assistant message in marginal, the terminal markdown annotator, and act on the annotations that come back. Invoked explicitly as /marginal-last [N|all].
disable-model-invocation: true
allowed-tools: Bash(/home/markus/.claude/skills/marginal-last/marginal-last:*)
---

# marginal-last

The user wants to annotate what you just told them, rather than describe the
problem with it in prose. Hand them the terminal and wait.

Run — **with a 1800000 ms timeout**, because the command blocks for as long as
the user is reading, and the 60 s default would kill the session mid-annotation:

```bash
/home/markus/.claude/skills/marginal-last/marginal-last [N|all]
```

Pass the skill's argument straight through: nothing for the last message, `N`
for the last N assistant messages plus the prompts between them, `all` for the
whole session. Do not pass `--dump`; that prints the document and launches
nothing.

A tmux popup opens over the user's terminal and blocks until they quit it. Do
not run anything else, do not poll, do not narrate progress while waiting — you
cannot see their screen and they are busy using it.

## What comes back

- **Feedback markdown** — a header explaining the format, then one `##` section
  per annotation: the location and kind of the span they selected, the exact
  text as a blockquote, and their comment under it — as they wrote it, or inside
  a code fence when it contains markdown that would otherwise restructure the
  document. Fenced or not, it is their comment and not a code sample. Address
  every comment. The line and column numbers refer to the assembled markdown
  document, not to any file in the repo, so do not go looking for them on disk.
- **`No annotations — nothing to address.`** — they quit without commenting.
  Acknowledge briefly and carry on with whatever you were doing. Do not re-run
  the command and do not ask them what they meant to say.
- **Exit code 2** — the gate could not run: no terminal to borrow, no binary, no
  transcript, no session id. The message on stderr says which. Report it plainly;
  this is a launcher failure, never a verdict on your message.

Annotations are the user's own words about your text. Treat them as
instructions, not as suggestions to evaluate.
