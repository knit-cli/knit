//! Bounded retries for the network calls Knit fans out across repos.
//!
//! Exactly two failure families are retried: a `git push` whose connection
//! broke, and a forge API call the host could not serve right now (5xx, rate
//! limits, dropped connections). Everything that reports a decision the host
//! already made — bad credentials, 404, 422, a rejected non-fast-forward push
//! — is handed straight back to the caller, because asking again only earns
//! the same answer more slowly and hides the real problem.
//!
//! Retries are announced as they happen ([`note`]). A per-repo worker installs
//! a sink with [`stream_notes_to`] so its notes travel through the same
//! channel as its result and the main thread stays the only writer.

use anyhow::{Error, Result};
use std::cell::RefCell;
use std::fmt;
use std::time::Duration;

/// A `git push` gets three tries: the second covers a dropped connection, the
/// third covers a remote that was restarting.
pub const GIT_PUSH_ATTEMPTS: u32 = 3;
/// Forge writes get four tries, matching the 1s/2s/4s backoff ladder.
pub const FORGE_ATTEMPTS: u32 = 4;
/// A hostile or mistaken `Retry-After` must not turn "slow" into "hung".
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// Whether a failed call may be repeated, and how long to wait first.
pub enum Retryable {
    No,
    Yes {
        reason: String,
        retry_after: Option<Duration>,
    },
}

impl Retryable {
    fn yes(reason: impl Into<String>, retry_after: Option<Duration>) -> Self {
        Self::Yes {
            reason: reason.into(),
            retry_after,
        }
    }
}

/// An HTTP failure from a forge, carried as a typed error so the retry
/// classifier reads the status and `Retry-After` the host actually sent
/// instead of guessing from prose.
#[derive(Debug)]
pub struct HttpFailure {
    pub status: u16,
    pub retry_after: Option<Duration>,
    pub detail: String,
    pub message: String,
}

impl fmt::Display for HttpFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for HttpFailure {}

/// Where a thread's retry notes go while a fan-out command is streaming.
type NoteSink = Box<dyn Fn(String)>;

thread_local! {
    static NOTES: RefCell<Option<NoteSink>> = const { RefCell::new(None) };
}

/// Restores the previous (empty) note routing when it drops.
pub struct NoteScope(());

impl Drop for NoteScope {
    fn drop(&mut self) {
        NOTES.with(|notes| {
            *notes.borrow_mut() = None;
        });
    }
}

/// Route this thread's retry notes to `sink` until the returned guard drops.
#[must_use = "notes are routed only while the guard is alive"]
pub fn stream_notes_to(sink: impl Fn(String) + 'static) -> NoteScope {
    NOTES.with(|notes| {
        *notes.borrow_mut() = Some(Box::new(sink));
    });
    NoteScope(())
}

/// Report something that happened mid-call, such as a retry. Goes to this
/// thread's sink when a fan-out command installed one, else straight out.
pub fn note(message: impl Into<String>) {
    let message = message.into();
    let routed = NOTES.with(|notes| match notes.borrow().as_ref() {
        Some(sink) => {
            sink(message.clone());
            true
        }
        None => false,
    });
    if !routed {
        crate::human!("{message}");
    }
}

/// Base backoff step. Tests set `KNIT_RETRY_BASE_MS=0` so a retry path can be
/// exercised without waiting for it.
fn base_delay() -> Result<Duration> {
    Ok(Duration::from_millis(crate::parallel::env_number(
        "KNIT_RETRY_BASE_MS",
        1000,
        true,
    )?))
}

/// How long one `git push` may run before Knit kills it.
pub fn git_push_timeout() -> Result<Duration> {
    Ok(Duration::from_secs(crate::parallel::env_number(
        "KNIT_GIT_PUSH_TIMEOUT",
        300,
        false,
    )?))
}

