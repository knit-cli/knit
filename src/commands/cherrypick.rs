use crate::checkout::{checkout_dir, ensure_mutable_checkouts};
use crate::git::{git_output, git_output_optional, rev_parse};
use crate::ids::{short_sha, slugify};
use crate::model::{BundleNode, ChangeGroup, CommitRef, RepoEntry};
use crate::output as out;
use crate::repo_selectors::resolve_repo_indexes;
use crate::store::{
    bundle_path, find_knit_root, read_json, save_active_bundle, set_bundle_override, ActiveBundle,
};
use crate::tracking::{sync_note, sync_observed_changes_for_repo_ids};
use anyhow::{bail, Context, Result};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::OsString;
use std::path::Path;

pub fn cherrypick_from_bundle(
    source_bundle_id: &str,
    targets: &[String],
    repo_selectors: &[String],
    dry_run: bool,
) -> Result<()> {
    if targets.is_empty() {
        bail!("Pass at least one source bundle selector to cherry-pick.");
    }

    let mut active = crate::store::load_active_bundle_for_update()?;
    ensure_mutable_checkouts(&active)?;
    let source = load_source_bundle(&active.root, source_bundle_id)?;
    let selected = selected_source_commits(&source, targets, repo_selectors, Some(&active))?;
    if selected.is_empty() {
        bail!("No source commits matched the requested selectors.");
    }

    print_plan(&selected, dry_run);
    if dry_run {
        return Ok(());
    }

    let repo_ids = apply_cherrypicks(&active, &selected)?;
    let changes = sync_observed_changes_for_repo_ids(&mut active, Some(&repo_ids))?;
    if changes.is_empty() {
        println!("{}", out::ok("No new commits were recorded."));
    } else {
        for change in &changes {
            println!(
                "{}: {}",
                out::repo(&change.repo_id),
                out::warn(sync_note(change))
            );
        }
        save_active_bundle(&active)?;
    }

    Ok(())
}

pub fn split_bundle(
    source_bundle_id: &str,
    title: Option<&str>,
    targets: &[String],
    repo_selectors: &[String],
    force: bool,
) -> Result<()> {
    if targets.is_empty() {
        bail!("Pass at least one source bundle selector to split.");
    }

    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd)
        .context("No Knit workspace found. Run `knit bundle start \"feature title\"` first.")?;
    let source = load_source_bundle(&root, source_bundle_id)?;
    let selected = selected_source_commits(&source, targets, repo_selectors, None)?;
    if selected.is_empty() {
        bail!("No source commits matched the requested selectors.");
    }

    let repo_ids = selected_repo_ids(&selected);
    let title = title
        .map(str::to_string)
        .unwrap_or_else(|| format!("{} split", source.title));
    let bundle_id = slugify(&title);
    preflight_split_destination(&root, &source, &repo_ids)?;

    crate::commands::init::start_bundle(
        &title,
        source.project_id.as_deref(),
        &repo_ids,
        false,
        true,
        false,
        force,
        false,
        None,
    )?;
    set_bundle_override(Some(bundle_id));
    cherrypick_from_bundle(source_bundle_id, targets, &repo_ids, false)
}

