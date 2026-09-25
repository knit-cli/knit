//! Opt-in repository-sequence execution.
//!
//! A project (or one lane/target scope) may declare
//! `landing.execution = { "mode": "repository_sequence", "repoOrder": [...] }`.
//! Generation then compiles every repository's merge/build/deploy/verify steps
//! into an explicit `workflow` sequence: all of repository A's steps, then all
//! of repository B's, in the declared order. Declared `needs`/`requires` and
//! cross-repo dependencies are honored, and anything the declared order cannot
//! satisfy — a dependency on a later repository, an undeclared repository, a
//! cycle, an ambiguous step — is rejected with a named error instead of being
//! silently dropped from the workflow.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const CAPABILITY: &str = "repository-sequence";
pub(crate) const MODE: &str = "repository_sequence";

/// Parse and validate an `execution` configuration block. `None` when the
/// scope declares no execution mode (the default workflow stays in force).
pub(crate) fn parse_execution(config: &Value, scope: &str) -> Result<Option<Vec<String>>> {
    let Some(config) = config.as_object() else {
        if config.is_null() {
            return Ok(None);
        }
        bail!("{scope}: execution must be an object");
    };
    if config.is_empty() {
        return Ok(None);
    }
    let mode = config
        .get("mode")
        .and_then(Value::as_str)
        .with_context(|| format!("{scope}: execution.mode is required"))?;
    if mode != MODE {
        bail!("{scope}: unsupported execution.mode `{mode}`; the only supported mode is `{MODE}`");
    }
    let order = config
        .get("repoOrder")
        .and_then(Value::as_array)
        .with_context(|| format!("{scope}: execution.repoOrder is required"))?;
    if order.is_empty() {
        bail!("{scope}: execution.repoOrder must declare at least one repository");
    }
    let mut repos = Vec::new();
    for entry in order {
        let repo = entry
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .with_context(|| {
                format!("{scope}: execution.repoOrder entries must be nonempty repository IDs")
            })?;
        if repos.iter().any(|r| r == repo) {
            bail!("{scope}: execution.repoOrder repeats repository {repo}");
        }
        repos.push(repo.to_owned());
    }
    Ok(Some(repos))
}

/// Compile the plan's steps into an explicit per-repository workflow sequence.
///
/// Every step must belong to exactly one declared repository (by `repoId`);
/// steps without a repository run last, after every repository group. The
/// declared order must satisfy every declared dependency: a step may only
/// need steps from its own group or an earlier group. Within a group the
/// steps are ordered topologically by their declared needs, so no data
/// dependency is dropped when the workflow replaces the authored `needs`.
pub(crate) fn compile_workflow(steps: &[Value], repo_order: &[String]) -> Result<Value> {
    let position: BTreeMap<&str, usize> = repo_order
        .iter()
        .enumerate()
        .map(|(i, r)| (r.as_str(), i))
        .collect();
    let id_of = |step: &Value| step["id"].as_str().unwrap_or("").to_owned();
    let mut groups: Vec<Vec<&Value>> = vec![Vec::new(); repo_order.len() + 1];
    let mut group_of: BTreeMap<String, usize> = BTreeMap::new();
    for step in steps {
        let id = id_of(step);
        let group = match step["repoId"].as_str() {
            Some(repo) => *position
                .get(repo)
                .with_context(|| format!(
                    "execution repository_sequence does not declare repository {repo} for step {id}; add it to execution.repoOrder"
                ))?,
            None => repo_order.len(),
        };
        group_of.insert(id, group);
        groups[group].push(step);
    }
    // Cross-group dependencies must point backwards in the declared order.
    // Declared `requires` count as dependencies too: a producer constraint
    // the order cannot satisfy is a contradiction, not a detail to drop.
    for step in steps {
        let id = id_of(step);
        let group = group_of[&id];
        for need in super::graph::strings(&step["needs"])
            .into_iter()
            .chain(super::graph::strings(&step["requires"]))
        {
            if let Some(&other) = group_of.get(&need) {
                if other > group {
                    let step_repo = step["repoId"].as_str().unwrap_or("<no repository>");
                    let need_repo = steps
                        .iter()
                        .find(|s| id_of(s) == need)
                        .and_then(|s| s["repoId"].as_str())
                        .unwrap_or("<no repository>");
                    bail!(
                        "execution repository order contradicts a declared dependency: step {id} (repository {step_repo}, position {}) needs {need} (repository {need_repo}, position {}); declare {need_repo} before {step_repo} or drop the dependency",
                        group + 1,
                        other + 1
                    );
                }
            } else {
                bail!("step {id} needs unknown step {need}");
            }
        }
    }
    let mut sequence = vec![];
    for (index, group) in groups.iter().enumerate() {
        if group.is_empty() {
            continue;
        }
        let ordered = topological_group(group, index, repo_order)?;
        sequence
            .push(json!({"sequence": ordered.into_iter().map(|id| json!({"step": id})).collect::<Vec<_>>()}));
    }
    if sequence.is_empty() {
        bail!("execution repository_sequence compiled an empty workflow");
    }
    Ok(json!({"sequence": sequence}))
}

