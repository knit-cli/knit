use super::graph::{
    bundle_fingerprint, canonical_hash, effect, project_fingerprint, recovery, strings, validation,
};
use crate::store::{read_json, write_json, ActiveBundle};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub(super) fn local_project(active: &ActiveBundle) -> Result<Value> {
    let config = crate::store::load_config(&active.root)?;
    match active
        .bundle
        .project_id
        .as_ref()
        .or(config.active_project.as_ref())
    {
        Some(id) => read_json(&crate::store::project_path(&active.root, id)),
        None => Ok(Value::Null),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn generate(
    artifact: Option<&Path>,
    project_file: Option<&Path>,
    out: Option<&Path>,
    provider: Option<&str>,
    target: Option<&str>,
    lane: Option<&str>,
    force: bool,
    json_output: bool,
) -> Result<()> {
    let active = match artifact {
        Some(p) => ActiveBundle::unlocked(std::env::current_dir()?, p.into(), read_json(p)?),
        None => crate::store::load_active_bundle()?,
    };
    let bundle = match artifact {
        Some(p) => read_json(p)?,
        None => serde_json::to_value(&active.bundle)?,
    };
    let project = match project_file {
        Some(p) => read_json(p)?,
        None if artifact.is_some() => Value::Null,
        None => local_project(&active)?,
    };
    let plan = build(&active, &bundle, &project, provider, target, lane)?;
    let path = out
        .map(PathBuf::from)
        .unwrap_or_else(|| destination_path(&active, target, lane));
    if path.exists() && !force {
        bail!("plan already exists: {}; pass --force", path.display());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_json(&path, &plan)?;
    if json_output {
        println!("{}", serde_json::to_string(&plan)?);
    } else {
        display_plan(&active, &plan, &path)?;
    }
    Ok(())
}

pub(super) fn build(
    active: &ActiveBundle,
    bundle: &Value,
    project: &Value,
    provider: Option<&str>,
    target: Option<&str>,
    lane: Option<&str>,
) -> Result<Value> {
    let mut compatible = project.clone();
    // Legacy typed recipe resolution supplies destinations and trigger selection.
    // Policy is read from the untouched input below.
    if compatible["landing"].is_object() {
        compatible["landing"]
            .as_object_mut()
            .unwrap()
            .remove("onFailure");
    }
    let typed = if project.is_null() {
        None
    } else {
        Some(serde_json::from_value(compatible)?)
    };
    // Deployment consumers need not have changed or be materialized in a bundle.
    // Add project bindings only to generation's view, never to bundle membership.
    let mut generation_bundle = active.bundle.clone();
    if let Some(repos) = project["repos"].as_array() {
        for repo in repos {
            let id = repo["id"].as_str().unwrap_or("");
            if !generation_bundle.repos.iter().any(|r| r.id == id) {
                generation_bundle.repos.push(serde_json::from_value(json!({"id":id,"path":repo["path"].as_str().unwrap_or("."),"remote":repo["remote"],"baseBranch":repo["baseBranch"],"featureBranch":null,"worktreePath":null}))?);
            }
        }
    }
    let generation_active = ActiveBundle::unlocked(
        active.root.clone(),
        active.bundle_path.clone(),
        generation_bundle,
    );
    let base = super::super::plan::build_plan_with_project(
        &generation_active,
        typed,
        provider,
        target,
        lane,
    )?;
    let mut plan = serde_json::to_value(base)?;
    plan["schemaVersion"] = json!("0.2");
    plan["requiredExecutorVersion"] = json!("0.2");
    plan["recipeRepos"] = json!(project["repos"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["id"].as_str())
        .collect::<Vec<_>>());
    plan["recipeBases"] = json!(project["repos"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| !active
            .bundle
            .repos
            .iter()
            .any(|b| Some(b.id.as_str()) == r["id"].as_str()))
        .filter_map(|r| Some((
            r["id"].as_str()?.to_owned(),
            r["baseBranch"].as_str()?.to_owned()
        )))
        .collect::<std::collections::BTreeMap<_, _>>());
    plan["bundleFingerprint"] = json!(bundle_fingerprint(bundle));
    plan["projectFingerprint"] = json!(project_fingerprint(project));
    plan["maxParallel"] = project["landing"]
        .get("maxParallel")
        .cloned()
        .unwrap_or(json!(4));
    let policy = project["landing"]["onFailure"].as_str().unwrap_or("stop");
    // Never upgrade legacy rollback into permission to execute compensation.
    plan["onFailure"] = json!(if policy == "recover" {
        "recover"
    } else {
        "stop"
    });
    let mut recipes = project["landing"]["deployments"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let override_config = if let Some(lane) = lane {
        &project["landing"]["lanes"][lane]
    } else if let Some(target) = target {
        &project["landing"]["targets"][target]
    } else {
        &Value::Null
    };
    if let Some(a) = override_config["deployments"].as_array() {
        recipes.extend(a.clone());
    }
    // Match only the branch recipes selected by the shared legacy resolver.
    if lane.is_none() && target.is_none() {
        let branches: BTreeSet<String> = plan["steps"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| matches!(s["type"].as_str(), Some("merge_pr" | "merge_branch")))
            .filter_map(|s| {
                s["targetBranch"].as_str().map(str::to_owned).or_else(|| {
                    active
                        .bundle
                        .publications
                        .iter()
                        .find(|p| Some(p.repo_id.as_str()) == s["repoId"].as_str())
                        .map(|p| p.base_branch.clone())
                })
            })
            .collect();
        for branch in branches {
            if let Some(a) = project["landing"]["targets"][&branch]["deployments"].as_array() {
                recipes.extend(a.clone());
            }
        }
    }
    let merges: Vec<String> = plan["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| matches!(s["type"].as_str(), Some("merge_pr" | "merge_branch")))
        .map(|s| s["id"].as_str().unwrap().into())
        .collect();
    let mut steps = vec![];
    for mut s in plan["steps"].as_array().unwrap().clone() {
        if let Some(recipe) = recipes.iter().rev().find(|r| r["id"] == s["id"]) {
            for k in ["label", "recovery", "effect", "locks", "sourceRepos"] {
                if let Some(v) = recipe.get(k) {
                    s[k] = v.clone();
                }
            }
            let mut needs = strings(&s["needs"]);
            for r in strings(&recipe["whenChanged"]) {
                let m = format!("merge-{r}");
                if r == "*" {
                    needs.extend(merges.clone());
                } else if merges.contains(&m) {
                    needs.push(m);
                }
            }
            for producer in plan["steps"].as_array().unwrap() {
                if matches!(producer["type"].as_str(), Some("merge_pr" | "merge_branch"))
                    && strings(&s["sourceRepos"])
                        .iter()
                        .any(|r| producer["repoId"] == *r)
                {
                    needs.push(producer["id"].as_str().unwrap().into());
                }
            }
            needs.sort();
            needs.dedup();
            if let Some(build) = recipe.get("build") {
                let mut b = command_inherit(build, &s);
                b["id"] = json!(format!("{}-build", s["id"].as_str().unwrap()));
                b["type"] = json!("run");
                b["role"] = json!("build");
                b["repoId"] = s["repoId"].clone();
                if let Some(sources) = s.get("sourceRepos") {
                    b["sourceRepos"] = sources.clone();
                }
                b["needs"] = json!(needs);
                b["requires"] = json!(needs);
                b["effect"] = json!("read_only");
                b["recovery"] = json!({"mode":"none"});
                needs = vec![b["id"].as_str().unwrap().into()];
                steps.push(b);
            }
            s["needs"] = json!(needs);
            s["requires"] = json!(needs);
            let verify = recipe.get("verify").cloned();
            s["recovery"] = recovery(&s);
            steps.push(s.clone());
            if let Some(verify) = verify {
                let mut verify = command_inherit(&verify, &s);
                verify["id"] = json!(format!("{}-verify", s["id"].as_str().unwrap()));
                verify["type"] = json!("run");
                verify["role"] = json!("verify");
                verify["repoId"] = s["repoId"].clone();
                if let Some(sources) = s.get("sourceRepos") {
                    verify["sourceRepos"] = sources.clone();
                }
                verify["needs"] = json!([s["id"]]);
                verify["requires"] = json!([s["id"]]);
                verify["effect"] = json!("read_only");
                verify["recovery"] = json!({"mode":"none"});
                steps.push(verify);
            }
        } else {
            s["recovery"] = recovery(&s);
            steps.push(s);
        }
    }
    let mut custom = project["landing"]["steps"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if let Some(a) = override_config["steps"].as_array() {
        custom.extend(a.clone());
    }
    for mut s in custom {
        if s.get("type").is_none() {
            s["type"] = json!("run");
        }
        s["recovery"] = recovery(&s);
        steps.push(s);
    }
    for step in &mut steps {
        step["effect"] = json!(effect(step));
    }
    plan["steps"] = json!(steps);
    let result = validation(&plan, Some(bundle), Some(project));
    if result["valid"] != true {
        bail!("{}", result["errors"]);
    }
    Ok(plan)
}

pub(crate) fn destination_path(
    active: &ActiveBundle,
    target: Option<&str>,
    lane: Option<&str>,
) -> PathBuf {
    let environment = lane
        .map(|v| format!("lane:{v}"))
        .or_else(|| target.map(|v| format!("target:{v}")));
    let file = match environment {
        Some(e) => format!(
            "{}--{}.land.json",
            active.bundle.id,
            &canonical_hash(&json!(e))[..12]
        ),
        None => format!("{}.land.json", active.bundle.id),
    };
    active.root.join(".knit/land-plans").join(file)
}

fn command_inherit(command: &Value, parent: &Value) -> Value {
    let mut result = command.clone();
    let mut env = parent["env"].as_object().cloned().unwrap_or_default();
    if let Some(overrides) = command["env"].as_object() {
        env.extend(overrides.clone());
    }
    if !env.is_empty() {
        result["env"] = json!(env);
    }
    for key in ["cwd", "timeoutSeconds"] {
        if result.get(key).is_none() {
            if let Some(value) = parent.get(key) {
                result[key] = value.clone();
            }
        }
    }
    result
}

pub(crate) fn display_plan(active: &ActiveBundle, plan: &Value, path: &Path) -> Result<()> {
    let (steps, waves) = super::graph::compile(plan)?;
    let mut effective = plan.clone();
    effective["steps"] = json!(steps);
    let display: super::super::LandPlan = serde_json::from_value(effective)?;
    super::super::display::print_plan(active, &display, path);
    println!("Hash: {}", canonical_hash(plan));
    println!(
        "Maximum parallel commands: {} (checkout and resource locks can serialize them)",
        plan["maxParallel"].as_u64().unwrap_or(4)
    );
    for (i, wave) in waves.iter().enumerate() {
        println!("Wave {}: {}", i + 1, wave.join(" | "));
    }
    for step in steps {
        if !step["command"].is_null() {
            println!(
                "{} argv={} cwd={}",
                step["id"],
                step["command"],
                step["cwd"].as_str().unwrap_or(".")
            );
        }
        println!("{} recovery={}", step["id"], recovery(&step));
    }
    Ok(())
}
