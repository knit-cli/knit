//! The one failure shape every `--json` command shares.
//!
//! When a command run with `--json` fails, `main` prints
//! `{"error":{"kind":"…","message":"…"}}` on stdout, the usual `Error: …` line
//! on stderr, and exits non-zero. Nothing here is per command: `main` sees the
//! parsed `--json` flag and the returned error, so a new `--json` command gets
//! the envelope without doing anything.
//!
//! A command picks the `kind` by tagging the error where the cause is known
//! ([`error`] for a new error, [`classify`] for one already built). Untagged
//! errors report `other`. A command whose answer *is* a report even on
//! failure (a failed readiness probe, an invalid landing plan) calls
//! [`document_printed`] after printing it, and then no envelope follows.

use serde_json::{json, Value};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

/// Stable machine-readable error categories, listed in `docs/harness.md` and
/// `schemas/error.schema.json`. Hosts may match on them; new kinds can appear,
/// so a host must treat an unknown kind like `other`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// Not inside a Knit workspace.
    NoWorkspace,
    /// The workspace has no bundle the command could resolve to.
    NoBundle,
    /// No sync remote is configured, or the named one does not exist.
    NoRemote,
    /// The remote exists but no token is configured for it.
    NoToken,
    /// A transport or server failure talking to a remote.
    Http,
    /// The remote answered, but the thing asked for is not there.
    NotFound,
    /// The command line itself was not valid.
    Usage,
    /// Everything not yet classified.
    Other,
}

impl ErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::NoWorkspace => "noWorkspace",
            ErrorKind::NoBundle => "noBundle",
            ErrorKind::NoRemote => "noRemote",
            ErrorKind::NoToken => "noToken",
            ErrorKind::Http => "http",
            ErrorKind::NotFound => "notFound",
            ErrorKind::Usage => "usage",
            ErrorKind::Other => "other",
        }
    }
}

/// An error carrying its kind. It displays exactly as the wrapped error and
/// forwards its causes, so the human `Error:` output does not change.
#[derive(Debug)]
struct Classified {
    kind: ErrorKind,
    inner: anyhow::Error,
}

impl fmt::Display for Classified {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.inner, f)
    }
}

impl std::error::Error for Classified {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.inner.chain().nth(1)
    }
}

/// A new error of `kind` with `message`.
pub fn error(kind: ErrorKind, message: impl fmt::Display) -> anyhow::Error {
    classify(kind, anyhow::anyhow!("{message}"))
}

/// Tag `error` with `kind`, unless a cause already carries a kind: the tag set
/// where the cause was found is the more precise one (a missing remote stays
/// `noRemote` even when the caller files everything it sees as `http`).
/// `other` is the default and is never stored.
pub fn classify(kind: ErrorKind, error: anyhow::Error) -> anyhow::Error {
    if kind == ErrorKind::Other || tagged_kind(&error).is_some() {
        return error;
    }
    anyhow::Error::new(Classified { kind, inner: error })
}

fn tagged_kind(error: &anyhow::Error) -> Option<ErrorKind> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<Classified>())
        .map(|classified| classified.kind)
}

/// The kind reported for `error`: its tag, else `other`.
pub fn kind_of(error: &anyhow::Error) -> ErrorKind {
    tagged_kind(error).unwrap_or(ErrorKind::Other)
}

/// The `--json` error envelope for `error`.
pub fn envelope(error: &anyhow::Error) -> Value {
    envelope_for(kind_of(error), &format!("{error:#}"))
}

/// The envelope for a kind and message that did not come from an
/// `anyhow::Error` (a command line clap rejected).
pub fn envelope_for(kind: ErrorKind, message: &str) -> Value {
    json!({
        "error": {
            "kind": kind.as_str(),
            "message": message,
        }
    })
}

static DOCUMENT_PRINTED: AtomicBool = AtomicBool::new(false);

/// Record that this command already printed its JSON answer on stdout, so a
/// failure after it must not add a second document.
pub fn document_printed() {
    DOCUMENT_PRINTED.store(true, Ordering::Relaxed);
}

/// Print the envelope for a failed `--json` command on stdout, unless the
/// command already printed its own document.
pub fn report_json_failure(error: &anyhow::Error) {
    if !DOCUMENT_PRINTED.load(Ordering::Relaxed) {
        println!("{}", envelope(error));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classified_errors_display_like_the_wrapped_error() {
        let plain = anyhow::anyhow!("root cause").context("outer step");
        let expected_alt = format!("{plain:#}");
        let expected_debug = format!("{plain:?}");
        let tagged = classify(ErrorKind::Http, plain);
        assert_eq!(format!("{tagged:#}"), expected_alt);
        assert_eq!(format!("{tagged}"), "outer step");
        assert_eq!(format!("{tagged:?}"), expected_debug);
    }

    #[test]
    fn the_innermost_tag_wins_and_survives_context() {
        let missing = error(ErrorKind::NoRemote, "No sync remote configured.");
        let wrapped = classify(ErrorKind::Http, missing.context("pull failed"));
        assert_eq!(kind_of(&wrapped), ErrorKind::NoRemote);
        assert_eq!(
            envelope(&wrapped),
            json!({"error": {"kind": "noRemote", "message": "pull failed: No sync remote configured."}})
        );
    }

    #[test]
    fn untagged_errors_are_other() {
        let error = anyhow::anyhow!("boom");
        assert_eq!(kind_of(&error), ErrorKind::Other);
        assert_eq!(
            kind_of(&classify(ErrorKind::Other, error)),
            ErrorKind::Other
        );
    }
}
