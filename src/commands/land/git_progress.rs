//! Opt-in output for Git operations performed by a landing step.
use std::cell::RefCell;
use std::ffi::OsString;
use std::process::{Command, Output};

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::auth::ResolvedCredential;

thread_local! {
    static CURRENT: RefCell<Option<(String, Vec<Value>)>> = const { RefCell::new(None) };
}

struct Scope(Option<(String, Vec<Value>)>);
impl Drop for Scope {
    fn drop(&mut self) {
        CURRENT.with(|state| *state.borrow_mut() = self.0.take());
    }
}

pub(crate) fn scope<T>(label: &str, f: impl FnOnce() -> T) -> (T, Vec<Value>) {
    let _guard = Scope(CURRENT.with(|state| state.replace(Some((label.into(), Vec::new())))));
    let result = f();
    let records = CURRENT.with(|state| {
        state
            .borrow_mut()
            .as_mut()
            .map(|(_, records)| std::mem::take(records))
            .unwrap_or_default()
    });
    (result, records)
}

pub(crate) fn active(args: &[OsString]) -> bool {
    let operation = args.first().and_then(|arg| arg.to_str());
    let effect = matches!(
        operation,
        Some("fetch" | "merge" | "push" | "checkout" | "reset")
    ) || (operation == Some("worktree")
        && args
            .get(1)
            .is_some_and(|arg| arg == "add" || arg == "remove"));
    effect && CURRENT.with(|state| state.borrow().is_some())
}

pub(crate) fn run(
    command: &mut Command,
    args: &[OsString],
    credentials: &[ResolvedCredential],
) -> Result<Output> {
    let label = CURRENT.with(|state| {
        state
            .borrow()
            .as_ref()
            .map(|(label, _)| label.clone())
            .unwrap_or_default()
    });
    // Display the original operation, never the injected credential helper,
    // environment, or URL arguments (which can contain embedded credentials).
    let display = format!(
        "git {}",
        args.first()
            .map(|s| s.to_string_lossy())
            .unwrap_or_default()
    );
    eprintln!("[{label}] $ {display}");
    // Git otherwise hides transfer progress because its stderr is a pipe.
    // Respect an explicitly quiet invocation.
    if args.first().is_some_and(|s| s == "fetch" || s == "push")
        && !args
            .iter()
            .any(|s| s == "--quiet" || s == "-q" || s == "--no-progress")
    {
        command.arg("--progress");
    }
    let started = crate::time::now_iso();
    // An interrupted merge/push still needs its existing local cleanup.
    // Do not clear cancellation globally or allow later forward operations.
    let cleanup = args.first().is_some_and(|s| s == "reset")
        || (args.first().is_some_and(|s| s == "merge") && args.iter().any(|s| s == "--abort"));
    let result = super::process::run_streamed_redacted(
        command,
        None,
        &crate::auth_git::redaction_patterns(credentials),
        cleanup,
    );
    let record = match &result {
        Ok(output) => {
            json!({"phase":"git", "command":display, "startedAt":started, "finishedAt":crate::time::now_iso(), "status":if output.status.success() && !output.timed_out && !output.cancelled {"succeeded"} else {"failed"}, "stdout":output.stdout, "stderr":output.stderr, "exitCode":output.status.code(), "timedOut":output.timed_out, "cancelled":output.cancelled})
        }
        Err(error) => {
            json!({"phase":"git", "command":display, "startedAt":started, "status":"failed", "error":crate::auth_git::redact(credentials, &error.to_string())})
        }
    };
    CURRENT.with(|state| {
        if let Some((_, records)) = state.borrow_mut().as_mut() {
            records.push(record);
        }
    });
    let output = result?;
    if output.timed_out || output.cancelled {
        bail!(
            "{display} {}",
            if output.cancelled {
                "cancelled"
            } else {
                "timed out"
            }
        );
    }
    Ok(Output {
        status: output.status,
        stdout: output.stdout.into_bytes(),
        stderr: output.stderr.into_bytes(),
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn scopes_are_opt_in_and_restore_the_previous_scope() {
        let fetch = ["fetch".into(), "origin".into()];
        assert!(!super::active(&fetch));
        super::scope("outer", || {
            assert!(super::active(&fetch));
            assert!(!super::active(&["rev-parse".into(), "HEAD".into()]));
            assert!(!super::active(&["worktree".into(), "list".into()]));
            super::scope("inner", || assert!(super::active(&fetch)));
            assert!(super::active(&fetch));
        });
        assert!(!super::active(&fetch));
    }
}
