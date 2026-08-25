//! Artifact-mode landing: land a bundle straight from its artifact JSON (no
//! local workspace, no plan/run files) and append the landed node to the
//! artifact. A terminal landing merges the recorded reviews; an intermediate
//! one merges the feature branches on the host and leaves the reviews open.

use super::types::DEFAULT_LAND_PROVIDER;
use super::{
    artifact_target, ensure_open_and_ready, ensure_open_for_retarget, normalize_lane_name,
    normalize_target_branch, state_is_merged,
};
use crate::ids::node_id;
use crate::model::{BundleNode, MergeMethod};
use crate::output as out;
use crate::providers::{self, publication_for_repo};
use crate::store::{read_json, write_json};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub fn apply_land_from_artifact(
    artifact_path: &Path,
    out_path: Option<&Path>,
    target_branch: Option<&str>,
    lane_name: Option<&str>,
    repo_targets: &[String],
    repo_absent: &[String],
    declared_terminal: Option<bool>,
) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let target_branch = normalize_target_branch(target_branch)?;
    let lane_name = normalize_lane_name(lane_name)?;
    let repo_targets = parse_repo_targets(repo_targets)?;
    let repo_absent = parse_repo_absent(repo_absent)?;
    if lane_name.is_none() && !repo_targets.is_empty() {
        bail!("--repo-target requires --lane");
    }
    if lane_name.is_none() && !repo_absent.is_empty() {
        bail!("--repo-absent requires --lane");
    }
    if let Some(repo_id) = repo_absent.iter().find(|id| repo_targets.contains_key(*id)) {
        bail!(
            "Repository `{repo_id}` is given both a lane branch and declared absent from the lane. One of the two is a mistake."
        );
    }
    // The bundle's last stop has to carry every repository, or the skipped
    // ones keep an open review after the bundle is archived.
    if declared_terminal == Some(true) && !repo_absent.is_empty() {
        bail!(
            "This landing is declared terminal but skips {}. A bundle's last stop has to carry every repository, or those reviews stay open after it is archived.",
            repo_absent
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let mut bundle: crate::model::ChangeGroup = read_json(artifact_path)
        .with_context(|| format!("failed to load bundle artifact {}", artifact_path.display()))?;
    if bundle.repos.is_empty() {
        bail!("Bundle artifact has no repos.");
    }
    if bundle.publications.is_empty() && lane_name.is_none() {
        bail!("Bundle artifact has no review publications. Run publish first.");
    }
    // The repositories with recorded work or a review: what a lane moves.
    let changed_repo_ids = crate::commands::publish::publish_scope_repo_ids(&bundle);
    if lane_name.is_some() {
        let published_repo_ids = bundle
            .repos
            .iter()
            .filter(|repo| changed_repo_ids.contains(&repo.id))
            .map(|repo| repo.id.as_str())
            .collect::<BTreeSet<_>>();
        if published_repo_ids.is_empty() {
            bail!("Bundle artifact records no changed repositories to land into a lane.");
        }
        for repo_id in repo_targets.keys() {
            if !published_repo_ids.contains(repo_id.as_str()) {
                bail!("--repo-target names unpublished or unknown repository `{repo_id}`");
            }
        }
        for repo_id in &repo_absent {
            if !published_repo_ids.contains(repo_id.as_str()) {
                bail!("--repo-absent names unpublished or unknown repository `{repo_id}`");
            }
        }
        for repo_id in &published_repo_ids {
            if !repo_targets.contains_key(*repo_id) && !repo_absent.contains(*repo_id) {
                bail!(
                    "Landing lane `{}` has no resolved artifact target for repository `{repo_id}`. Pass `--repo-target {repo_id}=BRANCH`, or `--repo-absent {repo_id}` if it has no {} environment.",
                    lane_name.as_deref().unwrap_or_default(),
                    lane_name.as_deref().unwrap_or_default()
                );
            }
        }
        if published_repo_ids
            .iter()
            .all(|repo_id| repo_absent.contains(*repo_id))
        {
            bail!(
                "Landing lane `{}` carries none of this bundle's published repositories; they are all declared absent, so there is nothing to land there.",
                lane_name.as_deref().unwrap_or_default()
            );
        }
    }

    // Decided before anything moves, because it decides what moves: a
    // terminal landing merges the reviews, an intermediate lane merges the
    // feature branches and leaves the reviews open. A caller with project
    // metadata (a trusted host resolving a lane) states the answer; otherwise
    // judge it the way a local plan does, by whether every repository is
    // headed for its own configured base.
    let terminal = declared_terminal.unwrap_or_else(|| {
        // Same rule as a local plan: a destination this bundle's work does not
        // all reach cannot be where that work ends.
        if !repo_absent.is_empty() {
            return false;
        }
        // Judge the repositories that will actually move. A lane carries the
        // changed repositories whether or not they have a review; the other
        // destinations merge reviews, so only reviewed repositories count.
        let judged = bundle
            .repos
            .iter()
            .filter(|repo| {
                if lane_name.is_some() {
                    changed_repo_ids.contains(&repo.id)
                } else {
                    publication_for_repo(&bundle, &repo.id).is_some()
                }
            })
            .filter(|repo| !repo_absent.contains(&repo.id))
            .collect::<Vec<_>>();
        // Nothing to judge is not "everything reaches its base": an empty
        // landing cannot be the bundle's last stop.
        if judged.is_empty() {
            return false;
        }
        judged.iter().all(|repo| {
            let destination = repo_targets
                .get(&repo.id)
                .map(String::as_str)
                .or(target_branch.as_deref())
                .or_else(|| {
                    publication_for_repo(&bundle, &repo.id).map(|pub_| pub_.base_branch.as_str())
                });
            destination == Some(repo.base_branch.as_str())
        })
    });
    // Mirrors the local plan: an intermediate explicit destination, lane or
    // raw target, is reached by merging the feature branches; the terminal
    // destination merges the reviews. Without either, each review is merged
    // where it already points.
    let branch_merges = !terminal && (lane_name.is_some() || target_branch.is_some());

    let started_at = now_iso();
    let mut merged_repo_ids = Vec::new();
    let mut publication_urls = Vec::new();

    let repos = bundle.repos.clone();

    for repo in &repos {
        let publication = publication_for_repo(&bundle, &repo.id).cloned();
        // Review merges need a review. Branch merges need recorded work, which
        // is the same scope a local lane landing uses.
        if branch_merges {
            if !changed_repo_ids.contains(&repo.id) {
                continue;
            }
        } else if publication.is_none() {
            continue;
        }
        let forge = providers::for_repo(repo)?;
        let target = artifact_target(&cwd, forge.as_ref(), repo)?;

        if repo_absent.contains(&repo.id) {
            println!(
                "{} {} {}",
                out::muted("not in this lane"),
                out::repo(&repo.id),
                out::muted("keeps its work for the terminal landing")
            );
            continue;
        }

        if branch_merges {
            merge_feature_branch_into_destination(
                forge.as_ref(),
                &target,
                repo,
                publication.as_ref(),
                repo_targets
                    .get(&repo.id)
                    .map(String::as_str)
                    .or(target_branch.as_deref()),
                lane_name.as_deref(),
            )?;
            merged_repo_ids.push(repo.id.clone());
            if let Some(publication) = &publication {
                publication_urls.push(publication.url.clone());
            }
            continue;
        }

        let publication = publication.expect("review merges are skipped without a publication");
        let mut pr = forge.view(&target, &publication.url)?;
        let repo_target = repo_targets
            .get(&repo.id)
            .map(String::as_str)
            .or(target_branch.as_deref());
        if let Some(target_branch) = repo_target {
            let current_base = pr
                .base_ref_name
                .as_deref()
                .unwrap_or(&publication.base_branch)
                .to_string();
            if current_base != target_branch {
                if state_is_merged(&pr) {
                    bail!(
                        "{}: PR #{} already merged into `{current_base}` and cannot be landed into `{target_branch}`.",
                        repo.id,
                        pr.number
                    );
                }
                ensure_open_for_retarget(&repo.id, &pr)?;
                forge
                    .edit_base(&target, &publication.url, target_branch)
                    .with_context(|| {
                        format!(
                            "{}: failed to retarget PR #{} from `{current_base}` to `{target_branch}`",
                            repo.id, pr.number
                        )
                    })?;
                pr = forge.view(&target, &publication.url)?;
                if pr.base_ref_name.as_deref() != Some(target_branch) {
                    bail!(
                        "{}: provider did not retarget PR #{} to `{target_branch}`",
                        repo.id,
                        pr.number
                    );
                }
                providers::upsert_publication(&mut bundle, repo, forge.as_ref(), &pr);
                println!(
                    "{} {} PR #{} {} -> {}",
                    out::ok("retargeted"),
                    out::repo(&repo.id),
                    pr.number,
                    out::branch(&current_base),
                    out::branch(target_branch)
                );
            }
        }
        if state_is_merged(&pr) {
            providers::upsert_publication(&mut bundle, repo, forge.as_ref(), &pr);
            merged_repo_ids.push(repo.id.clone());
            publication_urls.push(publication.url.clone());
            println!(
                "{} {} {}",
                out::ok("already merged"),
                out::repo(&repo.id),
                out::muted(&publication.url)
            );
            continue;
        }

        ensure_open_and_ready(&repo.id, &pr)?;

        let checks_detail = match forge.wait_for_checks(&target, &publication.url, true, 1800, 10) {
            Ok(summary) => summary.status,
            Err(err) if forge.id() == "github" && providers::is_gh_checks_access_error(&err) => {
                "passed (checks unavailable)".to_string()
            }
            Err(err) => return Err(err),
        };
        println!(
            "{} {} {}",
            out::ok("checks"),
            out::repo(&repo.id),
            out::muted(&checks_detail)
        );

        forge
            .merge(
                &target,
                &publication.url,
                MergeMethod::default().as_str(),
                false,
                pr.head_ref_oid.as_deref(),
            )
            .with_context(|| format!("{}: merging {}", repo.id, publication.url))?;

        let refreshed = forge.view(&target, &publication.url)?;
        providers::upsert_publication(&mut bundle, repo, forge.as_ref(), &refreshed);
        merged_repo_ids.push(repo.id.clone());
        publication_urls.push(publication.url.clone());
        println!(
            "{} {} {}",
            out::ok("merged"),
            out::repo(&repo.id),
            out::muted(&publication.url)
        );
    }

    // Record a landed node in the artifact without writing land plan/run files.
    let node = BundleNode::feature_landed(
        node_id("land"),
        started_at,
        format!("land-{}", bundle.id),
        format!("run-artifact-{}", bundle.id),
        DEFAULT_LAND_PROVIDER.to_string(),
        merged_repo_ids,
        publication_urls,
        Some(crate::model::NodeLanding {
            terminal,
            lane: lane_name.clone(),
            target_branch: target_branch.clone(),
        }),
    );
    bundle.nodes.push(node);
    bundle.head_node_id = bundle.nodes.last().map(|node| node.id.clone());
    bundle.updated_at = now_iso();

    match out_path {
        Some(path) => write_json(path, &bundle),
        None => {
            let json =
                serde_json::to_string_pretty(&bundle).context("failed to encode bundle JSON")?;
            println!("{json}");
            Ok(())
        }
    }
}

/// Send one repository's feature branch into its destination branch on the
/// host — the lane's branch for it, or the one raw target — leaving its
/// review untouched. The review-base guard is the same rule the local plan
/// enforces, re-checked here because a review can be retargeted onto the
/// destination after the host resolved it.
/// `publication` is optional on purpose: reaching an environment is a branch
/// merge, which does not need a review. It is consulted only to refuse a
/// landing that would merge a feature branch into its own review's base, and a
/// repository with no review has none to spend.
fn merge_feature_branch_into_destination(
    forge: &dyn providers::Forge,
    target: &providers::PrTarget,
    repo: &crate::model::RepoEntry,
    publication: Option<&crate::model::PublicationEntry>,
    destination: Option<&str>,
    lane_name: Option<&str>,
) -> Result<()> {
    let destination = destination.with_context(|| {
        format!(
            "{}: this landing has no resolved destination branch for this repository",
            repo.id
        )
    })?;
    let feature_branch = repo.feature_branch.as_deref().with_context(|| {
        format!(
            "{}: the bundle artifact records no feature branch to merge into `{destination}`",
            repo.id
        )
    })?;

    if let Some(publication) = publication {
        let pr = forge.view(target, &publication.url)?;
        let live_base = pr
            .base_ref_name
            .as_deref()
            .unwrap_or(&publication.base_branch);
        if live_base == destination {
            let (landing, way_out) = match lane_name {
                Some(lane) => (
                    format!(
                        "Landing lane `{lane}` sends `{}` to `{destination}`, which is",
                        repo.id
                    ),
                    format!(
                        "Point the lane at a different branch for `{}`, or declare the lane terminal so Knit merges the review itself.",
                        repo.id
                    ),
                ),
                None => (
                    format!("Landing into `{destination}` sends `{}` to", repo.id),
                    "Land into a different branch, or declare this landing terminal so Knit merges the review itself.".to_string(),
                ),
            };
            bail!(
                "{landing} the base of its recorded review {}. Merging the feature branch there would put the review's own commits into its base and the forge would close it as merged, so this landing cannot leave the review open. {way_out}",
                publication.url
            );
        }
    }

    let status = forge
        .merge_branch(target, destination, feature_branch)
        .with_context(|| format!("{}: merging {feature_branch} into {destination}", repo.id))?;
    match status {
        providers::BranchMergeStatus::Merged => println!(
            "{} {} {} -> {}",
            out::ok("merged"),
            out::repo(&repo.id),
            out::branch(feature_branch),
            out::branch(destination)
        ),
        providers::BranchMergeStatus::AlreadyContained => println!(
            "{} {} {}",
            out::ok("already there"),
            out::repo(&repo.id),
            out::muted(format!("{destination} already contains {feature_branch}"))
        ),
    }
    Ok(())
}

fn parse_repo_absent(values: &[String]) -> Result<BTreeSet<String>> {
    let mut absent = BTreeSet::new();
    for value in values {
        let repo_id = value.trim();
        if repo_id.is_empty() {
            bail!("Invalid --repo-absent `{value}`; repository must be non-empty");
        }
        if !absent.insert(repo_id.to_string()) {
            bail!("Duplicate --repo-absent for repository `{repo_id}`");
        }
    }
    Ok(absent)
}

fn parse_repo_targets(values: &[String]) -> Result<BTreeMap<String, String>> {
    let mut targets = BTreeMap::new();
    for value in values {
        let (repo_id, branch) = value.split_once('=').ok_or_else(|| {
            anyhow::anyhow!("Invalid --repo-target `{value}`; expected REPO=BRANCH")
        })?;
        let repo_id = repo_id.trim();
        let branch = branch.trim();
        if repo_id.is_empty() || branch.is_empty() {
            bail!("Invalid --repo-target `{value}`; repository and branch must be non-empty");
        }
        if targets
            .insert(repo_id.to_string(), branch.to_string())
            .is_some()
        {
            bail!("Duplicate --repo-target for repository `{repo_id}`");
        }
    }
    Ok(targets)
}
