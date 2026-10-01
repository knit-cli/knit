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
            if s["type"] == "await_update"
                && s["status"] == "succeeded"
                && s["output"]["observation"].is_object()
            {
                let node_id = format!("gate-{}", canonical_hash(&json!([run_id, s["id"]])));
                if !typed.nodes.iter().any(|node| node.id == node_id) {
                    let change: crate::model::RepoChange =
                        serde_json::from_value(s["output"]["observation"].clone())?;
                    typed.nodes.push(crate::model::BundleNode::git_observed(
                        node_id,
                        now_iso(),
                        vec![change],
                    ));
                    if let Some(repo) = typed
                        .repos
                        .iter_mut()
                        .find(|repo| Some(repo.id.as_str()) == s["repoId"].as_str())
                    {
                        repo.head_sha = s["output"]["revision"].as_str().map(str::to_owned);
                    }
                }
            }
            if s["type"] == "merge_pr" {
                if let Some(base) = s["reviewBefore"]["targetBranch"].as_str() {
                    for publication in &mut typed.publications {
                        if Some(publication.repo_id.as_str()) == s["repoId"].as_str() {
                            publication.base_branch = base.to_owned();
                        }
                    }
                }
            }
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
        *bundle = crate::model::preserve_bundle_extensions(&bundle, serde_json::to_value(typed)?)?;
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
    existing: Option<&Value>,
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
            // Source-only runners may not yet have the fork's feature object.
            if git(root, &["cat-file", "-e", &format!("{pin}^{{commit}}")]).is_err() {
                let remote = super::mergeability::bundle_remote(bundle, repo, None, true);
                git(root, &["fetch", "--no-tags", remote, pin])?;
            }
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
                super::branch_checkout::CAPABILITY,
                super::gates::CAPABILITY,
            ]
            .contains(&cap.as_str())
            {
                bail!("runner lacks capability {cap}");
            }
        }
    }
    super::branch_checkout::preflight(steps, roots, bundle, plan)?;
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
            let mut target = provider_target(
                roots,
                forge.as_ref(),
                repo,
                crate::providers::publication_for_repo(&typed, &repo.id)
                    .map(|p| p.base_branch.as_str())
                    .unwrap_or(&repo.base_branch),
            )?;
            gate_merge_target(plan, step, repo, forge.as_ref(), &mut target)?;
            restore_gated_target_base(plan, existing, id, &mut target);
            if super::gates::gated_repos(plan).contains(id) {
                // Before acceptance the gate owns SHA validation. Contribution
                // adapters still validate repository, branch and base identity.
                let accepted = existing.and_then(|run| accepted_gate_pin(run, id));
                target.verify_head = accepted.is_some();
                if let (Some(identity), Some(pin)) = (target.contribution.as_mut(), accepted) {
                    identity.sha = pin;
                }
            }
            if existing.is_some_and(|run| {
                run["steps"].as_array().into_iter().flatten().any(|s| {
                    s["type"] == "await_update" && s["repoId"] == id && s["status"] == "succeeded"
                })
            }) {
                let root = roots.get(id).context("update gate needs a checkout")?;
                let local = update_git(root, &["rev-parse", "HEAD"])?;
                let pin = plan["bundleHeads"][id]
                    .as_str()
                    .context("accepted update pin required")?;
                update_changes(root, local.trim(), pin)
                    .context("local checkout changed after the update gate accepted its head")?;
            }
            let pub_ = crate::providers::publication_for_repo(&typed, id)
                .context("missing publication")?;
            let pr = forge.view(&target, &pub_.url)?;
            gated_review_identity(plan, repo, pub_, &pr, existing)?;
            if !super::super::state_is_merged(&pr) {
                super::super::ensure_open_and_ready(id, &pr)?;
            }
            // A gated repository's head legitimately moves when its update
            // lands; the gate verifies it and the merge is pinned to it.
            if let Some(pin) = plan["bundleHeads"][id].as_str().filter(|_| {
                !super::gates::gated_repos(plan).contains(id)
                    || existing.is_some_and(|run| {
                        run["steps"].as_array().is_some_and(|steps| {
                            steps.iter().any(|s| {
                                s["type"] == "await_update"
                                    && s["repoId"] == id
                                    && s["status"] == "succeeded"
                            })
                        })
                    })
            }) {
                if pr.head_ref_oid.as_deref().is_some_and(|head| head != pin) {
                    bail!("{id}: live review head differs from reviewed plan");
                }
            }
        }
    }
    Ok(())
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    crate::git::git_output(root, args).map(|output| output.trim().into())
}

