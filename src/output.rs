use std::env;
use std::fmt::{self, Display};
use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};

// Every `print!`, `println!`, `eprint!` and `eprintln!` in this crate expands
// to these instead of the standard library's macros (`lib.rs` declares this
// module first with `#[macro_use]`, and a crate's own `macro_rules!` shadow
// the prelude's). The standard macros panic when the reader of a pipe has
// gone away (`knit log | head`); these drop the output and let the command
// finish. See `write_stream` for why knit keeps running rather than dying the
// way `git log | head` does. Unit tests keep the standard macros so the test
// harness still captures their output.
#[cfg(not(test))]
macro_rules! print {
    ($($arg:tt)*) => {
        $crate::output::write_stream($crate::output::Stream::Out, format_args!($($arg)*), false)
    };
}

#[cfg(not(test))]
macro_rules! println {
    () => {
        $crate::output::write_stream($crate::output::Stream::Out, format_args!(""), true)
    };
    ($($arg:tt)*) => {
        $crate::output::write_stream($crate::output::Stream::Out, format_args!($($arg)*), true)
    };
}

#[cfg(not(test))]
macro_rules! eprint {
    ($($arg:tt)*) => {
        $crate::output::write_stream($crate::output::Stream::Err, format_args!($($arg)*), false)
    };
}

#[cfg(not(test))]
macro_rules! eprintln {
    () => {
        $crate::output::write_stream($crate::output::Stream::Err, format_args!(""), true)
    };
    ($($arg:tt)*) => {
        $crate::output::write_stream($crate::output::Stream::Err, format_args!($($arg)*), true)
    };
}

#[doc(hidden)]
#[derive(Clone, Copy)]
pub enum Stream {
    Out,
    Err,
}

static STDOUT_CLOSED: AtomicBool = AtomicBool::new(false);
static STDERR_CLOSED: AtomicBool = AtomicBool::new(false);

/// Write one `print!`-family message. A stream whose reader has closed
/// (`BrokenPipe`, which Rust reports instead of raising SIGPIPE because it
/// starts every program with SIGPIPE ignored) is marked closed and every later
/// message to it is dropped silently; the command carries on and exits with its
/// own status.
///
/// Knit deliberately does not restore SIGPIPE's default "terminate" action,
/// which is how `git log | head` exits: the first progress line written after
/// the reader left would kill knit on the spot, in the middle of a commit
/// across repos, a land, or a merge before its rollback, without running the
/// drop guards that release bundle locks or the rollback paths. Finishing the
/// operation with its output discarded is the only outcome that never leaves
/// repos half-updated. Any other write error keeps the standard macros'
/// behavior and panics.
#[doc(hidden)]
pub fn write_stream(stream: Stream, args: fmt::Arguments<'_>, newline: bool) {
    let (closed, name) = match stream {
        Stream::Out => (&STDOUT_CLOSED, "stdout"),
        Stream::Err => (&STDERR_CLOSED, "stderr"),
    };
    if closed.load(Ordering::Relaxed) {
        return;
    }
    let result = match stream {
        Stream::Out => write_message(&mut io::stdout().lock(), args, newline),
        Stream::Err => write_message(&mut io::stderr().lock(), args, newline),
    };
    match result {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
            closed.store(true, Ordering::Relaxed);
        }
        Err(error) => panic!("failed printing to {name}: {error}"),
    }
}

fn write_message(out: &mut impl Write, args: fmt::Arguments<'_>, newline: bool) -> io::Result<()> {
    out.write_fmt(args)?;
    if newline {
        out.write_all(b"\n")?;
    }
    Ok(())
}

/// When set, `human!` lines go to stderr instead of stdout. A `--json` command
/// flips this once at startup so stdout carries only its machine-readable
/// document while progress lines stay visible on stderr.
static HUMAN_LINES_TO_STDERR: AtomicBool = AtomicBool::new(false);

pub fn route_human_lines_to_stderr() {
    HUMAN_LINES_TO_STDERR.store(true, Ordering::Relaxed);
}

pub fn human_lines_to_stderr() -> bool {
    HUMAN_LINES_TO_STDERR.load(Ordering::Relaxed)
}

