//! `knit publish` — create one review object (PR/MR) per repo and keep their
//! bodies cross-linked. [`scope`] resolves which repos publish, [`remote`]
//! executes per-repo publishing, [`sync`] maintains PR bodies, and [`status`]
//! reports recorded/live state. The `*_from_artifact` entry points run the
//! same flows from a bundle artifact JSON with no local worktrees.

mod pr_body;
mod remote;
mod scope;
mod status;
mod sync;

pub use status::show_publication_status;

use crate::model::ChangeGroup;
use crate::output as out;
use crate::providers::publication_for_repo;
use crate::store::{load_active_bundle_for_update, save_active_bundle};
use anyhow::{bail, Context, Result};
use remote::{
    apply_artifact_publish_result, apply_publish_remote_result, publish_repo_remote,
    publish_repo_remote_from_artifact, ArtifactPublishResult, PublishJob, PublishRemoteResult,
};
use scope::{
    filter_indexes_by_provider, resolve_publish_repo_indexes,
    resolve_publish_repo_indexes_for_bundle, BaseOverrides,
};
use std::path::Path;
use sync::{sync_publications_for_indexes, sync_publications_for_indexes_from_artifact};

pub fn create_publications(
    selectors: &[String],
    all: bool,
    draft: bool,
    bases: &[String],
    sync: bool,
    set_upstream: bool,
    remote: &[String],
    no_remote: bool,
    provider: Option<&str>,
) -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    if active.bundle.repos.is_empty() {
        bail!("The resolved bundle has no repos. Run `knit bundle add <repo-path>` first.");
    }

    let indexes = resolve_publish_repo_indexes(&active, selectors, all)?;
    let indexes = filter_indexes_by_provider(&active.bundle.repos, indexes, provider)?;
    let base_overrides = BaseOverrides::parse(bases)?;
    base_overrides.validate_tracked_repos(&active.bundle)?;
    let bundle_snapshot = active.bundle.clone();
    let mut failures = Vec::new();
    let mut bundle_changed = false;

    let jobs: Vec<PublishJob> = indexes
        .iter()
        .map(|&index| {
            let repo = active.bundle.repos[index].clone();
            let base_branch =
                base_overrides.branch_for(&repo, publication_for_repo(&active.bundle, &repo.id));
            PublishJob {
                repo_index: index,
                repo,
                base_branch,
            }
        })
        .collect();

    let results: Vec<(String, Result<PublishRemoteResult>)> = std::thread::scope(|scope| {
        let active = &active;
        let bundle = &bundle_snapshot;
        let handles: Vec<_> = jobs
            .iter()
            .map(|job| {
                let job = job.clone();
                let repo_id = job.repo.id.clone();
                scope.spawn(move || {
                    (
                        repo_id,
                        publish_repo_remote(active, bundle, &job, draft, set_upstream),
                    )
                })
            })
            .collect();

        handles
            .into_iter()
            .map(|handle| handle.join().expect("publish worker thread panicked"))
            .collect()
    });

    for (repo_id, result) in results {
        match result {
            Ok(outcome) => {
                if apply_publish_remote_result(&mut active, &outcome)? {
                    bundle_changed = true;
                }
            }
            Err(error) => {
                println!(
                    "{}: {}",
                    out::repo(&repo_id),
                    out::danger("PR create failed")
                );
                failures.push(format!("{repo_id}: {error:#}"));
            }
        }
    }

    if bundle_changed {
        save_active_bundle(&active)?;
    }

    if failures.is_empty() && sync {
        failures.extend(sync_publications_for_indexes(&mut active, &indexes)?);
    } else if !sync {
        println!(
            "{}",
            out::warn("Skipped PR body sync. Run `knit publish sync` to add cross-links later.")
        );
    }

    // Sync the bundle artifact to the configured KnitHub remote alongside the
    // host review objects (default on; see `knit config set push-sync`).
    crate::commands::remote::maybe_sync_bundle_to_remote(remote, no_remote)?;

    if !failures.is_empty() {
        bail!(
            "PR publishing completed with failures:\n{}",
            failures.join("\n")
        );
    }

    Ok(())
}

