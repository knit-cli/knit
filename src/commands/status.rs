use crate::advice;
use crate::checkout::{checkout_dir, checkout_display_path, checkout_mode_label, is_in_place};
use crate::commands::bundle::bundle_state;
use crate::commands::bundle::BundleStatus;
use crate::git::current_branch;
use crate::git::git_output;
use crate::model::PublicationEntry;
use crate::output as out;
use crate::status::status_label;
use crate::store::{ensure_workspace_fallback_status_is_unambiguous, load_active_bundle};
use crate::tracking::{detect_unrecorded_changes, status_note};
use anyhow::Result;
use serde::Serialize;

/// Machine-readable `knit status --json` document. A host that drives knit
/// reads this instead of parsing the table; the shape is a contract, so change
/// it only deliberately.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusDocument {
    bundle: String,
    resolved_from: String,
    state: String,
    repos: Vec<StatusRepo>,
    publications: StatusPublications,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusRepo {
    id: String,
    /// The branch the checkout is actually on. Null when there is no checkout.
    branch: Option<String>,
    /// The bundle's feature branch. Null before the branch is created.
    expected_branch: Option<String>,
    worktree: String,
    mode: String,
    checkout_present: bool,
    /// `clean`, `dirty`, or the missing-checkout reason.
    status: String,
    /// True when an in-place checkout sits on a branch other than the bundle's.
    wrong_branch: bool,
    /// Commits made outside knit that the ledger has not recorded yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    unrecorded: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusPublications {
    /// Review objects (PRs/MRs) recorded for this bundle.
    reviews: usize,
    /// Repos tracked in the bundle, for the "1/3 published" reading.
    repos: usize,
}

pub fn show_status(json: bool) -> Result<()> {
    let active = load_active_bundle()?;
    ensure_workspace_fallback_status_is_unambiguous(&active)?;
    let unrecorded = detect_unrecorded_changes(&active)?;
    let state = bundle_state(&active.bundle);

    let mut repos = Vec::with_capacity(active.bundle.repos.len());
    for repo in &active.bundle.repos {
        let expected_branch = repo.feature_branch.clone();
        let worktree = checkout_display_path(repo);
        let mode = checkout_mode_label(repo).to_string();
        let unrecorded_note = unrecorded
            .iter()
            .find(|change| change.repo_id == repo.id)
            .map(|change| status_note(change).to_string());

        let Some(status_dir) = checkout_dir(&active, repo) else {
            let missing = if is_in_place(repo) {
                "missing checkout"
            } else {
                "missing worktree"
            };
            repos.push(StatusRepo {
                id: repo.id.clone(),
                branch: None,
                expected_branch,
                worktree,
                mode,
                checkout_present: false,
                status: missing.to_string(),
                wrong_branch: false,
                unrecorded: unrecorded_note,
            });
            continue;
        };

        let actual_branch =
            current_branch(&status_dir)?.unwrap_or_else(|| "(detached)".to_string());
        let wrong_branch = is_in_place(repo)
            && repo.feature_branch.is_some()
            && Some(&actual_branch) != repo.feature_branch.as_ref();
        let short_status = git_output(&status_dir, ["status", "--short"])?;
        repos.push(StatusRepo {
            id: repo.id.clone(),
            branch: Some(actual_branch),
            expected_branch,
            worktree,
            mode,
            checkout_present: true,
            status: status_label(&short_status).to_string(),
            wrong_branch,
            unrecorded: unrecorded_note,
        });
    }

    if json {
        let document = StatusDocument {
            bundle: active.bundle.id.clone(),
            resolved_from: active.resolution_source.label().to_string(),
            state: state.as_str().to_string(),
            publications: StatusPublications {
                reviews: recorded_review_count(&active),
                repos: active.bundle.repos.len(),
            },
            repos,
        };
        println!("{}", serde_json::to_string_pretty(&document)?);
        return Ok(());
    }

    println!(
        "{} {} ({})",
        out::heading("Bundle:"),
        out::node(&active.bundle.id),
        active.resolution_source.label()
    );
    println!(
        "{} {}\n",
        out::heading("State:"),
        out::status(state.as_str())
    );
    println!(
        "{} {} {} {} {}",
        out::header_field("repo", 14),
        out::header_field("branch", 26),
        out::header_field("worktree", 48),
        out::header_field("mode", 10),
        out::heading("status")
    );

    for repo in &repos {
        let expected_branch = repo.expected_branch.as_deref().unwrap_or("(not created)");
        let branch = match (&repo.branch, repo.wrong_branch) {
            (Some(actual), true) => format!("{actual} != {expected_branch}"),
            _ => expected_branch.to_string(),
        };
        let mut label = repo.status.clone();
        if repo.wrong_branch {
            label.push_str(" (wrong branch)");
        }
        if let Some(note) = &repo.unrecorded {
            label.push_str(&format!(" ({note})"));
        }
        println!(
            "{} {} {} {} {}",
            out::repo_field(&repo.id, 14),
            out::branch_field(&branch, 26),
            out::path_field(&repo.worktree, 48),
            out::header_field(&repo.mode, 10),
            out::status(&label)
        );
    }

    print_publication_summary(&active);
    print_closed_summary(&active, state);

    Ok(())
}

fn print_closed_summary(active: &crate::store::ActiveBundle, state: BundleStatus) {
    if state != BundleStatus::Closed {
        return;
    }
    println!();
    println!(
        "{} ledger marker only; generated worktrees and local feature branches are preserved.",
        out::heading("Closed:")
    );
    advice::print(
        &active.root,
        format!(
            "to remove this bundle's local generated state, run `knit bundle delete {} --force --worktrees --branches` (add `--force-branches` if needed).",
            active.bundle.id
        ),
    );
}

fn print_publication_summary(active: &crate::store::ActiveBundle) {
    if active.bundle.publications.is_empty() || bundle_state(&active.bundle) != BundleStatus::Open {
        return;
    }
    let tracked_count = active.bundle.repos.len();
    let review_count = recorded_review_count(active);
    if review_count == 0 {
        return;
    }

    println!();
    println!(
        "{} {}/{} review object(s) recorded, not landed",
        out::heading("Publications:"),
        review_count,
        tracked_count
    );
    advice::print(
        &active.root,
        "when the user says to land/release, run `knit land` to create or show the plan, then `knit land apply` after inspection; do not merge the host review objects directly.",
    );
}

fn recorded_review_count(active: &crate::store::ActiveBundle) -> usize {
    active
        .bundle
        .publications
        .iter()
        .filter(|publication| is_review_publication(publication))
        .count()
}

fn is_review_publication(publication: &PublicationEntry) -> bool {
    crate::providers::is_review_kind(&publication.kind)
}
