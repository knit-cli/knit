use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn canonical_hash(value: &Value) -> String {
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                Value::Object(map.iter().map(|(k, v)| (k.clone(), sorted(v))).collect())
            }
            Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
            _ => value.clone(),
        }
    }
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&sorted(value)).expect("JSON value"))
    )
}

/// Portable source identity excludes machine bindings and receipt-only metadata.
pub(crate) fn bundle_fingerprint(value: &Value) -> String {
    fn fields(v: &Value, names: &[&str]) -> Value {
        Value::Object(
            names
                .iter()
                .filter_map(|k| {
                    v.get(*k)
                        .filter(|v| !v.is_null())
                        .map(|v| ((*k).into(), v.clone()))
                })
                .collect(),
        )
    }
    let mut repos: Vec<_> = value["repos"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|v| {
            fields(
                v,
                &[
                    "id",
                    "remote",
                    "baseBranch",
                    "baseSha",
                    "featureBranch",
                    "headSha",
                ],
            )
        })
        .collect();
    repos.sort_by_key(|v| v["id"].as_str().unwrap_or("").to_owned());
    let mut pubs: Vec<_> = value["publications"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|v| {
            fields(
                v,
                &[
                    "repoId",
                    "provider",
                    "kind",
                    "number",
                    "url",
                    "baseBranch",
                    "headBranch",
                ],
            )
        })
        .collect();
    pubs.sort_by_key(|v| v["repoId"].as_str().unwrap_or("").to_owned());
    let changed = serde_json::from_value::<crate::model::ChangeGroup>(value.clone())
        .map(|b| crate::commands::publish::publish_scope_repo_ids(&b))
        .unwrap_or_default();
    canonical_hash(
        &json!({"id":value["id"],"projectId":value["projectId"],"repos":repos,"publications":pubs,"changedRepos":changed}),
    )
}
pub(crate) fn project_fingerprint(value: &Value) -> String {
    let mut repos: Vec<_> = value["repos"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| {
            let mut v = serde_json::Map::new();
            for key in ["id", "remote", "baseBranch"] {
                if let Some(x) = r.get(key).filter(|v| !v.is_null()) {
                    v.insert(key.into(), x.clone());
                }
            }
            Value::Object(v)
        })
        .collect();
    repos.sort_by_key(|v| v["id"].as_str().unwrap_or("").to_owned());
    canonical_hash(&json!({"id":value["id"],"landing":value["landing"],"repos":repos}))
}

pub(super) fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}
pub(super) fn effect(step: &Value) -> &str {
    step["effect"]
        .as_str()
        .unwrap_or_else(|| match step["type"].as_str() {
            Some("merge_pr" | "merge_branch") => "source",
            Some("wait_checks") => "read_only",
            Some("run")
                if matches!(step["role"].as_str(), Some("build" | "verify" | "capture")) =>
            {
                "read_only"
            }
            Some("deploy") => "deployment",
            _ => "external",
        })
}
pub(super) fn recovery(step: &Value) -> Value {
    if step["recovery"].is_object() {
        return step["recovery"].clone();
    }
    match effect(step) {
        "read_only" => json!({"mode":"none"}),
        "source" if step["type"] == "merge_pr" => json!({"mode":"revert_pr"}),
        _ => json!({"mode":"manual", "reason":"No executable compensation declared"}),
    }
}

