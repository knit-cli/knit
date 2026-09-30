use crate::checkout::checkout_dir;
use crate::git::{commit_details, git_output, is_ancestor, merge_base, rev_list, rev_parse};
use crate::ids::node_id;
use crate::model::{BundleNode, ChangeGroup, Movement, NodeRewrite, RepoChange, RepoEntry};
use crate::rewrite::{record_rewrite, RepoRewrite, RewriteKind};
use crate::store::ActiveBundle;
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub fn detect_unrecorded_changes(active: &ActiveBundle) -> Result<Vec<RepoChange>> {
    let mut changes = Vec::new();

    for repo in &active.bundle.repos {
        let Some(worktree_dir) = repo_worktree_dir(active, repo) else {
            continue;
        };
        if rebase_in_progress(&worktree_dir)? {
            continue;
        }
        let after_sha = rev_parse(&worktree_dir, "HEAD")
            .with_context(|| format!("{}: failed to read worktree HEAD", repo.id))?;
        let before_sha = latest_recorded_head_sha(&active.bundle, repo);

        if before_sha.as_deref() == Some(after_sha.as_str()) {
            continue;
        }

        changes.push(build_repo_change(
            &worktree_dir,
            repo.id.clone(),
            before_sha,
            after_sha,
        )?);
    }

    Ok(changes)
}

pub fn latest_recorded_head_sha(bundle: &ChangeGroup, repo: &RepoEntry) -> Option<String> {
    ledger_recorded_head_sha(bundle, repo)
        .or_else(|| repo.head_sha.clone())
        .or_else(|| repo.base_sha.clone())
}

pub fn ledger_recorded_head_sha(bundle: &ChangeGroup, repo: &RepoEntry) -> Option<String> {
    let mut head = None;

    for node in &bundle.nodes {
        for commit in &node.commits {
            if commit.repo_id == repo.id {
                head = Some(commit.sha.clone());
            }
        }
        if node.commits.is_empty() {
            if let Some(group_id) = &node.commit_group_id {
                if let Some(group) = bundle
                    .commit_groups
                    .iter()
                    .find(|group| &group.id == group_id)
                {
                    for commit in &group.commits {
                        if commit.repo_id == repo.id {
                            head = Some(commit.sha.clone());
                        }
                    }
                }
            }
        }
        for change in &node.repo_changes {
            if change.repo_id == repo.id {
                head = Some(change.after_sha.clone());
            }
        }
    }

    if bundle.nodes.is_empty() {
        for group in &bundle.commit_groups {
            for commit in &group.commits {
                if commit.repo_id == repo.id {
                    head = Some(commit.sha.clone());
                }
            }
        }
    }

    head
}

pub fn status_note(change: &RepoChange) -> String {
    match change.movement {
        Movement::Advanced => format!("unrecorded commits: {}", change.commits.len()),
        Movement::Rewound => format!("rewound commits: {}", change.dropped_commits.len()),
        Movement::Diverged => format!(
            "diverged (+{} -{})",
            change.commits.len(),
            change.dropped_commits.len()
        ),
    }
}

pub fn sync_note(change: &RepoChange) -> String {
    match change.movement {
        Movement::Advanced => format!("observed {} unrecorded commit(s)", change.commits.len()),
        Movement::Rewound => format!(
            "observed rewind removing {} commit(s)",
            change.dropped_commits.len()
        ),
        Movement::Diverged => format!(
            "observed divergence (+{} -{} commit(s))",
            change.commits.len(),
            change.dropped_commits.len()
        ),
    }
}

pub fn sync_observed_changes(active: &mut ActiveBundle) -> Result<Vec<RepoChange>> {
    sync_observed_changes_for_repo_ids(active, None)
}