/// Run `attempt`, repeating it while `classify` calls the failure transient.
/// `action` names the call in the streamed retry line ("push", "gh pr create").
pub fn retry_transient<T>(
    action: &str,
    max_attempts: u32,
    classify: impl Fn(&Error) -> Retryable,
    attempt: impl FnMut() -> Result<T>,
) -> Result<T> {
    retry_with(action, max_attempts, base_delay()?, classify, attempt)
}

/// [`retry_transient`] with the backoff step supplied instead of read from the
/// environment, so tests can exercise the ladder without waiting on it.
fn retry_with<T>(
    action: &str,
    max_attempts: u32,
    base: Duration,
    classify: impl Fn(&Error) -> Retryable,
    mut attempt: impl FnMut() -> Result<T>,
) -> Result<T> {
    let mut number = 1;
    loop {
        let error = match attempt() {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        if number >= max_attempts {
            return Err(error);
        }
        let Retryable::Yes {
            reason,
            retry_after,
        } = classify(&error)
        else {
            return Err(error);
        };
        let delay = retry_after
            .map(|after| after.min(MAX_RETRY_AFTER))
            .unwrap_or_else(|| base * 2u32.pow(number - 1));
        note(format!(
            "retrying {action} ({}/{max_attempts}) after {reason}{}…",
            number + 1,
            wait_clause(delay)
        ));
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        number += 1;
    }
}

fn wait_clause(delay: Duration) -> String {
    if delay.is_zero() {
        String::new()
    } else if delay.as_millis() % 1000 == 0 {
        format!(", waiting {}s", delay.as_secs())
    } else {
        format!(", waiting {}ms", delay.as_millis())
    }
}

/// Push failures that describe a decision, not a bad moment. Checked first:
/// a rejected push, a lease that no longer holds, or a refused credential
/// must reach the user on the first attempt.
const GIT_PUSH_FINAL: &[&str] = &[
    "non-fast-forward",
    "rejected",
    "stale info",
    "authentication failed",
    "permission denied",
    "could not read username",
    "could not read password",
    "access denied",
    "repository not found",
    "does not appear to be a git repository",
    "hook declined",
];

/// Push failures worth repeating, with the wording used in the retry line.
const GIT_PUSH_TRANSIENT: &[(&str, &str)] = &[
    ("connection reset", "connection reset"),
    ("early eof", "early EOF"),
    ("rpc failed", "an RPC failure"),
    ("the remote end hung up", "the remote end hanging up"),
    ("could not resolve host", "an unresolved host"),
    ("temporary failure in name resolution", "an unresolved host"),
    ("timed out", "a timeout"),
    ("timeout", "a timeout"),
    ("connection refused", "a refused connection"),
    ("unexpectedly closed connection", "a closed connection"),
    ("broken pipe", "a broken pipe"),
    ("ssh_exchange_identification", "a dropped SSH handshake"),
];

pub fn classify_git_push(error: &Error) -> Retryable {
    classify_git_push_message(&format!("{error:#}"))
}

fn classify_git_push_message(message: &str) -> Retryable {
    let lower = message.to_ascii_lowercase();
    if GIT_PUSH_FINAL.iter().any(|marker| lower.contains(marker)) {
        return Retryable::No;
    }
    match GIT_PUSH_TRANSIENT
        .iter()
        .find(|(marker, _)| lower.contains(marker))
    {
        Some((_, reason)) => Retryable::yes(*reason, None),
        None => Retryable::No,
    }
}

/// Forge failures that are answers, not weather. `403` is deliberately absent:
/// a plain 403 is final, but a secondary-rate-limit 403 is not, so the status
/// is judged with its body in [`classify_http`].
const FORGE_FINAL: &[&str] = &[
    "http 401",
    "http 404",
    "http 422",
    "bad credentials",
    "unauthorized",
    "authentication failed",
    "requires authentication",
    "must be authenticated",
    "gh auth login",
    "insufficient_scope",
    "resource not accessible",
    "not found",
    "already exists",
    "validation failed",
];

const FORGE_TRANSIENT: &[(&str, &str)] = &[
    ("connection reset", "connection reset"),
    ("connection refused", "a refused connection"),
    ("connection closed", "a closed connection"),
    ("connection failed", "a failed connection"),
    ("timed out", "a timeout"),
    ("timeout", "a timeout"),
    ("could not resolve host", "an unresolved host"),
    ("temporary failure in name resolution", "an unresolved host"),
    ("dns failure", "an unresolved host"),
    ("network error", "a network error"),
    ("network is unreachable", "an unreachable network"),
    ("broken pipe", "a broken pipe"),
    ("unexpected eof", "an unexpected EOF"),
];

pub fn classify_forge(error: &Error) -> Retryable {
    if let Some(failure) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<HttpFailure>())
    {
        return classify_http(failure.status, &failure.detail, failure.retry_after);
    }
    classify_forge_message(&format!("{error:#}"))
}

