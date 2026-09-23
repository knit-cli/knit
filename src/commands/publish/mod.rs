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
    publish_repo_remote_from_artifact, push_publish_branch, report_publish_remote_result,
    report_pushed, PublishEvent, PublishJob, PublishRemoteResult, PushedInfo,
};
pub(crate) use scope::publish_scope_repo_ids;
use scope::{
    filter_indexes_by_provider, resolve_publish_destination,
    resolve_publish_destination_for_artifact, resolve_publish_repo_indexes,
    resolve_publish_repo_indexes_for_bundle, PublishDestination,
};
use std::path::Path;
use sync::{sync_publications_for_indexes, sync_publications_for_indexes_from_artifact};

// Command entry point: these arguments are the subcommand's flags.
#[allow(clippy::too_many_arguments)]
pub fn create_publications(
    selectors: &[String],
    all: bool,
    draft: bool,
    renew: bool,
    target: Option<&str>,
    lane: Option<&str>,
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
    let destination = resolve_publish_destination(&active, target, lane)?;
    let mut failures = Vec::new();
    let mut bundle_changed = false;

    let jobs = resolve_publish_jobs(&active.bundle.repos, &indexes, &destination)?;
    // Body sync follows what this run actually publishes: a repo the lane
    // excluded keeps its existing review and body untouched.
    let indexes: Vec<usize> = jobs.iter().map(|job| job.repo_index).collect();

    let total = jobs.len();
    let limit = crate::parallel::forge_jobs()?;
    if total > 1 {
        println!("{}", out::muted(publishing_header(total, limit)));
    }

    // Phase 1: every selected repo's feature branch reaches origin before any
    // review object is touched. Workers stream their steps over a channel so
    // each push is printed the moment it exists. The pool is bounded by the
    // forge limit: a hundred-repo bundle must not open a hundred simultaneous
    // writes against a host that rate-limits them.
    let (tx, rx) = std::sync::mpsc::channel();
    let pushed: Vec<(String, Result<PushedInfo>)> = std::thread::scope(|scope| {
        let active = &active;
        let sender = tx.clone();
        crate::parallel::spawn_bounded(scope, &jobs, limit, move |job| {
            let repo_id = job.repo.id.clone();
            let notes = sender.clone();
            let note_repo = repo_id.clone();
            let _notes = crate::retry::stream_notes_to(move |line| {
                let _ = notes.send(PublishEvent::Note(format!(
                    "{}: {line}",
                    out::repo(&note_repo)
                )));
            });
            let result = push_publish_branch(active, job, set_upstream);
            // The receiver outlives every worker; a send cannot fail.
            let _ = sender.send(PublishEvent::PushDone {
                repo_id,
                result: Box::new(result),
            });
        });
        drop(tx);

        let mut pushed = Vec::new();
        for event in rx {
            match event {
                PublishEvent::Note(line) => println!("{line}"),
                PublishEvent::PushDone { repo_id, result } => match *result {
                    Ok(pushed_info) => {
                        report_pushed(&repo_id, &pushed_info);
                        pushed.push((repo_id, Ok(pushed_info)));
                    }
                    Err(error) => {
                        println!("{}: {}", out::repo(&repo_id), out::danger("push failed"));
                        pushed.push((repo_id, Err(error)));
                    }
                },
                PublishEvent::Done { .. } => unreachable!("review phase event in push phase"),
            }
        }
        pushed
    });

    // Phase 2: best-effort hosted bundle sync. Publishing an open bundle
    // means branches + artifact, so the sync's own gate (every open branch on
    // origin) is now satisfiable, and its upsert teaches the bundle the
    // hosted web URL before any PR body is rendered. Failures stay
    // best-effort warnings, exactly like the final sync below.
    crate::commands::remote::maybe_sync_bundle_to_remote(
        &mut active,
        remote,
        no_remote,
        crate::commands::push::PushForce::No,
    )?;

    // Phase 3: create or adopt the review objects, using a bundle snapshot
    // refreshed after the sync recorded the hosted web URL. A repo whose
    // branch push failed is left out: its review cannot point at a branch
    // that is not on origin.
    let bundle_snapshot = active.bundle.clone();
    let create_jobs: Vec<(PublishJob, PushedInfo)> = jobs
        .into_iter()
        .filter_map(|job| {
            let repo_id = &job.repo.id;
            let pushed = pushed
                .iter()
                .find(|(pushed_id, _)| pushed_id == repo_id)
                .and_then(|(_, result)| result.as_ref().ok().cloned());
            pushed.map(|pushed| (job, pushed))
        })
        .collect();
    for (repo_id, result) in &pushed {
        if let Err(error) = result {
            failures.push(format!("{repo_id}: {error:#}"));
        }
    }
    let total = create_jobs.len();

    let (tx, rx) = std::sync::mpsc::channel();
    let outcomes: Vec<PublishRemoteResult> = std::thread::scope(|scope| {
        let active = &active;
        let bundle = &bundle_snapshot;
        let sender = tx.clone();
        crate::parallel::spawn_bounded(scope, &create_jobs, limit, move |(job, pushed)| {
            let repo_id = job.repo.id.clone();
            let notes = sender.clone();
            let note_repo = repo_id.clone();
            let _notes = crate::retry::stream_notes_to(move |line| {
                let _ = notes.send(PublishEvent::Note(format!(
                    "{}: {line}",
                    out::repo(&note_repo)
                )));
            });
            let result = publish_repo_remote(active, bundle, job, draft, renew, &pushed.sha);
            let _ = sender.send(PublishEvent::Done {
                repo_id,
                result: Box::new(result),
            });
        });
        drop(tx);

        let mut outcomes = Vec::new();
        let mut done = 0;
        for event in rx {
            match event {
                PublishEvent::Note(line) => println!("{line}"),
                PublishEvent::PushDone { .. } => unreachable!("push event in review phase"),
                PublishEvent::Done { repo_id, result } => {
                    done += 1;
                    let progress = out::progress(done, total);
                    match *result {
                        Ok(outcome) => {
                            report_publish_remote_result(&outcome, &progress);
                            outcomes.push(outcome);
                        }
                        Err(error) => {
                            println!(
                                "{}: {}{progress}",
                                out::repo(&repo_id),
                                out::danger("PR create failed")
                            );
                            failures.push(format!("{repo_id}: {error:#}"));
                        }
                    }
                }
            }
        }
        outcomes
    });

    for outcome in &outcomes {
        if apply_publish_remote_result(&mut active, outcome)? {
            bundle_changed = true;
        }
    }

    if bundle_changed {
        save_active_bundle(&active)?;
        let targets_changed = active.bundle.publications.iter().any(|publication| {
            publication_for_repo(&bundle_snapshot, &publication.repo_id)
                .is_some_and(|previous| previous.base_branch != publication.base_branch)
        });
        if targets_changed
            && active
                .root
                .join(".knit/land-plans")
                .join(format!("{}.land.json", active.bundle.id))
                .exists()
        {
            println!("{}", out::warn("PR targets changed. Regenerate and inspect the existing landing plan with `knit land plan --force` before applying it."));
        } else if renew {
            println!(
                "{}",
                out::warn(
                    "Review publications were renewed. Regenerate and inspect the landing plan with `knit land plan --force` before applying it."
                )
            );
        }
    }

    if failures.is_empty() && sync {
        failures.extend(sync_publications_for_indexes(&mut active, &indexes)?);
    } else if !sync {
        println!(
            "{}",
            out::warn("Skipped PR body sync. Run `knit publish sync` to add cross-links later.")
        );
    }

    // Final hosted bundle sync: the artifact that reaches the sync remote now
    // records this run's review objects (and the web URL) together.
    crate::commands::remote::maybe_sync_bundle_to_remote(
        &mut active,
        remote,
        no_remote,
        crate::commands::push::PushForce::No,
    )?;

    if !failures.is_empty() {
        bail!(
            "PR publishing completed with failures:\n{}\n\nre-run `knit publish create` to retry only these repos; repos that already have a review object are left alone.",
            failures.join("\n")
        );
    }

    Ok(())
}

