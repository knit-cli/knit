//! `knit land preflight` — read-only structural and live readiness report.
//!
//! Given an exact saved plan, reports whether the document is structurally
//! valid (the shared graph validation, against the bundle and project when
//! available) and whether its pending branch merges are live-ready (fresh
//! target tips, integration source provenance, and — when the plan opted in
//! with `preflight.mergeability = "all"` — a full conflict simulation in
//! isolated temp checkouts). Nothing is mutated: no remote ref changes, no
//! source checkout writes, no run receipts.
//!
//! The JSON contract is machine-readable for an attached local UI handoff:
//! `{valid, planHash, structural, live, checks[], errors[], handoff}` where
//! each check is `{repoId, sourceBranch, sourceSha, targetBranch, targetSha,
//! status, conflicts[]}`.

use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

type Roots = BTreeMap<String, PathBuf>;

pub fn preflight(
    plan_path: &Path,
    artifact: Option<&Path>,
    project_file: Option<&Path>,
    roots_file: Option<&Path>,
    json_output: bool,
) -> Result<()> {
    let plan: Value = crate::store::read_json(plan_path)?;
    if plan["kind"] != "KnitLandPlan" {
        bail!("{} is not a landing plan", plan_path.display());
    }
    let local = artifact.is_none();
    let active = local.then(crate::store::load_active_bundle).transpose()?;
    let bundle: Value = match artifact {
        Some(path) => crate::store::read_json(path)?,
        None => {
            let active = active.as_ref().unwrap();
            if plan["bundleId"] != active.bundle.id {
                bail!("plan belongs to a different bundle");
            }
            serde_json::to_value(&active.bundle)?
        }
    };
    let project: Option<Value> = match project_file {
        Some(path) => Some(crate::store::read_json(path)?),
        None if local => {
            let active = active.as_ref().unwrap();
            let project = super::generate::local_project(active)?;
            (!project.is_null()).then_some(project)
        }
        None => None,
    };
    let structural = super::graph::validation(&plan, Some(&bundle), project.as_ref());
    let mut errors: Vec<String> = structural["errors"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    let roots = resolve_roots(local, roots_file, &bundle, project.as_ref())?;
    // Live readiness: pending branch merges only; already-merged work keeps
    // its receipts and is not the preflight's business. Report mode lists
    // every pending branch merge, but without a declared policy the entries
    // are informational — no simulation, no requirements.
    let is_done = |_: &str| false;
    let (checks, live_errors) = super::mergeability::mergeability_checks(
        &plan,
        &roots,
        &bundle,
        &is_done,
        super::mergeability::CheckMode::Report,
    );
    errors.extend(live_errors.iter().cloned());
    let live_valid = live_errors.is_empty();
    let valid = structural["valid"] == true && live_valid;
    let report = json!({
        "valid": valid,
        "planHash": super::graph::canonical_hash(&plan),
        "bundleId": plan["bundleId"],
        "structural": {"valid": structural["valid"] == true, "errors": structural["errors"]},
        "live": {
            "mergeability": if super::mergeability::mergeability_enabled(&plan) { json!("all") } else { Value::Null },
            "valid": live_valid,
        },
        "checks": super::mergeability::checks_to_json(&checks),
        "errors": errors,
        "handoff": {
            "planPath": absolute(plan_path)?,
            "repoRoots": roots,
            "environment": plan["lane"]
                .as_str()
                .or_else(|| plan["targetBranch"].as_str())
                .map(|name| json!(name))
                .unwrap_or(Value::Null),
        },
    });
    if json_output {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "Landing preflight for plan {} ({})",
            plan_path.display(),
            report["planHash"].as_str().unwrap_or("")
        );
        for check in &checks {
            println!(
                "  {} {} -> {}: {} {}",
                check.repo_id,
                check.source_sha.get(..12).unwrap_or(""),
                check.target_branch,
                check.status,
                if check.conflicts.is_empty() {
                    String::new()
                } else {
                    format!(
                        "({})",
                        check
                            .conflicts
                            .iter()
                            .filter_map(|c| c["file"].as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            );
        }
        for error in &errors {
            println!("  error: {error}");
        }
        if !super::mergeability::mergeability_enabled(&plan) {
            println!("  mergeability policy absent: no merge simulation required");
        }
    }
    if !valid {
        bail!("landing preflight failed");
    }
    Ok(())
}

fn absolute(path: &Path) -> Result<PathBuf> {
    Ok(if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    })
}

fn resolve_roots(
    local: bool,
    roots_file: Option<&Path>,
    bundle: &Value,
    project: Option<&Value>,
) -> Result<Roots> {
    if let Some(path) = roots_file {
        return crate::store::read_json(path);
    }
    if !local {
        return Ok(Roots::new());
    }
    let active = crate::store::load_active_bundle()?;
    let mut roots: Roots = active
        .bundle
        .repos
        .iter()
        .filter_map(|r| crate::checkout::checkout_dir(&active, r).map(|p| (r, p)))
        .map(|(r, p)| Ok((r.id.clone(), dunce::canonicalize(p)?)))
        .collect::<Result<_>>()?;
    let _ = bundle;
    if let Some(repos) = project.and_then(|p| p["repos"].as_array()) {
        for repo in repos {
            if let (Some(id), Some(path)) = (repo["id"].as_str(), repo["path"].as_str()) {
                if roots.contains_key(id) {
                    continue;
                }
                let path = PathBuf::from(path);
                let path = if path.is_absolute() {
                    path
                } else {
                    active.root.join(path)
                };
                if path.is_dir() {
                    roots.insert(id.to_owned(), dunce::canonicalize(path)?);
                }
            }
        }
    }
    Ok(roots)
}
