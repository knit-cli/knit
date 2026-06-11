//! Builds the default land plan from the resolved bundle and its project's
//! landing template: one merge step per recorded PR plus any project deployments.

use super::{
    ensure_provider, LandCheckout, LandPlan, LandStep, LandStepKind, DEFAULT_LAND_PROVIDER,
    LAND_PLAN_KIND,
};
use crate::model::{
    DeployMode, KnitProject, MergeMethod, ProjectLandingMergePlan, ProjectLandingPlan, RepoEntry,
    SCHEMA_VERSION,
};
use crate::providers::publication_for_repo;
use crate::store::{load_config, project_path, read_json, ActiveBundle};
use crate::time::now_iso;
use anyhow::{bail, Result};
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn build_default_plan(
    active: &ActiveBundle,
    requested_provider: Option<&str>,
) -> Result<LandPlan> {
    let project = load_project_for_bundle(active)?;
    let landing = project
        .as_ref()
        .and_then(|project| project.landing.as_ref());
    let provider = requested_provider
        .or_else(|| landing.and_then(|landing| landing.provider.as_deref()))
        .unwrap_or(DEFAULT_LAND_PROVIDER)
        .to_string();
    ensure_provider(&provider)?;
    let merge = landing.map(|landing| &landing.merge);
    let mut steps = Vec::new();
    let ordered_ids: BTreeSet<String> = merge
        .map(|m| m.repo_order.iter().cloned().collect())
        .unwrap_or_default();
    let empty_needs = BTreeMap::new();
    let merge_needs = merge.map(|m| &m.needs).unwrap_or(&empty_needs);
    let mut previous_ordered: Option<String> = None;
    for repo in ordered_merge_repos(active, merge) {
        if publication_for_repo(&active.bundle, &repo.id).is_none() {
            continue;
        }
        let id = format!("merge-{}", repo.id);
        let needs = if let Some(explicit_needs) = merge_needs.get(&repo.id) {
            explicit_needs.clone()
        } else if ordered_ids.contains(&repo.id) {
            previous_ordered.iter().cloned().collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        steps.push(LandStep {
            id: id.clone(),
            step_type: LandStepKind::MergePr,
            needs,
            repo_id: Some(repo.id.clone()),
            method: Some(merge_method(merge)),
            wait_for_checks: Some(merge_wait_for_checks(merge)),
            required_checks_only: Some(merge_required_checks_only(merge)),
            delete_branch: Some(merge_delete_branch(merge)),
            required_only: None,
            timeout_seconds: Some(merge_timeout_seconds(merge)),
            interval_seconds: Some(merge_interval_seconds(merge)),
            cwd: None,
            command: Vec::new(),
            env: BTreeMap::new(),
            deployment_mode: None,
            checkout: None,
        });
        if ordered_ids.contains(&repo.id) {
            previous_ordered = Some(id);
        }
    }
    append_project_deployments(active, landing, &mut steps)?;

    if steps.is_empty() {
        bail!(
            "No PR publications or project landing deployments are available for this bundle. Run `knit publish create` first or configure project landing deployments."
        );
    }

    Ok(LandPlan {
        schema_version: SCHEMA_VERSION.to_string(),
        kind: LAND_PLAN_KIND.to_string(),
        id: format!("land-{}", active.bundle.id),
        provider,
        bundle_id: active.bundle.id.clone(),
        source_project_id: project.as_ref().map(|project| project.id.clone()),
        created_at: now_iso(),
        on_failure: landing.and_then(|landing| landing.on_failure),
        steps,
    })
}

fn load_project_for_bundle(active: &ActiveBundle) -> Result<Option<KnitProject>> {
    let config = load_config(&active.root)?;
    let Some(project_id) = active
        .bundle
        .project_id
        .as_deref()
        .or(config.active_project.as_deref())
    else {
        return Ok(None);
    };
    read_json(&project_path(&active.root, project_id)).map(Some)
}

fn ordered_merge_repos<'a>(
    active: &'a ActiveBundle,
    merge: Option<&ProjectLandingMergePlan>,
) -> Vec<&'a RepoEntry> {
    let mut repos = Vec::new();
    let mut seen = BTreeSet::new();
    if let Some(merge) = merge {
        for repo_id in &merge.repo_order {
            if let Some(repo) = active.bundle.repos.iter().find(|repo| repo.id == *repo_id) {
                if seen.insert(repo.id.clone()) {
                    repos.push(repo);
                }
            }
        }
    }

    if merge
        .and_then(|merge| merge.include_unlisted)
        .unwrap_or(true)
    {
        for repo in &active.bundle.repos {
            if seen.insert(repo.id.clone()) {
                repos.push(repo);
            }
        }
    }

    repos
}

