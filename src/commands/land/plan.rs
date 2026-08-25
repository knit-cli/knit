//! Builds the default land plan from the resolved bundle and its project's
//! landing template: one merge step per recorded PR plus the deployments for
//! each declared target branch selected by those PRs.

use super::process::DEFAULT_COMMAND_TIMEOUT_SECONDS;
use super::{
    ensure_provider, LandCheckout, LandPlan, LandStep, LandStepKind, DEFAULT_LAND_PROVIDER,
    LAND_PLAN_KIND,
};
use crate::model::{
    DeployMode, KnitProject, MergeMethod, ProjectLandingLane, ProjectLandingMergePlan,
    ProjectLandingPlan, RepoEntry, SCHEMA_VERSION,
};
use crate::providers::publication_for_repo;
use crate::store::{load_config, project_path, read_json, ActiveBundle};
use crate::time::now_iso;
use anyhow::{bail, Result};
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn build_default_plan(
    active: &ActiveBundle,
    requested_provider: Option<&str>,
    target_branch: Option<&str>,
    lane_name: Option<&str>,
) -> Result<LandPlan> {
    let project = load_project_for_bundle(active)?;
    let landing = project
        .as_ref()
        .and_then(|project| project.landing.as_ref());
    let provider = requested_provider
        .or_else(|| landing.and_then(|landing| landing.provider.as_deref()))
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| inferred_plan_provider(active));
    ensure_provider(&provider)?;
    let merge = landing.map(|landing| &landing.merge);

    // Every repo this bundle actually changed, in the project's merge order.
    // Review merges narrow this to the repos that have a review recorded;
    // branch merges use it whole, because reaching an environment does not
    // require a review at all.
    let changed_repo_ids = crate::commands::publish::publish_scope_repo_ids(&active.bundle);
    let scope = ordered_merge_repos(active, merge)
        .into_iter()
        .filter(|repo| changed_repo_ids.contains(&repo.id))
        .collect::<Vec<_>>();
    let lane = resolve_lane(project.as_ref(), landing, lane_name)?;
    let (destinations, lane_absent) =
        resolve_destinations(&scope, lane_name, lane, target_branch, active)?;
    if let Some(lane_name) = lane_name {
        // Every repository this bundle changed sits outside the lane, so there
        // is no environment for this work to reach. Say that, rather than fall
        // through to the unrelated "nothing published" message below.
        if !scope.is_empty() && destinations.is_empty() && lane_absent.len() == scope.len() {
            bail!(
                "Landing lane `{lane_name}` carries none of the repositories this bundle changed ({}). Those repositories are declared absent from the lane, so this bundle has nothing to land there.",
                lane_absent
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    let terminal = resolve_terminal(
        active,
        landing,
        target_branch,
        lane_name,
        lane,
        &destinations,
        &lane_absent,
    );

    // One rule for every explicit destination: an environment the bundle only
    // passes through is reached by merging its feature branches, so the
    // bundle's review objects stay open against the destination that ends its
    // life; the terminal destination merges the reviews themselves. `--target
    // <branch>` is an ad-hoc lane that sends every changed repository to the
    // same branch, and follows the same rule. A bare request names no
    // destination of its own — it lands each review where it already points —
    // so it always merges the reviews, and warns below if that does not
    // finish the bundle.
    let explicit_destination = lane_name.is_some() || target_branch.is_some();
    let branch_merges = !terminal && explicit_destination;
    let mut steps = Vec::new();
    let ordered_ids: BTreeSet<String> = merge
        .map(|m| m.repo_order.iter().cloned().collect())
        .unwrap_or_default();
    let empty_needs = BTreeMap::new();
    let merge_needs = merge.map(|m| &m.needs).unwrap_or(&empty_needs);
    let mut previous_ordered: Option<String> = None;
    for repo in &scope {
        if !branch_merges && publication_for_repo(&active.bundle, &repo.id).is_none() {
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
        if let Some(lane_name) = lane_name {
            // Declared absent: this repository has no place in this
            // environment, so it keeps its work for the terminal landing.
            if lane_absent.contains(&repo.id) {
                continue;
            }
            if !destinations.contains_key(&repo.id) {
                bail!(
                    "Landing lane `{lane_name}` has no branch for repository `{}`. Add landing.lanes.{lane_name}.branches.{} or defaultBranch. If `{}` has no {lane_name} environment at all, declare it absent with `\"{}\": null`.",
                    repo.id,
                    repo.id,
                    repo.id,
                    repo.id
                );
            }
        }
        let step = if branch_merges {
            let destination = destinations
                .get(&repo.id)
                .cloned()
                .expect("destination was checked above");
            ensure_branch_merge_spares_review(
                active,
                lane_name,
                &repo.id,
                &destination,
                &lane_absent,
            )?;
            LandStep {
                id: id.clone(),
                step_type: LandStepKind::MergeBranch,
                needs,
                repo_id: Some(repo.id.clone()),
                target_branch: Some(destination),
                method: None,
                wait_for_checks: None,
                required_checks_only: None,
                delete_branch: None,
                required_only: None,
                timeout_seconds: None,
                interval_seconds: None,
                cwd: None,
                command: Vec::new(),
                env: BTreeMap::new(),
                deployment_mode: None,
                checkout: None,
            }
        } else {
            LandStep {
                id: id.clone(),
                step_type: LandStepKind::MergePr,
                needs,
                repo_id: Some(repo.id.clone()),
                target_branch: None,
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
            }
        };
        steps.push(step);
        if ordered_ids.contains(&repo.id) {
            previous_ordered = Some(id);
        }
    }

    // The stored lane projection covers exactly the repos this plan moves.
    let target_branches = if lane_name.is_some() {
        steps
            .iter()
            .filter(|step| is_merge_step(step))
            .filter_map(|step| step.repo_id.clone())
            .filter_map(|repo_id| {
                destinations
                    .get(&repo_id)
                    .map(|branch| (repo_id, branch.clone()))
            })
            .collect::<BTreeMap<_, _>>()
    } else {
        BTreeMap::new()
    };
    ensure_terminal_plan_covers_changed_repos(
        active,
        terminal,
        &changed_repo_ids,
        &scope,
        &steps,
        merge,
    )?;

    let mut deployments_skipped = BTreeMap::new();
    append_project_deployments(
        active,
        project.as_ref(),
        landing,
        target_branch,
        lane_name,
        &changed_repo_ids,
        &mut steps,
        &mut deployments_skipped,
    )?;

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
        target_branch: target_branch.map(ToOwned::to_owned),
        lane: lane_name.map(ToOwned::to_owned),
        target_branches,
        lane_absent,
        deployments_skipped,
        changed_repos: changed_repo_ids.clone(),
        bundle_heads: bundle_heads(active),
        terminal,
        source_project_id: project.as_ref().map(|project| project.id.clone()),
        created_at: now_iso(),
        on_failure: landing.and_then(|landing| landing.on_failure),
        require_checks: landing
            .map(|landing| landing.require_checks.clone())
            .unwrap_or_default(),
        steps,
    })
}

/// Each tracked repository's recorded head, which is what a plan is pinned to.
///
/// `head_sha` is maintained by `knit commit` and by the observed-movement
/// tracking, so it moves whenever the bundle's work does.
pub(super) fn bundle_heads(active: &ActiveBundle) -> BTreeMap<String, String> {
    active
        .bundle
        .repos
        .iter()
        .filter_map(|repo| Some((repo.id.clone(), repo.head_sha.clone()?)))
        .collect()
}

/// A terminal landing is the bundle's last stop: it merges, archives the
/// bundle and removes its worktrees. So it has to carry everything the bundle
/// changed, or work is stranded on a branch nobody will land — the bundle is
/// closed, its worktrees are gone, and the forge says the feature shipped.
///
/// Two ways a changed repository falls out of a terminal plan, and they need
/// different fixes, so name which one happened:
///   - it has no recorded review, because `knit publish create` never ran for
///     it or its review was closed;
///   - the project's merge order excludes it, via `includeUnlisted: false`.
///
/// Intermediate destinations are deliberately allowed to carry a subset: a
/// lane can declare a repository absent, and the work waits for the terminal
/// landing. That is what `laneAbsent` records.
fn ensure_terminal_plan_covers_changed_repos(
    active: &ActiveBundle,
    terminal: bool,
    changed_repo_ids: &BTreeSet<String>,
    scope: &[&RepoEntry],
    steps: &[LandStep],
    merge: Option<&ProjectLandingMergePlan>,
) -> Result<()> {
    if !terminal {
        return Ok(());
    }
    let merged: BTreeSet<&str> = steps
        .iter()
        .filter(|step| is_merge_step(step))
        .filter_map(|step| step.repo_id.as_deref())
        .collect();
    let in_scope: BTreeSet<&str> = scope.iter().map(|repo| repo.id.as_str()).collect();

    let mut unpublished = Vec::new();
    let mut excluded = Vec::new();
    for repo_id in changed_repo_ids {
        if merged.contains(repo_id.as_str()) {
            continue;
        }
        // A repository the bundle no longer tracks cannot be landed and is not
        // this check's business.
        if !active.bundle.repos.iter().any(|repo| repo.id == *repo_id) {
            continue;
        }
        if in_scope.contains(repo_id.as_str()) {
            unpublished.push(repo_id.as_str());
        } else {
            excluded.push(repo_id.as_str());
        }
    }
    if unpublished.is_empty() && excluded.is_empty() {
        return Ok(());
    }

    let mut reasons = Vec::new();
    if !unpublished.is_empty() {
        reasons.push(format!(
            "{} {} no recorded review — run `knit publish create` first",
            unpublished.join(", "),
            if unpublished.len() == 1 {
                "has"
            } else {
                "have"
            }
        ));
    }
    if !excluded.is_empty() {
        let include_unlisted = merge
            .and_then(|merge| merge.include_unlisted)
            .unwrap_or(true);
        reasons.push(if include_unlisted {
            format!("{} is not in this plan's merge scope", excluded.join(", "))
        } else {
            format!(
                "{} {} excluded by the project's `merge.repoOrder` with `includeUnlisted: false` — add {} to the order, or allow unlisted repositories",
                excluded.join(", "),
                if excluded.len() == 1 { "is" } else { "are" },
                if excluded.len() == 1 { "it" } else { "them" }
            )
        });
    }
    bail!(
        "This landing archives the bundle, but it does not carry every repository the bundle changed: {}. Landing now would strand that work on its feature branch.",
        reasons.join("; ")
    );
}

/// A branch merge only leaves the review open if it goes somewhere the review
/// is not already pointed at. When a lane maps a repository onto that repo's
/// own review base, merging the feature branch there puts the review's commits
/// into its base, and the forge closes it as merged — silently spending the
/// one review the bundle has, in a landing that claims to be a stop along the
/// way. Refuse instead, and name both ways out.
pub(super) fn ensure_branch_merge_spares_review(
    active: &ActiveBundle,
    lane_name: Option<&str>,
    repo_id: &str,
    destination: &str,
    lane_absent: &BTreeSet<String>,
) -> Result<()> {
    let Some(publication) = publication_for_repo(&active.bundle, repo_id) else {
        return Ok(());
    };
    if publication.base_branch != destination {
        return Ok(());
    }
    let (landing, way_out) = match lane_name {
        Some(lane) => {
            // "Declare the lane terminal" is only a way out when the lane could
            // be terminal. A lane that skips repositories cannot, so pointing
            // at it would send the reader into the next error instead of out
            // of this one.
            let way_out = if lane_absent.is_empty() {
                format!("Point the lane at a different branch for `{repo_id}`, or declare the lane terminal so Knit merges the review itself.")
            } else {
                format!(
                    "Point the lane at a different branch for `{repo_id}`. This lane cannot be terminal instead, because it skips {}, and a bundle's last stop has to carry every repository.",
                    lane_absent
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            (
                format!("Landing lane `{lane}` sends `{repo_id}` to `{destination}`, which is"),
                way_out,
            )
        }
        // An ad-hoc `--target` landing: every repository goes to the one
        // branch, so the only ways out are another branch or declaring this
        // one terminal.
        None => (
            format!("Landing into `{destination}` sends `{repo_id}` to"),
            format!("Land into a different branch, or declare `landing.targets.{destination}.terminal: true` so Knit merges the review itself."),
        ),
    };
    bail!(
        "{landing} the base of its recorded review {}. Merging the feature branch there would put the review's own commits into its base and the forge would close it as merged, so this landing cannot leave the review open. {way_out}",
        publication.url
    );
}

pub(super) fn is_merge_step(step: &LandStep) -> bool {
    matches!(
        step.step_type,
        LandStepKind::MergePr | LandStepKind::MergeBranch
    )
}

/// Where each repository's work is headed in this landing: the lane's branch
/// for it, the one raw target branch, or the base its review already records.
///
/// The second half of the answer is the repositories a lane deliberately does
/// not carry. They are not destinations and not mistakes, so they travel
/// separately and end up named in the plan rather than silently missing.
fn resolve_destinations(
    scope: &[&RepoEntry],
    lane_name: Option<&str>,
    lane: Option<&ProjectLandingLane>,
    target_branch: Option<&str>,
    active: &ActiveBundle,
) -> Result<(BTreeMap<String, String>, BTreeSet<String>)> {
    if let (Some(_), Some(lane)) = (lane_name, lane) {
        let mut destinations = BTreeMap::new();
        let mut absent = BTreeSet::new();
        for repo in scope {
            match lane_destination(lane, &repo.id) {
                LaneDestination::Branch(branch) => {
                    destinations.insert(repo.id.clone(), branch.to_string());
                }
                LaneDestination::Absent => {
                    absent.insert(repo.id.clone());
                }
                // Leniently: a repo the lane never mentions is only a problem
                // if this plan actually merges it, and the step loop says so
                // by name.
                LaneDestination::Unmapped => {}
            }
        }
        return Ok((destinations, absent));
    }
    if let Some(target_branch) = target_branch {
        return Ok((
            scope
                .iter()
                .map(|repo| (repo.id.clone(), target_branch.to_string()))
                .collect(),
            BTreeSet::new(),
        ));
    }
    Ok((
        scope
            .iter()
            .filter_map(|repo| {
                let base = publication_for_repo(&active.bundle, &repo.id)?
                    .base_branch
                    .clone();
                Some((repo.id.clone(), base))
            })
            .collect(),
        BTreeSet::new(),
    ))
}

/// Whether landing this plan finishes the bundle.
///
/// A bundle's work is done when it has reached every repository's configured
/// base branch; any other destination is an environment the bundle passes
/// through, so landing there must leave it open. A project can override that
/// per lane or per branch-keyed target with `terminal`, which is how a
/// release branch that is not a repo's configured base still ends the
/// bundle's life.
fn resolve_terminal(
    active: &ActiveBundle,
    landing: Option<&ProjectLandingPlan>,
    target_branch: Option<&str>,
    lane_name: Option<&str>,
    lane: Option<&ProjectLandingLane>,
    destinations: &BTreeMap<String, String>,
    lane_absent: &BTreeSet<String>,
) -> bool {
    if lane_name.is_some() {
        if let Some(declared) = lane.and_then(|lane| lane.terminal) {
            return declared;
        }
        // A destination this bundle's work does not all reach cannot be where
        // that work ends, however the branches happen to line up.
        if !lane_absent.is_empty() {
            return false;
        }
        // An unresolvable lane is not a claim that the bundle is finished.
        return !destinations.is_empty() && destinations_are_configured_bases(active, destinations);
    }

    if let Some(target_branch) = target_branch {
        if let Some(declared) = landing
            .and_then(|landing| landing.targets.get(target_branch))
            .and_then(|target| target.terminal)
        {
            return declared;
        }
        return destinations_are_configured_bases(active, destinations);
    }

    // Without an explicit destination each review keeps its recorded base.
    destinations_are_configured_bases(active, destinations)
}

/// Whether every destination is the repository's own configured base branch.
/// A deploy-only plan merges nothing and keeps its historical behavior.
fn destinations_are_configured_bases(
    active: &ActiveBundle,
    destinations: &BTreeMap<String, String>,
) -> bool {
    destinations.iter().all(|(repo_id, branch)| {
        active
            .bundle
            .repos
            .iter()
            .find(|repo| repo.id == *repo_id)
            .is_some_and(|repo| repo.base_branch == *branch)
    })
}

/// Look up and validate the project lane a landing was asked for.
fn resolve_lane<'a>(
    project: Option<&'a KnitProject>,
    landing: Option<&'a ProjectLandingPlan>,
    lane_name: Option<&str>,
) -> Result<Option<&'a ProjectLandingLane>> {
    let Some(lane_name) = lane_name else {
        return Ok(None);
    };
    let project = project.ok_or_else(|| {
        anyhow::anyhow!(
            "Landing lane `{lane_name}` needs a project-backed bundle with landing.lanes configured."
        )
    })?;
    let landing = landing
        .ok_or_else(|| anyhow::anyhow!("Project `{}` has no landing configuration.", project.id))?;
    let lane = landing.lanes.get(lane_name).ok_or_else(|| {
        let available = landing.lanes.keys().cloned().collect::<Vec<_>>();
        if available.is_empty() {
            anyhow::anyhow!("Project `{}` declares no landing lanes.", project.id)
        } else {
            anyhow::anyhow!(
                "Unknown landing lane `{lane_name}`. Available lanes: {}.",
                available.join(", ")
            )
        }
    })?;
    validate_lane(project, lane_name, lane)?;
    Ok(Some(lane))
}

fn validate_lane(project: &KnitProject, lane_name: &str, lane: &ProjectLandingLane) -> Result<()> {
    if lane
        .default_branch
        .as_deref()
        .is_some_and(|branch| branch.trim().is_empty())
    {
        bail!("landing.lanes.{lane_name}.defaultBranch must not be empty");
    }
    if let (Some(default), Some(wildcard)) =
        (lane.default_branch.as_deref(), lane.branches.get("*"))
    {
        match wildcard.as_deref() {
            Some(wildcard) if wildcard != default => bail!(
                "landing lane `{lane_name}` declares conflicting defaultBranch `{default}` and branches.* `{wildcard}`"
            ),
            // A null wildcard says repositories are absent unless named, which
            // is the opposite of what a defaultBranch says.
            None => bail!(
                "landing lane `{lane_name}` declares defaultBranch `{default}` and a null branches.*, which contradict each other. Keep the defaultBranch, or drop it and name the repositories that are in this lane."
            ),
            Some(_) => {}
        }
    }
    for (repo_id, branch) in &lane.branches {
        if branch
            .as_deref()
            .is_some_and(|branch| branch.trim().is_empty())
        {
            bail!("landing.lanes.{lane_name}.branches.{repo_id} must not be empty. Use null to declare `{repo_id}` absent from this lane.");
        }
        if repo_id != "*" && !project.repos.iter().any(|repo| repo.id == *repo_id) {
            bail!("landing lane `{lane_name}` maps unknown project repository `{repo_id}`");
        }
    }
    // The terminal destination is where the bundle's work ends, so every
    // repository has to reach it. A lane that skips one cannot be the last
    // stop: archiving there would strand that repository's review open.
    if lane.terminal == Some(true) {
        let absent = lane
            .branches
            .iter()
            .filter(|(_, branch)| branch.is_none())
            .map(|(repo_id, _)| repo_id.as_str())
            .collect::<Vec<_>>();
        if !absent.is_empty() {
            bail!(
                "landing lane `{lane_name}` is declared terminal but skips {}. A bundle's last stop has to carry every repository, or those reviews stay open after it is archived. Give them a branch, or drop `\"terminal\": true`.",
                absent.join(", ")
            );
        }
    }
    // A lane cannot both skip a repository and deploy it: one of the two is a
    // mistake, and guessing which would hide it.
    for deployment in &lane.deployments {
        let Some(repo_id) = deployment.repo_id.as_deref() else {
            continue;
        };
        if matches!(lane_destination(lane, repo_id), LaneDestination::Absent) {
            bail!(
                "landing lane `{lane_name}` declares `{repo_id}` absent but its deployment `{}` targets that repository. Give `{repo_id}` a branch in this lane, or move the deployment.",
                deployment.id
            );
        }
    }
    Ok(())
}

/// Where a lane sends one repository. A lane is an environment, so there are
/// three answers, not two: a branch, "this repository has no place in this
/// environment", and "the lane never says".
enum LaneDestination<'a> {
    Branch(&'a str),
    Absent,
    Unmapped,
}

fn lane_destination<'a>(lane: &'a ProjectLandingLane, repo_id: &str) -> LaneDestination<'a> {
    // An explicit entry wins over the wildcard and the default, including when
    // it is null: that is how a repo opts out of a lane everything else joins.
    if let Some(entry) = lane.branches.get(repo_id) {
        return match entry {
            Some(branch) => LaneDestination::Branch(branch.as_str()),
            None => LaneDestination::Absent,
        };
    }
    // A wildcard entry answers for every repo the lane does not name, so a
    // null wildcard is an allow-list: only the named repos are in this lane.
    if let Some(wildcard) = lane.branches.get("*") {
        return match wildcard {
            Some(branch) => LaneDestination::Branch(branch.as_str()),
            None => LaneDestination::Absent,
        };
    }
    match lane.default_branch.as_deref() {
        Some(branch) => LaneDestination::Branch(branch),
        None => LaneDestination::Unmapped,
    }
}

