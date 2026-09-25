//! Mergeability preflight, integration source provenance, and the expected
//! target guard.
//!
//! A plan that opts in with `preflight = { "mergeability": "all" }` may not
//! change any remote ref or run any effectful command until every pending
//! `merge_branch` step has been simulated: the target tip is resolved live,
//! the exact pinned source is merged into it inside an isolated temporary
//! checkout (the source checkout is never touched), and any conflicting files
//! are collected into a report. One conflict anywhere fails the whole
//! preflight, so a later repository's conflict can never leave an earlier
//! repository's remote already merged. Plans without the policy get no
//! global branch scans; the only always-on verification is integration
//! source provenance, which must stand on its own.
//!
//! The tips observed during the preflight are recorded on the run receipts
//! as `expectedTarget`. The real merge runs against the exact expected tip:
//! the target is fetched, required to equal the expected SHA, and the result
//! is published with `--force-with-lease=<branch>:<expected>` — an atomic
//! conditional update — but only after asserting the merge result descends
//! from the expected tip, so no update can ever rewrite history. If the
//! target moved or was rewound in the meantime, the merge is rejected
//! instead of overwriting the intervening work.
//!
//! `integrationSources` names, per repository, an explicit `{branch, sha}`
//! pair to merge instead of the bundle head. It only applies to
//! `merge_branch` steps, never to a review merge, and the bundle's own
//! fingerprint and pins stay untouched. Provenance is verified independently
//! of any review state: the branch is resolved live, its tip must equal the
//! pinned SHA, and the pinned SHA must contain the reviewed bundle head as
//! an ancestor.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub(crate) const CAPABILITY: &str = "mergeability-preflight";
pub(crate) const CAPABILITY_SOURCES: &str = "integration-sources";

/// An error raised when a step refused before any of its effects happened —
/// target drift caught by the expected-target guard, integration-source
/// provenance that no longer holds. Nothing to reconcile: the executor
/// records the step as safely retryable (no uncertain attribution, quiesced)
/// instead of demanding a recovery probe.
#[derive(Debug)]
pub(crate) struct KnownNoEffect(pub String);

impl std::fmt::Display for KnownNoEffect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for KnownNoEffect {}

/// One merge check as it appears in preflight reports.
#[derive(Clone)]
pub(crate) struct MergeCheck {
    pub step_id: String,
    pub repo_id: String,
    pub source_branch: Option<String>,
    pub source_sha: String,
    pub target_branch: String,
    pub target_sha: Option<String>,
    pub status: String,
    pub conflicts: Vec<Value>,
    pub error: Option<String>,
}

impl MergeCheck {
    fn to_json(&self) -> Value {
        json!({
            "stepId": self.step_id,
            "repoId": self.repo_id,
            "sourceBranch": self.source_branch,
            "sourceSha": self.source_sha,
            "targetBranch": self.target_branch,
            "targetSha": self.target_sha,
            "status": self.status,
            "conflicts": self.conflicts,
            "error": self.error,
        })
    }
}

/// Why the checks are running: the apply path is strict and opt-in, the
/// read-only report is informational when no policy is declared.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckMode {
    Apply,
    Report,
}

pub(crate) fn mergeability_enabled(plan: &Value) -> bool {
    plan["preflight"]["mergeability"].as_str() == Some("all")
}

pub(crate) fn has_integration_sources(plan: &Value) -> bool {
    plan["integrationSources"]
        .as_object()
        .is_some_and(|sources| !sources.is_empty())
}

/// The bundle view required checks must be evaluated against: when a
/// repository integrates an explicit source, its effective head is the
/// pinned integration SHA — an old verdict recorded at the reviewed feature
/// head must read as stale, never as green for code that will not run.
/// Returns an ephemeral copy; the original artifact is never modified.
pub(crate) fn effective_required_checks_bundle(plan: &Value, bundle: &Value) -> Value {
    let mut effective = bundle.clone();
    for (repo, pin) in plan["integrationSources"].as_object().into_iter().flatten() {
        if let Some(sha) = pin["sha"].as_str() {
            if let Some(entry) = effective["repos"]
                .as_array_mut()
                .and_then(|repos| repos.iter_mut().find(|r| r["id"].as_str() == Some(repo)))
            {
                entry["headSha"] = json!(sha);
                // Drop every checkout binding so the recorded head is the
                // authority for freshness.
                entry["worktreePath"] = Value::Null;
                entry["checkoutMode"] = json!("worktree");
            }
        }
    }
    effective
}

