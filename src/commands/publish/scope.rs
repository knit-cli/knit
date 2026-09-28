//! Repo selection for publish: which tracked repos are in publishing scope,
//! provider filtering, and the destination each repo's review targets.

use crate::commands::land::lanes::{
    lane_destination, load_project_for_bundle, normalize_lane_name, normalize_target_branch,
    resolve_lane, workspace_is_scoped, LaneDestination,
};
use crate::model::{ChangeGroup, ProjectLandingLane, RepoEntry};
use crate::providers::{self};
use crate::repo_selectors::resolve_repo_indexes;
use crate::store::ActiveBundle;
use anyhow::{bail, Result};
use std::collections::BTreeSet;

/// Narrow resolved repo indexes to those hosted on `provider` (e.g. "github",
/// "gitlab", "forgejo"/"codeberg", "bitbucket"). With no provider the indexes pass through
/// unchanged, preserving the default "publish to wherever each repo is hosted"
/// behavior. The provider string is canonicalized through the forge registry,
/// so "codeberg" and "gitea" both match the Forgejo adapter.
pub(super) fn filter_indexes_by_provider(
    repos: &[RepoEntry],
    indexes: Vec<usize>,
    provider: Option<&str>,
) -> Result<Vec<usize>> {
    let Some(requested) = provider else {
        return Ok(indexes);
    };
    let want = providers::by_id(requested)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "unknown provider `{requested}`. Known providers: github, gitlab, forgejo, bitbucket."
            )
        })?
        .id()
        .to_string();
    let mut filtered = Vec::new();
    for index in indexes {
        if providers::for_repo(&repos[index])?.id() == want.as_str() {
            filtered.push(index);
        }
    }
    if filtered.is_empty() {
        bail!("No repos in the selected set are hosted on `{want}`.");
    }
    Ok(filtered)
}

pub(super) fn resolve_publish_repo_indexes(
    active: &ActiveBundle,
    selectors: &[String],
    all: bool,
) -> Result<Vec<usize>> {
    if all || !selectors.is_empty() {
        return resolve_repo_indexes(active, selectors, all);
    }

    let repo_ids = publish_scope_repo_ids(&active.bundle);
    if repo_ids.is_empty() {
        bail!(
            "No repos in bundle `{}` have recorded commits, repo changes, or publications. Pass repo selectors or --all to publish tracked repos anyway.",
            active.bundle.id
        );
    }

    let indexes = active
        .bundle
        .repos
        .iter()
        .enumerate()
        .filter_map(|(index, repo)| repo_ids.contains(&repo.id).then_some(index))
        .collect::<Vec<_>>();

    if indexes.is_empty() {
        bail!(
            "Bundle `{}` has recorded work, but none of it matches the tracked repos.",
            active.bundle.id
        );
    }

    Ok(indexes)
}

pub(crate) fn publish_scope_repo_ids(bundle: &ChangeGroup) -> BTreeSet<String> {
    let mut repo_ids = recorded_work_repo_ids(bundle);
    repo_ids.extend(
        bundle
            .publications
            .iter()
            .filter(|publication| providers::is_review_kind(&publication.kind))
            .map(|publication| publication.repo_id.clone()),
    );
    repo_ids
}

fn recorded_work_repo_ids(bundle: &ChangeGroup) -> BTreeSet<String> {
    let mut repo_ids = BTreeSet::new();

    for group in &bundle.commit_groups {
        repo_ids.extend(group.commits.iter().map(|commit| commit.repo_id.clone()));
    }

    for node in &bundle.nodes {
        repo_ids.extend(node.commits.iter().map(|commit| commit.repo_id.clone()));
        repo_ids.extend(
            node.repo_changes
                .iter()
                .map(|repo_change| repo_change.repo_id.clone()),
        );
    }

    repo_ids
}

pub(super) fn resolve_publish_repo_indexes_for_bundle(
    bundle: &ChangeGroup,
    selectors: &[String],
    all: bool,
) -> Result<Vec<usize>> {
    if all || !selectors.is_empty() {
        // Best-effort: reuse selector logic only when we have an ActiveBundle.
        // For artifact-only publish, require --all or omit selectors.
        if !selectors.is_empty() {
            bail!("Artifact-only publish does not support repo selectors yet. Use --all or omit selectors.");
        }
    }

    let repo_ids = publish_scope_repo_ids(bundle);
    let indexes = bundle
        .repos
        .iter()
        .enumerate()
        .filter_map(|(index, repo)| repo_ids.contains(&repo.id).then_some(index))
        .collect::<Vec<_>>();

    if indexes.is_empty() {
        bail!(
            "Bundle `{}` has no repos eligible for publishing. Pass --all to force publishing every repo.",
            bundle.id
        );
    }

    Ok(indexes)
}

