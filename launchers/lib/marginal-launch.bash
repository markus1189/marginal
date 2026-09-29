# shellcheck shell=bash
#
# marginal-launch.bash — what marginal-last and marginal-diff share: finding the
# binary, and borrowing a tty to run it on. Sourced, never executed.
#
# The two launchers used to carry identical copies of both, and a fix to one
# copy had to be remembered for the other. The caller defines `die`, which
# must print its message and exit 2; nothing here exits any other way.
#
# Found by each launcher as `@marginalLaunchLib@`, which the flake rewrites to
# this file's store path, and otherwise as `<launcher dir>/../lib/`.

# The directory this file was sourced from, left unnormalised on purpose: the
# launchers are installed by symlinking their directory into ~/.claude/skills,
# and `..` must be resolved by the kernel, physically, from the real location.
# `cd`+`pwd` would resolve it logically, from the symlink.
MARGINAL_LIB_DIR="$(dirname "${BASH_SOURCE[0]}")"

# Rewritten by the flake's postInstall to the store path of the binary this
# file is installed beside; in a checkout it stays the sentinel, which no
# -x test can match. Ahead of the PATH probe: a packaged launcher and its
# packaged binary are one derivation and therefore one version, PATH may hold
# any other.
MARGINAL_PACKAGED_BIN="@marginalBin@"

# ---------------------------------------------------------------- binary
#
# $MARGINAL_BIN  →  <repo>/target/release/marginal  →  the packaged binary
# →  `marginal` on PATH.  Prints the first one that actually runs.
#
# "Actually runs", not `-x`. A repo build linked against a nix glibc keeps its
# mode bits after a garbage collection deletes that glibc's loader, and then
# exec fails with ENOENT. `-x` chose it anyway, over a working marginal
# further down the list, and the review died in the popup with rc=126.
# `--help` needs no tty and exits 0, so it is the cheapest honest probe.

marginal_runs() {
  [ -f "$1" ] && [ -x "$1" ] && "$1" --help >/dev/null 2>&1 </dev/null
}

marginal_find_binary() {
  local candidate skipped=""
  for candidate in \
      "${MARGINAL_BIN:-}" \
      "$MARGINAL_LIB_DIR/../../target/release/marginal" \
      "$MARGINAL_PACKAGED_BIN" \
      "$(command -v marginal || true)"; do
    [ -n "$candidate" ] || continue
    if marginal_runs "$candidate"; then
      printf '%s\n' "$candidate"
      return 0
    fi
    # Only a file that is there and does not run is worth mentioning; the
    # sentinel and an absent repo build are the normal case.
    if [ -e "$candidate" ]; then
      printf '%s: skipping %s: it is there but does not run (a stale build?)\n' \
        "${0##*/}" "$candidate" >&2
      skipped=" (skipped: $candidate)"
    fi
  done
  die "marginal not found$skipped — build it (cargo build --release) or set \$MARGINAL_BIN"
}

# ---------------------------------------------------------------- the tty
#
# marginal refuses to start without a real tty, and a launcher run from an
# agent's tool call has none. A tmux popup is a blocking tty on the client that
# is already showing the agent's pane; a terminal window is the same trade
# without tmux. There is deliberately no third branch: a gate that cannot reach
# the human must say so, not answer on their behalf.
#
# Three things about this, all measured:
#   - A tmux session with no attached client still runs the command, with a pty,
#     and still reports its exit status — to an audience of nobody. A TUI
#     launched that way waits for a keypress that can never arrive, so the
#     attached-client check below is what stands between the user and a silent
#     30-minute hang.
#   - The popup does not inherit this shell's environment; it gets the tmux
#     server's. The binary is passed by absolute path for that reason, and the
#     locale is forwarded explicitly, since a server started before the current
#     locale settings would otherwise render multibyte text differently here
#     than everywhere else.
#   - alacritty does NOT forward its child's exit status: `alacritty -e sh -c
#     'exit 7'` returns 0. Nothing may read the status as a verdict; the result
#     file is the verdict.
#
# marginal_run_on_tty TITLE CMD [ARG...] — run CMD on a borrowed tty and wait
# for it. Its status is left in MARGINAL_RC, as a diagnostic only.
#
# The caller's EXIT trap must call marginal_release_tty. A launcher killed
# mid-review, by an agent's tool timeout or a Ctrl-C, used to leave the popup
# up: the human kept annotating, and every comment went to a result file no
# one would ever read. So the popup client runs as a background job, because
# bash runs no trap while a foreground child is running, and TERM, INT and HUP
# end the launcher through `die`, whose exit closes the popup and kills the
# client. A popup that is gone takes marginal with it (SIGHUP).

MARGINAL_RC=0
MARGINAL_CHILD=""
MARGINAL_CHILD_KIND=""

