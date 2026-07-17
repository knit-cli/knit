//! `knit tag` — cross-repo known-good markers on origin base branches.
//!
//! A tag records the post-land state of the mains: per repo, the commit
//! `origin/<base_branch>` points at after a fresh fetch, named as one set. The
//! `tag.created` ledger node is the source of truth; annotated git tags
//! `knit/<name>` in each repo are a default-on export of it (host/CI
//! visibility, checkout-without-knit, GC protection of the pinned commits).
//!
//! The tag claims exactly what Knit can prove: "at tag time these were the
//! mains, named as one set, with this evidence" — the evidence being recorded
//! feature-branch checks plus best-effort host CI verdicts for the tagged
//! commits themselves. It never claims "tested"; red or missing evidence is a
//! recorded warning, not an error. Tagging is deliberately decoupled from
//! landing: land, verify main however you trust, then tag. Tags are immutable
//! — re-running `knit tag <name>` on the same bundle resumes an interrupted
//! tag set instead of moving it.

use crate::git::{
    git_output, git_output_optional, is_ancestor, ref_commit_sha, ref_exists, remote_ref_sha,
};
use crate::ids::{expand_repo_selectors, node_id, short_sha};
use crate::model::{BundleNode, ChangeGroup, CommitRef};
use crate::output as out;
use crate::providers::{self, github, CheckRun, PrTarget};
use crate::repo_selectors::resolve_repo_indexes;
use crate::store::{
    find_knit_root, load_active_bundle, load_active_bundle_for_update, load_config, read_json,
    save_active_bundle, ActiveBundle, BundleResolutionSource,
};
use crate::time::now_iso;
use crate::tracking::latest_recorded_head_sha;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

/// Local tag name (`knit/<name>`), the form `git tag` creates and lists.
fn local_tag_name(name: &str) -> String {
    format!("knit/{name}")
}

/// Fully qualified ref. Feature branches live at `refs/heads/knit/<slug>`, so
/// a short `knit/<name>` is ambiguous in ref lookups — always qualify.
fn tag_ref(name: &str) -> String {
    format!("refs/tags/knit/{name}")
}

struct TagTarget {
    repo_id: String,
    path: PathBuf,
    base_branch: String,
}

/// `knit tag <name>`: pin the freshly fetched origin bases of the resolved
/// bundle's repos, record a `tag.created` node, and export git tags.
pub fn create_tag(
    name: &str,
    selectors: &[String],
    note: Option<&str>,
    no_push: bool,
    no_git: bool,
) -> Result<()> {
    let mut active = load_active_bundle_for_update()?;
    create_tag_on_active(&mut active, name, selectors, note, no_push, no_git, &[], false)
}

/// The create/resume entry point for an already-resolved, write-locked bundle.
/// `knit tag` loads the bundle then calls this; `knit land apply` reuses it to
/// tag the state it just landed without re-resolving the archived bundle.
/// `remote`/`no_remote` control the KnitHub artifact sync after a push, so a
/// land run's explicit `--no-remote` carries into its tag.
#[allow(clippy::too_many_arguments)]
pub(crate) fn create_tag_on_active(
    active: &mut ActiveBundle,
    name: &str,
    selectors: &[String],
    note: Option<&str>,
    no_push: bool,
    no_git: bool,
    remote: &[String],
    no_remote: bool,
) -> Result<()> {
    validate_tag_name(name)?;
    if active.bundle.repos.is_empty() {
        bail!("The resolved bundle has no repos. Run `knit bundle add <repo-path>` first.");
    }

    if let Some(node) = find_tag_node(&active.bundle, name) {
        if note.is_some() || !selectors.is_empty() {
            println!(
                "{}",
                out::muted(format!(
                    "tag `{}` is already recorded on this bundle — resuming; -m/--repo are ignored.",
                    local_tag_name(name)
                ))
            );
        }
        let node = node.clone();
        return resume_tag_set(active, &node, name, no_push, remote, no_remote);
    }

    let indexes = resolve_repo_indexes(active, &expand_repo_selectors(selectors), false)?;
    create_tag_set_on(active, name, &indexes, note, no_push, no_git, remote, no_remote)
}

