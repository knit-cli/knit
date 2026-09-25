use super::graph::{canonical_hash, compile, effect, recovery, strings, validation};
use crate::store::read_json;
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

type Roots = BTreeMap<String, PathBuf>;

pub(super) fn unique_id(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{prefix}-{:x}-{nanos:x}-{:x}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

/// Ownership locks intentionally survive a crash. A dead parent does not prove
/// its external effects or children quiesced. Operators reconcile before removal.
struct Lock(PathBuf);
impl Lock {
    fn acquire(key: &str) -> Result<Self> {
        let dir = std::env::temp_dir().join("knit-landing-ownership");
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.lock", canonical_hash(&json!(key))));
        let mut f=OpenOptions::new().write(true).create_new(true).open(&path).with_context(||format!("landing resource is owned; reconcile and verify process quiescence before removing {}",path.display()))?;
        writeln!(f, "{}", std::process::id())?;
        f.sync_all()?;
        Ok(Self(path))
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub(super) fn durable(path: &Path, value: &Value) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let temp = path.with_extension(format!("{}.tmp", unique_id("write")));
    let mut opts = OpenOptions::new();
    opts.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&temp)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    // Close our writer before Windows replaces the destination. std::fs::rename
    // uses MoveFileExW(REPLACE_EXISTING); never delete the durable old receipt.
    drop(file);
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(error)
            .with_context(|| format!("failed to replace landing receipt {}", path.display()));
    }
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

struct Journal {
    value: Mutex<Value>,
    path: PathBuf,
    bundle: Mutex<Value>,
    bundle_out: PathBuf,
    workspace: PathBuf,
    pins: Mutex<()>,
}
impl Journal {
    fn edit(&self, f: impl FnOnce(&mut Value)) -> Result<()> {
        let mut r = self.value.lock().unwrap();
        f(&mut r);
        r["updatedAt"] = json!(now_iso());
        durable(&self.path, &r)?;
        self.persist_sources(&r)
    }
    fn persist_sources(&self, run: &Value) -> Result<()> {
        let mut bundle = self.bundle.lock().unwrap();
        let mut typed: crate::model::ChangeGroup = serde_json::from_value(bundle.clone())?;
        let run_id = run["id"].as_str().context("run id required")?;
        for s in run["steps"].as_array().context("run steps required")? {
            if !matches!(s["type"].as_str(), Some("merge_pr" | "merge_branch"))
                || !matches!(
                    s["attribution"].as_str(),
                    Some("performed" | "already_satisfied")
                )
            {
                continue;
            }
            let repo = s["repoId"].as_str().context("merge repo required")?;
            let node_id = format!("receipt-{}", canonical_hash(&json!([run_id, s["id"]])));
            if typed.nodes.iter().any(|n| n.id == node_id) {
                continue;
            }
            if let Some(branch) = s["output"]["targetBranch"].as_str() {
                typed.nodes.push(crate::model::BundleNode::branch_landed(
                    node_id,
                    now_iso(),
                    repo.into(),
                    branch.into(),
                    s["output"]["source"].as_str().map(str::to_owned),
                    run_id.into(),
                    run["plan"]["lane"].as_str().map(str::to_owned),
                ));
            }
            if s["type"] == "merge_pr" {
                for publication in &mut typed.publications {
                    if publication.repo_id == repo {
                        publication.state = "MERGED".into();
                        publication.updated_at = now_iso();
                        if let Some(branch) = s["output"]["targetBranch"].as_str() {
                            publication.base_branch = branch.into();
                        }
                    }
                }
            }
        }
        typed.head_node_id = typed.nodes.last().map(|n| n.id.clone());
        *bundle = serde_json::to_value(typed)?;
        durable(&self.bundle_out, &bundle)
    }
    fn snapshot(&self) -> Value {
        self.value.lock().unwrap().clone()
    }
    fn step(&self, id: &str) -> Value {
        self.snapshot()["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .unwrap()
            .clone()
    }
    fn edit_step(&self, id: &str, f: impl FnOnce(&mut Value)) -> Result<()> {
        self.edit(|r| {
            let s = r["steps"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|s| s["id"] == id)
                .unwrap();
            f(s)
        })
    }
}

fn absolute(p: &Path) -> Result<PathBuf> {
    Ok(if p.is_absolute() {
        p.into()
    } else {
        std::env::current_dir()?.join(p)
    })
}
fn roots_from_file(path: Option<&Path>) -> Result<Roots> {
    path.map(read_json)
        .transpose()
        .map(Option::unwrap_or_default)
}
fn cwd(step: &Value, spec: &Value, roots: &Roots) -> Result<PathBuf> {
    let repo = step["repoId"]
        .as_str()
        .context("command needs repoId and runner repo-root binding")?;
    let root = roots
        .get(repo)
        .with_context(|| format!("missing repo-root binding for {repo}"))?;
    let sub = spec["cwd"].as_str().or(step["cwd"].as_str()).unwrap_or(".");
    let result = dunce::canonicalize(root.join(sub))
        .with_context(|| format!("cwd unavailable for {}", step["id"]))?;
    if !result.starts_with(dunce::canonicalize(root)?) {
        bail!("cwd escapes runner checkout");
    }
    Ok(result)
}
pub(super) fn command_path(step: &Value, spec: &Value, dir: &Path) -> Result<PathBuf> {
    let name = spec["command"][0]
        .as_str()
        .context("command argv required")?;
    let child_path = [&spec["env"], &step["env"]].into_iter().find_map(|env| {
        env.as_object()?
            .iter()
            .find(|(key, _)| {
                if cfg!(windows) {
                    key.eq_ignore_ascii_case("PATH")
                } else {
                    key.as_str() == "PATH"
                }
            })
            .and_then(|(_, value)| value.as_str())
            .map(std::ffi::OsString::from)
    });
    let parent_path = std::env::var_os("PATH").unwrap_or_default();
    let explicit =
        name.contains('/') || cfg!(windows) && (name.contains('\\') || name.contains(':'));
    let mut candidates = Vec::new();
    if explicit {
        let path = dir.join(name);
        #[cfg(windows)]
        if !name.to_ascii_lowercase().ends_with(".exe") {
            let mut exe = path.as_os_str().to_owned();
            exe.push(".exe");
            candidates.push(PathBuf::from(exe));
        }
        candidates.push(path);
    } else {
        let mut directories = Vec::new();
        #[cfg(windows)]
        {
            // Rust Command searches child PATH, application/system directories,
            // then parent PATH. Only .exe is inferred (not shell PATHEXT).
            if let Some(path) = &child_path {
                directories
                    .extend(std::env::split_paths(path).filter(|p| !p.as_os_str().is_empty()));
            }
            if let Some(parent) = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(Path::to_path_buf))
            {
                directories.push(parent);
            }
            if let Some(windows) = std::env::var_os("SystemRoot") {
                let windows = PathBuf::from(windows);
                directories.push(windows.join("System32"));
                directories.push(windows);
            }
            directories
                .extend(std::env::split_paths(&parent_path).filter(|p| !p.as_os_str().is_empty()));
        }
        #[cfg(not(windows))]
        directories.extend(std::env::split_paths(
            child_path.as_ref().unwrap_or(&parent_path),
        ));
        for directory in directories {
            let candidate = dir.join(directory).join(name);
            #[cfg(windows)]
            let candidate = if name.contains('.') {
                candidate
            } else {
                candidate.with_extension("exe")
            };
            candidates.push(candidate);
        }
    }
    for path in candidates {
        if !path.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if path.metadata()?.permissions().mode() & 0o111 == 0 {
                continue;
            }
        }
        return Ok(path);
    }
    bail!("runner missing executable {name} in {}", dir.display())
}
fn executable(step: &Value, spec: &Value, dir: &Path) -> Result<()> {
    command_path(step, spec, dir).map(|_| ())
}

fn preflight(
    plan: &Value,
    steps: &[Value],
    roots: &Roots,
    bundle: &Value,
    skip_checks: bool,
) -> Result<()> {
    // Required checks must speak for the sources actually being integrated;
    // see effective_required_checks_bundle.
    let effective_bundle = super::mergeability::effective_required_checks_bundle(plan, bundle);
    let check_bundle = crate::store::ActiveBundle::unlocked(
        std::env::current_dir()?,
        PathBuf::new(),
        serde_json::from_value(effective_bundle)?,
    );
    if !skip_checks {
        super::super::validate::preflight_required_checks(
            &check_bundle,
            &strings(&plan["requireChecks"]),
            false,
        )?;
    }
    for (repo, root) in roots {
        if let Some(pin) = plan["bundleHeads"][repo].as_str() {
            // Verify object availability without switching or modifying the checkout.
            let resolved = git(root, &["rev-parse", &format!("{pin}^{{commit}}")])?;
            if resolved != pin {
                bail!("{repo}: source pin is not an exact commit SHA");
            }
        }
    }
    let mut suffixes = BTreeSet::new();
    for repo in roots.keys() {
        let suffix: String = repo
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect();
        if !suffixes.insert(suffix) {
            bail!("repo IDs collide in canonical KNIT_CHECKOUT bindings");
        }
    }
    for root in roots.values() {
        if !root.is_absolute() || !root.is_dir() {
            bail!("repo roots must be existing absolute paths");
        }
    }
    if let Some(caps) = plan.get("requiredCapabilities") {
        for cap in strings(caps) {
            if ![
                "commands",
                "recovery",
                "workflow",
                "wave-v1",
                "source-merges",
                "repository-sequence",
                "mergeability-preflight",
                "integration-sources",
            ]
            .contains(&cap.as_str())
            {
                bail!("runner lacks capability {cap}");
            }
        }
    }
    for step in steps {
        if matches!(step["type"].as_str(), Some("merge_pr" | "merge_branch"))
            && !step["repoId"]
                .as_str()
                .is_some_and(|id| roots.contains_key(id))
        {
            bail!("source mutation requires a repo-root binding for before-state capture and exact merge pins");
        }
        if matches!(step["type"].as_str(), Some("run" | "deploy"))
            && step["deploymentMode"] != "push"
        {
            executable(step, step, &cwd(step, step, roots)?)?;
            let r = recovery(step);
            if r["mode"] == "command" {
                for spec in [&r["capture"], &r, &r["verify"]] {
                    executable(step, spec, &cwd(step, spec, roots)?)?;
                }
                if let Some(p) = r.get("probe") {
                    executable(step, p, &cwd(step, p, roots)?)?;
                }
            }
        } else if step["type"] == "manual" {
            let r = recovery(step);
            if r["mode"] == "command" {
                for spec in [&r["capture"], &r, &r["verify"]] {
                    executable(step, spec, &cwd(step, spec, roots)?)?;
                }
                if let Some(probe) = r.get("probe") {
                    executable(step, probe, &cwd(step, probe, roots)?)?;
                }
            }
        } else if matches!(step["type"].as_str(), Some("merge_pr" | "wait_checks")) {
            let typed: crate::model::ChangeGroup = serde_json::from_value(bundle.clone())?;
            let id = step["repoId"].as_str().context("repoId required")?;
            let repo = typed
                .repos
                .iter()
                .find(|r| r.id == id)
                .context("unknown repo")?;
            let forge = crate::providers::for_repo(repo)?;
            let target = provider_target(roots, forge.as_ref(), repo)?;
            let pub_ = crate::providers::publication_for_repo(&typed, id)
                .context("missing publication")?;
            let pr = forge.view(&target, &pub_.url)?;
            if !super::super::state_is_merged(&pr) {
                super::super::ensure_open_and_ready(id, &pr)?;
            }
            if let Some(pin) = plan["bundleHeads"][id].as_str() {
                if pr.head_ref_oid.as_deref().is_some_and(|head| head != pin) {
                    bail!("{id}: live review head differs from reviewed plan");
                }
            }
        }
    }
    Ok(())
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git").args(args).current_dir(root).output()?;
    if !output.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8(output.stdout)?.trim().into())
}

