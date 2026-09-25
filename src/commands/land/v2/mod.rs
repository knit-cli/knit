//! Versioned saved-plan entry points. The JSON document is the execution identity;
//! workflow compilation never mutates that document or its hash.
mod destinations;
mod generate;
pub use destinations::destinations;
mod graph;
mod mergeability;
pub(crate) use mergeability::KnownNoEffect;
mod preflight;
pub use preflight::preflight;
mod runtime;
mod sequence;
mod source;
pub use source::source;
#[cfg(test)]
mod tests;
use crate::store::read_json;
use anyhow::{bail, Result};
pub use generate::generate;
pub(super) use generate::{destination_path, display_plan};
pub(crate) use graph::{canonical_hash, validation};
pub use runtime::{apply, apply_with_checks, recover};
pub(super) use runtime::{immutable_plan_path, local_apply};
use serde_json::Value;
use std::path::Path;

pub fn validate(
    plan: &Path,
    bundle: Option<&Path>,
    project: Option<&Path>,
    _json: bool,
) -> Result<()> {
    let plan: Value = read_json(plan)?;
    let bundle = bundle.map(read_json::<Value>).transpose()?;
    let project = project.map(read_json::<Value>).transpose()?;
    let result = validation(&plan, bundle.as_ref(), project.as_ref());
    println!("{}", serde_json::to_string(&result)?);
    if result["valid"] != true {
        bail!("Landing plan is invalid");
    }
    Ok(())
}

/// Check the exact in-memory document which will be executed, not an earlier read.
pub(super) fn verify_expected_hash(plan: &Value, expected: Option<&str>) -> Result<()> {
    if let Some(expected) = expected {
        if expected.len() != 64
            || !expected
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            bail!("--expected-plan-hash must be a lowercase canonical SHA-256");
        }
        if canonical_hash(plan) != expected {
            bail!("Landing plan hash differs from --expected-plan-hash; the plan changed after review. Review the current document before applying.");
        }
    }
    Ok(())
}

pub fn show(path: &Path, target: Option<&str>, lane: Option<&str>) -> Result<()> {
    let active = crate::store::load_active_bundle()?;
    let plan: Value = read_json(path)?;
    if plan["kind"] != "KnitLandPlan" || plan["bundleId"] != active.bundle.id {
        bail!("Saved plan does not belong to this bundle");
    }
    if target.is_some() || lane.is_some() {
        let typed = serde_json::from_value(plan.clone())?;
        super::ensure_requested_selection_matches_plan(&active, target, lane, &typed)?;
    }
    display_plan(&active, &plan, path)
}
