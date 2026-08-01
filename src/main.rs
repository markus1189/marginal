//! marginal — POC: open a markdown file, navigate by block, annotate ranges.
//!
//! Usage:
//!   marginal FILE.md [--result PATH]
//!   marginal --dump-blocks FILE.md     (headless; prints the block table)

mod app;
mod blocks;
mod editor;
mod highlight;
mod table;
mod ui;
mod wrap;

use std::io;
use std::process::ExitCode;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use app::{App, Mode, Sel};

const USAGE: &str =
    "usage: marginal [--dump-blocks] [--raw] [--result[=]PATH] [--label[=]NAME] [--] FILE";

struct Args {
    file: String,
    result: Option<String>,
    label: Option<String>,
    dump: bool,
    pretty: bool,
}

/// Argv as UTF-8, or the argument that is not.
///
/// `std::env::args()` unwraps each argument and panics on one that is not valid
/// UTF-8, which exits 101 with a panic message where the documented contract is
/// 0, 1 or 2. Linux filenames are arbitrary byte strings, so an ordinary shell
/// glob over a directory holding a Latin-1 name is enough to reach it.
fn argv_utf8(args: impl Iterator<Item = std::ffi::OsString>) -> Result<Vec<String>, String> {
    args.map(|a| {
        a.into_string()
            .map_err(|bad| format!("argument is not valid UTF-8: {}", bad.to_string_lossy()))
    })
    .collect()
}

/// The value of a flag that takes one. A flag consuming the next token
/// unconditionally silently ate a following flag as its value: `--result
/// --label out.json f.md` wrote the whole review to a file named `--label`, and
/// `--label --result o.json` swallowed the result path so nothing was saved at
/// all. The `starts_with('-')` guard in the match only ever saw tokens that
/// reached it, and `ok_or` fired only when the flag was the very last argument.
///
/// The guard is a refusal, not a rule about what a value may contain: the
/// `flag=value` spelling takes whatever it is given, and the message points at
/// it — a label is free text, and `-WIP` is a perfectly ordinary thing to call
/// one.
fn value(next: Option<String>, flag: &str, needs: &str) -> Result<String, String> {
    match next {
        None => Err(format!("{flag} needs {needs}")),
        Some(v) if v.starts_with('-') => Err(format!(
            "{flag} needs {needs}, not {v} (write {flag}={v} to mean it)"
        )),
        Some(v) => Ok(v),
    }
}

/// `Ok(None)` means help was asked for, which is not a failure.
///
/// Argv arrives as a parameter rather than being read here, because the bug
/// this guards against lives in the *wiring* and nothing else: `argv_utf8` had
/// a test from the day the panic was fixed, and putting `std::env::args()` back
/// on the line below still left the suite green with the panic restored. There
/// is now no argv this function can read except the one it is given, so the
/// test and `main` walk the same path.
///
/// Decoding is the first thing that happens, before a single flag is looked at,
/// so an undecodable argument is an error whatever else is on the command
/// line — `--help` included. See README's exit codes.
fn parse_args(argv: impl Iterator<Item = std::ffi::OsString>) -> Result<Option<Args>, String> {
    parse_argv(argv_utf8(argv)?)
}

fn parse_argv(argv: Vec<String>) -> Result<Option<Args>, String> {
    let mut file = None;
    let mut result = None;
    let mut label = None;
    let mut dump = false;
    let mut pretty = true;
    // Two escape hatches, because a value that starts with a dash was otherwise
    // not expressible at all and the refusal blamed the wrong token.
    //
    // `--` ends the flags: everything after it is the FILE, however it is
    // spelled. It used to reach the unknown-flag arm, so nothing is being taken
    // away. Only the first one is the marker — a second `--` is by then an
    // ordinary operand, the same as under every getopt.
    //
    // `--flag=VALUE` is a spelling of `--flag VALUE` and nothing more: the value
    // is taken verbatim, dashes, `=` signs, empty and all, so `--label=-WIP`
    // says what no pair of tokens could. `--result=o.json` used to be reported
    // as `unknown flag: --result=o.json`, which named the flag *and* the value
    // and blamed both. An `=` after any other flag is still an unknown flag:
    // `--raw=1` and `--=x` are typos, not requests.
    let mut ended = false;
    let mut it = argv.into_iter();
    while let Some(a) = it.next() {
        if ended {
            file = Some(a);
            continue;
        }
        match a.as_str() {
            "--" => ended = true,
            "--dump-blocks" => dump = true,
            "--raw" => pretty = false,
            "--result" => result = Some(value(it.next(), "--result", "a path")?),
            "--label" => label = Some(value(it.next(), "--label", "a name")?),
            "-h" | "--help" => return Ok(None),
            _ => {
                if let Some(v) = a.strip_prefix("--result=") {
                    result = Some(v.to_string());
                } else if let Some(v) = a.strip_prefix("--label=") {
                    label = Some(v.to_string());
                } else if a.starts_with('-') {
                    return Err(format!("unknown flag: {a}"));
                } else {
                    file = Some(a);
                }
            }
        }
    }
    Ok(Some(Args {
        file: file.ok_or("no input file")?,
        result,
        label,
        dump,
        pretty,
    }))
}

/// Where a write to `path` would actually land: `path` itself, or — when it is
/// a symlink — the far end of the chain it points at, which is the file
/// `fs::write` would create or overwrite. Only the last component is followed
/// here; a symlinked *directory* in the middle is the kernel's business and
/// `open` resolves it either way.
///
/// The hop limit is the kernel's own, so a symlink cycle ends up reported by
/// the `open` in `preflight` (as `ELOOP`) rather than spun on here.
fn write_target(path: &std::path::Path) -> std::path::PathBuf {
    let mut p = path.to_path_buf();
    for _ in 0..40 {
        // Not a symlink (or unreadable): this is the end of the chain.
        let Ok(link) = std::fs::read_link(&p) else {
            break;
        };
        p = match p.parent() {
            Some(dir) if link.is_relative() => dir.join(link),
            _ => link,
        };
    }
    p
}