/// The create core, operating on an already-loaded bundle.
#[allow(clippy::too_many_arguments)]
pub(crate) fn create_tag_set_on(
    active: &mut ActiveBundle,
    name: &str,
    indexes: &[usize],
    note: Option<&str>,
    no_push: bool,
    no_git: bool,
    remote: &[String],
    no_remote: bool,
) -> Result<()> {
    let partial = indexes.len() < active.bundle.repos.len();
    if partial {
        println!(
            "{}",
            out::muted("note: tagging a subset — a partial set weakens the known-good claim.")
        );
    }

    let targets = targets_for(active, indexes)?;
    let pins = fetch_pins(&targets)?;
    if !no_git {
        preflight_collisions(&targets, name)?;
    }

    let evidence = collect_ci_evidence(active, indexes, &targets, &pins);
    let message = build_annotation(active, name, note, indexes, &targets, &pins, &evidence);
    print_honesty_warnings(active, indexes, &pins, &evidence);

    if !no_git {
        create_local_tags(&targets, &pins, name, &message)?;
    }

    let commit_refs: Vec<CommitRef> = pins
        .iter()
        .map(|(repo_id, sha)| CommitRef {
            repo_id: repo_id.clone(),
            sha: sha.clone(),
        })
        .collect();
    let node = BundleNode::tag_created(
        node_id("tag"),
        now_iso(),
        name.to_string(),
        message,
        commit_refs,
    );
    let node_ref = node.id.clone();
    active.bundle.nodes.push(node);
    active.bundle.head_node_id = active.bundle.nodes.last().map(|node| node.id.clone());
    active.bundle.updated_at = now_iso();
    save_active_bundle(active)?;

    println!(
        "{} {} {}",
        out::heading("Tag:"),
        out::branch(local_tag_name(name)),
        out::node(&node_ref)
    );
    let movement = if no_git { "recorded" } else { "tagged" };
    for (repo_id, sha) in &pins {
        println!(
            "  {} {} {}",
            out::repo(repo_id),
            out::movement(movement),
            out::sha(short_sha(sha))
        );
    }

    if no_git || no_push {
        if !no_git {
            crate::advice::print(
                &active.root,
                format!("push the tags later by re-running `knit tag {name}`."),
            );
        }
        return Ok(());
    }

    push_tags(&targets, name)?;
    super::remote::maybe_sync_bundle_to_remote(remote, no_remote)?;
    Ok(())
}

/// Resume an interrupted tag set from its ledger node: recreate missing local
/// tags at the recorded pins, verify existing ones, push only where origin
/// lacks the tag. Never appends a second node.
fn resume_tag_set(
    active: &ActiveBundle,
    node: &BundleNode,
    name: &str,
    no_push: bool,
    remote: &[String],
    no_remote: bool,
) -> Result<()> {
    let message = node.message.as_deref().unwrap_or("");
    let mut pushed_any = false;

    for pin in &node.commits {
        let Some(repo) = active.bundle.repos.iter().find(|repo| repo.id == pin.repo_id) else {
            bail!(
                "{}: pinned by tag `{}` but no longer tracked in this bundle.",
                pin.repo_id,
                local_tag_name(name)
            );
        };
        let path = PathBuf::from(&repo.path);
        if !path.exists() {
            bail!("{}: original repo path does not exist: {}", repo.id, path.display());
        }

        match ref_commit_sha(&path, &tag_ref(name))? {
            None => {
                git_output(
                    &path,
                    [
                        OsString::from("tag"),
                        OsString::from("-a"),
                        OsString::from(local_tag_name(name)),
                        OsString::from(&pin.sha),
                        OsString::from("-m"),
                        OsString::from(message),
                    ],
                )
                .with_context(|| format!("{}: failed to recreate tag", repo.id))?;
                println!(
                    "{}: {} {}",
                    out::repo(&repo.id),
                    out::movement("recreated"),
                    out::sha(short_sha(&pin.sha))
                );
            }
            Some(sha) if sha != pin.sha => bail!(
                "{}: local tag `{}` points at {}, but the ledger pinned {}. Tags are immutable; \
                 delete the tag manually only if you know it is wrong.",
                repo.id,
                local_tag_name(name),
                short_sha(&sha),
                short_sha(&pin.sha)
            ),
            Some(_) => {}
        }

        if no_push {
            continue;
        }
        match remote_ref_sha(&path, "origin", &tag_ref(name))? {
            None => {
                git_output(
                    &path,
                    [OsString::from("push"), OsString::from("origin"), OsString::from(tag_ref(name))],
                )
                .with_context(|| format!("{}: failed to push tag", repo.id))?;
                pushed_any = true;
                println!(
                    "{}: {} {}",
                    out::repo(&repo.id),
                    out::movement("pushed"),
                    out::branch(local_tag_name(name))
                );
            }
            Some(sha) if sha != pin.sha => bail!(
                "{}: origin tag `{}` points at {}, but the ledger pinned {}. Tags are immutable; \
                 delete the remote tag manually only if you know it is wrong.",
                repo.id,
                local_tag_name(name),
                short_sha(&sha),
                short_sha(&pin.sha)
            ),
            Some(_) => {
                println!("{}: {}", out::repo(&repo.id), out::muted("up to date"));
            }
        }
    }

    if pushed_any {
        super::remote::maybe_sync_bundle_to_remote(remote, no_remote)?;
    }
    Ok(())
}

