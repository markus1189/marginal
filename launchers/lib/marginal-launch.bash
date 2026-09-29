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
# →  `marginal` on PATH.  Prints the one chosen.

marginal_find_binary() {
  if [ -n "${MARGINAL_BIN:-}" ] && [ -x "${MARGINAL_BIN}" ]; then
    printf '%s\n' "$MARGINAL_BIN"
  elif [ -x "$MARGINAL_LIB_DIR/../../target/release/marginal" ]; then
    printf '%s\n' "$MARGINAL_LIB_DIR/../../target/release/marginal"
  elif [ -x "$MARGINAL_PACKAGED_BIN" ]; then
    printf '%s\n' "$MARGINAL_PACKAGED_BIN"
  elif command -v marginal >/dev/null; then
    command -v marginal
  else
    die "marginal not found — build it (cargo build --release) or set \$MARGINAL_BIN"
  fi
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

MARGINAL_RC=0

# shellcheck disable=SC2034  # MARGINAL_RC is this function's output
marginal_run_on_tty() {
  local title="$1"; shift
  MARGINAL_RC=0
  if [ -n "${TMUX:-}" ]; then
    [ -n "$(tmux list-clients -t "${TMUX_PANE:-}" -F '#{client_tty}' 2>/dev/null)" ] \
      || die "this tmux session has no attached client — nobody could see the popup"

    # if-blocks, not `[ -n "$x" ] && popup+=(…)`: a false test is a failed
    # command, and under `set -e` an unset LC_ALL would end the script here.
    local popup=(-E -w 90% -h 90% -T " marginal · $title ")
    if [ -n "${TMUX_PANE:-}" ]; then popup+=(-t "$TMUX_PANE"); fi
    if [ -n "${LANG:-}" ]; then popup+=(-e "LANG=$LANG"); fi
    if [ -n "${LC_ALL:-}" ]; then popup+=(-e "LC_ALL=$LC_ALL"); fi
    if [ -n "${COLORTERM:-}" ]; then popup+=(-e "COLORTERM=$COLORTERM"); fi

    tmux display-popup "${popup[@]}" -- "$@" || MARGINAL_RC=$?
  elif [ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ] && command -v alacritty >/dev/null; then
    alacritty --title "marginal · $title" -e "$@" || MARGINAL_RC=$?
  else
    die "no way to reach a terminal — run Claude Code inside tmux, or install alacritty"
  fi
}
