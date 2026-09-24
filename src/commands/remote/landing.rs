//! Versioned landing plan transport. Edits use ancestry, never last-write-wins.
//! Runtime ownership tokens are local private state, not part of synced plans.

use super::client::{
    effective_workspace_config, request, request_json, resolve_project_id, resolve_remote,
    resolve_token,
};
use crate::model::KnitRemote;
use crate::store::{read_json, write_json};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlanRecord {
    bundle_slug: String,
    revision: u64,
    hash: String,
    #[serde(default)]
    parent_hash: Option<String>,
    plan: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunRecord {
    bundle_slug: String,
    run: Value,
}

#[derive(Default, Debug, Serialize, Deserialize)]
struct Artifacts {
    #[serde(default)]
    plans: Vec<PlanRecord>,
    #[serde(default)]
    runs: Vec<RunRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlanCursor {
    file: String,
    hash: String,
    revision: u64,
    bundle_slug: String,
}

#[derive(Default, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SyncIndex {
    project: String,
    remote: String,
    #[serde(default)]
    plans: BTreeMap<String, PlanCursor>,
    /// Immutable revisions remain executable for recovery after a newer edit.
    #[serde(default)]
    plan_versions: BTreeMap<String, String>,
    #[serde(default)]
    runs: BTreeMap<String, String>,
    #[serde(default)]
    recipe_hash: Option<String>,
}

#[derive(Deserialize)]
struct RecipeRecord {
    landing: Value,
    hash: String,
}

fn local_recipes(root: &Path, project: &str) -> Result<(Value, Value)> {
    let document: Value = read_json(&crate::store::project_path(root, project))?;
    let landing = document
        .get("landing")
        .filter(|v| !v.is_null())
        .cloned()
        .unwrap_or_else(|| json!({}));
    Ok((document, landing))
}

fn install_recipes(root: &Path, index: &mut SyncIndex, remote: RecipeRecord) -> Result<bool> {
    if !remote.landing.is_object() || document_hash(&remote.landing) != remote.hash {
        bail!("Invalid project landing recipe hash");
    }
    let (mut project, local) = local_recipes(root, &index.project)?;
    let local_hash = document_hash(&local);
    if local_hash == remote.hash {
        index.recipe_hash = Some(remote.hash);
        return Ok(false);
    }
    if index.recipe_hash.as_ref() == Some(&remote.hash) {
        return Ok(false); // Only the local recipe moved.
    }
    if index.recipe_hash.as_ref() != Some(&local_hash) {
        let candidate = root.join(".knit/landing-sync/conflicts").join(format!(
            "{}-{}.recipes.json",
            index.project,
            &remote.hash[..12]
        ));
        save(&candidate, &remote.landing)?;
        bail!("Landing recipes have divergent or untracked local changes. Project preserved; remote recipe candidate: {}. Back up the local edit, adopt the candidate landing fields and pull --plans to record that base, then reapply your edits and push --plans.", candidate.display());
    }
    project["landing"] = remote.landing;
    save(&crate::store::project_path(root, &index.project), &project)?;
    index.recipe_hash = Some(remote.hash);
    Ok(true)
}

fn push_recipes(
    root: &Path,
    index: &mut SyncIndex,
    remote: &KnitRemote,
    token: &str,
) -> Result<()> {
    let (_, landing) = local_recipes(root, &index.project)?;
    let hash = document_hash(&landing);
    if index.recipe_hash.as_ref() == Some(&hash) {
        return Ok(());
    }
    let path = format!("/projects/{}/landing-recipes", index.project);
    let current: RecipeRecord = request_json(remote, token, "GET", &path, None)?;
    if document_hash(&current.landing) != current.hash {
        bail!("Invalid remote recipe hash");
    }
    if current.hash == hash {
        index.recipe_hash = Some(hash);
        return Ok(());
    }
    let previous = index.recipe_hash.as_ref().context("Landing recipes have no shared sync ancestor. Pull --plans and reconcile the remote recipe candidate before pushing local recipe changes.")?;
    let saved: RecipeRecord = request_json(
        remote,
        token,
        "PUT",
        &path,
        Some(&json!({"landing":landing,"expectedHash":previous})),
    )?;
    if saved.hash != hash || saved.landing != landing {
        bail!("Server returned different landing recipes");
    }
    index.recipe_hash = Some(saved.hash);
    Ok(())
}

/// Canonical serde JSON maps are ordered, including maps nested in arrays.
pub(crate) fn document_hash(value: &Value) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).expect("JSON serializes"))
    )
}