/// `knit tag` / `knit tag list`: the union of `knit/*` tags across repos,
/// marking partial sets.
pub fn list_tags() -> Result<()> {
    let (_root, targets) = resolve_read_targets()?;
    let mut tags: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (repo_id, path) in &targets {
        if !path.exists() {
            continue;
        }
        let Some(output) = git_output_optional(path, ["tag", "--list", "knit/*"])? else {
            continue;
        };
        for line in output.lines() {
            let tag = line.trim();
            if !tag.is_empty() {
                tags.entry(tag.to_string()).or_default().insert(repo_id.clone());
            }
        }
    }

    if tags.is_empty() {
        println!("{}", out::muted("No knit/* tags found."));
        return Ok(());
    }

    let width = tags.keys().map(|tag| tag.len()).max().unwrap_or(3).max(3);
    println!("{}  {}", out::header_field("tag", width), out::heading("repos"));
    for (tag, repos) in &tags {
        let missing: Vec<&str> = targets
            .iter()
            .map(|(repo_id, _)| repo_id.as_str())
            .filter(|repo_id| !repos.contains(*repo_id))
            .collect();
        let coverage = format!("{}/{}", repos.len(), targets.len());
        if missing.is_empty() {
            println!("{}  {}", out::repo_field(tag, width), coverage);
        } else {
            println!(
                "{}  {}   {}",
                out::repo_field(tag, width),
                coverage,
                out::warn(format!("partial (missing: {})", missing.join(", ")))
            );
        }
    }
    Ok(())
}

/// `knit tag show <name>`: per-repo local/remote SHAs, the annotation
/// subject, and ledger provenance.
pub fn show_tag(name: &str) -> Result<()> {
    let (root, targets) = resolve_read_targets()?;
    let mut found_git = false;
    let mut subject: Option<String> = None;

    println!("{} {}", out::heading("Tag:"), out::branch(local_tag_name(name)));
    for (repo_id, path) in &targets {
        if !path.exists() {
            println!("  {} {}", out::repo(repo_id), out::muted("(missing path)"));
            continue;
        }
        let local = ref_commit_sha(path, &tag_ref(name))?;
        let remote = match remote_ref_sha(path, "origin", &tag_ref(name)) {
            Ok(remote) => remote
                .map(|sha| short_sha(&sha))
                .unwrap_or_else(|| "-".to_string()),
            Err(_) => "(unreachable)".to_string(),
        };
        if local.is_some() {
            found_git = true;
            if subject.is_none() {
                subject = git_output_optional(
                    path,
                    ["tag", "--list", "--format=%(subject)", &local_tag_name(name)],
                )?;
            }
        }
        println!(
            "  {} local {} remote {}",
            out::repo(repo_id),
            out::sha(
                local
                    .map(|sha| short_sha(&sha))
                    .unwrap_or_else(|| "-".to_string())
            ),
            out::sha(remote)
        );
    }
    if let Some(subject) = subject {
        println!("{} {}", out::heading("Subject:"), subject);
    }

    let provenance = scan_tag_provenance(&root, name)?;
    for (bundle_id, node_ref, message) in &provenance {
        println!("{} {}", out::heading("Bundle:"), out::repo(bundle_id));
        println!("{} {}", out::heading("Node:"), out::node(node_ref));
        for line in message.lines() {
            println!("  {}", out::muted(line));
        }
    }

    if !found_git && provenance.is_empty() {
        bail!(
            "No tag `{}` found in any repo or bundle ledger.",
            local_tag_name(name)
        );
    }
    Ok(())
}

