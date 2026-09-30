//! In-memory ledger projection for explicit and externally observed Git rewrites.
use crate::checkout::checkout_dir;
use crate::git::{commit_author, commit_details, git_output, is_ancestor, rev_list};
use crate::ids::{commit_group_id, node_id};
use crate::model::{BundleNode, CommitGroup, CommitRef, Movement, NodeRewrite, RepoChange};
use crate::store::ActiveBundle;
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewriteKind {
    Squash,
    Rebase,
    Observed,
}

#[derive(Debug, Clone)]
pub struct RepoRewrite {
    pub repo_id: String,
    pub old_head: String,
    pub new_head: String,
    pub old_base: String,
    pub new_base: String,
}

/// Record a completed rewrite without persisting the bundle or changing Git.
/// All fallible inspection precedes mutation, so errors leave `active` intact.
pub fn record_rewrite(
    active: &mut ActiveBundle,
    rewrites: &[RepoRewrite],
    kind: RewriteKind,
    message: Option<&str>,
) -> Result<Vec<String>> {
    if rewrites.is_empty() {
        return Ok(Vec::new());
    }
    let now = now_iso();
    let mut paths = BTreeMap::new();
    let mut changes = Vec::new();
    let mut mapping = BTreeMap::new();
    let mut replaced = BTreeSet::new();
    for rewrite in rewrites {
        if paths.contains_key(&rewrite.repo_id) {
            bail!("duplicate rewrite repository: {}", rewrite.repo_id);
        }
        let repo = active
            .bundle
            .repos
            .iter()
            .find(|r| r.id == rewrite.repo_id)
            .with_context(|| format!("unknown rewrite repository: {}", rewrite.repo_id))?;
        let path =
            checkout_dir(active, repo).with_context(|| format!("{}: missing checkout", repo.id))?;
        let mut dropped = rev_list(&path, &rewrite.old_base, &rewrite.old_head)?;
        let mut new = rev_list(&path, &rewrite.new_base, &rewrite.new_head)?;
        if kind == RewriteKind::Squash && new.len() > 1 {
            bail!(
                "{}: squash must produce at most one feature commit",
                repo.id
            );
        }
        if kind == RewriteKind::Observed {
            // Reconciliation records actual movement: shared SHAs and their
            // groups remain current, rather than being dropped and re-added.
            // Hosted replay applies drops after additions within a node.
            let shared: BTreeSet<_> = dropped
                .iter()
                .filter(|sha| new.contains(sha))
                .cloned()
                .collect();
            dropped.retain(|sha| !shared.contains(sha));
            new.retain(|sha| !shared.contains(sha));
        }
        for sha in &dropped {
            replaced.insert((repo.id.clone(), sha.clone()));
        }
        if kind != RewriteKind::Squash {
            for (old, new) in map_commits(&path, &dropped, &new, kind)? {
                mapping.insert((repo.id.clone(), old), new);
            }
        }
        let details = commit_details(
            &path,
            &dropped.iter().chain(&new).cloned().collect::<Vec<_>>(),
        );
        changes.push(RepoChange {
            repo_id: repo.id.clone(),
            movement: if is_ancestor(&path, &rewrite.new_head, &rewrite.old_head) {
                Movement::Rewound
            } else {
                Movement::Diverged
            },
            before_sha: Some(rewrite.old_head.clone()),
            after_sha: rewrite.new_head.clone(),
            base_before_sha: (rewrite.old_base != rewrite.new_base)
                .then(|| rewrite.old_base.clone()),
            base_after_sha: (rewrite.old_base != rewrite.new_base)
                .then(|| rewrite.new_base.clone()),
            commits: new,
            dropped_commits: dropped,
            commit_details: details,
        });
        paths.insert(repo.id.clone(), path);
    }

    let mut retained = Vec::new();
    let mut superseded = Vec::new();
    let mut created = Vec::new();
    for group in &active.bundle.commit_groups {
        let (affected, untouched): (Vec<_>, Vec<_>) = group
            .commits
            .iter()
            .cloned()
            .partition(|c| replaced.contains(&(c.repo_id.clone(), c.sha.clone())));
        if affected.is_empty() {
            retained.push(group.clone());
            continue;
        }
        superseded.push(group.clone());
        if !untouched.is_empty() {
            let mut remainder = group.clone();
            remainder.commits = untouched;
            retained.push(remainder);
        }
        if kind != RewriteKind::Squash {
            let mapped: Option<Vec<_>> = affected
                .iter()
                .map(|c| {
                    mapping
                        .get(&(c.repo_id.clone(), c.sha.clone()))
                        .map(|sha| CommitRef {
                            repo_id: c.repo_id.clone(),
                            sha: sha.clone(),
                        })
                })
                .collect();
            if let Some(commits) = mapped {
                created.push(CommitGroup {
                    id: commit_group_id(),
                    message: group.message.clone(),
                    created_at: now.clone(),
                    commits,
                    author: group.author.clone(),
                });
            }
        }
    }
    if kind == RewriteKind::Squash {
        let text = message.map(str::to_owned).unwrap_or_else(|| {
            if superseded.len() == 1 {
                superseded[0].message.clone()
            } else {
                active.bundle.title.clone()
            }
        });
        let commits: Vec<_> = changes
            .iter()
            .filter(|change| !change.commits.is_empty())
            .map(|change| CommitRef {
                repo_id: change.repo_id.clone(),
                sha: change.after_sha.clone(),
            })
            .collect();
        // A rebase can absorb a squash entirely into upstream. Its new base
        // still belongs in repo state, but is never authored bundle work.
        if let Some(first) = commits.first() {
            created.push(CommitGroup {
                id: commit_group_id(),
                message: text,
                created_at: now.clone(),
                author: commit_author(&paths[&first.repo_id], &first.sha).ok(),
                commits,
            });
        }
    }
    let assigned: BTreeSet<_> = created
        .iter()
        .flat_map(|g| &g.commits)
        .map(|c| (c.repo_id.clone(), c.sha.clone()))
        .collect();
    if kind == RewriteKind::Rebase {
        let commits: Vec<_> = changes
            .iter()
            .flat_map(|c| {
                c.commits.iter().map(|sha| CommitRef {
                    repo_id: c.repo_id.clone(),
                    sha: sha.clone(),
                })
            })
            .filter(|c| !assigned.contains(&(c.repo_id.clone(), c.sha.clone())))
            .collect();
        if !commits.is_empty() {
            created.push(CommitGroup {
                id: commit_group_id(),
                message: active.bundle.title.clone(),
                created_at: now.clone(),
                author: commit_author(&paths[&commits[0].repo_id], &commits[0].sha).ok(),
                commits,
            });
        }
    }
    let assigned: BTreeSet<_> = created
        .iter()
        .flat_map(|g| &g.commits)
        .map(|c| (c.repo_id.clone(), c.sha.clone()))
        .collect();
    let mut observed_changes = changes.clone();
    for change in &mut observed_changes {
        change
            .commits
            .retain(|sha| !assigned.contains(&(change.repo_id.clone(), sha.clone())));
    }
    let mut observation = BundleNode::git_observed(node_id("git"), now.clone(), observed_changes);
    observation.rewrite = Some(NodeRewrite {
        kind: match kind {
            RewriteKind::Squash => "squash",
            RewriteKind::Rebase => "rebase",
            RewriteKind::Observed => "observed",
        }
        .into(),
        superseded_groups: superseded,
    });
    active.bundle.nodes.push(observation);
    let mut ids = Vec::new();
    for group in created {
        let group_changes = changes
            .iter()
            .filter_map(|change| {
                let commits: Vec<_> = group
                    .commits
                    .iter()
                    .filter(|c| c.repo_id == change.repo_id)
                    .map(|c| c.sha.clone())
                    .collect();
                if commits.is_empty() {
                    return None;
                }
                Some(RepoChange {
                    repo_id: change.repo_id.clone(),
                    movement: Movement::Advanced,
                    before_sha: change.before_sha.clone(),
                    after_sha: change.after_sha.clone(),
                    base_before_sha: None,
                    base_after_sha: None,
                    commit_details: commit_details(&paths[&change.repo_id], &commits),
                    commits,
                    dropped_commits: Vec::new(),
                })
            })
            .collect();
        active.bundle.nodes.push(BundleNode::commit_group(
            group.id.clone(),
            now.clone(),
            group.message.clone(),
            group.commits.clone(),
            group_changes,
        ));
        ids.push(group.id.clone());
        retained.push(group);
    }
    active.bundle.commit_groups = retained;
    for rewrite in rewrites {
        let repo = active
            .bundle
            .repos
            .iter_mut()
            .find(|r| r.id == rewrite.repo_id)
            .unwrap();
        repo.head_sha = Some(rewrite.new_head.clone());
        repo.base_sha = Some(rewrite.new_base.clone());
    }
    active.bundle.head_node_id = active.bundle.nodes.last().map(|n| n.id.clone());
    active.bundle.updated_at = now;
    Ok(ids)
}

