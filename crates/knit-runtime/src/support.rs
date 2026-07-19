//! Small self-contained helpers mirroring the knit CLI's conventions (styled
//! output, atomic JSON, canonical paths) so the runtime crate stays free of
//! knit internals while looking identical in the terminal.

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fmt::Display;
use std::fs;
use std::path::Path;
use std::process::Command;

pub(crate) mod out {
    use std::env;
    use std::fmt::Display;
    use std::io::IsTerminal;

    #[derive(Clone, Copy)]
    enum Style {
        Bold,
        Dim,
        CyanBold,
        Yellow,
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

    pub fn warn(text: impl Display) -> String {
        paint(text, Style::Yellow)
    }

    fn paint(text: impl Display, style: Style) -> String {
        let text = text.to_string();
        if !should_color() {
            return text;
        }
        let code = match style {
            Style::Bold => "\x1b[1m",
            Style::Dim => "\x1b[2m",
            Style::CyanBold => "\x1b[1;36m",
            Style::Yellow => "\x1b[33m",
        };
        format!("{code}{text}\x1b[0m")
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
        std::io::stdout().is_terminal()
            && env::var("TERM").map(|term| term != "dumb").unwrap_or(true)
    }
}

pub(crate) fn canonicalize(path: impl AsRef<Path>) -> std::io::Result<std::path::PathBuf> {
    dunce::canonicalize(path)
}

/// Uppercase a repo or service id into an environment variable suffix,
/// mapping every non-alphanumeric character to `_` (`gloss-web-ui` ->
/// `GLOSS_WEB_UI`).
pub(crate) fn env_var_suffix(id: &str) -> String {
    id.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

pub(crate) fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub(crate) fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
}

/// Write JSON atomically: serialize to a sibling temp file, then rename over
/// the target, matching the knit CLI's artifact-write behavior.
pub(crate) fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let text = serde_json::to_string_pretty(value).context("failed to serialize JSON")?;
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "knit".to_string());
    let temp_path = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    fs::write(&temp_path, format!("{text}\n"))
        .with_context(|| format!("failed to write {}", temp_path.display()))?;
    match fs::rename(&temp_path, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temp_path);
            Err(error).with_context(|| format!("failed to write {}", path.display()))
        }
    }
}

pub(crate) fn rev_parse(cwd: &Path, reference: impl Display) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", &reference.to_string()])
        .output()
        .context("failed to run git rev-parse")?;
    if !output.status.success() {
        anyhow::bail!("git rev-parse failed in {}", cwd.display());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