/// Commands run from immutable merge revisions. Mutable user/source checkouts
/// are never switched. A checkout is created once and reused by compensation.
fn pinned_roots(step: &Value, roots: &Roots, journal: &Journal, phase: &str) -> Result<Roots> {
    let id = step["id"].as_str().unwrap();
    let _pin_guard = journal.pins.lock().unwrap();
    let record = journal.step(id);
    let snapshot = journal.snapshot();
    let (steps, _) = compile(&snapshot["plan"])?;
    fn visit(id: &str, steps: &[Value], all: &mut BTreeSet<String>) {
        if !all.insert(id.into()) {
            return;
        }
        if let Some(s) = steps.iter().find(|s| s["id"] == id) {
            for n in strings(&s["needs"]) {
                visit(&n, steps, all);
            }
        }
    }
    let mut ancestors = BTreeSet::new();
    for n in strings(&step["needs"]) {
        visit(&n, &steps, &mut ancestors);
    }
    let mut revisions = record["sourceRevisions"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    if revisions.is_empty() {
        for ancestor in ancestors {
            let producer = steps.iter().find(|s| s["id"] == ancestor).unwrap();
            if matches!(producer["type"].as_str(), Some("merge_pr" | "merge_branch")) {
                let prior = journal.step(&ancestor);
                let revision = prior["output"]["revision"].as_str().context(
                    "required merge did not produce a pinned revision; supply repo roots",
                )?;
                revisions.insert(producer["repoId"].as_str().unwrap().into(), json!(revision));
            }
        }
        // A consumer's first resolved revision is immutable for this whole run.
        // The pin mutex covers resolution and durable publication across parallel steps.
        if let Some(repo) = step["repoId"].as_str() {
            if !revisions.contains_key(repo) {
                if let Some(rev) = snapshot["steps"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find_map(|s| s["sourceRevisions"][repo].as_str())
                {
                    revisions.insert(repo.into(), json!(rev));
                }
            }
        }
        if let Some(branch) = step["checkout"]["branch"].as_str() {
            let repo = step["repoId"]
                .as_str()
                .context("checkout repoId required")?;
            if !revisions.contains_key(repo) {
                let root = roots.get(repo).context("checkout binding missing")?;
                let remote = step["checkout"]["remote"].as_str().unwrap_or("origin");
                git(root, &["fetch", "--no-tags", remote, branch])?;
                revisions.insert(repo.into(), json!(git(root, &["rev-parse", "FETCH_HEAD"])?));
            }
        }
        if let Some(repo) = step["repoId"].as_str() {
            if !revisions.contains_key(repo) {
                if let Some(rev) = snapshot["plan"]["bundleHeads"][repo].as_str() {
                    revisions.insert(repo.into(), json!(rev));
                }
            }
        }
        if let Some(repo) = step["repoId"].as_str() {
            if !revisions.contains_key(repo) {
                if let Some(branch) = snapshot["plan"]["recipeBases"][repo].as_str() {
                    let root = roots.get(repo).context("consumer binding required")?;
                    git(root, &["fetch", "--no-tags", "origin", branch])?;
                    revisions.insert(repo.into(), json!(git(root, &["rev-parse", "FETCH_HEAD"])?));
                }
            }
        }
        for repo in strings(&step["sourceRepos"]) {
            if revisions.contains_key(&repo) {
                continue;
            }
            let root = roots.get(&repo).context("sourceRepos binding missing")?;
            let previous = snapshot["steps"]
                .as_array()
                .unwrap()
                .iter()
                .find_map(|s| s["sourceRevisions"][&repo].as_str());
            let revision =
                if let Some(rev) = previous.or(snapshot["plan"]["bundleHeads"][&repo].as_str()) {
                    rev.to_owned()
                } else {
                    let branch = snapshot["plan"]["recipeBases"][&repo]
                        .as_str()
                        .context("sourceRepos requires an immutable source or project base")?;
                    git(root, &["fetch", "--no-tags", "origin", branch])?;
                    git(root, &["rev-parse", "FETCH_HEAD"])?
                };
            revisions.insert(repo, json!(revision));
        }
        journal.edit_step(id, |s| s["sourceRevisions"] = json!(revisions))?;
    }
    let _ = phase;
    let mut bound = roots.clone();
    for (repo, rev) in revisions {
        let root = roots
            .get(&repo)
            .context("pinned checkout binding missing")?;
        let rev = rev.as_str().context("revision must be string")?;
        let checkout = journal
            .path
            .with_extension("checkouts")
            .join(canonical_hash(&json!([repo, rev])));
        if !checkout.exists() {
            fs::create_dir_all(checkout.parent().unwrap())?;
            git(
                root,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    checkout.to_str().context("checkout path invalid")?,
                    rev,
                ],
            )?;
        }
        if git(&checkout, &["rev-parse", "HEAD"])? != rev {
            bail!("pinned checkout changed outside the runner");
        }
        bound.insert(repo, dunce::canonicalize(checkout)?);
    }
    Ok(bound)
}

fn run_command(
    step: &Value,
    spec: &Value,
    roots: &Roots,
    journal: &Journal,
    phase: &str,
    capture: Option<&Value>,
) -> Result<Value> {
    let id = step["id"].as_str().unwrap();
    let snapshot = journal.snapshot();
    let operation = format!("{}:{id}", snapshot["id"].as_str().unwrap());
    let attempt = unique_id("attempt");
    let dir = journal.path.with_extension("outputs");
    fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    }
    let output = dir.join(format!("{attempt}.json"));
    let capture_file = dir.join(format!(
        "{}.capture.json",
        canonical_hash(&json!(operation))
    ));
    if let Some(capture) = capture {
        durable(&capture_file, capture)?;
    }
    let bound_roots = pinned_roots(step, roots, journal, phase)?;
    let mut cmd = Command::new(command_path(step, spec, &cwd(step, spec, &bound_roots)?)?);
    cmd.args(strings(&spec["command"]).into_iter().skip(1))
        .current_dir(cwd(step, spec, &bound_roots)?);
    for env in [&step["env"], &spec["env"]] {
        if let Some(env) = env.as_object() {
            for (k, v) in env {
                cmd.env(k, v.as_str().context("env values must be strings")?);
            }
        }
    }
    // Authoritative run contract, gated behind executor 0.4 so older plans'
    // recipe environments are untouched. Applied after recipe env so a recipe
    // can supplement the environment but never rewrite the landing's own
    // pins: the environment being landed into (the lane or target name), the
    // selected integration input (the override SHA or the reviewed bundle
    // head — distinct from the merge output), the revision the checkout
    // actually runs from (the merged revision), and the resolved target
    // branch (the repository's own merge destination for this plan).
    if snapshot["plan"]["requiredExecutorVersion"] == "0.4" {
        if let Some(repo) = step["repoId"].as_str() {
            let resolved = journal.step(id);
            let input_source = snapshot["plan"]["integrationSources"][repo]["sha"]
                .as_str()
                .or_else(|| snapshot["plan"]["bundleHeads"][repo].as_str());
            if let Some(source) = input_source {
                cmd.env("KNIT_SOURCE_SHA", source);
            }
            let revision = resolved["sourceRevisions"][repo].as_str();
            if let Some(revision) = revision {
                cmd.env("KNIT_REV", revision);
            }
            let environment = snapshot["plan"]["lane"]
                .as_str()
                .or_else(|| snapshot["plan"]["targetBranch"].as_str())
                .unwrap_or("");
            cmd.env("KNIT_LAND_ENVIRONMENT", environment);
            let repo_target = snapshot["plan"]["steps"].as_array().and_then(|steps| {
                steps
                    .iter()
                    .find(|s| s["type"] == "merge_branch" && s["repoId"].as_str() == Some(repo))
            });
            let target = step["targetBranch"]
                .as_str()
                .or_else(|| repo_target.and_then(|s| s["targetBranch"].as_str()))
                .or_else(|| snapshot["plan"]["targetBranches"][repo].as_str())
                .or_else(|| snapshot["plan"]["targetBranch"].as_str());
            cmd.env("KNIT_TARGET_BRANCH", target.unwrap_or(""));
        }
    }
    cmd.env("KNIT_LAND_OPERATION_ID", &operation)
        .env("KNIT_LAND_ATTEMPT_ID", &attempt)
        .env("KNIT_LAND_RUN_FILE", &journal.path)
        .env("KNIT_LAND_OUTPUT_FILE", &output)
        .env("KNIT_LAND_PHASE", phase);
    if let Some(capture) = capture {
        cmd.env("KNIT_LAND_CAPTURE", serde_json::to_string(capture)?)
            .env("KNIT_LAND_CAPTURE_FILE", &capture_file);
    }
    // Every command consumes the pinned receipts of prerequisite merges.
    let outputs: BTreeMap<String, Value> = snapshot["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| {
            s.get("output")
                .map(|o| (s["id"].as_str().unwrap().to_owned(), o.clone()))
        })
        .collect();
    cmd.env("KNIT_LAND_INPUTS", serde_json::to_string(&outputs)?);
    let mut suffixes = BTreeSet::new();
    for (repo, root) in &bound_roots {
        let suffix: String = repo
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect();
        if !suffixes.insert(suffix.clone()) {
            bail!("repo IDs collide in canonical KNIT_CHECKOUT bindings");
        }
        cmd.env(format!("KNIT_CHECKOUT_{suffix}"), root);
        cmd.env(format!("KNIT_CHECKOUT_{repo}"), root);
    }
    let working = cwd(step, spec, &bound_roots)?;
    cmd.env("KNIT_DEPLOY_CHECKOUT", &working)
        .env("KNIT_BUNDLE", snapshot["bundleId"].as_str().unwrap_or(""));
    if let Some(repo) = step["repoId"].as_str() {
        cmd.env("KNIT_REPO", repo);
    }
    cmd.env("KNIT_ROOT", &journal.workspace);
    journal.edit_step(id, |s| {
        let attempts = s
            .as_object_mut()
            .unwrap()
            .entry("attempts")
            .or_insert(json!([]));
        attempts
            .as_array_mut()
            .unwrap()
            .push(json!({"id":attempt,"phase":phase,"status":"running","startedAt":now_iso()}));
    })?;
    let timeout = Some(spec["timeoutSeconds"].as_u64().unwrap_or(1800));
    let result = if phase == "forward" && step["interactive"] == true {
        super::super::process::run_attached(&mut cmd, timeout)
    } else {
        super::super::process::run_captured(&mut cmd, timeout)
    };
    let receipt = match result {
        Ok(o) => {
            json!({"id":attempt,"phase":phase,"status":if o.status.success() && !o.timed_out && !o.cancelled {"succeeded"}else{"failed"},"stdout":o.stdout,"stderr":o.stderr,"exitCode":o.status.code(),"timedOut":o.timed_out,"cancelled":o.cancelled,"finishedAt":now_iso(),"output":if output.exists(){read_json::<Value>(&output).unwrap_or(Value::Null)}else{Value::Null}})
        }
        Err(e) => {
            json!({"id":attempt,"phase":phase,"status":"failed","error":format!("{e:#}"),"finishedAt":now_iso()})
        }
    };
    let mut receipt = receipt;
    receipt["stepId"] = json!(id);
    journal.edit_step(id, |s| {
        let a = s["attempts"].as_array_mut().unwrap();
        let prev = a.iter_mut().find(|a| a["id"] == attempt).unwrap();
        *prev = receipt.clone();
    })?;
    Ok(receipt)
}
fn require_success(receipt: &Value) -> Result<()> {
    if receipt["status"] != "succeeded" {
        bail!(
            "step {} phase {} failed (exit {}, timeout {}, cancelled {}): {} {}",
            receipt["stepId"],
            receipt["phase"],
            receipt["exitCode"],
            receipt["timedOut"],
            receipt["cancelled"],
            receipt["error"].as_str().unwrap_or(""),
            receipt["stderr"].as_str().unwrap_or("")
        );
    }
    Ok(())
}

