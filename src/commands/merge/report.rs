//! Read-side merge commands: `knit merge status` / `show`, and pushing a
//! recorded merge run's branch-target steps to their remotes.

use super::{
    acquire_run_locks, latest_merge_run, resolve_stored_path, short_or_dash, short_sha, MergeRun,
    MergeRunStatus, MergeStepStatus, MergeTargetKind,
};
use crate::advice;
use crate::git::{git_output, rev_parse};
use crate::ids::slugify;
use crate::output as out;
use crate::store::{read_json, write_json};
use crate::time::now_iso;
use anyhow::{bail, Result};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub(super) fn show_merge_status(root: &Path, run_selector: Option<&str>) -> Result<()> {
    let (path, run) = resolve_merge_run(root, run_selector, &[])?;
    println!(
        "{} {} {}",
        out::heading("Merge run"),
        out::node(&run.id),
        out::status(run.status.as_str())
    );
    println!("{} {} -> {}", out::heading("Flow:"), run.source, run.into);
    println!(
        "{} {}",
        out::heading("Run file:"),
        out::path(path.display())
    );
    println!();
    println!(
        "{}  {}  {}  {}  {}  {}",
        out::header_field("repo", 14),
        out::header_field("target", 18),
        out::header_field("status", 12),
        out::header_field("before", 8),
        out::header_field("after", 8),
        out::heading("checkout")
    );
    for step in &run.steps {
        println!(
            "{}  {}  {}  {}  {}  {}",
            out::repo_field(&step.repo_id, 14),
            out::branch_field(&step.target, 18),
            out::status(&format!("{:<12}", step.status)),
            out::sha(short_or_dash(&step.before_sha)),
            out::sha(step.after_sha.as_deref().map(short_sha).unwrap_or("-")),
            out::path(&step.checkout_path)
        );
        if let Some(pushed_sha) = &step.pushed_sha {
            println!(
                "  pushed {} {}",
                out::sha(short_sha(pushed_sha)),
                step.pushed_at.as_deref().unwrap_or("")
            );
        } else if step.target_kind == MergeTargetKind::Branch
            && step.status == MergeStepStatus::Succeeded
        {
            println!("  {}", out::muted("not pushed"));
        }
        if let Some(message) = &step.message {
            println!(
                "  {}",
                out::danger(message.lines().next().unwrap_or(message))
            );
        }
    }
    if run.status == MergeRunStatus::Conflicted {
        advice::print(
            root,
            "`knit merge --continue` after resolving conflicts, or `knit merge --abort`.",
        );
    }
    Ok(())
}

pub(super) fn show_merge_run_json(root: &Path, run_selector: Option<&str>) -> Result<()> {
    let (_, run) = resolve_merge_run(root, run_selector, &[])?;
    println!("{}", serde_json::to_string_pretty(&run)?);
    Ok(())
}

pub(super) fn push_recorded_merge_run(
    root: &Path,
    run_selector: Option<&str>,
    repos: &[String],
    set_upstream: bool,
) -> Result<()> {
    let (path, mut run) = resolve_merge_run(
        root,
        run_selector,
        &[MergeRunStatus::Succeeded, MergeRunStatus::PushFailed],
    )?;
    let _locks = acquire_run_locks(root, &run)?;
    push_merge_run_steps(root, &mut run, repos, set_upstream)?;
    run.status = MergeRunStatus::Succeeded;
    run.updated_at = now_iso();
    write_json(&path, &run)?;
    Ok(())
}

pub(super) fn push_merge_run_steps(
    root: &Path,
    run: &mut MergeRun,
    repos: &[String],
    set_upstream: bool,
) -> Result<()> {
    let repo_filter = repos
        .iter()
        .map(|repo| slugify(repo))
        .collect::<BTreeSet<_>>();
    let mut failures = Vec::new();
    let mut eligible = 0usize;
    for step in &mut run.steps {
        if step.target_kind != MergeTargetKind::Branch {
            continue;
        }
        if !repo_filter.is_empty() && !repo_filter.contains(&step.repo_id) {
            continue;
        }
        eligible += 1;
        if step.status != MergeStepStatus::Succeeded {
            failures.push(format!(
                "{}: step is {}, expected succeeded",
                step.repo_id, step.status
            ));
            continue;
        }
        let Some(after_sha) = &step.after_sha else {
            failures.push(format!("{}: no afterSha recorded", step.repo_id));
            continue;
        };
        let checkout = resolve_stored_path(root, &step.checkout_path);
        let head = match rev_parse(&checkout, "HEAD") {
            Ok(head) => head,
            Err(error) => {
                failures.push(format!("{}: {error:#}", step.repo_id));
                continue;
            }
        };
        if head != *after_sha {
            failures.push(format!(
                "{}: checkout HEAD {} does not match merge run afterSha {}",
                step.repo_id,
                short_sha(&head),
                short_sha(after_sha)
            ));
            continue;
        }
        let mut args = vec![std::ffi::OsString::from("push")];
        if set_upstream {
            args.push(std::ffi::OsString::from("-u"));
        }
        args.push(std::ffi::OsString::from("origin"));
        args.push(std::ffi::OsString::from(&step.target));
        match git_output(&checkout, args) {
            Ok(_) => {
                let now = now_iso();
                step.pushed_at = Some(now);
                step.pushed_sha = Some(after_sha.clone());
                step.push_remote = Some("origin".to_string());
                println!(
                    "{}: {} origin/{} {}",
                    out::repo(&step.repo_id),
                    out::movement("pushed"),
                    step.target,
                    out::sha(short_sha(after_sha))
                );
            }
            Err(error) => failures.push(format!("{}: {error:#}", step.repo_id)),
        }
    }
    if !failures.is_empty() {
        bail!("merge push failed:\n{}", failures.join("\n"));
    }
    if eligible == 0 {
        bail!("Merge run has no branch-target steps to push.");
    }
    Ok(())
}

fn resolve_merge_run(
    root: &Path,
    selector: Option<&str>,
    statuses: &[MergeRunStatus],
) -> Result<(PathBuf, MergeRun)> {
    if let Some(selector) = selector {
        let path = resolve_merge_run_selector(root, selector);
        let run: MergeRun = read_json(&path)?;
        if !statuses.is_empty() && !statuses.contains(&run.status) {
            bail!(
                "Merge run {} is {}, expected one of {}.",
                run.id,
                run.status,
                statuses
                    .iter()
                    .map(|status| status.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        return Ok((path, run));
    }
    latest_merge_run(root, statuses)
}

fn resolve_merge_run_selector(root: &Path, selector: &str) -> PathBuf {
    let path = PathBuf::from(selector);
    if path.exists() {
        return path;
    }
    root.join(".knit/merge-runs")
        .join(format!("{}.json", selector.trim_end_matches(".json")))
}
