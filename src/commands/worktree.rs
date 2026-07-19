use crate::checkout::is_in_place;
use crate::commands::agents::{
    print_bundle_worktree_agents_summary, write_bundle_worktree_agents_md,
};
use crate::git::{
    branch_exists, current_branch, git_output, git_output_optional, is_git_worktree, ref_exists,
    resolve_base_ref, rev_parse,
};
use crate::ids::node_id;
use crate::model::{BundleNode, RepoEntry};
use crate::output as out;
use crate::store::{load_active_bundle_for_update, save_active_bundle, ActiveBundle};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

pub fn create_worktrees() -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    if active.bundle.repos.is_empty() {
        bail!("The resolved bundle has no repos. Run `knit bundle add <repo-path>` first.");
    }

    let materialized_repo_ids = materialize_repos(&mut active, None)?;
    let bundle_agents = write_bundle_worktree_agents_md(&active)?;
    print_bundle_worktree_agents_summary(bundle_agents.as_deref());
    let now = now_iso();
    active.bundle.nodes.push(BundleNode::worktrees_materialized(
        node_id("worktree"),
        now,
        materialized_repo_ids,
    ));
    active.bundle.head_node_id = active.bundle.nodes.last().map(|node| node.id.clone());
    active.bundle.updated_at = now_iso();
    save_active_bundle(&active)?;
    Ok(())
}

pub fn materialize_repos(
    active: &mut ActiveBundle,
    only_repo_ids: Option<&[String]>,
) -> Result<Vec<String>> {
    let bundle_id = active.bundle.id.clone();
    fs::create_dir_all(active.root.join(".knit/worktrees").join(&bundle_id))
        .context("failed to create bundle worktree directory")?;

    let jobs: Vec<(usize, RepoEntry)> = active
        .bundle
        .repos
        .iter()
        .enumerate()
        .filter(|(_, repo)| {
            only_repo_ids.is_none_or(|repo_ids| repo_ids.iter().any(|repo_id| repo_id == &repo.id))
        })
        .map(|(index, repo)| (index, repo.clone()))
        .collect();

    if jobs.is_empty() {
        return Ok(Vec::new());
    }

    let root = active.root.clone();
    let results: Vec<(String, Result<MaterializeResult>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = jobs
            .iter()
            .map(|(repo_index, repo)| {
                let repo_index = *repo_index;
                let repo = repo.clone();
                let root = root.clone();
                let bundle_id = bundle_id.clone();
                let repo_id = repo.id.clone();
                scope.spawn(move || {
                    (
                        repo_id,
                        materialize_one_repo(&root, &bundle_id, repo_index, &repo),
                    )
                })
            })
            .collect();

        handles
            .into_iter()
            .map(|handle| handle.join().expect("worktree worker thread panicked"))
            .collect()
    });

    let mut materialized_repo_ids = Vec::new();
    let mut failures = Vec::new();
    for (repo_id, result) in results {
        match result {
            Ok(update) => {
                apply_materialize_result(&mut active.bundle.repos[update.repo_index], &update);
                print_materialize_result(&update);
                materialized_repo_ids.push(repo_id);
            }
            Err(error) => {
                crate::human!(
                    "{}: {}",
                    out::repo(&repo_id),
                    out::danger("worktree failed")
                );
                failures.push(format!("{repo_id}: {error:#}"));
            }
        }
    }

    if !failures.is_empty() {
        bail!("worktree failed:\n{}", failures.join("\n"));
    }

    materialized_repo_ids.sort_by(|left, right| {
        active
            .bundle
            .repos
            .iter()
            .position(|repo| &repo.id == left)
            .cmp(
                &active
                    .bundle
                    .repos
                    .iter()
                    .position(|repo| &repo.id == right),
            )
    });

    Ok(materialized_repo_ids)
}

struct MaterializeResult {
    repo_index: usize,
    repo_id: String,
    base_sha: Option<String>,
    feature_branch: Option<String>,
    worktree_path: Option<String>,
    head_sha: Option<String>,
    log: MaterializeLog,
}

enum MaterializeLog {
    InPlace,
    WorktreeExists(String),
    WorktreeFromBranch,
    WorktreeFromOrigin(String),
    WorktreeCreated(String),
}