pub fn sync_observed_changes_for_repo_ids(
    active: &mut ActiveBundle,
    repo_ids: Option<&[String]>,
) -> Result<Vec<RepoChange>> {
    // A multi-repository command can have completed some Git rebases while
    // others are still pending. Its recovery record owns the entire ledger.
    if active
        .root
        .join(".knit/rebase")
        .join(format!("{}.json", active.bundle.id))
        .exists()
    {
        bail!("A bundle rebase is pending; run knit rebase --continue or knit rebase --abort before recording more work");
    }
    for repo in &active.bundle.repos {
        if let Some(path) = repo_worktree_dir(active, repo) {
            if rebase_in_progress(&path)? {
                bail!("{}: a Git rebase is pending; run git rebase --continue or git rebase --abort before recording more work", repo.id);
            }
        }
    }
    let changes = detect_unrecorded_changes(active)?;
    let changes = match repo_ids {
        Some(repo_ids) => changes
            .into_iter()
            .filter(|change| repo_ids.iter().any(|repo_id| repo_id == &change.repo_id))
            .collect::<Vec<_>>(),
        None => changes,
    };
    if changes.is_empty() {
        return Ok(changes);
    }

    let mut rewrites = Vec::new();
    for change in &changes {
        if change.movement == Movement::Advanced {
            continue;
        }
        let repo = active
            .bundle
            .repos
            .iter()
            .find(|r| r.id == change.repo_id)
            .unwrap();
        let path = repo_worktree_dir(active, repo).context("missing changed checkout")?;
        let old_head = change
            .before_sha
            .clone()
            .context("rewrite has no previous head")?;
        let old_base = repo
            .base_sha
            .clone()
            .or(merge_base(&path, &old_head, &change.after_sha)?)
            .context("rewrite has no recorded or common base")?;
        let new_base = if change.movement == Movement::Diverged {
            observed_base(&path, repo, &old_base, &change.after_sha)?
        } else {
            old_base.clone()
        };
        // Ordinary divergence (amends, resets and unrelated replacement work)
        // keeps the existing observation semantics. Only an advanced upstream
        // merge base identifies a manual rebase that can recreate groups.
        if change.movement == Movement::Diverged && new_base == old_base {
            continue;
        }
        rewrites.push(RepoRewrite {
            repo_id: repo.id.clone(),
            old_head,
            new_head: change.after_sha.clone(),
            old_base,
            new_base,
        });
    }
    let first_new_node = active.bundle.nodes.len();
    record_rewrite(active, &rewrites, RewriteKind::Observed, None)?;
    let rewritten_changes: Vec<_> = active.bundle.nodes[first_new_node..]
        .iter()
        .filter(|node| node.node_type == "git.observed")
        .flat_map(|node| node.repo_changes.iter().cloned())
        .collect();
    let ordinary: Vec<_> = changes
        .iter()
        .filter(|c| !rewrites.iter().any(|r| r.repo_id == c.repo_id))
        .cloned()
        .collect();
    if !ordinary.is_empty() {
        for change in &ordinary {
            if let Some(repo) = active
                .bundle
                .repos
                .iter_mut()
                .find(|r| r.id == change.repo_id)
            {
                repo.head_sha = Some(change.after_sha.clone());
            }
        }
        // Preserve ordinary observation additions/drops, but active groups
        // cannot keep members made unreachable by an amend or replacement.
        let mut unreachable = std::collections::BTreeSet::new();
        for change in &ordinary {
            if change.movement != Movement::Diverged {
                continue;
            }
            let repo = active
                .bundle
                .repos
                .iter()
                .find(|r| r.id == change.repo_id)
                .unwrap();
            let path = repo_worktree_dir(active, repo).context("missing changed checkout")?;
            for commit in active.bundle.commit_groups.iter().flat_map(|g| &g.commits) {
                if commit.repo_id == change.repo_id
                    && !is_ancestor(&path, &commit.sha, &change.after_sha)
                {
                    unreachable.insert((commit.repo_id.clone(), commit.sha.clone()));
                }
            }
        }
        let mut superseded_groups = Vec::new();
        for group in &mut active.bundle.commit_groups {
            if group
                .commits
                .iter()
                .any(|c| unreachable.contains(&(c.repo_id.clone(), c.sha.clone())))
            {
                superseded_groups.push(group.clone());
                group
                    .commits
                    .retain(|c| !unreachable.contains(&(c.repo_id.clone(), c.sha.clone())));
            }
        }
        active
            .bundle
            .commit_groups
            .retain(|g| !g.commits.is_empty());
        let mut node = BundleNode::git_observed(node_id("git"), now_iso(), ordinary);
        if !superseded_groups.is_empty() {
            node.rewrite = Some(NodeRewrite {
                kind: "observed".into(),
                superseded_groups,
            });
        }
        active.bundle.nodes.push(node);
        active.bundle.head_node_id = active.bundle.nodes.last().map(|node| node.id.clone());
        active.bundle.updated_at = now_iso();
    }
    let changes = changes
        .into_iter()
        .map(|change| {
            rewritten_changes
                .iter()
                .find(|c| c.repo_id == change.repo_id)
                .cloned()
                .unwrap_or(change)
        })
        .collect();

    Ok(changes)
}