/// Where a publish run sends each repo's review: the same destination flags
/// `knit land` understands, defaulting to each repo's configured bundle base.
#[derive(Debug)]
pub(super) enum PublishDestination {
    /// No flags: every selected repo publishes against its configured bundle
    /// base branch (`repo.baseBranch`).
    ConfiguredBases,
    /// `--target BRANCH`: one literal branch for every selected repo.
    TargetBranch(String),
    /// `--lane NAME`: per-repo branches from the project's
    /// `landing.lanes.<name>`, resolved and validated exactly as `knit land`
    /// resolves it.
    Lane {
        name: String,
        lane: ProjectLandingLane,
    },
}

impl PublishDestination {
    /// Resolve one repo's destination branch. `Ok(None)` means the lane
    /// deliberately excludes this repo (a `null` entry), so publishing skips
    /// it; everything else is an error surfaced before any push or API write.
    pub(super) fn branch_for(&self, repo: &RepoEntry) -> Result<Option<String>> {
        match self {
            PublishDestination::ConfiguredBases => Ok(Some(repo.base_branch.clone())),
            PublishDestination::TargetBranch(branch) => Ok(Some(branch.clone())),
            PublishDestination::Lane { name, lane } => match lane_destination(lane, &repo.id) {
                LaneDestination::Branch(branch) => Ok(Some(branch.to_string())),
                LaneDestination::Absent => Ok(None),
                LaneDestination::Unmapped => bail!(
                    "Publish lane `{name}` has no branch for repository `{}`. Add landing.lanes.{name}.branches.{} or defaultBranch. If `{}` has no {name} environment at all, declare it absent with `\"{}\": null`.",
                    repo.id,
                    repo.id,
                    repo.id,
                    repo.id
                ),
            },
        }
    }

    /// The lane name, when this destination is a lane.
    pub(super) fn lane_name(&self) -> Option<&str> {
        match self {
            PublishDestination::Lane { name, .. } => Some(name),
            _ => None,
        }
    }
}

/// Turn the `--target`/`--lane` flags into a destination for a
/// workspace-backed publish run. Mutually exclusive flags, unknown lanes, and
/// conflicting lane configurations are refused here, before anything moves.
pub(super) fn resolve_publish_destination(
    active: &ActiveBundle,
    target: Option<&str>,
    lane: Option<&str>,
) -> Result<PublishDestination> {
    let target = normalize_target_branch(target)?;
    let lane = normalize_lane_name(lane)?;
    match (target, lane) {
        (Some(_), Some(_)) => bail!("Pass only one of --target or --lane."),
        (Some(branch), None) => Ok(PublishDestination::TargetBranch(branch)),
        (None, Some(name)) => {
            let project = load_project_for_bundle(active)?;
            let landing = project
                .as_ref()
                .and_then(|project| project.landing.as_ref());
            let scoped = workspace_is_scoped(active);
            let lane = resolve_lane(project.as_ref(), landing, Some(&name), scoped)?
                .expect("resolve_lane returns a lane when a name is given");
            Ok(PublishDestination::Lane {
                name,
                lane: lane.clone(),
            })
        }
        (None, None) => Ok(PublishDestination::ConfiguredBases),
    }
}

/// Turn `--target` into a destination for an artifact publish run, which has
/// no workspace and therefore no project metadata to resolve a lane from.
pub(super) fn resolve_publish_destination_for_artifact(
    target: Option<&str>,
    lane: Option<&str>,
) -> Result<PublishDestination> {
    let target = normalize_target_branch(target)?;
    let lane = normalize_lane_name(lane)?;
    match (target, lane) {
        (Some(_), Some(_)) => bail!("Pass only one of --target or --lane."),
        (Some(branch), None) => Ok(PublishDestination::TargetBranch(branch)),
        (None, Some(_)) => bail!(
            "Artifact publish cannot resolve --lane: it reads a bundle artifact, not a project's landing.lanes. Pass --target BRANCH instead."
        ),
        (None, None) => Ok(PublishDestination::ConfiguredBases),
    }
}