fn map_commits(
    path: &Path,
    old: &[String],
    new: &[String],
    kind: RewriteKind,
) -> Result<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    let mut used = BTreeSet::new();
    // Preserve exact identity first, including commits retained by a rewind.
    for sha in old.iter().filter(|sha| new.contains(sha)) {
        result.insert(sha.clone(), sha.clone());
        used.insert(sha.clone());
    }
    let mut patches: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for sha in new.iter().filter(|sha| !used.contains(*sha)) {
        if let Some(patch) = patch_id(path, sha)? {
            patches.entry(patch).or_default().push(sha.clone());
        }
    }
    for sha in old.iter().filter(|sha| !new.contains(sha)) {
        if let Some(patch) = patch_id(path, sha)? {
            if let Some(candidates) = patches.get_mut(&patch) {
                if !candidates.is_empty() {
                    let target = candidates.remove(0);
                    used.insert(target.clone());
                    result.insert(sha.clone(), target);
                }
            }
        }
    }
    if kind == RewriteKind::Rebase && old.len() == new.len() {
        let unmatched_old: Vec<_> = old
            .iter()
            .filter(|sha| !result.contains_key(*sha))
            .collect();
        let unmatched_new = new.iter().filter(|sha| !used.contains(*sha));
        for (old, new) in unmatched_old.into_iter().zip(unmatched_new) {
            result.insert(old.clone(), new.clone());
        }
    }
    Ok(result)
}

