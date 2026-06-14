//! `knit history` and `knit related`. [`target`] resolves which repo/path a
//! related query is about; [`related`] joins git file history with Knit
//! history events and renders the cross-repo context.

mod related;
mod target;

use crate::history::{format_history_event, load_history_events, refresh_project_history};
use crate::ids::slugify;
use crate::model::KnitProject;
use crate::output as out;
use crate::store::{find_knit_root, load_config, project_path, read_json};
use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use related::{
    git_commits_for_paths, print_related_instance, related_instance_time, related_instances,
    related_repo_paths,
};
use target::resolve_related_target;

pub fn show_history(
    project: Option<&str>,
    limit: usize,
    repo: Option<&str>,
    bundle: Option<&str>,
) -> Result<()> {
    let (root, project_id) = resolve_project(project)?;
    let appended = refresh_project_history(&root, &project_id)?;
    if appended > 0 {
        println!(
            "{} {} new event(s)",
            out::heading("History refreshed:"),
            appended
        );
    }

    let mut events = load_history_events(&root, &project_id)?;
    events.retain(|event| {
        repo.is_none_or(|repo| event.repo_id.as_deref() == Some(repo))
            && bundle.is_none_or(|bundle| event.bundle_id.as_deref() == Some(bundle))
    });
    events.sort_by(|a, b| {
        let a_time = a.occurred_at.as_deref().unwrap_or(&a.recorded_at);
        let b_time = b.occurred_at.as_deref().unwrap_or(&b.recorded_at);
        a_time.cmp(b_time).then(a.event_id.cmp(&b.event_id))
    });

    if events.is_empty() {
        println!("{}", out::muted("No history events recorded yet."));
        return Ok(());
    }

    let start = events.len().saturating_sub(limit);
    for event in events.into_iter().skip(start) {
        println!("{}", format_history_event(&event));
    }
    Ok(())
}

pub fn refresh_history(project: Option<&str>) -> Result<()> {
    let (root, project_id) = resolve_project(project)?;
    let appended = refresh_project_history(&root, &project_id)?;
    println!(
        "{} {} {}",
        out::movement("refreshed"),
        out::repo(&project_id),
        out::muted(format!("{appended} new event(s)"))
    );
    Ok(())
}

pub fn show_related_history(
    project: Option<&str>,
    repo: Option<&str>,
    paths: &[PathBuf],
    limit: usize,
    commit_limit: usize,
    pull: bool,
    remote: Option<&str>,
) -> Result<()> {
    if paths.is_empty() {
        bail!("Pass at least one path to inspect.");
    }
    if limit == 0 {
        bail!("--limit must be greater than zero.");
    }
    if commit_limit == 0 {
        bail!("--commit-limit must be greater than zero.");
    }

    let (root, project_id) = resolve_project(project)?;
    if pull {
        crate::commands::remote::pull_history_from_remote(Some(&project_id), remote)?;
    }

    let project = load_project(&root, &project_id)?;
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let target = resolve_related_target(&root, &project, repo, paths, &cwd)?;
    let git_commits = git_commits_for_paths(&target.checkout, &target.paths, commit_limit)?;
    let commit_set = git_commits
        .iter()
        .map(|commit| commit.sha.clone())
        .collect::<BTreeSet<_>>();

    let appended = refresh_project_history(&root, &project_id)?;
    if appended > 0 {
        println!(
            "{} {} new event(s)",
            out::heading("History refreshed:"),
            appended
        );
    }

    println!(
        "{} {} {}",
        out::heading("Query:"),
        out::repo(&target.repo_id),
        target.paths.join(" ")
    );
    println!(
        "{} {} {}",
        out::heading("Git commits:"),
        git_commits.len(),
        out::muted(format!("inspected up to {commit_limit}"))
    );

    if git_commits.is_empty() {
        println!("{}", out::muted("No Git commits touched those paths."));
        return Ok(());
    }

    let events = load_history_events(&root, &project_id)?;
    let mut instances = related_instances(&events, &target.repo_id, &commit_set);
    if instances.is_empty() {
        println!(
            "{}",
            out::muted("No Knit history events matched those Git commits.")
        );
        println!(
            "{}",
            out::muted("Those commits may be ordinary Git history outside Knit, or local history may need `knit history pull`.")
        );
        return Ok(());
    }

    instances.sort_by(|left, right| {
        related_instance_time(right)
            .cmp(&related_instance_time(left))
            .then(left.bundle_id.cmp(&right.bundle_id))
            .then(left.scope_label().cmp(&right.scope_label()))
    });

    let repo_paths = related_repo_paths(&project, &target);
    let total = instances.len();
    for instance in instances.iter().take(limit) {
        print_related_instance(instance, &repo_paths);
    }
    if total > limit {
        println!(
            "{}",
            out::muted(format!(
                "{} more related instance(s) hidden; rerun with --limit {total}",
                total - limit
            ))
        );
    }

    Ok(())
}

fn load_project(root: &Path, project_id: &str) -> Result<KnitProject> {
    let path = project_path(root, project_id);
    read_json(&path).with_context(|| format!("failed to read project {}", path.display()))
}

fn resolve_project(project: Option<&str>) -> Result<(std::path::PathBuf, String)> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let config = load_config(&root)?;
    let project_id = project
        .map(slugify)
        .or(config.active_project)
        .context("No active project selected. Pass --project or run `knit init <name>`.")?;
    Ok((root, project_id))
}
