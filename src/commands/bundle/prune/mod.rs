//! `knit prune` — find and delete dead-work bundles, orphan worktrees, and
//! orphaned KnitHub remote bundle records.
//!
//! A bundle is "dead work" when it has no open PRs, no uncommitted tracked
//! changes in any checkout, and — for repos with no recorded review object —
//! no commits on the feature branch (local or `origin/`) that base lacks.
//! The same per-bundle signals drive the prune decision, the `--untracked`
//! relaxation, and the `--report` view.

mod assess;
mod orphans;

use super::{bundle_json_paths, current_root, delete_bundle};
use crate::output as out;
use anyhow::{bail, Result};
use assess::{assess_bundles, PruneAssessment, PruneCache};
use orphans::{orphan_worktree_candidates, remote_orphan_candidates, remove_orphan_worktree};

struct PruneCandidate {
    id: String,
    repo_count: usize,
    reason: String,
}

/// Surface a non-fatal prune problem on stderr without aborting the whole run.
pub(super) fn print_prune_warning(message: impl std::fmt::Display) {
    eprintln!("{}", out::warn(message));
}

pub fn prune_merged_bundles(
    apply: bool,
    refresh: bool,
    untracked: bool,
    report: bool,
    worktrees: bool,
    force: bool,
    branches: bool,
    force_branches: bool,
    remote_branches: bool,
    remote_bundles: bool,
    include_finished: bool,
) -> Result<()> {
    if force_branches && !branches {
        bail!("Use --branches with --force-branches.");
    }
    if remote_branches && !branches {
        bail!("Use --branches with --remote-branches.");
    }
    if branches && !worktrees {
        bail!(
            "Pruning local branches requires --worktrees so generated checkouts are removed first."
        );
    }

    let root = current_root()?;
    let config = if remote_bundles {
        Some(crate::store::load_effective_config(&root)?)
    } else {
        None
    };
    let dir = root.join(".knit/bundles");
    if !dir.exists() {
        println!("{}", out::muted("No bundles."));
        return Ok(());
    }

    let mut entries = bundle_json_paths(&dir)?;
    entries.sort();
    let cache = PruneCache::new();
    let (assessments, local_ids) = assess_bundles(&root, entries, refresh, &cache);

    let mut candidates = Vec::new();
    let mut blocked_untracked = Vec::new();
    let mut kept_finished = 0usize;
    for assessment in &assessments {
        // Landed and archived bundles are finished work, not dead work: their
        // artifacts are the audit record of what shipped. Only `--archived`
        // opts them into pruning; open dead work is pruned as before.
        if !include_finished
            && matches!(
                assessment.status,
                crate::commands::bundle::BundleStatus::Landed
                    | crate::commands::bundle::BundleStatus::Archived
            )
        {
            kept_finished += 1;
            continue;
        }
        if let Some(reason) = assessment.candidate_reason(untracked) {
            candidates.push(PruneCandidate {
                id: assessment.id.clone(),
                repo_count: assessment.repo_count,
                reason,
            });
        } else if assessment.blocked_by_untracked_only() {
            blocked_untracked.push(assessment);
        }
    }

    let (orphan_worktrees, blocked_orphan_worktrees) = if worktrees {
        orphan_worktree_candidates(&root, force)?
    } else {
        (Vec::new(), Vec::new())
    };
    let remote_orphans = if remote_bundles {
        remote_orphan_candidates(config.as_ref(), &local_ids, &root, refresh)
    } else {
        Vec::new()
    };

    if report {
        print_prune_report(&assessments, untracked);
    }

    if kept_finished > 0 {
        println!(
            "{}",
            out::muted(format!(
                "Kept {kept_finished} finished (landed/archived) bundle artifact(s) as history; pass --archived to prune them too."
            ))
        );
    }

    if candidates.is_empty()
        && orphan_worktrees.is_empty()
        && blocked_untracked.is_empty()
        && blocked_orphan_worktrees.is_empty()
        && remote_orphans.is_empty()
    {
        println!(
            "{}",
            out::muted("No dead bundles, orphan worktrees, or remote orphan records to prune.")
        );
        return Ok(());
    }

    if !candidates.is_empty() {
        println!("{}", out::heading("Dead bundle candidates:"));
        for candidate in &candidates {
            println!(
                "  {} {} repo(s), {}",
                out::node(&candidate.id),
                candidate.repo_count,
                out::muted(&candidate.reason)
            );
        }
    }

    if !blocked_untracked.is_empty() {
        println!(
            "{}",
            out::heading("Blocked by untracked files (use --untracked to prune):")
        );
        for assessment in &blocked_untracked {
            println!(
                "  {} {} repo(s), {}",
                out::node(&assessment.id),
                assessment.repo_count,
                out::muted(format!("{}, only untracked files", assessment.pr_basis()))
            );
        }
    }
    if !blocked_orphan_worktrees.is_empty() {
        println!(
            "{}",
            out::heading("Blocked orphan worktrees (use --force to prune):")
        );
        for orphan in &blocked_orphan_worktrees {
            println!(
                "  {} {}",
                out::node(&orphan.id),
                out::path(orphan.path.display())
            );
        }
    }
    if !orphan_worktrees.is_empty() {
        println!("{}", out::heading("Orphan worktree candidates:"));
        for orphan in &orphan_worktrees {
            if orphan.discards_pending {
                println!(
                    "  {} {} {}",
                    out::node(&orphan.id),
                    out::path(orphan.path.display()),
                    out::muted("discards uncommitted work")
                );
            } else {
                println!(
                    "  {} {}",
                    out::node(&orphan.id),
                    out::path(orphan.path.display())
                );
            }
        }
    }
    if !remote_orphans.is_empty() {
        println!("{}", out::heading("Remote orphan bundle candidates:"));
        for orphan in &remote_orphans {
            println!(
                "  {} {} ({})",
                out::node(&orphan.slug),
                out::muted("KnitHub record with no local bundle"),
                out::muted(orphan.reason)
            );
        }
    }

    if !apply {
        println!();
        println!(
            "{}",
            out::warn(format!(
                "Run `{}` to delete these bundle artifacts.",
                suggested_prune_apply_command(
                    untracked,
                    worktrees,
                    force || !blocked_orphan_worktrees.is_empty(),
                    branches,
                    force_branches,
                    remote_branches,
                    remote_bundles,
                )
            ))
        );
        return Ok(());
    }

    let mut pruned = 0usize;
    for candidate in candidates {
        delete_bundle(
            &candidate.id,
            true,
            worktrees,
            branches,
            force_branches,
            remote_branches,
            remote_bundles,
            config.as_ref(),
        )?;
        pruned += 1;
    }
    let mut removed_orphans = 0usize;
    for orphan in orphan_worktrees {
        remove_orphan_worktree(&orphan, force)?;
        removed_orphans += 1;
    }
    // Remote orphan records are archived, never deleted: a record whose local
    // artifact is gone is the last remaining trace of shipped work, and the
    // hosted dashboard is the durable archive of record. True deletion stays a
    // per-bundle decision via `knit bundle delete --remote-bundles`.
    let mut removed_remote = 0usize;
    if let Some(config) = config.as_ref() {
        for orphan in remote_orphans {
            match crate::commands::remote::archive_remote_bundle_by_id(config, &orphan.remote_id) {
                Ok(slug) => {
                    println!(
                        "{}: {} {}",
                        out::node(&orphan.slug),
                        out::movement("archived remote bundle"),
                        out::muted(slug)
                    );
                    removed_remote += 1;
                }
                Err(err) => print_prune_warning(format!(
                    "{}: failed to archive remote bundle record: {err:#}",
                    orphan.slug
                )),
            }
        }
    }

    println!("{} {} bundle(s)", out::heading("Pruned:"), pruned);
    if removed_orphans > 0 {
        println!(
            "{} {} orphan worktree dir(s)",
            out::heading("Removed:"),
            removed_orphans
        );
    }
    if removed_remote > 0 {
        println!(
            "{} {} remote orphan record(s)",
            out::heading("Archived:"),
            removed_remote
        );
    }
    Ok(())
}