/// Can `path` be written? Asked *before* the session rather than after it: a
/// bad `--result` used to surface only on exit, by which point the reviewer had
/// done all the work and there was nowhere left to put it.
///
/// The question is asked without creating anything at `path` and without
/// removing anything at all, which is the part the first version got wrong
/// twice over:
///
/// * `exists()` follows symlinks, so a **dangling** `--result` link read as
///   absent. `create(true)` then made the file the link pointed at, and
///   `remove_file(path)` unlinked the link. One run deleted the indirection a
///   shared result path exists to provide *and* left behind the zero-byte file
///   the removal was there to prevent — both invariants, in one command.
/// * Sampling "did it exist?" and opening afterwards is a window another writer
///   can step into: the file it created in between was opened intact (no
///   truncate) and then unlinked as if this process had made it. Rare in
///   practice — the launcher hands out a fresh `mktemp -d` — but it is somebody
///   else's file being deleted.
///
/// So: what is already there is opened for writing and left exactly as it is,
/// and what is not there yet is answered for by a probe file of this process's
/// own, in the directory that would have to hold it. Nothing that pre-flight
/// did not create is ever opened for creation or removed, at any interleaving.
/// A pre-flight still leaves no result file behind, so it cannot make a session
/// that never ran look like one that did.
fn preflight(path: &str) -> io::Result<()> {
    let target = write_target(std::path::Path::new(path));

    // No `create`, no `truncate`: an existing result file survives the question
    // untouched, and a symlink is followed rather than replaced.
    let absent = match std::fs::OpenOptions::new().write(true).open(&target) {
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => e,
        Err(e) => return Err(e),
    };

    // Nothing there yet, so the real question is whether the directory takes a
    // new file. `""`, `/` and `..` name no file to create, and for those the
    // open's own error is already the answer.
    let (Some(dir), Some(_)) = (target.parent(), target.file_name()) else {
        return Err(absent);
    };
    let dir = if dir.as_os_str().is_empty() {
        std::path::Path::new(".")
    } else {
        dir
    };
    // Pid and clock: unique against every other process and against a probe an
    // earlier run was killed before removing.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let probe = dir.join(format!(
        ".marginal-preflight-{}-{stamp}",
        std::process::id()
    ));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args_os().skip(1)) {
        Ok(Some(a)) => a,
        Ok(None) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("marginal: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    let src = match std::fs::read_to_string(&args.file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("marginal: cannot read {}: {e}", args.file);
            return ExitCode::from(2);
        }
    };

    if args.dump {
        use io::Write;
        let stdout = io::stdout();
        let mut out = stdout.lock();
        for b in blocks::parse(&src) {
            let level = if b.level > 0 {
                format!("  level={}", b.level)
            } else {
                String::new()
            };
            // A closed pipe (`| head`) is a normal way to end, not a panic.
            if writeln!(
                out,
                "{:>3}  {:<12} L{}-{}{}",
                b.id,
                b.kind,
                b.start(),
                b.end(),
                level
            )
            .is_err()
            {
                return ExitCode::SUCCESS;
            }
        }
        return ExitCode::SUCCESS;
    }

    if let Some(path) = &args.result {
        if let Err(e) = preflight(path) {
            eprintln!("marginal: cannot write {path}: {e}");
            return ExitCode::from(2);
        }
    }

    let mut app = App::new(args.file.clone(), &src);
    app.label = args.label;
    app.pretty = args.pretty;
    match run(&mut app) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("marginal: {e}");
            return ExitCode::from(2);
        }
    }

    let (feedback, code) = finish(&app, args.result.as_deref());
    if !feedback.is_empty() {
        print!("{feedback}");
    }
    ExitCode::from(code)
}

/// Everything after the last keypress: write the result file if one was asked
/// for, then say what belongs on stdout and what the exit code is.
///
/// Returning the feedback rather than printing it is the whole point of the
/// split. The failed-write path is the one where the markdown *is* the review —
/// the annotations have no other copy left — and an early `return` here once
/// took a whole session with it. That regression was fixed with no test, because
/// the only seam was `main`, which needs a terminal to reach. This is the seam:
/// exit code and rescued text come back together, both assertable, and the
/// caller cannot print one without the other.
fn finish(app: &App, result: Option<&str>) -> (String, u8) {
    let mut failed = false;
    if let Some(path) = result {
        let json = serde_json::to_string_pretty(&app.result()).expect("serialize");
        if let Err(e) = std::fs::write(path, json) {
            eprintln!("marginal: cannot write {path}: {e}");
            failed = true;
        }
    }

    let feedback = app.feedback_markdown();
    // 2 for a failed write, but the markdown still travels with it, because
    // that is exactly when it is the last copy of the annotations.
    let code = if failed {
        2
    } else {
        u8::from(!app.annotations.is_empty())
    };
    (feedback, code)
}

fn run(app: &mut App) -> io::Result<()> {
    if !io::IsTerminal::is_terminal(&io::stdout()) {
        return Err(io::Error::other(
            "stdout is not a terminal (run under a real tty, or use --dump-blocks)",
        ));
    }
    let mut terminal = ratatui::init();
    let mut scroll = app::Anchor::default();
    let outcome = loop {
        if let Err(e) = terminal.draw(|f| ui::draw(f, app, &mut scroll)) {
            break Err(e);
        }
        match event::read() {
            Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => handle_key(app, k),
            Ok(_) => {}
            Err(e) => break Err(e),
        }
        if app.quit {
            break Ok(());
        }
    };
    ratatui::restore();
    outcome
}

