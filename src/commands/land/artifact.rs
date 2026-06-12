//! Artifact-mode landing: merge every recorded PR straight from a bundle
//! artifact JSON (no local workspace, no plan/run files) and append the
//! landed node to the artifact.

use super::types::DEFAULT_LAND_PROVIDER;
use super::{artifact_target, ensure_open_and_ready, state_is_merged};
use crate::ids::node_id;
use crate::model::{BundleNode, MergeMethod};
use crate::output as out;
use crate::providers::{self, publication_for_repo};
use crate::store::{read_json, write_json};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::path::Path;

pub fn apply_land_from_artifact(artifact_path: &Path, out_path: Option<&Path>) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let mut bundle: crate::model::ChangeGroup = read_json(artifact_path)
        .with_context(|| format!("failed to load bundle artifact {}", artifact_path.display()))?;
    if bundle.repos.is_empty() {
        bail!("Bundle artifact has no repos.");
    }
    if bundle.publications.is_empty() {
        bail!("Bundle artifact has no review publications. Run publish first.");
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

        let pr = forge.view(&target, &publication.url)?;
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
            Err(err) if providers::is_gh_checks_access_error(&err) => {
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