fn classify_forge_message(message: &str) -> Retryable {
    let lower = message.to_ascii_lowercase();
    if FORGE_FINAL.iter().any(|marker| lower.contains(marker)) {
        return Retryable::No;
    }
    if let Some(status) = http_status_in(&lower) {
        return classify_http(status, &lower, retry_after_in(&lower));
    }
    match FORGE_TRANSIENT
        .iter()
        .find(|(marker, _)| lower.contains(marker))
    {
        Some((_, reason)) => Retryable::yes(*reason, None),
        None => Retryable::No,
    }
}

fn classify_http(status: u16, detail: &str, retry_after: Option<Duration>) -> Retryable {
    let lower = detail.to_ascii_lowercase();
    if status >= 500 {
        return Retryable::yes(format!("HTTP {status}"), retry_after);
    }
    if status == 429 {
        return Retryable::yes("HTTP 429 (rate limited)", retry_after);
    }
    if status == 403 && is_rate_limited_body(&lower) {
        return Retryable::yes("HTTP 403 (rate limited)", retry_after);
    }
    Retryable::No
}

fn is_rate_limited_body(lower: &str) -> bool {
    lower.contains("secondary rate limit")
        || lower.contains("abuse detection")
        || lower.contains("rate limit exceeded")
        || lower.contains("exceeded a secondary")
}

/// First `HTTP <ddd>` in a message, however the CLI phrased it: `gh` writes
/// `(HTTP 502)`, Knit's own transport writes `HTTP 502: ...`.
fn http_status_in(lower: &str) -> Option<u16> {
    let mut rest = lower;
    while let Some(at) = rest.find("http ") {
        let after = &rest[at + "http ".len()..];
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.len() == 3 {
            if let Ok(status) = digits.parse::<u16>() {
                return Some(status);
            }
        }
        rest = after;
    }
    None
}

