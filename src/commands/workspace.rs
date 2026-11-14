//! Project-wide workspace state that is intentionally independent of bundle context.

use crate::commands::base::inspect_base;
use crate::commands::bundle::list_open_bundles;
use crate::commands::project::load_project_by_id;
use crate::git::{git_output, ref_commit_sha};
use crate::ids::short_sha;
use crate::model::{ChangeGroup, ProjectRepoEntry};
use crate::output as out;
use crate::review_base;
use crate::store::{find_knit_root, load_config};
use anyhow::{Context, Result};
use std::path::Path;

pub fn show_workspace_status() -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let config = load_config(&root)?;
    let project_id = config
        .active_project
        .context("No active Knit project. Run `knit init <name>` first.")?;
    let project = load_project_by_id(&root, &project_id)?;
    let bundles = list_open_bundles(&root)?;

    println!("{} {}", out::heading("Project:"), out::repo(&project_id));
    for repo in &project.repos {
        let state = inspect_base(Path::new(&repo.path), &repo.base_branch)
            .with_context(|| format!("{}: failed to inspect workspace state", repo.id))?;
        let local = state
            .local_sha
            .as_deref()
            .map(short_sha)
            .unwrap_or_else(|| "missing".to_string());
        let remote = state
            .remote_sha
            .as_deref()
            .map(short_sha)
            .unwrap_or_else(|| "missing".to_string());
        let ahead = state
            .ahead
            .map(|value| value.to_string())
            .unwrap_or_else(|| "?".to_string());
        let behind = state
            .behind
            .map(|value| value.to_string())
            .unwrap_or_else(|| "?".to_string());
        let cleanliness = if state.dirty { "dirty" } else { "clean" };
        println!(
            "  {} current={} base={} local={} origin={} ahead={} behind={} {}",
            out::repo(&repo.id),
            out::branch(&state.current_branch),
            out::branch(&repo.base_branch),
            out::sha(local),
            out::sha(remote),
            ahead,
            behind,
            out::muted(cleanliness)
        );
        let notes = review_base_notes(repo, &bundles)
            .with_context(|| format!("{}: failed to resolve review bases", repo.id))?;
        if !notes.is_empty() {
            println!(
                "    {} ahead/behind compare {} with origin/{}; open reviews target another base:",
                out::warn("note:"),
                repo.base_branch,
                repo.base_branch
            );
            for note in notes {
                println!("      {note}");
            }
        }
    }

    if bundles.is_empty() {
        println!("{} 0", out::heading("Open bundles:"));
    } else {
        let ids: Vec<_> = bundles.iter().map(|bundle| bundle.id.as_str()).collect();
        println!(
            "{} {} ({})",
            out::heading("Open bundles:"),
            bundles.len(),
            ids.join(", ")
        );
    }
    Ok(())
}

/// One line per open bundle whose recorded review for this repo targets a
/// base other than the source checkout's `origin/<base>`. Reads cached refs only.
fn review_base_notes(repo: &ProjectRepoEntry, bundles: &[ChangeGroup]) -> Result<Vec<String>> {
    let cwd = Path::new(&repo.path);
    let origin_ref = format!("refs/remotes/origin/{}", repo.base_branch);
    let mut notes = Vec::new();
    for bundle in bundles {
        let Some(entry) = bundle.repos.iter().find(|entry| entry.id == repo.id) else {
            continue;
        };
        let Some(base) = review_base::recorded(bundle, entry)? else {
            continue;
        };
        let review = base.review_url.as_deref().unwrap_or_default();
        if let Some(repository) = base.review_outside_origin(cwd) {
            notes.push(format!(
                "{}: {review} is in {repository}, but the bundle records no contribution target, so its base is unknown here; `knit --bundle {} rebase` explains how to record it",
                bundle.id, bundle.id
            ));
            continue;
        }
        if base.is_origin_branch(cwd, &repo.base_branch) {
            continue;
        }
        let mut note = format!("{}: {review} targets {}", bundle.id, base.label());
        match ref_commit_sha(cwd, &base.tracking_ref(entry)?)? {
            Some(base_sha) => {
                note.push_str(&format!(" {}", out::sha(short_sha(&base_sha))));
                let mut counts = Vec::new();
                if ref_commit_sha(cwd, &origin_ref)?.is_some() {
                    counts.push(format!(
                        "origin/{} behind={}",
                        repo.base_branch,
                        count(cwd, &origin_ref, &base_sha)?
                    ));
                }
                if let Some(branch) = entry.feature_branch.as_deref() {
                    let feature_ref = format!("refs/heads/{branch}");
                    if ref_commit_sha(cwd, &feature_ref)?.is_some() {
                        counts.push(format!(
                            "{branch} behind={}",
                            count(cwd, &feature_ref, &base_sha)?
                        ));
                    }
                }
                if !counts.is_empty() {
                    note.push_str(&format!("; {}", counts.join(", ")));
                }
            }
            None => {
                note.push_str(" (not fetched");
                if base.is_origin() || base.branch == entry.base_branch {
                    note.push_str(&format!("; run `knit --bundle {} fetch`", bundle.id));
                }
                note.push(')');
            }
        }
        notes.push(note);
    }
    Ok(notes)
}

fn count(cwd: &Path, from: &str, to: &str) -> Result<String> {
    git_output(cwd, ["rev-list", "--count", &format!("{from}..{to}")])
}
