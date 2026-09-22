//! Shared secret token entry for credentials collected interactively or from
//! stdin: one reader so every token path behaves the same.
//!
//! At a terminal the token is typed at a hidden prompt that submits as soon
//! as Enter is pressed — no Ctrl-D. Piped or redirected stdin keeps the
//! bounded read-to-EOF contract used by secret managers. Either way the
//! value is trimmed and validated BEFORE any caller mutates stored
//! configuration, and error messages never echo the token itself.

use anyhow::{bail, Context, Result};
use std::io::{self, IsTerminal, Read};

/// Maximum accepted token size; piped reads are capped before allocation
/// grows, and terminal input is checked after the hidden prompt returns.
pub(crate) const MAX_TOKEN_BYTES: usize = 64 * 1024;

/// Read one secret token. `prompt` is shown only at a terminal.
pub(crate) fn read_token(prompt: &str, label: &str) -> Result<String> {
    if io::stdin().is_terminal() {
        let raw = rpassword::prompt_password(prompt)
            .context("could not read the token from the hidden prompt")?;
        ensure_size(&raw, label)?;
        validate(&raw, label)
    } else {
        let mut bytes = Vec::new();
        io::stdin()
            .take(MAX_TOKEN_BYTES.saturating_add(1) as u64)
            .read_to_end(&mut bytes)
            .with_context(|| format!("failed to read the {label} from stdin"))?;
        if bytes.len() > MAX_TOKEN_BYTES {
            bail!("{label} exceeds {MAX_TOKEN_BYTES} bytes");
        }
        let raw = String::from_utf8(bytes)
            .map_err(|_| anyhow::anyhow!("{label} from stdin is not UTF-8"))?;
        validate(&raw, label)
    }
}

/// The size bound applies to both entry paths: a pasted multi-megabyte blob
/// at a hidden prompt is exactly as unusable as one arriving on a pipe.
fn ensure_size(raw: &str, label: &str) -> Result<()> {
    if raw.len() > MAX_TOKEN_BYTES {
        bail!("{label} exceeds {MAX_TOKEN_BYTES} bytes");
    }
    Ok(())
}

/// Trim surrounding whitespace and reject empty or multi-line/control-bearing
/// input before any mutation happens.
fn validate(raw: &str, label: &str) -> Result<String> {
    let token = raw.trim();
    if token.is_empty() {
        bail!("{label} is empty; nothing was saved");
    }
    if token.chars().any(char::is_control) {
        bail!("{label} must be a single line without control characters");
    }
    Ok(token.to_owned())
}

/// Remove every occurrence of `secret` from `text`. Server error bodies and
/// transport summaries can reflect request headers; anything printed after a
/// failed request passes through here first.
pub(crate) fn redact(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        text.to_owned()
    } else {
        text.replace(secret, "[REDACTED]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_trims_and_rejects_control_characters() {
        assert_eq!(validate("  secret \n", "label").unwrap(), "secret");
        assert_eq!(validate("\r\nsecret\r\n", "label").unwrap(), "secret");
        for empty in ["", " ", "\n", "\r\n", " \t "] {
            let error = validate(empty, "remote token").unwrap_err().to_string();
            assert_eq!(error, "remote token is empty; nothing was saved");
        }
        for multiline in ["two\nlines", "carriage\rreturn", "tab\tinside", "nul\0byte"] {
            let error = validate(multiline, "remote token").unwrap_err().to_string();
            assert_eq!(
                error,
                "remote token must be a single line without control characters"
            );
        }
        // Plain internal spaces are not control characters.
        assert_eq!(validate("a b", "label").unwrap(), "a b");
    }

    #[test]
    fn the_size_bound_covers_terminal_input_too() {
        // A hidden prompt can receive a pasted blob just like a pipe can.
        let oversized = "x".repeat(MAX_TOKEN_BYTES + 1);
        let error = ensure_size(&oversized, "forge token")
            .unwrap_err()
            .to_string();
        assert_eq!(error, "forge token exceeds 65536 bytes");
        assert!(ensure_size(&"x".repeat(MAX_TOKEN_BYTES), "forge token").is_ok());
    }

    #[test]
    fn redaction_strips_every_occurrence() {
        assert_eq!(
            redact("a SECRET b SECRET", "SECRET"),
            "a [REDACTED] b [REDACTED]"
        );
        assert_eq!(redact("untouched", ""), "untouched");
    }
}