marginal_release_tty() {
  [ -n "$MARGINAL_CHILD" ] || return 0
  if [ "$MARGINAL_CHILD_KIND" = popup ]; then
    if [ -n "${TMUX_PANE:-}" ]; then
      tmux display-popup -C -t "$TMUX_PANE" 2>/dev/null || true
    else
      tmux display-popup -C 2>/dev/null || true
    fi
  fi
  kill "$MARGINAL_CHILD" 2>/dev/null || true
  MARGINAL_CHILD=""
}

# shellcheck disable=SC2034  # MARGINAL_RC is this function's output
marginal_run_on_tty() {
  local title="$1"; shift
  MARGINAL_RC=0
  trap 'die "interrupted — the review was abandoned and its window closed"' TERM INT HUP
  if [ -n "${TMUX:-}" ]; then
    # Otherwise the list-clients probe below fails and says "no attached client"
    command -v tmux >/dev/null || die "\$TMUX is set but tmux is not on PATH"
    [ -n "$(tmux list-clients -t "${TMUX_PANE:-}" -F '#{client_tty}' 2>/dev/null)" ] \
      || die "this tmux session has no attached client — nobody could see the popup"

    # if-blocks, not `[ -n "$x" ] && popup+=(…)`: a false test is a failed
    # command, and under `set -e` an unset LC_ALL would end the script here.
    # 90% leaves the agent's pane visible around the edges, which is worth
    # having on a big terminal. On a small one it is not: every row matters,
    # and the full client shows two more document rows than 90% does at 40x12,
    # 50x16 and 80x16. Below 60x20 the popup takes the whole client. If the
    # size cannot be read, 90%.
    local size="" cw="" ch="" extent=90%
    size="$(tmux display-message -p -t "${TMUX_PANE:-}" '#{client_width} #{client_height}' 2>/dev/null)" || size=""
    read -r cw ch <<<"$size" || true
    if [[ "$cw" =~ ^[0-9]+$ && "$ch" =~ ^[0-9]+$ ]] && { [ "$cw" -lt 60 ] || [ "$ch" -lt 20 ]; }; then
      extent=100%
    fi

    # -T is expanded as a tmux format, and the title carries the caller's
    # arguments: `#(cmd)` in a git pathspec ran cmd, `#{…}` was substituted.
    # `##` is a literal `#`.
    local popup=(-E -w "$extent" -h "$extent" -T " marginal · ${title//#/##} ")
    if [ -n "${TMUX_PANE:-}" ]; then popup+=(-t "$TMUX_PANE"); fi
    if [ -n "${LANG:-}" ]; then popup+=(-e "LANG=$LANG"); fi
    if [ -n "${LC_ALL:-}" ]; then popup+=(-e "LC_ALL=$LC_ALL"); fi
    if [ -n "${COLORTERM:-}" ]; then popup+=(-e "COLORTERM=$COLORTERM"); fi

    tmux display-popup "${popup[@]}" -- "$@" &
    MARGINAL_CHILD=$! MARGINAL_CHILD_KIND=popup
  elif [ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ] && command -v alacritty >/dev/null; then
    alacritty --title "marginal · $title" -e "$@" &
    MARGINAL_CHILD=$! MARGINAL_CHILD_KIND=window
  else
    die "no way to reach a terminal — run Claude Code inside tmux, or install alacritty"
  fi
  wait "$MARGINAL_CHILD" || MARGINAL_RC=$?
  MARGINAL_CHILD=""
  trap - TERM INT HUP
}

# ---------------------------------------------------------------- the result
#
# marginal rewrites the result file after every change to the annotations with
# `"final": false`, and once more with `"final": true` when the human quits
# (README, "The result file"). So a file is not by itself a verdict:
#
#   absent          no verdict — the caller says so and exits 2
#   final: true     the verdict, as it always was
#   final: false    the session ended without the human quitting — a crash, a
#                   lost terminal, a signal. Every annotation in it is real and
#                   was committed by the human; whether they were done is not
#                   known. Hand them back, labelled; never call an empty one
#                   an approval, which its `decision` field would.
#
# A file without the key comes from a marginal that wrote only on quit, so it is
# final. `.final // true` would not do: jq's `//` treats `false` as missing.

marginal_result_final() {
  jq -r 'if .final == false then "false" else "true" end' "$1"
}

# marginal_check_interrupted FINAL COUNT — dies when an interrupted review holds
# nothing, and otherwise prints the notice that goes above the feedback of an
# interrupted one (nothing for a final review).
marginal_check_interrupted() {
  [ "$1" = true ] && return 0
  [ "$2" -gt 0 ] \
    || die "the review was interrupted before the user quit (launcher rc=$MARGINAL_RC)," \
           "with no annotations committed — no verdict is available. It is not an approval."
  # No rc here: a popup whose marginal was killed by a signal reports 0.
  cat <<'EOF'
**This review was interrupted.** marginal ended before the user quit it, so
what follows are the comments they had committed up to that point: their own
words, but possibly not all of them, and not a sign-off on the rest. Address
them, then ask the user whether they had more to say.

EOF
}