fn provider_target(
    roots: &Roots,
    forge: &dyn crate::providers::Forge,
    repo: &crate::model::RepoEntry,
) -> Result<crate::providers::PrTarget> {
    if let Some(root) = roots.get(&repo.id) {
        Ok(crate::providers::PrTarget::checkout(root))
    } else {
        super::super::artifact_target(&std::env::current_dir()?, forge, repo)
    }
}

fn provider_step(
    step: &Value,
    plan: &Value,
    bundle: &Value,
    roots: &Roots,
    journal: &Journal,
) -> Result<Value> {
    let typed: crate::model::ChangeGroup = serde_json::from_value(bundle.clone())?;
    let id = step["repoId"].as_str().context("repoId required")?;
    let repo = typed
        .repos
        .iter()
        .find(|r| r.id == id)
        .context("unknown repo")?;
    let forge = crate::providers::for_repo(repo)?;
    let target = provider_target(roots, forge.as_ref(), repo)?;
    let sid = step["id"].as_str().unwrap();
    if matches!(step["type"].as_str(), Some("merge_pr" | "merge_branch")) {
        let branch = step["targetBranch"]
            .as_str()
            .or(plan["targetBranches"][id].as_str())
            .or(plan["targetBranch"].as_str())
            .or_else(|| {
                crate::providers::publication_for_repo(&typed, id).map(|p| p.base_branch.as_str())
            })
            .context("source destination required")?;
        if let Some(root) = roots.get(id) {
            // Read-only pre-mutation state. A target that disappeared or a
            // transport that cannot answer refused the step before any
            // effect: a known no-effect failure, not an uncertain one.
            let before =
                crate::git::remote_ref_sha(root, "origin", &format!("refs/heads/{branch}"))
                    .map_err(|e| {
                        super::mergeability::KnownNoEffect(format!(
                            "{id}: reading target branch {branch} from origin failed: {e:#}"
                        ))
                    })?
                    .ok_or_else(|| {
                        super::mergeability::KnownNoEffect(format!(
                    "{id}: target branch {branch} is missing from origin; nothing was merged"
                ))
                    })?;
            journal.edit_step(sid, |s| {
                s["before"] = json!({"targetBranch":branch,"revision":before})
            })?;
        }
    }
    if step["type"] == "merge_branch" {
        let branch = step["targetBranch"]
            .as_str()
            .context("branch target required")?;
        if let Some(pub_) = crate::providers::publication_for_repo(&typed, id) {
            let pr = forge.view(&target, &pub_.url)?;
            if pr.base_ref_name.as_deref() == Some(branch) {
                bail!("branch merge would close review; use terminal review merge");
            }
        }
        // The integrated source is the plan's pinned integration source when
        // present, otherwise the reviewed bundle head. Integration sources
        // never change the recorded bundle pins.
        let Some((source_branch, head)) = super::mergeability::merge_source(plan, bundle, id)
        else {
            bail!("{id}: pinned source SHA required");
        };
        let integration = plan["integrationSources"][id].as_object().is_some();
        if let Some(root) = roots.get(id) {
            // An integration source is certified by its own provenance check,
            // never by the original review's green checks. A provenance
            // failure refuses before any effect: record it as safely
            // retryable, not uncertain.
            if integration {
                if let Some(reviewed) = plan["bundleHeads"][id].as_str() {
                    let branch = source_branch
                        .as_deref()
                        .context("integration source branch required")?;
                    super::mergeability::verify_integration_source(
                        root, id, branch, &head, reviewed,
                    )
                    .map_err(|e| super::mergeability::KnownNoEffect(format!("{e:#}")))?;
                }
            }
            // Expected-target contract: when the mergeability preflight
            // recorded the tip this merge was planned against, the merge
            // fetches the target, requires it to be exactly that tip, and
            // publishes with a conditional (force-with-lease) update, so
            // intervening work is refused rather than overwritten.
            let expected = journal
                .step(sid)
                .get("expectedTarget")
                .cloned()
                .unwrap_or(Value::Null);
            let expected_sha = expected["sha"]
                .as_str()
                .filter(|_| expected["branch"].as_str() == Some(branch));
            let mut bound = repo.clone();
            bound.path = root.to_string_lossy().into();
            let workspace = journal.path.parent().context("run parent required")?;
            let outcome = crate::commands::merge::merge_branch_into_target(
                workspace,
                &bound,
                &head,
                branch,
                true,
                expected_sha,
            )?;
            return Ok(
                json!({"attribution":if outcome.merged {"performed"}else{"already_satisfied"},"source":head,"sourceBranch":source_branch,"revision":outcome.after_sha,"targetBranch":branch}),
            );
        }
        let status = forge.merge_branch(&target, branch, &head)?;
        return Ok(
            json!({"attribution":if matches!(status,crate::providers::BranchMergeStatus::Merged){"performed"}else{"already_satisfied"},"source":head,"sourceBranch":source_branch,"targetBranch":branch}),
        );
    }
    if step["type"] == "deploy" && step["deploymentMode"] == "push" {
        return Ok(
            json!({"attribution":"already_satisfied","detail":"push completed by prerequisite merge"}),
        );
    }
    let pub_ = crate::providers::publication_for_repo(&typed, id).context("missing review")?;
    let mut pr = forge.view(&target, &pub_.url)?;
    let desired = step["targetBranch"]
        .as_str()
        .or(plan["targetBranches"][id].as_str())
        .or(plan["targetBranch"].as_str())
        .unwrap_or(&pub_.base_branch);
    if step["type"] == "wait_checks" {
        forge.wait_for_checks(
            &target,
            &pub_.url,
            step["requiredOnly"].as_bool().unwrap_or(true),
            step["timeoutSeconds"].as_u64().unwrap_or(1800),
            step["intervalSeconds"].as_u64().unwrap_or(10),
        )?;
        return Ok(json!({"attribution":"already_satisfied"}));
    }
    if super::super::state_is_merged(&pr) {
        if pr.base_ref_name.as_deref().unwrap_or(&pub_.base_branch) != desired {
            bail!("review merged into another destination");
        }
        return Ok(
            json!({"attribution":"already_satisfied","publicationUrl":pub_.url,"source":pr.head_ref_oid,"targetBranch":desired}),
        );
    }
    if pr.base_ref_name.as_deref().unwrap_or(&pub_.base_branch) != desired {
        forge.edit_base(&target, &pub_.url, desired)?;
        pr = forge.view(&target, &pub_.url)?;
        if pr.base_ref_name.as_deref() != Some(desired) {
            bail!("review retarget not confirmed");
        }
    }
    super::super::ensure_open_and_ready(id, &pr)?;
    if step["waitForChecks"].as_bool().unwrap_or(true) {
        forge.wait_for_checks(
            &target,
            &pub_.url,
            step["requiredChecksOnly"].as_bool().unwrap_or(true),
            step["timeoutSeconds"].as_u64().unwrap_or(1800),
            step["intervalSeconds"].as_u64().unwrap_or(10),
        )?;
    }
    let pin = plan["bundleHeads"][id]
        .as_str()
        .or(pr.head_ref_oid.as_deref());
    journal.edit_step(sid,|s|s["reviewBefore"]=json!({"publicationUrl":pub_.url,"state":pr.state,"source":pin,"targetBranch":desired}))?;
    forge.merge(
        &target,
        &pub_.url,
        step["method"].as_str().unwrap_or("merge"),
        step["deleteBranch"].as_bool().unwrap_or(false),
        pin,
    )?;
    // Persist accepted effect before making any further provider call.
    let output = json!({"attribution":"performed","publicationUrl":pub_.url,"source":pin,"targetBranch":desired});
    journal.edit_step(sid, |s| {
        s["attribution"] = json!("performed");
        s["output"] = output.clone();
    })?;
    Ok(output)
}

