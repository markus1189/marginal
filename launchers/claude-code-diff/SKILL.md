---
name: marginal-diff
description: Open a git diff in marginal, the terminal annotator, and act on the comments that come back anchored to real files and line numbers. Invoked explicitly as /marginal-diff [git-diff-args].
disable-model-invocation: true
allowed-tools: Bash(/home/markus/.claude/skills/marginal-diff/marginal-diff:*)
---

# marginal-diff

The user wants to review a diff — usually work you just did — by commenting on
it directly rather than describing the problem in prose. Hand them the terminal
and wait.

Run — **with a 1800000 ms timeout**, because the command blocks for as long as
the user is reading, and the 60 s default would kill the session mid-review:

```bash
/home/markus/.claude/skills/marginal-diff/marginal-diff [GIT-DIFF-ARGS...]
```

Every argument is passed through to `git diff` unchanged. With none, it reviews
`HEAD` — staged and unstaged work together, which is the usual case when you
have just finished something. Pass what the user asked for:

| the user means | pass |
|---|---|
| what you just changed, uncommitted | *(nothing)* |
| only what is staged | `--cached` |
| the commits on this branch | `main...HEAD` |
| the last N commits | `HEAD~N` |
| one path only | `-- src/app.rs` |

Do not pass `--dump` or `--dump-map`; those print the document or the line map
and launch nothing.

A tmux popup opens over the user's terminal and blocks until they quit it. Do
not run anything else, do not poll, and do not narrate progress while waiting —
you cannot see their screen and they are busy using it.

## What comes back

- **Feedback markdown** — a preamble, then one `##` section per comment. Each
  heading is a **real location in the working tree**, not a position in the
  diff, followed by the exact text they selected as a blockquote and their
  comment under it. Address every comment.

  Read the side label on every heading, because it decides where to go:

  - **`src/app.rs:312-314 (new)`** — those lines are in the file *now*, at those
    numbers. Open it and go there.
  - **`src/app.rs:298-299 (old)`** — those lines were **deleted**. They are not
    in the working tree at all. The numbers say where they used to be, and the
    blockquote is the only copy you will get. Do not go looking for them on
    disk, and do not "restore" them unless the comment asks you to.
  - Both on one heading — the comment covers a replacement: the removed lines
    and what replaced them.
  - A bare path with no line numbers — the comment is about the file as a whole
    (a rename, a mode change, a binary file, or a whole-file heading).

  A single comment may list several locations, separated by commas, when the
  selection crossed a hunk or file boundary.

- **`No annotations — nothing to address.`** — they quit without commenting.
  Acknowledge briefly and carry on. Do not re-run the command and do not ask
  them what they meant to say.

- **`No changes to review …`** — the diff was empty. Say so; do not go hunting
  for a different range unless the user asks.

- **Exit code 2** — the gate could not run: no terminal to borrow, no binary,
  not a git repository, a combined (merge) diff, or a diff that is not valid
  UTF-8. The message on stderr says which. Report it plainly; this is a launcher
  failure, never a verdict on your work.

## The diff is a snapshot

It was taken when the review started. If you changed any of these files while
the user was reading — you should not have, but if you did — the line numbers
describe the older content. Re-read a file before editing it when in doubt; the
blockquote tells you what the line said when they commented on it.

Annotations are the user's own words about your work. Treat them as
instructions, not as suggestions to evaluate.
