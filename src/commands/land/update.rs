//! `knit land update` — merge each feature branch up to its PR base branch
//! (or record an externally-resolved merge with `--continue-merge`), then
//! optionally push and refresh the recorded PR metadata.

use super::bundle_primary_provider;
use crate::checkout::checkout_dir;
use crate::git::{current_branch, git_output, is_ancestor, rev_list, rev_parse};
use crate::ids::{node_id, short_sha};
use crate::model::{BundleNode, RepoChange};
use crate::output as out;
use crate::providers::{self, publication_for_repo, PrTarget};
use crate::repo_selectors::resolve_repo_indexes;
use crate::store::{load_active_bundle_for_update, save_active_bundle, ActiveBundle};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::path::PathBuf;

pub(super) fn run_branch_update(
    selectors: &[String],
    all: bool,
    push: bool,
    set_upstream: bool,
    continue_merge: bool,
) -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    if active.bundle.repos.is_empty() {
        bail!("The resolved bundle has no repos. Run `knit bundle add <repo-path>` first.");
    }
    let indexes = resolve_repo_indexes(&active, selectors, all)?;
    let targets = indexes
        .iter()
        .map(|index| update_target(&active, *index))
        .collect::<Result<Vec<_>>>()?;

    if !continue_merge {
        preflight_update_targets(&targets)?;
    } else {
        preflight_continue_targets(&targets)?;
    }

    let mut changes = Vec::new();
    if continue_merge {
        for target in &targets {
            if let Some(change) = record_existing_update(&mut active, target)? {
                print_update_change(&change, "recorded");
                changes.push(change);
            } else {
                println!(
                    "{}: {}",
                    out::repo(&target.repo_id),
                    out::muted("unchanged")
                );
            }
        }
    } else {
        for target in &targets {
            match merge_base_into_feature(&mut active, target) {
                Ok(Some(change)) => {
                    print_update_change(&change, "updated");
                    changes.push(change);
                }
                Ok(None) => {
                    println!(
                        "{}: {}",
                        out::repo(&target.repo_id),
                        out::muted("already contains latest base")
                    );
                }
                Err(error) => {
                    bail!(
                        "{}: failed to update from base: {error:#}\nResolve the merge in {}, commit it, then run `knit land update --continue-merge{}`.",
                        target.repo_id,
                        target.cwd.display(),
                        if push { " --push" } else { "" }
                    );
                }
            }
        }
    }

    if !changes.is_empty() {
        append_land_update_node(&mut active, changes)?;
        save_active_bundle(&active)?;
    } else {
        println!("{}", out::ok("No feature branches needed base updates."));
    }

    if push {
        push_update_targets(&targets, set_upstream)?;
        refresh_update_publications(&mut active, &targets)?;
        save_active_bundle(&active)?;
        // Mirror the pushed feature branches into the KnitHub remote bundle
        // (default on; see `knit config set push-sync`).
        crate::commands::remote::maybe_sync_bundle_to_remote(&[], false)?;
    }

    Ok(())
}

struct LandUpdateTarget {
    repo_index: usize,
    repo_id: String,
    cwd: PathBuf,
    feature_branch: String,
    base_branch: String,
    publication_url: String,
    recorded_head: String,
}

fn update_target(active: &ActiveBundle, repo_index: usize) -> Result<LandUpdateTarget> {
    let repo = &active.bundle.repos[repo_index];
    let publication = publication_for_repo(&active.bundle, &repo.id).with_context(|| {
        format!(
            "{}: no PR publication recorded. Run `knit publish create` first.",
            repo.id
        )
    })?;
    let Some(cwd) = checkout_dir(active, repo) else {
        bail!("{}: no feature checkout is recorded.", repo.id);
    };
    let feature_branch = repo.feature_branch.clone().with_context(|| {
        format!(
            "{}: no feature branch recorded. Run `knit bundle worktree`.",
            repo.id
        )
    })?;
    let recorded_head = repo.head_sha.clone().with_context(|| {
        format!(
            "{}: no recorded feature head. Run `knit sync` before updating.",
            repo.id
        )
    })?;

    Ok(LandUpdateTarget {
        repo_index,
        repo_id: repo.id.clone(),
        cwd,
        feature_branch,
        base_branch: publication.base_branch.clone(),
        publication_url: publication.url.clone(),
        recorded_head,
    })
}

