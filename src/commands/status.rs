use crate::advice;
use crate::checkout::{checkout_dir, checkout_display_path, checkout_mode_label, is_in_place};
use crate::commands::bundle::bundle_state;
use crate::git::current_branch;
use crate::git::git_output;
use crate::model::{PublicationEntry, BUNDLE_STATE_CLOSED};
use crate::output as out;
use crate::status::status_label;
use crate::store::load_active_bundle;
use crate::tracking::{detect_unrecorded_changes, status_note};
use anyhow::Result;

pub fn show_status() -> Result<()> {
    let active = load_active_bundle()?;
    let unrecorded = detect_unrecorded_changes(&active)?;
    let state = bundle_state(&active.bundle);
    println!(
        "{} {} ({})",
        out::heading("Bundle:"),
        out::node(&active.bundle.id),
        active.resolution_source.label()
    );
    println!("{} {}\n", out::heading("State:"), out::status(state));
    println!(
        "{} {} {} {} {}",
        out::header_field("repo", 14),
        out::header_field("branch", 26),
        out::header_field("worktree", 48),
        out::header_field("mode", 10),
        out::heading("status")
    );

    for repo in &active.bundle.repos {
        let expected_branch = repo.feature_branch.as_deref().unwrap_or("(not created)");
        let worktree = checkout_display_path(repo);
        let Some(status_dir) = checkout_dir(&active, repo) else {
            let label = if is_in_place(repo) {
                "missing checkout"
            } else {
                "missing worktree"
            };
            println!(
                "{} {} {} {} {}",
                out::repo_field(&repo.id, 14),
                out::branch_field(expected_branch, 26),
                out::path_field(&worktree, 48),
                out::header_field(checkout_mode_label(repo), 10),
                out::status(label)
            );
            continue;
        };
        let actual_branch =
            current_branch(&status_dir)?.unwrap_or_else(|| "(detached)".to_string());
        let branch = if is_in_place(repo)
            && repo.feature_branch.is_some()
            && actual_branch != expected_branch
        {
            format!("{actual_branch} != {expected_branch}")
        } else {
            expected_branch.to_string()
        };
        let short_status = git_output(&status_dir, ["status", "--short"])?;
        let mut label = status_label(&short_status).to_string();
        if is_in_place(repo) && repo.feature_branch.is_some() && actual_branch != expected_branch {
            label.push_str(" (wrong branch)");
        }
        if let Some(change) = unrecorded.iter().find(|change| change.repo_id == repo.id) {
            label.push_str(&format!(" ({})", status_note(change)));
        }
        println!(
            "{} {} {} {} {}",
            out::repo_field(&repo.id, 14),
            out::branch_field(&branch, 26),
            out::path_field(&worktree, 48),
            out::header_field(checkout_mode_label(repo), 10),
            out::status(&label)
        );
    }

    print_publication_summary(&active);
    print_closed_summary(&active, state);

    Ok(())
}

fn print_closed_summary(active: &crate::store::ActiveBundle, state: &str) {
    if state != BUNDLE_STATE_CLOSED {
        return;
    }
    println!();
    println!(
        "{} {}",
        out::heading("Closed:"),
        "ledger marker only; generated worktrees and local feature branches are preserved."
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
    if active.bundle.publications.is_empty() || has_landed_node(&active.bundle) {
        return;
    }
    let tracked_count = active.bundle.repos.len();
    let github_prs = active
        .bundle
        .publications
        .iter()
        .filter(|publication| is_github_pr(publication))
        .count();
    if github_prs == 0 {
        return;
    }

    println!();
    println!(
        "{} {}/{} GitHub PR(s) recorded, not landed",
        out::heading("Publications:"),
        github_prs,
        tracked_count
    );
    advice::print(
        &active.root,
        "when the user says to land/release, run `knit land` to create or show the plan, then `knit land apply` after inspection; do not use `gh pr merge`.",
    );
}

fn is_github_pr(publication: &PublicationEntry) -> bool {
    publication.provider == "github" && publication.kind == "pull_request"
}

fn has_landed_node(bundle: &crate::model::ChangeGroup) -> bool {
    bundle
        .nodes
        .iter()
        .any(|node| node.node_type == "feature.landed")
}
