//! `knit land check` — a live landing-readiness preflight.
//!
//! For each recorded review publication it fetches the host PR once and reports
//! state, mergeability, checks, review decision, and a verdict, so you can see
//! whether `knit land apply` will succeed (and why not) before running it. The
//! per-repo assessment is shared with `knit publish status --live`.

use crate::checkout::checkout_dir;
use crate::output as out;
use crate::providers::{self, publication_for_repo, CheckRun, PrTarget};
use crate::store::{load_active_bundle, ActiveBundle};
use anyhow::{bail, Result};
use std::path::PathBuf;

/// Live landing readiness for one repo's review publication.
pub(crate) struct LandReadiness {
    pub repo_id: String,
    pub number: u64,
    pub state: String,
    /// `clean`, `conflict`, `unknown`, or `-` for terminal states.
    pub mergeable: String,
    /// `passed`, `failed`, `pending`, `none`, or `-`.
    pub checks: String,
    /// `approved`, `changes`, `none`, or `-`.
    pub review: String,
    pub verdict: String,
    /// True when the PR is not landable yet (so callers can color/aggregate).
    pub blocked: bool,
}

pub fn check_landing() -> Result<()> {
    let active = load_active_bundle()?;
    if active.bundle.repos.is_empty() {
        bail!("The resolved bundle has no repos. Run `knit bundle add <repo-path>` first.");
    }

    let publications: Vec<(usize, String)> = active
        .bundle
        .repos
        .iter()
        .enumerate()
        .filter_map(|(index, repo)| {
            publication_for_repo(&active.bundle, &repo.id)
                .map(|publication| (index, publication.url.clone()))
        })
        .collect();

    if publications.is_empty() {
        println!(
            "{}",
            out::muted("No review publications recorded. Run `knit publish` first.")
        );
        return Ok(());
    }

    println!("Bundle: {}\n", out::heading(&active.bundle.id));
    println!(
        "{}  {}  {}  {}  {}  {}  {}",
        out::header_field("repo", 16),
        out::header_field("pr", 6),
        out::header_field("state", 8),
        out::header_field("mergeable", 10),
        out::header_field("checks", 9),
        out::header_field("review", 9),
        out::heading("verdict")
    );

    let mut ready = 0usize;
    let mut blocked = 0usize;
    let mut landed = 0usize;
    for (index, url) in &publications {
        let readiness = assess_landing_readiness(&active, &active.bundle.repos[*index], url);
        print_readiness_row(&readiness);
        if readiness.state == "MERGED" {
            landed += 1;
        } else if readiness.blocked {
            blocked += 1;
        } else {
            ready += 1;
        }
    }

    println!();
    println!(
        "{} {ready} ready, {blocked} blocked, {landed} already landed",
        out::heading("Readiness:")
    );
    if blocked == 0 {
        println!(
            "{} when ready, run `knit land` then `knit land apply`.",
            out::heading("Next:")
        );
    }
    Ok(())
}

/// Render one readiness row, coloring the verdict by landability.
pub(crate) fn print_readiness_row(r: &LandReadiness) {
    let verdict = if r.state == "MERGED" {
        out::ok(&r.verdict)
    } else if r.blocked {
        out::warn(&r.verdict)
    } else {
        out::ok(&r.verdict)
    };
    println!(
        "{}  {}  {}  {}  {}  {}  {}",
        out::repo_field(&r.repo_id, 16),
        out::sha(format!("{:<6}", format!("#{}", r.number))),
        out::status(&format!("{:<8}", r.state.to_lowercase())),
        format!("{:<10}", r.mergeable),
        format!("{:<9}", r.checks),
        format!("{:<9}", r.review),
        verdict
    );
}

