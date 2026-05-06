use std::env;
use std::fmt::Display;
use std::io::IsTerminal;

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
        "advanced" | "added" | "committed" | "created" | "fetched" | "pushed" | "staged" => {
            ok(text)
        }
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

fn should_color() -> bool {
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