/// Print a human progress/status line: stdout normally, stderr when a `--json`
/// command has claimed stdout for its machine-readable document.
#[macro_export]
macro_rules! human {
    ($($arg:tt)*) => {{
        if $crate::output::human_lines_to_stderr() {
            eprintln!($($arg)*);
        } else {
            println!($($arg)*);
        }
    }};
}

#[derive(Clone, Copy)]
enum Style {
    Bold,
    Dim,
    Green,
    Yellow,
    RedBold,
    CyanBold,
    BlueBold,
    Magenta,
}

pub fn heading(text: impl Display) -> String {
    paint(text, Style::Bold)
}

pub fn muted(text: impl Display) -> String {
    paint(text, Style::Dim)
}

/// Progress tail for streamed multi-repo output: ` (done/total)` in muted
/// style, or nothing when a single repo is reported and a count adds no
/// information.
pub fn progress(done: usize, total: usize) -> String {
    if total <= 1 {
        String::new()
    } else {
        muted(format!(" ({done}/{total})"))
    }
}

pub fn path(text: impl Display) -> String {
    paint(text, Style::Dim)
}

pub fn repo(text: impl Display) -> String {
    paint(text, Style::CyanBold)
}

pub fn repo_field(text: &str, width: usize) -> String {
    repo(format!("{text:<width$}"))
}

pub fn header_field(text: &str, width: usize) -> String {
    heading(format!("{text:<width$}"))
}

pub fn path_field(text: &str, width: usize) -> String {
    path(format!("{text:<width$}"))
}

pub fn branch(text: impl Display) -> String {
    paint(text, Style::BlueBold)
}

pub fn branch_field(text: &str, width: usize) -> String {
    branch(format!("{text:<width$}"))
}

pub fn node(text: impl Display) -> String {
    paint(text, Style::Magenta)
}

pub fn sha(text: impl Display) -> String {
    paint(text, Style::Yellow)
}

pub fn ok(text: impl Display) -> String {
    paint(text, Style::Green)
}

pub fn warn(text: impl Display) -> String {
    paint(text, Style::Yellow)
}

pub fn danger(text: impl Display) -> String {
    paint(text, Style::RedBold)
}

pub fn status(text: &str) -> String {
    if text.contains("rewound") || text.contains("diverged") {
        danger(text)
    } else if text.starts_with("clean") {
        ok(text)
    } else {
        warn(text)
    }
}

pub fn movement(text: &str) -> String {
    match text {
        "advanced" | "added" | "committed" | "created" | "fetched" | "pushed" | "resolved"
        | "staged" => ok(text),
        "rewound" | "diverged" | "dropped" | "removed" => danger(text),
        _ => warn(text),
    }
}

fn paint(text: impl Display, style: Style) -> String {
    let text = text.to_string();
    if !should_color() {
        return text;
    }

    format!("{}{}{}", code(style), text, "\x1b[0m")
}

pub fn should_color() -> bool {
    if env::var_os("NO_COLOR").is_some() {
        return false;
    }

    match env::var("KNIT_COLOR").as_deref() {
        Ok("always") => return true,
        Ok("never") => return false,
        _ => {}
    }

    if env::var("CLICOLOR_FORCE")
        .map(|value| value != "0")
        .unwrap_or(false)
    {
        return true;
    }

    std::io::stdout().is_terminal() && env::var("TERM").map(|term| term != "dumb").unwrap_or(true)
}

fn code(style: Style) -> &'static str {
    match style {
        Style::Bold => "\x1b[1m",
        Style::Dim => "\x1b[2m",
        Style::Green => "\x1b[32m",
        Style::Yellow => "\x1b[33m",
        Style::RedBold => "\x1b[1;31m",
        Style::CyanBold => "\x1b[1;36m",
        Style::BlueBold => "\x1b[1;34m",
        Style::Magenta => "\x1b[35m",
    }
}

#[cfg(test)]
mod tests {
    use super::progress;

    #[test]
    fn progress_tail_is_empty_for_a_single_repo_and_counts_otherwise() {
        std::env::set_var("NO_COLOR", "1");
        assert_eq!(progress(1, 1), "");
        assert_eq!(progress(1, 0), "");
        assert_eq!(progress(1, 3), " (1/3)");
        assert_eq!(progress(3, 3), " (3/3)");
    }
}
