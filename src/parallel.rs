//! Bounded parallelism for Knit's per-repo fan-out commands.
//!
//! `knit push` and `knit publish create` used to spawn one thread per repo.
//! That is fine for a five-repo bundle and hostile for a hundred-repo one:
//! a hundred simultaneous `git push` processes and a hundred simultaneous
//! forge API writes are how a workspace turns into a rate-limited, swap-bound
//! stall. Work is handed to a small pool instead, and the pool size is a
//! deliberate, overridable number rather than "however many repos exist".
//!
//! Git pushes and forge writes get separate limits because they fail
//! differently: git pushes are bound by network and CPU, forge writes by the
//! host's rate limiter.

use anyhow::{bail, Result};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Concurrent `git push` processes, unless `KNIT_GIT_JOBS` says otherwise.
pub const DEFAULT_GIT_JOBS: usize = 8;
/// Concurrent forge writes (PR creation and friends), unless
/// `KNIT_FORGE_JOBS` says otherwise. Lower than the git limit because code
/// hosts rate-limit writes far more aggressively than a git remote does.
pub const DEFAULT_FORGE_JOBS: usize = 4;

pub fn git_jobs() -> Result<usize> {
    Ok(env_number("KNIT_GIT_JOBS", DEFAULT_GIT_JOBS as u64, false)? as usize)
}

pub fn forge_jobs() -> Result<usize> {
    Ok(env_number("KNIT_FORGE_JOBS", DEFAULT_FORGE_JOBS as u64, false)? as usize)
}

/// Read a numeric tuning knob from the environment. An unset or empty value
/// takes the default; anything that is not a whole number — or a zero where
/// zero is meaningless — is an error rather than a silently ignored setting.
pub(crate) fn env_number(name: &str, default: u64, allow_zero: bool) -> Result<u64> {
    let Some(raw) = std::env::var_os(name) else {
        return Ok(default);
    };
    let raw = raw.to_string_lossy().trim().to_string();
    if raw.is_empty() {
        return Ok(default);
    }
    match raw.parse::<u64>() {
        Ok(0) if !allow_zero => {
            bail!("{name} must be a positive whole number, got `{raw}`.")
        }
        Ok(value) => Ok(value),
        Err(_) => bail!(
            "{name} must be a{} whole number, got `{raw}`.",
            if allow_zero { "" } else { " positive" }
        ),
    }
}

/// Run `run` over every job on at most `limit` threads, inside the caller's
/// [`std::thread::scope`].
///
/// The workers are spawned and left running: the caller keeps the scope open
/// and drains its result channel while they work, so per-repo output still
/// streams as each repo finishes instead of arriving after the last join.
pub fn spawn_bounded<'scope, 'env, T, F>(
    scope: &'scope std::thread::Scope<'scope, 'env>,
    jobs: &'env [T],
    limit: usize,
    run: F,
) where
    T: Sync + 'env,
    F: Fn(&'env T) + Send + Sync + 'env,
{
    if jobs.is_empty() {
        return;
    }
    let workers = limit.clamp(1, jobs.len());
    let next = Arc::new(AtomicUsize::new(0));
    let run = Arc::new(run);
    for _ in 0..workers {
        let next = Arc::clone(&next);
        let run = Arc::clone(&run);
        scope.spawn(move || loop {
            let index = next.fetch_add(1, Ordering::Relaxed);
            let Some(job) = jobs.get(index) else {
                return;
            };
            run(job);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicI64;

    #[test]
    fn env_number_reads_defaults_and_rejects_nonsense() {
        assert_eq!(env_number("KNIT_TEST_UNSET_LIMIT", 8, false).unwrap(), 8);
        // Values are validated, not silently ignored: a typo in a tuning knob
        // must not quietly run with the default.
        std::env::set_var("KNIT_TEST_LIMIT", "zero");
        assert!(env_number("KNIT_TEST_LIMIT", 8, false).is_err());
        std::env::set_var("KNIT_TEST_LIMIT", "0");
        assert!(env_number("KNIT_TEST_LIMIT", 8, false).is_err());
        assert_eq!(env_number("KNIT_TEST_LIMIT", 8, true).unwrap(), 0);
        std::env::set_var("KNIT_TEST_LIMIT", "3");
        assert_eq!(env_number("KNIT_TEST_LIMIT", 8, false).unwrap(), 3);
        std::env::remove_var("KNIT_TEST_LIMIT");
    }

    #[test]
    fn bounded_pool_runs_every_job_without_exceeding_the_limit() {
        let jobs: Vec<usize> = (0..50).collect();
        let done = AtomicUsize::new(0);
        let live = AtomicI64::new(0);
        let peak = AtomicI64::new(0);
        std::thread::scope(|scope| {
            spawn_bounded(scope, &jobs, 4, |_job| {
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(2));
                done.fetch_add(1, Ordering::SeqCst);
                live.fetch_sub(1, Ordering::SeqCst);
            });
        });
        assert_eq!(done.load(Ordering::SeqCst), 50);
        assert!(peak.load(Ordering::SeqCst) <= 4, "{peak:?}");
    }

    #[test]
    fn bounded_pool_tolerates_no_jobs() {
        let jobs: Vec<usize> = Vec::new();
        std::thread::scope(|scope| {
            spawn_bounded(scope, &jobs, 4, |_job| unreachable!());
        });
    }
}
