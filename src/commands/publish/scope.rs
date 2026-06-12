//! Repo selection for publish: which tracked repos are in publishing scope,
//! provider filtering, and per-repo base branch overrides.

use crate::model::{ChangeGroup, RepoEntry};
use crate::providers::{self};
use crate::repo_selectors::resolve_repo_indexes;
use crate::store::ActiveBundle;
use anyhow::{bail, Result};
use std::collections::{BTreeMap, BTreeSet};

/// Narrow resolved repo indexes to those hosted on `provider` (e.g. "github",
/// "gitlab", "forgejo"/"codeberg"). With no provider the indexes pass through
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
                "unknown provider `{requested}`. Known providers: github, gitlab, forgejo."
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

pub(super) fn publish_scope_repo_ids(bundle: &ChangeGroup) -> BTreeSet<String> {
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

#[derive(Debug, Default)]
pub(super) struct BaseOverrides {
    default: Option<String>,
    per_repo: BTreeMap<String, String>,
}

impl BaseOverrides {
    pub(super) fn parse(values: &[String]) -> Result<Self> {
        let mut overrides = Self::default();
        for value in values {
            let value = value.trim();
            if value.is_empty() {
                bail!("--base cannot be empty.");
            }
            if let Some((repo_id, branch)) = value.split_once('=') {
                let repo_id = repo_id.trim();
                let branch = branch.trim();
                if repo_id.is_empty() || branch.is_empty() {
                    bail!("Use --base REPO=BRANCH with both sides present.");
                }
                overrides
                    .per_repo
                    .insert(crate::ids::slugify(repo_id), branch.to_string());
            } else if overrides.default.replace(value.to_string()).is_some() {
                bail!("Pass only one default --base value, or use repeated --base REPO=BRANCH overrides.");
            }
        }
        Ok(overrides)
    }

    pub(super) fn branch_for(
        &self,
        repo: &RepoEntry,
        existing: Option<&crate::model::PublicationEntry>,
    ) -> String {
        self.per_repo
            .get(&repo.id)
            .or(self.default.as_ref())
            .cloned()
            .or_else(|| existing.map(|publication| publication.base_branch.clone()))
            .unwrap_or_else(|| repo.base_branch.clone())
    }

    pub(super) fn validate_tracked_repos(&self, bundle: &ChangeGroup) -> Result<()> {
        for repo_id in self.per_repo.keys() {
            if !bundle.repos.iter().any(|repo| &repo.id == repo_id) {
                bail!("--base references unknown repo `{repo_id}`.");
            }
        }
        Ok(())
    }
}