fn print_prune_report(assessments: &[PruneAssessment], untracked: bool) {
    println!("{}", out::heading("Bundle report:"));
    for assessment in assessments {
        let status = if let Some(reason) = assessment.candidate_reason(untracked) {
            format!("prunable — {reason}")
        } else if assessment.blocked_by_untracked_only() {
            "kept — only untracked files (prunable with --untracked)".to_string()
        } else if assessment.saw_open_publication {
            "kept — open PR(s)".to_string()
        } else if assessment.saw_unpublished_commits {
            "kept — unpublished commits on the feature branch".to_string()
        } else if assessment.pending.tracked {
            "kept — uncommitted tracked changes".to_string()
        } else {
            "kept".to_string()
        };

        let mut detail = vec![
            format!("{} repo(s)", assessment.repo_count),
            assessment.pr_basis().to_string(),
        ];
        if assessment.saw_unpublished_commits {
            detail.push("unpublished commits".to_string());
        }
        if assessment.pending.tracked {
            detail.push("tracked changes".to_string());
        }
        if assessment.pending.untracked {
            detail.push("untracked files".to_string());
        }

        println!("  {} {}", out::node(&assessment.id), out::muted(status));
        println!("      {}", out::muted(detail.join(", ")));
    }
    println!();
}

fn suggested_prune_apply_command(
    untracked: bool,
    worktrees: bool,
    force: bool,
    branches: bool,
    force_branches: bool,
    remote_branches: bool,
    remote_bundles: bool,
) -> String {
    if worktrees && force && branches && force_branches && remote_branches && remote_bundles {
        let base = "knit bundle prune --apply --all";
        return if untracked {
            format!("{base} --untracked")
        } else {
            base.to_string()
        };
    }
    let mut command = vec!["knit", "bundle", "prune", "--apply"];
    if untracked {
        command.push("--untracked");
    }
    if worktrees {
        command.push("--worktrees");
    }
    if force {
        command.push("--force");
    }
    if branches {
        command.push("--branches");
    }
    if force_branches {
        command.push("--force-branches");
    }
    if remote_branches {
        command.push("--remote-branches");
    }
    if remote_bundles {
        command.push("--remote-bundles");
    }
    command.join(" ")
}

#[cfg(test)]
mod prune_tests {
    use super::*;

    #[test]
    fn suggested_command_includes_force_when_needed() {
        let cmd = suggested_prune_apply_command(false, true, true, false, false, false, false);
        assert_eq!(cmd, "knit bundle prune --apply --worktrees --force");
    }
}