fn preflight_update_targets(targets: &[LandUpdateTarget]) -> Result<()> {
    for target in targets {
        ensure_update_branch(target)?;
        ensure_clean_worktree(target)?;
        let actual_head = rev_parse(&target.cwd, "HEAD")
            .with_context(|| format!("{}: failed to read HEAD", target.repo_id))?;
        if actual_head != target.recorded_head {
            bail!(
                "{}: feature checkout is at {}, but the bundle records {}. Run `knit sync` first, or use `knit land update --continue-merge` after resolving an update merge.",
                target.repo_id,
                out::sha(short_sha(&actual_head)),
                out::sha(short_sha(&target.recorded_head))
            );
        }
    }
    Ok(())
}

fn preflight_continue_targets(targets: &[LandUpdateTarget]) -> Result<()> {
    for target in targets {
        ensure_update_branch(target)?;
        ensure_clean_worktree(target)?;
    }
    Ok(())
}

fn ensure_update_branch(target: &LandUpdateTarget) -> Result<()> {
    let actual = current_branch(&target.cwd)?.unwrap_or_else(|| "(detached HEAD)".to_string());
    if actual != target.feature_branch {
        bail!(
            "{}: expected feature branch `{}`, found `{actual}` in {}.",
            target.repo_id,
            target.feature_branch,
            target.cwd.display()
        );
    }
    Ok(())
}

fn ensure_clean_worktree(target: &LandUpdateTarget) -> Result<()> {
    let status = git_output(&target.cwd, ["status", "--short"])?;
    if !status.trim().is_empty() {
        bail!(
            "{}: feature checkout has uncommitted changes in {}. Commit or clean them before updating.",
            target.repo_id,
            target.cwd.display()
        );
    }
    Ok(())
}

fn merge_base_into_feature(
    active: &mut ActiveBundle,
    target: &LandUpdateTarget,
) -> Result<Option<RepoChange>> {
    git_output(
        &target.cwd,
        [
            OsString::from("fetch"),
            OsString::from("origin"),
            OsString::from(&target.base_branch),
        ],
    )
    .with_context(|| {
        format!(
            "{}: failed to fetch origin/{}",
            target.repo_id, target.base_branch
        )
    })?;
    let base_sha = rev_parse(&target.cwd, "FETCH_HEAD")
        .with_context(|| format!("{}: failed to read fetched base head", target.repo_id))?;
    let before = rev_parse(&target.cwd, "HEAD")
        .with_context(|| format!("{}: failed to read feature head", target.repo_id))?;

    if is_ancestor(&target.cwd, &base_sha, &before) {
        return Ok(None);
    }

    let base_label = format!("origin/{}", target.base_branch);
    git_output(
        &target.cwd,
        [
            OsString::from("merge"),
            OsString::from("--no-ff"),
            OsString::from("--no-edit"),
            OsString::from(&base_label),
        ],
    )
    .with_context(|| format!("{}: git merge {base_label} failed", target.repo_id))?;

    let after = rev_parse(&target.cwd, "HEAD")
        .with_context(|| format!("{}: failed to read updated feature head", target.repo_id))?;
    let change = advanced_change(&target.cwd, target.repo_id.clone(), before, after)?;
    active.bundle.repos[target.repo_index].head_sha = Some(change.after_sha.clone());
    Ok(Some(change))
}

