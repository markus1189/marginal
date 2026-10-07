//! marginal — POC: open a document, navigate by block, annotate ranges.
//!
//! Usage:
//!   marginal FILE.md [--result PATH]
//!   marginal --format plain FILE.tex   (any text file, paragraph-wise)
//!   marginal --dump-blocks FILE.md     (headless; prints the block table)

mod app;
mod blocks;
mod editor;
mod format;
mod help;
mod highlight;
mod plain;
mod table;
mod ui;
mod wrap;

use std::io;
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use app::{App, Mode};
use format::Format;

const USAGE: &str = "usage: marginal [--dump-blocks] [--raw] [--result[=]PATH] \
                     [--label[=]NAME] [--format[=]NAME] [--] FILE";

struct Args {
    file: String,
    result: Option<String>,
    label: Option<String>,
    /// `None` means "ask the extension", resolved once in `main` so the dump
    /// and the TUI cannot answer it differently.
    format: Option<Format>,
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

/// A `--format` value, or an error naming what would have worked. Unlike
/// `--label`, this flag has a closed set of values, so a typo is worth catching
/// before the session rather than after it — opening a `.tex` file through the
/// markdown parser is quiet, and every unit in it is wrong.
fn named_format(name: &str) -> Result<Format, String> {
    Format::from_name(name).ok_or_else(|| {
        format!(
            "unknown format: {name} (known: {})",
            format::NAMES.join(", ")
        )
    })
}

fn parse_argv(argv: Vec<String>) -> Result<Option<Args>, String> {
    let mut file = None;
    let mut result = None;
    let mut label = None;
    let mut format = None;
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
    //
    // One operand, and a second is refused rather than silently winning: `a.md
    // b.md` used to open `b.md` without a word, so a glob that matched two files
    // reviewed whichever sorted last and filed the verdict under that name.
    let mut ended = false;
    let mut it = argv.into_iter();
    let mut operand = |a: String| match file.replace(a) {
        None => Ok(()),
        Some(first) => Err(format!(
            "one FILE only, got {first} and {}",
            file.as_deref().unwrap_or_default()
        )),
    };
    while let Some(a) = it.next() {
        if ended {
            operand(a)?;
            continue;
        }
        match a.as_str() {
            "--" => ended = true,
            "--dump-blocks" => dump = true,
            "--raw" => pretty = false,
            "--result" => result = Some(value(it.next(), "--result", "a path")?),
            "--label" => label = Some(value(it.next(), "--label", "a name")?),
            "--format" => format = Some(named_format(&value(it.next(), "--format", "a name")?)?),
            "-h" | "--help" => return Ok(None),
            _ => {
                if let Some(v) = a.strip_prefix("--result=") {
                    result = Some(v.to_string());
                } else if let Some(v) = a.strip_prefix("--label=") {
                    label = Some(v.to_string());
                } else if let Some(v) = a.strip_prefix("--format=") {
                    format = Some(named_format(v)?);
                } else if a.starts_with('-') {
                    return Err(format!("unknown flag: {a}"));
                } else {
                    operand(a)?;
                }
            }
        }
    }
    Ok(Some(Args {
        file: file.ok_or("no input file")?,
        result,
        label,
        format,
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
/// and the directory is answered for by a probe file of this process's own.
/// Nothing that pre-flight did not create is ever opened for creation or
/// removed, at any interleaving. A pre-flight still leaves no result file
/// behind, so it cannot make a session that never ran look like one that did.
///
/// The directory is asked even when the file exists, because the result is
/// written by `write_atomic` — a temporary file beside the target, renamed over
/// it — so a writable file in a directory that takes no new file is a result
/// that cannot be saved. An existing file must still be writable itself: the
/// rename would replace a read-only one, and pre-flight refused those before.
fn preflight(path: &str) -> io::Result<()> {
    let target = write_target(std::path::Path::new(path));

    // No `create`, no `truncate`: an existing result file survives the question
    // untouched, and a symlink is followed rather than replaced.
    let absent = match std::fs::OpenOptions::new().write(true).open(&target) {
        Ok(_) => None,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Some(e),
        Err(e) => return Err(e),
    };

    // The real question is whether the directory takes a new file. `""`, `/`
    // and `..` name no file to create, and for those the open's own error is
    // already the answer.
    let Some(dir) = containing_dir(&target) else {
        return Err(absent.unwrap_or_else(|| io::Error::other("names no file")));
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

/// The directory a file at `target` lives in — `.` for a bare name — or `None`
/// when `target` names no file at all (`""`, `/`, `..`).
fn containing_dir(target: &std::path::Path) -> Option<&std::path::Path> {
    target.file_name()?;
    let dir = target.parent()?;
    Some(if dir.as_os_str().is_empty() {
        std::path::Path::new(".")
    } else {
        dir
    })
}

/// Replace the file `path` resolves to with `contents`, all or nothing.
///
/// `fs::write` truncates first and writes second, so a process killed between
/// the two — or a disk that fills half-way — left an empty or torn result file,
/// which a launcher then reads as a verdict or fails to parse. Written to a
/// temporary file beside the target instead, flushed to disk, and renamed over
/// it: a reader sees the old file or the new one, never a mix.
///
/// The target is the far end of any symlink chain (`write_target`), so the
/// rename replaces the file a link points at and the link survives — the same
/// indirection `preflight` is careful to keep. An existing file's permissions
/// carry over to its replacement.
fn write_atomic(path: &str, contents: &str) -> io::Result<()> {
    use io::Write as _;
    let target = write_target(std::path::Path::new(path));
    let (Some(dir), Some(name)) = (containing_dir(&target), target.file_name()) else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "names no file"));
    };
    let tmp = dir.join(format!(
        ".{}.marginal-{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    let create = || {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
    };
    let write = || -> io::Result<()> {
        // The name carries this pid, so one already there is a leftover of an
        // earlier process that had the same pid and was killed mid-write.
        let mut f = match create() {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                std::fs::remove_file(&tmp)?;
                create()?
            }
            f => f?,
        };
        if let Ok(meta) = std::fs::metadata(&target) {
            f.set_permissions(meta.permissions())?;
        }
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, &target)
    };
    let written = write();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// The result file as written: the review, and whether the session it came
/// from is over.
///
/// `final` is what separates a verdict from a snapshot. The file is rewritten
/// after every change to the annotations while the session runs, with `final:
/// false`, so a session that is killed outright still leaves every committed
/// annotation behind; only the write in `finish` after the human quits says
/// `true`. See README, "The result file".
#[derive(serde::Serialize)]
struct Saved {
    #[serde(flatten)]
    outcome: app::Outcome,
    #[serde(rename = "final")]
    done: bool,
}

fn result_json(app: &App, done: bool) -> String {
    let saved = Saved {
        outcome: app.result(),
        done,
    };
    serde_json::to_string_pretty(&saved).expect("an Outcome always serialises")
}

/// Keeps the result file in step with the annotations while the session runs.
///
/// Every event is followed by a snapshot, and the snapshot is written only when
/// it differs from the last one written — which, since nothing else in the
/// result moves, means after an annotation was added, removed or changed.
/// Before the first change nothing is written at all, so a session killed
/// before anyone commented still leaves no file, and "no file, no verdict"
/// holds for it exactly as before.
struct Autosave<'a> {
    path: Option<&'a str>,
    last: String,
}

impl<'a> Autosave<'a> {
    fn new(app: &App, path: Option<&'a str>) -> Self {
        let last = path.map_or_else(String::new, |_| result_json(app, false));
        Self { path, last }
    }

    fn after_event(&mut self, app: &mut App) {
        let Some(path) = self.path else { return };
        let now = result_json(app, false);
        if now == self.last {
            return;
        }
        // Not fatal: the write in `finish` is tried regardless, and the
        // feedback on stdout is the rescue if that fails too. But the human
        // should know their work is not reaching the disk.
        match write_atomic(path, &now) {
            Ok(()) => self.last = now,
            Err(e) => app.status = format!("autosave failed: {e}"),
        }
    }
}

/// Would writing the result to `result` overwrite the document under review?
///
/// Asked of the file a write would land on — through the same symlink chain
/// `preflight` follows — and answered by device and inode, so a hard link, a
/// `./` prefix or a symlink to the document is caught as surely as the same
/// spelling twice. `marginal --result victim.md victim.md` used to replace the
/// document with its own review JSON on exit, and the review was the only thing
/// left that quoted it.
///
/// A target that does not exist yet cannot be the document; anything else this
/// cannot stat is left for `preflight` to report.
#[cfg(unix)]
fn clobbers_input(input: &str, result: &str) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    let target = write_target(std::path::Path::new(result));
    match (std::fs::metadata(input), std::fs::metadata(target)) {
        (Ok(a), Ok(b)) => (a.dev(), a.ino()) == (b.dev(), b.ino()),
        _ => false,
    }
}

/// Elsewhere there is no inode to ask, so the canonical paths stand in: a hard
/// link gets through there, a symlink and a second spelling do not.
#[cfg(not(unix))]
fn clobbers_input(input: &str, result: &str) -> bool {
    let target = write_target(std::path::Path::new(result));
    match (std::fs::canonicalize(input), std::fs::canonicalize(target)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
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

    // One decision, made before either consumer, so `--dump-blocks` cannot
    // report a parse the TUI would not have used.
    let format = args.format.unwrap_or_else(|| Format::of_path(&args.file));

    if args.dump {
        use io::Write;
        let stdout = io::stdout();
        let mut out = stdout.lock();
        for b in format.parse(&src) {
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
        if clobbers_input(&args.file, path) {
            eprintln!(
                "marginal: --result {path} is the file under review; \
                 the review would overwrite it"
            );
            return ExitCode::from(2);
        }
        if let Err(e) = preflight(path) {
            eprintln!("marginal: cannot write {path}: {e}");
            return ExitCode::from(2);
        }
    }

    let mut app = App::open(args.file.clone(), &src, format);
    app.label = args.label;
    app.pretty = args.pretty;
    let end = match run(&mut app, args.result.as_deref()) {
        Ok(end) => end,
        Err(e) => {
            eprintln!("marginal: {e}");
            return ExitCode::from(2);
        }
    };

    // `finish` is the last thing standing between the annotations and the
    // void, so it gets the same guard as the loop: a panic in it is exit 2,
    // with the autosaved snapshot still on disk, rather than 101.
    let (feedback, code) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        finish(&app, args.result.as_deref(), &end)
    }))
    .unwrap_or_else(|_| (String::new(), 2));
    if !feedback.is_empty() {
        // Not `print!`: it panics when stdout is gone, and after a hangup it is.
        use io::Write as _;
        let mut out = io::stdout();
        let _ = out.write_all(feedback.as_bytes());
        let _ = out.flush();
    }
    ExitCode::from(code)
}

/// A line on stderr that cannot fail. `eprintln!` panics when the write does,
/// and once the terminal has hung up it does — so every word said after the
/// session goes through here instead.
fn say(msg: &str) {
    use io::Write as _;
    let _ = writeln!(io::stderr(), "marginal: {msg}");
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
///
/// Every ending goes through here, not just a quit — a signal, a terminal that
/// failed or went away, a panic in the loop. Those used to `return` from `main`
/// with exit 2 and nothing written, which is the same early return the
/// paragraph above describes, reached by a different door. Only a quit is the
/// human's verdict, so only a quit writes `final: true` and grades 0/1; the
/// rest write what there is with `final: false` and exit 2.
fn finish(app: &App, result: Option<&str>, end: &End) -> (String, u8) {
    let mut failed = false;
    if let Some(path) = result {
        if let Err(e) = write_atomic(path, &result_json(app, end.is_quit())) {
            say(&format!("cannot write {path}: {e}"));
            failed = true;
        }
    }
    if let Some(why) = end.reason() {
        say(&format!("session ended early: {why}"));
    }

    let feedback = app.feedback_markdown();
    // 2 for a failed write, but the markdown still travels with it, because
    // that is exactly when it is the last copy of the annotations.
    let code = if failed || !end.is_quit() {
        2
    } else {
        u8::from(!app.annotations.is_empty())
    };
    (feedback, code)
}

/// How often the loop looks up from waiting for input: to notice a signal, and
/// a terminal that is no longer there. Idle ticks draw nothing.
const TICK: Duration = Duration::from_millis(200);

/// How a session ended. Only `Quit` is the human's own.
#[derive(Debug)]
enum End {
    Quit,
    /// SIGTERM, SIGHUP or SIGINT, by number.
    Signal(usize),
    /// A draw or a read failed, or the terminal hung up.
    Failed(io::Error),
    Panicked,
}

impl End {
    const fn is_quit(&self) -> bool {
        matches!(self, Self::Quit)
    }

    fn reason(&self) -> Option<String> {
        match self {
            Self::Quit => None,
            Self::Signal(n) => Some(format!("stopped by {}", signal_name(*n))),
            Self::Failed(e) => Some(format!("terminal failed: {e}")),
            Self::Panicked => Some("internal error (panic)".into()),
        }
    }
}

/// The signals that end a session through `finish` instead of killing it with
/// the review in memory. SIGINT is here although raw mode turns `C-c` into a
/// key, because `kill -INT` and a parent's process-group interrupt still send it.
#[cfg(unix)]
const STOP_SIGNALS: [i32; 3] = [
    signal_hook::consts::SIGTERM,
    signal_hook::consts::SIGHUP,
    signal_hook::consts::SIGINT,
];
#[cfg(not(unix))]
const STOP_SIGNALS: [i32; 2] = [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT];

fn signal_name(n: usize) -> String {
    use signal_hook::consts::{SIGINT, SIGTERM};
    match i32::try_from(n) {
        Ok(SIGTERM) => "SIGTERM".into(),
        Ok(SIGINT) => "SIGINT".into(),
        #[cfg(unix)]
        Ok(signal_hook::consts::SIGHUP) => "SIGHUP".into(),
        _ => format!("signal {n}"),
    }
}

/// Turn the stop signals into a number in `stop` rather than a death.
///
/// Each of them used to kill the process with its default action: the
/// annotations went with it, and SIGTERM left the terminal raw and on the
/// alternate screen. signal-hook's flag handlers only store an atomic — the one
/// thing a signal handler can soundly do — and the loop reads it every `TICK`.
fn catch_stop_signals(stop: &Arc<AtomicUsize>) -> io::Result<()> {
    for sig in STOP_SIGNALS {
        let n = usize::try_from(sig).map_err(io::Error::other)?;
        signal_hook::flag::register_usize(sig, Arc::clone(stop), n)?;
    }
    Ok(())
}

/// What the session loop needs from a terminal. A trait so the loop — every
/// way a session can end — runs headless under `cargo test`.
trait Screen {
    fn draw(&mut self, app: &mut App) -> io::Result<()>;
    /// The next event, or `None` when `wait` passed without one.
    fn next(&mut self, wait: Duration) -> io::Result<Option<Event>>;
    /// Is there still a terminal? Asked on every idle tick.
    fn alive(&self) -> bool;
}

/// The session: draw, wait, act, until something ends it. Never returns early
/// with the review in memory — every way out is an `End` for `finish`.
///
/// A panic anywhere inside is caught and reported as `End::Panicked`, so it
/// is exit 2 with the result written instead of exit 101 with it lost. The
/// `AssertUnwindSafe` is sound in the sense that matters here: after a panic
/// `app` may hold a half-finished edit, but every field is still a valid
/// value, and all that is read from it afterwards is the annotation list —
/// which is also already on disk as of the last completed event.
fn run_loop(
    app: &mut App,
    screen: &mut impl Screen,
    stop: &AtomicUsize,
    save: &mut Autosave,
) -> End {
    let session = std::panic::AssertUnwindSafe(|| {
        let mut dirty = true;
        loop {
            let sig = stop.load(Ordering::Relaxed);
            if sig != 0 {
                return End::Signal(sig);
            }
            if dirty {
                if let Err(e) = screen.draw(app) {
                    return End::Failed(e);
                }
                dirty = false;
            }
            match screen.next(TICK) {
                Ok(Some(ev)) => {
                    handle_event(app, ev);
                    save.after_event(app);
                    dirty = true;
                }
                Ok(None) if screen.alive() => {}
                Ok(None) => return End::Failed(io::Error::other("the terminal went away")),
                Err(e) => return End::Failed(e),
            }
            if app.quit {
                return End::Quit;
            }
        }
    });
    std::panic::catch_unwind(session).unwrap_or(End::Panicked)
}

fn run(app: &mut App, result: Option<&str>) -> io::Result<End> {
    if !io::IsTerminal::is_terminal(&io::stdout()) {
        return Err(io::Error::other(
            "stdout is not a terminal (run under a real tty, or use --dump-blocks)",
        ));
    }
    let stop = Arc::new(AtomicUsize::new(0));
    catch_stop_signals(&stop)?;
    let mut tty = Tty::open()?;
    let mut save = Autosave::new(app, result);
    let end = run_loop(app, &mut tty, &stop, &mut save);
    // Restored before `finish` says a word, so what it prints lands on the
    // normal screen rather than on an alternate one about to be discarded.
    drop(tty);
    Ok(end)
}

/// The real terminal: raw mode, the alternate screen and bracketed paste for
/// as long as this value lives, however the session ends.
///
/// Bracketed paste because without it a terminal delivers a paste as the
/// keystrokes that would type it, and the first newline in it is Enter: the
/// comment committed half-way, and the rest of the clipboard ran as Normal-mode
/// commands — `see:\nxx` committed `see:`, then `x`, `x` removed two
/// annotations.
///
/// Set up by hand rather than by `ratatui::init`, for one reason: every
/// teardown step in ratatui ends in `eprintln!` on failure — `restore()`, its
/// panic hook, and `Terminal`'s `Drop` showing the cursor — and `eprintln!`
/// panics when stderr is a terminal that has hung up. That is exactly the
/// terminal a hangup leaves behind, so the teardown here ignores every error,
/// and the `Terminal` is never dropped (its only `Drop` work is the cursor,
/// which `restore_tty` shows).
struct Tty {
    terminal: std::mem::ManuallyDrop<ratatui::DefaultTerminal>,
    scroll: app::Anchor,
    events: mpsc::Receiver<io::Result<Event>>,
}

/// Undo everything `Tty::open` did, in an order that works from any state,
/// ignoring every error: this runs on a dying terminal as often as a live one.
fn restore_tty() {
    use crossterm::{cursor::Show, event::DisableBracketedPaste, terminal::LeaveAlternateScreen};
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = crossterm::execute!(
        io::stdout(),
        DisableBracketedPaste,
        LeaveAlternateScreen,
        Show
    );
}

impl Tty {
    fn open() -> io::Result<Self> {
        use crossterm::{event::EnableBracketedPaste, terminal::EnterAlternateScreen};
        // The panic message has to land on the normal screen, so the terminal
        // is restored before the default hook prints it — the job ratatui's
        // hook did, minus its `eprintln!`.
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_tty();
            hook(info);
        }));
        let setup = || {
            crossterm::terminal::enable_raw_mode()?;
            crossterm::execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
            ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(io::stdout()))
        };
        let terminal = setup().inspect_err(|_| restore_tty())?;
        Ok(Self {
            terminal: std::mem::ManuallyDrop::new(terminal),
            scroll: app::Anchor::default(),
            events: spawn_reader(),
        })
    }
}