pub fn create_publications_from_artifact(
    artifact_path: &Path,
    out_path: Option<&Path>,
    selectors: &[String],
    all: bool,
    draft: bool,
    bases: &[String],
    sync: bool,
    push: bool,
    provider: Option<&str>,
) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let mut bundle: ChangeGroup = crate::store::read_json(artifact_path)
        .with_context(|| format!("failed to load bundle artifact {}", artifact_path.display()))?;
    if bundle.repos.is_empty() {
        bail!("Bundle artifact has no repos.");
    }
    if push {
        bail!("Artifact publish does not support git push. Re-run with --no-push.");
    }

    let indexes = resolve_publish_repo_indexes_for_bundle(&bundle, selectors, all)?;
    let indexes = filter_indexes_by_provider(&bundle.repos, indexes, provider)?;
    let base_overrides = BaseOverrides::parse(bases)?;
    base_overrides.validate_tracked_repos(&bundle)?;
    let bundle_snapshot = bundle.clone();
    let mut failures = Vec::new();

    let jobs: Vec<PublishJob> = indexes
        .iter()
        .map(|&index| {
            let repo = bundle.repos[index].clone();
            let base_branch =
                base_overrides.branch_for(&repo, publication_for_repo(&bundle, &repo.id));
            PublishJob {
                repo_index: index,
                repo,
                base_branch,
            }
        })
        .collect();

    let results: Vec<(String, Result<ArtifactPublishResult>)> = std::thread::scope(|scope| {
        let cwd = cwd.as_ref();
        let bundle = &bundle_snapshot;
        let handles: Vec<_> = jobs
            .iter()
            .map(|job| {
                let job = job.clone();
                let repo_id = job.repo.id.clone();
                scope.spawn(move || {
                    (
                        repo_id,
                        publish_repo_remote_from_artifact(cwd, bundle, &job, draft),
                    )
                })
            })
            .collect();

        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .expect("artifact publish worker thread panicked")
            })
            .collect()
    });

    for (repo_id, result) in results {
        match result {
            Ok(outcome) => apply_artifact_publish_result(&mut bundle, &outcome),
            Err(error) => {
                println!(
                    "{}: {}",
                    out::repo(&repo_id),
                    out::danger("PR create failed")
                );
                failures.push(format!("{repo_id}: {error:#}"));
            }
        }
    }

    if failures.is_empty() && sync {
        failures.extend(sync_publications_for_indexes_from_artifact(
            &cwd,
            &mut bundle,
            &indexes,
        )?);
    } else if !sync {
        println!(
            "{}",
            out::warn("Skipped PR body sync. Run `knit publish sync` to add cross-links later.")
        );
    }

    if !failures.is_empty() {
        bail!(
            "PR publishing completed with failures:\n{}",
            failures.join("\n")
        );
    }

    write_bundle_artifact_output(&bundle, out_path)?;
    Ok(())
}

pub fn sync_publications(selectors: &[String], all: bool, provider: Option<&str>) -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    if active.bundle.repos.is_empty() {
        bail!("The resolved bundle has no repos. Run `knit bundle add <repo-path>` first.");
    }

    let indexes = resolve_publish_repo_indexes(&active, selectors, all)?;
    let indexes = filter_indexes_by_provider(&active.bundle.repos, indexes, provider)?;
    let failures = sync_publications_for_indexes(&mut active, &indexes)?;
    if !failures.is_empty() {
        bail!("PR sync completed with failures:\n{}", failures.join("\n"));
    }

    Ok(())
}

pub fn sync_publications_from_artifact(
    artifact_path: &Path,
    out_path: Option<&Path>,
    selectors: &[String],
    all: bool,
    provider: Option<&str>,
) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let mut bundle: ChangeGroup = crate::store::read_json(artifact_path)
        .with_context(|| format!("failed to load bundle artifact {}", artifact_path.display()))?;
    if bundle.repos.is_empty() {
        bail!("Bundle artifact has no repos.");
    }
    let indexes = resolve_publish_repo_indexes_for_bundle(&bundle, selectors, all)?;
    let indexes = filter_indexes_by_provider(&bundle.repos, indexes, provider)?;
    let failures = sync_publications_for_indexes_from_artifact(&cwd, &mut bundle, &indexes)?;
    if !failures.is_empty() {
        bail!("PR sync completed with failures:\n{}", failures.join("\n"));
    }
    write_bundle_artifact_output(&bundle, out_path)?;
    Ok(())
}