/// Fetch a publication's live PR state and classify its landing readiness. Forge
/// errors are captured into the verdict rather than aborting the whole table.
pub(crate) fn assess_landing_readiness(
    active: &ActiveBundle,
    repo: &crate::model::RepoEntry,
    publication_url: &str,
) -> LandReadiness {
    let base = LandReadiness {
        repo_id: repo.id.clone(),
        number: providers::pr_number_from_url(publication_url).unwrap_or(0),
        state: "?".to_string(),
        mergeable: "-".to_string(),
        checks: "-".to_string(),
        review: "-".to_string(),
        verdict: String::new(),
        blocked: true,
    };

    let forge = match providers::for_repo(repo) {
        Ok(forge) => forge,
        Err(error) => {
            return LandReadiness {
                verdict: format!("provider unavailable: {error}"),
                ..base
            }
        }
    };
    let cwd = checkout_dir(active, repo).unwrap_or_else(|| PathBuf::from(&repo.path));
    let target = PrTarget::checkout(&cwd);

    let pr = match forge.view(&target, publication_url) {
        Ok(pr) => pr,
        Err(error) => {
            return LandReadiness {
                verdict: format!("PR unavailable: {error}"),
                ..base
            }
        }
    };
    let state = pr.state.clone().unwrap_or_else(|| "UNKNOWN".to_string());

    // Terminal states: nothing to assess.
    match state.as_str() {
        "MERGED" => {
            return LandReadiness {
                state,
                verdict: "already landed".to_string(),
                blocked: false,
                ..base
            }
        }
        "CLOSED" => {
            return LandReadiness {
                state,
                verdict: "closed".to_string(),
                ..base
            }
        }
        _ => {}
    }

    if pr.is_draft.unwrap_or(false) {
        return LandReadiness {
            state,
            verdict: "draft".to_string(),
            ..base
        };
    }

    let mergeable = if pr.is_conflicting() {
        "conflict"
    } else if pr.mergeable.as_deref() == Some("MERGEABLE") {
        "clean"
    } else {
        "unknown"
    };
    let review = match pr.review_decision.as_deref() {
        Some("APPROVED") => "approved",
        Some("CHANGES_REQUESTED") => "changes",
        _ => "none",
    };
    let (checks, checks_outcome) = match forge.check_runs(&target, publication_url, true) {
        Ok(runs) => checks_label(&runs),
        Err(_) => ("unknown".to_string(), ChecksOutcome::Unknown),
    };

    let (verdict, blocked) = if pr.is_conflicting() {
        ("conflict — run knit land update".to_string(), true)
    } else if matches!(checks_outcome, ChecksOutcome::Failed) {
        ("checks failing".to_string(), true)
    } else if matches!(checks_outcome, ChecksOutcome::Pending) {
        ("checks pending".to_string(), true)
    } else if review == "changes" {
        ("changes requested".to_string(), true)
    } else {
        ("ready".to_string(), false)
    };

    LandReadiness {
        repo_id: repo.id.clone(),
        number: pr.number,
        state,
        mergeable: mergeable.to_string(),
        checks,
        review: review.to_string(),
        verdict,
        blocked,
    }
}

enum ChecksOutcome {
    None,
    Passed,
    Pending,
    Failed,
    Unknown,
}

fn checks_label(runs: &[CheckRun]) -> (String, ChecksOutcome) {
    if runs.is_empty() {
        return ("none".to_string(), ChecksOutcome::None);
    }
    let failed = runs.iter().any(|run| {
        matches!(run.bucket.as_deref(), Some("fail" | "cancel"))
            || matches!(run.state.as_deref(), Some("FAILURE" | "CANCELLED"))
    });
    if failed {
        return ("failed".to_string(), ChecksOutcome::Failed);
    }
    let pending = runs.iter().any(|run| {
        !matches!(run.bucket.as_deref(), Some("pass" | "skipping"))
            && !matches!(run.state.as_deref(), Some("SUCCESS" | "SKIPPED"))
    });
    if pending {
        ("pending".to_string(), ChecksOutcome::Pending)
    } else {
        ("passed".to_string(), ChecksOutcome::Passed)
    }
}