fn inferred_plan_provider(active: &ActiveBundle) -> String {
    let providers = active
        .bundle
        .repos
        .iter()
        .filter_map(|repo| publication_for_repo(&active.bundle, &repo.id))
        .map(|publication| publication.provider.as_str())
        .collect::<BTreeSet<_>>();
    if providers.len() == 1 {
        providers
            .into_iter()
            .next()
            .unwrap_or(DEFAULT_LAND_PROVIDER)
            .to_string()
    } else {
        DEFAULT_LAND_PROVIDER.to_string()
    }
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

#[allow(clippy::too_many_arguments)]
fn append_project_deployments(
    active: &ActiveBundle,
    project: Option<&KnitProject>,
    landing: Option<&ProjectLandingPlan>,
    explicit_target: Option<&str>,
    lane_name: Option<&str>,
    changed_repo_ids: &BTreeSet<String>,
    steps: &mut Vec<LandStep>,
    skipped: &mut BTreeMap<String, Vec<String>>,
) -> Result<()> {
    let Some(landing) = landing else {
        return Ok(());
    };
    let mut pending: Vec<PendingDeployment<'_>> = Vec::new();
    let merge_step_ids = steps
        .iter()
        .filter(|step| is_merge_step(step))
        .filter_map(|step| Some((step.repo_id.clone()?, step.id.clone())))
        .collect::<BTreeMap<_, _>>();
    let all_merge_ids = steps
        .iter()
        .filter(|step| is_merge_step(step))
        .map(|step| step.id.clone())
        .collect::<Vec<_>>();
    if let Some(lane_name) = lane_name {
        let lane = landing
            .lanes
            .get(lane_name)
            .expect("lane was resolved before deployments");
        for deployment in &lane.deployments {
            // Resolved from the lane declaration itself, not from this plan's
            // merge steps or its resolved destinations: both cover only the
            // repositories this bundle changed. A deployment triggered by
            // another repository's change has no merge of its own and is
            // outside that set, yet it still deploys into a real branch.
            let target_branch = deployment.repo_id.as_deref().and_then(|repo_id| {
                match lane_destination(lane, repo_id) {
                    LaneDestination::Branch(branch) => Some(branch),
                    LaneDestination::Absent | LaneDestination::Unmapped => None,
                }
            });
            push_pending_deployment(
                active,
                project,
                &mut pending,
                deployment,
                target_branch,
                Some(lane_name),
                changed_repo_ids,
            )?;
        }
        return finish_deployments(&pending, steps, skipped, &merge_step_ids, &all_merge_ids);
    }
    // Where each merge step actually sends its repository. A branch merge
    // names its destination on the step and needs no review, so read it from
    // there; a review merge goes to the raw target or the base it published
    // to.
    let target_by_repo = steps
        .iter()
        .filter(|step| is_merge_step(step))
        .filter_map(|step| {
            let repo_id = step.repo_id.clone()?;
            let branch = match step.step_type {
                LandStepKind::MergeBranch => step.target_branch.clone()?,
                _ => explicit_target.map(ToOwned::to_owned).or_else(|| {
                    Some(
                        publication_for_repo(&active.bundle, &repo_id)?
                            .base_branch
                            .clone(),
                    )
                })?,
            };
            Some((repo_id, branch))
        })
        .collect::<BTreeMap<_, _>>();
    let target_branches = active
        .bundle
        .repos
        .iter()
        .filter_map(|repo| target_by_repo.get(&repo.id))
        .fold(Vec::<String>::new(), |mut branches, branch| {
            if !branches.contains(branch) {
                branches.push(branch.clone());
            }
            branches
        });
    let has_declared_target = target_branches
        .iter()
        .any(|branch| landing.targets.contains_key(branch));
    let all_merges_use_configured_bases = target_by_repo.iter().all(|(repo_id, branch)| {
        active
            .bundle
            .repos
            .iter()
            .find(|repo| repo.id == *repo_id)
            .is_some_and(|repo| repo.base_branch == *branch)
    });

    // Top-level deployments are the backward-compatible configured-base lane.
    // A branch-keyed target takes precedence whenever one of the recorded PR
    // bases declares it. Deploy-only plans also retain their legacy behavior.
    if target_by_repo.is_empty() || (!has_declared_target && all_merges_use_configured_bases) {
        for deployment in &landing.deployments {
            push_pending_deployment(
                active,
                project,
                &mut pending,
                deployment,
                None,
                None,
                changed_repo_ids,
            )?;
        }
    }

    for branch in &target_branches {
        let Some(target) = landing.targets.get(branch) else {
            continue;
        };
        for deployment in &target.deployments {
            // "Does this repository land into this branch?" only decides
            // anything for a deployment that fires on its own repository. One
            // triggered by another repository's change has no merge of its own
            // to match against, and discarding it here would drop it silently.
            let fires_on_own_repo =
                deployment_watches(project, deployment)?.is_some_and(|watched| {
                    watched
                        .iter()
                        .all(|id| Some(id) == deployment.repo_id.as_ref())
                });
            if fires_on_own_repo
                && deployment
                    .repo_id
                    .as_ref()
                    .is_some_and(|repo_id| target_by_repo.get(repo_id) != Some(branch))
            {
                continue;
            }
            push_pending_deployment(
                active,
                project,
                &mut pending,
                deployment,
                Some(branch),
                None,
                changed_repo_ids,
            )?;
        }
    }

    finish_deployments(&pending, steps, skipped, &merge_step_ids, &all_merge_ids)
}

/// A deployment selected for this landing, before skips and dependencies are
/// resolved. Selection and materialisation are separate passes because a
/// deployment nothing triggers can still be required by one that fires.
struct PendingDeployment<'a> {
    deployment: &'a crate::model::ProjectLandingDeployment,
    target_branch: Option<String>,
    lane_name: Option<String>,
    /// The repositories it watches, or `None` when it always runs.
    watched: Option<Vec<String>>,
    /// Whether its own trigger fired.
    triggered: bool,
}

