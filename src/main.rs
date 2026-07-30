//! marginal — POC: open a markdown file, navigate by block, annotate ranges.
//!
//! Usage:
//!   marginal FILE.md [--result PATH]
//!   marginal --dump-blocks FILE.md     (headless; prints the block table)

mod app;
mod blocks;
mod editor;
mod highlight;
mod ui;

use std::io;
use std::process::ExitCode;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use app::{App, Mode, Sel};

const USAGE: &str = "usage: marginal [--dump-blocks] [--result PATH] FILE";

struct Args {
    file: String,
    result: Option<String>,
    dump: bool,
}

/// `Ok(None)` means help was asked for, which is not a failure.
fn parse_args() -> Result<Option<Args>, String> {
    let mut file = None;
    let mut result = None;
    let mut dump = false;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dump-blocks" => dump = true,
            "--result" => {
                result = Some(it.next().ok_or("--result needs a path")?);
            }
            "-h" | "--help" => return Ok(None),
            other if other.starts_with('-') => return Err(format!("unknown flag: {other}")),
            other => file = Some(other.to_string()),
        }
    }
    Ok(Some(Args {
        file: file.ok_or("no input file")?,
        result,
        dump,
    }))
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

    let mut app = App::new(args.file.clone(), &src);
    match run(&mut app) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("marginal: {e}");
            return ExitCode::from(2);
        }
    }

    if let Some(path) = &args.result {
        let json = serde_json::to_string_pretty(&app.result()).expect("serialize");
        if let Err(e) = std::fs::write(path, json) {
            eprintln!("marginal: cannot write {path}: {e}");
            return ExitCode::from(2);
        }
    }

    let feedback = app.feedback_markdown();
    if !feedback.is_empty() {
        print!("{feedback}");
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
    let mut scroll = 0usize;
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

    match app.mode {
        // Readline bindings, as bash has trained everyone to expect.
        Mode::Input => {
            let alt = k.modifiers.contains(KeyModifiers::ALT);
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
        // Paging. C-f/C-b keep two lines of overlap, as vim does.
        Mode::Normal if ctrl => match code {
            KeyCode::Char('d') => app.page(1, true),
            KeyCode::Char('u') => app.page(-1, true),
            KeyCode::Char('f') => app.page(1, false),
            KeyCode::Char('b') => app.page(-1, false),
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
            KeyCode::Char('0') | KeyCode::Home => app.goto_line_start(),
            KeyCode::Char('$') | KeyCode::End => app.goto_line_end(),
            KeyCode::Char('g') => app.goto_first(),
            KeyCode::Char('G') => app.goto_last(),
            KeyCode::Char('v') => app.toggle_blocks(),
            KeyCode::Char('V') => app.toggle_lines(),
            // widen / narrow along the markdown hierarchy
            KeyCode::Char('+' | '=') => app.expand(),
            KeyCode::Char('-' | '_') => app.contract(),
            KeyCode::Char('c') => app.begin_comment(),
            KeyCode::Char('x') => app.remove_at_cursor(),
            KeyCode::Esc => app.sel = Sel::Here,
            _ => {}
        },
    }
}
