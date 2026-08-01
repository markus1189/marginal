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
            "--result" => {
                result = Some(it.next().ok_or("--result needs a path")?);
            }
            "--label" => {
                label = Some(it.next().ok_or("--label needs a name")?);
            }
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
                // text to insert.
                KeyCode::Char(c) if !ctrl && !alt => e.insert(c),
                _ => {}
            }
        }
        // Normal mode binds no ALT chord, and the arms below match on `code`
        // alone — so without this guard `M-x` reached the plain `x` arm and
        // removed an annotation, with no undo and no confirmation, while `M-q`
        // quit. Input mode already guarded the other direction ("anything else
        // with a modifier is a chord we do not bind"); this is the same rule for
        // the other two modes. Emacs bindings make both chords reflex, and
        // crossterm decodes a quick `Esc` then a key as that key's ALT chord.
        Mode::Normal if alt => {}
        // The peek overlay swallows the movement keys: while it is up, j/k
        // scroll the overlay rather than the cursor underneath it.
        Mode::Normal if app.peek => match code {
            KeyCode::Char('z' | 'q') | KeyCode::Esc => app.toggle_peek(),
            KeyCode::Char('c') if ctrl => app.quit = true,
            KeyCode::Char('j') | KeyCode::Down => app.scroll_peek(1),
            KeyCode::Char('k') | KeyCode::Up => app.scroll_peek(-1),
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
            // In raw mode this arrives as a keystroke and no SIGINT is ever
            // raised, so without this line C-c leaves you trapped.
            KeyCode::Char('c') => app.quit = true,
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
}