/// Isolated commands use immutable revisions. Explicit branch commands use
/// the registered checkout and record its observed HEAD without pinning it.
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
                // Sequence ordering alone does not make another repository
                // an input to a command running in its own branch checkout.
                if super::branch_checkout::enabled(step)
                    && producer["repoId"] != step["repoId"]
                    && !strings(&step["sourceRepos"])
                        .iter()
                        .any(|r| producer["repoId"] == *r)
                {
                    continue;
                }
                let prior = journal.step(&ancestor);
                let revision = prior["output"]["revision"].as_str().context(
                    "required merge did not produce a pinned revision; supply repo roots",
                )?;
                revisions.insert(producer["repoId"].as_str().unwrap().into(), json!(revision));
            }
        }
        if let Some(branch) = step["checkout"]["branch"].as_str() {
            let repo = step["repoId"]
                .as_str()
                .context("checkout repoId required")?;
            if !revisions.contains_key(repo) {
                let root = roots.get(repo).context("checkout binding missing")?;
                let routed = super::branch_checkout::routed_step(
                    step,
                    &snapshot["plan"],
                    &snapshot["sourceBundle"],
                );
                let remote = routed["checkout"]["remote"].as_str().unwrap_or("origin");
                git(root, &["fetch", "--no-tags", remote, branch])?;
                revisions.insert(repo.into(), json!(git(root, &["rev-parse", "FETCH_HEAD"])?));
            }
        }
        if let Some(repo) = step["repoId"].as_str() {
            if !revisions.contains_key(repo) {
                // A new command follows an accepted gate rather than another
                // step's older pin. Its own receipt and authored/merge sources
                // have already taken precedence above.
                let accepted = accepted_gate_pin(&snapshot, repo);
                let previous = snapshot["steps"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find_map(|s| s["sourceRevisions"][repo].as_str());
                let fallback = effective_pin(&snapshot["plan"], journal, repo);
                if let Some(rev) = accepted.as_deref().or(previous).or(fallback.as_deref()) {
                    revisions.insert(repo.into(), json!(rev));
                }
            }
        }
        if let Some(repo) = step["repoId"].as_str() {
            if !revisions.contains_key(repo) {
                if let Some(branch) = snapshot["plan"]["recipeBases"][repo].as_str() {
                    let root = roots.get(repo).context("consumer binding required")?;
                    git(
                        root,
                        &[
                            "fetch",
                            "--no-tags",
                            super::mergeability::recorded_remote(
                                &snapshot["plan"]["repositoryIdentities"][&repo],
                                Some(branch),
                                false,
                            ),
                            branch,
                        ],
                    )?;
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
            let accepted = effective_pin(&snapshot["plan"], journal, &repo);
            let gated = accepted_gate_pin(&snapshot, &repo);
            let revision = if let Some(rev) = gated.as_deref().or(previous).or(accepted.as_deref())
            {
                rev.to_owned()
            } else {
                let branch = snapshot["plan"]["recipeBases"][&repo]
                    .as_str()
                    .context("sourceRepos requires an immutable source or project base")?;
                git(
                    root,
                    &[
                        "fetch",
                        "--no-tags",
                        super::mergeability::recorded_remote(
                            &snapshot["plan"]["repositoryIdentities"][&repo],
                            Some(branch),
                            false,
                        ),
                        branch,
                    ],
                )?;
                git(root, &["rev-parse", "FETCH_HEAD"])?
            };
            revisions.insert(repo, json!(revision));
        }
        journal.edit_step(id, |s| s["sourceRevisions"] = json!(revisions))?;
    }
    let branch_repo = if super::branch_checkout::enabled(step) {
        let repo = step["repoId"]
            .as_str()
            .context("branch checkout repo required")?;
        let root = roots.get(repo).context("branch checkout binding missing")?;
        let revision = super::branch_checkout::command_revision(
            root,
            &super::branch_checkout::routed_step(
                step,
                &snapshot["plan"],
                &snapshot["sourceBundle"],
            ),
            phase,
            record["sourceRevisions"][repo].as_str(),
        )?;
        revisions.insert(repo.into(), json!(revision));
        journal.edit_step(id, |s| s["sourceRevisions"] = json!(revisions))?;
        Some(repo)
    } else {
        None
    };
    let mut bound = roots.clone();
    for (repo, rev) in revisions {
        if branch_repo == Some(repo.as_str()) {
            continue;
        }
        let root = roots
            .get(&repo)
            .context("pinned checkout binding missing")?;
        let rev = rev.as_str().context("revision must be string")?;
        let checkout = journal
            .path
            .with_extension("checkouts")
            .join(canonical_hash(&json!([repo, rev])));
        if !checkout.exists() {
            if git(root, &["cat-file", "-e", &format!("{rev}^{{commit}}")]).is_err() {
                let identity = super::mergeability::repo_identity(&snapshot["sourceBundle"], &repo);
                let identity = if identity.is_null() {
                    &snapshot["plan"]["repositoryIdentities"][&repo]
                } else {
                    identity
                };
                let feature_checkout = step["repoId"] == repo
                    && step["checkout"]["branch"].is_string()
                    && step["checkout"]["branch"] == identity["featureBranch"];
                let source = snapshot["plan"]["bundleHeads"][&repo].as_str() == Some(rev)
                    || feature_checkout;
                let remote = super::mergeability::recorded_remote(identity, None, source);
                git(root, &["fetch", "--no-tags", remote, rev])?;
            }
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
    let (result, records) = super::super::git_progress::scope(&format!("{id}/{phase}"), || {
        run_command_inner(step, spec, roots, journal, phase, capture)
    });
    record_git_commands(journal, id, records)?;
    result
}

fn record_git_commands(journal: &Journal, id: &str, records: Vec<Value>) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    journal.edit_step(id, |s| {
        let attempts = s
            .as_object_mut()
            .unwrap()
            .entry("attempts")
            .or_insert(json!([]));
        attempts.as_array_mut().unwrap().extend(records);
    })
}

fn run_command_inner(
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
    if matches!(
        snapshot["plan"]["requiredExecutorVersion"].as_str(),
        Some("0.4" | "0.5" | "0.6")
    ) {
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
    // Identify arbitrary recipes without echoing inline scripts, secret argv,
    // or the environment. The full authored command remains in the plan.
    eprintln!("[{id}/{phase}] $ {}", cmd.get_program().to_string_lossy());
    let result = if phase == "forward" && step["interactive"] == true {
        super::super::process::run_attached(&mut cmd, timeout)
    } else if phase == "capture" {
        super::super::process::run_captured(&mut cmd, timeout)
    } else {
        super::super::process::run_streamed_managed(&mut cmd, timeout)
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
    base: &str,
) -> Result<crate::providers::PrTarget> {
    if let Some(root) = roots.get(&repo.id) {
        crate::contribution::target(root, repo, forge, base, false)
    } else {
        crate::contribution::target(&std::env::current_dir()?, repo, forge, base, true)
    }
}

/// A gate may change only the reviewed SHA, never the review's identity.
fn gated_review_identity(
    plan: &Value,
    repo: &crate::model::RepoEntry,
    publication: &crate::model::PublicationEntry,
    pr: &crate::providers::PullRequest,
    run: Option<&Value>,
) -> Result<()> {
    if !super::gates::gated_repos(plan).contains(&repo.id) {
        return Ok(());
    }
    let check = || -> Result<()> {
        let id = &repo.id;
        if pr.number != publication.number
            || pr.url != publication.url
            || pr.head_ref_name.as_deref() != Some(publication.head_branch.as_str())
        {
            bail!("{id}: gated review identity or source branch changed");
        }
        let desired = plan["steps"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|s| s["type"] == "merge_pr" && s["repoId"] == *id)
            .and_then(|s| s["targetBranch"].as_str())
            .or(plan["targetBranches"][id].as_str())
            .or(plan["targetBranch"].as_str())
            .unwrap_or(&publication.base_branch);
        let retargeted = run
            .and_then(|r| r["steps"].as_array())
            .and_then(|steps| {
                steps
                    .iter()
                    .find(|s| s["type"] == "merge_pr" && s["repoId"] == *id)
            })
            .and_then(|s| s["reviewBefore"]["targetBranch"].as_str());
        let base = pr
            .base_ref_name
            .as_deref()
            .context("gated review has no base identity")?;
        if retargeted.is_some_and(|b| base != b)
            || (retargeted.is_none() && base != publication.base_branch && base != desired)
        {
            bail!("{id}: gated review base changed outside the authorized landing destination");
        }
        let source = pr
            .source_repository
            .as_deref()
            .filter(|s| !s.is_empty())
            .context("gated review has no source repository identity")?;
        if publication.provider == "github" {
            if let Some((_, name)) = crate::contribution::source(repo)
                .and_then(|remote| crate::auth::remote_target(remote).ok())
            {
                if !source.eq_ignore_ascii_case(&name) {
                    bail!(
                        "{id}: gated review source repository differs from the recorded repository"
                    );
                }
            }
        }
        if let Some(receipt) = run.and_then(|r| r["steps"].as_array()).and_then(|steps| {
            steps.iter().find(|s| {
                s["type"] == "await_update" && s["repoId"] == *id && s["status"] == "succeeded"
            })
        }) {
            if receipt["output"]["sourceRepository"].as_str() != Some(source) {
                bail!("{id}: gated review source repository changed after acceptance");
            }
        }
        Ok(())
    };
    check().map_err(|e| super::mergeability::KnownNoEffect(format!("{e:#}")).into())
}

// Contribution adapters validate the base while reading the review. On resume,
// use the confirmed retarget receipt, never an unverified live destination.
fn restore_gated_target_base(
    plan: &Value,
    run: Option<&Value>,
    repo: &str,
    target: &mut crate::providers::PrTarget,
) {
    if !super::gates::gated_repos(plan).contains(repo) {
        return;
    }
    if let (Some(identity), Some(base)) = (
        target.contribution.as_mut(),
        run.and_then(|r| r["steps"].as_array())
            .and_then(|steps| {
                steps
                    .iter()
                    .find(|s| s["type"] == "merge_pr" && s["repoId"] == repo)
            })
            .and_then(|s| s["reviewBefore"]["targetBranch"].as_str()),
    ) {
        identity.base = base.to_owned();
    }
}

/// Require adapters that send the accepted head in the host's merge operation.
fn gate_merge_target(
    plan: &Value,
    step: &Value,
    repo: &crate::model::RepoEntry,
    forge: &dyn crate::providers::Forge,
    target: &mut crate::providers::PrTarget,
) -> Result<()> {
    if step["type"] != "merge_pr" || !super::gates::gated_repos(plan).contains(&repo.id) {
        return Ok(());
    }
    if forge.id() == "github" {
        return Ok(());
    }
    if forge.id() == "gitlab" && step["method"] != "rebase" {
        if target.repo_full_name.is_none() {
            target.repo_full_name = crate::contribution::destination(repo)
                .and_then(|remote| forge.repo_full_name(remote));
        }
        if target.repo_full_name.is_some() {
            return Ok(());
        }
    }
    bail!("{}: await_update requires atomic head-conditional merge support; supported routes are GitHub and GitLab API merge/squash, not this {} route", repo.id, forge.id())
}

fn provider_step(
    step: &Value,
    plan: &Value,
    bundle: &Value,
    roots: &Roots,
    journal: &Journal,
) -> Result<Value> {
    let typed: crate::model::ChangeGroup =
        serde_json::from_value(journal.bundle.lock().unwrap().clone())?;
    let id = step["repoId"].as_str().context("repoId required")?;
    let repo = typed
        .repos
        .iter()
        .find(|r| r.id == id)
        .context("unknown repo")?;
    let forge = crate::providers::for_repo(repo)?;
    let mut target = provider_target(
        roots,
        forge.as_ref(),
        repo,
        crate::providers::publication_for_repo(&typed, &repo.id)
            .map(|p| p.base_branch.as_str())
            .unwrap_or(&repo.base_branch),
    )?;
    gate_merge_target(plan, step, repo, forge.as_ref(), &mut target)?;
    restore_gated_target_base(plan, Some(&journal.snapshot()), id, &mut target);
    if super::gates::gated_repos(plan).contains(id) {
        if let (Some(identity), Some(pin)) = (
            target.contribution.as_mut(),
            effective_pin(plan, journal, id),
        ) {
            identity.sha = pin;
        }
    }
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
            let before = crate::git::remote_ref_sha(
                root,
                crate::commands::merge::destination_remote(repo),
                &format!("refs/heads/{branch}"),
            )
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
                        root, id, branch, &head, reviewed, bundle,
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
            let outcome = if super::branch_checkout::enabled(step) {
                super::branch_checkout::merge(
                    root,
                    &super::branch_checkout::routed_step(step, plan, bundle),
                    &head,
                    source_branch.as_deref(),
                    expected_sha,
                )?
            } else {
                crate::commands::merge::merge_branch_into_target(
                    workspace,
                    &bound,
                    &head,
                    source_branch.as_deref(),
                    branch,
                    true,
                    expected_sha,
                )?
            };
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
    gated_review_identity(plan, repo, pub_, &pr, Some(&journal.snapshot()))?;
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
        if super::gates::gated_repos(plan).contains(id)
            && effective_pin(plan, journal, id)
                .as_deref()
                .is_some_and(|pin| pr.head_ref_oid.as_deref() != Some(pin))
        {
            bail!("{id}: merged review head differs from accepted landing head");
        }
        if pr.base_ref_name.as_deref().unwrap_or(&pub_.base_branch) != desired {
            bail!("review merged into another destination");
        }
        return Ok(
            json!({"attribution":"already_satisfied","publicationUrl":pub_.url,"source":pr.head_ref_oid,"targetBranch":desired}),
        );
    }
    if pr.base_ref_name.as_deref().unwrap_or(&pub_.base_branch) != desired {
        forge.edit_base(&target, &pub_.url, desired)?;
        if let Some(identity) = &mut target.contribution {
            identity.base = desired.to_owned();
        }
        pr = forge.view(&target, &pub_.url)?;
        gated_review_identity(plan, repo, pub_, &pr, Some(&journal.snapshot()))?;
        if pr.base_ref_name.as_deref() != Some(desired) {
            bail!("review retarget not confirmed");
        }
        // Keep the confirmed base even if readiness now reports a conflict;
        // land update must fetch the base the review actually targets.
        journal.edit_step(sid, |s| s["reviewBefore"] = json!({"publicationUrl":pub_.url,"state":pr.state,"source":pr.head_ref_oid,"targetBranch":desired}))?;
    }
    let gated = super::gates::gated_repos(plan).contains(id);
    let no_merge_effect = |error: anyhow::Error| -> anyhow::Error {
        if gated {
            super::mergeability::KnownNoEffect(format!("{error:#}")).into()
        } else {
            error
        }
    };
    super::super::ensure_open_and_ready(id, &pr).map_err(no_merge_effect)?;
    let effective = effective_pin(plan, journal, id);
    if super::gates::gated_repos(plan).contains(id)
        && effective
            .as_deref()
            .is_some_and(|pin| pr.head_ref_oid.as_deref() != Some(pin))
    {
        return Err(super::mergeability::KnownNoEffect(format!(
            "{id}: live review head differs from accepted landing head"
        ))
        .into());
    }
    if step["waitForChecks"].as_bool().unwrap_or(true) {
        forge
            .wait_for_checks(
                &target,
                &pub_.url,
                step["requiredChecksOnly"].as_bool().unwrap_or(true),
                step["timeoutSeconds"].as_u64().unwrap_or(1800),
                step["intervalSeconds"].as_u64().unwrap_or(10),
            )
            .map_err(no_merge_effect)?;
    }
    if super::gates::gated_repos(plan).contains(id) {
        let mut checked = plan.clone();
        let current = journal.bundle.lock().unwrap().clone();
        let run = journal.snapshot();
        for repo in super::gates::gated_repos(plan) {
            let accepted = run["steps"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|s| {
                    s["type"] == "await_update" && s["repoId"] == repo && s["status"] == "succeeded"
                })
                .and_then(|s| s["output"]["revision"].as_str());
            let recorded = current["repos"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|r| r["id"] == repo)
                .and_then(|r| r["headSha"].as_str());
            if let Some(pin) = accepted.or(recorded) {
                checked["bundleHeads"][&repo] = json!(pin);
            }
        }
        let check_bundle =
            super::mergeability::effective_required_checks_bundle(&checked, &current);
        let active = crate::store::ActiveBundle::unlocked(
            journal.workspace.clone(),
            PathBuf::new(),
            serde_json::from_value(check_bundle)?,
        );
        super::super::validate::preflight_required_checks(
            &active,
            &strings(&plan["requireChecks"]),
            journal.snapshot()["checksSkipped"] == true,
        )
        .map_err(no_merge_effect)?;
    }
    if gated {
        let refreshed = forge.view(&target, &pub_.url).map_err(no_merge_effect)?;
        gated_review_identity(plan, repo, pub_, &refreshed, Some(&journal.snapshot()))?;
        super::super::ensure_open_and_ready(id, &refreshed).map_err(no_merge_effect)?;
        if refreshed.head_ref_oid.as_deref() != effective.as_deref() {
            return Err(super::mergeability::KnownNoEffect(format!(
                "{id}: live review head changed after checks"
            ))
            .into());
        }
    }
    let pin = effective.as_deref().or(pr.head_ref_oid.as_deref());
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

/// The revision a repository's review merge must land: the head a satisfied
/// `await_update` gate accepted, otherwise the reviewed head.
fn effective_pin(plan: &Value, journal: &Journal, repo: &str) -> Option<String> {
    accepted_gate_pin(&journal.snapshot(), repo)
        .or_else(|| plan["bundleHeads"][repo].as_str().map(str::to_owned))
}

fn accepted_gate_pin(run: &Value, repo: &str) -> Option<String> {
    run["steps"]
        .as_array()
        .and_then(|steps| {
            steps.iter().find(|s| {
                s["type"] == "await_update" && s["repoId"] == repo && s["status"] == "succeeded"
            })
        })
        .and_then(|s| s["output"]["revision"].as_str().map(str::to_owned))
}

/// Evaluate a landing gate. A satisfied gate succeeds with a receipt; an
/// unsatisfied one pauses the run and leaves the step pending.
fn gate(
    step: &Value,
    plan: &Value,
    bundle: &Value,
    roots: &Roots,
    journal: &Journal,
) -> Result<()> {
    let id = step["id"].as_str().unwrap();
    journal.edit_step(id, |s| {
        s["status"] = json!("running");
        s["startedAt"] = json!(now_iso());
    })?;
    match gate_outcome(step, plan, bundle, roots, journal) {
        Ok(output) => journal.edit_step(id, |s| {
            s["status"] = json!("succeeded");
            s["attribution"] = json!("already_satisfied");
            s["quiesced"] = json!(true);
            s["output"] = output;
            s["finishedAt"] = json!(now_iso());
            if let Some(record) = s.as_object_mut() {
                record.remove("waiting");
                record.remove("error");
            }
        }),
        Err(e) => {
            let paused = e.downcast_ref::<super::gates::LandingPaused>().is_some();
            journal.edit_step(id, |s| {
                if paused {
                    s["status"] = json!("pending");
                    s.as_object_mut().unwrap().remove("error");
                    s.as_object_mut().unwrap().remove("finishedAt");
                    s["waiting"] = json!({"since": now_iso(), "reason": format!("{e}")});
                } else {
                    s["status"] = json!("failed");
                    s["error"] = json!(format!("{e:#}"));
                    s["finishedAt"] = json!(now_iso());
                }
            })?;
            Err(e)
        }
    }
}

fn gate_outcome(
    step: &Value,
    plan: &Value,
    bundle: &Value,
    roots: &Roots,
    journal: &Journal,
) -> Result<Value> {
    let id = step["id"].as_str().unwrap();
    let instructions = step["instructions"].as_str().unwrap_or_default().to_owned();
    let acknowledgement = journal.snapshot()["acknowledgements"][id].clone();
    let acknowledged = acknowledgement.is_object();
    if step["type"] == "manual" {
        if acknowledged {
            return Ok(
                json!({"acknowledged":true,"notes":acknowledgement["notes"],"acknowledgedAt":acknowledgement["at"]}),
            );
        }
        return Err(super::gates::LandingPaused {
            step: id.to_owned(),
            instructions,
            waiting_for: "a person to confirm it".to_owned(),
            next: format!("When that is done, run `knit land resume --acknowledge {id}`."),
        }
        .into());
    }
    let repo_id = step["repoId"]
        .as_str()
        .context("await_update requires repoId")?;
    let typed: crate::model::ChangeGroup =
        serde_json::from_value(journal.bundle.lock().unwrap().clone())?;
    let repo = typed
        .repos
        .iter()
        .find(|r| r.id == repo_id)
        .with_context(|| format!("{repo_id}: not tracked by this bundle"))?;
    let publication =
        crate::providers::publication_for_repo(&typed, repo_id).with_context(|| {
            format!("{repo_id}: {id} watches the repository's review, but none is recorded")
        })?;
    let forge = crate::providers::for_repo(repo)?;
    let mut target = provider_target(roots, forge.as_ref(), repo, &publication.base_branch)?;
    // Read the proposed SHA, then validate ancestry and paths below. Keep the
    // contribution adapter's source repository, branch and base checks enabled.
    target.verify_head = false;
    let pr = forge.view(&target, &publication.url)?;
    gated_review_identity(plan, repo, publication, &pr, Some(&journal.snapshot()))?;
    if super::super::state_is_merged(&pr) {
        bail!("{repo_id}: the review merged before {id} accepted an update; reconcile the landing by hand");
    }
    super::super::ensure_open_and_ready(repo_id, &pr)?;
    if pr.head_ref_name.as_deref() != Some(publication.head_branch.as_str()) {
        bail!("{repo_id}: review source branch differs from the recorded publication");
    }
    let planned_base = plan["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|s| s["type"] == "merge_pr" && s["repoId"] == repo_id)
        .and_then(|s| s["targetBranch"].as_str())
        .or(plan["targetBranches"][repo_id].as_str())
        .or(plan["targetBranch"].as_str())
        .unwrap_or(&publication.base_branch);
    if !pr
        .base_ref_name
        .as_deref()
        .is_some_and(|base| base == publication.base_branch || base == planned_base)
    {
        bail!("{repo_id}: review base differs from the recorded or planned destination");
    }
    let live = pr
        .head_ref_oid
        .clone()
        .with_context(|| format!("{repo_id}: the provider did not report the review head"))?;
    let reviewed = plan["bundleHeads"][repo_id]
        .as_str()
        .with_context(|| format!("{repo_id}: the plan has no reviewed head"))?
        .to_owned();
    if !super::mergeability::is_commit_sha(&live) || !super::mergeability::is_commit_sha(&reviewed)
    {
        bail!("{repo_id}: exact review commit SHAs are required");
    }
    if live == reviewed {
        if crate::tracking::latest_recorded_head_sha(&typed, repo)
            .is_some_and(|head| head != reviewed)
        {
            bail!("{repo_id}: recorded bundle head differs from the unchanged review; push and verify the recorded update first");
        }
        if acknowledged {
            let root = roots.get(repo_id).context("update gate needs a checkout")?;
            let local = update_git(root, &["rev-parse", "HEAD"])?;
            update_changes(root, local.trim(), &live)
                .context("local checkout contains work outside the accepted review head")?;
            return Ok(
                json!({"sourceRepository":pr.source_repository,"previous":reviewed,"revision":reviewed,"commits":[],"files":[],"acknowledged":true,"notes":acknowledgement["notes"]}),
            );
        }
        let branch = repo
            .feature_branch
            .as_deref()
            .unwrap_or("the feature branch");
        return Err(super::gates::LandingPaused {
            step: id.to_owned(),
            instructions,
            waiting_for: format!(
                "a new commit on {repo_id}'s review (its head is still the reviewed {})",
                crate::ids::short_sha(&reviewed)
            ),
            next: format!(
                "Push the update to {branch} (for example `knit commit` then `knit push` in the {repo_id} checkout), then run `knit land resume`. If the reviewed head already contains it, run `knit land resume --acknowledge {id}`."
            ),
        }
        .into());
    }
    let root = roots
        .get(repo_id)
        .with_context(|| format!("{repo_id}: {id} needs a checkout to verify the update"))?;
    if git(root, &["cat-file", "-e", &format!("{live}^{{commit}}")]).is_err() {
        let remote = super::mergeability::bundle_remote(bundle, repo_id, None, true);
        if git(root, &["fetch", "--no-tags", remote, &live]).is_err() {
            let branch = repo
                .feature_branch
                .as_deref()
                .context("feature branch required to fetch the update")?;
            git(
                root,
                &[
                    "fetch",
                    "--no-tags",
                    remote,
                    &format!("refs/heads/{branch}"),
                ],
            )?;
        }
    }
    let local = update_git(root, &["rev-parse", "HEAD"])?;
    update_changes(root, local.trim(), &live)
        .context("local checkout contains work outside the accepted review head")?;
    let (commits, files) = update_changes(root, &reviewed, &live)?;
    if let Some(patterns) = step["paths"].as_array() {
        let patterns: Vec<&str> = patterns.iter().filter_map(Value::as_str).collect();
        let outside: Vec<&String> = files
            .iter()
            .filter(|f| !patterns.iter().any(|p| super::gates::path_matches(p, f)))
            .collect();
        if !outside.is_empty() {
            bail!(
                "{repo_id}: the update touches files outside the allowed paths ({}): {}",
                patterns.join(", "),
                outside
                    .iter()
                    .map(|f| f.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    let observation = gate_observation(journal, root, repo_id, &reviewed, &live, &commits)?;
    Ok(
        json!({"sourceRepository":pr.source_repository,"previous":reviewed,"revision":live,"commits":commits,"files":files,"observation":observation,"acknowledged":acknowledged}),
    )
}

/// Security-sensitive local plumbing: preserve path delimiters and refuse
/// non-UTF-8 names instead of lossily matching them against allowed globs.
fn update_git(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("--no-replace-objects")
        .args(args)
        .current_dir(root)
        .output()?;
    if !output.status.success() {
        bail!(
            "update verification failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8(output.stdout).context("update paths must be valid UTF-8")
}

fn update_changes(root: &Path, reviewed: &str, live: &str) -> Result<(Vec<String>, Vec<String>)> {
    for sha in [reviewed, live] {
        if !super::mergeability::is_commit_sha(sha) {
            bail!("update requires exact commit SHAs");
        }
    }
    let grafts = update_git(root, &["rev-parse", "--git-path", "info/grafts"])?;
    if root.join(grafts.trim()).exists() || std::env::var_os("GIT_GRAFT_FILE").is_some() {
        bail!("update verification refuses grafted history");
    }
    if update_git(root, &["rev-parse", "--is-shallow-repository"])?.trim() == "true" {
        bail!("update verification requires complete history; unshallow the checkout first");
    }
    update_git(root, &["merge-base", "--is-ancestor", reviewed, live]).context(
        "update must contain the reviewed head and append commits, not rewrite reviewed work",
    )?;
    let commits: Vec<String> = update_git(
        root,
        &["rev-list", "--reverse", &format!("{reviewed}..{live}")],
    )?
    .lines()
    .map(str::to_owned)
    .collect();
    let mut touched = BTreeSet::new();
    for commit in &commits {
        let output = update_git(
            root,
            &[
                "diff-tree",
                "--root",
                "-m",
                "--no-commit-id",
                "--name-only",
                "--no-renames",
                "-r",
                "-z",
                commit,
            ],
        )?;
        touched.extend(
            output
                .split('\0')
                .filter(|path| !path.is_empty())
                .map(str::to_owned),
        );
    }
    Ok((commits, touched.into_iter().collect()))
}

/// Record commits a gate accepted in the bundle ledger when this workspace
/// has not recorded them yet (the update may come from another machine).
fn gate_observation(
    journal: &Journal,
    root: &Path,
    repo_id: &str,
    reviewed: &str,
    live: &str,
    commits: &[String],
) -> Result<Value> {
    let bundle = journal.bundle.lock().unwrap();
    let typed: crate::model::ChangeGroup = serde_json::from_value(bundle.clone())?;
    let Some(repo) = typed.repos.iter().find(|r| r.id == repo_id) else {
        return Ok(Value::Null);
    };
    let recorded = crate::tracking::latest_recorded_head_sha(&typed, repo)
        .unwrap_or_else(|| reviewed.to_owned());
    if recorded == live {
        return Ok(Value::Null);
    }
    update_changes(root, reviewed, &recorded)
        .context("recorded bundle head rewrote reviewed work")?;
    let (remaining, _) = update_changes(root, &recorded, live)
        .context("live review does not contain the recorded bundle head")?;
    let commits = if recorded == reviewed {
        commits
    } else {
        &remaining
    };
    Ok(json!({
        "repoId": repo_id, "movement": "advanced", "beforeSha": recorded,
        "afterSha": live, "commits": commits, "droppedCommits": [],
        "commitDetails": crate::git::commit_details(root, commits),
    }))
}

// Reconciliation and recovery must verify the source actually merged, which
// may be an update gate's accepted SHA rather than the immutable plan's SHA.
fn restore_review_receipt_target(
    target: &mut crate::providers::PrTarget,
    output: &Value,
) -> Result<()> {
    if let Some(identity) = target.contribution.as_mut() {
        if let Some(base) = output["targetBranch"].as_str() {
            identity.base = base.to_owned();
        }
        if let Some(source) = output["source"].as_str() {
            if !super::mergeability::is_commit_sha(source) {
                bail!("merge receipt source must be an exact commit SHA");
            }
            identity.sha = source.to_owned();
        }
    }
    Ok(())
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
        let mut target = provider_target(
            roots,
            forge.as_ref(),
            repo,
            crate::providers::publication_for_repo(&typed, &repo.id)
                .map(|p| p.base_branch.as_str())
                .unwrap_or(&repo.base_branch),
        )?;
        let publication =
            crate::providers::publication_for_repo(&typed, id).context("missing review")?;
        restore_review_receipt_target(&mut target, output)?;
        if target.contribution.is_some() {
            forge.view(&target, &publication.url)?;
        }
        output["revision"] = json!(forge.merged_revision(&target, &publication.url)?.context(
            "provider has not reported an immutable merged revision; resume after reconciliation"
        )?);
    }
    let revision = output["revision"].as_str().unwrap();
    if ![40, 64].contains(&revision.len()) || !revision.bytes().all(|c| c.is_ascii_hexdigit()) {
        bail!("provider returned invalid merged commit identity");
    }
    if let Some(root) = roots.get(id) {
        let remote = super::mergeability::destination(bundle, id);
        let object = format!("{revision}^{{commit}}");
        if git(root, &["cat-file", "-e", &object]).is_err()
            && git(root, &["fetch", "--no-tags", remote, revision]).is_err()
        {
            let branch = output["targetBranch"]
                .as_str()
                .context("merge destination required")?;
            git(root, &["fetch", "--no-tags", remote, branch])?;
        }
        if git(root, &["rev-parse", &object])? != revision {
            bail!("merge object identity mismatch");
        }
    }
    Ok(())
}

fn needs_terminal(step: &Value) -> bool {
    step["interactive"] == true || (step["type"] == "manual" && !super::gates::is_gate(step))
}

fn terminal_preflight(plan: &Value, local: bool) -> Result<()> {
    use std::io::IsTerminal;
    let (steps, _) = compile(plan)?;
    if !local && super::gates::uses_gates(plan) {
        bail!("landing gates pause the run for a person; run this plan locally with `knit land apply` and continue it with `knit land resume`");
    }
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
    let id = step["id"].as_str().unwrap();
    let (result, records) = super::super::git_progress::scope(&format!("{id}/forward"), || {
        forward_step_inner(step, plan, bundle, roots, journal)
    });
    record_git_commands(journal, id, records)?;
    result
}

fn forward_step_inner(
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
    if super::gates::is_gate(step) {
        return gate(step, plan, bundle, roots, journal);
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
                            if let Some(e) = result.as_ref().err().filter(|e| {
                                e.downcast_ref::<super::gates::LandingPaused>().is_none()
                            }) {
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
            // Joining the entire batch is mandatory before compensation. A
            // real failure outranks a gate that paused in the same batch.
            let mut paused = None;
            for result in results {
                if let Err(e) = result {
                    if e.downcast_ref::<super::gates::LandingPaused>().is_some() {
                        paused.get_or_insert(e);
                    } else {
                        return Err(e);
                    }
                }
            }
            if let Some(paused) = paused {
                return Err(paused);
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
                    let mut target = provider_target(
                        roots,
                        forge.as_ref(),
                        repo,
                        crate::providers::publication_for_repo(&typed, &repo.id)
                            .map(|p| p.base_branch.as_str())
                            .unwrap_or(&repo.base_branch),
                    )?;
                    restore_review_receipt_target(&mut target, &record["output"])?;
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
                    let (url, _) = crate::commands::revert::create_review(forge.as_ref(), &target, repo, &pub_.url, "Revert landing change", "Source compensation for a landing run; deployment restoration is recorded separately.")?;
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
pub(super) fn verify_run(run: &Value, plan: &Value) -> Result<()> {
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
    let merges = super::ledger::completed_merges(&r, bundle)?;
    let repos = super::ledger::merge_repos(&merges);
    let urls = super::ledger::merge_urls(&merges);
    let no_merges = super::ledger::no_source_merges(&r);
    anyhow::ensure!(
        !merges.is_empty() || no_merges,
        "landing lacks completed source merge receipts"
    );
    if let Some(node) = typed
        .nodes
        .iter_mut()
        .find(|n| n.node_type == "feature.landed" && n.run_id.as_deref() == Some(run_id))
    {
        // A crash or old version may have persisted the summary before cleanup.
        // Resume repairs only missing evidence; it never appends a second landing.
        anyhow::ensure!(
            node.plan_id.as_deref() == plan["id"].as_str(),
            "landing node plan differs from run"
        );
        anyhow::ensure!(
            node.repo_ids
                .as_ref()
                .map(|ids| ids.iter().collect::<BTreeSet<_>>())
                == Some(repos.iter().collect()),
            "landing node repositories differ from run"
        );
        anyhow::ensure!(
            node.publication_urls.is_empty()
                || node.publication_urls.iter().collect::<BTreeSet<_>>() == urls.iter().collect(),
            "landing node publications differ from run"
        );
        anyhow::ensure!(
            node.landing
                .as_ref()
                .is_none_or(|landing| landing.branch_only != Some(true))
                || super::ledger::branch_only(&merges),
            "branch-only landing node differs from review merge receipts"
        );
        anyhow::ensure!(
            node.landing
                .as_ref()
                .and_then(|l| l.merge_mode.as_deref())
                .is_none()
                || (no_merges
                    && node.landing.as_ref().and_then(|l| l.merge_mode.as_deref()) == Some("none")),
            "landing merge mode differs from run"
        );
        if no_merges {
            node.landing
                .as_mut()
                .context("no-merge node lacks landing destination")?
                .merge_mode = Some("none".into());
        }
        if node.publication_urls.is_empty() {
            node.publication_urls = urls;
        }
        if super::ledger::branch_only(&merges) {
            let landing = node
                .landing
                .as_mut()
                .context("branch-only node lacks landing destination")?;
            landing.branch_only = Some(true);
        }
    } else {
        let node = crate::model::BundleNode::feature_landed(
            unique_id("land"),
            now_iso(),
            plan["id"].as_str().unwrap().into(),
            run_id.into(),
            plan["provider"].as_str().unwrap_or("github").into(),
            repos,
            urls,
            Some(crate::model::NodeLanding {
                merge_mode: no_merges.then(|| "none".into()),
                branch_only: super::ledger::branch_only(&merges).then_some(true),
                terminal: plan["terminal"] != false,
                lane: plan["lane"].as_str().map(str::to_owned),
                target_branch: plan["targetBranch"].as_str().map(str::to_owned),
            }),
        );
        typed.nodes.push(node);
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
    *bundle = crate::model::preserve_bundle_extensions(bundle, serde_json::to_value(typed)?)?;
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
    let (acknowledge, note) = local
        .as_ref()
        .and_then(|(_, options)| *options)
        .map(|options| (options.acknowledge, options.note))
        .unwrap_or((&[], None));
    if note.is_some_and(|note| note.len() > 4096) {
        bail!("acknowledgement note exceeds 4096 bytes");
    }
    if note.is_some() && acknowledge.is_empty() {
        bail!("--note requires --acknowledge");
    }
    for id in acknowledge {
        if !plan["steps"].as_array().is_some_and(|steps| {
            steps
                .iter()
                .any(|step| step["id"] == *id && super::gates::is_gate(step))
        }) {
            bail!("{id}: --acknowledge must name a landing gate in this immutable plan");
        }
    }
    if json_output {
        crate::output::route_human_lines_to_stderr();
    }
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
        let merged =
            serde_json::to_value(crate::model::merge_ledgers(&incoming, &prior, now_iso()))?;
        // Current extensions are authoritative. Restore them first, then fill
        // only missing extensions from the saved result (including prior-only nodes).
        let merged = crate::model::preserve_bundle_extensions(&bundle, merged)?;
        bundle = crate::model::preserve_bundle_extensions(result, merged)?;
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
                            && (s["reviewBefore"]["targetBranch"].is_string()
                                || matches!(
                                    s["attribution"].as_str(),
                                    Some("performed" | "already_satisfied")
                                ))
                    })
                }) {
                    if publication["baseBranch"] == receipt["output"]["targetBranch"]
                        || publication["baseBranch"] == receipt["reviewBefore"]["targetBranch"]
                    {
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
    if resume {
        for receipt in existing
            .as_ref()
            .into_iter()
            .flat_map(|run| run["steps"].as_array().into_iter().flatten())
            .filter(|s| s["type"] == "await_update" && s["status"] == "succeeded")
        {
            let id = receipt["repoId"].as_str().context("gate repo required")?;
            if let Some(repo) = bundle["repos"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|r| r["id"] == id)
            {
                if repo["headSha"] != receipt["output"]["revision"]
                    && repo["headSha"] != plan["bundleHeads"][id]
                {
                    bail!("{id}: bundle head changed after the update gate accepted its immutable head; review a new plan");
                }
            }
        }
        // Only the reviewed head is mutable at an update gate. All other
        // repository, publication and changed-scope identity remains pinned.
        for repo in comparable["repos"].as_array_mut().into_iter().flatten() {
            if let Some(id) = repo["id"]
                .as_str()
                .filter(|id| super::gates::gated_repos(plan).contains(*id))
            {
                if let Some(original) = source["repos"]
                    .as_array()
                    .and_then(|repos| repos.iter().find(|r| r["id"] == id))
                {
                    repo["headSha"] = original["headSha"].clone();
                }
            }
        }
    }
    if super::graph::bundle_fingerprint(&super::branch_checkout::reviewed_identity(
        plan,
        &comparable,
    )) != super::graph::bundle_fingerprint(&super::branch_checkout::reviewed_identity(
        plan, source,
    )) {
        bail!("bundle changed since this run started");
    }
    if validation["valid"] != true {
        bail!("{}", validation["errors"]);
    }
    if plan["schemaVersion"] != "0.2" {
        bail!("exact-plan artifact execution requires v0.2");
    }
    let (execution_plan, resolved_sources) =
        super::branch_checkout::execution_sources(plan, source, &roots, existing.as_ref())?;
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
            &execution_plan,
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
        let mut checked_plan = execution_plan.clone();
        if let Some(run) = &existing {
            // Pending gates validate these incoming heads themselves. Named
            // checks refreshed after a recorded bump must not be compared to
            // the old plan head before the gate can accept that bump.
            for repo in bundle["repos"].as_array().into_iter().flatten() {
                if let Some(id) = repo["id"]
                    .as_str()
                    .filter(|id| super::gates::gated_repos(plan).contains(*id))
                {
                    checked_plan["bundleHeads"][id] = repo["headSha"].clone();
                }
            }
            for receipt in run["steps"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|s| s["type"] == "await_update" && s["status"] == "succeeded")
            {
                let id = receipt["repoId"]
                    .as_str()
                    .context("gate receipt repo required")?;
                checked_plan["bundleHeads"][id] = receipt["output"]["revision"].clone();
            }
        }
        preflight(
            &checked_plan,
            &steps,
            &roots,
            &bundle,
            skip_checks,
            existing.as_ref(),
        )?;
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
            let snapshot = super::branch_checkout::source_snapshot(&bundle, &resolved_sources);
            let mut run = new_run(plan, plan_path, &snapshot);
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
        r["finalization"]["synchronization"] = json!("pending");
        for id in acknowledge {
            if r["acknowledgements"][id].is_null() {
                r["acknowledgements"][id] = json!({"at":now_iso(),"notes":note});
            }
        }
        r["checksSkipped"] = json!(skip_checks);
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
        compensation(&execution_plan, &bundle, &roots, &journal)
    } else {
        journal.edit(|r| {
            r["status"] = json!("running");
            r.as_object_mut().unwrap().remove("pause");
            r.as_object_mut().unwrap().remove("error");
        })?;
        let forward_result = forward(&execution_plan, &bundle, &roots, &journal);
        match forward_result {
            Ok(()) => finalize(plan, &mut bundle, &journal, out),
            Err(e) if e.downcast_ref::<super::gates::LandingPaused>().is_some() => {
                let pause = e.downcast_ref::<super::gates::LandingPaused>().unwrap();
                journal.edit(|r| {
                    r["status"] = json!("paused");
                    r["pause"] = json!({"step":pause.step,"instructions":pause.instructions,
                        "since":now_iso(),"waitingFor":pause.waiting_for,"next":pause.next});
                })?;
                Ok(())
            }
            Err(e) => {
                journal.edit(|r| {
                    r["status"] = json!("failed");
                    r["error"] = json!(format!("{e:#}"));
                    r["serviceStatus"] = json!("unknown");
                })?;
                if plan["onFailure"] == "recover"
                    && !super::super::process::cancellation_requested()
                {
                    let _ = compensation(&execution_plan, &bundle, &roots, &journal);
                }
                Err(e)
            }
        }
    };
    let local_sync = local.as_ref().and_then(|(active, options)| {
        options.map(|options| {
            (
                active.root.clone(),
                options.remote.to_vec(),
                options.no_remote,
            )
        })
    });
    let result = result.and_then(|()| {
        if !recovering && journal.snapshot()["status"] != "paused" {
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
        s["status"] != "running"
            && (s["attribution"] != "uncertain" || s["quiesced"] == true)
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
    // Once the authority accepts the final receipt, keep it immutable even if
    // a secondary sync destination fails. Ordinary sync can retry those bytes.
    let completion = completion.and_then(|()| {
        if let Some((root, remotes, no_remote)) = local_sync {
            crate::commands::remote::landing::sync_finished_run(&root, plan, &remotes, no_remote)?;
        }
        Ok(())
    });
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
    if !json_output && journal.snapshot()["status"] == "paused" {
        let pause = journal.snapshot()["pause"].clone();
        eprintln!(
            "{}\n{}",
            pause["instructions"].as_str().unwrap_or(""),
            pause["next"].as_str().unwrap_or("")
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
    super::branch_checkout::local_roots(&plan, &project, active, &mut roots)?;
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
        let mut live = super::branch_checkout::live_sources(&plan);
        if run_path.is_some() {
            live.extend(super::gates::gated_repos(&plan));
        }
        let unrecorded: Vec<_> = crate::tracking::detect_unrecorded_changes(active)?
            .into_iter()
            .filter(|change| !live.contains(&change.repo_id))
            .collect();
        if !unrecorded.is_empty() {
            bail!("worktree heads changed; run knit sync and regenerate the plan");
        }
        // Required checks are evaluated against the same effective heads the
        // executor uses: with an integration source, freshness speaks for the
        // pinned source, not the reviewed feature head.
        if live.is_empty() && super::mergeability::has_integration_sources(&plan) {
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
        } else if live.is_empty() {
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
    let raw_bundle: Value = read_json(&active.bundle_path)?;
    let bundle = crate::model::preserve_bundle_extensions(
        &raw_bundle,
        serde_json::to_value(&active.bundle)?,
    )?;
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
            let mut raw = journal.bundle.lock().unwrap();
            *raw = crate::model::preserve_bundle_extensions(
                &raw,
                serde_json::to_value(&active.bundle)?,
            )?;
            durable(&active.bundle_path, &raw)?;
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
            let mut raw = journal.bundle.lock().unwrap();
            *raw = crate::model::preserve_bundle_extensions(
                &raw,
                serde_json::to_value(&active.bundle)?,
            )?;
            durable(&active.bundle_path, &raw)?;
            drop(raw);
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

#[cfg(test)]
mod gate_history_tests {
    use super::*;
    #[test]
    fn updates_include_transient_edits_rename_sources_and_newline_names_and_refuse_rewrites() {
        let root = std::env::temp_dir().join(unique_id("gate-history"));
        fs::create_dir_all(&root).unwrap();
        let run = |args: &[&str]| update_git(&root, args).unwrap().trim().to_owned();
        run(&["init", "-b", "main"]);
        run(&["config", "user.name", "Synthetic Test"]);
        run(&["config", "user.email", "test@example.invalid"]);
        run(&["config", "commit.gpgsign", "false"]);
        fs::write(root.join("source.txt"), "original").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "Reviewed work"]);
        let reviewed = run(&["rev-parse", "HEAD"]);
        fs::write(root.join("source.txt"), "temporary forbidden edit").unwrap();
        run(&["commit", "-am", "Transient edit"]);
        fs::write(root.join("source.txt"), "original").unwrap();
        run(&["commit", "-am", "Revert transient edit"]);
        run(&["mv", "source.txt", "Cargo.toml"]);
        #[cfg(unix)]
        fs::write(root.join("Cargo.lock\nsource.rs"), "not a manifest").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "Rename to manifest"]);
        let live = run(&["rev-parse", "HEAD"]);
        let (commits, paths) = update_changes(&root, &reviewed, &live).unwrap();
        assert_eq!(commits.len(), 3);
        assert!(paths.contains(&"source.txt".into()));
        assert!(paths.contains(&"Cargo.toml".into()));
        #[cfg(unix)]
        assert!(paths.contains(&"Cargo.lock\nsource.rs".into()));
        run(&["checkout", "--orphan", "rewritten"]);
        run(&["commit", "-am", "Unrelated replacement"]);
        let replacement = run(&["rev-parse", "HEAD"]);
        assert!(update_changes(&root, &reviewed, &replacement).is_err());
        // A replacement ref must not manufacture the required ancestry.
        run(&["replace", "--graft", &replacement, &reviewed]);
        assert!(update_changes(&root, &reviewed, &replacement).is_err());
        assert!(update_changes(&root, "HEAD", &live).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod landing_record_tests {
    use super::*;
    use crate::commands::bundle::validate_change_group;

    fn fixture(steps: Value, terminal: bool) -> (Value, Value, Journal, PathBuf) {
        let root = std::env::temp_dir().join(unique_id("landing-record"));
        fs::create_dir_all(&root).unwrap();
        let mut bundle = serde_json::to_value(crate::model::ChangeGroup::new(
            "sample".into(),
            "Synthetic landing".into(),
            "2026-01-01T00:00:00Z".into(),
        ))
        .unwrap();
        bundle["repos"] = json!([
            {"id":"api","path":"api","baseBranch":"main"},
            {"id":"web","path":"web","baseBranch":"main"},
            {"id":"docs","path":"docs","baseBranch":"main"}
        ]);
        let plan = json!({"id":"plan-sample","bundleId":"sample","provider":"github","terminal":terminal,"steps":steps});
        let run = json!({"id":"run-sample","plan":plan,"steps":steps});
        let journal = Journal {
            value: Mutex::new(run),
            path: root.join("run.json"),
            bundle: Mutex::new(bundle.clone()),
            bundle_out: root.join("bundle.json"),
            workspace: root.clone(),
            pins: Mutex::new(()),
        };
        (plan, bundle, journal, root)
    }
    fn assert_valid(bundle: &Value) {
        let typed = serde_json::from_value(bundle.clone()).unwrap();
        assert_eq!(validate_change_group(&typed), Vec::<String>::new());
        assert!(!bundle.to_string().contains("landingMerges"));
        let schema: Value =
            serde_json::from_str(include_str!("../../../../schemas/bundle.schema.json")).unwrap();
        assert!(jsonschema::validator_for(&schema).unwrap().is_valid(bundle));
    }
    fn summary(bundle: &Value) -> &Value {
        bundle["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["type"] == "feature.landed")
            .unwrap()
    }

    #[test]
    fn landing_record_finalize_review_and_mixed_merges_validate_and_resume_repairs() {
        let steps = json!([
            {"id":"web","type":"merge_pr","repoId":"web","status":"succeeded","output":{"publicationUrl":"https://example.invalid/web/pull/2","targetBranch":"main"}},
            {"id":"api","type":"merge_pr","repoId":"api","status":"succeeded","output":{"publicationUrl":"https://example.invalid/api/pull/1","targetBranch":"main"}},
            {"id":"api-again","type":"merge_pr","repoId":"api","status":"succeeded","output":{"publicationUrl":"https://example.invalid/api/pull/1","targetBranch":"main"}}
        ]);
        for mixed in [false, true] {
            let mut steps = steps.clone();
            if mixed {
                steps.as_array_mut().unwrap().push(json!({"id":"docs","type":"merge_branch","repoId":"docs","status":"succeeded","output":{"targetBranch":"main"}}));
            }
            let (plan, mut bundle, journal, root) = fixture(steps, true);
            finalize(&plan, &mut bundle, &journal, &journal.bundle_out).unwrap();
            assert_valid(&bundle);
            assert_eq!(
                summary(&bundle)["publicationUrls"],
                json!([
                    "https://example.invalid/api/pull/1",
                    "https://example.invalid/web/pull/2"
                ])
            );
            assert!(summary(&bundle)["landing"]["branchOnly"].is_null());
            assert_eq!(bundle["state"], "archived");
            let original = summary(&bundle).clone();
            {
                let mut prior = journal.bundle.lock().unwrap();
                let node = prior["nodes"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|n| n["type"] == "feature.landed")
                    .unwrap();
                node.as_object_mut().unwrap().remove("publicationUrls");
            }
            finalize(&plan, &mut bundle, &journal, &journal.bundle_out).unwrap();
            assert_valid(&bundle);
            assert_eq!(summary(&bundle), &original);
            assert_eq!(
                bundle["nodes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|n| n["type"] == "feature.landed")
                    .count(),
                1
            );
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn landing_record_finalize_and_resume_preserve_extensions_across_appended_nodes() {
        let (plan, mut bundle, journal, root) = fixture(
            json!([
                {"id":"api","type":"merge_pr","repoId":"api","status":"succeeded","attribution":"performed","output":{"publicationUrl":"https://example.invalid/api/pull/1","targetBranch":"main"}}
            ]),
            true,
        );
        bundle["custom"] = json!({"bundle":1});
        bundle["repos"][0]["custom"] = json!({"repo":"api"});
        bundle["repos"][1]["custom"] = json!({"repo":"web"});
        bundle["nodes"][0]["custom"] = json!({"node":1});
        bundle["nodes"][0]["landing"] = json!({"terminal":false,"custom":{"nested":1}});
        let original_node = bundle["nodes"][0].clone();
        *journal.bundle.lock().unwrap() = bundle.clone();
        // Receipt persistence runs before finalize and itself appends a node.
        journal.edit(|_| {}).unwrap();
        finalize(&plan, &mut bundle, &journal, &journal.bundle_out).unwrap();
        assert_eq!(bundle["nodes"][0], original_node);
        assert_eq!(bundle["custom"], json!({"bundle":1}));
        assert_eq!(bundle["repos"][0]["custom"], json!({"repo":"api"}));
        assert_eq!(bundle["repos"][1]["custom"], json!({"repo":"web"}));
        assert_valid(&bundle);
        let expected;
        {
            let mut prior = journal.bundle.lock().unwrap();
            let node = prior["nodes"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|n| n["type"] == "feature.landed")
                .unwrap();
            node["custom"] = json!({"audit":"keep"});
            node["landing"]["custom"] = json!({"destination":"keep"});
            expected = node.clone();
            node.as_object_mut().unwrap().remove("publicationUrls");
        }
        finalize(&plan, &mut bundle, &journal, &journal.bundle_out).unwrap();
        assert_eq!(summary(&bundle), &expected);
        assert_eq!(bundle["nodes"][0], original_node);
        assert_eq!(bundle["custom"], json!({"bundle":1}));
        assert_eq!(read_json::<Value>(&journal.bundle_out).unwrap(), bundle);
        assert_valid(&bundle);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn landing_record_no_merge_completion_keeps_lifecycle_without_source_claims() {
        for terminal in [false, true] {
            let (plan, mut bundle, journal, root) = fixture(
                json!([
                    {"id":"deploy","type":"run","repoId":"api","status":"succeeded"}
                ]),
                terminal,
            );
            finalize(&plan, &mut bundle, &journal, &journal.bundle_out).unwrap();
            assert_valid(&bundle);
            let node = summary(&bundle);
            assert_eq!(node["landing"]["mergeMode"], "none");
            assert_eq!(node["repoIds"], json!([]));
            assert!(node["publicationUrls"].is_null());
            assert!(node["landing"]["branchOnly"].is_null());
            let typed_node = serde_json::from_value(node.clone()).unwrap();
            assert_eq!(crate::model::is_terminal_landed_node(&typed_node), terminal);
            assert_eq!(bundle["state"] == "archived", terminal);
            let schema: Value =
                serde_json::from_str(include_str!("../../../../schemas/bundle.schema.json"))
                    .unwrap();
            let schema = jsonschema::validator_for(&schema).unwrap();
            for field in ["repoIds", "publicationUrls", "branchOnly", "mergeMode"] {
                let mut bad = bundle.clone();
                let node = bad["nodes"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|n| n["type"] == "feature.landed")
                    .unwrap();
                match field {
                    "repoIds" => node[field] = json!(["api"]),
                    "publicationUrls" => {
                        node[field] = json!(["https://example.invalid/api/pull/1"])
                    }
                    "branchOnly" => node["landing"][field] = json!(true),
                    _ => node["landing"][field] = json!("unknown"),
                }
                assert!(!schema.is_valid(&bad));
                assert!(!validate_change_group(&serde_json::from_value(bad).unwrap()).is_empty());
            }
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn landing_record_validation_rejects_missing_review_urls_and_contradictory_mode() {
        let (plan, mut bundle, journal, root) = fixture(
            json!([
                {"id":"api","type":"merge_pr","repoId":"api","status":"succeeded","output":{"publicationUrl":"https://example.invalid/api/pull/1","targetBranch":"main"}}
            ]),
            true,
        );
        finalize(&plan, &mut bundle, &journal, &journal.bundle_out).unwrap();
        let schema: Value =
            serde_json::from_str(include_str!("../../../../schemas/bundle.schema.json")).unwrap();
        let schema = jsonschema::validator_for(&schema).unwrap();
        for case in 0..4 {
            let mut malformed = bundle.clone();
            let node = malformed["nodes"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|n| n["type"] == "feature.landed")
                .unwrap();
            match case {
                0 => {
                    node.as_object_mut().unwrap().remove("publicationUrls");
                }
                1 => {
                    node["publicationUrls"] = json!([]);
                    node["landing"]["branchOnly"] = json!(false);
                }
                2 => node["publicationUrls"] = json!([""]),
                3 => node["landing"]["branchOnly"] = json!(true),
                _ => unreachable!(),
            }
            assert!(!schema.is_valid(&malformed), "case {case}");
            assert!(
                !validate_change_group(&serde_json::from_value(malformed).unwrap()).is_empty(),
                "case {case}"
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn landing_record_branch_only_terminal_and_intermediate_validate_without_urls() {
        for terminal in [false, true] {
            let (plan, mut bundle, journal, root) = fixture(
                json!([
                    {"id":"api","type":"merge_branch","repoId":"api","status":"succeeded","output":{"targetBranch":"staging"}}
                ]),
                terminal,
            );
            finalize(&plan, &mut bundle, &journal, &journal.bundle_out).unwrap();
            assert_valid(&bundle);
            assert_eq!(summary(&bundle)["landing"]["branchOnly"], true);
            assert!(summary(&bundle)["publicationUrls"].is_null());
            assert_eq!(bundle["state"] == "archived", terminal);
            let mut malformed = bundle.clone();
            let node = malformed["nodes"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|n| n["type"] == "feature.landed")
                .unwrap();
            node["landing"]
                .as_object_mut()
                .unwrap()
                .remove("branchOnly");
            assert!(
                validate_change_group(&serde_json::from_value(malformed).unwrap())
                    .iter()
                    .any(|e| e.contains("publicationUrls"))
            );
            fs::remove_dir_all(root).unwrap();
        }
    }
}
