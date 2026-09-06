//! Explicit, checkpointed bundle continuation between machines.
mod in_;
mod out;
pub(crate) mod probe;
pub mod report;
use crate::{
    model::{ambient_origin, ChangeGroup, HandoffLocationState, NodeOrigin},
    store,
};
use anyhow::{bail, Context, Result};
pub use in_::handoff_in;
pub use out::handoff_out;
use std::path::{Path, PathBuf};

pub fn handoff_probe(
    project: Option<&str>,
    slug: Option<&str>,
    workspace: Option<&Path>,
    json: bool,
) -> Result<()> {
    if json {
        crate::output::route_human_lines_to_stderr();
    }
    let (project, slug) = resolve_reference(project, slug)?;
    let root = target_root(&project, workspace)?;
    let (report, _) = prepare_probe(&project, &slug, &root, true);
    report.print(json)?;
    if report.verdict == "fail" {
        return Err(report::ProbeFailed.into());
    }
    Ok(())
}

pub(crate) fn prepare_probe(
    project: &str,
    slug: &str,
    root: &Path,
    allow_unpublished: bool,
) -> (
    report::ProbeReport,
    Option<crate::commands::remote::handoff::HandoffExport>,
) {
    use report::Check;
    let mut remote = match crate::commands::remote::handoff::HandoffExport::fetch(
        project,
        slug,
        allow_unpublished,
    ) {
        Ok(remote) => remote,
        Err(e) => {
            let mut r = report::ProbeReport::default();
            r.add(Check::fail("remote", "Sync remote", format!("{e:#}")));
            return (r, None);
        }
    };
    let requirements = remote
        .project
        .as_ref()
        .and_then(|p| p.requirements.clone())
        .unwrap_or_default();
    let size = remote
        .bundle
        .nodes
        .iter()
        .rev()
        .filter_map(|n| n.handoff.as_ref())
        .find_map(|h| h.size_mib);
    let mut r = probe::system_checks(root, &requirements, size);
    if remote.unpublished {
        r.add(Check::warn("bundle", "Bundle checkpoint", "Bundle not published yet; checking project requirements and repository access. Acceptance will recheck the published checkpoint."));
    }
    r.add(Check::ok(
        "remote",
        "Sync remote",
        "Global remote configured and token valid",
    ));
    if remote.bundle.schema_version != crate::model::SCHEMA_VERSION {
        r.add(Check::fail(
            "schema",
            "Bundle schema",
            format!(
                "Expected {}; found {}",
                crate::model::SCHEMA_VERSION,
                remote.bundle.schema_version
            ),
        ));
    }
    let parent = probe::existing_parent(root).unwrap_or_else(|_| std::env::temp_dir());
    for (id, result) in remote.probe_repositories(&parent) {
        r.add(match result {
            Ok(()) => Check::ok(&format!("forge:{id}"), &id, "Repository reachable"),
            Err(e) => Check::fail(&format!("forge:{id}"), &id, format!("{e:#}")),
        });
    }
    match remote.bundle.handoff_location() {
        Some(location) if location.state == HandoffLocationState::Conflict => r.add(Check::fail(
            "location",
            "Bundle location",
            "Competing handoffs require reconciliation",
        )),
        Some(location)
            if location.state == HandoffLocationState::Active
                && !location
                    .origin
                    .as_ref()
                    .is_some_and(|o| same_origin(o, &ambient_origin())) =>
        {
            let message = format!(
                "Active on {}; source must publish handoff out before acceptance",
                location.label
            );
            r.add(if allow_unpublished {
                Check::warn("location", "Bundle location", message)
            } else {
                Check::fail("location", "Bundle location", message)
            })
        }
        _ => r.add(Check::ok(
            "location",
            "Bundle location",
            "Ready for continuation",
        )),
    }
    if let Err(e) = check_existing_target(root, &remote.bundle) {
        r.add(Check::fail(
            "target-state",
            "Existing checkout",
            format!("{e:#}"),
        ));
    }
    (r, Some(remote))
}