#[allow(clippy::too_many_arguments)]
fn push_pending_deployment<'a>(
    active: &ActiveBundle,
    project: Option<&KnitProject>,
    pending: &mut Vec<PendingDeployment<'a>>,
    deployment: &'a crate::model::ProjectLandingDeployment,
    target_branch: Option<&str>,
    lane_name: Option<&str>,
    changed_repo_ids: &BTreeSet<String>,
) -> Result<()> {
    if let Some(repo_id) = &deployment.repo_id {
        if !active.bundle.repos.iter().any(|repo| repo.id == *repo_id) {
            return Ok(());
        }
    }
    if pending
        .iter()
        .any(|entry| entry.deployment.id == deployment.id)
    {
        bail!(
            "landing step id `{}` is selected more than once; use unique deployment ids across landing targets",
            deployment.id
        );
    }
    let watched = deployment_watches(project, deployment)?;
    ensure_push_deployment_is_not_cross_repo(deployment, watched.as_deref())?;
    // A bundle that recorded no work at all is a deploy-only plan: there is no
    // change set to scope against, so scoping has nothing to say and the
    // configured deployments stand.
    let triggered = match &watched {
        None => true,
        Some(_) if changed_repo_ids.is_empty() => true,
        Some(watched) => watched
            .iter()
            .any(|repo_id| changed_repo_ids.contains(repo_id)),
    };
    pending.push(PendingDeployment {
        deployment,
        target_branch: target_branch.map(ToOwned::to_owned),
        lane_name: lane_name.map(ToOwned::to_owned),
        watched,
        triggered,
    });
    Ok(())
}