fn validate_tag_name(name: &str) -> Result<()> {
    let valid_chars = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    let starts_alphanumeric = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric());
    if !valid_chars
        || !starts_alphanumeric
        || name.contains("..")
        || name.ends_with(".lock")
        || matches!(name, "list" | "show")
    {
        bail!(
            "Tag name `{name}` is invalid: use letters, digits, `.`, `_`, `-`; start with a \
             letter or digit; no `..`, no trailing `.lock`, and not `list`/`show`."
        );
    }
    Ok(())
}

fn find_tag_node<'a>(bundle: &'a ChangeGroup, name: &str) -> Option<&'a BundleNode> {
    bundle
        .nodes
        .iter()
        .rev()
        .find(|node| node.node_type == "tag.created" && node.title.as_deref() == Some(name))
}

fn targets_for(active: &ActiveBundle, indexes: &[usize]) -> Result<Vec<TagTarget>> {
    let mut targets = Vec::new();
    let mut failures = Vec::new();
    for &index in indexes {
        let repo = &active.bundle.repos[index];
        let path = PathBuf::from(&repo.path);
        if !path.exists() {
            failures.push(format!(
                "{}: original repo path does not exist: {}",
                repo.id,
                path.display()
            ));
            continue;
        }
        if git_output_optional(&path, ["remote", "get-url", "origin"])?.is_none() {
            failures.push(format!(
                "{}: no `origin` remote configured in {}",
                repo.id,
                path.display()
            ));
            continue;
        }
        targets.push(TagTarget {
            repo_id: repo.id.clone(),
            path,
            base_branch: repo.base_branch.clone(),
        });
    }
    if !failures.is_empty() {
        bail!("tag preflight failed:\n{}", failures.join("\n"));
    }
    Ok(targets)
}

/// Fetch every target's origin and pin `origin/<base_branch>`. All repos are
/// fetched before anything is created, so the set is internally consistent
/// even when origin moves mid-run.
fn fetch_pins(targets: &[TagTarget]) -> Result<Vec<(String, String)>> {
    let results: Vec<(String, Result<String>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = targets
            .iter()
            .map(|target| scope.spawn(move || (target.repo_id.clone(), fetch_pin(target))))
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("tag fetch worker thread panicked"))
            .collect()
    });

    let mut pins = Vec::new();
    let mut failures = Vec::new();
    for (repo_id, result) in results {
        match result {
            Ok(sha) => pins.push((repo_id, sha)),
            Err(error) => failures.push(format!("{repo_id}: {error:#}")),
        }
    }
    if !failures.is_empty() {
        bail!("tag fetch failed:\n{}", failures.join("\n"));
    }
    Ok(pins)
}

fn fetch_pin(target: &TagTarget) -> Result<String> {
    git_output(&target.path, ["fetch", "origin"])?;
    let remote_ref = format!("origin/{}", target.base_branch);
    ref_commit_sha(&target.path, &remote_ref)?
        .with_context(|| format!("no `{remote_ref}` after fetch"))
}

