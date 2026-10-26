use crate::checkout::{checkout_dir, ensure_mutable_checkouts};
use crate::git::{commit_details, git_output_optional, rev_list, rev_parse};
use crate::ids::{commit_group_id, short_sha};
use crate::model::{BundleNode, CommitGroup, CommitRef, Movement, RepoChange};
use crate::output as out;
use crate::store::{load_active_bundle_for_update, save_active_bundle, ActiveBundle};
use crate::time::now_iso;
use crate::tracking::{sync_note, sync_observed_changes};
use anyhow::Result;
use std::collections::BTreeMap;
use std::path::Path;

pub fn sync_bundle() -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    ensure_mutable_checkouts(&active)?;
    let changes = sync_observed_changes(&mut active)?;

    if changes.is_empty() {
        println!("{}", out::ok("No unrecorded git commits found."));
        return Ok(());
    }

    for change in &changes {
        println!(
            "{}: {}",
            out::repo(&change.repo_id),
            out::warn(sync_note(change))
        );
    }

    save_active_bundle(&active)?;
    Ok(())
}

/// One named group an earlier observation superseded, and where its commits
/// live now.
struct Restorable {
    group: CommitGroup,
    /// The current commit for each of the group's recorded commits.
    mapped: Vec<CommitRef>,
}

/// Restore commit groups an earlier `knit sync` turned into anonymous observed
/// work.
///
/// Before Knit recognised content-identical in-place rewrites, re-signing or
/// re-dating a bundle's commits outside Knit made `knit sync` record a
/// `git.observed` node that superseded every named group. The names were not
/// lost — the node still carries them under `rewrite.supersededGroups` — so
/// this walks those records, maps each recorded commit onto the commit that
/// replaced it, and puts the group back on the current SHAs.
///
/// Mapping is content-based and refuses to guess: a group is restored only
/// when every one of its commits maps to exactly one current commit with the
/// same tree, author and subject (or, when the superseded object is no longer
/// in the repository, the same subject recorded alongside it). Without
/// `apply` nothing is written.
pub fn repair_commit_groups(apply: bool) -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    let present: Vec<(String, String)> = active
        .bundle
        .commit_groups
        .iter()
        .flat_map(|group| &group.commits)
        .map(|commit| (commit.repo_id.clone(), commit.sha.clone()))
        .collect();
    // A restored group keeps the original's message and creation time, so an
    // earlier repair is recognizable even though its commits are new.
    let restored: Vec<(&str, &str)> = active
        .bundle
        .commit_groups
        .iter()
        .map(|group| (group.message.as_str(), group.created_at.as_str()))
        .collect();

    // Current bundle commits per repo, with the checkout they live in.
    let mut repos: BTreeMap<String, (std::path::PathBuf, Vec<String>)> = BTreeMap::new();
    for repo in &active.bundle.repos {
        let Some(path) = checkout_dir(&active, repo) else {
            continue;
        };
        let Some(base) = repo.base_sha.as_deref() else {
            continue;
        };
        let Ok(head) = rev_parse(&path, "HEAD") else {
            continue;
        };
        let Ok(base) = rev_parse(&path, base) else {
            continue;
        };
        let commits = rev_list(&path, &base, &head)?;
        repos.insert(repo.id.clone(), (path, commits));
    }

    let mut restorable = Vec::new();
    let mut refused = Vec::new();
    for node in &active.bundle.nodes {
        let Some(rewrite) = &node.rewrite else {
            continue;
        };
        if rewrite.kind != "observed" {
            continue;
        }
        // Subjects recorded for commits the repository may have since pruned.
        let subjects: BTreeMap<&String, &str> = node
            .repo_changes
            .iter()
            .flat_map(|change| &change.commit_details)
            .map(|(sha, detail)| (sha, detail.subject.as_str()))
            .collect();
        for group in &rewrite.superseded_groups {
            // A group whose commits are already current needs no repair, and
            // neither does one an earlier repair already put back.
            if restored.contains(&(group.message.as_str(), group.created_at.as_str()))
                || group
                    .commits
                    .iter()
                    .all(|commit| present.contains(&(commit.repo_id.clone(), commit.sha.clone())))
            {
                continue;
            }
            match map_group(group, &repos, &subjects) {
                // Already restored under another creation time (by hand, or by
                // an older repair): its commits belong to a current group.
                Ok(mapped)
                    if mapped.iter().all(|commit| {
                        present.contains(&(commit.repo_id.clone(), commit.sha.clone()))
                    }) => {}
                Ok(mapped) => restorable.push(Restorable {
                    group: group.clone(),
                    mapped,
                }),
                Err(reason) => refused.push(format!("{} ({}): {reason}", group.id, group.message)),
            }
        }
    }

    // A later observation can supersede a group this walk already restored.
    // Keep the newest record of each group id; the mapping is the same.
    let mut seen = std::collections::BTreeSet::new();
    restorable.retain(|candidate| seen.insert(candidate.group.id.clone()));

    if restorable.is_empty() {
        for line in &refused {
            println!("{} {line}", out::warn("Not restorable:"));
        }
        println!(
            "{}",
            out::ok("No superseded commit groups need restoring in this bundle.")
        );
        return Ok(());
    }

    for candidate in &restorable {
        println!(
            "{} {} {}",
            out::heading("Restore group"),
            out::node(&candidate.group.id),
            candidate.group.message
        );
        for (before, after) in candidate.group.commits.iter().zip(&candidate.mapped) {
            println!(
                "  {}: {} -> {}",
                out::repo(&after.repo_id),
                out::sha(short_sha(&before.sha)),
                out::sha(short_sha(&after.sha))
            );
        }
    }
    for line in &refused {
        println!("{} {line}", out::warn("Not restorable:"));
    }
    if !apply {
        println!(
            "{} re-run with `--apply` to write the restored groups",
            out::heading("Preview:")
        );
        return Ok(());
    }

    let now = now_iso();
    for candidate in restorable {
        let group = CommitGroup {
            // A fresh id: the superseded group's own id already belongs to a
            // recorded node, and ledger node ids must stay unique.
            id: commit_group_id(),
            message: candidate.group.message.clone(),
            created_at: candidate.group.created_at.clone(),
            commits: candidate.mapped.clone(),
            author: candidate.group.author.clone(),
        };
        let changes = group_repo_changes(&active, &repos, &group);
        active.bundle.nodes.push(BundleNode::commit_group(
            group.id.clone(),
            now.clone(),
            group.message.clone(),
            group.commits.clone(),
            changes,
        ));
        println!(
            "{} {} {}",
            out::movement("restored"),
            out::node(&group.id),
            group.message
        );
        active.bundle.commit_groups.push(group);
    }
    // Groups read in the order they were recorded, so the ledger stays in
    // chronological order after the restore.
    active
        .bundle
        .commit_groups
        .sort_by(|left, right| left.created_at.cmp(&right.created_at));
    active.bundle.head_node_id = active.bundle.nodes.last().map(|node| node.id.clone());
    active.bundle.updated_at = now;
    save_active_bundle(&active)?;
    println!(
        "{} push the restored ledger with `knit sync push --bundles --force-with-lease`",
        out::heading("Next:")
    );
    Ok(())
}

