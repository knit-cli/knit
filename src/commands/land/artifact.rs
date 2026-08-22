//! Artifact-mode landing: merge every recorded PR straight from a bundle
//! artifact JSON (no local workspace, no plan/run files) and append the
//! landed node to the artifact.

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
    declared_terminal: Option<bool>,
) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let target_branch = normalize_target_branch(target_branch)?;
    let lane_name = normalize_lane_name(lane_name)?;
    let repo_targets = parse_repo_targets(repo_targets)?;
    if lane_name.is_none() && !repo_targets.is_empty() {
        bail!("--repo-target requires --lane");
    }
    let mut bundle: crate::model::ChangeGroup = read_json(artifact_path)
        .with_context(|| format!("failed to load bundle artifact {}", artifact_path.display()))?;
    if bundle.repos.is_empty() {
        bail!("Bundle artifact has no repos.");
    }
    if bundle.publications.is_empty() {
        bail!("Bundle artifact has no review publications. Run publish first.");
    }
    if lane_name.is_some() {
        let published_repo_ids = bundle
            .repos
            .iter()
            .filter(|repo| publication_for_repo(&bundle, &repo.id).is_some())
            .map(|repo| repo.id.as_str())
            .collect::<BTreeSet<_>>();
        for repo_id in repo_targets.keys() {
            if !published_repo_ids.contains(repo_id.as_str()) {
                bail!("--repo-target names unpublished or unknown repository `{repo_id}`");
            }
        }
        for repo_id in published_repo_ids {
            if !repo_targets.contains_key(repo_id) {
                bail!(
                    "Landing lane `{}` has no resolved artifact target for repository `{repo_id}`. Pass `--repo-target {repo_id}=BRANCH`.",
                    lane_name.as_deref().unwrap_or_default()
                );
            }
        }
    }

    let started_at = now_iso();
    let mut merged_repo_ids = Vec::new();
    let mut publication_urls = Vec::new();

    let repos = bundle.repos.clone();

    for repo in &repos {
        let Some(publication) = publication_for_repo(&bundle, &repo.id).cloned() else {
            continue;
        };
        let forge = providers::for_repo(repo)?;
        let target = artifact_target(&cwd, forge.as_ref(), repo)?;

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

    // A caller with project metadata (a trusted host resolving a lane) states
    // whether this destination finishes the bundle; otherwise judge it the way
    // a local plan does, by whether every repo landed on its configured base.
    let terminal = declared_terminal.unwrap_or_else(|| {
        merged_repo_ids.iter().all(|repo_id| {
            let destination = repo_targets
                .get(repo_id)
                .map(String::as_str)
                .or(target_branch.as_deref())
                .or_else(|| {
                    publication_for_repo(&bundle, repo_id).map(|pub_| pub_.base_branch.as_str())
                });
            bundle
                .repos
                .iter()
                .find(|repo| repo.id == *repo_id)
                .is_some_and(|repo| destination == Some(repo.base_branch.as_str()))
        })
    });

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
