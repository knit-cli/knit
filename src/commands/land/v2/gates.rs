//! Library releases and landing gates.
//!
//! A project may declare library → consumer edges in `landing.dependencies`.
//! Generation expands an edge only when the bundle changed the library and the
//! plan merges it: the library merges first, an optional `manual` release gate
//! pauses the run until someone acknowledges the release, and each changed
//! consumer gets an optional `await_update` gate that waits for its version
//! bump before that consumer's review merges. Everything compiles to ordinary
//! plan steps, so the saved plan remains the reviewed, editable document.
//!
//! Gates pause a run instead of blocking a terminal. A paused run is
//! quiescent; `knit land resume` continues it.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const CAPABILITY: &str = "landing-gates";
pub(crate) const EXECUTOR_VERSION: &str = "0.6";

/// A run stopped at a gate on purpose. Not a failure: receipts are complete
/// and nothing is in flight, so the run can be resumed.
#[derive(Debug)]
pub(crate) struct LandingPaused {
    pub step: String,
    pub instructions: String,
    pub waiting_for: String,
    /// The command or action that continues the run.
    pub next: String,
}

impl std::fmt::Display for LandingPaused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "landing paused at {}: waiting for {}",
            self.step, self.waiting_for
        )
    }
}

impl std::error::Error for LandingPaused {}

/// Whether a step is a gate the run pauses at (instead of prompting).
pub(crate) fn is_gate(step: &Value) -> bool {
    step["type"] == "await_update" || (step["type"] == "manual" && step["acknowledge"] == "resume")
}