fn handle_key(app: &mut App, k: KeyEvent) {
    let code = k.code;
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    // Every modifier this program does not bind, tested at once rather than
    // named one at a time. SHIFT is not a chord: crossterm sets it on every
    // uppercase char, and `J`, `K`, `G`, `V` and `P` are real bindings. CONTROL
    // is bound below, and a CONTROL chord keeps its meaning however much else is
    // held down — `Esc` then `C-c` arrives as CONTROL|ALT, and that is precisely
    // what someone types when they are trying to get out. What is left is ALT
    // alone plus SUPER, HYPER and META: unreachable while ratatui pushes no
    // Kitty enhancement flags, but a denylist of two bits let all three through
    // to the unmodified bindings, so `Super-x` removed an annotation exactly as
    // `M-x` did.
    let unbound = !ctrl && !(k.modifiers - KeyModifiers::SHIFT).is_empty();

    match app.mode {
        // Readline bindings, as bash has trained everyone to expect.
        Mode::Input => {
            let e = &mut app.editor;
            match code {
                KeyCode::Enter => app.commit_comment(),
                // Raw mode delivers C-c as a key, not a signal. Unbound, it
                // does nothing at all — so bind it to the obvious thing.
                KeyCode::Esc => app.cancel_input(),
                KeyCode::Char('c') if ctrl => app.cancel_input(),

                // C-j inserts a newline; Enter is reserved for committing.
                KeyCode::Char('j') if ctrl => e.newline(),

                KeyCode::Char('a') if ctrl => e.home(),
                KeyCode::Char('e') if ctrl => e.end(),
                KeyCode::Char('b') if ctrl => e.left(),
                KeyCode::Char('f') if ctrl => e.right(),
                KeyCode::Char('d') if ctrl => e.delete_forward(),
                KeyCode::Char('k') if ctrl => e.kill_to_end(),
                KeyCode::Char('u') if ctrl => e.kill_to_start(),
                KeyCode::Char('w') if ctrl => e.kill_word_back_ws(),
                KeyCode::Char('h') if ctrl => e.backspace(),
                KeyCode::Char('p') if ctrl => e.history_prev(),
                KeyCode::Char('n') if ctrl => e.history_next(),

                KeyCode::Char('b') if alt => e.word_left(),
                KeyCode::Char('f') if alt => e.word_right(),
                KeyCode::Char('d') if alt => e.kill_word_forward(),
                KeyCode::Backspace if alt => e.kill_word_back(),

                KeyCode::Backspace => e.backspace(),
                KeyCode::Delete => e.delete_forward(),
                KeyCode::Left => e.left(),
                KeyCode::Right => e.right(),
                KeyCode::Home => e.home(),
                KeyCode::End => e.end(),
                KeyCode::Up => e.history_prev(),
                KeyCode::Down => e.history_next(),

                // Anything else with a modifier is a chord we do not bind, not
                // text to insert. `unbound` covers ALT and the exotic three;
                // SHIFT has to stay allowed or no capital letter could be typed.
                KeyCode::Char(c) if !ctrl && !unbound => e.insert(c),
                _ => {}
            }
        }
        // The emergency exit, bound once for every path through Normal mode. In
        // raw mode C-c arrives as a keystroke and no SIGINT is ever raised, so
        // without this line C-c leaves you trapped — and it has to survive the
        // guard below, because the user who is already trying to bail out types
        // `Esc` then `C-c`, which crossterm hands over as CONTROL|ALT. Sitting
        // behind the peek arm as well as behind the ALT guard, it was reachable
        // from neither.
        Mode::Normal if ctrl && code == KeyCode::Char('c') => app.quit = true,
        // Normal mode binds no ALT chord, and the arms below match on `code`
        // alone — so without this guard `M-x` reached the plain `x` arm and
        // removed an annotation, with no undo and no confirmation, while `M-q`
        // quit. Input mode already guarded the other direction ("anything else
        // with a modifier is a chord we do not bind"); this is the same rule for
        // the other two modes. Emacs bindings make both chords reflex, and
        // crossterm decodes a quick `Esc` then a key as that key's ALT chord.
        Mode::Normal if unbound => {}
        // The peek overlay swallows the movement keys: while it is up, j/k
        // scroll the overlay rather than the cursor underneath it. Every binding
        // here is the unmodified key and says so: the overlay used to close on
        // `C-q` and `C-z` and scroll on `C-j`/`C-k`, which is the same "a chord
        // we do not bind reached its unmodified action" bug as `M-x`.
        Mode::Normal if app.peek => match code {
            KeyCode::Char('z' | 'q') | KeyCode::Esc if !ctrl => app.toggle_peek(),
            KeyCode::Char('j') | KeyCode::Down if !ctrl => app.scroll_peek(1),
            KeyCode::Char('k') | KeyCode::Up if !ctrl => app.scroll_peek(-1),
            _ => {}
        },
        // Paging. C-f/C-b keep two lines of overlap, as vim does.
        Mode::Normal if ctrl => match code {
            KeyCode::Char('d') => app.page(1, true),
            KeyCode::Char('u') => app.page(-1, true),
            KeyCode::Char('f') => app.page(1, false),
            KeyCode::Char('b') => app.page(-1, false),
            // Display-row motion. `j`/`k` move a source line, which is one
            // keypress out of a line that wraps to thousands of rows; these
            // reach the middle of one. Not `gj`/`gk`: `g` is already first line.
            KeyCode::Char('n') => app.move_row(1),
            KeyCode::Char('p') => app.move_row(-1),
            _ => {}
        },
        Mode::Normal => match code {
            KeyCode::PageDown => app.page(1, false),
            KeyCode::PageUp => app.page(-1, false),
            KeyCode::Char('q') => app.quit = true,
            KeyCode::Char('j') | KeyCode::Down => app.move_line(1),
            KeyCode::Char('k') | KeyCode::Up => app.move_line(-1),
            KeyCode::Char('h') | KeyCode::Left => app.move_char(-1),
            KeyCode::Char('l') | KeyCode::Right => app.move_char(1),
            KeyCode::Char('J') => app.move_block(1),
            KeyCode::Char('K') => app.move_block(-1),
            // Inline motions: the way to reach a code span or link sitting past
            // the right edge of the pane without panning the viewport there.
            KeyCode::Char('w') => app.move_inline(1),
            KeyCode::Char('b') => app.move_inline(-1),
            KeyCode::Char('0') | KeyCode::Home => app.goto_line_start(),
            KeyCode::Char('$') | KeyCode::End => app.goto_line_end(),
            KeyCode::Char('g') => app.goto_first(),
            KeyCode::Char('G') => app.goto_last(),
            KeyCode::Char('v') => app.toggle_blocks(),
            KeyCode::Char('V') => app.toggle_lines(),
            // widen / narrow along the markdown hierarchy
            KeyCode::Char('+' | '=') => app.expand(),
            KeyCode::Char('-' | '_') => app.contract(),
            KeyCode::Char('P') => app.toggle_pretty(),
            KeyCode::Char('z') => app.toggle_peek(),
            // Enter is the primary; `c` stays bound because it is what the
            // first two weeks of muscle memory reach for.
            KeyCode::Enter | KeyCode::Char('c') => app.begin_comment(),
            KeyCode::Char('x') => app.remove_at_cursor(),
            // `]`/`[` rather than `n`/`N`: search will want those, and vim
            // already spells "next/previous change hunk" with brackets.
            KeyCode::Char(']') => app.goto_mark(1),
            KeyCode::Char('[') => app.goto_mark(-1),
            KeyCode::Esc => app.sel = Sel::Here,
            _ => {}
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("marginal-{}-{name}", std::process::id()));
        p
    }

    /// An empty directory of this test's own. Never the user's files, and never
    /// shared with another test — several of these watch for a stray file.
    fn scratch(name: &str) -> std::path::PathBuf {
        let d = tmp(name);
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The `--result` path was only ever tried after the session, so an
    /// unwritable one was discovered when every annotation was already made and
    /// the early `return` skipped the feedback markdown that was the only other
    /// copy of them.
    #[test]
    fn preflight_rejects_an_unwritable_path_without_leaving_a_file() {
        let bad = tmp("no-such-dir/out.json");
        assert!(
            preflight(bad.to_str().unwrap()).is_err(),
            "accepted {bad:?}"
        );

        let good = tmp("out.json");
        let _ = std::fs::remove_file(&good);
        assert!(preflight(good.to_str().unwrap()).is_ok());
        assert!(
            !good.exists(),
            "pre-flight left a file behind, so its absence no longer means the TUI never ran"
        );

        // An existing result file is checked for writability, not emptied.
        std::fs::write(&good, "keep").unwrap();
        assert!(preflight(good.to_str().unwrap()).is_ok());
        assert_eq!(std::fs::read_to_string(&good).unwrap(), "keep");
        let _ = std::fs::remove_file(&good);

        // An empty `--result` is still refused, with the message the OS has
        // always given it. Improving that message is somebody else's commit;
        // quietly starting to *accept* it would be this one's fault.
        assert!(preflight("").is_err());
    }

    /// `exists()` follows symlinks, so a dangling `--result` link read as
    /// absent: `create(true)` made the file it pointed at and `remove_file`
    /// then unlinked the link. A result path deliberately pointed through a
    /// symlink into a shared location silently stopped being an indirection,
    /// and the zero-byte file the removal exists to prevent was left behind at
    /// the other end — both halves of the doc comment broken by one run.
    #[cfg(unix)]
    #[test]
    fn preflight_asks_through_a_symlink_without_replacing_it() {
        let dir = scratch("symlink");
        let link = dir.join("out.json");
        let target = dir.join("shared.json");
        let at = |p: &std::path::Path| preflight(p.to_str().unwrap());

        // Dangling: the link is the only thing that exists yet.
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(at(&link).is_ok(), "refused a writable indirection");
        assert!(link.is_symlink(), "pre-flight deleted the symlink itself");
        assert!(
            !target.exists(),
            "pre-flight created the file the link points at"
        );

        // Resolved: the file at the far end is probed, not emptied, and the
        // link still points at it afterwards.
        std::fs::write(&target, "keep").unwrap();
        assert!(at(&link).is_ok());
        assert!(
            link.is_symlink(),
            "pre-flight replaced the link with a file"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");

        // A chain is followed to its end, and one that lands nowhere writable
        // is still an error — the link surviving is not a licence to accept it.
        let chained = dir.join("chain.json");
        std::os::unix::fs::symlink("out.json", &chained).unwrap();
        assert!(at(&chained).is_ok());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");

        let nowhere = dir.join("nowhere.json");
        std::os::unix::fs::symlink(dir.join("no-such-dir/x.json"), &nowhere).unwrap();
        assert!(at(&nowhere).is_err(), "accepted a link into a missing dir");
        assert!(nowhere.is_symlink());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `existed` sample sat before the `open`, and `truncate(false)` meant
    /// an interloper's file was opened intact and then unlinked as if this
    /// process had made it. Deleting a file nobody asked to delete needs no
    /// unlucky machine to matter, only an unlucky interleaving — and against a
    /// writer sharing the path this lost thousands of files per twenty thousand
    /// attempts.
    ///
    /// The property this pins is stronger than "usually survives": pre-flight
    /// creates nothing at the result path at all, so there is no interleaving
    /// left in which somebody else's file is the one it removes. A flaky pass
    /// here would mean the property is back to being statistical.
    #[test]
    fn preflight_destroys_no_file_it_did_not_create() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        const PRECIOUS: &str = "the other process's data";
        let dir = scratch("race");
        let path = dir.join("out.json");
        let arg = path.to_str().unwrap().to_string();

        let stop = Arc::new(AtomicBool::new(false));
        let flight = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let _ = preflight(&arg);
                }
            })
        };

        let (mut destroyed, mut truncated) = (0, 0);
        for _ in 0..2000 {
            std::fs::write(&path, PRECIOUS).unwrap();
            match std::fs::read_to_string(&path) {
                Err(_) => destroyed += 1,
                Ok(s) if s != PRECIOUS => truncated += 1,
                Ok(_) => {}
            }
            let _ = std::fs::remove_file(&path);
        }
        stop.store(true, Ordering::Relaxed);
        flight.join().unwrap();

        assert_eq!(
            (destroyed, truncated),
            (0, 0),
            "pre-flight unlinked another writer's result file"
        );
        // …and cleaned up after itself while doing it.
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert!(left.is_empty(), "pre-flight left a file behind: {left:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `std::env::args()` panics on an argument that is not valid UTF-8, exiting
    /// 101 with a panic message where README documents 0/1/2 and STATUS.md lists
    /// "unreadable file -> exits 2" among the hardened edges. A Linux filename is
    /// an arbitrary byte string, so a shell glob over a directory holding a
    /// Latin-1 name reaches it with a file that is perfectly readable.
    ///
    /// This goes through `parse_args`, not `argv_utf8`, because the function was
    /// never the risk. It was tested in isolation while the call site kept its
    /// own copy of the decision, and a call site is exactly what a decoding rule
    /// can be forgotten at: reverting that one line to `std::env::args()` left
    /// the whole suite green with the panic back in the binary.
    #[cfg(unix)]
    #[test]
    fn a_non_utf8_argument_reaches_the_parser_as_an_error_not_a_panic() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;
        let bad = || OsString::from_vec(vec![0xff, b'b', b'a', b'd']);
        let ok = |s: &str| OsString::from(s);

        // As the FILE, as a flag's value, and as a stray operand alike: the
        // whole list is decoded before any of it is interpreted.
        for case in [
            vec![bad()],
            vec![ok("--label"), bad(), ok("f.md")],
            vec![ok("--result"), ok("o.json"), ok("f.md"), bad()],
            // `--help` is no exception. An argument list that cannot be decoded
            // is an error whatever else is on it; printing help while quietly
            // dropping the argument nobody could read is the worse answer.
            vec![ok("--help"), bad()],
        ] {
            let err = parse_args(case.clone().into_iter())
                .err()
                .unwrap_or_else(|| panic!("{case:?} parsed instead of erroring"));
            assert!(err.contains("not valid UTF-8"), "{case:?}: {err}");
            // The message shows the argument, lossily, so the human can tell
            // which one it was.
            assert!(err.contains('\u{fffd}'), "{case:?}: {err}");
        }

        // …and a decodable list still parses, so "reject everything" cannot
        // pass this test.
        let a = parse_args([ok("--label"), ok("WIP"), ok("f.md")].into_iter())
            .unwrap()
            .unwrap();
        assert_eq!((a.file.as_str(), a.label.as_deref()), ("f.md", Some("WIP")));
        assert!(parse_args([ok("--help")].into_iter()).unwrap().is_none());
    }

    fn argv(a: &[&str]) -> Result<Option<Args>, String> {
        parse_argv(a.iter().map(ToString::to_string).collect())
    }

    /// `--result` and `--label` took the next token whatever it was, so a
    /// mistyped command line silently did something else: `--result --label
    /// out.json f.md` wrote the review to a file literally named `--label`, and
    /// `--label --result o.json` ate the result path so nothing was saved.
    #[test]
    fn a_flag_is_never_swallowed_as_another_flags_value() {
        assert!(argv(&["--result", "--label", "o.json", "f.md"]).is_err());
        assert!(argv(&["--label", "--result", "o.json", "f.md"]).is_err());
        assert!(argv(&["--result", "--raw", "f.md"]).is_err());
        // Still an error when the flag is simply last, as it always was.
        assert!(argv(&["f.md", "--result"]).is_err());

        // And the ordinary forms keep working.
        let a = argv(&["--result", "o.json", "--label", "PLAN.md", "f.md"])
            .unwrap()
            .unwrap();
        assert_eq!(a.result.as_deref(), Some("o.json"));
        assert_eq!(a.label.as_deref(), Some("PLAN.md"));
        assert_eq!(a.file, "f.md");

        // The refusal now says how to mean it, which is the whole reason the
        // guard is tolerable: it is a spelling rule, not a ban.
        let e = argv(&["--label", "-WIP", "f.md"]).err().unwrap();
        assert!(e.contains("--label=-WIP"), "{e}");
    }

    /// A file whose name starts with a dash could not be named at all: every
    /// such token reached the unknown-flag arm. `--` sat in that same arm, so
    /// giving it the end-of-flags meaning every getopt already gives it takes
    /// nothing away.
    #[test]
    fn a_double_dash_ends_the_flags() {
        let file = |a: &[&str]| argv(a).unwrap().unwrap().file;

        assert_eq!(file(&["--", "-notes.md"]), "-notes.md");
        // Past the marker, a flag is a filename — including `-h`, which would
        // otherwise print the usage and exit 0 with nothing read.
        assert_eq!(file(&["--", "-h"]), "-h");
        assert_eq!(file(&["--", "--label"]), "--label");
        // Only the first `--` is the marker; the second is an ordinary operand,
        // and the last operand is the file, as it has always been.
        assert_eq!(file(&["--", "--", "f.md"]), "f.md");
        // Flags before it are still flags.
        let a = argv(&["--raw", "--label", "PLAN.md", "--", "-f.md"])
            .unwrap()
            .unwrap();
        assert_eq!(a.file, "-f.md");
        assert_eq!(a.label.as_deref(), Some("PLAN.md"));
        assert!(!a.pretty);

        // On its own it names no file, which is the same error as no arguments.
        assert!(argv(&["--"]).is_err());
        // And it is not a value: `--result --` is the mistake the value guard
        // exists to catch, not a request for a file named `--`. Say
        // `--result=--` if that is really what you meant.
        assert!(argv(&["--result", "--", "f.md"]).is_err());
    }

    /// `--result=o.json` failed with `unknown flag: --result=o.json`, blaming a
    /// flag that exists for the shape of the token. The `=` form is also the
    /// only way to give a flag a value that starts with a dash — a path can
    /// dodge with `./-x`, but a label is free text and `-WIP` is a name.
    #[test]
    fn a_flag_takes_its_value_after_an_equals_sign_too() {
        let ok = |a: &[&str]| argv(a).unwrap().unwrap();

        let a = ok(&["--result=o.json", "--label=PLAN.md", "f.md"]);
        assert_eq!(a.result.as_deref(), Some("o.json"));
        assert_eq!(a.label.as_deref(), Some("PLAN.md"));
        assert_eq!(a.file, "f.md");

        // Verbatim: dashes, a second `=`, and the empty value all pass through.
        // `--label=` is exactly `--label ''`, which has always been accepted;
        // an empty `--result` is refused later, by the pre-flight, exactly as
        // `--result ''` is.
        assert_eq!(ok(&["--label=-WIP", "f.md"]).label.as_deref(), Some("-WIP"));
        assert_eq!(
            ok(&["--label=-- draft --", "f.md"]).label.as_deref(),
            Some("-- draft --")
        );
        assert_eq!(
            ok(&["--result=--label", "f.md"]).result.as_deref(),
            Some("--label")
        );
        assert_eq!(ok(&["--label=a=b", "f.md"]).label.as_deref(), Some("a=b"));
        assert_eq!(ok(&["--label=", "f.md"]).label.as_deref(), Some(""));
        assert_eq!(ok(&["--result=", "f.md"]).result.as_deref(), Some(""));

        // An `=` is not suddenly special everywhere. The flags that take no
        // value do not start taking one, an empty flag name is not the
        // end-of-flags marker wearing a value, and a filename may contain `=`.
        for bad in [
            &["--raw=1", "f.md"],
            &["--dump-blocks=1", "f.md"],
            &["--=x", "f.md"],
            &["--help=me", "f.md"],
        ] {
            let e = argv(bad).err().unwrap();
            assert_eq!(e, format!("unknown flag: {}", bad[0]), "{bad:?}");
        }
        assert_eq!(ok(&["a=b.md"]).file, "a=b.md");
        // Past `--` it is a filename like any other.
        assert_eq!(ok(&["--", "--label=x"]).file, "--label=x");
    }

    const DOC: &str = "# Steps\n\n- one\n- two\n";

    fn key(c: char, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), m)
    }

    /// `ctrl` was bound at function scope but `alt` only inside the `Mode::Input`
    /// arm, and the peek and Normal arms match on `code` alone. So every ALT
    /// chord fell through to its unmodified binding: `M-x` removed an annotation
    /// with no undo and no confirmation, and `M-q` quit.
    #[test]
    fn normal_mode_binds_no_alt_chord() {
        let mut app = App::new("t.md".into(), DOC);
        handle_key(&mut app, key('c', KeyModifiers::NONE));
        app.editor.set("keep me");
        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.annotations.len(), 1, "setup failed");

        handle_key(&mut app, key('x', KeyModifiers::ALT));
        assert_eq!(app.annotations.len(), 1, "M-x removed an annotation");

        handle_key(&mut app, key('q', KeyModifiers::ALT));
        assert!(!app.quit, "M-q quit");

        // The unmodified keys still work, so the guard is not a blanket mute.
        handle_key(&mut app, key('x', KeyModifiers::NONE));
        assert!(app.annotations.is_empty(), "plain x stopped working");
        handle_key(&mut app, key('q', KeyModifiers::NONE));
        assert!(app.quit, "plain q stopped working");
    }

    fn annotated() -> App {
        let mut app = App::new("t.md".into(), DOC);
        handle_key(&mut app, key('c', KeyModifiers::NONE));
        app.editor.set("keep me");
        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.annotations.len(), 1, "setup failed");
        app
    }

    fn peeking() -> App {
        let mut app = App::new("t.md".into(), DOC);
        handle_key(&mut app, key('z', KeyModifiers::NONE));
        assert!(app.peek, "setup failed");
        app.peek_rows = 10;
        app
    }

    /// The ALT guard was a denylist of two bits sitting in front of the CONTROL
    /// arm, so it got both ends wrong. `C-c` is the only way out of raw mode —
    /// no SIGINT is raised — and the sequence someone types once they are
    /// already trying to bail out is `Esc` then `C-c`, which crossterm decodes
    /// as CONTROL|ALT: swallowed, in Normal mode and behind the peek overlay
    /// alike. `crossterm::KeyModifiers` has six bits, and this sweeps all 64
    /// combinations rather than the two the guard happened to name.
    #[test]
    fn the_emergency_exit_survives_every_modifier_in_every_mode() {
        for bits in 0..64u8 {
            let m = KeyModifiers::from_bits_truncate(bits);
            if !m.contains(KeyModifiers::CONTROL) {
                continue;
            }

            let mut app = App::new("t.md".into(), DOC);
            handle_key(&mut app, key('c', m));
            assert!(app.quit, "normal mode: C-c with {m:?} did not quit");

            let mut app = peeking();
            handle_key(&mut app, key('c', m));
            assert!(app.quit, "peek overlay: C-c with {m:?} did not quit");

            // In Input mode C-c is cancel, not quit — the escape hatch out of
            // the comment editor, and equally unreachable if a stray modifier
            // can turn it into an ordinary keystroke.
            let mut app = App::new("t.md".into(), DOC);
            handle_key(&mut app, key('c', KeyModifiers::NONE));
            assert_eq!(app.mode, Mode::Input, "setup failed");
            app.editor.set("draft");
            handle_key(&mut app, key('c', m));
            assert_eq!(
                app.mode,
                Mode::Normal,
                "input mode: C-c with {m:?} did not cancel"
            );
        }
    }

    /// The class the ALT guard only half closed: it named ALT and CONTROL, which
    /// leaves SUPER, HYPER and META falling through to the unmodified bindings —
    /// `Super-x` removed an annotation exactly as `M-x` did. Nothing can produce
    /// those today (ratatui pushes no Kitty enhancement flags), and nothing warns
    /// the day something does. SHIFT is the one modifier that must ride along:
    /// crossterm sets it on every uppercase char, so a guard that swallowed it
    /// would take `J`, `K`, `G`, `V` and `P` with it.
    #[test]
    fn only_shift_rides_along_with_a_normal_mode_binding() {
        for bits in 0..64u8 {
            let m = KeyModifiers::from_bits_truncate(bits);
            let bare = (m - KeyModifiers::SHIFT).is_empty();

            let mut app = annotated();
            handle_key(&mut app, key('x', m));
            assert_eq!(
                app.annotations.is_empty(),
                bare,
                "x with {m:?} reached remove_at_cursor"
            );

            let mut app = App::new("t.md".into(), DOC);
            handle_key(&mut app, key('q', m));
            assert_eq!(app.quit, bare, "q with {m:?}");

            let mut app = App::new("t.md".into(), DOC);
            handle_key(&mut app, key('c', m));
            if m.contains(KeyModifiers::CONTROL) {
                assert!(app.quit, "C-c with {m:?} is the exit");
            } else {
                assert_eq!(app.mode == Mode::Input, bare, "c with {m:?}");
            }
        }

        // …and the capitals, which only arrive with SHIFT set, still act.
        let mut app = App::new("t.md".into(), DOC);
        handle_key(&mut app, key('J', KeyModifiers::SHIFT));
        assert!(app.cursor.line > 1, "S-J stopped moving a block");
        let pretty = app.pretty;
        handle_key(&mut app, key('P', KeyModifiers::SHIFT));
        assert_ne!(app.pretty, pretty, "S-P stopped toggling pretty");
        handle_key(&mut app, key('V', KeyModifiers::SHIFT));
        assert!(
            matches!(app.sel, Sel::Lines { .. }),
            "S-V stopped selecting lines"
        );
    }

    /// Input mode named the same two bits: "anything else with a modifier is a
    /// chord we do not bind" was spelled `!ctrl && !alt`, so `Super-Z` typed a
    /// `Z` into the comment. Same sweep, same rule — only SHIFT rides along,
    /// because that is how a capital letter arrives in the first place.
    #[test]
    fn input_mode_inserts_only_an_unmodified_character() {
        for bits in 0..64u8 {
            let m = KeyModifiers::from_bits_truncate(bits);
            let mut app = App::new("t.md".into(), DOC);
            handle_key(&mut app, key('c', KeyModifiers::NONE));
            assert_eq!(app.mode, Mode::Input, "setup failed");
            handle_key(&mut app, key('Z', m));
            assert_eq!(
                app.editor.text() == "Z",
                (m - KeyModifiers::SHIFT).is_empty(),
                "Z with {m:?}"
            );
        }
    }

    /// A CONTROL chord means the same thing however much else is held down, so
    /// `Esc` then `C-d` — CONTROL|ALT, the way an Emacs-trained hand pages — has
    /// to page. The ALT guard sat in front of the CONTROL arm and ate all six.
    #[test]
    fn a_ctrl_chord_keeps_its_meaning_when_alt_rides_along() {
        let doc: String = (1..=60).map(|i| format!("line {i}\n\n")).collect();
        for c in ['d', 'u', 'f', 'b', 'n', 'p'] {
            let mut moved = Vec::new();
            for m in [
                KeyModifiers::CONTROL,
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ] {
                let mut app = App::new("t.md".into(), &doc);
                app.viewport = 10;
                app.move_line(40);
                let start = app.cursor.line;
                handle_key(&mut app, key(c, m));
                assert_ne!(app.cursor.line, start, "C-{c} with {m:?} did nothing");
                moved.push(app.cursor.line);
            }
            assert_eq!(
                moved[0], moved[1],
                "C-M-{c} landed somewhere else than C-{c}"
            );
        }
    }

    /// The peek overlay matches on `code` alone too.
    #[test]
    fn the_peek_overlay_binds_no_alt_chord() {
        let mut app = App::new("t.md".into(), DOC);
        handle_key(&mut app, key('z', KeyModifiers::NONE));
        assert!(app.peek, "setup failed");
        handle_key(&mut app, key('q', KeyModifiers::ALT));
        assert!(app.peek, "M-q closed the overlay");
        assert!(!app.quit);
        handle_key(&mut app, key('q', KeyModifiers::NONE));
        assert!(!app.peek, "plain q stopped closing the overlay");
    }

    /// …and the same was true of its CONTROL chords, which the ALT-only guard
    /// never covered: `C-q` and `C-z` closed the overlay and `C-j`/`C-k`
    /// scrolled it, because every arm matched on `code` alone. `C-c` stays
    /// bound — it is the emergency exit — and the plain keys stay bound, so the
    /// overlay is never a room with no door.
    #[test]
    fn the_peek_overlay_binds_no_ctrl_chord_but_the_exit() {
        for c in ['q', 'z'] {
            let mut app = peeking();
            handle_key(&mut app, key(c, KeyModifiers::CONTROL));
            assert!(app.peek, "C-{c} closed the overlay");
            assert!(!app.quit, "C-{c} quit");
        }

        for (c, code) in [('j', KeyCode::Down), ('k', KeyCode::Up)] {
            let mut app = peeking();
            app.peek_scroll = 3;
            handle_key(&mut app, key(c, KeyModifiers::CONTROL));
            assert_eq!(app.peek_scroll, 3, "C-{c} scrolled the overlay");
            handle_key(&mut app, KeyEvent::new(code, KeyModifiers::CONTROL));
            assert_eq!(app.peek_scroll, 3, "C-{code:?} scrolled the overlay");
        }

        // The overlay still scrolls and still closes on the unmodified keys.
        let mut app = peeking();
        handle_key(&mut app, key('j', KeyModifiers::NONE));
        assert_eq!(app.peek_scroll, 1, "plain j stopped scrolling");
        handle_key(&mut app, key('k', KeyModifiers::NONE));
        assert_eq!(app.peek_scroll, 0, "plain k stopped scrolling");
        handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.peek, "Esc stopped closing the overlay");

        let mut app = peeking();
        handle_key(&mut app, key('z', KeyModifiers::NONE));
        assert!(!app.peek, "plain z stopped closing the overlay");
    }

    const INPUT_TEXT: &str = "alpha beta gamma";
    /// Byte 8 — `alpha be|ta gamma`. Inside a word, with a whole word on either
    /// side and a space either way, so a motion or a kill that goes the wrong
    /// direction lands somewhere visibly different rather than on the same
    /// boundary the right one would have found.
    const INPUT_CURSOR: usize = 8;

    /// Input mode, one comment already committed so the history is not empty,
    /// `INPUT_TEXT` in the buffer and the cursor at `INPUT_CURSOR`.
    fn editing() -> App {
        let mut app = App::new("t.md".into(), DOC);
        handle_key(&mut app, key('c', KeyModifiers::NONE));
        app.editor.set("older note");
        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.annotations.len(), 1, "setup failed");

        handle_key(&mut app, key('c', KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Input, "setup failed");
        app.editor.set(INPUT_TEXT);
        for _ in 0..(INPUT_TEXT.len() - INPUT_CURSOR) {
            app.editor.left();
        }
        assert_eq!(app.editor.row_col(), (0, INPUT_CURSOR), "setup failed");
        app
    }

    /// Every binding Input mode has, against the buffer it is supposed to leave
    /// behind. `handle_key` is a 64-arm dispatch table typed out by hand and it
    /// had exactly two tests, neither of which pressed a readline key: `C-k`
    /// calling `kill_to_start` and `C-u` calling `kill_to_end` would have passed
    /// the suite, and so would `M-f` moving left. The editor's own tests cannot
    /// see it — they call the methods, and it is the wiring that is unproven.
    ///
    /// One row per arm of the `Mode::Input` match, in the order they appear
    /// there. `C-n` and `Down` need a `C-p` first: with nothing being browsed
    /// they are documented no-ops, and a row asserting "nothing happened" would
    /// pass just as well if they were unbound.
    #[test]
    fn every_input_mode_binding_does_what_its_name_says() {
        let ctrl = KeyModifiers::CONTROL;
        let alt = KeyModifiers::ALT;
        let plain = |c: KeyCode| KeyEvent::new(c, KeyModifiers::NONE);
        let chord = |c: char, m| key(c, m);

        // name, keys, expected text, expected (row, col), expected mode
        let table = vec![
            (
                "Enter",
                vec![plain(KeyCode::Enter)],
                "",
                (0, 0),
                Mode::Normal,
            ),
            ("Esc", vec![plain(KeyCode::Esc)], "", (0, 0), Mode::Normal),
            ("C-c", vec![chord('c', ctrl)], "", (0, 0), Mode::Normal),
            (
                "C-j",
                vec![chord('j', ctrl)],
                "alpha be\nta gamma",
                (1, 0),
                Mode::Input,
            ),
            (
                "C-a",
                vec![chord('a', ctrl)],
                INPUT_TEXT,
                (0, 0),
                Mode::Input,
            ),
            (
                "C-e",
                vec![chord('e', ctrl)],
                INPUT_TEXT,
                (0, 16),
                Mode::Input,
            ),
            (
                "C-b",
                vec![chord('b', ctrl)],
                INPUT_TEXT,
                (0, 7),
                Mode::Input,
            ),
            (
                "C-f",
                vec![chord('f', ctrl)],
                INPUT_TEXT,
                (0, 9),
                Mode::Input,
            ),
            (
                "C-d",
                vec![chord('d', ctrl)],
                "alpha bea gamma",
                (0, 8),
                Mode::Input,
            ),
            (
                "C-k",
                vec![chord('k', ctrl)],
                "alpha be",
                (0, 8),
                Mode::Input,
            ),
            (
                "C-u",
                vec![chord('u', ctrl)],
                "ta gamma",
                (0, 0),
                Mode::Input,
            ),
            (
                "C-w",
                vec![chord('w', ctrl)],
                "alpha ta gamma",
                (0, 6),
                Mode::Input,
            ),
            (
                "C-h",
                vec![chord('h', ctrl)],
                "alpha bta gamma",
                (0, 7),
                Mode::Input,
            ),
            (
                "C-p",
                vec![chord('p', ctrl)],
                "older note",
                (0, 10),
                Mode::Input,
            ),
            (
                "C-p C-n",
                vec![chord('p', ctrl), chord('n', ctrl)],
                INPUT_TEXT,
                (0, 16),
                Mode::Input,
            ),
            (
                "M-b",
                vec![chord('b', alt)],
                INPUT_TEXT,
                (0, 6),
                Mode::Input,
            ),
            (
                "M-f",
                vec![chord('f', alt)],
                INPUT_TEXT,
                (0, 10),
                Mode::Input,
            ),
            (
                "M-d",
                vec![chord('d', alt)],
                "alpha be gamma",
                (0, 8),
                Mode::Input,
            ),
            (
                "M-Backspace",
                vec![KeyEvent::new(KeyCode::Backspace, alt)],
                "alpha ta gamma",
                (0, 6),
                Mode::Input,
            ),
            (
                "Backspace",
                vec![plain(KeyCode::Backspace)],
                "alpha bta gamma",
                (0, 7),
                Mode::Input,
            ),
            (
                "Delete",
                vec![plain(KeyCode::Delete)],
                "alpha bea gamma",
                (0, 8),
                Mode::Input,
            ),
            (
                "Left",
                vec![plain(KeyCode::Left)],
                INPUT_TEXT,
                (0, 7),
                Mode::Input,
            ),
            (
                "Right",
                vec![plain(KeyCode::Right)],
                INPUT_TEXT,
                (0, 9),
                Mode::Input,
            ),
            (
                "Home",
                vec![plain(KeyCode::Home)],
                INPUT_TEXT,
                (0, 0),
                Mode::Input,
            ),
            (
                "End",
                vec![plain(KeyCode::End)],
                INPUT_TEXT,
                (0, 16),
                Mode::Input,
            ),
            (
                "Up",
                vec![plain(KeyCode::Up)],
                "older note",
                (0, 10),
                Mode::Input,
            ),
            (
                "Up Down",
                vec![plain(KeyCode::Up), plain(KeyCode::Down)],
                INPUT_TEXT,
                (0, 16),
                Mode::Input,
            ),
            (
                "Z",
                vec![chord('Z', KeyModifiers::SHIFT)],
                "alpha beZta gamma",
                (0, 9),
                Mode::Input,
            ),
        ];

        for (name, keys, text, at, mode) in table {
            let mut app = editing();
            for k in keys {
                handle_key(&mut app, k);
            }
            assert_eq!(app.editor.text(), text, "{name}: text");
            assert_eq!(app.editor.row_col(), at, "{name}: cursor");
            assert_eq!(app.mode, mode, "{name}: mode");
        }

        // Enter, Esc and C-c all leave Normal mode over an empty buffer; what
        // separates committing from cancelling is whether the comment survived.
        let mut app = editing();
        handle_key(&mut app, plain(KeyCode::Enter));
        assert_eq!(app.annotations.len(), 2, "Enter did not commit");
        assert_eq!(app.annotations[1].text, INPUT_TEXT);
        for cancel in [plain(KeyCode::Esc), chord('c', ctrl)] {
            let mut app = editing();
            handle_key(&mut app, cancel);
            assert_eq!(app.annotations.len(), 1, "{cancel:?} committed the comment");
        }
    }

    /// `C-p` leaves the cursor at the end of the recalled entry, which is
    /// exactly where `C-k` and `M-d` have nothing to kill — and the five kill
    /// keys ended history browsing whether or not they killed anything. The
    /// screen did not change, nothing was edited, and the draft parked by the
    /// `C-p` became unreachable by any key. Every row here is a sequence a hand
    /// actually types: recall an old comment, decide against reusing it, and
    /// press the key that clears a line.
    ///
    /// `editor.rs` could not see it. Its tests call the methods and assert on
    /// the buffer, and the buffer is identical either way; what differs is the
    /// key you have to press next.
    #[test]
    fn a_kill_key_that_kills_nothing_leaves_the_draft_recallable() {
        let ctrl = KeyModifiers::CONTROL;
        let alt = KeyModifiers::ALT;
        let plain = |c: KeyCode| KeyEvent::new(c, KeyModifiers::NONE);
        let chord = |c: char, m| key(c, m);
        let home = chord('a', ctrl);

        for (name, keys) in [
            ("C-p C-k", vec![chord('k', ctrl)]),
            ("C-p M-d", vec![chord('d', alt)]),
            ("C-p C-a C-u", vec![home, chord('u', ctrl)]),
            (
                "C-p C-a M-DEL",
                vec![home, KeyEvent::new(KeyCode::Backspace, alt)],
            ),
            ("C-p C-a C-w", vec![home, chord('w', ctrl)]),
            ("C-p C-d", vec![chord('d', ctrl)]),
            ("C-p C-a BS", vec![home, plain(KeyCode::Backspace)]),
        ] {
            let mut app = editing();
            handle_key(&mut app, chord('p', ctrl));
            assert_eq!(app.editor.text(), "older note", "{name}: setup failed");
            for k in keys {
                handle_key(&mut app, k);
            }
            assert_eq!(app.editor.text(), "older note", "{name}: killed something");
            handle_key(&mut app, chord('n', ctrl));
            assert_eq!(app.editor.text(), INPUT_TEXT, "{name}: draft lost");
        }
    }

    /// The other way the draft went missing, and the one that needed no no-op:
    /// recall a comment, change your mind about a word of it, and go looking
    /// again. The second `C-p` saw `browsing == None` — an edit turns it off —
    /// and parked the recalled comment over the draft, so `C-n` handed back
    /// `older note!` and the draft was gone with no key left to reach it.
    ///
    /// The editor's own tests could not see this either: the one that types
    /// after a recall asserts exactly the first three keystrokes and stops one
    /// `C-p` short of the loss.
    #[test]
    fn a_recalled_comment_you_have_edited_is_not_your_draft() {
        let ctrl = KeyModifiers::CONTROL;
        let chord = |c: char, m| key(c, m);

        let mut app = editing();
        handle_key(&mut app, chord('p', ctrl));
        assert_eq!(app.editor.text(), "older note");
        handle_key(&mut app, key('!', KeyModifiers::NONE));
        assert_eq!(app.editor.text(), "older note!", "setup failed");

        // Off to look at the history again. The edit is discarded — it never
        // had a slot — but the draft is still in the one slot there is.
        handle_key(&mut app, chord('p', ctrl));
        assert_eq!(app.editor.text(), "older note", "the edit was parked");
        handle_key(&mut app, chord('n', ctrl));
        assert_eq!(app.editor.text(), INPUT_TEXT, "draft lost");

        // And it commits as itself, not as a copy of the history entry.
        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.annotations.len(), 2);
        assert_eq!(app.annotations[1].text, INPUT_TEXT);
    }

    /// The other half of "a bad `--result` must not cost the session", and the
    /// half that was never tested. `preflight` catches an unwritable path before
    /// a word is written, but it cannot catch a disk that fills up, a directory
    /// removed mid-session or a path that only fails on the real write — and on
    /// that path the feedback markdown is the last copy of the annotations in
    /// existence. An early `return` here once threw a whole session away; the
    /// fix went in with no test, because the only seam was `main` and `main`
    /// needs a terminal.
    #[test]
    fn a_failed_result_write_still_hands_back_the_feedback() {
        let bad = tmp("finish-no-such-dir").join("out.json");
        let (feedback, code) = finish(&annotated(), Some(bad.to_str().unwrap()));
        assert_eq!(code, 2, "a failed write must still exit 2");
        assert!(
            feedback.contains("keep me"),
            "the rescued annotations went with the failed write: {feedback:?}"
        );
        assert!(!bad.exists(), "the write was supposed to fail");
    }

    /// …and the ordinary endings it shares its code with, so that "still prints
    /// the feedback" cannot be met by printing it unconditionally and calling
    /// every session a failure. The exit code is the diagnostic: 0 approved,
    /// 1 changes requested, 2 the review is only in the text above.
    #[test]
    fn finish_writes_the_result_and_grades_the_session() {
        let dir = scratch("finish");

        let out = dir.join("out.json");
        let (feedback, code) = finish(&annotated(), Some(out.to_str().unwrap()));
        assert_eq!(code, 1, "annotations are changes-requested");
        assert!(feedback.contains("keep me"));
        let json = std::fs::read_to_string(&out).unwrap();
        assert!(json.contains("keep me"), "{json}");
        assert!(json.contains("changes-requested"), "{json}");

        // A clean review prints nothing and exits 0 — but still writes the
        // file, because the result file is the verdict and "no annotations" is
        // a verdict. Only stdout is allowed to be empty here.
        let clean = dir.join("clean.json");
        let app = App::new("t.md".into(), DOC);
        let (feedback, code) = finish(&app, Some(clean.to_str().unwrap()));
        assert_eq!(code, 0);
        assert!(feedback.is_empty(), "{feedback:?}");
        assert!(std::fs::read_to_string(&clean)
            .unwrap()
            .contains("approved"));

        // With no `--result` there is nothing to fail: stdout is the only output
        // and the exit code still splits clean from annotated.
        let (feedback, code) = finish(&annotated(), None);
        assert_eq!(code, 1);
        assert!(feedback.contains("keep me"));
        let (feedback, code) = finish(&App::new("t.md".into(), DOC), None);
        assert_eq!(code, 0);
        assert!(feedback.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
