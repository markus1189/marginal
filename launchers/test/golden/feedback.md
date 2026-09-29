The user reviewed `git diff HEAD` in marginal. Each `##` heading below is a real
location — file, line range, and which side of the diff the selected lines live
on. Paths are relative to the repository root, `<root>`, not to the current directory:

  * **(new)** — the lines are in the file now, at those numbers. Go there.
  * **(old)** — the lines were DELETED. They are not in the working tree at all;
    the numbers are their position before the change, and the blockquote is the
    only copy. Do not go looking for them on disk.

The blockquote is the exact diff text they selected, `+`/`-` markers and all.
What follows it is their comment. Address every comment.

A heading ending in `· general` has no location and no blockquote: it is a
comment on the change as a whole, not on any line of it.

Line numbers were resolved against the diff as it was when review started. If
you have changed these files since, re-read them before editing.

## git diff HEAD · general

a general comment on the change

## with space.txt:2 (old), with space.txt:2 (new) · lines

> (quoted)

a replacement: old and new side

## with space.txt:2 (new) · lines

> (quoted)

an addition only

## script.sh · paragraph

> (quoted)

the mode change

## deleted.txt:1 (old) · lines

> (quoted)

a deleted line: old side only

## with space.txt · heading

> (quoted)

the file heading of a spaced path

## notes.md:7 (new) · lines

> (quoted)

````
see:
```
## not a heading
```
````

## deleted.txt · heading

> (quoted)

a deleted file

