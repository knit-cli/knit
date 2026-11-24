use crate::checkout::checkout_dir;
use crate::git::{
    commit_details, commit_exists, current_branch, git_output, is_ancestor, merge_base, reflog,
    rev_list, rev_list_first_parent, rev_parse, ReflogEntry,
};
use crate::ids::{node_id, short_sha};
use crate::model::{BundleNode, ChangeGroup, Movement, NodeRewrite, RepoChange, RepoEntry};
use crate::rewrite::{record_rewrite, RepoRewrite, RewriteKind};
use crate::store::ActiveBundle;
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A checkout that has not caught up with the head the ledger records, as
/// opposed to one that moved away from it.
///
/// The ledger travels between machines on its own (`knit sync pull` refreshes
/// the artifact and leaves checkouts alone), so the recorded head can be a
/// commit this checkout has never been on. Nothing happened to the branch
/// here, and there is nothing to record: the checkout needs bringing forward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BehindCheckout {
    pub repo_id: String,
    pub head_sha: String,
    pub recorded_sha: String,
    /// Recorded commits this checkout lacks, counted like unrecorded ones.
    /// `None` when the recorded head is not in this repository yet, so the
    /// distance cannot be measured.
    pub missing: Option<usize>,
    /// Commits this checkout has that the recorded head does not: work made
    /// here on the older head, to replay once the checkout catches up. Zero
    /// for a checkout that is only behind.
    pub local: usize,
}

/// How each checkout that is not on its recorded head differs from it.
#[derive(Debug, Default)]
pub struct LedgerDrift {
    /// Checkouts that moved away from their recorded head: work to record.
    pub unrecorded: Vec<RepoChange>,
    /// Checkouts that have not caught up with their recorded head.
    pub behind: Vec<BehindCheckout>,
}

/// Movements of a checkout away from its recorded head: work to record.
/// A checkout that is merely behind the ledger is not one of them; see
/// [`detect_behind_checkouts`].
pub fn detect_unrecorded_changes(active: &ActiveBundle) -> Result<Vec<RepoChange>> {
    Ok(detect_ledger_drift(active)?.unrecorded)
}

/// Checkouts that sit behind their recorded head without having moved.
pub fn detect_behind_checkouts(active: &ActiveBundle) -> Result<Vec<BehindCheckout>> {
    Ok(detect_ledger_drift(active)?.behind)
}

/// Refuse to act on checkouts that are behind the ledger. For commands that
/// publish a checkout's head or trust it to be the recorded one: from a
/// checkout that is behind, either would put an older head in place of
/// commits made elsewhere.
pub fn ensure_checkouts_not_behind(
    active: &ActiveBundle,
    repo_ids: Option<&[String]>,
) -> Result<()> {
    let behind: Vec<String> = detect_behind_checkouts(active)?
        .iter()
        .filter(|behind| repo_ids.is_none_or(|ids| ids.contains(&behind.repo_id)))
        .map(|behind| format!("{}: {}", behind.repo_id, behind_note(behind)))
        .collect();
    if !behind.is_empty() {
        bail!(
            "checkout is behind the bundle ledger:\n{}",
            behind.join("\n")
        );
    }
    Ok(())
}

pub fn detect_ledger_drift(active: &ActiveBundle) -> Result<LedgerDrift> {
    let mut drift = LedgerDrift::default();

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

        if let Some(recorded) = before_sha.as_deref() {
            if let Some(behind) = behind_checkout(&worktree_dir, repo, recorded, &after_sha) {
                drift.behind.push(behind);
                continue;
            }
        }

        drift.unrecorded.push(build_repo_change(
            &worktree_dir,
            repo.id.clone(),
            before_sha,
            after_sha,
        )?);
    }

    Ok(drift)
}

