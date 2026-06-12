//! Orphan cleanup targets: generated worktree directories whose bundle is
//! gone, and KnitHub remote bundle records with no local artifact whose PRs
//! are all merged or closed (unreachable by the local bundle scan).

use super::assess::{
    path_pending_changes, publication_state_is_closed, publication_state_is_merged, PruneCache,
};
use super::print_prune_warning;
use crate::git::{git_output, is_git_worktree};
use crate::model::{ChangeGroup, KnitConfig};
use crate::output as out;
use crate::providers;
use crate::store::bundle_exists;
use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

pub(super) struct OrphanWorktree {
    pub(super) id: String,
    pub(super) path: PathBuf,
    pub(super) discards_pending: bool,
}

/// A bundle that exists on the KnitHub sync remote but has no local artifact and
/// whose recorded pull requests are all merged or closed, so prune can never reach
/// it through the local `.knit/bundles/` scan.
pub(super) struct RemoteOrphan {
    pub(super) remote_id: String,
    pub(super) slug: String,
    pub(super) reason: &'static str,
}

/// Find KnitHub remote bundle records that have no local artifact and whose recorded
/// PRs are all merged or closed. These can never be reached by the local `.knit/bundles/`
/// scan once their local artifact is gone, so prune would otherwise leave them forever.
pub(super) fn remote_orphan_candidates(
    config: Option<&KnitConfig>,
    local_ids: &BTreeSet<String>,
    root: &Path,
    refresh: bool,
) -> Vec<RemoteOrphan> {
    let Some(config) = config else {
        return Vec::new();
    };
    let Some(project_id) = config.active_project.clone() else {
        print_prune_warning(
            "no active project configured; skipping KnitHub remote orphan detection",
        );
        return Vec::new();
    };
    let records = match crate::commands::remote::list_remote_bundles(config, &project_id) {
        Ok(records) => records,
        Err(err) => {
            print_prune_warning(format!(
                "could not list KnitHub remote bundles ({err:#}); skipping remote orphan detection"
            ));
            return Vec::new();
        }
    };
    let mut orphans = Vec::new();
    let cache = PruneCache::new();
    let jobs: Vec<RemoteOrphanJob> = records
        .into_iter()
        .filter_map(|record| {
            if record.lifecycle_state == "deleted" || local_ids.contains(&record.slug) {
                return None;
            }
            let payload = record.payload?;
            Some(RemoteOrphanJob {
                remote_id: record.remote_id,
                slug: record.slug,
                payload,
            })
        })
        .collect();

    let results: Vec<(String, Option<RemoteOrphan>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = jobs
            .iter()
            .map(|job| {
                let slug = job.slug.clone();
                let remote_id = job.remote_id.clone();
                let payload = job.payload.clone();
                let cache = cache.clone();
                scope.spawn(move || {
                    (
                        slug.clone(),
                        remote_payload_dead_reason(root, &payload, refresh, &cache).map(|reason| {
                            RemoteOrphan {
                                remote_id,
                                slug,
                                reason,
                            }
                        }),
                    )
                })
            })
            .collect();

        handles
            .into_iter()
            .map(|handle| handle.join().expect("remote orphan worker thread panicked"))
            .collect()
    });

    for (slug, orphan) in results {
        if let Some(orphan) = orphan {
            orphans.push(orphan);
        } else {
            let _ = slug;
        }
    }
    orphans
}

struct RemoteOrphanJob {
    remote_id: String,
    slug: String,
    payload: ChangeGroup,
}

/// Classify an orphaned remote bundle's pull requests. The remote's stored artifact can be
/// stale (it was pushed before the PR merged), so with `refresh` on we re-check each PR's
/// live state from its host by URL, falling back to the last synced state only when the
/// lookup fails. A bundle is dead only when it has publications and none are still open; one
/// with no recorded PRs is left alone in case it is unpublished work in progress.
fn remote_payload_dead_reason(
    root: &Path,
    payload: &ChangeGroup,
    refresh: bool,
    cache: &PruneCache,
) -> Option<&'static str> {
    if payload.publications.is_empty() {
        return None;
    }

    let states: Vec<String> = if refresh {
        std::thread::scope(|scope| {
            let handles: Vec<_> = payload
                .publications
                .iter()
                .map(|publication| {
                    let publication = publication.clone();
                    scope.spawn(move || refresh_remote_publication_state(root, &publication, cache))
                })
                .collect();

            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .expect("remote publication worker thread panicked")
                })
                .collect()
        })
    } else {
        payload
            .publications
            .iter()
            .map(|publication| publication.state.clone())
            .collect()
    };

    classify_remote_publication_states(&states)
}

fn refresh_remote_publication_state(
    root: &Path,
    publication: &crate::model::PublicationEntry,
    cache: &PruneCache,
) -> String {
    match providers::by_id(&publication.provider) {
        Some(forge) => match cache.view_pr(forge.as_ref(), root, &publication.url) {
            Ok(pr) => pr.state.unwrap_or_else(|| publication.state.clone()),
            Err(err) => {
                print_prune_warning(format!(
                    "could not refresh remote review object {} ({err:#}); using last synced state",
                    publication.url
                ));
                publication.state.clone()
            }
        },
        None => publication.state.clone(),
    }
}