// Command entry point: these arguments are the subcommand's flags.
#[allow(clippy::too_many_arguments)]
pub fn create_publications_from_artifact(
    artifact_path: &Path,
    out_path: Option<&Path>,
    selectors: &[String],
    all: bool,
    draft: bool,
    renew: bool,
    target: Option<&str>,
    lane: Option<&str>,
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
    let destination = resolve_publish_destination_for_artifact(target, lane)?;
    let bundle_snapshot = bundle.clone();
    let mut failures = Vec::new();

    let jobs = resolve_publish_jobs(&bundle.repos, &indexes, &destination)?;
    // Same rule as the worktree path: sync covers only what this run publishes.
    let indexes: Vec<usize> = jobs.iter().map(|job| job.repo_index).collect();

    let total = jobs.len();
    let limit = crate::parallel::forge_jobs()?;
    if total > 1 {
        println!("{}", out::muted(publishing_header(total, limit)));
    }

    // Same streaming shape and same bounded pool as the worktree path:
    // workers publish against the snapshot while the live artifact is updated
    // as each result arrives.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let cwd = cwd.as_ref();
        let snapshot = &bundle_snapshot;
        let sender = tx.clone();
        crate::parallel::spawn_bounded(scope, &jobs, limit, move |job| {
            let repo_id = job.repo.id.clone();
            let notes = sender.clone();
            let note_repo = repo_id.clone();
            let _notes = crate::retry::stream_notes_to(move |line| {
                let _ = notes.send(ArtifactPublishEvent::Note(format!(
                    "{}: {line}",
                    out::repo(&note_repo)
                )));
            });
            let result = publish_repo_remote_from_artifact(cwd, snapshot, job, draft, renew);
            // The receiver outlives every worker; a send cannot fail.
            let _ = sender.send(ArtifactPublishEvent::Done {
                repo_id,
                result: Box::new(result),
            });
        });
        drop(tx);

        let mut done = 0;
        for event in rx {
            match event {
                ArtifactPublishEvent::Note(line) => println!("{line}"),
                ArtifactPublishEvent::Done { repo_id, result } => {
                    done += 1;
                    let progress = out::progress(done, total);
                    match *result {
                        Ok(outcome) => {
                            apply_artifact_publish_result(&mut bundle, &outcome, &progress)
                        }
                        Err(error) => {
                            println!(
                                "{}: {}{progress}",
                                out::repo(&repo_id),
                                out::danger("PR create failed")
                            );
                            failures.push(format!("{repo_id}: {error:#}"));
                        }
                    }
                }
            }
        }
    });

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
            "PR publishing completed with failures:\n{}\n\nre-run `knit publish create` to retry only these repos; repos that already have a review object are left alone.",
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