fn preflight_split_destination(
    root: &Path,
    source: &ChangeGroup,
    repo_ids: &[String],
) -> Result<()> {
    let Some(project_id) = source.project_id.as_deref() else {
        bail!(
            "Source bundle {} is not associated with a project. Start a destination bundle with the needed repos, then run `knit cherrypick --from {}`.",
            out::repo(&source.id),
            source.id
        );
    };

    let project = crate::commands::project::load_project_by_id(root, project_id)?;
    let project_repo_ids = project
        .repos
        .iter()
        .map(|repo| repo.id.as_str())
        .collect::<BTreeSet<_>>();
    let missing = repo_ids
        .iter()
        .filter(|repo_id| !project_repo_ids.contains(repo_id.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        bail!(
            "Project {} is missing repo(s) needed for this split: {}. Add them to the project or start a destination bundle manually and run `knit cherrypick --from {}`.",
            out::repo(project_id),
            missing.join(", "),
            source.id
        );
    }

    Ok(())
}

fn load_source_bundle(root: &Path, source_bundle_id: &str) -> Result<ChangeGroup> {
    let path = bundle_path(root, source_bundle_id);
    read_json(&path).with_context(|| format!("failed to load source bundle {}", path.display()))
}

#[derive(Debug, Clone)]
struct SourceCommit {
    repo_id: String,
    sha: String,
    selector: String,
}

fn selected_source_commits(
    source: &ChangeGroup,
    targets: &[String],
    repo_selectors: &[String],
    destination: Option<&ActiveBundle>,
) -> Result<Vec<SourceCommit>> {
    let allowed_repos = if repo_selectors.is_empty() {
        None
    } else if let Some(active) = destination {
        Some(
            resolve_repo_indexes(active, repo_selectors, false)?
                .into_iter()
                .map(|index| active.bundle.repos[index].id.clone())
                .collect::<BTreeSet<_>>(),
        )
    } else {
        Some(resolve_source_repo_ids(source, repo_selectors)?)
    };

    let mut selected = Vec::new();
    let mut seen = HashSet::new();
    for target in targets {
        let refs = resolve_source_target(source, target)?;
        for commit in refs {
            if allowed_repos
                .as_ref()
                .is_some_and(|repo_ids| !repo_ids.contains(&commit.repo_id))
            {
                continue;
            }
            let key = (commit.repo_id.clone(), commit.sha.clone());
            if seen.insert(key) {
                selected.push(SourceCommit {
                    repo_id: commit.repo_id,
                    sha: commit.sha,
                    selector: target.clone(),
                });
            }
        }
    }
    Ok(selected)
}

fn resolve_source_repo_ids(source: &ChangeGroup, selectors: &[String]) -> Result<BTreeSet<String>> {
    let mut ids = BTreeSet::new();
    for selector in selectors {
        let matches = source
            .repos
            .iter()
            .filter(|repo| source_repo_matches(repo, selector))
            .map(|repo| repo.id.clone())
            .collect::<Vec<_>>();
        if matches.is_empty() {
            bail!("No source repo matched `{selector}`.");
        }
        ids.extend(matches);
    }
    Ok(ids)
}

fn source_repo_matches(repo: &RepoEntry, selector: &str) -> bool {
    selector == repo.id
        || selector == repo.path
        || repo
            .worktree_path
            .as_ref()
            .is_some_and(|worktree_path| selector == worktree_path)
}

fn resolve_source_target(source: &ChangeGroup, target: &str) -> Result<Vec<CommitRef>> {
    let node = resolve_source_node(source, target)?;
    let node_refs = commits_for_node(source, node)?;
    if node_is_named_match(node, target) || target == "HEAD" || target.starts_with("HEAD~") {
        return Ok(node_refs);
    }

    let matching = node_refs
        .into_iter()
        .filter(|commit| commit.sha == target || commit.sha.starts_with(target))
        .collect::<Vec<_>>();
    if matching.is_empty() {
        bail!("No recorded git commit matched `{target}`.");
    }
    Ok(matching)
}

fn resolve_source_node<'a>(source: &'a ChangeGroup, target: &str) -> Result<&'a BundleNode> {
    let loggable = source
        .nodes
        .iter()
        .filter(|node| crate::selectors::is_loggable_node(node))
        .collect::<Vec<_>>();
    if loggable.is_empty() {
        bail!("Source bundle has no loggable entries.");
    }

    if target == "HEAD" || target.starts_with("HEAD~") {
        let offset = if target == "HEAD" {
            0
        } else {
            target
                .strip_prefix("HEAD~")
                .and_then(|raw| raw.parse::<usize>().ok())
                .with_context(|| format!("Invalid log selector `{target}`."))?
        };
        let Some(index) = loggable.len().checked_sub(1 + offset) else {
            bail!("Log selector `{target}` is before the start of the source bundle log.");
        };
        return Ok(loggable[index]);
    }

    let named = loggable
        .iter()
        .filter(|node| node_is_named_match(node, target))
        .copied()
        .collect::<Vec<_>>();
    match named.as_slice() {
        [node] => return Ok(node),
        [] => {}
        _ => bail!("`{target}` is ambiguous; use a longer bundle node id."),
    }

    let sha_matches = loggable
        .iter()
        .filter_map(|node| {
            commits_for_node(source, node)
                .ok()
                .is_some_and(|commits| {
                    commits
                        .iter()
                        .any(|commit| commit.sha == target || commit.sha.starts_with(target))
                })
                .then_some(*node)
        })
        .collect::<Vec<_>>();
    match sha_matches.as_slice() {
        [node] => Ok(node),
        [] => bail!("No source bundle log entry or recorded git commit matched `{target}`."),
        _ => bail!("`{target}` is ambiguous; use a longer git SHA."),
    }
}

fn node_is_named_match(node: &BundleNode, target: &str) -> bool {
    node.id == target
        || node.id.starts_with(target)
        || node
            .commit_group_id
            .as_deref()
            .is_some_and(|id| id == target || id.starts_with(target))
}

fn commits_for_node(source: &ChangeGroup, node: &BundleNode) -> Result<Vec<CommitRef>> {
    if !node.commits.is_empty() {
        return Ok(node.commits.clone());
    }
    if let Some(group_id) = &node.commit_group_id {
        if let Some(group) = source
            .commit_groups
            .iter()
            .find(|group| &group.id == group_id)
        {
            return Ok(group.commits.clone());
        }
    }

    let commits = node
        .repo_changes
        .iter()
        .flat_map(|change| {
            change.commits.iter().map(|sha| CommitRef {
                repo_id: change.repo_id.clone(),
                sha: sha.clone(),
            })
        })
        .collect::<Vec<_>>();
    if commits.is_empty() {
        bail!(
            "Source selector `{}` does not contain cherry-pickable commits.",
            node.id
        );
    }
    Ok(commits)
}