fn classify_remote_publication_states(states: &[String]) -> Option<&'static str> {
    let mut saw_merged = false;
    for state in states {
        if publication_state_is_merged(state) {
            saw_merged = true;
        } else if !publication_state_is_closed(state) {
            return None;
        }
    }
    if saw_merged {
        Some("remote PRs are merged")
    } else {
        Some("remote PRs are closed")
    }
}

pub(super) fn orphan_worktree_candidates(
    root: &Path,
    force: bool,
) -> Result<(Vec<OrphanWorktree>, Vec<OrphanWorktree>)> {
    let worktrees_dir = root.join(".knit/worktrees");
    if !worktrees_dir.exists() {
        return Ok((Vec::new(), Vec::new()));
    }
    let mut candidates = Vec::new();
    let mut blocked = Vec::new();
    for entry in fs::read_dir(&worktrees_dir)
        .with_context(|| format!("failed to read {}", worktrees_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().to_string();
        if id.starts_with('.') || bundle_exists(root, &id) {
            continue;
        }
        let orphan = OrphanWorktree {
            id,
            path,
            discards_pending: false,
        };
        if path_pending_changes(&orphan.path)?.any() {
            if force {
                candidates.push(OrphanWorktree {
                    discards_pending: true,
                    ..orphan
                });
            } else {
                blocked.push(orphan);
            }
            continue;
        }
        candidates.push(orphan);
    }
    candidates.sort_by(|left, right| left.id.cmp(&right.id));
    blocked.sort_by(|left, right| left.id.cmp(&right.id));
    Ok((candidates, blocked))
}

pub(super) fn remove_orphan_worktree(orphan: &OrphanWorktree, force: bool) -> Result<()> {
    let worktrees = git_worktrees_under(&orphan.path)?;
    for worktree in worktrees {
        if is_linked_worktree(&worktree) {
            remove_git_worktree_from_self(&worktree, force)?;
        }
    }
    if orphan.path.exists() {
        fs::remove_dir_all(&orphan.path)
            .with_context(|| format!("failed to remove {}", orphan.path.display()))?;
    }
    println!(
        "{}: {} {}",
        out::node(&orphan.id),
        out::movement("removed orphan worktree"),
        out::path(orphan.path.display())
    );
    Ok(())
}

fn is_linked_worktree(path: &Path) -> bool {
    path.join(".git").is_file()
}

fn git_worktrees_under(path: &Path) -> Result<Vec<PathBuf>> {
    let mut worktrees = Vec::new();
    collect_git_worktrees(path, &mut worktrees)?;
    worktrees.sort();
    Ok(worktrees)
}

fn collect_git_worktrees(path: &Path, worktrees: &mut Vec<PathBuf>) -> Result<()> {
    if !path.is_dir() {
        return Ok(());
    }
    if is_git_worktree(path) {
        worktrees.push(path.to_path_buf());
        return Ok(());
    }
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        collect_git_worktrees(&entry?.path(), worktrees)?;
    }
    Ok(())
}

fn remove_git_worktree_from_self(worktree: &Path, force: bool) -> Result<()> {
    let mut args = vec![OsString::from("worktree"), OsString::from("remove")];
    if force {
        args.push(OsString::from("--force"));
    }
    args.push(worktree.as_os_str().to_os_string());
    git_output(worktree, args)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PublicationEntry;

    fn bundle_with_publication_states(states: &[&str]) -> ChangeGroup {
        let mut bundle = ChangeGroup::new(
            "feature".to_string(),
            "feature".to_string(),
            "2026-05-05T00:00:00.000Z".to_string(),
        );
        bundle.publications = states
            .iter()
            .enumerate()
            .map(|(index, state)| PublicationEntry {
                repo_id: format!("repo{index}"),
                provider: "github".to_string(),
                kind: providers::PULL_REQUEST_KIND.to_string(),
                number: index as u64 + 1,
                url: format!("https://github.com/acme/repo{index}/pull/{}", index + 1),
                base_branch: "main".to_string(),
                head_branch: format!("knit/feature-{index}"),
                state: (*state).to_string(),
                title: None,
                updated_at: "2026-05-05T00:00:00.000Z".to_string(),
            })
            .collect();
        bundle
    }

    // refresh=false keeps these pure (no host lookups), exercising the classification of
    // an orphan remote bundle from its recorded publication states.
    fn dead_reason(states: &[&str]) -> Option<&'static str> {
        let cache = PruneCache::new();
        remote_payload_dead_reason(
            Path::new("/nonexistent"),
            &bundle_with_publication_states(states),
            false,
            &cache,
        )
    }

    #[test]
    fn all_merged_publications_are_dead() {
        assert_eq!(
            dead_reason(&["MERGED", "merged"]),
            Some("remote PRs are merged")
        );
    }

    #[test]
    fn all_closed_publications_are_dead() {
        assert_eq!(dead_reason(&["CLOSED"]), Some("remote PRs are closed"));
    }

    #[test]
    fn merged_and_closed_without_open_is_dead() {
        assert_eq!(
            dead_reason(&["MERGED", "CLOSED"]),
            Some("remote PRs are merged")
        );
    }

    #[test]
    fn any_open_publication_keeps_the_bundle() {
        assert_eq!(dead_reason(&["MERGED", "OPEN"]), None);
    }

    #[test]
    fn no_publications_is_not_an_orphan() {
        assert_eq!(dead_reason(&[]), None);
    }
}