/// Build the per-repo publish jobs for the selected indexes against the
/// resolved destination. Repos the lane declares absent are dropped here with
/// a note, and any repo the lane fails to map is an error — both decided
/// before any push or review-object write happens.
fn resolve_publish_jobs(
    repos: &[crate::model::RepoEntry],
    indexes: &[usize],
    destination: &PublishDestination,
) -> Result<Vec<PublishJob>> {
    let mut jobs = Vec::new();
    let mut excluded = Vec::new();
    for &index in indexes {
        let repo = repos[index].clone();
        match destination.branch_for(&repo)? {
            Some(base_branch) => jobs.push(PublishJob {
                repo_index: index,
                repo,
                base_branch,
            }),
            None => excluded.push(repo.id.clone()),
        }
    }
    if let Some(name) = destination.lane_name() {
        for repo_id in &excluded {
            println!(
                "{} {} {}",
                out::muted(format!("not in lane `{name}`:")),
                out::repo(repo_id),
                out::muted("skipped, declared absent from the lane")
            );
        }
        if jobs.is_empty() {
            bail!("Landing lane `{name}` carries none of the selected repositories.");
        }
    }
    Ok(jobs)
}

/// Header for a multi-repo publish. The concurrency limit is named only when
/// it actually bounds the run, so the everyday three-repo bundle stays quiet.
fn publishing_header(total: usize, limit: usize) -> String {
    if total > limit {
        format!("publishing {total} repo(s), {limit} at a time…")
    } else {
        format!("publishing {total} repo(s)…")
    }
}