/// Refuse to reuse a name that exists anywhere, locally or on origin. Tags
/// are immutable and there is deliberately no `--force`.
fn preflight_collisions(targets: &[TagTarget], name: &str) -> Result<()> {
    let results: Vec<(String, Result<Vec<String>>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = targets
            .iter()
            .map(|target| {
                scope.spawn(move || {
                    (target.repo_id.clone(), preflight_collision(target, name))
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("tag preflight worker thread panicked"))
            .collect()
    });

    let mut collisions = Vec::new();
    let mut failures = Vec::new();
    for (repo_id, result) in results {
        match result {
            Ok(hits) => collisions.extend(hits.into_iter().map(|hit| format!("{repo_id} ({hit})"))),
            Err(error) => failures.push(format!("{repo_id}: {error:#}")),
        }
    }
    if !failures.is_empty() {
        bail!("tag preflight failed:\n{}", failures.join("\n"));
    }
    if !collisions.is_empty() {
        bail!(
            "tag `{}` already exists: {}. Tags are immutable — choose a new name.",
            local_tag_name(name),
            collisions.join(", ")
        );
    }
    Ok(())
}

fn preflight_collision(target: &TagTarget, name: &str) -> Result<Vec<String>> {
    let mut hits = Vec::new();
    if ref_commit_sha(&target.path, &tag_ref(name))?.is_some() {
        hits.push("local".to_string());
    }
    if remote_ref_sha(&target.path, "origin", &tag_ref(name))?.is_some() {
        hits.push("origin".to_string());
    }
    Ok(hits)
}

/// Best-effort host CI verdicts for the pinned commits themselves. Every
/// failure path degrades to `unknown (...)` — evidence collection must never
/// block tagging.
fn collect_ci_evidence(
    active: &ActiveBundle,
    indexes: &[usize],
    targets: &[TagTarget],
    pins: &[(String, String)],
) -> Vec<(String, String)> {
    let pin_map: BTreeMap<&str, &str> = pins
        .iter()
        .map(|(repo_id, sha)| (repo_id.as_str(), sha.as_str()))
        .collect();

    indexes
        .iter()
        .map(|&index| {
            let repo = &active.bundle.repos[index];
            let verdict = (|| {
                let Some(sha) = pin_map.get(repo.id.as_str()) else {
                    return "unknown (no pin)".to_string();
                };
                let Ok(forge) = providers::for_repo(repo) else {
                    return "unknown (no provider)".to_string();
                };
                if forge.id() != "github" {
                    return "unknown (provider not supported)".to_string();
                }
                let Some(remote) = repo.remote.as_deref() else {
                    return "unknown (no remote recorded)".to_string();
                };
                let Some(slug) = forge.repo_full_name(remote) else {
                    return "unknown (no GitHub remote)".to_string();
                };
                let Some(target) = targets.iter().find(|target| target.repo_id == repo.id) else {
                    return "unknown (no pin)".to_string();
                };
                let pr_target = PrTarget::explicit(target.path.clone(), slug.clone());
                match github::commit_check_runs(&pr_target, &slug, sha) {
                    Ok(runs) => ci_verdict(&runs).to_string(),
                    Err(_) => "unknown (ci query failed)".to_string(),
                }
            })();
            (repo.id.clone(), verdict)
        })
        .collect()
}

/// The `knit land check` classification, applied to one commit's runs.
fn ci_verdict(runs: &[CheckRun]) -> &'static str {
    if runs.is_empty() {
        return "none";
    }
    let failed = runs.iter().any(|run| {
        matches!(run.bucket.as_deref(), Some("fail" | "cancel"))
            || matches!(run.state.as_deref(), Some("FAILURE" | "CANCELLED"))
    });
    if failed {
        return "failed";
    }
    let pending = runs.iter().any(|run| {
        !matches!(run.bucket.as_deref(), Some("pass" | "skipping"))
            && !matches!(run.state.as_deref(), Some("SUCCESS" | "SKIPPED"))
    });
    if pending {
        "pending"
    } else {
        "passed"
    }
}

/// The single annotation text, used verbatim for both the git tag message and
/// the ledger node message. The first line is the subject hosts display.
fn build_annotation(
    active: &ActiveBundle,
    name: &str,
    note: Option<&str>,
    indexes: &[usize],
    targets: &[TagTarget],
    pins: &[(String, String)],
    evidence: &[(String, String)],
) -> String {
    let mut lines = vec![
        format!(
            "knit tag {name}: known-good main across {} repo(s)",
            pins.len()
        ),
        String::new(),
    ];
    if let Some(note) = note {
        lines.push(note.to_string());
    }
    lines.push(format!("bundle: {}", active.bundle.id));

    if let Some(landed) = active
        .bundle
        .nodes
        .iter()
        .rev()
        .find(|node| node.node_type == "feature.landed")
    {
        if let Some(run_id) = landed.run_id.as_deref() {
            lines.push(format!("landed: run {run_id} ({})", landed.created_at));
        }
    }

    let tagged: Vec<&str> = targets.iter().map(|target| target.repo_id.as_str()).collect();
    let untagged: Vec<&str> = active
        .bundle
        .repos
        .iter()
        .enumerate()
        .filter(|(index, _)| !indexes.contains(index))
        .map(|(_, repo)| repo.id.as_str())
        .collect();
    if untagged.is_empty() {
        lines.push(format!("repos: {}", tagged.join(", ")));
    } else {
        lines.push(format!(
            "repos: {} (partial — not tagged: {})",
            tagged.join(", "),
            untagged.join(", ")
        ));
    }

    let checks = super::check::latest_checks(&active.bundle);
    if !checks.is_empty() {
        lines.push("checks (recorded on feature branches, not these tagged commits):".to_string());
        for (check_name, node) in &checks {
            lines.push(format!(
                "  {check_name}: {} ({})",
                node.message.as_deref().unwrap_or(""),
                node.created_at
            ));
        }
    }

    lines.push("main CI (on the tagged commits, at tag time):".to_string());
    for (repo_id, verdict) in evidence {
        lines.push(format!("  {repo_id}: {verdict}"));
    }

    lines.push("pins:".to_string());
    for (repo_id, sha) in pins {
        let base = targets
            .iter()
            .find(|target| target.repo_id == *repo_id)
            .map(|target| target.base_branch.as_str())
            .unwrap_or("main");
        lines.push(format!("  {repo_id}: {sha} (origin/{base})"));
    }

    lines.join("\n")
}

/// The honesty pass: everything here is a warning, never an error. The tag
/// still records exactly what was observed.
fn print_honesty_warnings(
    active: &ActiveBundle,
    indexes: &[usize],
    pins: &[(String, String)],
    evidence: &[(String, String)],
) {
    for (check_name, node) in super::check::latest_checks(&active.bundle) {
        if !super::check::check_passed(node) {
            println!(
                "{} check {} is red ({})",
                out::warn("warning:"),
                out::repo(check_name),
                node.message.as_deref().unwrap_or("")
            );
        }
    }

    for (repo_id, verdict) in evidence {
        if verdict == "failed" || verdict == "pending" {
            println!(
                "{} {}: main CI is {} for the tagged commit",
                out::warn("warning:"),
                out::repo(repo_id),
                verdict
            );
        }
    }

    let pin_map: BTreeMap<&str, &str> = pins
        .iter()
        .map(|(repo_id, sha)| (repo_id.as_str(), sha.as_str()))
        .collect();
    for &index in indexes {
        let repo = &active.bundle.repos[index];
        let Some(pin) = pin_map.get(repo.id.as_str()) else {
            continue;
        };
        let Some(head) = latest_recorded_head_sha(&active.bundle, repo) else {
            continue;
        };
        let path = Path::new(&repo.path);
        if !ref_exists(path, &head) {
            continue;
        }
        if !is_ancestor(path, &head, pin) {
            println!(
                "{} {}: landed feature head {} is not an ancestor of tagged {} \
                 (expected for squash or rebase merges)",
                out::warn("warning:"),
                out::repo(&repo.id),
                out::sha(short_sha(&head)),
                out::sha(short_sha(pin))
            );
        }
    }
}

/// Sequential on purpose: a midway failure rolls the already-created tags
/// back so a retry starts clean.
fn create_local_tags(
    targets: &[TagTarget],
    pins: &[(String, String)],
    name: &str,
    message: &str,
) -> Result<()> {
    let mut created: Vec<&TagTarget> = Vec::new();
    for (target, (_, sha)) in targets.iter().zip(pins) {
        let result = git_output(
            &target.path,
            [
                OsString::from("tag"),
                OsString::from("-a"),
                OsString::from(local_tag_name(name)),
                OsString::from(sha),
                OsString::from("-m"),
                OsString::from(message),
            ],
        );
        if let Err(error) = result {
            for done in &created {
                let _ = git_output_optional(&done.path, ["tag", "-d", &local_tag_name(name)]);
            }
            return Err(error.context(format!("{}: failed to create tag", target.repo_id)));
        }
        created.push(target);
    }
    Ok(())
}

fn push_tags(targets: &[TagTarget], name: &str) -> Result<()> {
    let results: Vec<(String, Result<()>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = targets
            .iter()
            .map(|target| {
                scope.spawn(move || {
                    let result = git_output(
                        &target.path,
                        [
                            OsString::from("push"),
                            OsString::from("origin"),
                            OsString::from(tag_ref(name)),
                        ],
                    )
                    .map(|_| ());
                    (target.repo_id.clone(), result)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("tag push worker thread panicked"))
            .collect()
    });

    let mut failures = Vec::new();
    for (repo_id, result) in results {
        match result {
            Ok(()) => println!(
                "{}: {} {}",
                out::repo(&repo_id),
                out::movement("pushed"),
                out::branch(local_tag_name(name))
            ),
            Err(error) => {
                println!("{}: {}", out::repo(&repo_id), out::danger("push failed"));
                failures.push(format!("{repo_id}: {error:#}"));
            }
        }
    }
    if !failures.is_empty() {
        bail!(
            "tag push failed:\n{}\n\nre-run `knit tag {name}` to retry; already-pushed repos are skipped.",
            failures.join("\n")
        );
    }
    Ok(())
}

/// Repo set for the read-only verbs. Tags are project-wide facts, so `knit
/// tag`/`tag list`/`tag show` prefer the active project's full repo set — an
/// ambient bundle context (workspace fallback or worktree cwd) must not hide
/// tags living in repos outside that bundle. Deliberate targeting via
/// `--bundle`/`KNIT_BUNDLE` still scopes to that bundle's repos, and ad-hoc
/// workspaces without a project fall back to the resolved bundle.
fn resolve_read_targets() -> Result<(PathBuf, Vec<(String, PathBuf)>)> {
    let resolved = load_active_bundle();

    let deliberate = matches!(
        resolved.as_ref().map(|active| &active.resolution_source),
        Ok(BundleResolutionSource::Explicit | BundleResolutionSource::Env)
    );
    if !deliberate {
        let cwd = std::env::current_dir().context("failed to read current directory")?;
        if let Some(root) = find_knit_root(&cwd) {
            if let Some(project_id) = load_config(&root)?.active_project {
                let project = super::project::load_project_by_id(&root, &project_id)?;
                let targets: Vec<(String, PathBuf)> = project
                    .repos
                    .iter()
                    .map(|repo| (repo.id.clone(), PathBuf::from(&repo.path)))
                    .collect();
                if !targets.is_empty() {
                    return Ok((root, targets));
                }
            }
        }
    }

    let active = resolved?;
    if active.bundle.repos.is_empty() {
        bail!("The resolved bundle has no repos. Run `knit bundle add <repo-path>` first.");
    }
    let targets = active
        .bundle
        .repos
        .iter()
        .map(|repo| (repo.id.clone(), PathBuf::from(&repo.path)))
        .collect();
    Ok((active.root.clone(), targets))
}

/// Ledger provenance for a tag name, scanned across all bundle artifacts
/// (tags outlive their bundle's active pointer). Unreadable files are
/// skipped — this is a read-only report.
fn scan_tag_provenance(root: &Path, name: &str) -> Result<Vec<(String, String, String)>> {
    let dir = root.join(".knit/bundles");
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut provenance = Vec::new();
    for entry in fs::read_dir(&dir)
        .with_context(|| format!("failed to read bundle directory {}", dir.display()))?
    {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(bundle) = read_json::<ChangeGroup>(&path) else {
            continue;
        };
        for node in &bundle.nodes {
            if node.node_type == "tag.created" && node.title.as_deref() == Some(name) {
                provenance.push((
                    bundle.id.clone(),
                    node.id.clone(),
                    node.message.clone().unwrap_or_default(),
                ));
            }
        }
    }
    Ok(provenance)
}