pub(crate) fn validate_plan_preflight(plan: &Value) -> Result<()> {
    let Some(preflight) = plan.get("preflight") else {
        return Ok(());
    };
    if !preflight.is_object() {
        bail!("preflight must be an object");
    }
    for key in preflight.as_object().unwrap().keys() {
        if key != "mergeability" {
            bail!("preflight.{key} is not a recognized preflight policy");
        }
    }
    match preflight["mergeability"].as_str() {
        Some("all") => Ok(()),
        None => bail!("preflight declared without a mergeability policy"),
        Some(other) => bail!(
            "unsupported preflight.mergeability `{other}`; the only supported policy is `all`"
        ),
    }
}

/// A full (lowercase) commit SHA as required for pins: 40 or 64 hex digits.
pub(crate) fn is_commit_sha(value: &str) -> bool {
    (value.len() == 40 || value.len() == 64)
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

/// A branch name usable as `refs/heads/<branch>`.
pub(crate) fn is_valid_branch(value: &str) -> bool {
    if value.is_empty()
        || value.starts_with('-')
        || value.starts_with('.')
        || value.ends_with('/')
        || value.ends_with(".lock")
        || value.contains("..")
        || value.contains("//")
        || value.contains("@{")
        || value.contains(['\\', ' ', '~', '^', ':', '?', '*', '[', '\x7f'])
        || value.split('/').any(|c| c.is_empty() || c.starts_with('.'))
    {
        return false;
    }
    let refname = format!("refs/heads/{value}");
    std::process::Command::new("git")
        .args(["check-ref-format", "--allow-onelevel", &refname])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

pub(crate) fn validate_plan_integration_sources(
    plan: &Value,
    bundle: Option<&Value>,
) -> Result<()> {
    let Some(sources) = plan.get("integrationSources") else {
        return Ok(());
    };
    let sources = sources
        .as_object()
        .context("integrationSources must map repository IDs to {branch, sha}")?;
    if sources.is_empty() {
        bail!("integrationSources must declare at least one repository");
    }
    let steps = plan["steps"].as_array().context("steps required")?;
    for (repo, pin) in sources {
        if !pin.is_object() {
            bail!("integrationSources.{repo} must be an object with branch and sha");
        }
        for key in pin.as_object().unwrap().keys() {
            if !["branch", "sha"].contains(&key.as_str()) {
                bail!("integrationSources.{repo}.{key} is not a recognized field");
            }
        }
        let branch = pin["branch"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .with_context(|| {
                format!("integrationSources.{repo}.branch must be a nonempty branch name")
            })?;
        if !is_valid_branch(branch) {
            bail!("integrationSources.{repo}.branch is not a valid Git branch name: {branch}");
        }
        if !pin["sha"].as_str().is_some_and(is_commit_sha) {
            bail!(
                "integrationSources.{repo}.sha must be a full lowercase commit SHA (40 or 64 hex digits)"
            );
        }
        let merge_branch = steps
            .iter()
            .any(|s| s["type"] == "merge_branch" && s["repoId"].as_str() == Some(repo.as_str()));
        if !merge_branch {
            let review_merge = steps
                .iter()
                .any(|s| s["type"] == "merge_pr" && s["repoId"].as_str() == Some(repo.as_str()));
            if review_merge {
                bail!(
                    "integrationSources.{repo} is not allowed: {repo} merges its recorded review (merge_pr); integration sources only apply to merge_branch steps"
                );
            }
            bail!(
                "integrationSources.{repo} has no associated merge_branch step; integration sources only apply to merge_branch steps"
            );
        }
        if let Some(bundle) = bundle {
            if plan["bundleHeads"][repo].as_str().is_none() {
                bail!(
                    "integrationSources.{repo} requires a reviewed bundle head to verify provenance against"
                );
            }
            let feature_branch = bundle["repos"].as_array().and_then(|repos| {
                repos
                    .iter()
                    .find(|r| r["id"].as_str() == Some(repo.as_str()))
                    .and_then(|r| r["featureBranch"].as_str())
            });
            if feature_branch == Some(branch) {
                bail!(
                    "integrationSources.{repo}.branch is the bundle's own feature branch; name the integration branch the reviewed work was already merged into"
                );
            }
        }
    }
    Ok(())
}

/// The source a merge step integrates: the plan's integration source when
/// pinned, otherwise the reviewed bundle head.
pub(crate) fn merge_source(
    plan: &Value,
    bundle: &Value,
    repo_id: &str,
) -> Option<(Option<String>, String)> {
    if let Some(pin) = plan["integrationSources"][repo_id].as_object() {
        return Some((
            pin.get("branch").and_then(Value::as_str).map(str::to_owned),
            pin.get("sha")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
        ));
    }
    let sha = plan["bundleHeads"][repo_id]
        .as_str()
        .map(str::to_owned)
        .or_else(|| {
            bundle["repos"]
                .as_array()
                .and_then(|repos| repos.iter().find(|r| r["id"].as_str() == Some(repo_id)))
                .and_then(|r| r["headSha"].as_str().map(str::to_owned))
        })?;
    let branch = bundle["repos"]
        .as_array()
        .and_then(|repos| repos.iter().find(|r| r["id"].as_str() == Some(repo_id)))
        .and_then(|r| r["featureBranch"].as_str().map(str::to_owned));
    Some((branch, sha))
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    crate::git::git_output(dir, args)
        .with_context(|| format!("git {} (in {})", args.join(" "), dir.display()))
}

fn git_best_effort(dir: &Path, args: &[&str]) -> Option<String> {
    git(dir, args).ok()
}

/// Resolve the live tip of a branch on `origin` with a read-only
/// `ls-remote`. When origin answers, the answer is authoritative: a missing
/// ref is a missing branch, never papered over with a cached or local ref.
/// Fetch/transport failures are returned as errors so callers fail closed.
pub(crate) fn target_tip(root: &Path, branch: &str) -> Result<Option<String>> {
    crate::git::remote_ref_sha(root, "origin", &format!("refs/heads/{branch}"))
        .with_context(|| format!("failed to resolve origin/{branch} from {}", root.display()))
}

/// Verify an integration source against its live branch: the branch tip must
/// equal the pinned SHA and must contain the reviewed bundle head. Checked
/// independently of any review state, both at preflight and again at
/// execution time.
pub(crate) fn verify_integration_source(
    root: &Path,
    repo_id: &str,
    branch: &str,
    sha: &str,
    reviewed_head: &str,
) -> Result<()> {
    let tip = target_tip(root, branch)?.with_context(|| {
        format!("{repo_id}: integration source branch {branch} is missing from origin")
    })?;
    if tip != sha {
        bail!(
            "{repo_id}: integration source branch {branch} drifted from the pinned SHA (expected {sha}, found {tip}); re-author the plan with `knit land source`"
        );
    }
    let missing =
        |object: &str| git(root, &["cat-file", "-e", &format!("{object}^{{commit}}")]).is_err();
    if missing(sha) || missing(reviewed_head) {
        git(
            root,
            &[
                "fetch",
                "--quiet",
                "--no-tags",
                "origin",
                &format!("+refs/heads/{branch}:refs/knit/landing/source/{repo_id}"),
            ],
        )
        .with_context(|| {
            format!(
                "{repo_id}: fetching integration source branch {branch} from origin failed; refusing to certify provenance from local objects alone"
            )
        })?;
        if missing(sha) || missing(reviewed_head) {
            bail!(
                "{repo_id}: integration source objects for {branch} are unavailable after fetching; provenance cannot be verified"
            );
        }
    }
    if !crate::git::is_ancestor(root, reviewed_head, sha) {
        bail!(
            "{repo_id}: integration source {sha} does not include the reviewed bundle head {reviewed_head}; it cannot certify this bundle's work"
        );
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) struct Simulation {
    pub contained: bool,
    pub conflicts: Vec<String>,
}

/// Fetch a branch's objects into the source repository, auth-aware, so an
/// isolated clone of it can simulate locally. Fetching updates the
/// repository's refs, never its checkout.
fn fetch_objects(root: &Path, repo_id: &str, branch: &str) -> Result<()> {
    git(
        root,
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            "origin",
            &format!("+refs/heads/{branch}:refs/knit/landing/sim/{repo_id}/{branch}"),
        ],
    )
    .with_context(|| format!("{repo_id}: fetching branch {branch} from origin failed"))?;
    Ok(())
}

/// Simulate merging `source_sha` into `target_sha` inside an isolated
/// temporary checkout cloned from `root`. Missing objects are fetched into
/// `root` first — through Knit's credential-aware git wrapper — and the
/// simulation itself runs entirely against local objects.
///
/// A failed merge is only reported as a conflict when unmerged paths are
/// actually verified; any other failure — unrelated histories, a missing
/// source object, a broken merge driver — propagates with its original
/// error, and a diff that cannot be read is never treated as success.
pub(crate) fn simulate_merge(
    root: &Path,
    repo_id: &str,
    source_branch: Option<&str>,
    source_sha: &str,
    target_branch: &str,
    target_sha: &str,
) -> Result<Simulation> {
    let missing =
        |object: &str| git(root, &["cat-file", "-e", &format!("{object}^{{commit}}")]).is_err();
    if missing(target_sha) {
        fetch_objects(root, repo_id, target_branch)?;
    }
    if missing(source_sha) {
        if let Some(branch) = source_branch {
            fetch_objects(root, repo_id, branch)?;
        } else {
            git(
                root,
                &["fetch", "--quiet", "--no-tags", "origin", source_sha],
            )
            .with_context(|| format!("{repo_id}: fetching source object {source_sha} failed"))?;
        }
        if missing(source_sha) {
            bail!(
                "{repo_id}: source object {source_sha} is unavailable from origin; the pinned source cannot be simulated"
            );
        }
    }
    let temp = SimulationTemp::new(root)?;
    let tmp = &temp.dir;
    // A plain clone does not carry the repository's merge configuration;
    // transfer the effective merge/diff driver config so the probe predicts
    // the real merge. The sandbox identity only labels an aborted simulation
    // — no commit from here is ever published; real merges run in the
    // repository's own managed worktree with its own configuration.
    transfer_merge_config(root, tmp)?;
    transfer_info_attributes(root, tmp)?;
    let _ = git_best_effort(tmp, &["config", "user.name", "knit-landing-preflight"]);
    let _ = git_best_effort(
        tmp,
        &["config", "user.email", "landing-preflight@knit.invalid"],
    );
    git(tmp, &["checkout", "--quiet", "--detach", target_sha])
        .with_context(|| format!("simulated target {target_sha} unavailable"))?;
    if crate::git::is_ancestor(tmp, source_sha, target_sha) {
        return Ok(Simulation {
            contained: true,
            conflicts: vec![],
        });
    }
    match git(tmp, &["merge", "--no-ff", "--no-commit", source_sha]) {
        Ok(_) => {
            // The merge staged cleanly; verify no unmerged paths remain so a
            // partial driver result is never read as clean.
            let unmerged = unmerged_paths(tmp).with_context(|| {
                format!("merge of {source_sha} into {target_sha} succeeded but its status could not be verified")
            })?;
            if !unmerged.is_empty() {
                let _ = git_best_effort(tmp, &["merge", "--abort"]);
                let _ = git_best_effort(tmp, &["reset", "--quiet", "--hard", target_sha]);
                return Ok(Simulation {
                    contained: false,
                    conflicts: unmerged,
                });
            }
            let _ = git_best_effort(tmp, &["merge", "--abort"]);
            let _ = git_best_effort(tmp, &["reset", "--quiet", "--hard", target_sha]);
            Ok(Simulation {
                contained: false,
                conflicts: vec![],
            })
        }
        Err(merge_error) => {
            let unmerged = unmerged_paths(tmp);
            match unmerged {
                Ok(paths) if !paths.is_empty() => {
                    let _ = git_best_effort(tmp, &["merge", "--abort"]);
                    let _ = git_best_effort(tmp, &["reset", "--quiet", "--hard", target_sha]);
                    Ok(Simulation {
                        contained: false,
                        conflicts: paths,
                    })
                }
                Ok(_) => {
                    let _ = git_best_effort(tmp, &["merge", "--abort"]);
                    let _ = git_best_effort(tmp, &["reset", "--quiet", "--hard", target_sha]);
                    // No verified unmerged paths: this is a real failure
                    // (unrelated histories, missing objects, a failed merge
                    // driver), not a reportable conflict. Fail closed with
                    // the original error.
                    Err(merge_error.context(format!(
                        "simulated merge of {source_sha} into {target_branch} ({target_sha}) failed without reportable conflicts"
                    )))
                }
                Err(_) => Err(merge_error.context(format!(
                    "simulated merge of {source_sha} into {target_branch} ({target_sha}) failed and its conflict status could not be read"
                ))),
            }
        }
    }
}

fn unmerged_paths(dir: &Path) -> Result<Vec<String>> {
    let out = git(dir, &["diff", "--name-only", "--diff-filter=U"])?;
    Ok(out
        .lines()
        .map(str::to_owned)
        .filter(|l| !l.trim().is_empty())
        .collect())
}

struct SimulationTemp {
    dir: PathBuf,
}

/// Copy the source repository's effective merge/diff driver configuration
/// into the simulated clone, so a custom merge driver behaves in the probe as
/// it will in the real merge. The effective view (local, global, system,
/// includes) is transferred through git's own config plumbing; a read or set
/// failure fails the simulation rather than silently certifying the wrong
/// merge behavior.
fn transfer_merge_config(root: &Path, tmp: &Path) -> Result<()> {
    let listing = crate::git::git_output(root, ["config", "--null", "--list"])
        .with_context(|| format!("reading effective git config from {}", root.display()))?;
    for entry in listing.split('\0') {
        if entry.is_empty() {
            continue;
        }
        let Some((key, value)) = entry.split_once('\n') else {
            continue;
        };
        let normalized = key.to_ascii_lowercase();
        let relevant = normalized.starts_with("merge.")
            || normalized.starts_with("diff.")
            || normalized == "core.attributesfile";
        if relevant {
            git(tmp, &["config", "--add", key, value]).with_context(|| {
                format!("transferring merge configuration {key} to the simulated checkout")
            })?;
        }
    }
    Ok(())
}

/// Linked worktrees share the common repository's info/attributes. Resolve
/// through Git rather than assuming that the checkout has a .git directory.
fn transfer_info_attributes(root: &Path, tmp: &Path) -> Result<()> {
    let source = PathBuf::from(git(root, &["rev-parse", "--git-path", "info/attributes"])?);
    let source = if source.is_absolute() {
        source
    } else {
        root.join(source)
    };
    let contents = match std::fs::read(&source) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("reading effective Git info/attributes"),
    };
    let destination = PathBuf::from(git(tmp, &["rev-parse", "--git-path", "info/attributes"])?);
    let destination = if destination.is_absolute() {
        destination
    } else {
        tmp.join(destination)
    };
    std::fs::create_dir_all(destination.parent().context("attributes parent required")?)?;
    std::fs::write(destination, contents)
        .context("copying Git info/attributes into merge simulation")
}

impl SimulationTemp {
    fn new(root: &Path) -> Result<Self> {
        let dir = std::env::temp_dir().join(super::runtime::unique_id("knit-merge-sim"));
        let output = std::process::Command::new("git")
            .args([
                "clone",
                "--quiet",
                "--no-checkout",
                "--no-hardlinks",
                &root.to_string_lossy(),
                &dir.to_string_lossy(),
            ])
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()?;
        if !output.status.success() {
            bail!(
                "failed to create isolated merge simulation checkout: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(Self { dir })
    }
}

impl Drop for SimulationTemp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Run the mergeability preflight over a plan's pending `merge_branch` steps.
///
/// `is_done` reports whether a step already has a successful receipt (on
/// resume, successful merges keep their receipts and are not re-simulated).
/// In [`CheckMode::Apply`] the whole scan is opt-in: plans without a
/// mergeability policy and without integration sources are left alone, and
/// integration sources alone get only their provenance verification. In
/// [`CheckMode::Report`] every pending branch merge is reported, but without
/// a policy the entries are informational — no simulation, and unresolvable
/// tips are recorded rather than failing. Returns the per-step checks;
/// `errors` is nonempty when the whole preflight must fail.
pub(crate) fn mergeability_checks(
    plan: &Value,
    roots: &BTreeMap<String, PathBuf>,
    bundle: &Value,
    is_done: &dyn Fn(&str) -> bool,
    mode: CheckMode,
) -> (Vec<MergeCheck>, Vec<String>) {
    let mut checks = vec![];
    let mut errors = vec![];
    let enabled = mergeability_enabled(plan);
    let integrations = has_integration_sources(plan);
    if mode == CheckMode::Apply && !enabled && !integrations {
        return (checks, errors);
    }
    let steps = match plan["steps"].as_array() {
        Some(steps) => steps.clone(),
        None => return (checks, vec!["plan steps required".to_owned()]),
    };
    for step in &steps {
        if step["type"] != "merge_branch" {
            continue;
        }
        let Some(repo_id) = step["repoId"].as_str() else {
            continue;
        };
        let overridden = plan["integrationSources"][repo_id].is_object();
        if mode == CheckMode::Apply && !enabled && !overridden {
            // No mergeability policy declared: repositories without an
            // integration source are left entirely alone — no target scans,
            // no pins. The opt-in stays opt-in.
            continue;
        }
        let id = step["id"].as_str().unwrap_or("");
        if is_done(id) {
            continue;
        }
        if mode == CheckMode::Report && !enabled && !overridden {
            // Reporting an unconfigured merge is informational even when the
            // caller supplied no checkout bindings. Structural validation
            // reports malformed plans separately from live readiness.
            let (source_branch, source_sha) =
                merge_source(plan, bundle, repo_id).unwrap_or_default();
            let target_branch = step["targetBranch"].as_str().unwrap_or("");
            let target_sha = roots
                .get(repo_id)
                .and_then(|root| target_tip(root, target_branch).ok().flatten());
            checks.push(MergeCheck {
                step_id: id.to_owned(),
                repo_id: repo_id.to_owned(),
                source_branch,
                source_sha,
                target_branch: target_branch.to_owned(),
                target_sha,
                status: "not_required".to_owned(),
                conflicts: vec![],
                error: None,
            });
            continue;
        }
        let Some(target_branch) = step["targetBranch"].as_str() else {
            errors.push(format!("{id}: merge_branch requires targetBranch"));
            continue;
        };
        let Some(root) = roots.get(repo_id) else {
            errors.push(format!(
                "{id}: mergeability preflight requires a repo-root binding for {repo_id}"
            ));
            continue;
        };
        let Some((source_branch, source_sha)) = merge_source(plan, bundle, repo_id) else {
            errors.push(format!("{id}: no pinned source for {repo_id}"));
            continue;
        };
        let mut check = MergeCheck {
            step_id: id.to_owned(),
            repo_id: repo_id.to_owned(),
            source_branch: source_branch.clone(),
            source_sha: source_sha.clone(),
            target_branch: target_branch.to_owned(),
            target_sha: None,
            status: "pending".to_owned(),
            conflicts: vec![],
            error: None,
        };
        // Integration source provenance is always verified live, whatever the
        // mergeability policy: the override must stand on its own.
        if plan["integrationSources"][repo_id].is_object() {
            if let (Some(branch), Some(reviewed)) = (
                source_branch.as_deref(),
                plan["bundleHeads"][repo_id].as_str(),
            ) {
                if let Err(error) =
                    verify_integration_source(root, repo_id, branch, &source_sha, reviewed)
                {
                    check.status = "source_drift".to_owned();
                    check.error = Some(format!("{error:#}"));
                    errors.push(format!("{error:#}"));
                    checks.push(check);
                    continue;
                }
            }
        }
        if !enabled {
            // No policy declared. In report mode the entry is informational —
            // resolve what is resolvable, require nothing. In apply mode only
            // integration-source repositories reach this point, and their
            // provenance was just verified above; no target is scanned or
            // pinned for them.
            if mode == CheckMode::Report {
                check.target_sha = target_tip(root, target_branch).ok().flatten();
            }
            check.status = "not_required".to_owned();
            checks.push(check);
            continue;
        }
        let target_sha = match target_tip(root, target_branch) {
            Ok(Some(sha)) => sha,
            Ok(None) => {
                errors.push(format!(
                    "{id}: target branch {target_branch} does not exist on origin for {repo_id}"
                ));
                check.status = "missing_target".to_owned();
                checks.push(check);
                continue;
            }
            Err(error) => {
                errors.push(format!("{id}: failed to resolve target tip: {error:#}"));
                check.status = "error".to_owned();
                check.error = Some(format!("{error:#}"));
                checks.push(check);
                continue;
            }
        };
        check.target_sha = Some(target_sha.clone());
        match simulate_merge(
            root,
            repo_id,
            source_branch.as_deref(),
            &source_sha,
            target_branch,
            &target_sha,
        ) {
            Ok(result) if result.contained => {
                check.status = "already_contained".to_owned();
            }
            Ok(result) if result.conflicts.is_empty() => {
                check.status = "mergeable".to_owned();
            }
            Ok(result) => {
                check.status = "conflict".to_owned();
                check.conflicts = result
                    .conflicts
                    .iter()
                    .map(|file| {
                        json!({
                            "repo": repo_id,
                            "source": source_sha,
                            "target": target_sha,
                            "file": file,
                        })
                    })
                    .collect();
                errors.push(format!(
                    "{id}: {repo_id} source {source_sha} conflicts with {target_branch} ({target_sha}): {}",
                    result.conflicts.join(", ")
                ));
            }
            Err(error) => {
                check.status = "error".to_owned();
                check.error = Some(format!("{error:#}"));
                errors.push(format!("{id}: merge simulation failed: {error:#}"));
            }
        }
        checks.push(check);
    }
    (checks, errors)
}

pub(crate) fn checks_to_json(checks: &[MergeCheck]) -> Vec<Value> {
    checks.iter().map(MergeCheck::to_json).collect()
}

#[cfg(test)]
mod report_tests {
    use super::*;

    fn plan() -> Value {
        json!({
            "bundleHeads": {"api": "a".repeat(40)},
            "steps": [{"id": "merge-api", "type": "merge_branch", "repoId": "api", "targetBranch": "staging"}]
        })
    }

    #[test]
    fn report_without_policy_or_pin_needs_no_root() {
        let (checks, errors) = mergeability_checks(
            &plan(),
            &BTreeMap::new(),
            &json!({}),
            &|_| false,
            CheckMode::Report,
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, "not_required");
        assert_eq!(checks[0].source_sha, "a".repeat(40));
        assert_eq!(checks[0].target_branch, "staging");
        assert!(checks[0].target_sha.is_none());
        assert!(checks[0].error.is_none());
    }

    #[test]
    fn report_with_policy_or_pin_still_requires_root() {
        for policy in ["preflight", "integrationSources"] {
            let mut plan = plan();
            plan[policy] = if policy == "preflight" {
                json!({"mergeability": "all"})
            } else {
                json!({"api": {"branch": "compatibility", "sha": "b".repeat(40)}})
            };
            for mode in [CheckMode::Report, CheckMode::Apply] {
                let (_, errors) =
                    mergeability_checks(&plan, &BTreeMap::new(), &json!({}), &|_| false, mode);
                assert_eq!(errors.len(), 1, "{policy}: {errors:?}");
                assert!(errors[0].contains("repo-root binding for api"));
            }
        }
    }

    #[test]
    fn informational_report_skips_completed_merges_and_apply_stays_opt_in() {
        for (mode, done) in [(CheckMode::Report, true), (CheckMode::Apply, false)] {
            let (checks, errors) =
                mergeability_checks(&plan(), &BTreeMap::new(), &json!({}), &|_| done, mode);
            assert!(checks.is_empty());
            assert!(errors.is_empty());
        }
    }
}
