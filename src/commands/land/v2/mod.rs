//! Versioned saved-plan entry points. The JSON document is the execution identity;
//! workflow compilation never mutates that document or its hash.
mod generate;
mod graph;
mod runtime;
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