fn merge_method(merge: Option<&ProjectLandingMergePlan>) -> MergeMethod {
    merge.and_then(|merge| merge.method).unwrap_or_default()
}

fn merge_wait_for_checks(merge: Option<&ProjectLandingMergePlan>) -> bool {
    merge
        .and_then(|merge| merge.wait_for_checks)
        .unwrap_or(true)
}

fn merge_required_checks_only(merge: Option<&ProjectLandingMergePlan>) -> bool {
    merge
        .and_then(|merge| merge.required_checks_only)
        .unwrap_or(true)
}

fn merge_delete_branch(merge: Option<&ProjectLandingMergePlan>) -> bool {
    merge.and_then(|merge| merge.delete_branch).unwrap_or(false)
}

fn merge_timeout_seconds(merge: Option<&ProjectLandingMergePlan>) -> u64 {
    merge
        .and_then(|merge| merge.timeout_seconds)
        .unwrap_or(1800)
}

fn merge_interval_seconds(merge: Option<&ProjectLandingMergePlan>) -> u64 {
    merge.and_then(|merge| merge.interval_seconds).unwrap_or(10)
}

fn append_project_deployments(
    active: &ActiveBundle,
    landing: Option<&ProjectLandingPlan>,
    steps: &mut Vec<LandStep>,
) -> Result<()> {
    let Some(landing) = landing else {
        return Ok(());
    };
    let merge_step_ids = steps
        .iter()
        .filter(|step| step.step_type == LandStepKind::MergePr)
        .filter_map(|step| Some((step.repo_id.clone()?, step.id.clone())))
        .collect::<BTreeMap<_, _>>();
    let all_merge_ids = steps
        .iter()
        .filter(|step| step.step_type == LandStepKind::MergePr)
        .map(|step| step.id.clone())
        .collect::<Vec<_>>();

    for deployment in &landing.deployments {
        if let Some(repo_id) = &deployment.repo_id {
            if !active.bundle.repos.iter().any(|repo| repo.id == *repo_id) {
                continue;
            }
        }
        let mode = deployment.mode.unwrap_or(if deployment.command.is_empty() {
            DeployMode::Push
        } else {
            DeployMode::Command
        });
        let needs = if deployment.needs.is_empty() {
            default_deployment_needs(
                deployment.repo_id.as_deref(),
                &merge_step_ids,
                &all_merge_ids,
            )
        } else {
            deployment.needs.clone()
        };
        let checkout = deployment.checkout.as_ref().map(|checkout| LandCheckout {
            branch: checkout.branch.clone(),
            remote: checkout.remote.clone(),
            update: checkout.update,
        });
        steps.push(LandStep {
            id: deployment.id.clone(),
            step_type: LandStepKind::Deploy,
            needs,
            repo_id: deployment.repo_id.clone(),
            method: None,
            wait_for_checks: None,
            required_checks_only: None,
            delete_branch: None,
            required_only: None,
            timeout_seconds: None,
            interval_seconds: None,
            cwd: deployment.cwd.clone(),
            command: deployment.command.clone(),
            env: deployment.env.clone(),
            deployment_mode: Some(mode),
            checkout,
        });
    }

    Ok(())
}

pub(super) fn default_deployment_needs(
    repo_id: Option<&str>,
    merge_step_ids: &BTreeMap<String, String>,
    all_merge_ids: &[String],
) -> Vec<String> {
    if let Some(repo_id) = repo_id {
        if let Some(merge_step) = merge_step_ids.get(repo_id) {
            return vec![merge_step.clone()];
        }
    }
    all_merge_ids.to_vec()
}