/// Map every commit of a superseded group onto the current commit that
/// replaced it, or explain why it cannot be done.
fn map_group(
    group: &CommitGroup,
    repos: &BTreeMap<String, (std::path::PathBuf, Vec<String>)>,
    subjects: &BTreeMap<&String, &str>,
) -> Result<Vec<CommitRef>, String> {
    let mut mapped = Vec::new();
    let mut used: std::collections::BTreeSet<(String, String)> = Default::default();
    for commit in &group.commits {
        let Some((path, candidates)) = repos.get(&commit.repo_id) else {
            return Err(format!(
                "repo {} has no materialized checkout with a recorded base",
                commit.repo_id
            ));
        };
        let wanted = commit_identity(path, &commit.sha);
        let matches: Vec<&String> = candidates
            .iter()
            .filter(|sha| !used.contains(&(commit.repo_id.clone(), (*sha).clone())))
            .filter(|sha| match &wanted {
                Some(wanted) => commit_identity(path, sha).as_ref() == Some(wanted),
                // The superseded object is gone; fall back to the subject the
                // observation recorded for it.
                None => match subjects.get(&commit.sha) {
                    Some(subject) => commit_subject(path, sha).as_deref() == Some(subject),
                    None => false,
                },
            })
            .collect();
        match matches.as_slice() {
            [only] => {
                used.insert((commit.repo_id.clone(), (*only).clone()));
                mapped.push(CommitRef {
                    repo_id: commit.repo_id.clone(),
                    sha: (*only).clone(),
                });
            }
            [] => {
                return Err(format!(
                    "{}: no current commit matches {}",
                    commit.repo_id,
                    short_sha(&commit.sha)
                ))
            }
            several => {
                return Err(format!(
                    "{}: {} current commits match {} equally well",
                    commit.repo_id,
                    several.len(),
                    short_sha(&commit.sha)
                ))
            }
        }
    }
    Ok(mapped)
}

/// Per-repo movement for a restored group, describing the commits it owns.
fn group_repo_changes(
    active: &ActiveBundle,
    repos: &BTreeMap<String, (std::path::PathBuf, Vec<String>)>,
    group: &CommitGroup,
) -> Vec<RepoChange> {
    let mut changes = Vec::new();
    for repo in &active.bundle.repos {
        let commits: Vec<String> = group
            .commits
            .iter()
            .filter(|commit| commit.repo_id == repo.id)
            .map(|commit| commit.sha.clone())
            .collect();
        if commits.is_empty() {
            continue;
        }
        let details = repos
            .get(&repo.id)
            .map(|(path, _)| commit_details(path, &commits))
            .unwrap_or_default();
        changes.push(RepoChange {
            repo_id: repo.id.clone(),
            movement: Movement::Advanced,
            before_sha: repo.base_sha.clone(),
            after_sha: commits.last().cloned().unwrap_or_default(),
            base_before_sha: None,
            base_after_sha: None,
            commits,
            dropped_commits: Vec::new(),
            commit_details: details,
        });
    }
    changes
}

/// Tree, author and subject of a commit; `None` when the repository no longer
/// has the object.
fn commit_identity(path: &Path, sha: &str) -> Option<String> {
    git_output_optional(
        path,
        [
            "show",
            "-s",
            "--format=%T%n%an%n%ae%n%s",
            &format!("{sha}^{{commit}}"),
        ],
    )
    .ok()
    .flatten()
}

fn commit_subject(path: &Path, sha: &str) -> Option<String> {
    git_output_optional(path, ["show", "-s", "--format=%s", sha])
        .ok()
        .flatten()
        .map(|subject| subject.trim().to_string())
}