fn record_existing_update(
    active: &mut ActiveBundle,
    target: &LandUpdateTarget,
) -> Result<Option<RepoChange>> {
    let after = rev_parse(&target.cwd, "HEAD")
        .with_context(|| format!("{}: failed to read feature head", target.repo_id))?;
    if after == target.recorded_head {
        return Ok(None);
    }
    let change = advanced_change(
        &target.cwd,
        target.repo_id.clone(),
        target.recorded_head.clone(),
        after,
    )?;
    active.bundle.repos[target.repo_index].head_sha = Some(change.after_sha.clone());
    Ok(Some(change))
}

fn advanced_change(
    cwd: &std::path::Path,
    repo_id: String,
    before_sha: String,
    after_sha: String,
) -> Result<RepoChange> {
    if !is_ancestor(cwd, &before_sha, &after_sha) {
        bail!(
            "{repo_id}: update moved the branch in a non-forward direction from {} to {}",
            short_sha(&before_sha),
            short_sha(&after_sha)
        );
    }
    Ok(RepoChange {
        repo_id,
        movement: "advanced".to_string(),
        before_sha: Some(before_sha.clone()),
        after_sha: after_sha.clone(),
        commits: rev_list(cwd, &before_sha, &after_sha).context("failed to list update commits")?,
        dropped_commits: Vec::new(),
    })
}

fn append_land_update_node(active: &mut ActiveBundle, changes: Vec<RepoChange>) -> Result<()> {
    let now = now_iso();
    let provider = bundle_primary_provider(active);
    active.bundle.nodes.push(BundleNode::land_update(
        node_id("land_update"),
        now.clone(),
        provider,
        changes,
    ));
    active.bundle.head_node_id = active.bundle.nodes.last().map(|node| node.id.clone());
    active.bundle.updated_at = now;
    Ok(())
}

fn push_update_targets(targets: &[LandUpdateTarget], set_upstream: bool) -> Result<()> {
    let mut failures = Vec::new();
    for target in targets {
        if let Err(error) = push_update_target(target, set_upstream) {
            println!(
                "{}: {}",
                out::repo(&target.repo_id),
                out::danger("push failed")
            );
            failures.push(format!("{}: {error:#}", target.repo_id));
        }
    }

    if !failures.is_empty() {
        bail!("base update push failed:\n{}", failures.join("\n"));
    }
    Ok(())
}

fn push_update_target(target: &LandUpdateTarget, set_upstream: bool) -> Result<()> {
    let mut args = vec![OsString::from("push")];
    if set_upstream {
        args.push(OsString::from("--set-upstream"));
    }
    args.push(OsString::from("origin"));
    args.push(OsString::from(&target.feature_branch));
    git_output(&target.cwd, args)?;
    let sha = rev_parse(&target.cwd, "HEAD")?;
    println!(
        "{}: {} {} {}",
        out::repo(&target.repo_id),
        out::movement("pushed"),
        out::branch(format!("origin/{}", target.feature_branch)),
        out::sha(short_sha(&sha))
    );
    Ok(())
}

fn refresh_update_publications(
    active: &mut ActiveBundle,
    targets: &[LandUpdateTarget],
) -> Result<()> {
    for target in targets {
        let repo = active.bundle.repos[target.repo_index].clone();
        let forge = providers::for_repo(&repo)?;
        let pr_target = PrTarget::checkout(&target.cwd);
        let pr = forge
            .view(&pr_target, &target.publication_url)
            .with_context(|| format!("{}: failed to refresh PR metadata", target.repo_id))?;
        providers::upsert_publication(&mut active.bundle, &repo, forge.as_ref(), &pr);
    }
    Ok(())
}

fn print_update_change(change: &RepoChange, verb: &str) {
    println!(
        "{}: {} {} -> {} ({} commit(s))",
        out::repo(&change.repo_id),
        out::movement(verb),
        change
            .before_sha
            .as_deref()
            .map(short_sha)
            .map(out::sha)
            .unwrap_or_else(|| out::muted("-")),
        out::sha(short_sha(&change.after_sha)),
        change.commits.len()
    );
}
