//! `knit land remove`: take steps out of a saved plan. Nothing waits on a
//! removed step afterwards, and the edited plan must still validate.

use super::generate::{destination_path, local_project};
use crate::output as out;
use crate::store::{load_active_bundle, read_json, write_json};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::path::Path;

pub fn remove(
    plan_path: Option<&Path>,
    ids: &[String],
    target: Option<&str>,
    lane: Option<&str>,
) -> Result<()> {
    let active = load_active_bundle()?;
    let path = plan_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| destination_path(&active, target, lane));
    if !path.exists() {
        bail!(
            "no landing plan at {}; run `knit land` first",
            path.display()
        );
    }
    let mut plan: Value = read_json(&path)?;
    remove_steps(&mut plan, ids)?;
    let bundle = serde_json::to_value(&active.bundle)?;
    let project = local_project(&active)?;
    let result = super::graph::validation(&plan, Some(&bundle), Some(&project));
    if result["valid"] != true {
        bail!(
            "the plan would no longer be valid without {}: {}",
            ids.join(", "),
            result["errors"]
        );
    }
    write_json(&path, &plan)?;
    for id in ids {
        println!("{} {}", out::ok("removed"), out::node(id));
    }
    println!(
        "{} {}",
        out::heading("Plan file:"),
        out::path(path.display())
    );
    // A synced plan only runs once the edited revision is on the sync remotes.
    crate::commands::remote::landing::sync_finished_run(&active.root, &plan, &[], false).map_err(
        |error| {
            anyhow::anyhow!(
                "removed, but the edited plan could not be synced, so it cannot run yet: {error:#}"
            )
        },
    )?;
    if target.is_none() && lane.is_none() && plan_path.is_none_or(|p| p == path) {
        let mut locked = crate::store::load_active_bundle_for_update()?;
        crate::commands::bundle::close_if_merged(&mut locked, Some(&plan))?;
    }
    Ok(())
}

pub(crate) fn remove_steps(plan: &mut Value, ids: &[String]) -> Result<()> {
    let steps = plan["steps"].as_array_mut().into_iter().flatten();
    let known: Vec<String> = steps
        .filter_map(|s| s["id"].as_str().map(str::to_owned))
        .collect();
    for id in ids {
        if !known.contains(id) {
            bail!(
                "the plan has no step `{id}`; its steps are: {}",
                known.join(", ")
            );
        }
    }
    if let Some(steps) = plan["steps"].as_array_mut() {
        steps.retain(|s| !ids.iter().any(|id| s["id"] == *id));
        for step in steps.iter_mut() {
            for key in ["needs", "requires"] {
                if let Some(list) = step.get_mut(key).and_then(Value::as_array_mut) {
                    list.retain(|need| !ids.iter().any(|id| need == id));
                }
            }
        }
    }
    if let Some(workflow) = plan.get("workflow").cloned() {
        match prune_workflow(&workflow, ids) {
            Some(pruned) => plan["workflow"] = pruned,
            None => {
                plan.as_object_mut().unwrap().remove("workflow");
            }
        }
    }
    Ok(())
}

fn prune_workflow(node: &Value, ids: &[String]) -> Option<Value> {
    if let Some(id) = node.get("step") {
        return (!ids.iter().any(|removed| id == removed)).then(|| node.clone());
    }
    for key in ["sequence", "parallel"] {
        if let Some(children) = node.get(key).and_then(Value::as_array) {
            let kept: Vec<Value> = children
                .iter()
                .filter_map(|child| prune_workflow(child, ids))
                .collect();
            return (!kept.is_empty()).then(|| json!({ key: kept }));
        }
    }
    Some(node.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removed_steps_leave_no_dangling_needs_or_workflow_nodes() {
        let mut plan = json!({
            "steps": [
                {"id": "merge-api", "type": "merge_pr"},
                {"id": "deploy", "type": "deploy", "needs": ["merge-api"], "requires": ["merge-api"]},
                {"id": "verify", "type": "run", "needs": ["deploy"]}
            ],
            "workflow": {"sequence": [{"step": "merge-api"}, {"parallel": [{"step": "deploy"}]}, {"step": "verify"}]}
        });
        remove_steps(&mut plan, &["deploy".to_string()]).unwrap();
        let ids: Vec<_> = plan["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["merge-api", "verify"]);
        assert_eq!(plan["steps"][1]["needs"], json!([]));
        assert_eq!(
            plan["workflow"],
            json!({"sequence": [{"step": "merge-api"}, {"step": "verify"}]})
        );
    }

    #[test]
    fn an_unknown_step_names_the_ones_that_exist() {
        let mut plan = json!({"steps": [{"id": "merge-api"}]});
        let error = remove_steps(&mut plan, &["deploy".to_string()]).unwrap_err();
        assert!(error.to_string().contains("merge-api"), "{error}");
    }
}
