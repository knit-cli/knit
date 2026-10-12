//! Merge order for landing revert reviews: every ordering the forward landing
//! had between two merges is turned around, and merges it left unordered stay
//! unordered, so their reverts merge in the same wave.

use super::plan::is_merge_step;
use super::{resolve_stored_path, LandStep};
use crate::store::{read_json, ActiveBundle};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// `(earlier, later)` repository pairs: the forward landing merged `earlier`
/// before `later`, directly or through other steps.
pub(super) type Precedence = BTreeSet<(String, String)>;

/// The precedence of the plan the forward landing actually executed, read
/// from its run. `None` when that run is not available here.
pub(super) fn executed_forward_precedence(active: &ActiveBundle) -> Option<Precedence> {
    let compensation = active
        .bundle
        .nodes
        .iter()
        .rev()
        .find(|node| node.node_type == "pr.revert")?;
    let landing = active.bundle.nodes.iter().find(|node| {
        node.node_type == "feature.landed" && Some(&node.id) == compensation.target_node_id.as_ref()
    })?;
    let run_id = landing.run_id.as_deref()?;
    let dir = active.root.join(".knit/land-runs");
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let Ok(run) = read_json::<Value>(&entry.path()) else {
            continue;
        };
        if run["id"].as_str() != Some(run_id) {
            continue;
        }
        if run["plan"].is_object() {
            return Some(precedence(&run["plan"]));
        }
        let plan = run["planPath"]
            .as_str()
            .and_then(|path| read_json::<Value>(&resolve_stored_path(&active.root, path)).ok())?;
        return Some(precedence(&plan));
    }
    None
}

/// Which merges a plan orders before which. A workflow, when present, is
/// the plan's whole ordering; otherwise the steps' `needs` are.
pub(super) fn precedence(plan: &Value) -> Precedence {
    let steps = plan["steps"].as_array().cloned().unwrap_or_default();
    let merges: BTreeMap<String, String> = steps
        .iter()
        .filter(|step| matches!(step["type"].as_str(), Some("merge_pr" | "merge_branch")))
        .filter_map(|step| Some((step["id"].as_str()?.into(), step["repoId"].as_str()?.into())))
        .collect();
    let mut predecessors: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    match plan.get("workflow") {
        Some(workflow) => {
            workflow_predecessors(workflow, &BTreeSet::new(), &mut predecessors);
        }
        None => {
            for step in &steps {
                if let Some(id) = step["id"].as_str() {
                    predecessors.insert(
                        id.into(),
                        step["needs"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect(),
                    );
                }
            }
        }
    }
    let mut pairs = Precedence::new();
    for (step, later) in &merges {
        let mut seen = BTreeSet::new();
        let mut pending: Vec<String> = predecessors
            .get(step)
            .into_iter()
            .flatten()
            .cloned()
            .collect();
        while let Some(id) = pending.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(earlier) = merges.get(&id) {
                if earlier != later {
                    pairs.insert((earlier.clone(), later.clone()));
                }
            }
            pending.extend(predecessors.get(&id).into_iter().flatten().cloned());
        }
    }
    pairs
}

/// Direct predecessors of each step in a workflow tree: a sequence orders its
/// children, a parallel group does not.
fn workflow_predecessors(
    node: &Value,
    prior: &BTreeSet<String>,
    predecessors: &mut BTreeMap<String, BTreeSet<String>>,
) -> BTreeSet<String> {
    if let Some(id) = node["step"].as_str() {
        predecessors.insert(id.into(), prior.clone());
        return BTreeSet::from([id.to_owned()]);
    }
    let sequential = node.get("sequence").is_some();
    let children = node[if sequential { "sequence" } else { "parallel" }]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut frontier = prior.clone();
    let mut ends = BTreeSet::new();
    for child in &children {
        let child_ends = workflow_predecessors(
            child,
            if sequential { &frontier } else { prior },
            predecessors,
        );
        if sequential {
            frontier = child_ends;
        } else {
            ends.extend(child_ends);
        }
    }
    if sequential {
        frontier
    } else {
        ends
    }
}