fn pin_merge_result(step: &Value, bundle: &Value, roots: &Roots, output: &mut Value) -> Result<()> {
    if !matches!(step["type"].as_str(), Some("merge_pr" | "merge_branch")) {
        return Ok(());
    }
    let id = step["repoId"].as_str().context("merge repo required")?;
    if !output["revision"].is_string() {
        if step["type"] != "merge_pr" {
            bail!("branch merge receipt lacks an immutable commit identity; reconcile before resuming");
        }
        let typed: crate::model::ChangeGroup = serde_json::from_value(bundle.clone())?;
        let repo = typed
            .repos
            .iter()
            .find(|r| r.id == id)
            .context("unknown merge repo")?;
        let forge = crate::providers::for_repo(repo)?;
        let target = provider_target(roots, forge.as_ref(), repo)?;
        let publication =
            crate::providers::publication_for_repo(&typed, id).context("missing review")?;
        output["revision"] = json!(forge.merged_revision(&target, &publication.url)?.context(
            "provider has not reported an immutable merged revision; resume after reconciliation"
        )?);
    }
    let revision = output["revision"].as_str().unwrap();
    if ![40, 64].contains(&revision.len()) || !revision.bytes().all(|c| c.is_ascii_hexdigit()) {
        bail!("provider returned invalid merged commit identity");
    }
    if let Some(root) = roots.get(id) {
        let object = format!("{revision}^{{commit}}");
        if git(root, &["cat-file", "-e", &object]).is_err()
            && git(root, &["fetch", "--no-tags", "origin", revision]).is_err()
        {
            let branch = output["targetBranch"]
                .as_str()
                .context("merge destination required")?;
            git(root, &["fetch", "--no-tags", "origin", branch])?;
        }
        if git(root, &["rev-parse", &object])? != revision {
            bail!("merge object identity mismatch");
        }
    }
    Ok(())
}

fn needs_terminal(step: &Value) -> bool {
    step["interactive"] == true || step["type"] == "manual"
}

fn terminal_preflight(plan: &Value, local: bool) -> Result<()> {
    use std::io::IsTerminal;
    let (steps, _) = compile(plan)?;
    if steps.iter().any(needs_terminal) {
        if !local {
            bail!("interactive/manual steps require local execution; hosted/artifact runners cannot acknowledge them");
        }
        if !std::io::stdin().is_terminal()
            || !std::io::stdout().is_terminal()
            || !std::io::stderr().is_terminal()
        {
            bail!("interactive/manual steps require a local terminal (TTY) before any landing mutations");
        }
        #[cfg(unix)]
        {
            // A tty descriptor alone is insufficient: this process must own
            // its foreground before any earlier merge or external effect.
            let foreground = unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) };
            if foreground < 0 || foreground != unsafe { libc::getpgrp() } {
                bail!("interactive/manual steps require the controlling foreground terminal before any landing mutations");
            }
        }
    }
    Ok(())
}

fn manual_checkpoint(step: &Value) -> Result<Value> {
    #[cfg(not(unix))]
    use std::io::Read;
    use std::io::Write;
    eprintln!(
        "\nManual checkpoint {}: {}",
        step["id"].as_str().unwrap(),
        step["instructions"].as_str().unwrap()
    );
    eprint!("Type acknowledge to confirm completion, optionally followed by a space and notes (max 4096 bytes): ");
    std::io::stderr().flush()?;
    let mut answer = Vec::new();
    // Read a bounded line; never retain a terminal transcript or synthesize an answer.
    #[cfg(not(unix))]
    let stdin = std::io::stdin();
    #[cfg(not(unix))]
    let mut input = stdin.lock();
    while answer.len() <= 4096 {
        if super::super::process::cancellation_requested() {
            bail!("manual checkpoint cancelled; effect requires reconciliation");
        }
        #[cfg(unix)]
        {
            let mut fd = libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll receives a valid single stack-owned descriptor.
            let ready = unsafe { libc::poll(&mut fd, 1, 100) };
            if ready < 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                bail!("manual terminal read failed");
            }
            if ready <= 0 {
                continue;
            }
        }
        let mut byte = [0];
        #[cfg(unix)]
        let count = unsafe { libc::read(libc::STDIN_FILENO, byte.as_mut_ptr().cast(), 1) };
        #[cfg(not(unix))]
        let count = input.read(&mut byte)? as isize;
        if count < 0 {
            bail!("manual terminal read failed");
        }
        if count == 0 {
            bail!(
                "manual checkpoint ended without acknowledgement; effect requires reconciliation"
            );
        }
        if byte[0] == b'\n' {
            break;
        }
        answer.push(byte[0]);
    }
    if answer.len() > 4096 {
        bail!("manual checkpoint notes exceed 4096 bytes");
    }
    let answer = String::from_utf8(answer).context("manual acknowledgement must be UTF-8")?;
    let answer = answer.trim();
    let notes = answer
        .strip_prefix("acknowledge")
        .filter(|s| s.is_empty() || s.starts_with(char::is_whitespace))
        .context("manual checkpoint not acknowledged; effect requires reconciliation")?
        .trim();
    Ok(
        json!({"attribution":if effect(step) == "read_only" {"already_satisfied"} else {"performed"},"acknowledged":true,"notes":notes}),
    )
}

fn forward_step(
    step: &Value,
    plan: &Value,
    bundle: &Value,
    roots: &Roots,
    journal: &Journal,
) -> Result<()> {
    if super::super::process::cancellation_requested() {
        bail!("landing cancelled before scheduling another effect");
    }
    let id = step["id"].as_str().unwrap();
    let r = recovery(step);
    let old = journal.step(id);
    if old["status"] == "succeeded" {
        return Ok(());
    }
    if old["attribution"] == "performed"
        && matches!(step["type"].as_str(), Some("merge_pr" | "merge_branch"))
    {
        let mut output = old["output"].clone();
        pin_merge_result(step, bundle, roots, &mut output)?;
        journal.edit_step(id, |s| {
            s["status"] = json!("succeeded");
            s["output"] = output;
        })?;
        return Ok(());
    }
    if old["attribution"] == "uncertain" {
        if let Some(probe) = r.get("probe") {
            let receipt = run_command(step, probe, roots, journal, "probe", old.get("capture"))?;
            require_success(&receipt)?;
            let observed: Value = serde_json::from_str(receipt["stdout"].as_str().unwrap_or(""))?;
            if observed["quiesced"] != true {
                bail!("{id}: probe has not confirmed quiescence");
            }
            journal.edit_step(id, |s| s["quiesced"] = json!(true))?;
            match observed["status"].as_str() {
                Some("applied") => {
                    journal.edit_step(id, |s| {
                        s["status"] = json!("succeeded");
                        s["attribution"] = json!("performed");
                        s["output"] = observed.clone();
                    })?;
                    return Ok(());
                }
                Some("absent") => (),
                _ => bail!("{id}: uncertain effect requires recovery or manual reconciliation"),
            }
        } else {
            bail!("{id}: uncertain effect cannot be retried without an authoritative probe");
        }
    }
    journal.edit_step(id, |s| {
        s["status"] = json!("running");
        s["startedAt"] = json!(now_iso());
    })?;
    if r["mode"] == "command" && !old["capture"].is_object() {
        let receipt = run_command(step, &r["capture"], roots, journal, "capture", None)?;
        require_success(&receipt)?;
        let capture: Value = serde_json::from_str(receipt["stdout"].as_str().unwrap_or(""))?;
        if !capture.is_object() {
            bail!("{id}: capture stdout must be JSON object");
        }
        journal.edit_step(id, |s| s["capture"] = capture)?;
    }
    if super::super::process::cancellation_requested() {
        bail!("landing cancelled before forward effect");
    }
    journal.edit_step(id, |s| {
        s["attribution"] = json!(if effect(step) == "read_only" {
            "already_satisfied"
        } else {
            "uncertain"
        });
        s["intentAt"] = json!(now_iso());
        s["quiesced"] = json!(false);
    })?;
    let result = if step["type"] == "manual" {
        manual_checkpoint(step)
    } else if matches!(step["type"].as_str(), Some("run" | "deploy"))
        && step["deploymentMode"] != "push"
    {
        let capture = journal.step(id).get("capture").cloned();
        let receipt = run_command(step, step, roots, journal, "forward", capture.as_ref())?;
        journal.edit_step(id, |s| {
            s["stdout"] = receipt["stdout"].clone();
            s["stderr"] = receipt["stderr"].clone();
            s["exitCode"] = receipt["exitCode"].clone();
            s["quiesced"] = json!(true);
        })?;
        require_success(&receipt).map(|()|json!({"attribution":if effect(step)=="read_only" || receipt["output"]["attribution"]=="already_satisfied" {"already_satisfied"}else{"performed"},"receipt":receipt["output"]}))
    } else {
        provider_step(step, plan, bundle, roots, journal)
    };
    match result {
        Ok(mut output) => {
            journal.edit_step(id, |s| {
                s["attribution"] = output["attribution"].clone();
                s["output"] = output.clone();
                s["quiesced"] = json!(true);
            })?;
            pin_merge_result(step, bundle, roots, &mut output)?;
            journal.edit_step(id, |s| {
                s["status"] = json!("succeeded");
                s["output"] = output;
                s["finishedAt"] = json!(now_iso());
            })
        }
        Err(e) => {
            // A typed no-effect refusal (target drift guard, integration
            // source provenance) happened before anything the step did: it is
            // safely retryable, so it must not linger as an uncertain,
            // unquiesced effect that only a recovery probe could clear.
            let known_no_effect = e
                .downcast_ref::<super::mergeability::KnownNoEffect>()
                .is_some();
            journal.edit_step(id, |s| {
                s["status"] = json!("failed");
                s["error"] = json!(format!("{e:#}"));
                s["finishedAt"] = json!(now_iso());
                if known_no_effect {
                    if let Some(record) = s.as_object_mut() {
                        record.remove("attribution");
                    }
                    s["quiesced"] = json!(true);
                }
            })?;
            Err(e)
        }
    }
}

