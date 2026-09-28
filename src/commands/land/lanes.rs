//! Shared project landing-lane resolution: the one place a lane name becomes
//! per-repository branches. `knit land` plans with it, and `knit publish
//! create --lane` publishes against it, so both commands validate and resolve
//! destinations identically.

use crate::model::{KnitProject, ProjectLandingLane, ProjectLandingPlan};
use crate::store::{load_config, project_path, read_json, ActiveBundle};
use anyhow::{bail, Result};

pub(crate) fn normalize_target_branch(target_branch: Option<&str>) -> Result<Option<String>> {
    let Some(target_branch) = target_branch else {
        return Ok(None);
    };
    let target_branch = target_branch.trim();
    if target_branch.is_empty() {
        bail!("--target must name a non-empty branch");
    }
    Ok(Some(target_branch.to_string()))
}

pub(crate) fn normalize_lane_name(lane_name: Option<&str>) -> Result<Option<String>> {
    let Some(lane_name) = lane_name else {
        return Ok(None);
    };
    let lane_name = lane_name.trim();
    if lane_name.is_empty() {
        bail!("--lane must name a non-empty project landing lane");
    }
    Ok(Some(lane_name.to_string()))
}

/// Load the project a bundle belongs to: its recorded `projectId` when set,
/// else the workspace's active project. `None` when neither names one —
/// lane-dependent callers refuse that themselves.
pub(crate) fn load_project_for_bundle(active: &ActiveBundle) -> Result<Option<KnitProject>> {
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

/// Whether this workspace is a scoped clone (`knit clone --view`), carrying
/// only part of the project. Shared landing config still names every repo,
/// so validation must not read an absent repo as a typo there.
pub(crate) fn workspace_is_scoped(active: &ActiveBundle) -> bool {
    load_config(&active.root)
        .ok()
        .and_then(|config| config.scope_view)
        .is_some()
}

/// Look up and validate the project lane a destination was asked for.
pub(crate) fn resolve_lane<'a>(
    project: Option<&'a KnitProject>,
    landing: Option<&'a ProjectLandingPlan>,
    lane_name: Option<&str>,
    scoped: bool,
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
    validate_lane(project, lane_name, lane, scoped)?;
    Ok(Some(lane))
}

pub(crate) fn validate_lane(
    project: &KnitProject,
    lane_name: &str,
    lane: &ProjectLandingLane,
    scoped: bool,
) -> Result<()> {
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
        if repo_id != "*" && !scoped && !project.repos.iter().any(|repo| repo.id == *repo_id) {
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
pub(crate) enum LaneDestination<'a> {
    Branch(&'a str),
    Absent,
    Unmapped,
}

pub(crate) fn lane_destination<'a>(
    lane: &'a ProjectLandingLane,
    repo_id: &str,
) -> LaneDestination<'a> {
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