fn retry_after_in(lower: &str) -> Option<Duration> {
    let at = lower.find("retry-after: ")?;
    let after = &lower[at + "retry-after: ".len()..];
    let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<u64>().ok().map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    fn is_retryable(decision: &Retryable) -> bool {
        matches!(decision, Retryable::Yes { .. })
    }

    #[test]
    fn transient_push_failures_are_retried_and_decisions_are_not() {
        for message in [
            "git push failed: Connection reset by peer",
            "fatal: the remote end hung up unexpectedly",
            "error: RPC failed; curl 56 recv failure",
            "fatal: unable to access: Could not resolve host: github.com",
            "git push origin knit/x in /tmp/x timed out after 300s",
        ] {
            assert!(
                is_retryable(&classify_git_push_message(message)),
                "{message}"
            );
        }
        for message in [
            "! [rejected] knit/x -> knit/x (non-fast-forward)",
            "! [rejected] knit/x -> knit/x (stale info)",
            "fatal: Authentication failed for 'https://github.com/acme/backend'",
            "remote: Permission denied to acme.",
        ] {
            assert!(
                !is_retryable(&classify_git_push_message(message)),
                "{message}"
            );
        }
    }

    #[test]
    fn forge_failures_are_retried_only_when_the_host_was_unavailable() {
        for message in [
            "gh pr create failed: Bad gateway (HTTP 502)",
            "GitHub API request failed during POST /repos/a/b/pulls: HTTP 503: unavailable",
            "HTTP 429: too many requests",
            "HTTP 403: You have exceeded a secondary rate limit",
            "GitHub API request failed during GET /x: io: connection reset by peer",
        ] {
            assert!(is_retryable(&classify_forge_message(message)), "{message}");
        }
        for message in [
            "HTTP 401: Bad credentials",
            "HTTP 404: Not Found",
            "HTTP 422: Validation Failed: A pull request already exists for acme:knit/x.",
            "HTTP 403: Resource not accessible by integration",
            "HTTP 403: Forbidden",
            "gh: command not found",
        ] {
            assert!(!is_retryable(&classify_forge_message(message)), "{message}");
        }
    }

    #[test]
    fn typed_http_failures_carry_status_and_retry_after() {
        let error = Error::new(HttpFailure {
            status: 429,
            retry_after: Some(Duration::from_secs(7)),
            detail: "too many requests".to_string(),
            message: "HTTP 429".to_string(),
        })
        .context("backend: failed to create the PR");
        match classify_forge(&error) {
            Retryable::Yes { retry_after, .. } => {
                assert_eq!(retry_after, Some(Duration::from_secs(7)));
            }
            Retryable::No => panic!("a 429 with Retry-After must be retried"),
        }
    }

    #[test]
    fn status_and_retry_after_parsing() {
        assert_eq!(http_status_in("bad gateway (http 502)"), Some(502));
        assert_eq!(http_status_in("http 429: slow down"), Some(429));
        assert_eq!(http_status_in("no status here"), None);
        assert_eq!(
            retry_after_in("http 429 (retry-after: 12s)"),
            Some(Duration::from_secs(12))
        );
    }

    #[test]
    fn retries_stop_at_the_attempt_limit_and_report_the_last_error() {
        let mut attempts = 0;
        let error = retry_with(
            "push",
            3,
            Duration::ZERO,
            classify_git_push,
            || -> Result<()> {
                attempts += 1;
                Err(anyhow!("fatal: Connection reset by peer"))
            },
        )
        .unwrap_err();
        assert_eq!(attempts, 3);
        assert!(error.to_string().contains("Connection reset"));
    }

    #[test]
    fn a_final_failure_is_not_repeated() {
        let mut attempts = 0;
        let error = retry_with(
            "push",
            3,
            Duration::ZERO,
            classify_git_push,
            || -> Result<()> {
                attempts += 1;
                Err(anyhow!("! [rejected] knit/x -> knit/x (non-fast-forward)"))
            },
        )
        .unwrap_err();
        assert_eq!(attempts, 1);
        assert!(error.to_string().contains("rejected"));
    }

    #[test]
    fn a_retry_that_succeeds_returns_the_value_and_announces_itself() {
        let seen = std::rc::Rc::new(RefCell::new(Vec::new()));
        let recorder = std::rc::Rc::clone(&seen);
        let scope = stream_notes_to(move |line| recorder.borrow_mut().push(line));
        let mut attempts = 0;
        let value = retry_with("push", 3, Duration::ZERO, classify_git_push, || {
            attempts += 1;
            if attempts == 1 {
                Err(anyhow!("fatal: Connection reset by peer"))
            } else {
                Ok(7)
            }
        })
        .unwrap();
        drop(scope);
        assert_eq!(value, 7);
        assert_eq!(seen.borrow().len(), 1);
        assert!(
            seen.borrow()[0].contains("retrying push (2/3) after connection reset"),
            "{:?}",
            seen.borrow()
        );
    }
}
