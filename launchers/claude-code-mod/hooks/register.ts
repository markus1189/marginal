import type { Register } from 'claude-code'

import { buildDocument, collectTurns, parseSpec, verdict, type MarginalResult } from './document'

// Mods get pipes, never the terminal, and marginal refuses to start without a
// tty: the tmux popup of marginal-launch.bash lends it one. The document comes
// in on stdin, the result file goes out on stdout. The flake rewrites the
// sentinel to the library of its own package; an unpackaged copy (a dev mod
// being hot-reloaded) finds the library beside the `marginal` on PATH.
const HELPER = String.raw`
set -euo pipefail
die() { printf '%s\n' "$*" >&2; exit 2; }
label=$1
lib="@marginalLaunchLib@"
if [ ! -f "$lib" ]; then
  bin=$(command -v marginal) || die "marginal is not on PATH"
  lib="$(dirname "$(readlink -f "$bin")")/../share/claude-code/lib/marginal-launch.bash"
  [ -f "$lib" ] || die "no marginal-launch.bash beside $bin"
fi
. "$lib"
work=$(mktemp -d -t claude-code.marginal-mod.XXXXXX)
trap 'marginal_release_tty; rm -r "$work"' EXIT
cat >"$work/$label.md"
binary=$(marginal_find_binary)
marginal_run_on_tty "$label" "$binary" --result "$work/result.json" --label "$label" "$work/$label.md"
[ -f "$work/result.json" ] || die "marginal wrote no result file (rc=$MARGINAL_RC)"
cat "$work/result.json"
`

export const register: Register = on => {
  // A second popup over the first would race it for the same result prompt.
  let isReviewing = false

  on('session.start', async ($, e, next) => {
    await $.command.register({
      name: 'marginal',
      description: "Annotate the agent's last message(s) in marginal: /marginal [N|all]",
    })
    return next(e)
  })

  on('command.run', { command: 'marginal' }, async ($, e) => {
    const spec = parseSpec(e.args)
    if (!spec) return { text: 'Usage: /marginal [N|all] — N is how many assistant messages to include.' }
    if (isReviewing) return { text: 'A marginal review is already open.' }

    const document = buildDocument(collectTurns(await $.session.messages()), spec.count)
    if (!document) return { text: 'No assistant message with text in this session.' }

    // Detached: the review lasts as long as the person reads, far past any
    // hook budget. The loop owns the child; a reload ends both, and the
    // helper's traps close the popup.
    isReviewing = true
    void (async () => {
      try {
        const child = $.process.spawn({
          argv: ['bash', '-c', HELPER, 'marginal', document.label],
          input: document.text,
        })
        let stdout = ''
        let stderr = ''
        for await (const { stream, text } of child) {
          if (stream === 'stdout') stdout += text
          else stderr += text
        }
        if (stdout.trim() === '') {
          $.ui.toast(`marginal: ${stderr.trim() || 'the review produced no result'} — nothing sent.`)
          return
        }
        let result: MarginalResult
        try {
          result = JSON.parse(stdout) as MarginalResult
        } catch (err) {
          $.ui.toast(`marginal's result is unreadable (${String(err)}) — nothing sent.`)
          return
        }
        const out = verdict(result, document.label)
        if ('none' in out) {
          $.ui.toast(out.none)
          return
        }
        await $.prompt.submit({ text: out.prompt, asUser: true })
        $.ui.toast(
          `Sent ${out.count} annotation${out.count === 1 ? '' : 's'}${out.interrupted ? ' from an interrupted review' : ''}.`,
        )
      } catch (err) {
        $.ui.toast(`marginal failed: ${String(err)}`)
      } finally {
        isReviewing = false
      }
    })()

    return { text: `Reviewing ${document.label === 'conversation' ? 'the conversation' : 'the last message'} in marginal…` }
  })
}
