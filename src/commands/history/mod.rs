//! `knit history` and `knit related`. [`target`] resolves which repo/path a
//! related query is about; [`related`] joins git file history with Knit
//! history events and renders the cross-repo context.

pub(crate) mod query;
mod related;
mod target;

use crate::history::{
    format_history_event, load_history_events, rebuild_project_history, refresh_project_history,
};
use crate::history_query::{query_project_history, HistoryGrouping, HistoryQuery};
use crate::model::KnitProject;
use crate::output as out;
use crate::store::{project_path, read_json};
use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use related::{
    git_commits_for_paths, prefetch_commit_subjects, print_related_instance, related_instance_time,
    related_instances, related_repo_paths,
};
use target::resolve_related_target;

pub fn show_history(
    project: Option<&str>,
    limit: usize,
    repo: Option<&str>,
    bundle: Option<&str>,
    kinds: &[String],
    expression: Option<&str>,
) -> Result<()> {
    expression
        .map(crate::history_query::expression::parse)
        .transpose()?;
    let (root, project_id) = query::resolve_query_project(project)?;
    let query = HistoryQuery {
        expression: query::expression(&root, Some(&project_id), expression)?,
        bundle_id: bundle.map(ToString::to_string),
        repos: repo.map(|repo| vec![repo.to_string()]),
        kinds: kinds.to_vec(),
        grouping: HistoryGrouping::Event,
        reverse: true,
        limit: Some(limit),
        ..HistoryQuery::default()
    };
    let entries = query_project_history(&root, &project_id, &query)?;
    let events = entries
        .into_iter()
        .flat_map(|entry| entry.events)
        .collect::<Vec<_>>();

    if events.is_empty() {
        println!("{}", out::muted("No history events recorded yet."));
        return Ok(());
    }

    for event in events {
        println!("{}", format_history_event(&event));
    }
    Ok(())
}

pub fn refresh_history(project: Option<&str>, rebuild: bool) -> Result<()> {
    let (root, project_id) = resolve_project(project)?;
    if rebuild {
        let summary = rebuild_project_history(&root, &project_id)?;
        println!(
            "{} {} {}",
            out::movement("rebuilt"),
            out::repo(&project_id),
            out::muted(format!(
                "{} updated, {} new, {} preserved event(s)",
                summary.replaced, summary.added, summary.preserved
            ))
        );
        return Ok(());
    }

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
    // Direct ledger join: obsolete base.commit rows never surface here, the
    // same exclusion the indexed queries apply.
    let events: Vec<_> = events
        .into_iter()
        .filter(|event| !crate::history::is_obsolete_base_commit(event))
        .collect();
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
    let displayed = &instances[..limit.min(instances.len())];
    let subjects = prefetch_commit_subjects(displayed, &repo_paths);
    for instance in displayed {
        print_related_instance(instance, &repo_paths, &subjects);
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
    query::resolve_query_project(project)
}