fn selected_repo_ids(selected: &[SourceCommit]) -> Vec<String> {
    selected
        .iter()
        .map(|commit| commit.repo_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn print_plan(selected: &[SourceCommit], dry_run: bool) {
    if dry_run {
        println!("{}", out::heading("Cherry-pick plan"));
    }
    for commit in selected {
        println!(
            "{}: {} {} ({})",
            out::repo(&commit.repo_id),
            if dry_run {
                out::movement("would pick")
            } else {
                out::movement("picking")
            },
            out::sha(short_sha(&commit.sha)),
            commit.selector
        );
    }
}

fn apply_cherrypicks(active: &ActiveBundle, selected: &[SourceCommit]) -> Result<Vec<String>> {
    let repo_by_id = active
        .bundle
        .repos
        .iter()
        .map(|repo| (repo.id.clone(), repo))
        .collect::<HashMap<_, _>>();
    let grouped = group_by_repo(selected);
    let mut changed_repo_ids = Vec::new();

    for (repo_id, commits) in grouped {
        let repo = repo_by_id.get(&repo_id).with_context(|| {
            format!(
                "{} is not tracked in the destination bundle. Add it first or use `knit bundle split`.",
                out::repo(&repo_id)
            )
        })?;
        let Some(cwd) = checkout_dir(active, repo) else {
            bail!("{}: no destination checkout is recorded.", repo_id);
        };
        ensure_clean_checkout(&repo_id, &cwd)?;

        let mut repo_changed = false;
        for commit in commits {
            ensure_commit_exists(&repo_id, &cwd, &commit.sha)?;
            match cherry_pick_one(&repo_id, &cwd, &commit.sha)? {
                PickOutcome::Picked => repo_changed = true,
                PickOutcome::SkippedEmpty => println!(
                    "{}: {} {}",
                    out::repo(&repo_id),
                    out::muted("already applied"),
                    out::sha(short_sha(&commit.sha))
                ),
            }
        }
        if repo_changed {
            changed_repo_ids.push(repo_id);
        }
    }

    Ok(changed_repo_ids)
}

fn group_by_repo(selected: &[SourceCommit]) -> Vec<(String, Vec<SourceCommit>)> {
    let mut order = Vec::new();
    let mut grouped: HashMap<String, Vec<SourceCommit>> = HashMap::new();
    for commit in selected {
        if !grouped.contains_key(&commit.repo_id) {
            order.push(commit.repo_id.clone());
        }
        grouped
            .entry(commit.repo_id.clone())
            .or_default()
            .push(commit.clone());
    }
    order
        .into_iter()
        .filter_map(|repo_id| grouped.remove(&repo_id).map(|commits| (repo_id, commits)))
        .collect()
}

fn ensure_clean_checkout(repo_id: &str, cwd: &Path) -> Result<()> {
    let status = git_output(cwd, ["status", "--porcelain"])?;
    if !status.trim().is_empty() {
        bail!(
            "{}: destination checkout has uncommitted changes in {}.",
            repo_id,
            cwd.display()
        );
    }
    Ok(())
}

fn ensure_commit_exists(repo_id: &str, cwd: &Path, sha: &str) -> Result<()> {
    rev_parse(cwd, &format!("{sha}^{{commit}}"))
        .with_context(|| format!("{}: commit {} is not available locally", repo_id, sha))?;
    Ok(())
}

enum PickOutcome {
    Picked,
    SkippedEmpty,
}

fn cherry_pick_one(repo_id: &str, cwd: &Path, sha: &str) -> Result<PickOutcome> {
    let mut args = vec![OsString::from("cherry-pick")];
    args.push(OsString::from(sha));
    match git_output(cwd, args) {
        Ok(_) => Ok(PickOutcome::Picked),
        Err(_) if is_empty_cherry_pick(cwd)? => {
            git_output(cwd, ["cherry-pick", "--skip"])?;
            Ok(PickOutcome::SkippedEmpty)
        }
        Err(error) => bail!(
            "{}: cherry-pick {} failed in {}.\n{}\nResolve the cherry-pick there, then run `knit sync` to record the result.",
            repo_id,
            short_sha(sha),
            cwd.display(),
            error
        ),
    }
}

fn is_empty_cherry_pick(cwd: &Path) -> Result<bool> {
    if git_output_optional(cwd, ["rev-parse", "--verify", "CHERRY_PICK_HEAD"])?.is_none() {
        return Ok(false);
    }
    Ok(git_output(cwd, ["status", "--porcelain"])?
        .trim()
        .is_empty())
}