/// Order one repository's steps by their in-group needs. Independent steps
/// keep authoring order. A stall means the declared needs form a cycle.
fn topological_group(group: &[&Value], index: usize, repo_order: &[String]) -> Result<Vec<String>> {
    let label = repo_order
        .get(index)
        .map(|r| format!("repository {r}"))
        .unwrap_or_else(|| "steps without a repository".to_owned());
    let mut remaining: Vec<&Value> = group.to_vec();
    let mut emitted: BTreeSet<String> = BTreeSet::new();
    let mut ordered = vec![];
    while !remaining.is_empty() {
        let mut progressed = false;
        let mut rest = vec![];
        for step in remaining {
            let ready = super::graph::strings(&step["needs"])
                .into_iter()
                .chain(super::graph::strings(&step["requires"]))
                .all(|need| {
                    !group
                        .iter()
                        .any(|other| other["id"].as_str() == Some(need.as_str()))
                        || emitted.contains(&need)
                });
            if ready {
                ordered.push(step["id"].as_str().unwrap().to_owned());
                emitted.insert(step["id"].as_str().unwrap().to_owned());
                progressed = true;
            } else {
                rest.push(step);
            }
        }
        if !progressed {
            let stuck: Vec<String> = rest
                .iter()
                .map(|s| s["id"].as_str().unwrap().to_owned())
                .collect();
            bail!(
                "execution repository_sequence found a dependency cycle among {}'s steps: {}",
                label,
                stuck.join(", ")
            );
        }
        remaining = rest;
    }
    Ok(ordered)
}

/// Validate a saved plan's `execution` block (shape only; the workflow itself
/// is validated by the shared graph compiler).
pub(crate) fn validate_plan_execution(plan: &Value) -> Result<()> {
    let Some(execution) = plan.get("execution") else {
        return Ok(());
    };
    if !execution.is_object() {
        bail!("execution must be an object");
    }
    if execution.as_object().unwrap().is_empty() {
        bail!("execution must declare a mode");
    }
    if execution["mode"] != MODE {
        bail!(
            "unsupported execution.mode `{}`; the only supported mode is `{MODE}`",
            execution["mode"].as_str().unwrap_or("<missing>")
        );
    }
    let order = execution["repoOrder"]
        .as_array()
        .context("execution.repoOrder must be an array of repository IDs")?;
    if order.is_empty() {
        bail!("execution.repoOrder must declare at least one repository");
    }
    let mut seen = BTreeSet::new();
    let mut repos = Vec::new();
    for entry in order {
        let repo = entry
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .context("execution.repoOrder entries must be nonempty repository IDs")?;
        if !seen.insert(repo.to_owned()) {
            bail!("execution.repoOrder repeats repository {repo}");
        }
        repos.push(repo.to_owned());
    }
    // The declared order must not silently contradict the plan's actual
    // workflow: an execution declaration always ships with its compiled
    // workflow, and the workflow's compiled dependency edges must really
    // serialize the repositories — every step of a later repository must
    // transitively depend on every step of every earlier one. A parallel
    // node that spans repositories does not create those edges, so it is
    // rejected rather than read as a sequence.
    let Some(workflow) = plan.get("workflow").filter(|w| w.is_object()) else {
        bail!(
            "execution declares repository_sequence but the plan has no workflow; the declared order is not actually enforced"
        );
    };
    let _ = workflow;
    let (compiled, _) = super::graph::compile(plan)?;
    let mut ancestors: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    fn visit(id: &str, steps: &[Value], ancestors: &mut BTreeMap<String, BTreeSet<String>>) {
        if ancestors.contains_key(id) {
            return;
        }
        let mut all = BTreeSet::new();
        if let Some(step) = steps.iter().find(|s| s["id"].as_str() == Some(id)) {
            for need in super::graph::strings(&step["needs"]) {
                visit(&need, steps, ancestors);
                all.extend(ancestors.get(&need).cloned().unwrap_or_default());
                all.insert(need);
            }
        }
        ancestors.insert(id.to_owned(), all);
    }
    for step in &compiled {
        visit(step["id"].as_str().unwrap(), &compiled, &mut ancestors);
    }
    for step in &compiled {
        if let Some(repo) = step["repoId"].as_str() {
            if !repos.iter().any(|r| r == repo) {
                bail!(
                    "execution.repository_sequence does not declare repository {repo} (step {}); add it to repoOrder or remove the step",
                    step["id"].as_str().unwrap_or("")
                );
            }
        }
    }
    let group_of = |step: &Value| -> Option<usize> {
        let repo = step["repoId"].as_str()?;
        repos.iter().position(|r| r == repo).map(|i| i + 1)
    };
    for earlier in &compiled {
        let earlier_group = group_of(earlier);
        for later in &compiled {
            if earlier["id"] == later["id"] {
                continue;
            }
            let later_group = group_of(later);
            let (Some(eg), Some(lg)) = (earlier_group, later_group) else {
                continue;
            };
            if lg <= eg {
                continue;
            }
            let id = later["id"].as_str().unwrap();
            let dependency = earlier["id"].as_str().unwrap();
            if !ancestors[id].contains(dependency) {
                bail!(
                    "workflow contradicts execution.repository_sequence: {} (repository {}) does not depend on {} (repository {}); parallel cross-repository grouping cannot implement the declared order. Regenerate the plan so the workflow matches the declared repository order",
                    id,
                    later["repoId"].as_str().unwrap_or("<no repository>"),
                    dependency,
                    earlier["repoId"].as_str().unwrap_or("<no repository>")
                );
            }
        }
    }
    Ok(())
}
