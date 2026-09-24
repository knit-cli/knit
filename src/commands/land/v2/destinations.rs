//! Read-only destination discovery shared by local clients.
use super::{
    generate::{destination_path, local_project},
    graph::canonical_hash,
};
use crate::store::{load_active_bundle, read_json};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

pub fn destinations(json_output: bool) -> Result<()> {
    let active = load_active_bundle()?;
    let project = local_project(&active)?;
    let mut entries = BTreeMap::new();
    entries.insert(
        "default".to_owned(),
        ("default".to_owned(), "default".to_owned(), Value::Null),
    );
    for (field, kind) in [("lanes", "lane"), ("targets", "target")] {
        if let Some(scopes) = project["landing"][field].as_object() {
            for (name, scope) in scopes {
                entries.insert(
                    format!("{kind}:{name}"),
                    (kind.into(), name.clone(), scope.clone()),
                );
            }
        }
    }
    let mut saved = BTreeMap::new();
    let directory = active.root.join(".knit/land-plans");
    if directory.is_dir() {
        for entry in std::fs::read_dir(directory)? {
            let path = entry?.path();
            if !path.is_file() || !path.to_string_lossy().ends_with(".land.json") {
                continue;
            }
            let plan: Value = match read_json(&path) {
                Ok(plan) => plan,
                Err(error) => {
                    eprintln!(
                        "Skipping unreadable landing plan {}: {error}",
                        path.display()
                    );
                    continue;
                }
            };
            if plan["bundleId"] != active.bundle.id || plan["kind"] != "KnitLandPlan" {
                continue;
            }
            let (kind, name) = if let Some(name) = plan["lane"].as_str() {
                ("lane", name)
            } else if let Some(name) = plan["targetBranch"].as_str() {
                ("target", name)
            } else {
                ("default", "default")
            };
            let key = if kind == "default" {
                "default".into()
            } else {
                format!("{kind}:{name}")
            };
            entries
                .entry(key.clone())
                .or_insert((kind.into(), name.into(), Value::Null));
            if let Some((_, prior)) = saved.insert(key, (path, plan.clone())) {
                if prior != plan {
                    bail!("Multiple authored plans for the same destination; reconcile before discovery");
                }
            }
        }
    }
    let mut result = vec![];
    for (key, (kind, name, scope)) in entries {
        let mut branches = BTreeMap::new();
        let mut unmapped = BTreeSet::new();
        for repo in &active.bundle.repos {
            let branch = match kind.as_str() {
                "target" => json!(name),
                "lane" => {
                    let mapping = scope["branches"]
                        .get(&repo.id)
                        .or_else(|| scope["branches"].get("*"))
                        .or_else(|| scope.get("defaultBranch"));
                    if mapping.is_none() {
                        unmapped.insert(repo.id.clone());
                    }
                    mapping.cloned().unwrap_or(Value::Null)
                }
                _ => active
                    .bundle
                    .publications
                    .iter()
                    .find(|p| p.repo_id == repo.id)
                    .map(|p| json!(p.base_branch))
                    .unwrap_or_else(|| json!(repo.base_branch)),
            };
            branches.insert(repo.id.clone(), branch);
        }
        let changed = crate::commands::publish::publish_scope_repo_ids(&active.bundle);
        let mut destinations = BTreeMap::new();
        let mut absent = BTreeSet::new();
        for repo in active
            .bundle
            .repos
            .iter()
            .filter(|r| changed.contains(&r.id))
        {
            if kind == "default"
                && !active
                    .bundle
                    .publications
                    .iter()
                    .any(|p| p.repo_id == repo.id)
            {
                continue;
            }
            if let Some(branch) = branches[&repo.id].as_str() {
                destinations.insert(repo.id.clone(), branch.to_owned());
            } else if !unmapped.contains(&repo.id) {
                absent.insert(repo.id.clone());
            }
        }
        let terminal = scope
            .get("terminal")
            .filter(|v| v.is_boolean())
            .cloned()
            .unwrap_or_else(|| {
                if !changed.is_disjoint(&unmapped) {
                    Value::Null
                } else {
                    json!(super::super::plan::resolve_terminal(
                        &active,
                        None,
                        (kind == "target").then_some(name.as_str()),
                        (kind == "lane").then_some(name.as_str()),
                        None,
                        &destinations,
                        &absent
                    ))
                }
            });
        let label = scope["label"].as_str().unwrap_or(if kind == "default" {
            "Recorded review bases"
        } else {
            &name
        });
        let mut item = json!({"key":key,"kind":kind,"name":name,"label":label,"terminal":terminal,"branches":branches,"hasPlan":false});
        item["mergeEnabled"] = scope["merge"]
            .get("enabled")
            .or_else(|| project["landing"]["merge"].get("enabled"))
            .cloned()
            .unwrap_or(json!(true));
        if let Some((path, plan)) = saved.get(&key) {
            if let Some(enabled) = plan["merge"].get("enabled") {
                item["mergeEnabled"] = enabled.clone();
            }
            item["hasPlan"] = json!(true);
            item["planPath"] = json!(path);
            item["planHash"] = json!(canonical_hash(plan));
            // Saved intent is what apply executes, including ad-hoc destinations.
            if let Some(terminal) = plan.get("terminal") {
                item["terminal"] = terminal.clone();
            }
            if let Some(branches) = plan["targetBranches"].as_object() {
                for (repo, branch) in branches {
                    item["branches"][repo] = branch.clone();
                }
            }
            for step in plan["steps"].as_array().into_iter().flatten() {
                if matches!(step["type"].as_str(), Some("merge_pr" | "merge_branch")) {
                    if let (Some(repo), Some(branch)) =
                        (step["repoId"].as_str(), step["targetBranch"].as_str())
                    {
                        if plan["targetBranches"].get(repo).is_none() {
                            item["branches"][repo] = json!(branch);
                        }
                    }
                }
            }
            for repo in plan["laneAbsent"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                item["branches"][repo] = Value::Null;
            }
        } else {
            let path = destination_path(
                &active,
                (kind == "target").then_some(name.as_str()),
                (kind == "lane").then_some(name.as_str()),
            );
            item["planPath"] = json!(path);
        }
        result.push(item);
    }
    if json_output {
        println!("{}", json!({"destinations":result}));
    } else {
        for item in result {
            let state = match item["terminal"].as_bool() {
                Some(true) => "final",
                Some(false) => "intermediate",
                None => "unresolved",
            };
            println!(
                "{} — {} ({state}); plan: {}",
                item["key"].as_str().unwrap(),
                item["label"].as_str().unwrap(),
                if item["hasPlan"] == true {
                    "saved"
                } else {
                    "not generated"
                }
            );
            println!("  branches: {}", item["branches"]);
        }
    }
    Ok(())
}