fn write_bundle_artifact_output(bundle: &ChangeGroup, out_path: Option<&Path>) -> Result<()> {
    match out_path {
        Some(path) => crate::store::write_json(path, bundle),
        None => {
            let json =
                serde_json::to_string_pretty(bundle).context("failed to encode bundle JSON")?;
            println!("{json}");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::pr_body::{
        render_knit_pr_block, upsert_knit_pr_block, KNIT_PR_BLOCK_BEGIN, KNIT_PR_BLOCK_END,
    };
    use super::scope::publish_scope_repo_ids;
    use super::*;
    use crate::model::RepoEntry;
    use crate::model::{
        CommitGroup, CommitRef, PublicationEntry, CHANGE_GROUP_KIND, SCHEMA_VERSION,
    };
    use crate::providers;

    fn pr_publication(repo_id: &str, number: u64, url: &str) -> PublicationEntry {
        PublicationEntry {
            repo_id: repo_id.to_string(),
            provider: "github".to_string(),
            kind: providers::PULL_REQUEST_KIND.to_string(),
            number,
            url: url.to_string(),
            base_branch: "main".to_string(),
            head_branch: "knit/venue-capacity".to_string(),
            state: "OPEN".to_string(),
            title: None,
            updated_at: "2026-05-05T00:00:00.000Z".to_string(),
        }
    }

    fn repo(id: &str) -> RepoEntry {
        RepoEntry {
            id: id.to_string(),
            path: format!("/tmp/{id}"),
            remote: None,
            base_branch: "main".to_string(),
            checkout_mode: crate::model::CheckoutMode::Worktree,
            base_sha: None,
            feature_branch: Some("knit/venue-capacity".to_string()),
            worktree_path: None,
            head_sha: None,
        }
    }

    #[test]
    fn managed_block_is_replaced_without_touching_user_body() {
        let previous = format!("Intro\n\n{KNIT_PR_BLOCK_BEGIN}\nold\n{KNIT_PR_BLOCK_END}\n\nTail");
        let next = upsert_knit_pr_block(&previous, "new block");
        assert_eq!(next, "Intro\n\nnew block\n\nTail");
    }

    #[test]
    fn rendered_block_lists_known_and_pending_prs() {
        let mut bundle = ChangeGroup {
            schema_version: SCHEMA_VERSION.to_string(),
            kind: CHANGE_GROUP_KIND.to_string(),
            id: "venue-capacity".to_string(),
            title: "venue capacity".to_string(),
            state: Some(crate::model::BundleState::Open),
            closed_at: None,
            archived_at: None,
            deleted_at: None,
            project_id: None,
            created_at: "2026-05-05T00:00:00.000Z".to_string(),
            updated_at: "2026-05-05T00:00:00.000Z".to_string(),
            head_node_id: None,
            repos: vec![repo("backend"), repo("frontend"), repo("docs")],
            commit_groups: vec![CommitGroup {
                id: "kg_123".to_string(),
                message: "change backend and frontend".to_string(),
                created_at: "2026-05-05T00:00:00.000Z".to_string(),
                commits: vec![
                    CommitRef {
                        repo_id: "backend".to_string(),
                        sha: "abc123".to_string(),
                    },
                    CommitRef {
                        repo_id: "frontend".to_string(),
                        sha: "def456".to_string(),
                    },
                ],
                author: None,
            }],
            nodes: Vec::new(),
            publications: vec![pr_publication(
                "backend",
                123,
                "https://github.com/acme/backend/pull/123",
            )],
            work_item_ids: Vec::new(),
        };

        let block = render_knit_pr_block(&bundle, Some("backend"));
        assert!(block.contains("This PR is part of Knit bundle `venue-capacity`."));
        assert!(block.contains("`backend`: https://github.com/acme/backend/pull/123 (this PR)"));
        assert!(block.contains("`frontend`: pending"));
        assert!(!block.contains("`docs`: pending"));

        bundle.publications.push(pr_publication(
            "frontend",
            456,
            "https://github.com/acme/frontend/pull/456",
        ));
        let synced = render_knit_pr_block(&bundle, Some("backend"));
        assert!(synced.contains("`frontend`: https://github.com/acme/frontend/pull/456"));
        assert!(!synced.contains("`docs`: pending"));
    }

    #[test]
    fn publish_scope_excludes_tracked_repos_without_recorded_work() {
        let bundle = ChangeGroup {
            schema_version: SCHEMA_VERSION.to_string(),
            kind: CHANGE_GROUP_KIND.to_string(),
            id: "venue-capacity".to_string(),
            title: "venue capacity".to_string(),
            state: Some(crate::model::BundleState::Open),
            closed_at: None,
            archived_at: None,
            deleted_at: None,
            project_id: None,
            created_at: "2026-05-05T00:00:00.000Z".to_string(),
            updated_at: "2026-05-05T00:00:00.000Z".to_string(),
            head_node_id: None,
            repos: vec![repo("backend"), repo("docs")],
            commit_groups: vec![CommitGroup {
                id: "kg_123".to_string(),
                message: "change backend".to_string(),
                created_at: "2026-05-05T00:00:00.000Z".to_string(),
                commits: vec![CommitRef {
                    repo_id: "backend".to_string(),
                    sha: "abc123".to_string(),
                }],
                author: None,
            }],
            nodes: Vec::new(),
            publications: Vec::new(),
            work_item_ids: Vec::new(),
        };

        let scope = publish_scope_repo_ids(&bundle);
        assert!(scope.contains("backend"));
        assert!(!scope.contains("docs"));
    }
}