fn apply_materialize_result(repo: &mut RepoEntry, update: &MaterializeResult) {
    if let Some(base_sha) = &update.base_sha {
        if repo.base_sha.is_none() {
            repo.base_sha = Some(base_sha.clone());
        }
    }
    if let Some(feature_branch) = &update.feature_branch {
        repo.feature_branch = Some(feature_branch.clone());
    }
    if let Some(worktree_path) = &update.worktree_path {
        repo.worktree_path = Some(worktree_path.clone());
    }
    if let Some(head_sha) = &update.head_sha {
        match &update.log {
            MaterializeLog::WorktreeExists(_) if repo.head_sha.is_some() => {}
            _ => repo.head_sha = Some(head_sha.clone()),
        }
    }
}

fn print_materialize_result(update: &MaterializeResult) {
    match &update.log {
        MaterializeLog::InPlace => crate::human!(
            "{}: using in-place checkout at {}",
            out::repo(&update.repo_id),
            out::path(
                update
                    .worktree_path
                    .as_deref()
                    .unwrap_or(update.repo_id.as_str())
            )
        ),
        MaterializeLog::WorktreeExists(worktree_path) => crate::human!(
            "{}: worktree already present at {}",
            out::repo(&update.repo_id),
            out::path(worktree_path)
        ),
        MaterializeLog::WorktreeFromBranch => crate::human!(
            "{}: {} worktree from existing branch",
            out::repo(&update.repo_id),
            out::movement("created")
        ),
        MaterializeLog::WorktreeFromOrigin(remote_ref) => crate::human!(
            "{}: {} worktree from {}",
            out::repo(&update.repo_id),
            out::movement("created"),
            out::branch(remote_ref)
        ),
        MaterializeLog::WorktreeCreated(feature_branch) => crate::human!(
            "{}: {} {}",
            out::repo(&update.repo_id),
            out::movement("created"),
            out::branch(feature_branch)
        ),
    }
}

fn materialize_one_repo(
    root: &Path,
    bundle_id: &str,
    repo_index: usize,
    repo: &RepoEntry,
) -> Result<MaterializeResult> {
    let feature_branch = format!("knit/{bundle_id}");
    let repo_root = PathBuf::from(&repo.path);

    if is_in_place(repo) {
        let update = materialize_in_place(repo_index, repo, &repo_root, &feature_branch)?;
        return Ok(update);
    }

    let worktree_path = format!(".knit/worktrees/{bundle_id}/{}", repo.id);
    let worktree_abs = root.join(&worktree_path);
    let base_ref = resolve_base_ref(&repo_root, &repo.base_branch);
    let base_sha = rev_parse(&repo_root, &base_ref)
        .with_context(|| format!("{}: failed to resolve base ref {base_ref}", repo.id))?;

    let mut update = MaterializeResult {
        repo_index,
        repo_id: repo.id.clone(),
        base_sha: if repo.base_sha.is_none() {
            Some(base_sha)
        } else {
            None
        },
        feature_branch: Some(feature_branch.clone()),
        worktree_path: Some(worktree_path.clone()),
        head_sha: None,
        log: MaterializeLog::WorktreeCreated(feature_branch.clone()),
    };

    if worktree_abs.exists() {
        if is_git_worktree(&worktree_abs) {
            update.head_sha = Some(
                rev_parse(&worktree_abs, "HEAD")
                    .with_context(|| format!("{}: failed to read worktree HEAD", repo.id))?,
            );
            update.log = MaterializeLog::WorktreeExists(worktree_path);
            return Ok(update);
        }
        bail!(
            "{}: {} exists but is not a git worktree",
            repo.id,
            worktree_abs.display()
        );
    }

    if let Some(parent) = worktree_abs.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create worktree parent {}", parent.display()))?;
    }

    if branch_exists(&repo_root, &feature_branch) {
        git_output(
            &repo_root,
            [
                OsString::from("worktree"),
                OsString::from("add"),
                worktree_abs.as_os_str().to_os_string(),
                OsString::from(&feature_branch),
            ],
        )
        .with_context(|| format!("failed to add worktree for {}", repo.id))?;
        update.head_sha = Some(
            rev_parse(&worktree_abs, "HEAD")
                .with_context(|| format!("{}: failed to read worktree HEAD", repo.id))?,
        );
        update.log = MaterializeLog::WorktreeFromBranch;
        return Ok(update);
    }

    // Another user may already have pushed this bundle's feature branch.
    // Starting from origin instead of base keeps a second workspace on the
    // same history; forking from base here would diverge immediately.
    if let Some(remote_ref) = origin_feature_ref(&repo_root, &feature_branch) {
        git_output(
            &repo_root,
            [
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("--track"),
                OsString::from("-b"),
                OsString::from(&feature_branch),
                worktree_abs.as_os_str().to_os_string(),
                OsString::from(&remote_ref),
            ],
        )
        .with_context(|| {
            format!(
                "failed to create worktree from {remote_ref} for {}",
                repo.id
            )
        })?;
        update.head_sha = Some(
            rev_parse(&worktree_abs, "HEAD")
                .with_context(|| format!("{}: failed to read worktree HEAD", repo.id))?,
        );
        update.log = MaterializeLog::WorktreeFromOrigin(remote_ref);
        return Ok(update);
    }

    git_output(
        &repo_root,
        [
            OsString::from("worktree"),
            OsString::from("add"),
            OsString::from("-b"),
            OsString::from(&feature_branch),
            worktree_abs.as_os_str().to_os_string(),
            OsString::from(base_ref),
        ],
    )
    .with_context(|| format!("failed to create branch/worktree for {}", repo.id))?;
    update.head_sha = Some(
        rev_parse(&worktree_abs, "HEAD")
            .with_context(|| format!("{}: failed to read worktree HEAD", repo.id))?,
    );
    Ok(update)
}