/// What an artifact-mode publish worker reports. The worktree path has richer
/// steps (see `remote::PublishEvent`); this one only pushes review objects.
enum ArtifactPublishEvent {
    Note(String),
    Done {
        repo_id: String,
        result: Box<Result<remote::ArtifactPublishResult>>,
    },
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
        hosted_bundle_links, initial_pr_body, render_knit_pr_block, upsert_knit_pr_block,
        KNIT_PR_BLOCK_BEGIN, KNIT_PR_BLOCK_BEGIN_REFS, KNIT_PR_BLOCK_END, KNIT_PR_BLOCK_END_REFS,
    };
    use super::scope::publish_scope_repo_ids;
    use super::*;
    use crate::model::RepoEntry;
    use crate::model::{
        BundleSyncTarget, CommitGroup, CommitRef, PublicationEntry, CHANGE_GROUP_KIND,
        SCHEMA_VERSION,
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
            author: None,
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

    /// One bundle with recorded backend work and a backend publication: the
    /// shape every managed-block rendering test below starts from.
    fn published_bundle() -> ChangeGroup {
        ChangeGroup {
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
            sync_targets: Vec::new(),
            work_item_ids: Vec::new(),
        }
    }

    #[test]
    fn rendered_block_lists_known_and_pending_prs() {
        let mut bundle = published_bundle();

        let block = render_knit_pr_block(&bundle, Some("backend"), "github");
        assert!(block.contains("This PR is part of Knit bundle `venue-capacity`."));
        assert!(block.contains("`backend`: https://github.com/acme/backend/pull/123 (this PR)"));
        assert!(block.contains("`frontend`: pending"));
        assert!(!block.contains("`docs`: pending"));

        bundle.publications.push(pr_publication(
            "frontend",
            456,
            "https://github.com/acme/frontend/pull/456",
        ));
        let synced = render_knit_pr_block(&bundle, Some("backend"), "github");
        assert!(synced.contains("`frontend`: https://github.com/acme/frontend/pull/456"));
        assert!(!synced.contains("`docs`: pending"));
    }

    #[test]
    fn bitbucket_body_is_fenced_with_invisible_reference_definitions() {
        let body = initial_pr_body(&published_bundle(), "backend", "bitbucket");
        assert!(
            body.starts_with(&format!("{KNIT_PR_BLOCK_BEGIN_REFS}\n\n## Knit Bundle")),
            "{body}"
        );
        assert!(body.ends_with(&format!("\n\n{KNIT_PR_BLOCK_END_REFS}")));
        assert!(!body.contains("<!--"));
        assert!(body.contains("This PR is part of Knit bundle `venue-capacity`."));
    }

    #[test]
    fn non_bitbucket_providers_keep_html_comment_delimiters() {
        for provider in ["github", "gitlab", "forgejo"] {
            let block = render_knit_pr_block(&published_bundle(), Some("backend"), provider);
            assert!(
                block.starts_with(&format!("{KNIT_PR_BLOCK_BEGIN}\n## Knit Bundle")),
                "{provider}"
            );
            assert!(
                block.ends_with(&format!(
                    "Bundle title: venue capacity\n{KNIT_PR_BLOCK_END}"
                )),
                "{provider}"
            );
            assert!(!block.contains(KNIT_PR_BLOCK_BEGIN_REFS), "{provider}");
        }
    }

    #[test]
    fn sync_migrates_a_legacy_html_block_to_bitbucket_references() {
        let block = render_knit_pr_block(&published_bundle(), Some("backend"), "bitbucket");
        let legacy = format!(
            "Intro\n\n{KNIT_PR_BLOCK_BEGIN}\n## Knit Bundle\n\nstale\n{KNIT_PR_BLOCK_END}\n\nTail"
        );

        let migrated = upsert_knit_pr_block(&legacy, &block);
        assert_eq!(migrated, format!("Intro\n\n{block}\n\nTail"));
        assert!(!migrated.contains("<!--"));

        // A repeated sync finds the migrated pair and changes nothing.
        assert_eq!(upsert_knit_pr_block(&migrated, &block), migrated);
    }

    #[test]
    fn upsert_preserves_surrounding_text_with_its_spacing() {
        let block = render_knit_pr_block(&published_bundle(), Some("backend"), "bitbucket");
        let previous = format!(
            "Intro\n\n{KNIT_PR_BLOCK_BEGIN_REFS}\n\nold\n\n{KNIT_PR_BLOCK_END_REFS}\n\nTail  "
        );
        assert_eq!(
            upsert_knit_pr_block(&previous, &block),
            format!("Intro\n\n{block}\n\nTail  ")
        );
    }

    #[test]
    fn unmatched_markers_of_mixed_generations_never_eat_user_text() {
        // An HTML begin closed by a reference end is no pair at all: the body
        // is kept verbatim and the block is appended instead.
        let previous =
            format!("Intro\n\n{KNIT_PR_BLOCK_BEGIN}\nstray text\n{KNIT_PR_BLOCK_END_REFS}\n\nTail");
        assert_eq!(
            upsert_knit_pr_block(&previous, "new block"),
            format!("{previous}\n\nnew block")
        );
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
            sync_targets: Vec::new(),
            work_item_ids: Vec::new(),
        };

        let scope = publish_scope_repo_ids(&bundle);
        assert!(scope.contains("backend"));
        assert!(!scope.contains("docs"));
    }

    /// The bundle fixture plus one hosted sync target whose server reported a
    /// canonical web URL.
    fn hosted_bundle(url: &str) -> ChangeGroup {
        let mut bundle = published_bundle();
        bundle.sync_targets.push(BundleSyncTarget {
            remote: "hosted".to_string(),
            bundle_id: "rb-venue-capacity".to_string(),
            api_url: "https://sync.example.test".to_string(),
            artifact_hash: None,
            web_url: Some(url.to_string()),
        });
        bundle
    }

    #[test]
    fn hosted_link_leads_the_managed_block() {
        let bundle = hosted_bundle("https://app.example.test/bundles/rb-venue-capacity");
        let block = render_knit_pr_block(&bundle, Some("backend"), "github");
        let content = block
            .strip_prefix(KNIT_PR_BLOCK_BEGIN)
            .unwrap()
            .strip_suffix(KNIT_PR_BLOCK_END)
            .unwrap()
            .trim_start_matches('\n');
        assert!(
            content.starts_with(
                "[View bundle](https://app.example.test/bundles/rb-venue-capacity)\n\n## Knit Bundle"
            ),
            "{content}"
        );
        // The link is the first visible content of a fresh PR body too.
        let body = initial_pr_body(&bundle, "backend", "github");
        assert!(
            body.starts_with(&format!(
                "{KNIT_PR_BLOCK_BEGIN}\n[View bundle](https://app.example.test/bundles/rb-venue-capacity)"
            )),
            "{body}"
        );
    }

    #[test]
    fn hosted_links_are_deduped_sorted_and_invalid_omitted() {
        let mut bundle = hosted_bundle("https://b.example.test/bundles/1");
        bundle.sync_targets.push(BundleSyncTarget {
            remote: "mirror".to_string(),
            bundle_id: "rb-2".to_string(),
            api_url: "https://mirror.example.test".to_string(),
            artifact_hash: None,
            web_url: Some("https://a.example.test/bundles/2".to_string()),
        });
        // A duplicate of an existing URL, plus unusable values a server could
        // send: wrong scheme, credentials, garbage, and an empty host.
        bundle.sync_targets.push(BundleSyncTarget {
            remote: "dup".to_string(),
            bundle_id: "rb-3".to_string(),
            api_url: "https://dup.example.test".to_string(),
            artifact_hash: None,
            web_url: Some("https://b.example.test/bundles/1".to_string()),
        });
        bundle.sync_targets.push(BundleSyncTarget {
            remote: "ftp".to_string(),
            bundle_id: "rb-4".to_string(),
            api_url: "https://ftp.example.test".to_string(),
            artifact_hash: None,
            web_url: Some("ftp://app.example.test/bundles/4".to_string()),
        });
        bundle.sync_targets.push(BundleSyncTarget {
            remote: "creds".to_string(),
            bundle_id: "rb-5".to_string(),
            api_url: "https://creds.example.test".to_string(),
            artifact_hash: None,
            web_url: Some("https://user:pass@app.example.test/bundles/5".to_string()),
        });
        bundle.sync_targets.push(BundleSyncTarget {
            remote: "junk".to_string(),
            bundle_id: "rb-6".to_string(),
            api_url: "https://junk.example.test".to_string(),
            artifact_hash: None,
            web_url: Some("not a url".to_string()),
        });
        bundle.sync_targets.push(BundleSyncTarget {
            remote: "bare".to_string(),
            bundle_id: "rb-7".to_string(),
            api_url: "https://bare.example.test".to_string(),
            artifact_hash: None,
            web_url: Some("https://".to_string()),
        });
        bundle.sync_targets.push(BundleSyncTarget {
            remote: "none".to_string(),
            bundle_id: "rb-8".to_string(),
            api_url: "https://none.example.test".to_string(),
            artifact_hash: None,
            web_url: None,
        });

        assert_eq!(
            hosted_bundle_links(&bundle),
            vec![
                "https://a.example.test/bundles/2".to_string(),
                "https://b.example.test/bundles/1".to_string(),
            ]
        );
    }

    #[test]
    fn hosted_link_destinations_are_normalized_and_escaped() {
        // Normalization: scheme and host casing, and a space in the path.
        assert_eq!(
            hosted_bundle_links(&hosted_bundle("HTTPS://APP.EXAMPLE.TEST/bundles/a b"))
                .first()
                .map(String::as_str),
            Some("https://app.example.test/bundles/a%20b")
        );
        // Parentheses would reshape an inline destination: angle brackets.
        assert_eq!(
            hosted_bundle_links(&hosted_bundle("https://app.example.test/bundles/(1)"))
                .first()
                .map(String::as_str),
            Some("<https://app.example.test/bundles/(1)>")
        );
        // IPv6 authorities survive, angle-bracketed so the brackets stay
        // part of the destination.
        assert_eq!(
            hosted_bundle_links(&hosted_bundle("http://[::1]:8080/bundles/1"))
                .first()
                .map(String::as_str),
            Some("<http://[::1]:8080/bundles/1>")
        );
        // Control characters never reach a body: URL normalization strips
        // them out of the destination entirely.
        let sanitized = hosted_bundle_links(&hosted_bundle("https://app.example.test/bundles/1\t"));
        assert_eq!(
            sanitized.first().map(String::as_str),
            Some("https://app.example.test/bundles/1")
        );
        assert!(!sanitized[0].contains('\t'));
    }

    #[test]
    fn upsert_moves_the_linked_block_above_user_prose_in_order() {
        let bundle = hosted_bundle("https://app.example.test/bundles/rb-venue-capacity");
        let linked = render_knit_pr_block(&bundle, Some("backend"), "github");
        let plain = render_knit_pr_block(&published_bundle(), Some("backend"), "github");

        // Without a hosted link the placement is untouched, as before.
        let previous = format!("Intro\n\n{plain}\n\nTail");
        assert_eq!(
            upsert_knit_pr_block(&previous, &plain),
            format!("Intro\n\n{plain}\n\nTail")
        );

        // With one, the block leads and the surrounding prose keeps its
        // original order after it.
        assert_eq!(
            upsert_knit_pr_block(&previous, &linked),
            format!("{linked}\n\nIntro\n\nTail")
        );
    }

    #[test]
    fn repeated_sync_with_a_hosted_link_is_idempotent_and_replaces_changed_urls() {
        let bundle = hosted_bundle("https://app.example.test/bundles/rb-venue-capacity");
        let linked = render_knit_pr_block(&bundle, Some("backend"), "github");
        // The body a pre-link era wrote: the plain block after user prose.
        let plain = render_knit_pr_block(&published_bundle(), Some("backend"), "github");
        let body = format!("Intro\n\n{plain}\n\nTail");

        let once = upsert_knit_pr_block(&body, &linked);
        assert_eq!(once, format!("{linked}\n\nIntro\n\nTail"));
        // The next sync finds the block where it now lives and is a no-op.
        assert_eq!(upsert_knit_pr_block(&once, &linked), once);

        // The host moves: the new URL replaces the old one, still exactly
        // once, and the block stays on top.
        let moved = render_knit_pr_block(
            &hosted_bundle("https://next.example.test/bundles/rb-venue-capacity"),
            Some("backend"),
            "github",
        );
        let twice = upsert_knit_pr_block(&once, &moved);
        assert_eq!(twice, format!("{moved}\n\nIntro\n\nTail"));
        assert_eq!(twice.matches("[View bundle](").count(), 1);
        assert!(!twice.contains("app.example.test"));
    }

    #[test]
    fn new_linked_block_prepends_verbatim_prose_with_trailing_whitespace() {
        let bundle = hosted_bundle("https://app.example.test/bundles/rb-venue-capacity");
        let linked = render_knit_pr_block(&bundle, Some("backend"), "github");

        // No managed block yet, user prose with trailing spaces and newline:
        // the block moves in front and the prose is kept verbatim.
        let previous = "My own write-up  \n";
        assert_eq!(
            upsert_knit_pr_block(previous, &linked),
            format!("{linked}\n\nMy own write-up  \n")
        );

        // Without a hosted link the historical append placement holds.
        let plain = render_knit_pr_block(&published_bundle(), Some("backend"), "github");
        assert_eq!(
            upsert_knit_pr_block(previous, &plain),
            format!("My own write-up\n\n{plain}")
        );
    }

    #[test]
    fn bitbucket_linked_block_stays_ref_fenced_with_the_link_first_visible() {
        let bundle = hosted_bundle("https://app.example.test/bundles/rb-venue-capacity");
        let block = render_knit_pr_block(&bundle, Some("backend"), "bitbucket");
        assert!(
            block.starts_with(&format!(
                "{KNIT_PR_BLOCK_BEGIN_REFS}\n\n[View bundle](https://app.example.test/bundles/rb-venue-capacity)\n\n## Knit Bundle"
            )),
            "{block}"
        );
        assert!(block.ends_with(&format!("\n\n{KNIT_PR_BLOCK_END_REFS}")));
        assert!(!block.contains("<!--"));

        // A legacy HTML-commented body migrates to the ref-fenced shape and
        // the link lands on top.
        let legacy = format!(
            "Intro\n\n{KNIT_PR_BLOCK_BEGIN}\n## Knit Bundle\n\nstale\n{KNIT_PR_BLOCK_END}\n\nTail"
        );
        let migrated = upsert_knit_pr_block(&legacy, &block);
        assert_eq!(migrated, format!("{block}\n\nIntro\n\nTail"));
        assert!(!migrated.contains("<!--"));
        assert_eq!(upsert_knit_pr_block(&migrated, &block), migrated);
    }

    #[test]
    fn a_stray_label_deeper_in_the_block_does_not_relocate_it() {
        // A crafted block whose first content line is a heading and whose
        // label appears later must not count as leading with the hosted link.
        let stray = format!("{KNIT_PR_BLOCK_BEGIN}\n## Knit Bundle\n\n[View bundle](https://app.example.test/bundles/1)\n{KNIT_PR_BLOCK_END}");
        let previous = format!("Intro\n\n{stray}");
        assert_eq!(
            upsert_knit_pr_block(&previous, "replacement"),
            "Intro\n\nreplacement"
        );
    }
}