/// Repositories whose reviewed head a gate may legitimately move during the run.
pub(crate) fn gated_repos(plan: &Value) -> BTreeSet<String> {
    plan["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s["type"] == "await_update")
        .filter_map(|s| s["repoId"].as_str().map(str::to_owned))
        .collect()
}

pub(crate) fn uses_gates(plan: &Value) -> bool {
    plan["steps"].as_array().into_iter().flatten().any(is_gate)
}

fn project_repo_ids(project: &Value) -> Option<BTreeSet<String>> {
    project["repos"].as_array().map(|repos| {
        repos
            .iter()
            .filter_map(|r| r["id"].as_str().map(str::to_owned))
            .collect()
    })
}

/// Project configuration may only name project repositories. A repository a
/// bundle tracks directly takes part through wildcards; naming it needs
/// project membership, or the same file would be valid for one bundle and
/// invalid for the next.
fn ensure_project_repo(project: &Value, id: &str, context: &str) -> Result<()> {
    let repos = project_repo_ids(project).unwrap_or_default();
    if repos.contains(id) {
        return Ok(());
    }
    bail!(
        "{context} names unknown project repository `{id}`, which is not a repository of project `{}`. If bundles track `{id}` directly, add it to the project as an observed repository (observed repositories are not added to new bundles): `knit project add {id} <path> --observe`. Otherwise use a project repository id, or `\"*\"`.",
        project["id"].as_str().unwrap_or("?")
    )
}

/// Parse a `whenChanged` list: `None` when absent, `Some(None)` for `"*"`,
/// `Some(Some(ids))` for named repositories.
fn when_changed(
    step: &Value,
    project: &Value,
    context: &str,
) -> Result<Option<Option<Vec<String>>>> {
    let Some(value) = step.get("whenChanged") else {
        return Ok(None);
    };
    let list = value
        .as_array()
        .with_context(|| format!("{context}: whenChanged must be an array of repository ids"))?;
    if list.is_empty() {
        bail!("{context}: whenChanged is empty, so the step could never run. List the repositories it depends on, or remove the field to always run it.");
    }
    let mut ids = Vec::new();
    for entry in list {
        let id = entry
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .with_context(|| format!("{context}: whenChanged entries must be repository ids"))?;
        if ids.iter().any(|i| i == id) {
            bail!("{context}: whenChanged repeats `{id}`");
        }
        ids.push(id.to_owned());
    }
    if ids.iter().any(|id| id == "*") {
        if ids.len() > 1 {
            bail!("{context}: whenChanged combines `\"*\"` with named repositories; `\"*\"` already means every landing");
        }
        return Ok(Some(None));
    }
    for id in &ids {
        ensure_project_repo(project, id, &format!("{context} whenChanged"))?;
    }
    Ok(Some(Some(ids)))
}

/// Keep only the custom steps whose `whenChanged` matched. The field is a
/// generation rule, not a plan field, so it is removed from kept steps.
pub(crate) fn select_conditional_steps(
    steps: Vec<Value>,
    changed: &BTreeSet<String>,
    project: &Value,
) -> Result<Vec<Value>> {
    let mut kept = vec![];
    for mut step in steps {
        let context = format!(
            "landing step `{}`",
            step["id"].as_str().unwrap_or("<missing id>")
        );
        let keep = match when_changed(&step, project, &context)? {
            None | Some(None) => true,
            Some(Some(ids)) => ids.iter().any(|id| changed.contains(id)),
        };
        if let Some(object) = step.as_object_mut() {
            object.remove("whenChanged");
        }
        if keep {
            kept.push(step);
        }
    }
    Ok(kept)
}

struct Dependency {
    library: String,
    consumers: Option<Vec<String>>,
    release: Option<String>,
    bump: Option<(String, Option<Vec<String>>)>,
}

fn instructions(value: &Value, context: &str) -> Result<String> {
    value["instructions"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
        .with_context(|| format!("{context}: instructions are required"))
}

fn parse_dependencies(project: &Value) -> Result<Vec<Dependency>> {
    let Some(value) = project["landing"].get("dependencies") else {
        return Ok(vec![]);
    };
    if value.is_null() {
        return Ok(vec![]);
    }
    let entries = value
        .as_array()
        .context("landing.dependencies must be an array")?;
    let mut result: Vec<Dependency> = vec![];
    for (index, entry) in entries.iter().enumerate() {
        let context = format!("landing.dependencies[{index}]");
        let object = entry
            .as_object()
            .with_context(|| format!("{context} must be an object"))?;
        for key in object.keys() {
            if !matches!(key.as_str(), "library" | "consumers" | "release" | "bump") {
                bail!(
                    "{context}: unknown field `{key}` (expected library, consumers, release, bump)"
                );
            }
        }
        let library = entry["library"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .with_context(|| format!("{context}: library repository id is required"))?
            .to_owned();
        ensure_project_repo(project, &library, &format!("{context}.library"))?;
        if result.iter().any(|d| d.library == library) {
            bail!("{context}: library `{library}` is declared more than once");
        }
        let consumers = match entry.get("consumers") {
            Some(Value::String(s)) if s == "*" => None,
            Some(Value::Array(list)) => {
                if list.is_empty() {
                    bail!("{context}.consumers is empty; use \"*\" for every other repository the plan merges");
                }
                let mut ids = vec![];
                for item in list {
                    let id = item
                        .as_str()
                        .filter(|s| !s.trim().is_empty() && *s != "*")
                        .with_context(|| {
                            format!(
                                "{context}.consumers must be \"*\" or an array of repository ids"
                            )
                        })?;
                    if id == library {
                        bail!("{context}.consumers lists the library `{library}` itself");
                    }
                    if ids.iter().any(|i| i == id) {
                        bail!("{context}.consumers repeats `{id}`");
                    }
                    ensure_project_repo(project, id, &format!("{context}.consumers"))?;
                    ids.push(id.to_owned());
                }
                Some(ids)
            }
            _ => bail!("{context}.consumers must be \"*\" or an array of repository ids"),
        };
        let release = match entry.get("release") {
            None | Some(Value::Null) => None,
            Some(release) if release.is_object() => {
                if release
                    .as_object()
                    .unwrap()
                    .keys()
                    .any(|k| k != "instructions")
                {
                    bail!("{context}.release: unknown field");
                }
                Some(instructions(release, &format!("{context}.release"))?)
            }
            Some(_) => bail!("{context}.release must be an object with instructions"),
        };
        let bump = match entry.get("bump") {
            None | Some(Value::Null) => None,
            Some(bump) if bump.is_object() => {
                if bump
                    .as_object()
                    .unwrap()
                    .keys()
                    .any(|k| !matches!(k.as_str(), "instructions" | "paths"))
                {
                    bail!("{context}.bump: unknown field");
                }
                let text = instructions(bump, &format!("{context}.bump"))?;
                let paths = match bump.get("paths") {
                    None | Some(Value::Null) => None,
                    Some(Value::Array(list)) if !list.is_empty() => Some(
                        list.iter()
                            .map(|p| {
                                p.as_str()
                                    .filter(|s| !s.trim().is_empty())
                                    .map(str::to_owned)
                                    .with_context(|| {
                                        format!("{context}.bump.paths must contain nonempty path patterns")
                                    })
                            })
                            .collect::<Result<Vec<_>>>()?,
                    ),
                    Some(_) => bail!("{context}.bump.paths must be a nonempty array of path patterns"),
                };
                Some((text, paths))
            }
            Some(_) => bail!("{context}.bump must be an object with instructions"),
        };
        result.push(Dependency {
            library,
            consumers,
            release,
            bump,
        });
    }
    Ok(result)
}

/// Validate named repository references even in inactive scopes. A project
/// recipe must not become valid only for a particular bundle.
pub(crate) fn validate_project(project: &Value) -> Result<()> {
    let dependencies = parse_dependencies(project)?;
    let repos = project_repo_ids(project).unwrap_or_default();
    let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for dependency in dependencies {
        let consumers = dependency.consumers.unwrap_or_else(|| {
            repos
                .iter()
                .filter(|id| **id != dependency.library)
                .cloned()
                .collect()
        });
        edges
            .entry(dependency.library)
            .or_default()
            .extend(consumers);
    }
    fn visit(
        id: &str,
        edges: &BTreeMap<String, BTreeSet<String>>,
        visiting: &mut BTreeSet<String>,
        done: &mut BTreeSet<String>,
    ) -> Result<()> {
        if done.contains(id) {
            return Ok(());
        }
        if !visiting.insert(id.to_owned()) {
            bail!("landing.dependencies contains a cycle through {id}");
        }
        for next in edges.get(id).into_iter().flatten() {
            visit(next, edges, visiting, done)?;
        }
        visiting.remove(id);
        done.insert(id.to_owned());
        Ok(())
    }
    let mut done = BTreeSet::new();
    for id in edges.keys() {
        visit(id, &edges, &mut BTreeSet::new(), &mut done)?;
    }
    let known = known_step_ids(project, &Value::Null)?;
    let landing = &project["landing"];
    let mut scopes = vec![landing];
    for key in ["targets", "lanes"] {
        scopes.extend(
            landing[key]
                .as_object()
                .into_iter()
                .flat_map(|map| map.values()),
        );
    }
    for scope in scopes {
        for key in ["steps", "deployments"] {
            if scope
                .get(key)
                .is_some_and(|v| !v.is_array() && !v.is_null())
            {
                bail!("landing.{key} must be an array");
            }
            for step in scope[key].as_array().into_iter().flatten() {
                when_changed(step, project, key)?;
                for field in ["needs", "requires"] {
                    if let Some(needs) = step.get(field) {
                        let needs = needs
                            .as_array()
                            .with_context(|| format!("landing.{key}.{field} must be an array"))?;
                        for need in needs {
                            let need = need
                                .as_str()
                                .context("step prerequisites must be strings")?;
                            if !known.contains(need) {
                                bail!("landing.{key} needs unknown step {need}: expected a configured repository merge, deployment, landing step or dependency gate");
                            }
                        }
                    }
                }
                if let Some(id) = step["repoId"].as_str() {
                    ensure_project_repo(project, id, key)?;
                }
                for id in super::graph::strings(&step["sourceRepos"]) {
                    ensure_project_repo(project, &id, "sourceRepos")?;
                }
            }
        }
        for order in [
            &scope["merge"]["repoOrder"],
            &scope["execution"]["repoOrder"],
        ] {
            for id in super::graph::strings(order) {
                ensure_project_repo(project, &id, "repoOrder")?;
            }
        }
        for id in scope["merge"]["repositories"]
            .as_object()
            .into_iter()
            .flat_map(|map| map.keys())
        {
            ensure_project_repo(project, id, "merge.repositories")?;
        }
        for (id, needs) in scope["merge"]["needs"]
            .as_object()
            .into_iter()
            .flat_map(|map| map.iter())
        {
            ensure_project_repo(project, id, "merge.needs")?;
            for need in super::graph::strings(needs) {
                ensure_project_repo(project, &need, "merge.needs")?;
            }
        }
        for id in scope["branches"]
            .as_object()
            .into_iter()
            .flat_map(|map| map.keys())
            .filter(|id| *id != "*")
        {
            ensure_project_repo(project, id, "lane branches")?;
        }
    }
    Ok(())
}

/// Every step id the dependency configuration could produce, for the
/// conditional-needs rule.
fn dependency_step_ids(
    project: &Value,
    bundle_repos: &BTreeSet<String>,
) -> Result<BTreeSet<String>> {
    let mut ids = BTreeSet::new();
    let mut repos: BTreeSet<String> = project_repo_ids(project).unwrap_or_default();
    repos.extend(bundle_repos.iter().cloned());
    for dependency in parse_dependencies(project)? {
        if dependency.release.is_some() {
            ids.insert(format!("release-{}", dependency.library));
        }
        if dependency.bump.is_some() {
            for repo in &repos {
                if repo == &dependency.library
                    || dependency
                        .consumers
                        .as_ref()
                        .is_some_and(|ids| !ids.contains(repo))
                {
                    continue;
                }
                ids.insert(format!("bump-{repo}"));
            }
        }
    }
    Ok(ids)
}

fn merge_step<'a>(steps: &'a [Value], repo: &str) -> Option<&'a Value> {
    steps.iter().find(|s| {
        matches!(s["type"].as_str(), Some("merge_pr" | "merge_branch")) && s["repoId"] == repo
    })
}

fn add_need(step: &mut Value, need: &str) {
    let mut needs = super::graph::strings(&step["needs"]);
    if !needs.iter().any(|n| n == need) {
        needs.push(need.to_owned());
    }
    step["needs"] = json!(needs);
}

/// Expand `landing.dependencies` into ordering and gates for this plan.
pub(crate) fn expand_dependencies(
    steps: &mut Vec<Value>,
    project: &Value,
    changed: &BTreeSet<String>,
) -> Result<()> {
    let dependencies = parse_dependencies(project)?;
    if dependencies.is_empty() {
        return Ok(());
    }
    // What each consumer's merge waits for, and the bump gates to create.
    let mut consumer_needs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    #[derive(Default)]
    struct Bump {
        needs: Vec<String>,
        instructions: Vec<String>,
        paths: Vec<String>,
        unrestricted: bool,
    }
    let mut bumps: BTreeMap<String, Bump> = BTreeMap::new();
    let mut releases: Vec<(String, String)> = vec![];
    for dependency in &dependencies {
        if !changed.contains(&dependency.library) {
            continue;
        }
        let Some(library_merge) = merge_step(steps, &dependency.library) else {
            continue;
        };
        let library_merge_id = library_merge["id"].as_str().unwrap_or_default().to_owned();
        // Releases and bumps belong to landing the reviews themselves. An
        // environment the bundle only passes through keeps the order only.
        let reviewed = library_merge["type"] == "merge_pr";
        if !reviewed {
            continue;
        }
        let mut after = library_merge_id.clone();
        if reviewed {
            if let Some(text) = &dependency.release {
                let id = format!("release-{}", dependency.library);
                releases.push((id.clone(), text.clone()));
                after = id;
            }
        }
        let merged_consumers: Vec<String> = steps
            .iter()
            .filter(|s| matches!(s["type"].as_str(), Some("merge_pr" | "merge_branch")))
            .filter_map(|s| s["repoId"].as_str())
            .filter(|repo| *repo != dependency.library)
            .filter(|repo| {
                dependency
                    .consumers
                    .as_ref()
                    .is_none_or(|list| list.iter().any(|c| c == repo))
            })
            .map(str::to_owned)
            .collect();
        for consumer in merged_consumers {
            let consumer_reviewed =
                merge_step(steps, &consumer).is_some_and(|s| s["type"] == "merge_pr");
            match (&dependency.bump, reviewed && consumer_reviewed) {
                (Some((text, paths)), true) => {
                    let entry = bumps.entry(consumer.clone()).or_default();
                    entry.needs.push(after.clone());
                    entry.instructions.push(text.clone());
                    if let Some(paths) = paths {
                        for path in paths {
                            if !entry.paths.contains(path) {
                                entry.paths.push(path.clone());
                            }
                        }
                    } else {
                        entry.unrestricted = true;
                    }
                }
                _ => consumer_needs
                    .entry(consumer.clone())
                    .or_default()
                    .push(after.clone()),
            }
        }
    }
    for (id, text) in releases {
        let library = id.strip_prefix("release-").unwrap().to_owned();
        let position = steps
            .iter()
            .position(|s| s["id"] == format!("merge-{library}"))
            .map(|i| i + 1)
            .unwrap_or(steps.len());
        let merge_id = merge_step(steps, &library)
            .and_then(|s| s["id"].as_str())
            .unwrap_or_default()
            .to_owned();
        steps.insert(
            position,
            json!({"id":id,"type":"manual","acknowledge":"resume","repoId":library,"label":format!("Release {library}"),"instructions":text,"needs":[merge_id],"effect":"read_only","recovery":{"mode":"none"}}),
        );
    }
    for (
        consumer,
        Bump {
            needs,
            instructions,
            paths,
            unrestricted,
        },
    ) in bumps
    {
        let id = format!("bump-{consumer}");
        let mut gate = json!({"id":id,"type":"await_update","repoId":consumer,"label":format!("Bump {consumer}"),"instructions":instructions.join(" "),"needs":needs,"effect":"read_only","recovery":{"mode":"none"}});
        if !unrestricted {
            gate["paths"] = json!(paths);
        }
        let merge_index = steps
            .iter()
            .position(|s| s["type"] == "merge_pr" && s["repoId"] == consumer)
            .context("consumer review merge disappeared")?;
        add_need(&mut steps[merge_index], &id);
        steps.insert(merge_index, gate);
    }
    for (consumer, needs) in consumer_needs {
        if let Some(index) = steps.iter().position(|s| {
            matches!(s["type"].as_str(), Some("merge_pr" | "merge_branch"))
                && s["repoId"] == consumer
        }) {
            for need in needs {
                add_need(&mut steps[index], &need);
            }
        }
    }
    Ok(())
}

/// Every step id this project configuration could produce for some bundle.
pub(crate) fn known_step_ids(project: &Value, bundle: &Value) -> Result<BTreeSet<String>> {
    let bundle_repos: BTreeSet<String> = bundle["repos"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["id"].as_str().map(str::to_owned))
        .collect();
    let mut ids = dependency_step_ids(project, &bundle_repos)?;
    for repo in project_repo_ids(project)
        .unwrap_or_default()
        .into_iter()
        .chain(bundle_repos.iter().cloned())
    {
        ids.insert(format!("merge-{repo}"));
    }
    let landing = &project["landing"];
    let mut scopes = vec![landing.clone()];
    for key in ["targets", "lanes"] {
        if let Some(map) = landing[key].as_object() {
            scopes.extend(map.values().cloned());
        }
    }
    for scope in scopes {
        for key in ["deployments", "steps"] {
            for step in scope[key].as_array().into_iter().flatten() {
                if let Some(id) = step["id"].as_str() {
                    ids.insert(id.to_owned());
                    if key == "deployments" && step.get("build").is_some() {
                        ids.insert(format!("{id}-build"));
                    }
                    if key == "deployments" && step.get("verify").is_some() {
                        ids.insert(format!("{id}-verify"));
                    }
                }
            }
        }
    }
    Ok(ids)
}

/// Drop needs on steps this configuration can produce but this bundle did
/// not. Anything else is still an error: a typo must not silently remove an
/// ordering constraint.
pub(crate) fn prune_conditional_needs(steps: &mut [Value], known: &BTreeSet<String>) -> Result<()> {
    let present: BTreeSet<String> = steps
        .iter()
        .filter_map(|s| s["id"].as_str().map(str::to_owned))
        .collect();
    for step in steps.iter_mut() {
        for key in ["needs", "requires"] {
            let Some(list) = step.get(key).and_then(Value::as_array) else {
                continue;
            };
            let mut kept = vec![];
            for need in list {
                let Some(name) = need.as_str() else {
                    kept.push(need.clone());
                    continue;
                };
                if present.contains(name) {
                    kept.push(need.clone());
                } else if !known.contains(name) {
                    bail!(
                        "step {} needs unknown step {name}: no repository merge (merge-<repo>), deployment, landing step or dependency gate in this project configuration has that id",
                        step["id"].as_str().unwrap_or("<missing id>")
                    );
                }
            }
            step[key] = json!(kept);
        }
    }
    Ok(())
}

/// Record executor requirements for plans that contain gates.
pub(crate) fn require_executor(plan: &mut Value) {
    if !uses_gates(plan) {
        return;
    }
    plan["requiredExecutorVersion"] = json!(EXECUTOR_VERSION);
    let mut capabilities = super::graph::strings(&plan["requiredCapabilities"]);
    if !capabilities.iter().any(|c| c == CAPABILITY) {
        capabilities.push(CAPABILITY.to_owned());
    }
    capabilities.sort();
    capabilities.dedup();
    plan["requiredCapabilities"] = json!(capabilities);
}

/// Validate gate steps inside a saved plan.
pub(crate) fn validate_gates(plan: &Value, steps: &[Value]) -> Result<()> {
    for step in steps {
        let id = step["id"].as_str().unwrap_or("<missing id>");
        if let Some(mode) = step.get("acknowledge") {
            if step["type"] != "manual" || !matches!(mode.as_str(), Some("terminal" | "resume")) {
                bail!("{id}: acknowledge must be \"terminal\" or \"resume\" on a manual step");
            }
        }
        if is_gate(step) && super::graph::effect(step) != "read_only" {
            bail!("{id}: landing gates observe external completion and require effect read_only");
        }
        if step.get("paths").is_some() && step["type"] != "await_update" {
            bail!("{id}: paths is only supported on await_update steps");
        }
        if is_gate(step) && step.get("runner").is_some_and(|r| r != "local") {
            bail!("{id}: landing gates require a local runner");
        }
        if step["type"] != "await_update" {
            continue;
        }
        let repo = step["repoId"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .with_context(|| format!("{id}: await_update requires repoId"))?;
        if step["instructions"]
            .as_str()
            .is_none_or(|s| s.trim().is_empty())
        {
            bail!("{id}: await_update instructions required");
        }
        if step.get("command").is_some() {
            bail!("{id}: await_update steps cannot have a command");
        }
        if let Some(paths) = step.get("paths") {
            if !paths.as_array().is_some_and(|a| {
                !a.is_empty()
                    && a.iter()
                        .all(|p| p.as_str().is_some_and(|s| !s.trim().is_empty()))
            }) {
                bail!("{id}: paths must be a nonempty array of path patterns");
            }
        }
        let gated_merges: Vec<&Value> = steps
            .iter()
            .filter(|s| s["repoId"] == repo && s["type"] == "merge_pr")
            .collect();
        if gated_merges.is_empty() {
            bail!("{id}: await_update gates the review merge of {repo}, but the plan has no merge_pr step for {repo}");
        }
        if steps
            .iter()
            .filter(|s| s["type"] == "await_update" && s["repoId"] == repo)
            .count()
            > 1
        {
            bail!("{id}: only one await_update step per repository");
        }
        // The merge must actually wait for the gate, or the bump would be
        // checked after the reviewed head had already merged.
        let (compiled, _) = super::graph::compile(plan)?;
        let mut ancestors: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        fn visit(id: &str, steps: &[Value], ancestors: &mut BTreeMap<String, BTreeSet<String>>) {
            if ancestors.contains_key(id) {
                return;
            }
            ancestors.insert(id.to_owned(), BTreeSet::new());
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
        for merge in gated_merges {
            let merge_id = merge["id"].as_str().unwrap_or_default();
            visit(merge_id, &compiled, &mut ancestors);
            if !ancestors[merge_id].contains(id) {
                bail!("{id}: {merge_id} must depend on this await_update step");
            }
        }
    }
    if uses_gates(plan) {
        if !matches!(
            plan["requiredExecutorVersion"].as_str(),
            Some(EXECUTOR_VERSION)
        ) {
            bail!("landing gates (await_update, manual acknowledge \"resume\") require requiredExecutorVersion {EXECUTOR_VERSION}");
        }
        if !super::graph::strings(&plan["requiredCapabilities"])
            .iter()
            .any(|c| c == CAPABILITY)
        {
            bail!("requiredCapabilities must include {CAPABILITY} for landing gates");
        }
    }
    Ok(())
}

/// Path patterns: `*` and `?` stay within one path segment, `**` crosses
/// segments, and a pattern without `/` matches that name in any directory.
pub(crate) fn path_matches(pattern: &str, path: &str) -> bool {
    let pattern = if pattern.contains('/') {
        pattern.trim_start_matches('/').to_owned()
    } else {
        format!("**/{pattern}")
    };
    let mut regex = String::from("^");
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if chars.get(i + 1) == Some(&'*') => {
                if chars.get(i + 2) == Some(&'/') {
                    regex.push_str("(?:.*/)?");
                    i += 3;
                } else {
                    regex.push_str(".*");
                    i += 2;
                }
                continue;
            }
            '*' => regex.push_str("[^/]*"),
            '?' => regex.push_str("[^/]"),
            c => regex.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    regex.push('$');
    regex::Regex::new(&regex).is_ok_and(|r| r.is_match(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conditional_needs_drop_only_configured_steps_and_validate_inactive_scopes() {
        let project = json!({"repos":[{"id":"library"}],"landing":{"steps":[{"id":"release","whenChanged":["library"]}]}});
        let changed = BTreeSet::new();
        assert!(select_conditional_steps(
            project["landing"]["steps"].as_array().unwrap().clone(),
            &changed,
            &project
        )
        .unwrap()
        .is_empty());
        let known = known_step_ids(&project, &json!({"repos":[{"id":"extra"}]})).unwrap();
        let mut steps = vec![json!({"id":"finish","needs":["release","merge-extra"]})];
        prune_conditional_needs(&mut steps, &known).unwrap();
        assert_eq!(steps[0]["needs"], json!([]));
        steps[0]["needs"] = json!(["release-build"]);
        assert!(prune_conditional_needs(&mut steps, &known).is_err());
        let mut invalid = project.clone();
        invalid["landing"]["targets"]["unused"]["steps"] =
            json!([{"id":"gate","whenChanged":["typo"]}]);
        assert!(validate_project(&invalid)
            .unwrap_err()
            .to_string()
            .contains("--observe"));
    }
    #[test]
    fn dependencies_expand_bundle_only_consumers_and_refuse_cycles() {
        let project = json!({"repos":[{"id":"library"}],"landing":{"dependencies":[{"library":"library","consumers":"*","release":{"instructions":"Publish"},"bump":{"instructions":"Bump","paths":["Cargo.toml"]}}]}});
        validate_project(&project).unwrap();
        let mut steps = vec![
            json!({"id":"merge-library","repoId":"library","type":"merge_pr"}),
            json!({"id":"merge-extra","repoId":"extra","type":"merge_pr"}),
        ];
        expand_dependencies(
            &mut steps,
            &project,
            &BTreeSet::from(["library".into(), "extra".into()]),
        )
        .unwrap();
        let mut plan = json!({"steps":steps});
        require_executor(&mut plan);
        validate_gates(&plan, &steps).unwrap();
        assert_eq!(
            steps.iter().find(|s| s["id"] == "bump-extra").unwrap()["needs"],
            json!(["release-library"])
        );
        let mut invalid = project.clone();
        invalid["repos"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id":"second"}));
        invalid["landing"]["dependencies"]
            .as_array_mut()
            .unwrap()
            .push(json!({"library":"second","consumers":"*"}));
        assert!(validate_project(&invalid)
            .unwrap_err()
            .to_string()
            .contains("cycle"));
        let mut unknown = project;
        unknown["landing"]["dependencies"][0]["consumers"] = json!(["extra"]);
        assert!(validate_project(&unknown)
            .unwrap_err()
            .to_string()
            .contains("--observe"));
    }
    #[test]
    fn gates_require_version_capability_local_runner_and_dependency_order() {
        let steps = vec![
            json!({"id":"bump","type":"await_update","repoId":"consumer","instructions":"Bump"}),
            json!({"id":"merge","type":"merge_pr","repoId":"consumer","needs":["bump"]}),
        ];
        let mut plan = json!({"steps":steps});
        assert!(validate_gates(&plan, &steps).is_err());
        require_executor(&mut plan);
        validate_gates(&plan, &steps).unwrap();
        plan["steps"][1]["needs"] = json!([]);
        assert!(validate_gates(&plan, plan["steps"].as_array().unwrap()).is_err());
    }

    #[test]
    fn path_patterns_match_names_anywhere_and_anchored_paths() {
        assert!(path_matches("Cargo.toml", "Cargo.toml"));
        assert!(path_matches("Cargo.toml", "rust/crate/Cargo.toml"));
        assert!(!path_matches("Cargo.toml", "Cargo.toml.orig"));
        assert!(path_matches("**/Cargo.toml", "Cargo.toml"));
        assert!(path_matches("deploy/*.yaml", "deploy/values.yaml"));
        assert!(!path_matches("deploy/*.yaml", "deploy/nested/values.yaml"));
        assert!(path_matches("deploy/**", "deploy/nested/values.yaml"));
        assert!(!path_matches("Cargo.lock", "src/main.rs"));
    }
}