/// Look for this bundle's feature branch on `origin` before creating a fresh
/// local branch from base. Best-effort: a missing origin remote, an offline
/// fetch, or an absent remote branch all return `None` and materialization
/// falls back to branching from base. The targeted fetch keeps the
/// remote-tracking ref current even in workspaces that received the bundle
/// artifact without a prior `knit pull` (for example via `knit fetch`).
fn origin_feature_ref(repo_root: &Path, feature_branch: &str) -> Option<String> {
    git_output_optional(repo_root, ["remote", "get-url", "origin"])
        .ok()
        .flatten()?;
    let _ = git_output(repo_root, ["fetch", "origin", feature_branch]);
    let remote_ref = format!("origin/{feature_branch}");
    ref_exists(repo_root, &remote_ref).then_some(remote_ref)
}

fn materialize_in_place(
    repo_index: usize,
    repo: &RepoEntry,
    repo_root: &Path,
    feature_branch: &str,
) -> Result<MaterializeResult> {
    let base_ref = resolve_base_ref(repo_root, &repo.base_branch);
    let base_sha = rev_parse(repo_root, &base_ref)
        .with_context(|| format!("{}: failed to resolve base ref {base_ref}", repo.id))?;

    let current = current_branch(repo_root)?;
    if current.as_deref() != Some(feature_branch) {
        let short_status = git_output(repo_root, ["status", "--short"])?;
        if !short_status.trim().is_empty() {
            bail!(
                "{}: in-place checkout must be clean before switching branches.",
                repo.id
            );
        }

        if branch_exists(repo_root, feature_branch) {
            git_output(repo_root, ["checkout", feature_branch])
                .with_context(|| format!("{}: failed to checkout {feature_branch}", repo.id))?;
        } else if let Some(remote_ref) = origin_feature_ref(repo_root, feature_branch) {
            // A collaborator already pushed this bundle's branch: track it
            // instead of forking a same-named branch from base.
            git_output(
                repo_root,
                [
                    OsString::from("checkout"),
                    OsString::from("--track"),
                    OsString::from("-b"),
                    OsString::from(feature_branch),
                    OsString::from(&remote_ref),
                ],
            )
            .with_context(|| {
                format!(
                    "{}: failed to create {feature_branch} from {remote_ref}",
                    repo.id
                )
            })?;
        } else {
            git_output(
                repo_root,
                [
                    OsString::from("checkout"),
                    OsString::from("-b"),
                    OsString::from(feature_branch),
                    OsString::from(base_ref),
                ],
            )
            .with_context(|| format!("{}: failed to create {feature_branch}", repo.id))?;
        }
    }

    Ok(MaterializeResult {
        repo_index,
        repo_id: repo.id.clone(),
        base_sha: if repo.base_sha.is_none() {
            Some(base_sha)
        } else {
            None
        },
        feature_branch: Some(feature_branch.to_string()),
        worktree_path: Some(repo.path.clone()),
        head_sha: Some(
            rev_parse(repo_root, "HEAD")
                .with_context(|| format!("{}: failed to read in-place HEAD", repo.id))?,
        ),
        log: MaterializeLog::InPlace,
    })
}
