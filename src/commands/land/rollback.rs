//! `knit land rollback` — compensate a failed landing run.
//!
//! A land run that fails halfway can leave an arbitrary subset of PRs merged
//! into their base branches. Merged PRs cannot be un-merged, so rollback is
//! compensation, not reset: for every merge step the failed run completed,
//! create a provider-side revert PR (reusing the `knit revert` machinery) and
//! record a `pr.revert` ledger node targeting the run. The run is then marked
//! rolled back so `knit land resume` refuses to continue it.

use super::{resolve_land_run_path, LandRun, LandStatus, LandStepKind};
use crate::commands::revert::{create_provider_revert_prs, provider_revert_context};
use crate::output as out;
use crate::store::{
    load_active_bundle, load_active_bundle_for_update, read_json, write_json, ActiveBundle,
};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::path::Path;

pub fn rollback_land_run(run_path: Option<&Path>, apply: bool) -> Result<()> {
    let mut active = if apply {
        load_active_bundle_for_update()?
    } else {
        load_active_bundle()?
    };
    let path = resolve_land_run_path(&active, run_path)?
        .with_context(|| "No land run found. Run `knit land apply` first.")?;
    let mut run: LandRun = read_json(&path)?;
    if run.bundle_id != active.bundle.id {
        bail!(
            "Land run {} belongs to bundle {}, but resolved bundle is {}.",
            run.id,
            run.bundle_id,
            active.bundle.id
        );
    }
    if let Some(rolled_back_at) = &run.rolled_back_at {
        bail!(
            "Land run {} was already rolled back at {rolled_back_at}.",
            run.id
        );
    }
    match run.status {
        LandStatus::Failed => {}
        LandStatus::Succeeded => bail!(
            "Land run {} succeeded. Use `knit revert <landed-node> --apply` to revert a fully landed bundle.",
            run.id
        ),
        status => bail!(
            "Land run {} is {status}, expected failed. Only failed runs can be rolled back.",
            run.id
        ),
    }

    let merged = merged_steps(&run);
    if merged.is_empty() {
        bail!(
            "Land run {} merged no PRs before failing; there is nothing to roll back. Fix the issue and run `knit land resume`.",
            run.id
        );
    }

    println!(
        "{} {} {}",
        out::heading("Land rollback"),
        out::node(&run.id),
        out::muted(format!("({} merged PR(s))", merged.len()))
    );
    println!(
        "{} {}",
        out::heading("Run file:"),
        out::path(path.display())
    );
    println!("{} {}", out::heading("Provider:"), run.provider);
    println!();

    preflight_merged(&active, &run, &merged)?;

    if !apply {
        println!();
        println!(
            "{} knit land rollback --apply",
            out::heading("Create revert PRs:")
        );
        return Ok(());
    }

    let group_id = rollback_merged_steps(&mut active, &mut run, &path)?
        .expect("merged steps were checked to be non-empty");
    println!(
        "{} {} {}",
        out::heading("Rolled back"),
        out::node(&run.id),
        out::muted(format!("revert group {group_id}"))
    );
    Ok(())
}

/// Create revert PRs for every merge step the run completed, then mark the
/// run rolled back. Returns the revert group id, or `None` when the run had
/// no merged steps. Shared by `knit land rollback --apply` and the
/// `onFailure: rollback` path in the executor.
pub(super) fn rollback_merged_steps(
    active: &mut ActiveBundle,
    run: &mut LandRun,
    run_path: &Path,
) -> Result<Option<String>> {
    let merged = merged_steps(run);
    if merged.is_empty() {
        return Ok(None);
    }

    let group_id = create_provider_revert_prs(
        active,
        Some(&run.provider),
        &run.id,
        &format!("failed land run {}", run.id),
        &merged,
    )?;
    run.rolled_back_at = Some(now_iso());
    run.updated_at = now_iso();
    write_json(run_path, run)?;
    Ok(Some(group_id))
}

/// The `(repo id, PR url)` pairs for every merge step the run completed. A
/// succeeded merge step always records its publication URL.
fn merged_steps(run: &LandRun) -> Vec<(String, String)> {
    run.steps
        .iter()
        .filter(|step| {
            step.step_type == LandStepKind::MergePr && step.status == LandStatus::Succeeded
        })
        .filter_map(|step| Some((step.repo_id.clone()?, step.publication_url.clone()?)))
        .collect()
}

/// Verify each recorded merge actually shows as MERGED on the provider and
/// print its live state, so the preview reflects reality and apply fails
/// before creating any revert PR.
fn preflight_merged(
    active: &ActiveBundle,
    run: &LandRun,
    merged: &[(String, String)],
) -> Result<()> {
    for (repo_id, url) in merged {
        let (_, target, forge) = provider_revert_context(active, Some(&run.provider), repo_id)?;
        let pr = forge
            .view(&target, url)
            .with_context(|| format!("{repo_id}: failed to load {url}"))?;
        let state = pr.state.as_deref().unwrap_or("UNKNOWN");
        println!(
            "{} {} #{} {} {}",
            out::repo(repo_id),
            out::movement("prRevert"),
            pr.number,
            out::status(state),
            out::muted(url)
        );
        if state != "MERGED" {
            bail!(
                "{repo_id}: PR #{} is {state}, expected MERGED. The run recorded this merge as succeeded; refresh with `knit land status` and retry.",
                pr.number
            );
        }
    }
    Ok(())
}