fn identifier(value: &str) -> Result<&str> {
    if value.is_empty()
        || value == "."
        || value.contains("..")
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
    {
        bail!("Unsafe landing artifact identifier");
    }
    Ok(value)
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .with_context(|| format!("Landing artifact is missing {key}"))
}

pub(crate) fn environment_key(plan: &Value) -> String {
    if let Some(lane) = plan.get("lane").and_then(Value::as_str) {
        return format!("lane:{lane}");
    }
    if let Some(branch) = plan.get("targetBranch").and_then(Value::as_str) {
        return format!("target:{branch}");
    }
    "default".to_string()
}

fn plan_key(plan: &Value) -> Result<String> {
    Ok(format!(
        "{}:{}",
        identifier(text(plan, "bundleId")?)?,
        environment_key(plan)
    ))
}

fn plan_file(plan: &Value) -> Result<String> {
    let bundle = identifier(text(plan, "bundleId")?)?;
    let destination = environment_key(plan);
    if destination == "default" {
        Ok(format!("{bundle}.land.json"))
    } else {
        Ok(format!(
            "{bundle}--{}.land.json",
            &document_hash(&json!(destination))[..12]
        ))
    }
}

fn save(path: &Path, value: &impl Serialize) -> Result<()> {
    fs::create_dir_all(path.parent().context("artifact parent missing")?)?;
    write_json(path, value)
}

fn index_path(root: &Path, project: &str, remote: &str) -> PathBuf {
    let key = document_hash(&json!([project, remote]));
    root.join(".knit/landing-sync").join(format!("{key}.json"))
}

fn load_index(root: &Path, project: &str, remote: &str) -> Result<SyncIndex> {
    let path = index_path(root, project, remote);
    if path.exists() {
        read_json(&path)
    } else {
        Ok(SyncIndex {
            project: project.into(),
            remote: remote.into(),
            ..Default::default()
        })
    }
}