pub(super) fn compile(plan: &Value) -> Result<(Vec<Value>, Vec<Vec<String>>)> {
    let mut steps = plan["steps"]
        .as_array()
        .context("steps must be an array")?
        .clone();
    if steps.is_empty() {
        bail!("plan must contain at least one step");
    }
    let mut ids = BTreeSet::new();
    for step in &steps {
        let id = step["id"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .context("step id required")?;
        if !ids.insert(id.to_owned()) {
            bail!("duplicate step {id}");
        }
    }
    if let Some(workflow) = plan.get("workflow") {
        let mut edges = BTreeMap::new();
        fn walk(
            node: &Value,
            prior: &BTreeSet<String>,
            edges: &mut BTreeMap<String, BTreeSet<String>>,
        ) -> Result<BTreeSet<String>> {
            let obj = node
                .as_object()
                .context("workflow node must be an object")?;
            if obj.len() != 1 {
                bail!("workflow node must have exactly one of step, sequence, parallel");
            }
            if let Some(id) = node.get("step") {
                let id = id.as_str().context("workflow step must be a string")?;
                if edges.insert(id.into(), prior.clone()).is_some() {
                    bail!("workflow repeats {id}");
                }
                return Ok(BTreeSet::from([id.into()]));
            }
            let sequential = obj.contains_key("sequence");
            let children = node[if sequential { "sequence" } else { "parallel" }]
                .as_array()
                .context("unknown workflow node")?;
            if children.is_empty() {
                bail!("workflow groups must not be empty");
            }
            let mut frontier = prior.clone();
            let mut leaves = BTreeSet::new();
            for child in children {
                let ends = walk(child, if sequential { &frontier } else { prior }, edges)?;
                if sequential {
                    frontier = ends;
                } else {
                    leaves.extend(ends);
                }
            }
            Ok(if sequential { frontier } else { leaves })
        }
        walk(workflow, &BTreeSet::new(), &mut edges)?;
        if edges.keys().cloned().collect::<BTreeSet<_>>() != ids {
            bail!("workflow must reference every step exactly once and no unknown steps");
        }
        for step in &mut steps {
            step["needs"] = json!(edges[step["id"].as_str().unwrap()]);
        }
    }
    for step in &steps {
        if let Some(needs) = step.get("needs") {
            let a = needs.as_array().context("needs must be a string array")?;
            if a.iter().any(|n| !n.is_string()) {
                bail!("needs must be a string array");
            }
        }
        for need in strings(&step["needs"]) {
            if !ids.contains(&need) {
                bail!("{} needs unknown step {need}", step["id"]);
            }
        }
    }
    let mut done = BTreeSet::new();
    let mut waves = vec![];
    while done.len() < ids.len() {
        let wave: Vec<String> = steps
            .iter()
            .filter(|s| {
                !done.contains(s["id"].as_str().unwrap())
                    && strings(&s["needs"]).iter().all(|n| done.contains(n))
            })
            .map(|s| s["id"].as_str().unwrap().to_owned())
            .collect();
        if wave.is_empty() {
            bail!("dependency cycle in landing plan");
        }
        done.extend(wave.iter().cloned());
        waves.push(wave);
    }
    // Required inputs survive workflow edits: authoring order cannot remove data dependencies.
    let mut ancestors: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for id in waves.iter().flatten() {
        let step = steps.iter().find(|s| s["id"] == *id).unwrap();
        let mut all = BTreeSet::new();
        for n in strings(&step["needs"]) {
            all.extend(ancestors.get(&n).cloned().unwrap_or_default());
            all.insert(n);
        }
        for required in strings(&step["requires"]) {
            if !all.contains(&required) {
                bail!("{id} requires prior output from {required}");
            }
        }
        if step["role"] == "build" || step["type"] == "deploy" || step.get("sourceRepos").is_some()
        {
            for producer in &steps {
                if matches!(producer["type"].as_str(), Some("merge_pr" | "merge_branch"))
                    && (producer["repoId"] == step["repoId"]
                        || strings(&step["sourceRepos"])
                            .iter()
                            .any(|r| producer["repoId"] == *r))
                    && !all.contains(producer["id"].as_str().unwrap())
                {
                    bail!(
                        "{id}: builds and deployments must consume their prerequisite merge {}",
                        producer["id"]
                    );
                }
            }
        }
        ancestors.insert(id.clone(), all);
    }
    Ok((steps, waves))
}

fn check_spec(spec: &Value, context: &str) -> Result<()> {
    let argv = spec["command"]
        .as_array()
        .with_context(|| format!("{context}: command argv required"))?;
    if argv.is_empty() || argv.iter().any(|v| !v.is_string()) || argv[0].as_str() == Some("") {
        bail!("{context}: command must be nonempty string argv");
    }
    if spec.get("timeoutSeconds").is_some() && spec["timeoutSeconds"].as_u64().unwrap_or(0) == 0 {
        bail!("{context}: timeoutSeconds must be positive");
    }
    if let Some(cwd) = spec.get("cwd") {
        let cwd = cwd.as_str().context("cwd must be string")?;
        if std::path::Path::new(cwd).is_absolute()
            || std::path::Path::new(cwd)
                .components()
                .any(|p| matches!(p, std::path::Component::ParentDir))
        {
            bail!("{context}: cwd must be repo-relative without parent traversal");
        }
    }
    if let Some(env) = spec.get("env") {
        if !env
            .as_object()
            .is_some_and(|e| e.values().all(Value::is_string))
        {
            bail!("{context}: env must map strings to strings");
        }
    }
    Ok(())
}

pub(crate) fn validation(plan: &Value, bundle: Option<&Value>, project: Option<&Value>) -> Value {
    let mut errors = vec![];
    let mut waves = vec![];
    let mut manual = vec![];
    let check = (|| -> Result<()> {
        if plan["kind"] != "KnitLandPlan" {
            bail!("kind must be KnitLandPlan");
        }
        if !matches!(plan["schemaVersion"].as_str(), Some("0.1" | "0.2")) {
            bail!("unsupported plan schemaVersion");
        }
        let (steps, compiled) = compile(plan)?;
        waves = compiled;
        let v2 = plan["schemaVersion"] == "0.2";
        if v2 {
            super::super::ensure_provider(plan["provider"].as_str().context("provider required")?)?;
            if plan.get("terminal").is_some() && !plan["terminal"].is_boolean() {
                bail!("terminal must be boolean");
            }
            if plan["targetBranch"].is_string() && plan["lane"].is_string() {
                bail!("targetBranch and lane are mutually exclusive");
            }
            if let Some(version) = plan.get("requiredExecutorVersion") {
                if version != "0.2" && version != env!("CARGO_PKG_VERSION") {
                    bail!("unsupported requiredExecutorVersion");
                }
            }
            for key in ["id", "bundleId", "bundleFingerprint", "projectFingerprint"] {
                if plan[key].as_str().is_none_or(|s| s.trim().is_empty()) {
                    bail!("{key} required");
                }
            }
            let bundle_id = plan["bundleId"].as_str().unwrap();
            if bundle_id.contains(['/', '\\']) || matches!(bundle_id, "." | "..") {
                bail!("bundleId must be a portable single path component");
            }
            for key in ["targetBranch", "lane"] {
                if let Some(value) = plan.get(key) {
                    if value.as_str().is_none_or(|s| s.trim().is_empty()) {
                        bail!("{key} must be a nonempty string");
                    }
                }
            }
            for key in ["bundleFingerprint", "projectFingerprint"] {
                let h = plan[key].as_str().unwrap();
                if h.len() != 64
                    || !h
                        .bytes()
                        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
                {
                    bail!("{key} must be lowercase SHA-256");
                }
            }
            if plan.get("maxParallel").is_some() && plan["maxParallel"].as_u64().unwrap_or(0) == 0 {
                bail!("maxParallel must be positive");
            }
            if !matches!(plan["onFailure"].as_str(), None | Some("stop" | "recover")) {
                bail!("v0.2 onFailure must be stop or recover");
            }
        }
        for s in &steps {
            let id = s["id"].as_str().unwrap();
            match s["type"].as_str() {
                Some("run" | "deploy") if s["deploymentMode"] != "push" => {
                    if v2 {
                        check_spec(s, id)?;
                    }
                }
                Some("merge_pr" | "merge_branch" | "wait_checks" | "deploy") => {
                    if !s["repoId"].is_string() {
                        bail!("{id}: repoId required");
                    }
                }
                _ => bail!("{id}: unsupported operation type"),
            }
            if !v2 {
                continue;
            }
            if s["repoId"].as_str().is_none_or(|r| r.trim().is_empty()) {
                bail!("{id}: portable operations require repoId");
            }
            if let Some(sources) = s.get("sourceRepos") {
                let sources = sources
                    .as_array()
                    .context("sourceRepos must be an array of repository IDs")?;
                let mut seen = BTreeSet::new();
                for source in sources {
                    let repo = source
                        .as_str()
                        .filter(|s| !s.trim().is_empty())
                        .context("sourceRepos must contain nonempty strings")?;
                    if !seen.insert(repo) {
                        bail!("{id}: duplicate sourceRepos entry {repo}");
                    }
                    let merge = steps.iter().any(|p| {
                        matches!(p["type"].as_str(), Some("merge_pr" | "merge_branch"))
                            && p["repoId"] == repo
                    });
                    if !(merge
                        || plan["bundleHeads"][repo].is_string()
                        || strings(&plan["recipeRepos"]).iter().any(|r| r == repo)
                            && plan["recipeBases"][repo].is_string())
                    {
                        bail!("{id}: source repository {repo} lacks a pinned head, prerequisite merge, or declared project base");
                    }
                }
            }
            if let Some(runner) = s.get("runner") {
                if !matches!(runner.as_str(), Some("any" | "local" | "hosted")) {
                    bail!("{id}: unavailable runner requirement");
                }
            }
            if s.get("locks").is_some()
                && !s["locks"]
                    .as_array()
                    .is_some_and(|a| a.iter().all(Value::is_string))
            {
                bail!("{id}: locks must be strings");
            }

            if !matches!(
                effect(s),
                "source" | "deployment" | "external" | "read_only"
            ) {
                bail!("{id}: unknown effect");
            }
            if s["type"] == "merge_branch"
                && !s["targetBranch"]
                    .as_str()
                    .is_some_and(|b| !b.trim().is_empty() && !b.starts_with('-'))
            {
                bail!("{id}: merge_branch requires targetBranch");
            }
            if s["deploymentMode"] == "push" {
                if !strings(&s["command"]).is_empty() {
                    bail!("{id}: push deployment must not contain a command");
                }
                if !steps.iter().any(|p| {
                    matches!(p["type"].as_str(), Some("merge_pr" | "merge_branch"))
                        && p["repoId"] == s["repoId"]
                }) {
                    bail!("{id}: push deployment requires its source merge");
                }
            }
            let r = recovery(s);
            match r["mode"].as_str() {
                Some("command") => {
                    if s["deploymentMode"] == "push" {
                        bail!("{id}: an external push-triggered deployment cannot capture before-state after its source merge; declare manual recovery or use an explicit command adapter");
                    }
                    check_spec(&r, id)?;
                    check_spec(&r["capture"], id)?;
                    check_spec(&r["verify"], id)?;
                    if let Some(probe) = r.get("probe") {
                        check_spec(probe, id)?;
                    }
                    if r["idempotent"] != true {
                        bail!("{id}: command restoration must declare idempotent: true");
                    }
                }
                Some("none") if effect(s) == "read_only" => (),
                Some("revert_pr") if s["type"] == "merge_pr" => (),
                Some("manual") if r["reason"].as_str().is_some_and(|s| !s.trim().is_empty()) => {
                    manual.push(id.to_owned());
                }
                _ => bail!(
                    "{id}: invalid recovery declaration for effect {}",
                    effect(s)
                ),
            }
            if plan["onFailure"] == "recover"
                && matches!(effect(s), "deployment" | "external")
                && r["mode"] != "command"
            {
                bail!("{id}: automatic recovery requires capture, restore and verification");
            }
        }
        if let Some(bundle) = bundle {
            if plan["bundleId"] != bundle["id"] {
                bail!("plan belongs to a different bundle");
            }
            if v2 && plan["bundleFingerprint"] != bundle_fingerprint(bundle) {
                bail!("stale bundle fingerprint");
            }
            let typed: crate::model::ChangeGroup = serde_json::from_value(bundle.clone())?;
            if v2 {
                let expected: BTreeMap<String, String> = typed
                    .repos
                    .iter()
                    .filter_map(|r| r.head_sha.as_ref().map(|h| (r.id.clone(), h.clone())))
                    .collect();
                let actual: BTreeMap<String, String> =
                    serde_json::from_value(plan.get("bundleHeads").cloned().unwrap_or(json!({})))?;
                if expected != actual {
                    bail!("bundleHeads differ from the reviewed bundle source revisions");
                }
            }
            let changed = crate::commands::publish::publish_scope_repo_ids(&typed);
            let merged: BTreeSet<_> = steps
                .iter()
                .filter(|s| matches!(s["type"].as_str(), Some("merge_pr" | "merge_branch")))
                .filter_map(|s| s["repoId"].as_str())
                .collect();
            if plan["terminal"] != false {
                for id in &changed {
                    if !merged.contains(id.as_str()) {
                        bail!("terminal plan omits changed repository {id}");
                    }
                    if !steps
                        .iter()
                        .any(|s| s["repoId"] == *id && s["type"] == "merge_pr")
                    {
                        bail!("terminal plan requires review merge for {id}");
                    }
                }
            }
            for s in &steps {
                if let Some(id) = s["repoId"].as_str() {
                    if !(typed.repos.iter().any(|r| r.id == id)
                        || matches!(s["type"].as_str(), Some("run" | "deploy"))
                            && strings(&plan["recipeRepos"]).iter().any(|r| r == id))
                    {
                        bail!("unknown repository {id}");
                    }
                }
            }
        }
        if let Some(project) = project {
            if v2 && plan["projectFingerprint"] != project_fingerprint(project) {
                bail!("stale project fingerprint");
            }
        }
        Ok(())
    })();
    if let Err(e) = check {
        errors.push(format!("{e:#}"));
    }
    json!({"valid":errors.is_empty(),"errors":errors,"waves":waves,"recovery":{"automatic":manual.is_empty(),"manualSteps":manual}})
}