/// Rewrite a plan's merges for landing revert reviews. A merge keeps needs on
/// steps that wait for no merge; every needs edge between merges is replaced
/// by the turned-around forward precedence, reduced to its direct links.
/// Other steps keep their needs: a step that waited for a merge now waits for
/// that repository's revert.
pub(super) fn reverse_merges(steps: &mut [LandStep], forward: &Precedence) {
    let merge_repos: BTreeMap<String, String> = steps
        .iter()
        .filter(|step| is_merge_step(step))
        .filter_map(|step| Some((step.id.clone(), step.repo_id.clone()?)))
        .collect();
    let step_for_repo: BTreeMap<&str, &str> = merge_repos
        .iter()
        .map(|(id, repo)| (repo.as_str(), id.as_str()))
        .collect();
    let after_merges = steps_after_merges(steps, &merge_repos);
    let before = |earlier: &str, later: &str| forward.contains(&(earlier.into(), later.into()));

    let mut reversed_needs = BTreeMap::new();
    for (id, repo) in &merge_repos {
        let needs: Vec<String> = step_for_repo
            .iter()
            .filter(|(later, _)| before(repo, later))
            .filter(|(later, _)| {
                !step_for_repo
                    .keys()
                    .any(|between| before(repo, between) && before(between, later))
            })
            .map(|(_, step)| (*step).to_owned())
            .collect();
        reversed_needs.insert(id.clone(), needs);
    }
    for step in steps.iter_mut() {
        let Some(mut needs) = reversed_needs.remove(&step.id) else {
            continue;
        };
        let kept = step
            .needs
            .iter()
            .filter(|need| !merge_repos.contains_key(*need) && !after_merges.contains(*need))
            .cloned()
            .collect::<Vec<_>>();
        needs.splice(0..0, kept);
        step.needs = needs;
    }

    // List position does not order a plan; reading the merges in the order
    // they will run does help whoever inspects it.
    let slots: Vec<usize> = (0..steps.len())
        .filter(|&index| is_merge_step(&steps[index]))
        .collect();
    let mut merges: Vec<LandStep> = slots.iter().map(|&index| steps[index].clone()).collect();
    merges.sort_by_key(|step| {
        let repo = step.repo_id.as_deref().unwrap_or_default();
        std::cmp::Reverse(
            step_for_repo
                .keys()
                .filter(|earlier| before(earlier, repo))
                .count(),
        )
    });
    for (index, step) in slots.into_iter().zip(merges) {
        steps[index] = step;
    }
}

/// Steps that wait, directly or not, for a merge. A merge cannot keep a need
/// on one of them once merges run the other way round.
fn steps_after_merges(steps: &[LandStep], merges: &BTreeMap<String, String>) -> BTreeSet<String> {
    let mut after: BTreeSet<String> = BTreeSet::new();
    loop {
        let grown = steps
            .iter()
            .filter(|step| !after.contains(&step.id))
            .filter(|step| {
                step.needs
                    .iter()
                    .any(|need| merges.contains_key(need) || after.contains(need))
            })
            .map(|step| step.id.clone())
            .collect::<Vec<_>>();
        if grown.is_empty() {
            return after;
        }
        after.extend(grown);
    }
}

/// A repository sequence for revert landing: the repositories the forward
/// landing merged later come first. Forward-unordered repositories keep their
/// declared positions relative to each other.
pub(super) fn reverse_sequence(order: &mut [String], forward: &Precedence) {
    let declared = order.to_vec();
    order.sort_by_key(|repo| {
        std::cmp::Reverse(
            declared
                .iter()
                .filter(|other| forward.contains(&((*other).clone(), repo.clone())))
                .count(),
        )
    });
}

/// The precedence a declared repository sequence imposes: each repository
/// before every one after it.
pub(super) fn sequence_precedence(order: &[String]) -> Precedence {
    order
        .iter()
        .enumerate()
        .flat_map(|(index, earlier)| {
            order[index + 1..]
                .iter()
                .map(move |later| (earlier.clone(), later.clone()))
        })
        .collect()
}