fn resources(step: &Value, roots: &Roots, steps: &[Value]) -> BTreeSet<String> {
    let mut locks: BTreeSet<_> = strings(&step["locks"]).into_iter().collect();
    fn visit(
        id: &str,
        steps: &[Value],
        roots: &Roots,
        seen: &mut BTreeSet<String>,
        locks: &mut BTreeSet<String>,
    ) {
        if !seen.insert(id.into()) {
            return;
        }
        if let Some(s) = steps.iter().find(|s| s["id"] == id) {
            for source in strings(&s["sourceRepos"]) {
                if let Some(root) = roots.get(&source) {
                    locks.insert(format!("checkout:{}", root.display()));
                }
            }
            if let Some(root) = s["repoId"].as_str().and_then(|r| roots.get(r)) {
                locks.insert(format!("checkout:{}", root.display()));
            }
            for n in strings(&s["needs"]) {
                visit(&n, steps, roots, seen, locks);
            }
        }
    }
    for source in strings(&step["sourceRepos"]) {
        if let Some(root) = roots.get(&source) {
            locks.insert(format!("checkout:{}", root.display()));
        }
    }
    for need in strings(&step["needs"]) {
        visit(&need, steps, roots, &mut BTreeSet::new(), &mut locks);
    }

    for need in strings(&step["requires"]) {
        if let Some(repo) = need.strip_prefix("merge-") {
            if let Some(root) = roots.get(repo) {
                locks.insert(format!("checkout:{}", root.display()));
            }
        }
    }
    if let Some(root) = step["repoId"].as_str().and_then(|r| roots.get(r)) {
        locks.insert(format!("checkout:{}", root.display()));
    }
    locks
}
fn forward(plan: &Value, bundle: &Value, roots: &Roots, journal: &Journal) -> Result<()> {
    if journal.snapshot()["recoveryStartedAt"].is_string() {
        bail!("recovery has started; forward resume is permanently blocked");
    }
    let (steps, waves) = compile(plan)?;
    let max = usize::try_from(plan["maxParallel"].as_u64().unwrap_or(4))
        .context("maxParallel exceeds runner capacity range")?;
    for wave in waves {
        let mut pending: Vec<_> = wave
            .iter()
            .filter_map(|id| steps.iter().find(|s| s["id"] == *id))
            .filter(|s| journal.step(s["id"].as_str().unwrap())["status"] != "succeeded")
            .collect();
        while !pending.is_empty() {
            let mut used = BTreeSet::new();
            let mut batch = vec![];
            let mut rest = vec![];
            let mut exclusive = false;
            for s in pending {
                let locks = resources(s, roots, &steps);
                let terminal = needs_terminal(s);
                if !exclusive
                    && (!terminal || batch.is_empty())
                    && batch.len() < max
                    && used.is_disjoint(&locks)
                {
                    exclusive = terminal;
                    used.extend(locks);
                    batch.push(s);
                } else {
                    rest.push(s);
                }
            }
            pending = rest;
            let results = std::thread::scope(|scope| {
                let handles: Vec<_> = batch
                    .into_iter()
                    .map(|s| {
                        scope.spawn(move || {
                            let result = forward_step(s, plan, bundle, roots, journal);
                            if let Err(e) = &result {
                                journal.edit_step(s["id"].as_str().unwrap(), |r| {
                                    r["status"] = json!("failed");
                                    r["error"] = json!(format!("{e:#}"));
                                })?;
                            }
                            result
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| {
                        h.join()
                            .unwrap_or_else(|_| Err(anyhow::anyhow!("landing worker panicked")))
                    })
                    .collect::<Vec<_>>()
            });
            // Joining the entire batch is mandatory before compensation.
            for result in results {
                result?;
            }
            if super::super::process::cancellation_requested() {
                bail!("landing cancelled");
            }
        }
    }
    Ok(())
}

fn compensation(plan: &Value, bundle: &Value, roots: &Roots, journal: &Journal) -> Result<()> {
    journal.edit(|r| {
        if r.get("recoveryStartedAt").is_none() {
            r["recoveryStartedAt"] = json!(now_iso());
        }
        r["recoveryStatus"] = json!("running");
    })?;
    let (steps, waves) = compile(plan)?;
    let mut manual: Vec<String> = vec![];
    let mut failures: Vec<String> = vec![];
    let mut restored = 0;
    let mut service_unresolved = false;
    for id in waves.iter().rev().flatten() {
        let step = steps.iter().find(|s| s["id"] == *id).unwrap();
        let record = journal.step(id);
        let r = recovery(step);
        if record["recovery"]["status"] == "succeeded" {
            if matches!(effect(step), "deployment" | "external") {
                restored += 1;
            }
            continue;
        }
        if matches!(
            record["attribution"].as_str(),
            None | Some("already_satisfied")
        ) || effect(step) == "read_only"
        {
            continue;
        }
        fn depends_on(
            candidate: &str,
            required: &str,
            steps: &[Value],
            seen: &mut BTreeSet<String>,
        ) -> bool {
            if !seen.insert(candidate.into()) {
                return false;
            }
            steps
                .iter()
                .find(|s| s["id"] == candidate)
                .is_some_and(|s| {
                    strings(&s["needs"])
                        .iter()
                        .any(|n| n == required || depends_on(n, required, steps, seen))
                })
        }
        let blockers: Vec<String> = failures
            .iter()
            .chain(manual.iter())
            .filter(|other| depends_on(other, id, &steps, &mut BTreeSet::new()))
            .cloned()
            .collect();
        if !blockers.is_empty() {
            failures.push(id.clone());
            if matches!(effect(step), "deployment" | "external") {
                service_unresolved = true;
            }
            journal.edit_step(id, |s| {
                s["recovery"] = json!({"status":"blocked","blockedBy":blockers})
            })?;
            continue;
        }
        let attempt = (|| -> Result<()> {
            if record["attribution"] == "uncertain" && record["quiesced"] != true {
                let probe = r.get("probe").context(
                    "crash effect requires an authoritative probe and verified quiescence",
                )?;
                let receipt =
                    run_command(step, probe, roots, journal, "probe", record.get("capture"))?;
                require_success(&receipt)?;
                let observed: Value =
                    serde_json::from_str(receipt["stdout"].as_str().unwrap_or(""))?;
                if observed["quiesced"] != true {
                    bail!("probe has not confirmed quiescence");
                }
                journal.edit_step(id, |s| s["quiesced"] = json!(true))?;
                match observed["status"].as_str() {
                    Some("absent") => {
                        journal.edit_step(id, |s| s["attribution"] = json!("already_satisfied"))?;
                        return Ok(());
                    }
                    Some("applied" | "partial") => (),
                    _ => bail!("probe cannot attribute effect"),
                }
            }
            match r["mode"].as_str() {
                Some("command") => {
                    let outstanding: Vec<Value> = record["attempts"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|a| {
                            matches!(a["phase"].as_str(), Some("restore" | "verify-restored"))
                                && a["status"] == "running"
                        })
                        .cloned()
                        .collect();
                    for prior in &outstanding {
                        let probe = r.get("probe").context("interrupted restore needs its own quiescence reconciliation before retry")?;
                        let mut probe = probe.clone();
                        if !probe["env"].is_object() {
                            probe["env"] = json!({});
                        }
                        probe["env"]["KNIT_LAND_RECONCILE_ATTEMPT_ID"] = prior["id"].clone();
                        let receipt = run_command(
                            step,
                            &probe,
                            roots,
                            journal,
                            "probe-restore",
                            record.get("capture"),
                        )?;
                        require_success(&receipt)?;
                        let observed: Value =
                            serde_json::from_str(receipt["stdout"].as_str().unwrap_or(""))?;
                        if observed["quiesced"] != true
                            || observed["phase"] != prior["phase"]
                            || observed["attemptId"] != prior["id"]
                        {
                            bail!("interrupted restore probe must confirm this reverse attempt is quiescent");
                        }
                        journal.edit_step(id, |s| {
                            let a = s["attempts"]
                                .as_array_mut()
                                .unwrap()
                                .iter_mut()
                                .find(|a| a["id"] == prior["id"])
                                .unwrap();
                            a["status"] = json!("reconciled");
                            a["reconciliation"] = observed;
                        })?;
                    }
                    let capture = record
                        .get("capture")
                        .filter(|v| v.is_object())
                        .context("no durable before-state capture")?;
                    journal.edit_step(id, |s| {
                        s["recovery"] = json!({"status":"running","startedAt":now_iso()})
                    })?;
                    let result = run_command(step, &r, roots, journal, "restore", Some(capture))?;
                    require_success(&result)?;
                    let verify = run_command(
                        step,
                        &r["verify"],
                        roots,
                        journal,
                        "verify-restored",
                        Some(capture),
                    )?;
                    require_success(&verify)?;
                    journal.edit_step(id,|s|s["recovery"]=json!({"status":"succeeded","finishedAt":now_iso(),"verification":verify}))?;
                    if matches!(effect(step), "deployment" | "external") {
                        restored += 1;
                    }
                }
                Some("revert_pr") if record["attribution"] == "performed" => {
                    let typed: crate::model::ChangeGroup = serde_json::from_value(bundle.clone())?;
                    let repo = typed
                        .repos
                        .iter()
                        .find(|r| Some(r.id.as_str()) == step["repoId"].as_str())
                        .context("unknown repo")?;
                    let forge = crate::providers::for_repo(repo)?;
                    let target = provider_target(roots, forge.as_ref(), repo)?;
                    let pub_ = crate::providers::publication_for_repo(&typed, &repo.id)
                        .context("missing publication")?;
                    // A crash while proposing a revert requires explicit reconciliation;
                    // never blindly create a second compensation PR.
                    if matches!(
                        record["recovery"]["status"].as_str(),
                        Some("running" | "uncertain")
                    ) {
                        bail!("source revert proposal needs reconciliation before retry");
                    }
                    journal.edit_step(id, |s| s["recovery"] = json!({"status":"running"}))?;
                    let url=forge.revert_pull_request(&target,&pub_.url,"Revert landing change","Source compensation for a landing run; deployment restoration is recorded separately.")?;
                    journal.edit_step(id, |s| {
                        s["recovery"] =
                            json!({"status":"succeeded","sourceStatus":"revert_proposed","url":url})
                    })?;
                    journal.edit(|r| r["sourceStatus"] = json!("revert_proposed"))?;
                }
                _ => {
                    manual.push(id.clone());
                    if matches!(effect(step), "deployment" | "external") {
                        service_unresolved = true;
                    } else {
                        journal.edit(|r| r["sourceStatus"] = json!("partial"))?;
                    }
                    journal.edit_step(id, |s| {
                        s["recovery"] = json!({"status":"manual","reason":r["reason"]})
                    })?;
                }
            }
            Ok(())
        })();
        if let Err(e) = attempt {
            if matches!(effect(step), "deployment" | "external") {
                service_unresolved = true;
            } else {
                journal.edit(|r| r["sourceStatus"] = json!("partial"))?;
            }
            failures.push(id.clone());
            journal.edit_step(id,|s|{
            let uncertain=(r["mode"]=="revert_pr" && matches!(s["recovery"]["status"].as_str(),Some("running"|"uncertain"))) || s["attempts"].as_array().is_some_and(|a| a.iter().any(|a| matches!(a["phase"].as_str(),Some("restore"|"verify-restored")) && a["status"]=="running"));
            s["recovery"]=json!({"status":if uncertain{"uncertain"}else{"failed"},"error":format!("{e:#}")});
        })?;
        }
    }
    journal.edit(|r| {
        r["recoveryStatus"] = json!(if !failures.is_empty() {
            "failed"
        } else if !manual.is_empty() {
            "manual"
        } else {
            "restored"
        });
        r["serviceStatus"] = json!(if !service_unresolved {
            if restored > 0 {
                "restored"
            } else {
                "unchanged"
            }
        } else if restored > 0 {
            "partially_restored"
        } else {
            "unknown"
        });
    })?;
    if !failures.is_empty() || !manual.is_empty() {
        bail!(
            "recovery incomplete; failed: {}; manual: {}",
            failures.join(", "),
            manual.join(", ")
        );
    }
    Ok(())
}

fn lock_resources(plan: &Value, roots: &Roots) -> Result<Vec<Lock>> {
    // Aliased lanes and target branches can deploy the same service. Until a
    // recipe proves disjoint resources, ownership is conservatively project-wide.
    let mut keys = BTreeSet::from([format!(
        "project:{}",
        plan["sourceProjectId"].as_str().unwrap_or("local")
    )]);
    for root in roots.values() {
        keys.insert(format!("checkout:{}", root.canonicalize()?.display()));
    }
    for s in plan["steps"].as_array().context("steps required")? {
        for l in strings(&s["locks"]) {
            keys.insert(format!("resource:{l}"));
        }
    }
    keys.iter().map(|k| Lock::acquire(k)).collect()
}
fn new_run(plan: &Value, plan_path: &Path, bundle: &Value) -> Value {
    json!({"schemaVersion":"0.2","kind":"KnitLandRun","id":unique_id("land-run"),"planId":plan["id"],"bundleId":plan["bundleId"],"provider":plan["provider"],"planPath":absolute(plan_path).unwrap_or_else(|_|plan_path.into()),"planHash":canonical_hash(plan),"plan":plan,"sourceBundle":bundle,"status":"pending","createdAt":now_iso(),"updatedAt":now_iso(),"serviceStatus":"unchanged","sourceStatus":"unchanged","finalization":{},"steps":plan["steps"].as_array().unwrap().iter().map(|s|json!({"id":s["id"],"type":s["type"],"repoId":s["repoId"],"status":"pending"})).collect::<Vec<_>>()})
}
fn verify_run(run: &Value, plan: &Value) -> Result<()> {
    if run["schemaVersion"] != "0.2"
        || run["kind"] != "KnitLandRun"
        || !run["id"].is_string()
        || run["bundleId"] != plan["bundleId"]
        || run["planId"] != plan["id"]
    {
        bail!("run identity does not match its immutable plan");
    }
    let planned = plan["steps"].as_array().context("plan steps required")?;
    let recorded = run["steps"].as_array().context("run steps required")?;
    let ids: BTreeSet<_> = recorded.iter().filter_map(|s| s["id"].as_str()).collect();
    if ids.len() != recorded.len() || planned.len() != recorded.len() {
        bail!("run steps differ from immutable plan");
    }
    for step in planned {
        let receipt = recorded
            .iter()
            .find(|s| s["id"] == step["id"])
            .context("run steps differ from immutable plan")?;
        if receipt["type"] != step["type"]
            || receipt["repoId"] != step["repoId"]
            || !matches!(
                receipt["status"].as_str(),
                Some("pending" | "running" | "failed" | "succeeded")
            )
        {
            bail!("run step identity/status does not match immutable plan");
        }
        if let Some(attribution) = receipt.get("attribution") {
            if !matches!(
                attribution.as_str(),
                Some("performed" | "already_satisfied" | "uncertain")
            ) {
                bail!("unknown effect attribution in run");
            }
        }
    }

    if run["planHash"] != canonical_hash(plan) || run["planHash"] != canonical_hash(&run["plan"]) {
        bail!(
            "saved plan changed after this run started; resume requires the exact immutable plan"
        );
    }
    Ok(())
}
fn finalize(plan: &Value, bundle: &mut Value, journal: &Journal, out: &Path) -> Result<()> {
    let r = journal.snapshot();
    *bundle = journal.bundle.lock().unwrap().clone();
    let mut typed: crate::model::ChangeGroup = serde_json::from_value(bundle.clone())?;
    let run_id = r["id"].as_str().unwrap();
    if !typed
        .nodes
        .iter()
        .any(|n| n.node_type == "feature.landed" && n.run_id.as_deref() == Some(run_id))
    {
        let repos = r["steps"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| matches!(s["type"].as_str(), Some("merge_pr" | "merge_branch")))
            .filter_map(|s| s["repoId"].as_str().map(str::to_owned))
            .collect();
        typed.nodes.push(crate::model::BundleNode::feature_landed(
            unique_id("land"),
            now_iso(),
            plan["id"].as_str().unwrap().into(),
            run_id.into(),
            plan["provider"].as_str().unwrap_or("github").into(),
            repos,
            vec![],
            Some(crate::model::NodeLanding {
                terminal: plan["terminal"] != false,
                lane: plan["lane"].as_str().map(str::to_owned),
                target_branch: plan["targetBranch"].as_str().map(str::to_owned),
            }),
        ));
    }
    if plan["terminal"] != false && typed.state != Some(crate::model::BundleState::Archived) {
        typed.state = Some(crate::model::BundleState::Archived);
        typed.archived_at = Some(now_iso());
        typed.nodes.push(crate::model::BundleNode::feature_archived(
            unique_id("archive"),
            now_iso(),
            Some("landed".into()),
        ));
    }
    typed.head_node_id = typed.nodes.last().map(|n| n.id.clone());
    typed.updated_at = now_iso();
    *bundle = serde_json::to_value(typed)?;
    *journal.bundle.lock().unwrap() = bundle.clone();
    durable(out, bundle)?;
    journal.edit(|r| {
        r["finalization"]["ledger"] = json!("succeeded");
        r["finalization"]["archive"] = json!("succeeded");
        r["status"] = json!("succeeded");
    })
}

#[allow(clippy::too_many_arguments)]
pub fn apply(
    plan_path: &Path,
    artifact: &Path,
    project_file: Option<&Path>,
    roots_file: Option<&Path>,
    run_out: &Path,
    out: &Path,
    resume: bool,
    json_output: bool,
) -> Result<()> {
    apply_with_checks(
        plan_path,
        artifact,
        project_file,
        roots_file,
        run_out,
        out,
        resume,
        json_output,
        false,
        None,
    )
}
#[allow(clippy::too_many_arguments)]
pub fn apply_with_checks(
    plan_path: &Path,
    artifact: &Path,
    project_file: Option<&Path>,
    roots_file: Option<&Path>,
    run_out: &Path,
    out: &Path,
    resume: bool,
    json_output: bool,
    skip_checks: bool,
    expected_plan_hash: Option<&str>,
) -> Result<()> {
    let plan: Value = read_json(plan_path)?;
    super::verify_expected_hash(&plan, expected_plan_hash)?;
    let bundle: Value = read_json(artifact)?;
    let project = project_file.map(read_json::<Value>).transpose()?;
    if project.is_none()
        && plan["projectFingerprint"] != super::graph::project_fingerprint(&Value::Null)
    {
        bail!("--project-file is required to verify the saved recipe fingerprint before execution");
    }
    let roots = roots_from_file(roots_file)?;
    execute(
        plan_path,
        &plan,
        bundle,
        project.as_ref(),
        roots,
        run_out,
        out,
        resume,
        false,
        json_output,
        None,
        skip_checks,
    )
}
#[allow(clippy::too_many_arguments)]
fn execute(
    plan_path: &Path,
    plan: &Value,
    mut bundle: Value,
    project: Option<&Value>,
    roots: Roots,
    run_out: &Path,
    out: &Path,
    resume: bool,
    recovering: bool,
    json_output: bool,
    local: Option<(
        &mut crate::store::ActiveBundle,
        Option<&super::super::FinishLandOptions<'_>>,
    )>,
    skip_checks: bool,
) -> Result<()> {
    terminal_preflight(plan, local.is_some())?;
    if !resume && run_out.exists() {
        bail!("run output already exists; use --resume or a new path");
    }
    let existing = if resume {
        Some(read_json::<Value>(run_out)?)
    } else {
        None
    };
    if let Some(run) = &existing {
        verify_run(run, plan)?;
        if !recovering && run["recoveryStartedAt"].is_string() {
            bail!("recovery has started; forward resume is permanently blocked");
        }
    }
    if let Some(result) = existing.as_ref().and_then(|r| r.get("resultBundle")) {
        let incoming: crate::model::ChangeGroup = serde_json::from_value(bundle.clone())?;
        let prior: crate::model::ChangeGroup = serde_json::from_value(result.clone())?;
        bundle = serde_json::to_value(crate::model::merge_ledgers(&incoming, &prior, now_iso()))?;
    }
    let source = existing
        .as_ref()
        .map(|r| &r["sourceBundle"])
        .unwrap_or(&bundle);
    let finalization_only = !recovering
        && existing.as_ref().is_some_and(|r| {
            r["steps"]
                .as_array()
                .is_some_and(|steps| steps.iter().all(|s| s["status"] == "succeeded"))
        });
    let validation = validation(
        plan,
        Some(source),
        if finalization_only { None } else { project },
    );
    let mut comparable = bundle.clone();
    if let Some(run) = &existing {
        // get_mut, not index: indexing a bundle without a publications key
        // would insert `null`, and the typed round-trip inside the
        // fingerprint would then fail and silently change the scope.
        if let Some(publications) = comparable
            .get_mut("publications")
            .and_then(Value::as_array_mut)
        {
            for publication in publications {
                if let Some(receipt) = run["steps"].as_array().and_then(|a| {
                    a.iter().find(|s| {
                        s["repoId"] == publication["repoId"]
                            && s["type"] == "merge_pr"
                            && matches!(
                                s["attribution"].as_str(),
                                Some("performed" | "already_satisfied")
                            )
                    })
                }) {
                    if publication["baseBranch"] == receipt["output"]["targetBranch"] {
                        if let Some(original) = source["publications"]
                            .as_array()
                            .and_then(|a| a.iter().find(|p| p["repoId"] == publication["repoId"]))
                        {
                            publication["baseBranch"] = original["baseBranch"].clone();
                        }
                    }
                }
            }
        }
    }
    if super::graph::bundle_fingerprint(&comparable) != super::graph::bundle_fingerprint(source) {
        bail!("bundle changed since this run started");
    }
    if validation["valid"] != true {
        bail!("{}", validation["errors"]);
    }
    if plan["schemaVersion"] != "0.2" {
        bail!("exact-plan artifact execution requires v0.2");
    }
    let (steps, _) = compile(plan)?;
    for step in &steps {
        for repo in strings(&step["sourceRepos"]) {
            if !roots.contains_key(&repo) {
                bail!("sourceRepos requires repo-root binding for {repo}");
            }
        }
    }
    let forward_complete = existing.as_ref().is_some_and(|r| {
        r["steps"]
            .as_array()
            .is_some_and(|a| a.iter().all(|s| s["status"] == "succeeded"))
    });
    let mut pending_expectations: Vec<super::mergeability::MergeCheck> = Vec::new();
    if recovering {
        for step in &steps {
            let r = recovery(step);
            let record = existing
                .as_ref()
                .and_then(|r| r["steps"].as_array())
                .and_then(|a| a.iter().find(|s| s["id"] == step["id"]));
            if record.is_some_and(|s| {
                s["attribution"] != "already_satisfied" && s["recovery"]["status"] != "succeeded"
            }) && r["mode"] == "command"
            {
                for spec in [&r, &r["verify"]] {
                    executable(step, spec, &cwd(step, spec, &roots)?)?;
                }
            }
        }
    } else if !forward_complete {
        // Mergeability preflight runs first, before any remote ref is read for
        // mutation, any lock is claimed, or any effectful command executes:
        // every pending branch merge is simulated against a freshly resolved
        // target tip in an isolated temp checkout. On resume the still-pending
        // merges are revalidated; successful steps keep their receipts, and a
        // durably performed merge (receipt persisted, status not yet marked
        // succeeded) is reconciled by the executor from its recorded revision
        // rather than replayed against the tip its own merge moved.
        let done = |id: &str| {
            existing
                .as_ref()
                .and_then(|r| r["steps"].as_array())
                .and_then(|a| a.iter().find(|s| s["id"] == id))
                .is_some_and(|s| {
                    s["status"] == "succeeded"
                        || (matches!(s["type"].as_str(), Some("merge_pr" | "merge_branch"))
                            && s["attribution"] == "performed"
                            && s["output"]["revision"].is_string())
                })
        };
        let (checks, errors) = super::mergeability::mergeability_checks(
            plan,
            &roots,
            &bundle,
            &done,
            super::mergeability::CheckMode::Apply,
        );
        if !errors.is_empty() {
            bail!(
                "landing mergeability preflight failed:\n{}\n{}",
                errors.join("\n"),
                serde_json::to_string(&super::mergeability::checks_to_json(&checks))?
            );
        }
        pending_expectations = checks;
        preflight(plan, &steps, &roots, &bundle, skip_checks)?;
    }
    let _locks = lock_resources(plan, &roots)?;
    let current = std::env::current_dir()?;
    let plan_directory = absolute(plan_path)?
        .parent()
        .context("plan directory required")?
        .to_path_buf();
    let workspace = crate::store::find_knit_root(&plan_directory)
        .or_else(|| crate::store::find_knit_root(&current))
        .unwrap_or(current);
    let generation = std::env::temp_dir()
        .join("knit-landing-ownership")
        .join(format!(
            "{}.generation.json",
            canonical_hash(&json!(plan["sourceProjectId"].as_str().unwrap_or("local")))
        ));
    let claim = crate::commands::remote::landing::claim_for_run(
        &workspace,
        plan,
        existing.as_ref(),
        recovering,
    )?;
    if claim.is_none() && existing.is_none() && generation.exists() {
        let latest: Value = read_json(&generation)?;
        if latest["quiescent"] == false {
            bail!("previous landing generation has unresolved effects; resume or recover it before starting another run");
        }
    }
    if claim.is_none() {
        if let Some(prior) = &existing {
            if generation.exists() {
                let latest: Value = read_json(&generation)?;
                if latest["runId"] != prior["id"] {
                    bail!("landing run has been superseded; old resume/recovery cannot overwrite a newer environment generation");
                }
            }
        }
    }
    let journal = Journal {
        value: Mutex::new(existing.unwrap_or_else(|| {
            let mut run = new_run(plan, plan_path, &bundle);
            if let Some(project) = project {
                run["sourceProject"] = project.clone();
            }
            run
        })),
        path: absolute(run_out)?,
        bundle: Mutex::new(bundle.clone()),
        bundle_out: absolute(out)?,
        workspace: workspace.clone(),
        pins: Mutex::new(()),
    };
    journal.edit(|r| {
        if skip_checks {
            r["checksSkipped"] = json!(true);
        }
    })?;
    // Pin the tips the mergeability preflight planned against onto the run's
    // step receipts, durably, before the first merge executes. The drift
    // guard re-checks them immediately before each actual merge/push.
    for check in &pending_expectations {
        if let Some(sha) = &check.target_sha {
            let step_id = check.step_id.clone();
            let branch = check.target_branch.clone();
            let pinned = sha.clone();
            journal.edit_step(&step_id, |s| {
                s["expectedTarget"] =
                    json!({"branch": branch, "sha": pinned, "source": check.source_sha});
            })?;
        }
    }
    durable(
        &generation,
        &json!({"runId":journal.snapshot()["id"],"planHash":canonical_hash(plan),"quiescent":false}),
    )?;
    super::super::process::begin_execution()?;
    let result = if recovering {
        compensation(plan, &bundle, &roots, &journal)
    } else {
        journal.edit(|r| r["status"] = json!("running"))?;
        let forward_result = forward(plan, &bundle, &roots, &journal);
        match forward_result {
            Ok(()) => finalize(plan, &mut bundle, &journal, out),
            Err(e) => {
                journal.edit(|r| {
                    r["status"] = json!("failed");
                    r["error"] = json!(format!("{e:#}"));
                    r["serviceStatus"] = json!("unknown");
                })?;
                if plan["onFailure"] == "recover"
                    && !super::super::process::cancellation_requested()
                {
                    let _ = compensation(plan, &bundle, &roots, &journal);
                }
                Err(e)
            }
        }
    };
    let result = result.and_then(|()| {
        if !recovering {
            if let Some((active, options)) = local {
                finish_local(active, plan, &journal, options)?;
            }
        }
        Ok(())
    });
    if let Err(e) = &result {
        journal.edit(|r| r["finalization"]["error"] = json!(format!("{e:#}")))?;
    }
    let result_bundle = journal.bundle.lock().unwrap().clone();
    journal.edit(|r| {
        r["resultBundle"] = result_bundle;
        r["finalization"]["synchronization"] = json!("succeeded");
    })?;
    durable(out, &journal.bundle.lock().unwrap().clone())?;
    let snapshot = journal.snapshot();
    let quiescent = snapshot["steps"].as_array().unwrap().iter().all(|s| {
        (s["attribution"] != "uncertain" || s["quiesced"] == true)
            && !matches!(
                s["recovery"]["status"].as_str(),
                Some("running" | "uncertain")
            )
            && !s["attempts"].as_array().is_some_and(|a| {
                a.iter().any(|a| {
                    matches!(a["phase"].as_str(), Some("restore" | "verify-restored"))
                        && a["status"] == "running"
                })
            })
    });
    durable(
        &generation,
        &json!({"runId":snapshot["id"],"planHash":canonical_hash(plan),"quiescent":quiescent}),
    )?;
    let completion = crate::commands::remote::landing::finish_for_plan(claim, &snapshot, quiescent);
    if let Err(e) = &completion {
        journal.edit(|r| {
            r["finalization"]["synchronization"] = json!("failed");
            r["finalization"]["syncError"] = json!(format!("{e:#}"));
        })?;
    }
    if json_output {
        println!("{}", serde_json::to_string(&journal.snapshot())?);
    } else {
        eprintln!(
            "Landing run {}: {} ({})",
            journal.snapshot()["id"],
            journal.snapshot()["status"],
            run_out.display()
        );
    }
    result.and(completion)
}
pub(crate) fn immutable_plan_path(root: &Path, run: &Value) -> Result<PathBuf> {
    if let Some(stored) = run["planPath"].as_str() {
        let path = PathBuf::from(stored);
        let path = if path.is_absolute() {
            path
        } else {
            root.join(path)
        };
        if path.exists()
            && read_json::<Value>(&path)
                .is_ok_and(|plan| canonical_hash(&plan) == run["planHash"].as_str().unwrap_or(""))
        {
            return Ok(path);
        }
    }
    verify_run(run, &run["plan"])?;
    let hash = canonical_hash(&run["plan"]);
    let bundle = crate::ids::slugify(run["bundleId"].as_str().context("run bundle id required")?);
    let path = root
        .join(".knit/land-plans/revisions")
        .join(bundle)
        .join(format!("{hash}.land.json"));
    if path.exists() {
        if canonical_hash(&read_json::<Value>(&path)?) != hash {
            bail!("immutable plan revision has been modified");
        }
    } else {
        durable(&path, &run["plan"])?;
    }
    Ok(path)
}

#[allow(clippy::too_many_arguments)]
pub fn recover(
    plan_path: Option<&Path>,
    run_path: &Path,
    artifact: Option<&Path>,
    roots_file: Option<&Path>,
    run_out: Option<&Path>,
    out: Option<&Path>,
    apply: bool,
    json_output: bool,
) -> Result<()> {
    let run: Value = read_json(run_path)?;
    let plan = plan_path
        .map(read_json::<Value>)
        .transpose()?
        .unwrap_or_else(|| run["plan"].clone());
    verify_run(&run, &plan)?;
    if !apply {
        println!(
            "{}",
            json!({"planHash":run["planHash"],"recovery":validation(&plan,None,None)["recovery"],"steps":run["steps"]})
        );
        return Ok(());
    }
    if artifact.is_none() && roots_file.is_none() {
        let mut active = crate::store::load_active_bundle_for_update()?;
        let stored = immutable_plan_path(&active.root, &run)?;
        return local_apply(
            &mut active,
            plan_path.unwrap_or(&stored),
            Some(run_path),
            true,
            None,
            false,
            json_output,
            None,
        );
    }
    let out_run = run_out.unwrap_or(run_path);
    if out_run != run_path {
        durable(out_run, &run)?;
    }
    let bundle = artifact
        .map(read_json::<Value>)
        .transpose()?
        .or_else(|| run.get("resultBundle").cloned())
        .unwrap_or_else(|| run["sourceBundle"].clone());
    let default_out = run_path.with_extension("bundle.json");
    let roots = roots_from_file(roots_file)?;
    execute(
        plan_path.unwrap_or(Path::new("embedded-plan")),
        &plan,
        bundle,
        None,
        roots,
        out_run,
        out.unwrap_or(&default_out),
        true,
        true,
        json_output,
        None,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn local_apply(
    active: &mut crate::store::ActiveBundle,
    plan_path: &Path,
    run_path: Option<&Path>,
    recovering: bool,
    options: Option<&super::super::FinishLandOptions<'_>>,
    skip_checks: bool,
    json_output: bool,
    expected_plan_hash: Option<&str>,
) -> Result<()> {
    let plan: Value = read_json(plan_path)?;
    super::verify_expected_hash(&plan, expected_plan_hash)?;
    let project = super::generate::local_project(active)?;
    let mut roots: Roots = active
        .bundle
        .repos
        .iter()
        .filter_map(|r| crate::checkout::checkout_dir(active, r).map(|p| (r, p)))
        .map(|(r, p)| Ok((r.id.clone(), dunce::canonicalize(p)?)))
        .collect::<Result<_>>()?;
    if let Some(repos) = project["repos"].as_array() {
        for r in repos {
            if let (Some(id), Some(path)) = (r["id"].as_str(), r["path"].as_str()) {
                if !roots.contains_key(id) {
                    let path = PathBuf::from(path);
                    let path = if path.is_absolute() {
                        path
                    } else {
                        active.root.join(path)
                    };
                    if path.is_dir() {
                        roots.insert(id.into(), dunce::canonicalize(path)?);
                    }
                }
            }
        }
    }
    let finalization_only = run_path
        .filter(|p| p.exists())
        .map(read_json::<Value>)
        .transpose()?
        .is_some_and(|r| {
            r["steps"]
                .as_array()
                .is_some_and(|steps| steps.iter().all(|s| s["status"] == "succeeded"))
        });
    if !recovering && !finalization_only {
        let unrecorded = crate::tracking::detect_unrecorded_changes(active)?;
        if !unrecorded.is_empty() {
            bail!("worktree heads changed; run knit sync and regenerate the plan");
        }
        // Required checks are evaluated against the same effective heads the
        // executor uses: with an integration source, freshness speaks for the
        // pinned source, not the reviewed feature head.
        if super::mergeability::has_integration_sources(&plan) {
            let effective = super::mergeability::effective_required_checks_bundle(
                &plan,
                &serde_json::to_value(&active.bundle)?,
            );
            let check = crate::store::ActiveBundle::unlocked(
                active.root.clone(),
                active.bundle_path.clone(),
                serde_json::from_value(effective)?,
            );
            super::super::validate::preflight_required_checks(
                &check,
                &strings(&plan["requireChecks"]),
                skip_checks,
            )?;
        } else {
            super::super::validate::preflight_required_checks(
                active,
                &strings(&plan["requireChecks"]),
                skip_checks,
            )?;
        }
    }
    if options.is_some_and(|o| o.tag.is_some()) && plan["terminal"] == false {
        bail!("tag requires a terminal destination");
    }
    let run_path = run_path.map(PathBuf::from).unwrap_or_else(|| {
        active.root.join(".knit/land-runs").join(format!(
            "land-{}-{}.run.json",
            active.bundle.id,
            unique_id("run")
        ))
    });
    let resume = run_path.exists();
    let bundle = serde_json::to_value(&active.bundle)?;
    let output = active.bundle_path.clone();
    execute(
        plan_path,
        &plan,
        bundle,
        if recovering { None } else { Some(&project) },
        roots,
        &run_path,
        &output,
        resume,
        recovering,
        json_output,
        Some((active, options)),
        skip_checks,
    )
}

fn finish_local(
    active: &mut crate::store::ActiveBundle,
    plan: &Value,
    journal: &Journal,
    options: Option<&super::super::FinishLandOptions<'_>>,
) -> Result<()> {
    active.bundle = read_json(&active.bundle_path)?;
    let run = journal.snapshot();
    if run["finalization"]["cleanup"] != "succeeded" {
        journal.edit(|r| r["finalization"]["cleanup"] = json!("pending"))?;
        if plan["terminal"] != false && !options.is_some_and(|o| o.keep_worktrees) {
            crate::commands::clean::clean_worktrees_for_bundle(active, false)?;
            crate::store::save_active_bundle(active)?;
            *journal.bundle.lock().unwrap() = serde_json::to_value(&active.bundle)?;
        }
        if plan["terminal"] != false {
            crate::commands::bundle::clear_workspace_active_if_matches(
                &active.root,
                &active.bundle.id,
            )?;
        }
        journal.edit(|r| r["finalization"]["cleanup"] = json!("succeeded"))?;
    }
    if let Some(options) = options {
        if run["finalization"]["bundleSynchronization"] != "succeeded" {
            crate::commands::remote::sync_active_bundle_to_remote_if_enabled(
                active,
                options.remote,
                options.no_remote,
            )?;
            *journal.bundle.lock().unwrap() = serde_json::to_value(&active.bundle)?;
            journal.edit(|r| r["finalization"]["bundleSynchronization"] = json!("succeeded"))?;
        }
        if run["finalization"]["tag"] != "succeeded" {
            super::super::tag_landed_bundle(
                active,
                options.tag.clone(),
                options.no_tag,
                options.remote,
                options.no_remote,
            );
            journal.edit(|r| r["finalization"]["tag"] = json!("succeeded"))?;
        }
    }
    journal.edit(|r| r["finalized"] = json!(true))
}
