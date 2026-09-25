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
    if target.is_none() && lane.is_none() {
        for publication in &active.bundle.publications {
            let recorded_target = &project["landing"]["targets"][&publication.base_branch];
            if (recorded_target["merge"]["enabled"] == false
                || recorded_target["merge"]["repositories"][&publication.repo_id]["enabled"]
                    == false)
                && !effective_merge_policy(project, recorded_target)?
                    .enabled_for(&publication.repo_id)
            {
                bail!("Recorded target {} delegates integration to its procedure; select it explicitly with --target {} before generating a plan", publication.base_branch, publication.base_branch);
            }
        }
    }
    let override_config = if let Some(lane) = lane {
        &project["landing"]["lanes"][lane]
    } else if let Some(target) = target {
        &project["landing"]["targets"][target]
    } else {
        &Value::Null
    };
    let merge_policy = effective_merge_policy(project, override_config)?;
    let mut compatible = project.clone();
    if compatible["landing"].is_object() {
        compatible["landing"]["merge"] = serde_json::to_value(&merge_policy)?;
        if lane.is_some() || target.is_some() {
            compatible["landing"]["steps"] = override_config["steps"]
                .as_array()
                .cloned()
                .map(Value::Array)
                .unwrap_or(json!([]));
        }
    }
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
    plan["merge"] = json!({"enabled":merge_policy.enabled.unwrap_or(true)});
    if !merge_policy.repositories.is_empty() {
        plan["merge"]["repositories"] = serde_json::to_value(&merge_policy.repositories)?;
    }
    plan["requiredExecutorVersion"] = json!("0.3");
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
    if lane.is_some() || target.is_some() {
        recipes = override_config["deployments"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for key in ["maxParallel", "onFailure"] {
            if let Some(value) = override_config.get(key) {
                plan[key] = value.clone();
            }
        }
    }
    let mut default_custom = project["landing"]["steps"]
        .as_array()
        .cloned()
        .unwrap_or_default();
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
        let configured_target = branches
            .iter()
            .any(|b| project["landing"]["targets"].get(b).is_some());
        let non_base = active.bundle.publications.iter().any(|p| {
            branches.contains(&p.base_branch)
                && active
                    .bundle
                    .repos
                    .iter()
                    .any(|r| r.id == p.repo_id && r.base_branch != p.base_branch)
        });
        if configured_target || non_base {
            recipes.clear();
            default_custom.clear();
        }
        for branch in branches {
            if let Some(a) = project["landing"]["targets"][&branch]["steps"].as_array() {
                default_custom.extend(a.clone());
            }
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
            for k in [
                "label",
                "recovery",
                "effect",
                "locks",
                "sourceRepos",
                "interactive",
                "runner",
            ] {
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
    let mut custom = default_custom;
    if lane.is_some() || target.is_some() {
        custom = override_config["steps"]
            .as_array()
            .cloned()
            .unwrap_or_default();
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
    // Opt-in repository-sequence execution: compile the per-repository groups
    // into an explicit workflow, honoring every declared dependency. A scoped
    // (lane/target) block that omits the key inherits the project root; one
    // that sets it to null disables the root's mode for that scope.
    let execution = if lane.is_some() || target.is_some() {
        if override_config.get("execution").is_some() {
            override_config["execution"].clone()
        } else {
            project["landing"]["execution"].clone()
        }
    } else {
        project["landing"]["execution"].clone()
    };
    if let Some(order) = super::sequence::parse_execution(
        &execution,
        &lane
            .map(|l| format!("landing.lanes.{l}"))
            .or_else(|| target.map(|t| format!("landing.targets.{t}")))
            .unwrap_or_else(|| "landing".to_owned()),
    )? {
        let workflow = super::sequence::compile_workflow(&steps, &order)?;
        plan["execution"] = json!({"mode": super::sequence::MODE, "repoOrder": order});
        plan["workflow"] = workflow;
    }
    // Opt-in mergeability preflight policy, copied verbatim into the plan.
    // Scope semantics match execution: absent key inherits the root policy,
    // an explicit null disables it for that scope.
    let preflight = if lane.is_some() || target.is_some() {
        if override_config.get("preflight").is_some() {
            override_config["preflight"].clone()
        } else {
            project["landing"]["preflight"].clone()
        }
    } else {
        project["landing"]["preflight"].clone()
    };
    if !preflight.is_null() {
        super::mergeability::validate_plan_preflight(&json!({"preflight": preflight}))
            .map_err(|e| anyhow::anyhow!("landing preflight policy invalid: {e:#}"))?;
        plan["preflight"] = preflight;
    }
    if plan.get("execution").is_some() || plan.get("preflight").is_some() {
        plan["requiredExecutorVersion"] = json!("0.4");
        let mut capabilities = super::graph::strings(&plan["requiredCapabilities"]);
        if plan.get("execution").is_some()
            && !capabilities.contains(&super::sequence::CAPABILITY.to_owned())
        {
            capabilities.push(super::sequence::CAPABILITY.to_owned());
        }
        if plan.get("preflight").is_some()
            && !capabilities.contains(&super::mergeability::CAPABILITY.to_owned())
        {
            capabilities.push(super::mergeability::CAPABILITY.to_owned());
        }
        capabilities.sort();
        capabilities.dedup();
        plan["requiredCapabilities"] = json!(capabilities);
    }
    let result = validation(&plan, Some(bundle), Some(project));
    if result["valid"] != true {
        bail!("{}", result["errors"]);
    }
    Ok(plan)
}

/// Scope overrides inherit each repository's fields independently.
fn effective_merge_policy(
    project: &Value,
    scope: &Value,
) -> Result<crate::model::ProjectLandingMergePlan> {
    fn parse(value: Option<&Value>) -> Result<crate::model::ProjectLandingMergePlan> {
        match value {
            Some(value) => {
                if let Some(enabled) = value.get("enabled") {
                    if !enabled.is_boolean() {
                        bail!("merge.enabled must be boolean");
                    }
                }
                Ok(serde_json::from_value(value.clone())?)
            }
            None => Ok(Default::default()),
        }
    }
    let mut policy = parse(project["landing"].get("merge"))?;
    let overrides = parse(scope.get("merge"))?;
    if overrides.enabled.is_some() {
        policy.enabled = overrides.enabled;
    }
    for (id, override_policy) in overrides.repositories {
        let inherited = policy.repositories.entry(id).or_default();
        if override_policy.enabled.is_some() {
            inherited.enabled = override_policy.enabled;
        }
        if override_policy.mode.is_some() {
            inherited.mode = override_policy.mode;
        }
    }
    Ok(policy)
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
    if plan["merge"]["enabled"] == false {
        if plan["merge"]["repositories"]
            .as_object()
            .is_some_and(|repos| repos.values().any(|policy| policy["enabled"] == true))
        {
            println!(
                "Source integration: declared procedure with explicit repository merge exceptions"
            );
        } else {
            println!("Source integration: declared procedure (automatic merges disabled)");
        }
    }
    println!(
        "Maximum parallel commands: {} (checkout and resource locks can serialize them)",
        plan["maxParallel"].as_u64().unwrap_or(4)
    );
    for (i, wave) in waves.iter().enumerate() {
        println!("Wave {}: {}", i + 1, wave.join(" | "));
    }
    for step in steps {
        if step["interactive"] == true {
            println!("{}: attached local terminal required", step["id"]);
        }
        if step["type"] == "manual" {
            println!(
                "{}: manual acknowledgement required — {}",
                step["id"],
                step["instructions"].as_str().unwrap_or("")
            );
        }
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
