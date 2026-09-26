//! Opt-in execution in an attached branch of a trusted runner/source checkout.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub(super) const CAPABILITY: &str = "branch-checkout";
type Roots = BTreeMap<String, PathBuf>;

pub(super) fn enabled(step: &Value) -> bool {
    step["checkout"]["mode"] == "branch"
}

/// An attached branch merge is a reusable branch workflow. Explicit integration
/// sources and other checkout modes retain their reviewed source pins.
pub(super) fn live_sources(plan: &Value) -> std::collections::BTreeSet<String> {
    plan["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| enabled(s) && s["type"] == "merge_branch")
        .filter_map(|s| s["repoId"].as_str())
        .filter(|id| !plan["integrationSources"][*id].is_object())
        .map(str::to_owned)
        .collect()
}

/// Compare the stable source identity using the original heads only for live
/// branch sources. Repository, branch, review, base and scope changes still fail.
pub(super) fn reviewed_identity(plan: &Value, bundle: &Value) -> Value {
    let live = live_sources(plan);
    let mut result = bundle.clone();
    if let Some(repos) = result["repos"].as_array_mut() {
        for repo in repos {
            if let Some(id) = repo["id"].as_str().filter(|id| live.contains(*id)) {
                let original = plan["bundleHeads"][id].clone();
                repo["headSha"] = original;
            }
        }
    }
    result
}

/// Resolve branch tips once for each fresh application. The authored plan is
/// unchanged; the execution copy and receipt carry what this run actually uses.
/// Continuations use their recorded sources, never newly fetched feature work.
pub(super) fn execution_sources(
    plan: &Value,
    bundle: &Value,
    roots: &Roots,
    prior: Option<&Value>,
) -> Result<(Value, Value)> {
    let mut execution = plan.clone();
    let mut sources = serde_json::Map::new();
    for id in live_sources(plan) {
        let branch = bundle["repos"]
            .as_array()
            .and_then(|repos| repos.iter().find(|r| r["id"] == id))
            .and_then(|r| r["featureBranch"].as_str())
            .context("branch merge requires a feature branch")?;
        let root = roots
            .get(&id)
            .context("branch source requires a repo-root binding")?;
        git(root, &["check-ref-format", &format!("refs/heads/{branch}")])?;
        let sha = if let Some(run) = prior {
            run["sourceBundle"]["repos"]
                .as_array()
                .and_then(|repos| repos.iter().find(|r| r["id"] == id))
                .and_then(|repo| repo["headSha"].as_str())
                .context("recorded source revision missing")?
                .to_owned()
        } else {
            git(
                root,
                &[
                    "fetch",
                    "--no-tags",
                    "origin",
                    &format!("refs/heads/{branch}"),
                ],
            )?;
            git(root, &["rev-parse", "FETCH_HEAD^{commit}"])?
        };
        execution["bundleHeads"][&id] = json!(sha);
        sources.insert(id, json!({"branch":branch,"sha":sha}));
    }
    Ok((execution, Value::Object(sources)))
}

pub(super) fn source_snapshot(bundle: &Value, sources: &Value) -> Value {
    let mut snapshot = bundle.clone();
    if let Some(repos) = snapshot["repos"].as_array_mut() {
        for repo in repos {
            if let Some(sha) = repo["id"]
                .as_str()
                .and_then(|id| sources[id]["sha"].as_str())
            {
                repo["headSha"] = json!(sha);
            }
        }
    }
    snapshot
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    Ok(crate::git::git_output(root, args)?.trim().to_owned())
}

/// Carry the recipe's explicit checkout contract onto its branch merge.
pub(super) fn configure(plan: &mut Value) -> Result<()> {
    let steps = plan["steps"].as_array_mut().context("steps required")?;
    let mut configurations = BTreeMap::new();
    for step in steps.iter().filter(|s| enabled(s)) {
        let repo = step["repoId"]
            .as_str()
            .context("branch checkout requires repoId")?;
        if let Some(previous) = configurations.insert(repo.to_owned(), step["checkout"].clone()) {
            if previous != step["checkout"] {
                bail!("{repo}: conflicting branch checkout configurations");
            }
        }
    }
    if configurations.is_empty() {
        return Ok(());
    }
    for step in steps {
        if step["type"] == "merge_branch" {
            if let Some(checkout) = step["repoId"].as_str().and_then(|r| configurations.get(r)) {
                step["checkout"] = checkout.clone();
            }
        }
    }
    plan["requiredExecutorVersion"] = json!("0.5");
    let mut caps = super::graph::strings(&plan["requiredCapabilities"]);
    caps.push(CAPABILITY.into());
    caps.sort();
    caps.dedup();
    plan["requiredCapabilities"] = json!(caps);
    Ok(())
}

fn depends_on(step: &Value, other: &Value, steps: &[Value]) -> bool {
    let mut pending = super::graph::strings(&step["needs"]);
    let mut seen = std::collections::BTreeSet::new();
    while let Some(id) = pending.pop() {
        if other["id"] == id {
            return true;
        }
        if seen.insert(id.clone()) {
            if let Some(s) = steps.iter().find(|s| s["id"] == id) {
                pending.extend(super::graph::strings(&s["needs"]));
            }
        }
    }
    false
}

pub(super) fn validate(plan: &Value, steps: &[Value]) -> Result<()> {
    for step in steps {
        if let Some(mode) = step["checkout"].get("mode") {
            if mode != "branch" {
                bail!("unsupported checkout.mode; omit it for isolated execution or use branch");
            }
        }
        if !enabled(step) {
            continue;
        }
        if plan["schemaVersion"] != "0.2"
            || plan["requiredExecutorVersion"] != "0.5"
            || !super::graph::strings(&plan["requiredCapabilities"]).contains(&CAPABILITY.into())
        {
            bail!("branch checkout requires schema 0.2, requiredExecutorVersion 0.5 and branch-checkout capability");
        }
        let repo = step["repoId"]
            .as_str()
            .context("branch checkout requires repoId")?;
        let branch = step["checkout"]["branch"]
            .as_str()
            .context("checkout branch required")?;
        if branch.is_empty() || branch.starts_with('-') || branch == "HEAD" {
            bail!("invalid checkout branch");
        }
        if let Some(remote) = step["checkout"].get("remote") {
            if remote
                .as_str()
                .is_none_or(|r| r.is_empty() || r.starts_with('-'))
            {
                bail!("invalid checkout remote");
            }
        }
        if let Some(update) = step["checkout"].get("update") {
            if !matches!(update.as_str(), Some("fetch" | "pull" | "none")) {
                bail!("invalid checkout update");
            }
        }
        if !matches!(
            step["type"].as_str(),
            Some("merge_branch" | "run" | "deploy")
        ) {
            bail!("branch checkout supports branch merges and commands");
        }
        for other in steps
            .iter()
            .filter(|s| s["repoId"] == repo && s["id"] != step["id"])
        {
            if other["type"] == "merge_pr" {
                bail!("{repo}: branch checkout requires a branch merge, not a review merge");
            }
            if other["type"] == "merge_branch" {
                if !enabled(other) || other["targetBranch"] != branch {
                    bail!("{repo}: branch merge and command checkout must use the same branch mode and target");
                }
                if step["type"] != "merge_branch" && !depends_on(step, other, steps) {
                    bail!("{repo}: branch command must depend on its merge");
                }
            }
            if enabled(other)
                && (other["checkout"] != step["checkout"]
                    || (!depends_on(step, other, steps) && !depends_on(other, step, steps)))
            {
                bail!("{repo}: branch checkout steps must share a configuration and execute sequentially");
            }
        }
        if step["type"] == "merge_branch"
            && (step["targetBranch"] != branch
                || step["checkout"]["remote"].as_str().unwrap_or("origin") != "origin"
                || step["checkout"]["update"].as_str().unwrap_or("pull") != "pull")
        {
            bail!("{repo}: branch merge checkout must pull its target from origin");
        }
    }
    Ok(())
}

/// Only local workspace registration can override the usual feature roots.
/// Artifact execution keeps the explicit runner-owned --repo-roots bindings.
pub(super) fn local_roots(
    plan: &Value,
    project: &Value,
    active: &crate::store::ActiveBundle,
    roots: &mut Roots,
) -> Result<()> {
    for step in plan["steps"]
        .as_array()
        .context("steps required")?
        .iter()
        .filter(|s| enabled(s))
    {
        let repo = step["repoId"]
            .as_str()
            .context("branch checkout requires repoId")?;
        let path = project["repos"]
            .as_array()
            .and_then(|rs| rs.iter().find(|r| r["id"] == repo))
            .and_then(|r| r["path"].as_str())
            .context("branch checkout requires a registered project source checkout")?;
        let path = PathBuf::from(path);
        roots.insert(
            repo.into(),
            dunce::canonicalize(if path.is_absolute() {
                path
            } else {
                active.root.join(path)
            })?,
        );
    }
    Ok(())
}

fn clean(root: &Path) -> Result<()> {
    if dunce::canonicalize(git(root, &["rev-parse", "--show-toplevel"])?)?
        != dunce::canonicalize(root)?
    {
        bail!("branch checkout binding must name the repository root");
    }
    if !git(root, &["status", "--porcelain", "--untracked-files=all"])?.is_empty() {
        bail!(
            "branch checkout is dirty at {}; commit or stash changes first",
            root.display()
        );
    }
    for state in [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-merge",
        "rebase-apply",
    ] {
        let path = PathBuf::from(git(root, &["rev-parse", "--git-path", state])?);
        if (if path.is_absolute() {
            path
        } else {
            root.join(path)
        })
        .exists()
        {
            bail!(
                "branch checkout has an unfinished Git operation at {}",
                root.display()
            );
        }
    }
    Ok(())
}

/// Fetching is allowed here; no checkout, local branch or remote is changed.
fn inspect(root: &Path, checkout: &Value) -> Result<String> {
    clean(root)?;
    let branch = checkout["branch"]
        .as_str()
        .context("checkout branch required")?;
    git(root, &["check-ref-format", &format!("refs/heads/{branch}")])?;
    if let Some(held) = crate::git::branch_checkout_path(root, branch)? {
        if dunce::canonicalize(held)? != dunce::canonicalize(root)? {
            bail!("branch {branch} is checked out elsewhere");
        }
    }
    let local = crate::git::ref_commit_sha(root, &format!("refs/heads/{branch}"))?;
    if checkout["update"] == "none" {
        return local.context("checkout update none requires an existing local branch");
    }
    let remote = checkout["remote"].as_str().unwrap_or("origin");
    git(
        root,
        &[
            "fetch",
            "--no-tags",
            remote,
            &format!("refs/heads/{branch}"),
        ],
    )?;
    let tip = git(root, &["rev-parse", "FETCH_HEAD^{commit}"])?;
    if let Some(local) = local {
        if !crate::git::is_ancestor(root, &local, &tip) {
            bail!("branch {branch} has local commits not in {remote}/{branch}; reconcile them before landing");
        }
    }
    Ok(tip)
}

pub(super) fn preflight(steps: &[Value], roots: &Roots, bundle: &Value) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for step in steps.iter().filter(|s| enabled(s)) {
        let repo = step["repoId"]
            .as_str()
            .context("branch checkout requires repoId")?;
        if !seen.insert(repo) {
            continue;
        }
        let root = roots.get(repo).context("branch checkout binding missing")?;
        if let Some(feature) = bundle["repos"]
            .as_array()
            .and_then(|rs| rs.iter().find(|r| r["id"] == repo))
            .and_then(|r| r["featureBranch"].as_str())
        {
            if step["checkout"]["branch"] == feature
                || git(root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
                    .ok()
                    .as_deref()
                    == Some(feature)
            {
                bail!("{repo}: branch checkout must use a source checkout separate from the feature branch");
            }
        }
        inspect(root, &step["checkout"])?;
    }
    Ok(())
}

pub(super) fn prepare(root: &Path, checkout: &Value, expected: Option<&str>) -> Result<String> {
    let tip = inspect(root, checkout)?;
    if expected.is_some_and(|e| e != tip) {
        bail!("target branch drifted since landing preflight; refusing to merge");
    }
    let branch = checkout["branch"]
        .as_str()
        .context("checkout branch required")?;
    if crate::git::ref_commit_sha(root, &format!("refs/heads/{branch}"))?.is_none() {
        git(root, &["checkout", "-b", branch, &tip])?;
        let remote = checkout["remote"].as_str().unwrap_or("origin");
        // Leave a normal tracking branch behind, so the user's subsequent
        // plain `git pull` works too, including with custom fetch refspecs.
        git(
            root,
            &["config", &format!("branch.{branch}.remote"), remote],
        )?;
        git(
            root,
            &[
                "config",
                &format!("branch.{branch}.merge"),
                &format!("refs/heads/{branch}"),
            ],
        )?;
    } else {
        git(root, &["checkout", branch])?;
        if checkout["update"].as_str().unwrap_or("pull") == "pull" {
            git(root, &["merge", "--ff-only", &tip])?;
        }
    }
    git(root, &["rev-parse", "HEAD"])
}

pub(super) fn merge(
    root: &Path,
    step: &Value,
    source: &str,
    source_branch: Option<&str>,
    expected: Option<&str>,
) -> Result<crate::commands::merge::BranchMergeOutcome> {
    let before = prepare(root, &step["checkout"], expected)
        .map_err(|e| super::KnownNoEffect(format!("{e:#}")))?;
    if crate::git::is_ancestor(root, source, &before) {
        return Ok(crate::commands::merge::BranchMergeOutcome {
            after_sha: before,
            merged: false,
        });
    }
    // On failure leave normal Git state available for inspection; never reset
    // or discard changes in the user's source checkout.
    let branch = step["targetBranch"]
        .as_str()
        .context("branch target required")?;
    let name = source_branch.context("branch merge requires the source branch name")?;
    let message = crate::git::branch_merge_message(root, source, name, branch)?;
    git(
        root,
        &[
            "merge",
            "--no-ff",
            "--no-edit",
            "--message",
            &message,
            source,
        ],
    )?;
    let after = git(root, &["rev-parse", "HEAD"])?;
    if !crate::git::is_ancestor(root, &before, &after) {
        bail!("merge no longer descends from fetched target; refusing to push");
    }
    // Conditional fast-forward: the ancestry check above prevents rewrites,
    // while the lease also refuses a target rewind between fetch and push.
    git(
        root,
        &[
            "push",
            &format!("--force-with-lease=refs/heads/{branch}:{before}"),
            "origin",
            &format!("HEAD:refs/heads/{branch}"),
        ],
    )?;
    Ok(crate::commands::merge::BranchMergeOutcome {
        after_sha: after,
        merged: true,
    })
}

pub(super) fn command_revision(
    root: &Path,
    step: &Value,
    phase: &str,
    recorded: Option<&str>,
) -> Result<String> {
    if matches!(phase, "forward" | "capture") {
        return prepare(root, &step["checkout"], None);
    }
    // Recovery must never rewind or refresh a mutable user branch implicitly.
    let branch = git(root, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
    let revision = git(root, &["rev-parse", "HEAD"])?;
    if step["checkout"]["branch"] != branch || recorded != Some(revision.as_str()) {
        bail!("branch checkout changed since the command; reconcile it before recovery");
    }
    Ok(revision)
}