/// `Some` when the checkout at `head` is behind `recorded` rather than moved
/// away from it.
///
/// A checkout that was reset has HEAD on an ancestor of the recorded head, and
/// so has one that never received the recorded commits. A checkout whose
/// commits were rewritten has a HEAD that neither contains the recorded head
/// nor is contained by it, and so has one that never received the recorded
/// commits and has commits of its own. In both pairs the first moved away from
/// the recorded head and the second was never on it.
///
/// What tells them apart is whether this checkout's branch was ever on the
/// recorded head, and its reflog answers that: every way a branch reaches a
/// commit here (a commit, a merge, a fast-forward, its creation) leaves an
/// entry, and so does every way it leaves one. See [`branch_log`].
///
/// The reflog is the only evidence that says what this checkout did. The
/// ledger cannot: it is shared, and it is replaced wholesale by an artifact
/// pull, so it holds no record of which heads one machine has been on.
///
/// A checkout is behind only while the published feature branch has commits
/// it lacks ([`lacks_published_commits`]). One that has everything published
/// and still lacks the recorded head is on a branch that was rewound or
/// rewritten where it is published, without the ledger hearing of it: the
/// checkout is right, and its position is recorded. Beyond that:
///
/// - HEAD on an ancestor of the recorded head is behind unless the log names
///   the recorded head. A reset writes an entry that does, so a log without
///   one, or no log at all, means no reset happened here that git remembers.
/// - A HEAD that diverges from the recorded head is behind only when the log
///   shows the branch coming to HEAD without the recorded head
///   ([`came_without_recorded`]). Without that proof it is recorded as a
///   divergence, as a rewrite is: a branch whose log was expired after a
///   rewrite is still a rewrite.
///
/// Both lean the way that is safe when the log has nothing to say. For an
/// ancestor that is recording nothing: a rewind recorded by mistake supersedes
/// commit groups another machine made. For a divergence it is recording what
/// git shows, which is what Knit did before it could tell the two apart.
fn behind_checkout(
    worktree_dir: &Path,
    repo: &RepoEntry,
    recorded: &str,
    head: &str,
) -> Option<BehindCheckout> {
    let behind = |missing, local| BehindCheckout {
        repo_id: repo.id.clone(),
        head_sha: head.to_string(),
        recorded_sha: recorded.to_string(),
        missing,
        local,
    };
    // Not fetched yet: the ledger arrived before the commits did. The checkout
    // cannot have moved away from a commit it does not have.
    if !commit_exists(worktree_dir, recorded) {
        return Some(behind(None, 0));
    }
    // On the recorded head's line and past it: unrecorded work.
    if is_ancestor(worktree_dir, recorded, head) {
        return None;
    }
    let log = branch_log(worktree_dir);
    if log
        .iter()
        .any(|entry| entry.new == recorded || entry.old.as_deref() == Some(recorded))
    {
        return None;
    }
    // Nothing published that the checkout lacks: there is nothing to catch up
    // with, so the branch lost the recorded head where it is published.
    if !lacks_published_commits(worktree_dir, repo, head) {
        return None;
    }
    // Counted the way a divergence is, from where the two lines part.
    let base = merge_base(worktree_dir, head, recorded).ok().flatten()?;
    if base != head && !came_without_recorded(&log, head, &base) {
        return None;
    }
    let missing = rev_list_first_parent(worktree_dir, &base, recorded).ok()?;
    let local = rev_list_first_parent(worktree_dir, &base, head).ok()?;
    Some(behind(Some(missing.len()), local.len()))
}

/// The reflog of the branch this checkout is on, oldest entry first.
///
/// The branch's own reflog, not HEAD's: HEAD also remembers every commit the
/// checkout merely visited, such as a detached look at the fetched head or a
/// spell on another branch, and neither put the commits on this branch. Every
/// move of a checked-out branch is written to both logs, so the branch's log
/// misses nothing that happened to it. A detached checkout has only HEAD's.
fn branch_log(worktree_dir: &Path) -> Vec<ReflogEntry> {
    match current_branch(worktree_dir).ok().flatten() {
        Some(branch) => reflog(worktree_dir, &format!("refs/heads/{branch}")),
        None => reflog(worktree_dir, "HEAD"),
    }
}

/// Whether `log` shows the branch coming to `head` from a point where it had
/// not reached the recorded head: an unbroken run of entries that ends on
/// `head` and starts either where the branch was created or on `base`, the
/// commit where `head` and the recorded head part.
///
/// The caller has checked that no entry names the recorded head. That alone
/// proves little, because entries expire: a log emptied after a rewrite names
/// nothing. An unbroken run does prove it. Each entry in it starts on the
/// value the one before it ended on, so nothing between them is missing, and
/// the branch was not on the recorded head anywhere along it.
fn came_without_recorded(log: &[ReflogEntry], head: &str, base: &str) -> bool {
    let mut at = head;
    for entry in log.iter().rev() {
        if entry.new != at {
            return false;
        }
        if entry.created {
            return true;
        }
        match entry.old.as_deref() {
            Some(old) if old == base => return true,
            Some(old) => at = old,
            None => return false,
        }
    }
    false
}

