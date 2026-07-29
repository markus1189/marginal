//! annot-tui — POC: open a markdown file, navigate by block, annotate ranges.
//!
//! Usage:
//!   annot-tui FILE.md [--result PATH]
//!   annot-tui --dump-blocks FILE.md     (headless; prints the block table)

mod app;
mod blocks;
mod ui;

use std::io;
use std::process::ExitCode;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};

use app::{App, Mode, Sel};

struct Args {
    file: String,
    result: Option<String>,
    dump: bool,
}

fn parse_args() -> Result<Args, String> {
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
            "-h" | "--help" => return Err("usage: annot-tui [--dump-blocks] [--result PATH] FILE".into()),
            other if other.starts_with('-') => return Err(format!("unknown flag: {other}")),
            other => file = Some(other.to_string()),
        }
    }
    Ok(Args {
        file: file.ok_or("no input file")?,
        result,
        dump,
    })
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("annot-tui: {e}");
            return ExitCode::from(2);
        }
    };

    let src = match std::fs::read_to_string(&args.file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("annot-tui: cannot read {}: {e}", args.file);
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
            eprintln!("annot-tui: {e}");
            return ExitCode::from(2);
        }
    }

    if let Some(path) = &args.result {
        let json = serde_json::to_string_pretty(&app.result()).expect("serialize");
        if let Err(e) = std::fs::write(path, json) {
            eprintln!("annot-tui: cannot write {path}: {e}");
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
            Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => handle_key(app, k.code),
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

fn handle_key(app: &mut App, code: KeyCode) {
    match app.mode {
        Mode::Input => match code {
            KeyCode::Enter => app.commit_comment(),
            KeyCode::Esc => app.cancel_input(),
            KeyCode::Backspace => {
                app.input.pop();
            }
            KeyCode::Char(c) => app.input.push(c),
            _ => {}
        },
        Mode::Normal => match code {
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
            KeyCode::Char('+') | KeyCode::Char('=') => app.expand(),
            KeyCode::Char('-') | KeyCode::Char('_') => app.contract(),
            KeyCode::Char('c') => app.begin_comment(),
            KeyCode::Char('x') => app.remove_at_cursor(),
            KeyCode::Esc => app.sel = Sel::Here,
            _ => {}
        },
    }
}