fn patch_id(path: &Path, sha: &str) -> Result<Option<String>> {
    let patch = git_output(
        path,
        [
            "show",
            "--pretty=format:",
            "--no-color",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--no-ext-diff",
            "--no-textconv",
            "--binary",
            sha,
        ],
    )?;
    let mut child = Command::new("git")
        .current_dir(path)
        .args(["patch-id", "--stable"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .context("patch-id stdin unavailable")?
        .write_all(patch.as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "git patch-id failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8(output.stdout)?
        .split_whitespace()
        .next()
        .map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::rev_parse;
    use crate::history::bundle_history_snapshot;
    use crate::model::{ChangeGroup, CommitAuthor, RepoEntry};
    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn path(&self) -> &Path {
            &self.0
        }
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "knit-rewrite-{}-{}",
                std::process::id(),
                node_id("test")
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn git(path: &Path, args: &[&str]) -> String {
        let home = path.join(".test-home");
        std::fs::create_dir_all(&home).unwrap();
        let output = Command::new("git")
            .current_dir(path)
            .args(args)
            .env("HOME", home)
            .env_remove("GIT_AUTHOR_NAME")
            .env_remove("GIT_AUTHOR_EMAIL")
            .env_remove("GIT_COMMITTER_NAME")
            .env_remove("GIT_COMMITTER_EMAIL")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .trim_end()
            .to_owned()
    }
    fn commit(path: &Path, file: &str, text: &str) -> String {
        std::fs::write(path.join(file), text).unwrap();
        git(path, &["add", file]);
        git(path, &["commit", "-m", text]);
        rev_parse(path, "HEAD").unwrap()
    }
    fn fixture() -> (TempDir, ActiveBundle, String, String, String) {
        let dir = TempDir::new();
        git(dir.path(), &["init", "-b", "main"]);
        git(dir.path(), &["config", "user.name", "Test Author"]);
        git(dir.path(), &["config", "user.email", "author@example.test"]);
        git(dir.path(), &["config", "commit.gpgsign", "false"]);
        let base = commit(dir.path(), "base", "base");
        git(dir.path(), &["checkout", "-b", "feature"]);
        let first = commit(dir.path(), "first", "first");
        let second = commit(dir.path(), "second", "second");
        let mut bundle = ChangeGroup::new(
            "rewrite-test".into(),
            "Rewrite test".into(),
            "2026-01-01T00:00:00Z".into(),
        );
        let repo: RepoEntry = serde_json::from_value(serde_json::json!({
            "id":"repo", "path":dir.path(), "remote":null, "baseBranch":"main", "baseSha":base,
            "featureBranch":"feature", "worktreePath":dir.path(), "headSha":second
        }))
        .unwrap();
        bundle.repos.push(repo);
        for (id, sha) in [("first-group", &first), ("second-group", &second)] {
            let group = CommitGroup {
                id: id.into(),
                message: id.into(),
                created_at: "2026-01-01T00:00:00Z".into(),
                author: Some(CommitAuthor {
                    name: "Test Author".into(),
                    email: "author@example.test".into(),
                }),
                commits: vec![CommitRef {
                    repo_id: "repo".into(),
                    sha: sha.clone(),
                }],
            };
            bundle.nodes.push(BundleNode::commit_group(
                group.id.clone(),
                group.created_at.clone(),
                group.message.clone(),
                group.commits.clone(),
                vec![],
            ));
            bundle.commit_groups.push(group);
        }
        let active =
            ActiveBundle::unlocked(dir.path().into(), dir.path().join("bundle.json"), bundle);
        (dir, active, base, first, second)
    }

    /// Hosted replay adds all three representations and removes only explicit
    /// observed drops. Inline commits keep retired group nodes replayable.
    fn replay(bundle: &ChangeGroup) -> BTreeSet<(String, String)> {
        let mut result = BTreeSet::new();
        for node in &bundle.nodes {
            if matches!(
                node.node_type.as_str(),
                "tag.created" | "check.recorded" | "branch.landed"
            ) {
                continue;
            }
            let commits = node
                .commit_group_id
                .as_ref()
                .and_then(|id| bundle.commit_groups.iter().find(|g| &g.id == id))
                .map(|g| g.commits.as_slice())
                .unwrap_or(&[]);
            for c in commits.iter().chain(&node.commits) {
                result.insert((c.repo_id.clone(), c.sha.clone()));
            }
            for change in &node.repo_changes {
                for sha in &change.commits {
                    result.insert((change.repo_id.clone(), sha.clone()));
                }
                if node.node_type == "git.observed"
                    && matches!(change.movement, Movement::Diverged | Movement::Rewound)
                {
                    for sha in &change.dropped_commits {
                        result.remove(&(change.repo_id.clone(), sha.clone()));
                    }
                }
            }
        }
        result
    }
    fn assert_history(bundle: &ChangeGroup, old: &[String], new: &[String]) {
        let events = bundle_history_snapshot(bundle);
        for sha in old {
            assert!(
                events
                    .iter()
                    .any(|e| e.kind == "commit.dropped" && e.commit.as_ref() == Some(sha)),
                "missing dropped {sha}"
            );
        }
        for sha in new {
            assert!(
                events
                    .iter()
                    .any(|e| e.kind == "commit.recorded" && e.commit.as_ref() == Some(sha)),
                "missing recorded {sha}"
            );
        }
    }

    #[test]
    fn squash_replays_only_new_head_and_preserves_history() {
        let (dir, mut active, base, first, old) = fixture();
        git(dir.path(), &["reset", "--soft", &base]);
        git(dir.path(), &["commit", "-m", "squashed"]);
        let head = rev_parse(dir.path(), "HEAD").unwrap();
        let ids = record_rewrite(
            &mut active,
            &[RepoRewrite {
                repo_id: "repo".into(),
                old_head: old.clone(),
                new_head: head.clone(),
                old_base: base.clone(),
                new_base: base,
            }],
            RewriteKind::Squash,
            Some("Combined work"),
        )
        .unwrap();
        assert_eq!(ids.len(), 1);
        assert_eq!(active.bundle.commit_groups[0].message, "Combined work");
        assert_eq!(
            replay(&active.bundle),
            BTreeSet::from([("repo".into(), head.clone())])
        );
        let observed = active
            .bundle
            .nodes
            .iter()
            .find(|n| n.rewrite.is_some())
            .unwrap();
        assert!(observed.repo_changes[0].commits.is_empty());
        assert_eq!(
            observed.rewrite.as_ref().unwrap().superseded_groups.len(),
            2
        );
        assert_history(&active.bundle, &[first, old], &[head]);
        assert!(!active.bundle_path.exists(), "API must not persist");
    }

    #[test]
    fn rebase_maps_groups_and_records_portable_history() {
        let (dir, mut active, base, first, old) = fixture();
        git(dir.path(), &["checkout", "main"]);
        let upstream = commit(dir.path(), "upstream", "upstream");
        git(dir.path(), &["checkout", "feature"]);
        git(dir.path(), &["rebase", "main"]);
        let head = rev_parse(dir.path(), "HEAD").unwrap();
        let new = rev_list(dir.path(), &upstream, &head).unwrap();
        let ids = record_rewrite(
            &mut active,
            &[RepoRewrite {
                repo_id: "repo".into(),
                old_head: old.clone(),
                new_head: head.clone(),
                old_base: base,
                new_base: upstream.clone(),
            }],
            RewriteKind::Rebase,
            None,
        )
        .unwrap();
        assert_eq!(ids.len(), 2);
        assert_eq!(active.bundle.commit_groups[0].message, "first-group");
        assert_eq!(active.bundle.commit_groups[1].message, "second-group");
        assert_eq!(
            active.bundle.commit_groups[0].author.as_ref().unwrap().name,
            "Test Author"
        );
        assert_eq!(
            replay(&active.bundle),
            new.iter().map(|s| ("repo".into(), s.clone())).collect()
        );
        assert_history(&active.bundle, &[first, old], &new);
        assert_eq!(active.bundle.repos[0].base_sha.as_ref(), Some(&upstream));
        assert_eq!(
            crate::tracking::latest_recorded_head_sha(&active.bundle, &active.bundle.repos[0]),
            Some(head)
        );
    }

    #[test]
    fn observed_rewind_retires_removed_work_and_retains_partial_cross_repo_group() {
        let (dir, mut active, _base, first, old) = fixture();
        let other = CommitRef {
            repo_id: "other".into(),
            sha: "untouched".into(),
        };
        active.bundle.commit_groups[1].commits.push(other.clone());
        active.bundle.nodes[2].commits.push(other);
        git(dir.path(), &["reset", "--hard", &first]);
        crate::tracking::sync_observed_changes(&mut active).unwrap();
        assert_eq!(
            replay(&active.bundle),
            BTreeSet::from([
                ("repo".into(), first.clone()),
                ("other".into(), "untouched".into())
            ])
        );
        assert!(active
            .bundle
            .commit_groups
            .iter()
            .all(|g| g.commits.iter().all(|c| c.sha != old)));
        let partial = active
            .bundle
            .commit_groups
            .iter()
            .find(|g| g.id == "second-group")
            .unwrap();
        assert_eq!(partial.commits.len(), 1);
        assert_eq!(partial.commits[0].repo_id, "other");
        let observation = active
            .bundle
            .nodes
            .iter()
            .find(|n| n.rewrite.is_some())
            .unwrap();
        assert_eq!(observation.repo_changes[0].movement, Movement::Rewound);
        assert_eq!(
            observation
                .rewrite
                .as_ref()
                .unwrap()
                .superseded_groups
                .len(),
            1
        );
        assert_eq!(
            observation.rewrite.as_ref().unwrap().superseded_groups[0]
                .commits
                .len(),
            2
        );
        assert_eq!(observation.repo_changes[0].dropped_commits, vec![old]);
        assert!(observation.repo_changes[0].commits.is_empty());
        assert!(active
            .bundle
            .commit_groups
            .iter()
            .any(|g| g.id == "first-group"));
        assert_eq!(
            active.bundle.nodes.last().unwrap().node_type,
            "git.observed"
        );
    }

    #[test]
    fn observed_rebase_excludes_upstream_and_unmapped_remainder_stays_observed() {
        let (dir, mut active, _base, _, _) = fixture();
        git(dir.path(), &["checkout", "main"]);
        let upstream = commit(dir.path(), "upstream", "upstream");
        git(dir.path(), &["checkout", "feature"]);
        git(dir.path(), &["rebase", "main"]);
        let extra = commit(dir.path(), "extra", "extra");
        let reported = crate::tracking::sync_observed_changes(&mut active).unwrap();
        assert_eq!(reported.len(), 1);
        assert_eq!(reported[0].commits, vec![extra.clone()]);
        assert!(!reported[0].commit_details.contains_key(&upstream));
        assert!(crate::tracking::sync_note(&reported[0]).contains("(+1 -2"));
        assert_eq!(active.bundle.repos[0].base_sha.as_ref(), Some(&upstream));
        assert_eq!(active.bundle.commit_groups.len(), 2);
        let observed = active
            .bundle
            .nodes
            .iter()
            .find(|n| n.rewrite.is_some())
            .unwrap();
        assert_eq!(observed.repo_changes[0].commits, vec![extra.clone()]);
        assert!(!replay(&active.bundle).contains(&("repo".into(), upstream)));
        assert_eq!(
            crate::tracking::latest_recorded_head_sha(&active.bundle, &active.bundle.repos[0]),
            Some(extra)
        );
        assert!(crate::tracking::sync_observed_changes(&mut active)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn pending_git_rebase_does_not_mutate_bundle() {
        let (dir, mut active, _, first, _) = fixture();
        git(dir.path(), &["reset", "--hard", &first]);
        std::fs::create_dir(dir.path().join(".git/rebase-merge")).unwrap();
        let before = serde_json::to_value(&active.bundle).unwrap();
        let error = crate::tracking::sync_observed_changes(&mut active)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("rebase") && error.contains("--continue") && error.contains("--abort"),
            "{error}"
        );
        assert_eq!(serde_json::to_value(&active.bundle).unwrap(), before);
    }
    #[test]
    fn only_explicit_equal_count_rebase_uses_positional_fallback() {
        let (dir, _active, base, first, second) = fixture();
        git(dir.path(), &["reset", "--hard", &base]);
        let one = commit(dir.path(), "different-one", "different one");
        let two = commit(dir.path(), "different-two", "different two");
        let old = vec![first.clone(), second.clone()];
        let new = vec![one.clone(), two.clone()];
        assert!(map_commits(dir.path(), &old, &new, RewriteKind::Observed)
            .unwrap()
            .is_empty());
        assert_eq!(
            map_commits(dir.path(), &old, &new, RewriteKind::Rebase).unwrap(),
            BTreeMap::from([(first, one), (second, two.clone())])
        );
        assert!(map_commits(dir.path(), &old, &[two], RewriteKind::Rebase)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn pending_bundle_rebase_suppresses_even_completed_repository_sync() {
        let (dir, mut active, _, first, _) = fixture();
        git(dir.path(), &["reset", "--hard", &first]);
        std::fs::create_dir_all(dir.path().join(".knit/rebase")).unwrap();
        std::fs::write(dir.path().join(".knit/rebase/rewrite-test.json"), "{}").unwrap();
        let before = serde_json::to_value(&active.bundle).unwrap();
        let error = crate::tracking::sync_observed_changes(&mut active)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("rebase") && error.contains("--continue") && error.contains("--abort"),
            "{error}"
        );
        assert_eq!(serde_json::to_value(&active.bundle).unwrap(), before);
    }
    #[test]
    fn partial_observed_rewind_preserves_ungrouped_shared_sha() {
        let (dir, mut active, base, first, second) = fixture();
        active.bundle.commit_groups.clear();
        active.bundle.nodes.truncate(1);
        let change: RepoChange = serde_json::from_value(serde_json::json!({
            "repoId": "repo", "movement": "advanced", "beforeSha": base,
            "afterSha": second, "commits": [first, second]
        }))
        .unwrap();
        active.bundle.nodes.push(BundleNode::git_observed(
            "raw-work".into(),
            now_iso(),
            vec![change],
        ));
        git(dir.path(), &["reset", "--hard", &first]);
        let reported = crate::tracking::sync_observed_changes(&mut active).unwrap();
        assert_eq!(reported.len(), 1);
        assert!(reported[0].commits.is_empty());
        assert_eq!(reported[0].dropped_commits, vec![second]);
        assert_eq!(
            replay(&active.bundle),
            BTreeSet::from([("repo".into(), first)])
        );
        assert!(active.bundle.commit_groups.is_empty());
        assert_eq!(
            active.bundle.nodes.last().unwrap().node_type,
            "git.observed"
        );
    }

    #[test]
    fn ordinary_divergence_keeps_observation_and_retires_unreachable_group_members() {
        let (dir, mut active, base, first, second) = fixture();
        let groups_before = active.bundle.commit_groups.clone();
        git(dir.path(), &["reset", "--hard", &first]);
        let replacement = commit(dir.path(), "replacement", "replacement");
        let reported = crate::tracking::sync_observed_changes(&mut active).unwrap();
        assert_eq!(reported.len(), 1);
        assert_eq!(reported[0].movement, Movement::Diverged);
        assert_eq!(reported[0].commits, vec![replacement.clone()]);
        assert_eq!(reported[0].dropped_commits, vec![second]);
        assert_eq!(active.bundle.repos[0].base_sha, Some(base));
        assert!(active
            .bundle
            .commit_groups
            .iter()
            .flat_map(|g| &g.commits)
            .all(|c| c.sha == first));
        let audit = active
            .bundle
            .nodes
            .last()
            .unwrap()
            .rewrite
            .as_ref()
            .unwrap();
        assert_eq!(audit.kind, "observed");
        assert!(audit
            .superseded_groups
            .iter()
            .all(|g| groups_before.iter().any(|old| old.id == g.id)));
        assert!(!audit.superseded_groups.is_empty());
        assert_eq!(
            replay(&active.bundle),
            BTreeSet::from([("repo".into(), first), ("repo".into(), replacement)])
        );
    }

    #[test]
    fn positional_fallback_pairs_remaining_commits_after_reordered_patch_match() {
        let (dir, _active, base, first, second) = fixture();
        git(dir.path(), &["reset", "--hard", &base]);
        git(dir.path(), &["cherry-pick", &second]);
        let mapped_second = git(dir.path(), &["rev-parse", "HEAD"]);
        let unmatched_first = commit(dir.path(), "replacement", "replacement");
        let old = vec![first.clone(), second.clone()];
        let new = vec![mapped_second.clone(), unmatched_first.clone()];
        assert_eq!(
            map_commits(dir.path(), &old, &new, RewriteKind::Observed).unwrap(),
            BTreeMap::from([(second.clone(), mapped_second.clone())])
        );
        assert_eq!(
            map_commits(dir.path(), &old, &new, RewriteKind::Rebase).unwrap(),
            BTreeMap::from([(first, unmatched_first), (second, mapped_second)])
        );
    }
}