/// A push deployment means "the merge of my repository triggered this"; the
/// executor reports exactly that and does no work of its own. Letting it watch
/// another repository would make it claim a merge that never happened, so the
/// configuration is refused rather than the report quietly made false.
fn ensure_push_deployment_is_not_cross_repo(
    deployment: &crate::model::ProjectLandingDeployment,
    watched: Option<&[String]>,
) -> Result<()> {
    let mode = deployment.mode.unwrap_or(if deployment.command.is_empty() {
        DeployMode::Push
    } else {
        DeployMode::Command
    });
    if mode != DeployMode::Push {
        return Ok(());
    }
    let own_repo_only = watched.is_some_and(|watched| {
        watched
            .iter()
            .all(|id| Some(id) == deployment.repo_id.as_ref())
    });
    if own_repo_only {
        return Ok(());
    }
    bail!(
        "landing deployment `{}` uses push mode but watches another repository. A push deployment only reports that its own repository's merge triggered it, so it cannot be triggered by someone else's change. Give it `mode: \"command\"`, or drop the extra `whenChanged` entries.",
        deployment.id
    )
}

/// Materialise the selected deployments, resolving skips against the `needs`
/// graph first.
///
/// A deployment nothing triggers is skipped — unless a deployment that *is*
/// running needs it. "B needs A" means A has to run, so a skipped A that B
/// depends on is reinstated, transitively. Otherwise the plan would carry a
/// step whose dependency does not exist, and `ordered_step_ids` would refuse
/// the whole landing.
fn finish_deployments(
    pending: &[PendingDeployment<'_>],
    steps: &mut Vec<LandStep>,
    skipped: &mut BTreeMap<String, Vec<String>>,
    merge_step_ids: &BTreeMap<String, String>,
    all_merge_ids: &[String],
) -> Result<()> {
    let mut running: BTreeSet<&str> = pending
        .iter()
        .filter(|entry| entry.triggered)
        .map(|entry| entry.deployment.id.as_str())
        .collect();

    loop {
        let mut added = false;
        for entry in pending {
            if !running.contains(entry.deployment.id.as_str()) {
                continue;
            }
            for need in &entry.deployment.needs {
                if let Some(required) = pending
                    .iter()
                    .find(|candidate| candidate.deployment.id == *need)
                {
                    if running.insert(required.deployment.id.as_str()) {
                        added = true;
                    }
                }
            }
        }
        if !added {
            break;
        }
    }

    for entry in pending {
        if running.contains(entry.deployment.id.as_str()) {
            steps.push(deployment_step(entry, merge_step_ids, all_merge_ids));
        } else if let Some(watched) = &entry.watched {
            skipped.insert(entry.deployment.id.clone(), watched.clone());
        }
    }
    Ok(())
}

fn deployment_step(
    entry: &PendingDeployment<'_>,
    merge_step_ids: &BTreeMap<String, String>,
    all_merge_ids: &[String],
) -> LandStep {
    let deployment = entry.deployment;
    let mode = deployment.mode.unwrap_or(if deployment.command.is_empty() {
        DeployMode::Push
    } else {
        DeployMode::Command
    });
    let needs = if deployment.needs.is_empty() {
        default_deployment_needs(deployment.repo_id.as_deref(), merge_step_ids, all_merge_ids)
    } else {
        deployment.needs.clone()
    };
    let checkout = deployment.checkout.as_ref().map(|checkout| LandCheckout {
        branch: checkout.branch.clone(),
        remote: checkout.remote.clone(),
        update: checkout.update,
    });
    let mut env = deployment.env.clone();
    if let Some(target_branch) = &entry.target_branch {
        env.insert("KNIT_LAND_TARGET_BRANCH".to_string(), target_branch.clone());
    }
    if let Some(lane_name) = &entry.lane_name {
        env.insert("KNIT_LAND_LANE".to_string(), lane_name.clone());
    }
    LandStep {
        id: deployment.id.clone(),
        step_type: LandStepKind::Deploy,
        needs,
        repo_id: deployment.repo_id.clone(),
        target_branch: None,
        method: None,
        wait_for_checks: None,
        required_checks_only: None,
        delete_branch: None,
        required_only: None,
        timeout_seconds: (mode == DeployMode::Command).then_some(
            deployment
                .timeout_seconds
                .unwrap_or(DEFAULT_COMMAND_TIMEOUT_SECONDS),
        ),
        interval_seconds: None,
        cwd: deployment.cwd.clone(),
        command: deployment.command.clone(),
        env,
        deployment_mode: Some(mode),
        checkout,
    }
}

/// The repositories whose changes make `deployment` run, or `None` when it
/// always runs.
///
/// A deployment usually watches the repository it deploys. It may watch more:
/// an image that builds another repository's binary into itself has to
/// redeploy when that repository changes, or it ships a stale one. A step with
/// no repository of its own cannot be scoped, and is refused by validation
/// before it ever runs.
///
/// An unknown repository id is refused rather than ignored. Silently watching
/// a repository that does not exist means never deploying, and a typo is
/// exactly what that looks like.
fn deployment_watches(
    project: Option<&KnitProject>,
    deployment: &crate::model::ProjectLandingDeployment,
) -> Result<Option<Vec<String>>> {
    let Some(declared) = &deployment.when_changed else {
        return Ok(deployment.repo_id.clone().map(|repo_id| vec![repo_id]));
    };
    if declared.is_empty() {
        bail!(
            "landing deployment `{}` has an empty `whenChanged`, which can never match and would silently never deploy. List the repositories it depends on, or remove the field to watch its own repository.",
            deployment.id
        );
    }
    let unique: BTreeSet<&String> = declared.iter().collect();
    if unique.len() != declared.len() {
        bail!(
            "landing deployment `{}` repeats a repository in `whenChanged`.",
            deployment.id
        );
    }
    // `"*"` is checked *after* the ids, not before: short-circuiting on it let
    // a typo travel alongside it unvalidated, in the one field whose whole job
    // is to catch typos.
    let wildcard = declared.iter().any(|repo_id| repo_id == "*");
    if wildcard && declared.len() > 1 {
        bail!(
            "landing deployment `{}` combines `\"*\"` with named repositories in `whenChanged`. `\"*\"` already means every landing, so the extra entries change nothing and one of them is probably meant to stand alone.",
            deployment.id
        );
    }
    if let Some(project) = project {
        for repo_id in declared.iter().filter(|repo_id| *repo_id != "*") {
            if !project.repos.iter().any(|repo| repo.id == *repo_id) {
                bail!(
                    "landing deployment `{}` watches unknown repository `{repo_id}` in `whenChanged`. Use a repository id from this project, or `\"*\"` to deploy on every landing.",
                    deployment.id
                );
            }
        }
    }
    if wildcard {
        return Ok(None);
    }
    Ok(Some(declared.clone()))
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