pub(crate) fn target_root(project: &str, workspace: Option<&Path>) -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    if let Some(path) = workspace {
        return Ok(if path.is_absolute() {
            path.into()
        } else {
            cwd.join(path)
        });
    }
    if let Some(root) = store::find_knit_root(&cwd) {
        return Ok(root);
    }
    Ok(cwd.join(project.rsplit('/').next().context("Project ref is empty")?))
}
fn resolve_reference(project: Option<&str>, slug: Option<&str>) -> Result<(String, String)> {
    if let (Some(p), Some(s)) = (project, slug) {
        return Ok((p.into(), s.into()));
    }
    let active = store::load_active_bundle()?;
    Ok((
        project
            .map(str::to_owned)
            .or(active.bundle.project_id)
            .context("No project selected")?,
        slug.unwrap_or(&active.bundle.id).into(),
    ))
}
pub(crate) fn same_origin(left: &NodeOrigin, right: &NodeOrigin) -> bool {
    match (&left.environment_id, &right.environment_id) {
        (Some(a), Some(b)) => a == b,
        (None, None) => left.hostname == right.hostname && left.platform == right.platform,
        _ => false,
    }
}
pub(crate) fn check_git_operation(path: &Path) -> Result<()> {
    for marker in [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-merge",
        "rebase-apply",
        "BISECT_LOG",
    ] {
        let gitpath = crate::git::git_output(path, ["rev-parse", "--git-path", marker])?;
        let p = PathBuf::from(gitpath.trim());
        let p = if p.is_absolute() { p } else { path.join(p) };
        if p.exists() {
            bail!(
                "Finish or abort the Git operation in {} before handoff",
                path.display()
            );
        }
    }
    if !crate::git::git_output(path, ["ls-files", "-u"])?
        .trim()
        .is_empty()
    {
        bail!("Resolve unmerged files in {}", path.display());
    }
    Ok(())
}
pub(crate) fn check_existing_target(root: &Path, remote: &ChangeGroup) -> Result<()> {
    if root.exists()
        && !root.join(".knit/config.json").exists()
        && std::fs::read_dir(root)?.next().is_some()
    {
        bail!("Target directory is not empty and is not a Knit workspace");
    }
    let path = store::bundle_path(root, &remote.id);
    if !path.exists() {
        return Ok(());
    }
    let local: ChangeGroup = store::read_json(&path)?;
    if crate::model::ledger_relation(&local.node_id_sequence(), &remote.node_id_sequence())
        == crate::model::LedgerRelation::Diverged
    {
        bail!(
            "Ledgers diverged; run `knit --bundle {} pull --merge` and retry",
            remote.id
        );
    }
    let active = store::ActiveBundle::unlocked(root.into(), path, local);
    for repo in &active.bundle.repos {
        if let Some(cwd) = crate::checkout::checkout_dir(&active, repo) {
            check_git_operation(&cwd)?;
            if let Some(expected) = remote
                .repos
                .iter()
                .find(|r| r.id == repo.id)
                .and_then(|r| r.remote.as_deref())
            {
                let actual = crate::git::git_output(&cwd, ["remote", "get-url", "origin"])?;
                if !crate::commands::remote::handoff::same_repository_url(actual.trim(), expected) {
                    bail!(
                        "{} has a different repository origin than the handoff",
                        repo.id
                    );
                }
            }

            if crate::pending::path_pending_changes(&cwd)?.any() {
                bail!("{} has uncommitted changes; commit or preserve them before accepting the handoff",repo.id);
            }
            if crate::git::current_branch(&cwd)?.as_deref() != repo.feature_branch.as_deref() {
                bail!("{} checkout is on a different branch", repo.id);
            }
        }
    }
    Ok(())
}
pub fn handoff_status(json: bool) -> Result<()> {
    let active = store::load_active_bundle()?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({"bundleSlug":active.bundle.id,"location":active.bundle.handoff_location(),"out":out::published_out(&active)?})
            )?
        );
    } else {
        print_location(&active.bundle);
    }
    Ok(())
}
pub(crate) fn print_location(bundle: &ChangeGroup) {
    if let Some(l) = bundle.handoff_location() {
        let verb = match l.state {
            HandoffLocationState::Pending => "Awaiting continuation on",
            HandoffLocationState::Active => "Continued on",
            HandoffLocationState::Conflict => "Conflicting handoffs:",
        };
        crate::human!("{verb} {} ({})", l.label, l.updated_at);
    }
}
pub(crate) fn warn_elsewhere(bundle: &ChangeGroup) {
    if let Some(l) = bundle.handoff_location() {
        if l.state != HandoffLocationState::Active
            || !l
                .origin
                .as_ref()
                .is_some_and(|o| same_origin(o, &ambient_origin()))
        {
            eprintln!(
                "Warning: bundle handoff location is {} ({}); sync before continuing edits here.",
                l.label, l.updated_at
            );
        }
    }
}
