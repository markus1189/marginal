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

const USAGE: &str = "usage: marginal [--dump-blocks] [--raw] [--result PATH] [--label NAME] FILE";

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
fn value(next: Option<String>, missing: &str) -> Result<String, String> {
    match next {
        None => Err(missing.to_string()),
        Some(v) if v.starts_with('-') => Err(format!("{missing}, not {v}")),
        Some(v) => Ok(v),
    }
}

/// `Ok(None)` means help was asked for, which is not a failure.
fn parse_args() -> Result<Option<Args>, String> {
    parse_argv(argv_utf8(std::env::args_os().skip(1))?)
}

fn parse_argv(argv: Vec<String>) -> Result<Option<Args>, String> {
    let mut file = None;
    let mut result = None;
    let mut label = None;
    let mut dump = false;
    let mut pretty = true;
    let mut it = argv.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dump-blocks" => dump = true,
            "--raw" => pretty = false,
            "--result" => result = Some(value(it.next(), "--result needs a path")?),
            "--label" => label = Some(value(it.next(), "--label needs a name")?),
            "-h" | "--help" => return Ok(None),
            other if other.starts_with('-') => return Err(format!("unknown flag: {other}")),
            other => file = Some(other.to_string()),
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

/// Can `path` be written? Asked *before* the session rather than after it: a
/// bad `--result` used to surface only on exit, by which point the reviewer had
/// done all the work and there was nowhere left to put it.
///
/// An existing file is opened without truncating, and one created to probe the
/// directory is removed again — README's contract is that the result file's
/// absence means the TUI never ran, and an empty file left by a pre-flight would
/// make that read false.
fn preflight(path: &str) -> io::Result<()> {
    let existed = std::path::Path::new(path).exists();
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    if !existed {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

fn main() -> ExitCode {
    let args = match parse_args() {
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

    let mut write_failed = false;
    if let Some(path) = &args.result {
        let json = serde_json::to_string_pretty(&app.result()).expect("serialize");
        if let Err(e) = std::fs::write(path, json) {
            eprintln!("marginal: cannot write {path}: {e}");
            write_failed = true;
        }
    }

    // Printed even when the write failed, because that is exactly when it is
    // the last copy of the annotations. Returning early here used to take the
    // whole session with it.
    let feedback = app.feedback_markdown();
    if !feedback.is_empty() {
        print!("{feedback}");
    }
    if write_failed {
        return ExitCode::from(2);
    }

    if app.annotations.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
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
            KeyCode::Char('c') => app.begin_comment(),
            KeyCode::Char('x') => app.remove_at_cursor(),
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
    }

    /// `std::env::args()` panics on an argument that is not valid UTF-8, exiting
    /// 101 with a panic message where README documents 0/1/2 and STATUS.md lists
    /// "unreadable file -> exits 2" among the hardened edges. A Linux filename is
    /// an arbitrary byte string, so a shell glob over a directory holding a
    /// Latin-1 name reaches it with a file that is perfectly readable.
    #[cfg(unix)]
    #[test]
    fn a_non_utf8_argument_is_an_error_not_a_panic() {
        use std::os::unix::ffi::OsStringExt as _;
        let bad = std::ffi::OsString::from_vec(vec![0xff, b'b', b'a', b'd']);
        let err = argv_utf8([bad].into_iter()).unwrap_err();
        assert!(err.contains("not valid UTF-8"), "{err}");

        let ok = argv_utf8([std::ffi::OsString::from("f.md")].into_iter()).unwrap();
        assert_eq!(ok, vec!["f.md".to_string()]);
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
}