fn json_files(dir: &Path, suffix: &str) -> Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file() && entry.file_name().to_string_lossy().ends_with(suffix) {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

fn belongs_to_project(root: &Path, bundle: &str, project: &str) -> Result<bool> {
    let path = root
        .join(".knit/bundles")
        .join(format!("{}.bundle.json", identifier(bundle)?));
    if !path.exists() {
        return Ok(false);
    }
    let value: Value = read_json(&path)?;
    Ok(value.get("projectId").and_then(Value::as_str) == Some(project))
}

fn outgoing(root: &Path, index: &SyncIndex) -> Result<Artifacts> {
    let mut result = Artifacts::default();
    let mut slots = BTreeMap::new();
    for path in json_files(&root.join(".knit/land-plans"), ".land.json")? {
        let plan: Value = read_json(&path)?;
        if plan.get("schemaVersion").and_then(Value::as_str) != Some("0.2") {
            continue;
        }
        let bundle = text(&plan, "bundleId")?;
        if !belongs_to_project(root, bundle, &index.project)? {
            continue;
        }
        let key = plan_key(&plan)?;
        let hash = document_hash(&plan);
        if let Some(previous) = slots.insert(key.clone(), hash.clone()) {
            if previous != hash {
                bail!("Multiple edited landing plans for {bundle} have the same destination; select one before syncing.");
            }
            continue;
        }
        let cursor = index.plans.get(&key);
        if cursor.is_some_and(|c| c.hash == hash) {
            continue;
        }
        result.plans.push(PlanRecord {
            bundle_slug: bundle.into(),
            revision: cursor.map_or(1, |c| c.revision + 1),
            hash,
            parent_hash: cursor.map(|c| c.hash.clone()),
            plan,
        });
    }
    let mut run_hashes = BTreeMap::new();
    for path in json_files(&root.join(".knit/land-runs"), ".run.json")? {
        let run: Value = read_json(&path)?;
        if run.get("schemaVersion").and_then(Value::as_str) != Some("0.2") {
            continue;
        }
        let bundle = text(&run, "bundleId")?;
        if !belongs_to_project(root, bundle, &index.project)? {
            continue;
        }
        let id = text(&run, "id")?;
        let hash = document_hash(&run);
        if let Some(prior) = run_hashes.insert(id.to_owned(), hash.clone()) {
            if prior != hash {
                bail!("Multiple local files contain divergent landing run {id}; reconcile those receipts before syncing");
            }
            continue;
        }
        if index.runs.get(id) == Some(&hash) {
            continue;
        }
        // Running state belongs to its owning runner; it must not be replayed
        // from another machine's stale copy through ordinary sync.
        if run.get("status").and_then(Value::as_str) == Some("running") {
            continue;
        }
        result.runs.push(RunRecord {
            bundle_slug: bundle.into(),
            run,
        });
    }
    Ok(result)
}

/// Preserve a locally edited document when both sides moved. Save the remote
/// candidate separately so resolution never loses either version.
fn install_plan(root: &Path, index: &mut SyncIndex, record: &PlanRecord) -> Result<()> {
    identifier(&record.bundle_slug)?;
    if text(&record.plan, "bundleId")? != record.bundle_slug
        || document_hash(&record.plan) != record.hash
    {
        bail!("Landing plan identity or content hash does not match its envelope");
    }
    if record.plan.get("kind").and_then(Value::as_str) != Some("KnitLandPlan")
        || record.plan.get("schemaVersion").and_then(Value::as_str) != Some("0.2")
    {
        bail!("Unsupported landing plan document");
    }
    let key = plan_key(&record.plan)?;
    let file = plan_file(&record.plan)?;
    let path = root.join(".knit/land-plans").join(&file);
    let old = index.plans.get(&key);
    if old.is_some_and(|old| record.revision == old.revision && old.hash != record.hash) {
        bail!(
            "Remote rewrote immutable landing revision {}",
            record.revision
        );
    }
    let history = root
        .join(".knit/land-plans/revisions")
        .join(&record.bundle_slug)
        .join(format!("{}.land.json", record.hash));
    save(&history, &record.plan)?;
    index.plan_versions.insert(record.hash.clone(), key.clone());
    if let Some(old) = old {
        if record.revision < old.revision {
            return Ok(());
        }
    }
    if path.exists() {
        let local: Value = read_json(&path)?;
        let local_hash = document_hash(&local);
        // A pull of the same upstream revision is not a conflict with local
        // edits. Keep the pending edit and its original parent for the push.
        if local_hash != record.hash && old.is_some_and(|c| c.hash == record.hash) {
            return Ok(());
        }
        if local_hash != record.hash && old.is_none_or(|c| c.hash != local_hash) {
            let candidate = root.join(".knit/land-plans/conflicts").join(format!(
                "{}-{}.land.json",
                record.bundle_slug,
                &record.hash[..12]
            ));
            save(&candidate, &record.plan)?;
            bail!("Landing plan has local edits and a different remote revision. Local file preserved: {}. Remote candidate: {}", path.display(), candidate.display());
        }
    }
    save(&path, &record.plan)?;
    index.plans.insert(
        key,
        PlanCursor {
            file,
            hash: record.hash.clone(),
            revision: record.revision,
            bundle_slug: record.bundle_slug.clone(),
        },
    );
    Ok(())
}

fn install_run(root: &Path, index: &mut SyncIndex, record: &RunRecord) -> Result<()> {
    identifier(&record.bundle_slug)?;
    let id = identifier(text(&record.run, "id")?)?;
    if text(&record.run, "bundleId")? != record.bundle_slug {
        bail!("Landing run belongs to a different bundle");
    }
    if record.run["schemaVersion"] != "0.2" || record.run["kind"] != "KnitLandRun" {
        bail!("Unsupported landing run document");
    }
    let hash = document_hash(&record.run);
    // Local execution names files by bundle and invocation, while imported
    // receipts are named by logical id. Reuse the existing file by identity.
    let mut existing = Vec::new();
    for path in json_files(&root.join(".knit/land-runs"), ".run.json")? {
        let local: Value = read_json(&path)?;
        if local["id"] == id {
            existing.push(path);
        }
    }
    if existing.len() > 1 {
        bail!("Multiple local files contain landing run {id}; reconcile those receipts before syncing");
    }
    let path = existing
        .pop()
        .unwrap_or_else(|| root.join(".knit/land-runs").join(format!("{id}.run.json")));
    if path.exists() {
        let local: Value = read_json(&path)?;
        let local_hash = document_hash(&local);
        if local_hash != hash && index.runs.get(id) != Some(&local_hash) {
            let candidate = root
                .join(".knit/land-runs/conflicts")
                .join(format!("{id}-{}.run.json", &hash[..12]));
            private_save(&candidate, &record.run)?;
            bail!(
                "Landing run {id} has local execution receipts; remote candidate preserved at {}",
                candidate.display()
            );
        }
    }
    private_save(&path, &record.run)?;
    index.runs.insert(id.into(), hash);
    Ok(())
}

pub(super) fn push_plans(project: Option<&str>, remote_name: &str, required: bool) -> Result<()> {
    let (root, config) = effective_workspace_config()?;
    let _lock = crate::store::acquire_named_lock(&root, "landing-sync")?;
    let project = resolve_project_id(&root, &config, project)?;
    let mut index = load_index(&root, &project, remote_name)?;
    let payload = outgoing(&root, &index)?;
    if !required && payload.plans.is_empty() && payload.runs.is_empty() && index.plans.is_empty() {
        return Ok(());
    }
    let remote = resolve_remote(&config, remote_name)?;
    let token = resolve_token(remote_name, remote)?;
    push_recipes(&root, &mut index, remote, &token)?;
    save(&index_path(&root, &project, remote_name), &index)?;
    if payload.plans.is_empty() && payload.runs.is_empty() {
        return Ok(());
    }
    let _: Value = request_json(
        remote,
        &token,
        "POST",
        &format!("/projects/{project}/landing-artifacts"),
        Some(&serde_json::to_value(&payload)?),
    )?;
    // The server commits the import atomically. Only record ancestry after it
    // accepts; a lost response is safe to retry by immutable content hash.
    for record in &payload.plans {
        let key = plan_key(&record.plan)?;
        save(
            &root
                .join(".knit/land-plans/revisions")
                .join(&record.bundle_slug)
                .join(format!("{}.land.json", record.hash)),
            &record.plan,
        )?;
        index.plan_versions.insert(record.hash.clone(), key.clone());
        index.plans.insert(
            key,
            PlanCursor {
                file: plan_file(&record.plan)?,
                hash: record.hash.clone(),
                revision: record.revision,
                bundle_slug: record.bundle_slug.clone(),
            },
        );
    }
    for record in &payload.runs {
        index
            .runs
            .insert(text(&record.run, "id")?.into(), document_hash(&record.run));
    }
    save(&index_path(&root, &project, remote_name), &index)?;
    println!(
        "Pushed {} landing plan(s), {} run(s) to {remote_name}",
        payload.plans.len(),
        payload.runs.len()
    );
    Ok(())
}

pub(super) fn pull_plans(project: Option<&str>, remote_name: &str, required: bool) -> Result<()> {
    let (root, config) = effective_workspace_config()?;
    let _lock = crate::store::acquire_named_lock(&root, "landing-sync")?;
    let project = resolve_project_id(&root, &config, project)?;
    let remote = resolve_remote(&config, remote_name)?;
    let token = resolve_token(remote_name, remote)?;
    let response = request(
        remote,
        &token,
        "GET",
        &format!("/projects/{project}/landing-artifacts"),
        None,
    )?;
    if response.status == 404 {
        if required {
            bail!("{remote_name}: this server does not support landing plan synchronization; upgrade it before using --plans");
        }
        eprintln!(
            "{remote_name}: landing plan synchronization is not supported by this server yet"
        );
        return Ok(());
    }
    let artifacts: Artifacts = super::client::decode_response(response)?;
    let mut index = load_index(&root, &project, remote_name)?;
    let recipes: RecipeRecord = request_json(
        remote,
        &token,
        "GET",
        &format!("/projects/{project}/landing-recipes"),
        None,
    )?;
    let mut errors = Vec::new();
    match install_recipes(&root, &mut index, recipes) {
        Ok(true) => crate::commands::refresh_agents(Some(&project))?,
        Ok(false) => (),
        Err(error) => errors.push(error.to_string()),
    }
    // Newest first; older revisions are retained only in server history.
    let mut plans = artifacts.plans;
    plans.sort_by_key(|p| std::cmp::Reverse(p.revision));
    for record in &plans {
        if let Err(error) = install_plan(&root, &mut index, record) {
            errors.push(error.to_string());
        }
    }
    for record in &artifacts.runs {
        if let Err(error) = install_run(&root, &mut index, record) {
            errors.push(error.to_string());
        }
    }
    save(&index_path(&root, &project, remote_name), &index)?;
    if !errors.is_empty() {
        bail!(
            "Landing sync needs conflict resolution:\n{}",
            errors.join("\n")
        );
    }
    println!(
        "Pulled {} landing revision(s), {} run(s) from {remote_name}",
        plans.len(),
        artifacts.runs.len()
    );
    Ok(())
}

/// Private handle intentionally has no Debug/Serialize: it contains credentials.
pub(crate) struct LandingOwnership {
    root: PathBuf,
    project: String,
    remote_name: String,
    remote: KnitRemote,
    api_token: String,
    id: String,
    token: String,
    file: PathBuf,
    bundle_slug: String,
}

fn private_save(path: &Path, value: &Value) -> Result<()> {
    use std::io::Write;
    fs::create_dir_all(path.parent().context("ownership directory missing")?)?;
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let temporary = path.with_extension(format!("{}.tmp", crate::ids::node_id("private")));
    let mut file = options.open(&temporary)?;
    file.write_all(serde_json::to_string(value)?.as_bytes())?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    #[cfg(unix)]
    fs::File::open(path.parent().context("ownership directory missing")?)?.sync_all()?;
    Ok(())
}

/// Call only after acquiring the local environment lock. A synced plan must
/// be unchanged and claim the same shared authority on every machine.
pub(crate) fn claim_for_run(
    root: &Path,
    plan: &Value,
    prior_run: Option<&Value>,
    recovering: bool,
) -> Result<Option<LandingOwnership>> {
    let _lock = crate::store::acquire_named_lock(root, "landing-sync")?;
    let key = plan_key(plan)?;
    let hash = document_hash(plan);
    let mut matches = Vec::new();
    for path in json_files(&root.join(".knit/landing-sync"), ".json")? {
        let index: SyncIndex = read_json(&path)?;
        if let Some(cursor) = index.plans.get(&key) {
            if cursor.hash != hash && index.plan_versions.get(&hash) != Some(&key) {
                bail!("This synced landing plan has unsaved local edits. Run `knit sync push --plans` before execution.");
            }
            matches.push(index);
        }
    }
    if matches.is_empty() {
        return Ok(None);
    }
    let config = crate::store::load_effective_config(root)?;
    let mut authorities = BTreeMap::new();
    for index in matches {
        let remote = resolve_remote(&config, &index.remote)?;
        authorities.entry(remote.url.clone()).or_insert(index);
    }
    if authorities.len() != 1 {
        bail!("This plan is synchronized with multiple execution authorities. Select a single authority before landing.");
    }
    let index = authorities.into_values().next().expect("one authority");
    let remote = resolve_remote(&config, &index.remote)?.clone();
    let api_token = resolve_token(&index.remote, &remote)?;
    let file = root
        .join(".knit/landing-ownership")
        .join(format!("{hash}.json"));
    let mut payload = json!({"bundleSlug": text(plan, "bundleId")?, "planHash": hash, "environment": environment_key(plan), "owner": format!("local:{}", document_hash(&json!(root.to_string_lossy()))) });
    payload["action"] = json!(if recovering {
        "recover"
    } else if prior_run.is_some() {
        "resume"
    } else {
        "apply"
    });
    if let Some(run) = prior_run {
        payload["runIdentity"] = json!(text(run, "id")?);
    }
    if file.exists() {
        let previous: Value = read_json(&file)?;
        payload["id"] = previous["id"].clone();
        payload["token"] = previous["token"].clone();
        if let Some(run) = prior_run.filter(|_| {
            previous["action"] != payload["action"]
                || previous["runIdentity"] != payload["runIdentity"]
        }) {
            // The retained token proves continuity of exclusion. First bind
            // this durable journal to that ownership, then request its new
            // intent. Never attest quiescence based merely on a dead parent.
            let quiescent = run["steps"].as_array().is_some_and(|steps| {
                steps.iter().all(|step| {
                    step["status"] != "running"
                        && (step["attribution"] != "uncertain" || step["quiesced"] == true)
                        && !step["attempts"].as_array().is_some_and(|attempts| {
                            attempts
                                .iter()
                                .any(|attempt| attempt["status"] == "running")
                        })
                })
            });
            if !quiescent {
                bail!("Interrupted landing ownership remains held: reconcile outstanding processes and record quiescence before changing execution intent");
            }
            let _: Value = request_json(
                &remote,
                &api_token,
                "POST",
                &format!("/projects/{}/landing-artifacts", index.project),
                Some(
                    &json!({"plans":[], "runs":[{"bundleSlug":text(plan,"bundleId")?,"run":run}], "ownership":{"id":previous["id"],"token":previous["token"]}}),
                ),
            )?;
            payload["quiescent"] = json!(true);
        }
    }
    let lease: Value = request_json(
        &remote,
        &api_token,
        "POST",
        &format!("/projects/{}/landing-ownership", index.project),
        Some(&payload),
    )?;
    let id = text(&lease, "id")?.to_string();
    let token = text(&lease, "token")?.to_string();
    private_save(
        &file,
        &json!({"id": id, "token": token, "action":payload["action"], "runIdentity":payload["runIdentity"]}),
    )?;
    Ok(Some(LandingOwnership {
        root: root.into(),
        project: index.project,
        remote_name: index.remote,
        remote,
        api_token,
        id,
        token,
        file,
        bundle_slug: text(plan, "bundleId")?.into(),
    }))
}

/// Persist receipts even on failure. Release only after the executor confirms
/// quiescence; retain ownership for interrupted/unknown effects and recovery.
pub(crate) fn finish_for_plan(
    ownership: Option<LandingOwnership>,
    run: &Value,
    release: bool,
) -> Result<()> {
    let Some(lease) = ownership else {
        return Ok(());
    };
    let _: Value = request_json(
        &lease.remote,
        &lease.api_token,
        "POST",
        &format!("/projects/{}/landing-artifacts", lease.project),
        Some(
            &json!({"plans":[],"runs":[{"bundleSlug":lease.bundle_slug,"run":run}],"ownership":{"id":lease.id,"token":lease.token}}),
        ),
    )?;
    // A completed execution is an accepted local receipt, just like sync push.
    // Remember its exact content so a later pull can distinguish upstream
    // changes from unpushed recovery work instead of reporting a false conflict.
    {
        let _lock = crate::store::acquire_named_lock(&lease.root, "landing-sync")?;
        let mut index = load_index(&lease.root, &lease.project, &lease.remote_name)?;
        index
            .runs
            .insert(text(run, "id")?.into(), document_hash(run));
        save(
            &index_path(&lease.root, &lease.project, &lease.remote_name),
            &index,
        )?;
    }
    if release {
        let response = request(
            &lease.remote,
            &lease.api_token,
            "DELETE",
            &format!("/projects/{}/landing-ownership/{}", lease.project, lease.id),
            Some(&json!({"token":lease.token,"quiescent":true})),
        )?;
        if !(200..300).contains(&response.status) {
            bail!("Execution finished, but shared landing ownership could not be released (HTTP {}). Retry synchronization before another landing.", response.status);
        }
        fs::remove_file(&lease.file).with_context(|| {
            format!(
                "remove completed local ownership in {}",
                lease.root.display()
            )
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn temp() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "knit-plan-sync-{}-{}",
            std::process::id(),
            crate::ids::node_id("test")
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }
    fn record(revision: u64, command: &str) -> PlanRecord {
        let plan = json!({"schemaVersion":"0.2","kind":"KnitLandPlan","id":"land-demo","bundleId":"demo","steps":[{"id":"deploy","command":[command]}]});
        PlanRecord {
            bundle_slug: "demo".into(),
            revision,
            hash: document_hash(&plan),
            parent_hash: None,
            plan,
        }
    }
    #[test]
    fn divergent_pull_preserves_both_edits() {
        let root = temp();
        let mut index = SyncIndex::default();
        let first = record(1, "old");
        install_plan(&root, &mut index, &first).unwrap();
        let local = record(2, "local");
        save(&root.join(".knit/land-plans/demo.land.json"), &local.plan).unwrap();
        let remote = record(2, "remote");
        assert!(install_plan(&root, &mut index, &remote)
            .unwrap_err()
            .to_string()
            .contains("Local file preserved"));
        let current: Value = read_json(&root.join(".knit/land-plans/demo.land.json")).unwrap();
        assert_eq!(current, local.plan);
        assert_eq!(index.plans.values().next().unwrap().hash, first.hash);
        assert_eq!(
            json_files(&root.join(".knit/land-plans/conflicts"), ".land.json")
                .unwrap()
                .len(),
            1
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn unchanged_local_fast_forwards_but_old_remote_cannot_rewind() {
        let root = temp();
        let mut index = SyncIndex::default();
        let first = record(1, "old");
        install_plan(&root, &mut index, &first).unwrap();
        let next = record(2, "new");
        install_plan(&root, &mut index, &next).unwrap();
        install_plan(&root, &mut index, &first).unwrap();
        let current: Value = read_json(&root.join(".knit/land-plans/demo.land.json")).unwrap();
        assert_eq!(current, next.plan);
        assert_eq!(
            index.plan_versions.get(&first.hash),
            Some(&plan_key(&first.plan).unwrap())
        );
        assert!(root
            .join(".knit/land-plans/revisions/demo")
            .join(format!("{}.land.json", first.hash))
            .exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn rejects_path_traversal_and_tampered_hashes() {
        let root = temp();
        let mut index = SyncIndex::default();
        let mut bad = record(1, "ok");
        bad.bundle_slug = "../../escape".into();
        assert!(install_plan(&root, &mut index, &bad).is_err());
        let mut bad = record(1, "ok");
        bad.plan["steps"][0]["command"] = json!(["tampered"]);
        assert!(install_plan(&root, &mut index, &bad).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn distinct_environments_do_not_overwrite_default_plan() {
        let root = temp();
        let mut index = SyncIndex::default();
        let default = record(1, "default");
        install_plan(&root, &mut index, &default).unwrap();
        let mut staging = record(1, "stage");
        staging.plan["lane"] = json!("staging");
        staging.hash = document_hash(&staging.plan);
        install_plan(&root, &mut index, &staging).unwrap();
        assert_eq!(index.plans.len(), 2);
        assert_ne!(
            plan_file(&default.plan).unwrap(),
            plan_file(&staging.plan).unwrap()
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn run_pull_never_replaces_unpushed_recovery_receipts() {
        let root = temp();
        let mut index = SyncIndex::default();
        let r = RunRecord {
            bundle_slug: "demo".into(),
            run: json!({"schemaVersion":"0.2","kind":"KnitLandRun","id":"run-demo","bundleId":"demo","status":"failed"}),
        };
        install_run(&root, &mut index, &r).unwrap();
        let mut local = r.run.clone();
        local["recovery"] = json!({"status":"restored"});
        save(&root.join(".knit/land-runs/run-demo.run.json"), &local).unwrap();
        let mut newer = r;
        newer.run["detail"] = json!("remote");
        assert!(install_run(&root, &mut index, &newer).is_err());
        let actual: Value = read_json(&root.join(".knit/land-runs/run-demo.run.json")).unwrap();
        assert_eq!(actual, local);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn pulling_unchanged_upstream_keeps_pending_local_edit() {
        let root = temp();
        let mut index = SyncIndex::default();
        let base = record(1, "base");
        install_plan(&root, &mut index, &base).unwrap();
        let edit = record(2, "edit");
        save(&root.join(".knit/land-plans/demo.land.json"), &edit.plan).unwrap();
        install_plan(&root, &mut index, &base).unwrap();
        let actual: Value = read_json(&root.join(".knit/land-plans/demo.land.json")).unwrap();
        assert_eq!(actual, edit.plan);
        assert_eq!(index.plans.values().next().unwrap().revision, 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn imported_receipt_reuses_local_run_filename_and_keeps_captures_private() {
        let root = temp();
        let mut index = SyncIndex::default();
        let mut record = RunRecord {
            bundle_slug: "demo".into(),
            run: json!({"schemaVersion":"0.2","kind":"KnitLandRun","id":"run-demo","bundleId":"demo","status":"failed","steps":[{"capture":{"secret":"synthetic-secret"}}]}),
        };
        let original = root.join(".knit/land-runs/land-demo-invocation.run.json");
        save(&original, &record.run).unwrap();
        install_run(&root, &mut index, &record).unwrap();
        assert_eq!(
            json_files(&root.join(".knit/land-runs"), ".run.json").unwrap(),
            vec![original.clone()]
        );
        record.run["recoveryStatus"] = json!("restored");
        install_run(&root, &mut index, &record).unwrap();
        assert_eq!(read_json::<Value>(&original).unwrap(), record.run);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&original).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let mut local = record.run.clone();
        local["detail"] = json!("local edit");
        save(&original, &local).unwrap();
        record.run["detail"] = json!("remote edit");
        assert!(install_run(&root, &mut index, &record).is_err());
        let conflicts = json_files(&root.join(".knit/land-runs/conflicts"), ".run.json").unwrap();
        assert_eq!(conflicts.len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&conflicts[0]).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(root).unwrap();
    }
}