impl Drop for Tty {
    fn drop(&mut self) {
        restore_tty();
    }
}

/// Terminal events, read on a thread of their own and handed over a channel.
///
/// Not `event::poll` on the main thread, because crossterm's unix reader
/// never returns once the terminal has hung up: a `read` of `Ok(0)` or `EIO`
/// is retried in a loop that checks no timeout, so the process spun one core
/// forever on a dead tty (measured: 200 ticks per 2 s, indefinitely, with
/// SIGHUP ignored). Here the main thread waits on the channel with a timeout,
/// so it always gets back control to notice the hangup and end the session —
/// and the spinning reader dies with the process.
fn spawn_reader() -> mpsc::Receiver<io::Result<Event>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || loop {
        let ev = event::read();
        let failed = ev.is_err();
        if tx.send(ev).is_err() || failed {
            return;
        }
    });
    rx
}

impl Screen for Tty {
    fn draw(&mut self, app: &mut App) -> io::Result<()> {
        let scroll = &mut self.scroll;
        self.terminal.draw(|f| ui::draw(f, app, scroll)).map(|_| ())
    }

    fn next(&mut self, wait: Duration) -> io::Result<Option<Event>> {
        match self.events.recv_timeout(wait) {
            Ok(ev) => ev.map(Some),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(io::Error::other("the terminal event reader stopped"))
            }
        }
    }

    /// `isatty` asks the terminal driver (`TCGETS`), which answers `EIO` once
    /// the terminal has hung up — the check that notices a dead tty whether or
    /// not a SIGHUP ever reached this process.
    fn alive(&self) -> bool {
        io::IsTerminal::is_terminal(&io::stdout())
    }
}