pub fn repo_worktree_dir(active: &ActiveBundle, repo: &RepoEntry) -> Option<PathBuf> {
    checkout_dir(active, repo)
}

fn build_repo_change(
    worktree_dir: &Path,
    repo_id: String,
    before_sha: Option<String>,
    after_sha: String,
) -> Result<RepoChange> {
    let Some(before) = before_sha.clone() else {
        return Ok(described(
            worktree_dir,
            RepoChange {
                repo_id,
                movement: Movement::Advanced,
                base_before_sha: None,
                base_after_sha: None,
                before_sha,
                after_sha: after_sha.clone(),
                commits: vec![after_sha],
                dropped_commits: Vec::new(),
                commit_details: BTreeMap::new(),
            },
        ));
    };

    if is_ancestor(worktree_dir, &before, &after_sha) {
        return Ok(described(
            worktree_dir,
            RepoChange {
                repo_id,
                movement: Movement::Advanced,
                base_before_sha: None,
                base_after_sha: None,
                before_sha,
                after_sha: after_sha.clone(),
                commits: rev_list(worktree_dir, &before, &after_sha)
                    .context("failed to list advanced commits")?,
                dropped_commits: Vec::new(),
                commit_details: BTreeMap::new(),
            },
        ));
    }

    if is_ancestor(worktree_dir, &after_sha, &before) {
        return Ok(described(
            worktree_dir,
            RepoChange {
                repo_id,
                movement: Movement::Rewound,
                base_before_sha: None,
                base_after_sha: None,
                before_sha,
                after_sha: after_sha.clone(),
                commits: Vec::new(),
                dropped_commits: rev_list(worktree_dir, &after_sha, &before)
                    .context("failed to list dropped commits")?,
                commit_details: BTreeMap::new(),
            },
        ));
    }

    let base = merge_base(worktree_dir, &before, &after_sha)?;
    let (commits, dropped_commits) = if let Some(base) = base {
        (
            rev_list(worktree_dir, &base, &after_sha)
                .context("failed to list divergent commits")?,
            rev_list(worktree_dir, &base, &before).context("failed to list replaced commits")?,
        )
    } else {
        (vec![after_sha.clone()], vec![before])
    };

    Ok(described(
        worktree_dir,
        RepoChange {
            repo_id,
            movement: Movement::Diverged,
            base_before_sha: None,
            base_after_sha: None,
            before_sha,
            after_sha,
            commits,
            dropped_commits,
            commit_details: BTreeMap::new(),
        },
    ))
}

/// Capture each observed commit's subject and author date while the checkout is
/// still in hand. Without this the ledger only has SHAs, and later readers can
/// neither name the commit nor say when it was really written.
fn described(worktree_dir: &Path, mut change: RepoChange) -> RepoChange {
    let shas = change
        .commits
        .iter()
        .chain(change.dropped_commits.iter())
        .cloned()
        .collect::<Vec<_>>();
    change.commit_details = commit_details(worktree_dir, &shas);
    change
}

/// A detached, intermediate rebase HEAD must never become durable ledger state.
fn rebase_in_progress(path: &Path) -> Result<bool> {
    for name in ["rebase-merge", "rebase-apply"] {
        let state = git_output(path, ["rev-parse", "--git-path", name])?;
        let state = PathBuf::from(state.trim());
        if (if state.is_absolute() {
            state
        } else {
            path.join(state)
        })
        .exists()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn observed_base(path: &Path, repo: &RepoEntry, recorded: &str, head: &str) -> Result<String> {
    let mut refs = vec![
        format!("refs/remotes/origin/{}", repo.base_branch),
        format!("refs/heads/{}", repo.base_branch),
    ];
    if crate::contribution::configured(repo) {
        let destination = crate::contribution::role_ref(repo, &repo.base_branch, false)?;
        // A cached destination is authoritative even when it has not advanced.
        // Origin/local branches may contain feature work, not upstream work.
        if crate::git::ref_commit_sha(path, &destination)?.is_some() {
            refs = vec![destination];
        }
    }
    let mut newest = recorded.to_owned();
    for reference in refs {
        let Some(target) = crate::git::ref_commit_sha(path, &reference)? else {
            continue;
        };
        let Some(candidate) = merge_base(path, head, &target)? else {
            continue;
        };
        if candidate != recorded
            && is_ancestor(path, recorded, &candidate)
            && is_ancestor(path, &newest, &candidate)
        {
            newest = candidate;
        }
    }
    Ok(newest)
}
