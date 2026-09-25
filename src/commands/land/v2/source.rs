//! `knit land source` — pin an integration source into a new plan file.
//!
//! Resolves a source branch to its exact current SHA, verifies it still
//! contains the reviewed bundle head, and authors a new plan file with
//! `integrationSources[repo] = {branch, sha}`. The original plan, the bundle
//! fingerprint, and every recorded pin stay untouched; runs keep their
//! immutable plans, so the result is always a fresh file.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use super::mergeability::CAPABILITY_SOURCES;

#[allow(clippy::too_many_arguments)]
pub fn source(
    plan_path: &Path,
    repo_id: &str,
    branch: &str,
    expected_sha: Option<&str>,
    repo_root: Option<&Path>,
    out: &Path,
) -> Result<()> {
    if branch.trim().is_empty() || branch.starts_with('-') {
        bail!("integration source branch must be a nonempty branch name");
    }
    let mut plan: Value = crate::store::read_json(plan_path)?;
    if plan["kind"] != "KnitLandPlan" || plan["schemaVersion"] != "0.2" {
        bail!("integration sources require a schema 0.2 landing plan");
    }
    let merge_branch = plan["steps"].as_array().is_some_and(|steps| {
        steps
            .iter()
            .any(|s| s["type"] == "merge_branch" && s["repoId"].as_str() == Some(repo_id))
    });
    if !merge_branch {
        let review = plan["steps"].as_array().is_some_and(|steps| {
            steps
                .iter()
                .any(|s| s["type"] == "merge_pr" && s["repoId"].as_str() == Some(repo_id))
        });
        if review {
            bail!(
                "{repo_id} merges its recorded review in this plan; integration sources only apply to merge_branch steps. Generate a lane or target plan instead."
            );
        }
        bail!(
            "{repo_id} has no merge_branch step in this plan; integration sources only apply to merge_branch steps"
        );
    }
    let reviewed = plan["bundleHeads"][repo_id]
        .as_str()
        .context(format!(
            "{repo_id}: the plan has no reviewed bundle head to verify provenance against"
        ))?
        .to_owned();
    let root = resolve_root(repo_id, repo_root)?;
    let tip = crate::git::remote_ref_sha(&root, "origin", &format!("refs/heads/{branch}"))?
        .with_context(|| format!("{repo_id}: branch {branch} is missing from origin"))?;
    if let Some(expected) = expected_sha {
        if expected != tip {
            bail!(
                "{repo_id}: branch {branch} resolves to {tip}, not the requested {expected}; refresh the pinned SHA"
            );
        }
    }
    // Bring the branch's objects local so provenance can be verified exactly.
    let missing = |object: &str| {
        crate::git::git_output(&root, ["cat-file", "-e", &format!("{object}^{{commit}}")]).is_err()
    };
    if missing(&tip) || missing(&reviewed) {
        crate::git::git_output(
            &root,
            [
                "fetch",
                "--quiet",
                "--no-tags",
                "origin",
                &format!("+refs/heads/{branch}:refs/knit/landing/source/{repo_id}"),
            ],
        )
        .with_context(|| {
            format!(
                "{repo_id}: fetching integration source branch {branch} from origin failed; provenance cannot be verified"
            )
        })?;
        if missing(&tip) || missing(&reviewed) {
            bail!(
                "{repo_id}: integration source objects for {branch} are unavailable after fetching; provenance cannot be verified"
            );
        }
    }
    if !crate::git::is_ancestor(&root, &reviewed, &tip) {
        bail!(
            "{repo_id}: branch {branch} at {tip} does not include the reviewed bundle head {reviewed}; merge the reviewed work into the integration branch first"
        );
    }
    let absolute_out: PathBuf = if out.is_absolute() {
        out.to_path_buf()
    } else {
        std::env::current_dir()?.join(out)
    };
    let absolute_plan: PathBuf = if plan_path.is_absolute() {
        plan_path.to_path_buf()
    } else {
        std::env::current_dir()?.join(plan_path)
    };
    if absolute_out == absolute_plan {
        bail!(
            "refusing to overwrite the source plan {}; integration sources author a new plan file",
            absolute_plan.display()
        );
    }
    if is_revision_path(&absolute_out) {
        bail!(
            "refusing to write into the immutable plan revisions directory; choose a new plan path"
        );
    }
    if absolute_out.exists() {
        bail!(
            "plan file {} already exists; integration sources author a new file, never replace one",
            absolute_out.display()
        );
    }
    plan["integrationSources"][repo_id] = json!({"branch": branch, "sha": tip});
    // New semantics are gated behind executor 0.4. Never downgrade: an
    // unknown future requirement is rejected, and only older or absent
    // versions are upgraded.
    match plan["requiredExecutorVersion"].as_str() {
        Some("0.4") => {}
        Some("0.2" | "0.3") | None => plan["requiredExecutorVersion"] = json!("0.4"),
        Some(other) => bail!(
            "plan requires executor version {other}, which this authoring does not know; refusing to downgrade or rewrite it"
        ),
    }
    let mut capabilities = super::graph::strings(&plan["requiredCapabilities"]);
    if !capabilities.iter().any(|c| c == CAPABILITY_SOURCES) {
        capabilities.push(CAPABILITY_SOURCES.to_owned());
        capabilities.sort();
        capabilities.dedup();
        plan["requiredCapabilities"] = json!(capabilities);
    }
    let result = super::graph::validation(&plan, None, None);
    if result["valid"] != true {
        bail!(
            "authored plan is invalid with this integration source: {}",
            result["errors"]
        );
    }
    if let Some(parent) = absolute_out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::store::write_json(&absolute_out, &plan)?;
    println!(
        "{} {repo_id} integration source {branch} -> {tip}; plan {} (hash {})",
        crate::output::ok("pinned"),
        absolute_out.display(),
        super::graph::canonical_hash(&plan)
    );
    Ok(())
}

/// Immutable plan revisions live under `<...>/land-plans/revisions/`; never
/// author into them.
fn is_revision_path(path: &Path) -> bool {
    let mut saw_land_plans = false;
    for component in path.components() {
        match component.as_os_str().to_str() {
            Some("land-plans") => saw_land_plans = true,
            Some("revisions") if saw_land_plans => return true,
            _ => saw_land_plans = false,
        }
    }
    false
}

fn resolve_root(repo_id: &str, explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        let path = dunce::canonicalize(path).with_context(|| format!("{}", path.display()))?;
        if !path.is_dir() {
            bail!("--repo-root {} is not a directory", path.display());
        }
        return Ok(path);
    }
    let active = crate::store::load_active_bundle()?;
    if let Some(repo) = active.bundle.repos.iter().find(|r| r.id == repo_id) {
        if let Some(path) = crate::checkout::checkout_dir(&active, repo) {
            return Ok(dunce::canonicalize(path)?);
        }
        return Ok(dunce::canonicalize(&repo.path)?);
    }
    if let Ok(project) = super::generate::local_project(&active) {
        if let Some(path) = project["repos"].as_array().and_then(|repos| {
            repos
                .iter()
                .find(|r| r["id"].as_str() == Some(repo_id))
                .and_then(|r| r["path"].as_str())
                .map(PathBuf::from)
        }) {
            let path = if path.is_absolute() {
                path
            } else {
                active.root.join(path)
            };
            if path.is_dir() {
                return Ok(dunce::canonicalize(path)?);
            }
        }
    }
    bail!("{repo_id}: no checkout found; pass --repo-root with the repository's path");
}