/// One terminal event. Key releases and repeats, focus, mouse and resize carry
/// nothing to act on — a resize is picked up by the redraw every event causes.
fn handle_event(app: &mut App, ev: Event) {
    match ev {
        Event::Key(k) if k.kind == KeyEventKind::Press => handle_key(app, k),
        Event::Paste(s) => handle_paste(app, &s),
        _ => {}
    }
}

/// A paste is text for the comment editor and nothing else. Outside it there
/// is no text to put it in, and replaying it as keys is the bug bracketed paste
/// exists to prevent — so it is dropped whole, with a word on the status line.
fn handle_paste(app: &mut App, s: &str) {
    match app.mode {
        Mode::Input => app.editor.paste(s),
        Mode::Normal => app.status = "paste ignored".into(),
    }
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
                // A row up or down while there is one; the history only from
                // the first or last row. Up/Down used to be history alone, so
                // in a multi-line comment the arrow meant to reach the row
                // above swapped the whole buffer for an older comment. `C-p`
                // and `C-n` stay pure history.
                KeyCode::Up => {
                    if !e.up() {
                        e.history_prev();
                    }
                }
                KeyCode::Down => {
                    if !e.down() {
                        e.history_next();
                    }
                }

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
        // The `?` overlay, like peek, swallows everything but its own keys: it
        // covers the screen, so a key acting on what is underneath would act
        // on something the reader cannot see.
        Mode::Normal if app.help.is_some() => match code {
            KeyCode::Char('?' | 'q') | KeyCode::Esc if !ctrl => app.toggle_help(),
            KeyCode::Char('j') | KeyCode::Down if !ctrl => app.scroll_help(1),
            KeyCode::Char('k') | KeyCode::Up if !ctrl => app.scroll_help(-1),
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
            // Capital for the document-level variant of the same action.
            KeyCode::Char('C') => app.begin_general(),
            KeyCode::Char('y') => app.place("yes"),
            KeyCode::Char('n') => app.place("no"),
            KeyCode::Char('x') => app.remove_at_cursor(),
            // Edit in place: the line's annotation, or the newest general one.
            KeyCode::Char('e') => app.edit_at_cursor(),
            KeyCode::Char('E') => app.edit_general(),
            // `]`/`[` rather than `n`/`N`: vim already spells "next/previous
            // change hunk" with brackets, and `n` is the one-key `no`.
            KeyCode::Char(']') => app.goto_mark(1),
            KeyCode::Char('[') => app.goto_mark(-1),
            // The escape hatch for a document that is one long numbered list,
            // where the steps outnumber everything else in the ring. `#` because
            // it is what a numbered item is made of, and because every letter
            // near the mark keys is spoken for.
            KeyCode::Char('#') => app.toggle_steps(),
            KeyCode::Char('?') => app.toggle_help(),
            KeyCode::Esc => app.clear_selection(),
            _ => {}
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Sel;

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

    /// `--result victim.md victim.md` passed pre-flight — the file is writable,
    /// which is exactly the problem — and `finish` then replaced the document
    /// with its review JSON. Every spelling of "the same file" is refused: the
    /// path itself, a different spelling of it, a symlink to it and a hard link
    /// to it. A different file with the same contents is not the same file.
    #[cfg(unix)]
    #[test]
    fn a_result_path_that_is_the_input_file_is_refused() {
        let dir = scratch("clobber");
        let doc = dir.join("victim.md");
        std::fs::write(&doc, "# keep me\n").unwrap();
        let s = |p: &std::path::Path| p.to_str().unwrap().to_string();
        let input = s(&doc);

        let dotted = format!("{}/./victim.md", s(&dir));
        let sym = dir.join("link.json");
        std::os::unix::fs::symlink(&doc, &sym).unwrap();
        let hard = dir.join("hard.json");
        std::fs::hard_link(&doc, &hard).unwrap();
        for same in [input.clone(), dotted, s(&sym), s(&hard)] {
            assert!(clobbers_input(&input, &same), "{same} slipped through");
        }

        let twin = dir.join("twin.md");
        std::fs::write(&twin, "# keep me\n").unwrap();
        assert!(!clobbers_input(&input, &s(&twin)), "a copy is not the file");
        assert!(!clobbers_input(&input, &s(&dir.join("fresh.json"))));
        // A dangling symlink lands on a file that does not exist yet.
        let dangling = dir.join("dangling.json");
        std::os::unix::fs::symlink(dir.join("nothing-yet.json"), &dangling).unwrap();
        assert!(!clobbers_input(&input, &s(&dangling)));

        assert_eq!(std::fs::read_to_string(&doc).unwrap(), "# keep me\n");
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

    /// `--format` is the only flag with a closed set of values, so it is the
    /// only one where a typo can be caught, and it must be: the wrong backend
    /// is silent. Opening a `.tex` file as markdown does not fail, it produces
    /// a unit list built from `#` and `_` that mean nothing there, and every
    /// comment filed against it names lines the reviewer did not select.
    #[test]
    fn format_takes_a_known_name_in_both_spellings_and_nothing_else() {
        let ok = |a: &[&str]| argv(a).unwrap().unwrap();

        assert_eq!(ok(&["f.tex"]).format, None, "unset means ask the extension");
        assert_eq!(
            ok(&["--format", "plain", "f.tex"]).format,
            Some(Format::Plain)
        );
        assert_eq!(
            ok(&["--format=markdown", "f.tex"]).format,
            Some(Format::Markdown)
        );

        // A name no backend answers to is refused, and the message says what
        // would have worked — including through the `=` spelling, which takes
        // its value verbatim and so is the one that reaches `named_format`
        // without the dash guard in front of it.
        for bad in [
            vec!["--format", "latex", "f.tex"],
            vec!["--format=latex", "f.tex"],
            vec!["--format=", "f.tex"],
            // Right idea, wrong spelling: the extension, not the format name.
            vec!["--format=md", "f.md"],
        ] {
            let e = argv(&bad).err().unwrap_or_else(|| panic!("{bad:?} parsed"));
            assert!(e.contains("unknown format"), "{bad:?}: {e}");
            assert!(e.contains("markdown, plain"), "{bad:?}: {e}");
        }

        // And it obeys the rules every other valued flag obeys.
        assert!(argv(&["--format", "--raw", "f.md"]).is_err());
        assert!(argv(&["f.md", "--format"]).is_err());
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
        // Only the first `--` is the marker; the second is an ordinary operand
        // — so `-- -- f.md` names two files, which is refused like any two.
        assert_eq!(file(&["--", "--"]), "--");
        assert!(argv(&["--", "--", "f.md"]).is_err());
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

    /// Two operands used to mean "the last one": `marginal --dump-blocks a.md
    /// b.md` dumped `b.md` and said nothing about `a.md`, and a TUI run filed
    /// its verdict under whichever name a glob sorted last. Refused now, on
    /// either side of `--`, with both names in the message.
    #[test]
    fn a_second_file_is_refused_not_silently_preferred() {
        for bad in [
            &["a.md", "b.md"][..],
            &["--dump-blocks", "a.md", "b.md"],
            &["a.md", "--", "b.md"],
            &["--", "a.md", "b.md"],
            &["a.md", "--raw", "b.md"],
        ] {
            let e = argv(bad).err().unwrap_or_else(|| panic!("{bad:?} parsed"));
            assert!(e.contains("a.md") && e.contains("b.md"), "{bad:?}: {e}");
        }
        // One operand, however it is reached, still parses.
        assert_eq!(argv(&["--raw", "a.md"]).unwrap().unwrap().file, "a.md");
        assert_eq!(argv(&["--", "a.md"]).unwrap().unwrap().file, "a.md");
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
        let mut app = App::open("t.md".into(), DOC, Format::Markdown);
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

    /// `n` is the one-key `no`, `C-n` is a row motion and `M-n` binds
    /// nothing. Only the first of the three may annotate.
    #[test]
    fn plain_y_and_n_answer_and_their_chords_do_not() {
        let mut app = App::open("t.md".into(), DOC, Format::Markdown);
        handle_key(&mut app, key('n', KeyModifiers::CONTROL));
        handle_key(&mut app, key('n', KeyModifiers::ALT));
        handle_key(&mut app, key('y', KeyModifiers::ALT));
        assert!(app.annotations.is_empty(), "a chord answered");

        handle_key(&mut app, key('y', KeyModifiers::NONE));
        handle_key(&mut app, key('n', KeyModifiers::NONE));
        let texts: Vec<_> = app.annotations.iter().map(|a| a.text.as_str()).collect();
        assert_eq!(texts, ["no"]);
    }

    fn annotated() -> App {
        let mut app = App::open("t.md".into(), DOC, Format::Markdown);
        handle_key(&mut app, key('c', KeyModifiers::NONE));
        app.editor.set("keep me");
        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.annotations.len(), 1, "setup failed");
        app
    }

    fn peeking() -> App {
        let mut app = App::open("t.md".into(), DOC, Format::Markdown);
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

            let mut app = App::open("t.md".into(), DOC, Format::Markdown);
            handle_key(&mut app, key('c', m));
            assert!(app.quit, "normal mode: C-c with {m:?} did not quit");

            let mut app = peeking();
            handle_key(&mut app, key('c', m));
            assert!(app.quit, "peek overlay: C-c with {m:?} did not quit");

            // In Input mode C-c is cancel, not quit — the escape hatch out of
            // the comment editor, and equally unreachable if a stray modifier
            // can turn it into an ordinary keystroke.
            let mut app = App::open("t.md".into(), DOC, Format::Markdown);
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

            let mut app = App::open("t.md".into(), DOC, Format::Markdown);
            handle_key(&mut app, key('q', m));
            assert_eq!(app.quit, bare, "q with {m:?}");

            let mut app = App::open("t.md".into(), DOC, Format::Markdown);
            handle_key(&mut app, key('c', m));
            if m.contains(KeyModifiers::CONTROL) {
                assert!(app.quit, "C-c with {m:?} is the exit");
            } else {
                assert_eq!(app.mode == Mode::Input, bare, "c with {m:?}");
            }
        }

        // …and the capitals, which only arrive with SHIFT set, still act.
        let mut app = App::open("t.md".into(), DOC, Format::Markdown);
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
            let mut app = App::open("t.md".into(), DOC, Format::Markdown);
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
                let mut app = App::open("t.md".into(), &doc, Format::Markdown);
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
        let mut app = App::open("t.md".into(), DOC, Format::Markdown);
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
        let mut app = App::open("t.md".into(), DOC, Format::Markdown);
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

    /// Up in a multi-line comment recalled an older comment over the whole
    /// buffer instead of going to the row above. Now the arrows walk the rows
    /// and reach the history only from the first or last one; `C-p`/`C-n`
    /// are still the history from anywhere.
    #[test]
    fn up_and_down_walk_the_rows_before_the_history() {
        let plain = |c: KeyCode| KeyEvent::new(c, KeyModifiers::NONE);
        let mut app = editing();
        app.editor.set("row one\nrow two\nrow three");
        handle_key(&mut app, plain(KeyCode::Up));
        assert_eq!(
            app.editor.row_col(),
            (1, 7),
            "Up did not go to the row above"
        );
        handle_key(&mut app, plain(KeyCode::Up));
        assert_eq!(app.editor.row_col(), (0, 7));
        assert_eq!(app.editor.text(), "row one\nrow two\nrow three");

        // From the first row, Up is the history — and Down comes back to the
        // draft, whose rows it then walks as rows again.
        handle_key(&mut app, plain(KeyCode::Up));
        assert_eq!(app.editor.text(), "older note");
        handle_key(&mut app, plain(KeyCode::Down));
        assert_eq!(app.editor.text(), "row one\nrow two\nrow three");
        assert_eq!(
            app.editor.row_col(),
            (2, 9),
            "restored with the cursor at the end"
        );
        handle_key(&mut app, plain(KeyCode::Up));
        assert_eq!(app.editor.row_col(), (1, 7));
        handle_key(&mut app, plain(KeyCode::Down));
        assert_eq!(app.editor.row_col(), (2, 7));

        // C-p is the history even from the middle of a multi-line draft.
        handle_key(&mut app, plain(KeyCode::Up));
        handle_key(&mut app, key('p', KeyModifiers::CONTROL));
        assert_eq!(app.editor.text(), "older note");
    }

    /// Esc and `C-c` in the editor discarded the draft for good — a long,
    /// multi-line one included — and `C-c` is also the reflex for "get me out".
    /// Both still leave the editor with nothing committed, but the draft is now
    /// the newest history entry: open a comment, `C-p`, and it is back.
    #[test]
    fn a_cancelled_comment_comes_back_with_one_recall() {
        let draft = "first line\nsecond line";
        for cancel in [
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            key('c', KeyModifiers::CONTROL),
        ] {
            let mut app = App::open("t.md".into(), DOC, Format::Markdown);
            handle_key(&mut app, key('c', KeyModifiers::NONE));
            handle_event(&mut app, Event::Paste(draft.into()));
            handle_key(&mut app, cancel);
            assert_eq!(app.mode, Mode::Normal, "{cancel:?} did not cancel");
            assert!(app.annotations.is_empty(), "{cancel:?} committed");
            assert!(app.status.contains("C-p"), "{}", app.status);

            handle_key(&mut app, key('c', KeyModifiers::NONE));
            assert_eq!(app.editor.text(), "", "a new comment starts empty");
            handle_key(&mut app, key('p', KeyModifiers::CONTROL));
            assert_eq!(app.editor.text(), draft, "{cancel:?} lost the draft");
        }
    }

    /// A paste used to arrive as keystrokes: the newline in `see:\nxx` was
    /// Enter, which committed `see:`, and the two `x` that followed were
    /// Normal-mode `x` — two annotations removed with no undo. With bracketed
    /// paste on, the terminal hands it over as one `Event::Paste`, and this is
    /// what has to happen to it: all of it in the comment, none of it a key.
    #[test]
    fn a_multi_line_paste_is_comment_text_not_keystrokes() {
        let mut app = annotated();
        handle_key(&mut app, key('c', KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Input, "setup failed");
        handle_event(&mut app, Event::Paste("see:\r\nxx\rq".into()));
        assert_eq!(app.mode, Mode::Input, "the paste committed the comment");
        assert_eq!(app.editor.text(), "see:\nxx\nq");
        assert_eq!(app.annotations.len(), 1, "the paste removed an annotation");
        assert!(!app.quit, "the paste quit");

        // Enter is still the only commit, and the comment keeps its rows.
        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.annotations.len(), 2);
        assert_eq!(app.annotations[1].text, "see:\nxx\nq");
    }

    /// Outside the editor there is nowhere for text to go, and replaying it as
    /// keys is the bug itself — so a paste in Normal mode does nothing at all,
    /// over an annotation and behind the peek overlay alike.
    #[test]
    fn a_paste_in_normal_mode_is_ignored_not_replayed() {
        let mut app = annotated();
        let cursor = app.cursor;
        handle_event(&mut app, Event::Paste("xxJjq\nc".into()));
        assert_eq!(
            app.annotations.len(),
            1,
            "x from a paste removed an annotation"
        );
        assert!(!app.quit, "q from a paste quit");
        assert_eq!(
            app.mode,
            Mode::Normal,
            "c or \\n from a paste began a comment"
        );
        assert_eq!(app.cursor, cursor, "j/J from a paste moved the cursor");
        assert!(app.status.contains("paste ignored"), "{}", app.status);

        let mut app = peeking();
        handle_event(&mut app, Event::Paste("q".into()));
        assert!(app.peek && !app.quit, "a paste closed the overlay or quit");
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
        let (feedback, code) = finish(&annotated(), Some(bad.to_str().unwrap()), &End::Quit);
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
        let (feedback, code) = finish(&annotated(), Some(out.to_str().unwrap()), &End::Quit);
        assert_eq!(code, 1, "annotations are changes-requested");
        assert!(feedback.contains("keep me"));
        let json = std::fs::read_to_string(&out).unwrap();
        assert!(json.contains("keep me"), "{json}");
        assert!(json.contains("changes-requested"), "{json}");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["final"], true, "the human quit, so this is the verdict");

        // A clean review prints nothing and exits 0 — but still writes the
        // file, because the result file is the verdict and "no annotations" is
        // a verdict. Only stdout is allowed to be empty here.
        let clean = dir.join("clean.json");
        let app = App::open("t.md".into(), DOC, Format::Markdown);
        let (feedback, code) = finish(&app, Some(clean.to_str().unwrap()), &End::Quit);
        assert_eq!(code, 0);
        assert!(feedback.is_empty(), "{feedback:?}");
        assert!(std::fs::read_to_string(&clean)
            .unwrap()
            .contains("approved"));

        // With no `--result` there is nothing to fail: stdout is the only output
        // and the exit code still splits clean from annotated.
        let (feedback, code) = finish(&annotated(), None, &End::Quit);
        assert_eq!(code, 1);
        assert!(feedback.contains("keep me"));
        let (feedback, code) = finish(
            &App::open("t.md".into(), DOC, Format::Markdown),
            None,
            &End::Quit,
        );
        assert_eq!(code, 0);
        assert!(feedback.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn entries(dir: &std::path::Path) -> Vec<String> {
        let mut v: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    /// `fs::write` truncates and then writes, so a kill between the two left an
    /// empty result file — which a launcher reads as a verdict, or fails to
    /// parse — and a disk that filled half-way left a torn one. The write now
    /// goes to a temporary file that is renamed over the target, and every
    /// property the direct write had is kept: a symlink is written *through*,
    /// not replaced, and the file's permissions survive.
    #[cfg(unix)]
    #[test]
    fn a_result_write_is_all_or_nothing_and_keeps_links_and_modes() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = scratch("atomic");
        let out = dir.join("out.json");
        let arg = out.to_str().unwrap();

        write_atomic(arg, "one").unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "one");
        std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o600)).unwrap();
        write_atomic(arg, "two").unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "two");
        let mode = std::fs::metadata(&out).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the replacement lost the file's mode");
        assert_eq!(entries(&dir), ["out.json"], "a temporary file was left");

        // Through a symlink: the far end is replaced, the link stays a link.
        let link = dir.join("link.json");
        std::os::unix::fs::symlink("out.json", &link).unwrap();
        write_atomic(link.to_str().unwrap(), "three").unwrap();
        assert!(link.is_symlink(), "the rename replaced the link");
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "three");

        // A stale temporary file of this pid is a leftover, not an obstacle.
        std::fs::write(
            dir.join(format!(".out.json.marginal-{}.tmp", std::process::id())),
            "stale",
        )
        .unwrap();
        write_atomic(arg, "four").unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "four");
        assert_eq!(entries(&dir), ["link.json", "out.json"]);

        // A failed write leaves the old file whole and nothing beside it.
        assert!(write_atomic(dir.join("gone/x.json").to_str().unwrap(), "x").is_err());
        assert!(write_atomic("", "x").is_err());
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "four");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The rename needs a new file in the directory, so a writable result file
    /// in a directory that takes none would pass the old pre-flight and then
    /// fail on the first save, after the human had started.
    #[cfg(unix)]
    #[test]
    fn preflight_refuses_a_writable_file_in_a_directory_that_takes_no_new_one() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = scratch("ro-dir");
        let out = dir.join("out.json");
        std::fs::write(&out, "keep").unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        // Root writes anywhere, and then there is nothing to refuse.
        let root = std::fs::write(dir.join("probe"), "").is_ok();
        let refused = preflight(out.to_str().unwrap()).is_err();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        if !root {
            assert!(refused, "a result that cannot be renamed into place passed");
        }
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "keep");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Results were written once, in `finish`, after the last key — so a
    /// session that was killed took every annotation with it. Now each change
    /// is on disk as soon as it is made, marked `final: false`, and a session
    /// in which nothing changed writes nothing: "no file, no verdict" still
    /// holds for a review that never got started.
    #[test]
    fn every_change_to_the_annotations_is_on_disk_before_the_next_key() {
        let dir = scratch("autosave");
        let out = dir.join("out.json");
        let path = out.to_str().unwrap();
        let read = || -> serde_json::Value {
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap()
        };
        let press = |app: &mut App, save: &mut Autosave, k: KeyEvent| {
            handle_event(app, Event::Key(k));
            save.after_event(app);
        };
        let plain = |c: KeyCode| KeyEvent::new(c, KeyModifiers::NONE);

        let mut app = App::open("t.md".into(), DOC, Format::Markdown);
        let mut save = Autosave::new(&app, Some(path));
        press(&mut app, &mut save, plain(KeyCode::Char('j')));
        press(&mut app, &mut save, plain(KeyCode::Char('c')));
        press(&mut app, &mut save, plain(KeyCode::Char('x')));
        assert!(
            !out.exists(),
            "a file appeared before anything was committed"
        );

        press(&mut app, &mut save, plain(KeyCode::Enter));
        let v = read();
        assert_eq!(v["final"], false, "{v}");
        assert_eq!(v["annotations"][0]["text"], "x", "{v}");
        assert_eq!(v["decision"], "changes-requested");

        // Only a change writes: a motion over an unchanged review does not
        // bring back a file somebody else removed.
        std::fs::remove_file(&out).unwrap();
        press(&mut app, &mut save, plain(KeyCode::Char('k')));
        assert!(!out.exists(), "a key that changed nothing rewrote the file");

        // Removal is a change too, and an emptied review says so.
        press(&mut app, &mut save, plain(KeyCode::Char('x')));
        let v = read();
        assert_eq!(v["annotations"].as_array().unwrap().len(), 0, "{v}");
        assert_eq!(v["decision"], "approved");
        assert_eq!(v["final"], false);

        // And the quit overwrites the snapshot with the verdict.
        let (_, code) = finish(&app, Some(path), &End::Quit);
        assert_eq!((code, read()["final"].clone()), (0, true.into()));

        // Without --result there is nowhere to save and nothing is attempted.
        let mut app = annotated();
        let mut none = Autosave::new(&app, None);
        none.after_event(&mut app);
        assert!(!app.status.contains("autosave"), "{}", app.status);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A save that fails is not fatal — `finish` tries again and the feedback
    /// on stdout is the rescue — but it must not be silent either.
    #[test]
    fn a_failing_autosave_says_so_on_the_status_line() {
        let bad = tmp("autosave-no-such-dir").join("out.json");
        let mut app = App::open("t.md".into(), DOC, Format::Markdown);
        let mut save = Autosave::new(&app, bad.to_str());
        handle_key(&mut app, key('c', KeyModifiers::NONE));
        app.editor.set("note");
        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        save.after_event(&mut app);
        assert!(app.status.starts_with("autosave failed"), "{}", app.status);
    }

    /// One scripted step of a fake terminal.
    enum Step {
        Ev(Event),
        /// A tick with no input.
        Idle,
        Fail,
        /// A signal arrives during this wait.
        Signal(i32),
        Panic,
    }

    /// A terminal that plays a script. Running off its end is an error, so a
    /// loop that fails to stop shows up as `End::Failed("script ran out")`
    /// rather than as a hung test.
    struct Fake<'a> {
        steps: std::collections::VecDeque<Step>,
        stop: &'a AtomicUsize,
        alive: bool,
        /// Fail the draw with this 1-based number; 0 never fails.
        fail_draw: usize,
        draws: usize,
        waits: usize,
    }

    impl<'a> Fake<'a> {
        fn new(stop: &'a AtomicUsize, steps: Vec<Step>) -> Self {
            Self {
                steps: steps.into(),
                stop,
                alive: true,
                fail_draw: 0,
                draws: 0,
                waits: 0,
            }
        }
    }

    impl Screen for Fake<'_> {
        fn draw(&mut self, _: &mut App) -> io::Result<()> {
            self.draws += 1;
            if self.draws == self.fail_draw {
                return Err(io::Error::other("draw failed"));
            }
            Ok(())
        }

        fn next(&mut self, _: Duration) -> io::Result<Option<Event>> {
            self.waits += 1;
            match self.steps.pop_front() {
                Some(Step::Ev(e)) => Ok(Some(e)),
                Some(Step::Idle) => Ok(None),
                Some(Step::Fail) => Err(io::Error::other("read failed")),
                Some(Step::Signal(n)) => {
                    self.stop
                        .store(usize::try_from(n).unwrap(), Ordering::Relaxed);
                    Ok(None)
                }
                Some(Step::Panic) => panic!("injected panic mid-session"),
                None => Err(io::Error::other("script ran out")),
            }
        }

        fn alive(&self) -> bool {
            self.alive
        }
    }

    /// `c`, the text, Enter: one committed annotation, as keys.
    fn comment(text: &str) -> Vec<Step> {
        let k = |c| Step::Ev(Event::Key(KeyEvent::new(c, KeyModifiers::NONE)));
        let mut v = vec![k(KeyCode::Char('c'))];
        v.extend(text.chars().map(|c| k(KeyCode::Char(c))));
        v.push(k(KeyCode::Enter));
        v
    }

    /// Drive a whole session against `steps` and finish it, as `main` does.
    /// Returns how it ended, the exit code, the feedback and the result file.
    fn session(
        name: &str,
        steps: Vec<Step>,
        tweak: impl FnOnce(&mut Fake),
    ) -> (End, u8, String, serde_json::Value) {
        let dir = scratch(name);
        let out = dir.join("out.json");
        let path = out.to_str().unwrap();
        let stop = AtomicUsize::new(0);
        let mut app = App::open("t.md".into(), DOC, Format::Markdown);
        let mut save = Autosave::new(&app, Some(path));
        let mut fake = Fake::new(&stop, steps);
        tweak(&mut fake);
        let end = run_loop(&mut app, &mut fake, &stop, &mut save);
        let (feedback, code) = finish(&app, Some(path), &end);
        let json = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        (end, code, feedback, json)
    }

    /// SIGTERM and SIGHUP killed the process outright: the annotations were
    /// only in memory, and SIGTERM left the terminal raw on the alternate
    /// screen. Now a stop signal is a flag the loop reads on its next tick,
    /// and the session goes through `finish` like any other: the review is
    /// written, marked not final, printed, and the exit is 2.
    #[test]
    fn a_stop_signal_ends_the_session_through_finish() {
        for sig in STOP_SIGNALS {
            let mut steps = comment("keep me");
            steps.push(Step::Idle);
            steps.push(Step::Signal(sig));
            let (end, code, feedback, json) = session("signal", steps, |_| {});
            assert!(
                matches!(end, End::Signal(n) if n == usize::try_from(sig).unwrap()),
                "{end:?}"
            );
            assert_eq!(code, 2, "a signal is not the human's verdict");
            assert!(feedback.contains("keep me"), "{feedback:?}");
            assert_eq!(json["final"], false, "{json}");
            assert_eq!(json["annotations"][0]["text"], "keep me");
            assert!(end.reason().unwrap().contains("SIG"), "{:?}", end.reason());
        }
    }

    /// A draw or a read that failed returned from `main` with exit 2 and
    /// nothing written — the early return `finish` exists to prevent, reached
    /// by the other door. Neither is retried: an error ends the session at
    /// once, so a terminal that errors forever cannot keep it spinning.
    #[test]
    fn a_terminal_error_still_writes_the_review() {
        let mut steps = comment("keep me");
        steps.push(Step::Fail);
        steps.push(Step::Idle);
        let (end, code, feedback, json) = session("read-error", steps, |_| {});
        assert!(
            matches!(&end, End::Failed(e) if e.to_string() == "read failed"),
            "{end:?}"
        );
        assert_eq!(code, 2);
        assert!(feedback.contains("keep me"));
        assert_eq!(json["annotations"][0]["text"], "keep me");
        assert_eq!(json["final"], false);

        // A draw that fails: the one after the Enter that committed.
        let steps = comment("keep me");
        let enter_draw = steps.len() + 1;
        let (end, code, feedback, json) =
            session("draw-error", steps, |f| f.fail_draw = enter_draw);
        assert!(
            matches!(&end, End::Failed(e) if e.to_string() == "draw failed"),
            "{end:?}"
        );
        assert_eq!(code, 2);
        assert!(feedback.contains("keep me"));
        assert_eq!(json["annotations"][0]["text"], "keep me");
    }

    /// With SIGHUP ignored, closing the terminal left the process spinning a
    /// whole core forever: crossterm's reader loops on a hung-up tty without
    /// ever returning. The loop now looks up every tick, and a terminal that
    /// is no longer a terminal ends the session on the first idle tick.
    #[test]
    fn a_terminal_that_went_away_ends_the_session_on_the_next_tick() {
        let mut steps = comment("keep me");
        steps.extend((0..50).map(|_| Step::Idle));
        let (end, code, _, json) = session("hangup", steps, |f| f.alive = false);
        assert!(
            matches!(&end, End::Failed(e) if e.to_string().contains("went away")),
            "{end:?}"
        );
        assert_eq!(code, 2);
        assert_eq!(json["annotations"][0]["text"], "keep me");

        // On the first idle tick, not after waiting out the rest.
        let stop = AtomicUsize::new(0);
        let mut app = App::open("t.md".into(), DOC, Format::Markdown);
        let mut save = Autosave::new(&app, None);
        let mut fake = Fake::new(&stop, (0..5).map(|_| Step::Idle).collect());
        fake.alive = false;
        let _ = run_loop(&mut app, &mut fake, &stop, &mut save);
        assert_eq!(fake.waits, 1, "a dead terminal was waited on again");

        // A live one idles through every tick — and without redrawing: one
        // draw up front, one per event, none per tick.
        let mut fake = Fake::new(&stop, comment("x"));
        fake.steps.extend((0..5).map(|_| Step::Idle));
        let _ = run_loop(&mut app, &mut fake, &stop, &mut save);
        assert_eq!(fake.waits, 3 + 5 + 1, "the script did not run to its end");
        assert_eq!(fake.draws, 1 + 3, "an idle tick redrew");
    }

    /// A panic anywhere in the loop unwound out of `main`: exit 101, and the
    /// annotations went with it. Caught now, it ends the session like any
    /// other failure — written, printed, exit 2.
    #[test]
    fn a_panic_mid_session_still_writes_the_review() {
        let mut steps = comment("keep me");
        steps.push(Step::Panic);
        let (end, code, feedback, json) = session("panic", steps, |_| {});
        assert!(matches!(end, End::Panicked), "{end:?}");
        assert_eq!(code, 2);
        assert!(feedback.contains("keep me"));
        assert_eq!(json["annotations"][0]["text"], "keep me");
        assert_eq!(json["final"], false);
    }

    /// …and the one ending that is the human's: `q` is final and graded 0/1.
    #[test]
    fn only_a_quit_is_final() {
        let mut steps = comment("keep me");
        steps.push(Step::Ev(Event::Key(key('q', KeyModifiers::NONE))));
        let (end, code, _, json) = session("quit", steps, |_| {});
        assert!(end.is_quit() && end.reason().is_none(), "{end:?}");
        assert_eq!((code, json["final"].clone()), (1, true.into()));
    }
}