/// Whether the feature branch, as last fetched from where it is published, has
/// commits `head` lacks. The ref is the one `knit pull` fast-forwards the
/// checkout to; a branch that was never published or fetched has none.
fn lacks_published_commits(worktree_dir: &Path, repo: &RepoEntry, head: &str) -> bool {
    let Some(branch) = repo
        .feature_branch
        .clone()
        .or_else(|| current_branch(worktree_dir).ok().flatten())
    else {
        return false;
    };
    let reference = if crate::contribution::configured(repo) {
        crate::contribution::role_ref(repo, &branch, true).ok()
    } else {
        Some(format!("refs/remotes/origin/{branch}"))
    };
    reference
        .and_then(|reference| {
            crate::git::ref_commit_sha(worktree_dir, &reference)
                .ok()
                .flatten()
        })
        .is_some_and(|published| !is_ancestor(worktree_dir, &published, head))
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

/// Whether the ledger has ever named `sha` for this repo: as a recorded commit,
/// as either end of a movement, or as a commit a rewrite replaced.
pub fn ledger_mentions_commit(bundle: &ChangeGroup, repo_id: &str, sha: &str) -> bool {
    let grouped = |groups: &[crate::model::CommitGroup]| {
        groups
            .iter()
            .flat_map(|group| &group.commits)
            .any(|commit| commit.repo_id == repo_id && commit.sha == sha)
    };
    grouped(&bundle.commit_groups)
        || bundle.nodes.iter().any(|node| {
            node.commits
                .iter()
                .any(|commit| commit.repo_id == repo_id && commit.sha == sha)
                || node
                    .rewrite
                    .as_ref()
                    .is_some_and(|rewrite| grouped(&rewrite.superseded_groups))
                || node
                    .repo_changes
                    .iter()
                    .filter(|change| change.repo_id == repo_id)
                    .any(|change| {
                        change.after_sha == sha
                            || change.before_sha.as_deref() == Some(sha)
                            || change.commits.iter().any(|commit| commit == sha)
                            || change.dropped_commits.iter().any(|commit| commit == sha)
                    })
        })
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

/// What to tell the user about a checkout that is behind its recorded head.
///
/// A checkout with commits of its own cannot be fast-forwarded, so the command
/// named is the one that replays them on the recorded head.
pub fn behind_note(behind: &BehindCheckout) -> String {
    match (behind.missing, behind.local) {
        (Some(count), 0) => format!("behind recorded head by {count}; run `knit pull`"),
        (Some(count), local) => format!(
            "behind recorded head by {count}, with {local} commit(s) not on it; run `knit pull --feature --rebase`"
        ),
        (None, _) => format!(
            "recorded head {} is not in this checkout; run `knit pull`",
            short_sha(&behind.recorded_sha)
        ),
    }
}

pub fn sync_observed_changes(active: &mut ActiveBundle) -> Result<Vec<RepoChange>> {
    sync_observed_changes_for_repo_ids(active, None)
}

/// Refuses while a rebase owns the ledger or a checkout sits on an
/// intermediate rebase HEAD. Callers that only record opportunistically check
/// this first and leave the checkouts alone instead of failing.
pub fn ensure_no_pending_rebase(active: &ActiveBundle) -> Result<()> {
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
    Ok(())
}

pub fn sync_observed_changes_for_repo_ids(
    active: &mut ActiveBundle,
    repo_ids: Option<&[String]>,
) -> Result<Vec<RepoChange>> {
    ensure_no_pending_rebase(active)?;
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
        // merge base identifies a manual rebase that can recreate groups —
        // or a rewrite that replaced every commit with a content-identical
        // copy on the same base, which is what re-signing or re-dating the
        // bundle's commits outside Knit looks like.
        if change.movement == Movement::Diverged
            && new_base == old_base
            // A pruned old head can't be compared, so it stays ordinary
            // observed work rather than failing the whole reconcile.
            && !content_identical_rewrite(&path, &old_base, &old_head, &change.after_sha)
                .unwrap_or(false)
        {
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
    let new_nodes = &active.bundle.nodes[first_new_node..];
    let mut rewritten_changes: Vec<RepoChange> = new_nodes
        .iter()
        .filter(|node| node.node_type == "git.observed")
        .flat_map(|node| node.repo_changes.iter().cloned())
        .collect();
    // The observation node leaves out commits that went straight into a
    // successor group. The report still counts them: they are new commits on
    // the branch, and leaving them out read "+0 -2" for a re-dated history.
    for change in &mut rewritten_changes {
        for group_change in new_nodes
            .iter()
            .filter(|node| node.node_type == "commit.group")
            .flat_map(|node| &node.repo_changes)
            .filter(|c| c.repo_id == change.repo_id)
        {
            for sha in &group_change.commits {
                if !change.commits.contains(sha) {
                    change.commits.push(sha.clone());
                }
            }
            for (sha, detail) in &group_change.commit_details {
                change
                    .commit_details
                    .entry(sha.clone())
                    .or_insert_with(|| detail.clone());
            }
        }
        if let Some(rewrite) = rewrites.iter().find(|r| r.repo_id == change.repo_id) {
            let repo = active
                .bundle
                .repos
                .iter()
                .find(|r| r.id == change.repo_id)
                .unwrap();
            if let Some(path) = repo_worktree_dir(active, repo) {
                if let Ok(order) = rev_list(&path, &rewrite.new_base, &rewrite.new_head) {
                    change
                        .commits
                        .sort_by_key(|sha| order.iter().position(|o| o == sha));
                }
            }
        }
    }
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

/// Whether `base..new_head` is the same work as `base..old_head` with new
/// commit identities: the replaced commits map one to one, in order, with
/// identical trees, subjects and authors.
///
/// That is the shape of a rewrite that only touched commit *headers* — a
/// `git rebase --gpg-sign` to satisfy a signing rule, a committer-date
/// rewrite — and it is the one divergence where the bundle's existing commit
/// groups still describe exactly the commits in front of us. Anything else
/// (a changed tree, a reworded message, a different count, a reorder) stays
/// anonymous observed work, because Knit cannot claim the named group is
/// still the same change.
fn content_identical_rewrite(
    path: &Path,
    base: &str,
    old_head: &str,
    new_head: &str,
) -> Result<bool> {
    let old = rev_list(path, base, old_head)?;
    let new = rev_list(path, base, new_head)?;
    // Commits both sides share are untouched; only the replacements matter.
    let replaced: Vec<&String> = old.iter().filter(|sha| !new.contains(sha)).collect();
    let created: Vec<&String> = new.iter().filter(|sha| !old.contains(sha)).collect();
    if replaced.is_empty() || replaced.len() != created.len() {
        return Ok(false);
    }
    for (before, after) in replaced.into_iter().zip(created) {
        if commit_identity(path, before)? != commit_identity(path, after)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// A commit's content and authorship, with nothing that a header-only rewrite
/// is allowed to change: tree, author name, author email and full message.
fn commit_identity(path: &Path, sha: &str) -> Result<String> {
    git_output(
        path,
        [
            "show",
            "-s",
            "--format=%T%n%an%n%ae%n%B",
            &format!("{sha}^{{commit}}"),
        ],
    )
}

/// Describe how a checkout moved from the recorded head to `after_sha`.
///
/// Commits are listed along the first-parent line, so the lists hold the work
/// done on this branch plus any merge commits. What a merge brings in from the
/// base branch is base history, not bundle work, and is never listed: a branch
/// that merged a busy base would otherwise count hundreds of commits as its
/// own. See [`rev_list_first_parent`].
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
                commits: rev_list_first_parent(worktree_dir, &before, &after_sha)
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
                dropped_commits: rev_list_first_parent(worktree_dir, &after_sha, &before)
                    .context("failed to list dropped commits")?,
                commit_details: BTreeMap::new(),
            },
        ));
    }

    let base = merge_base(worktree_dir, &before, &after_sha)?;
    let (commits, dropped_commits) = if let Some(base) = base {
        (
            rev_list_first_parent(worktree_dir, &base, &after_sha)
                .context("failed to list divergent commits")?,
            rev_list_first_parent(worktree_dir, &base, &before)
                .context("failed to list replaced commits")?,
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
