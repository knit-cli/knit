//! `knit check` — recorded check verdicts on the bundle ledger.
//!
//! A check is a named fact ("ci", "functional", ...) recorded as a
//! `check.recorded` node: pass or fail, pinned to the exact per-repo head
//! SHAs it was computed against. `check run` executes the project command of
//! the same name and records its exit status; `check record` is the door for
//! external tools and humans; `check status` reports the latest verdict per
//! check and whether it is still fresh (no tracked repo has moved since).
//!
//! "Merge ready" is derived, never set: landing can require named checks to
//! be green *and fresh* (`landing.requireChecks` in the project), and a stale
//! or missing verdict counts as not green. Knit does not schedule or watch
//! anything — one command per check, the exit code is the verdict, and the
//! trust model is the same as committing: whoever can write the bundle can
//! attest.

use crate::checkout::checkout_dir;
use crate::git::rev_parse;
use crate::ids::{node_id, short_sha, slugify};
use crate::model::{BundleNode, ChangeGroup, CommitRef};
use crate::output as out;
use crate::store::ActiveBundle;
use crate::store::{load_active_bundle, load_active_bundle_for_update, save_active_bundle};
use crate::time::now_iso;
use anyhow::{bail, Result};
use std::collections::BTreeMap;

/// `knit check run <name>`: run the project command `name` and record the
/// verdict. The command failing is a recorded `fail`, then an error exit.
pub fn run_check(name: &str, explicit_repos: &[String], all: bool) -> Result<()> {
    let active = load_active_bundle()?;
    let outcome = super::run::run_named_command_collect(&active, name, explicit_repos, all)?;
    drop(active);

    let pass = outcome.failures.is_empty();
    let detail = if pass {
        outcome.command_display.clone()
    } else {
        format!(
            "{} ({})",
            outcome.command_display,
            outcome.failures.join(", ")
        )
    };
    append_check(name, pass, &detail)?;
    if !pass {
        bail!(
            "check `{}` failed: {}",
            slugify(name),
            outcome.failures.join(", ")
        );
    }
    Ok(())
}

/// `knit check record <name> --pass|--fail`: record a verdict computed
/// elsewhere (another tool, a host CI run, a human).
pub fn record_check(name: &str, pass: bool, detail: Option<&str>) -> Result<()> {
    append_check(name, pass, detail.unwrap_or("recorded externally"))?;
    Ok(())
}

/// `knit check status`: the latest verdict per check name with freshness.
pub fn show_check_status() -> Result<()> {
    let active = load_active_bundle()?;
    let latest = latest_checks(&active.bundle);
    if latest.is_empty() {
        println!("{}", out::muted("No checks recorded."));
        crate::advice::print(
            &active.root,
            "record one with `knit check run <project-command>` or `knit check record <name> --pass`.",
        );
        return Ok(());
    }

    let heads = current_heads(&active);
    println!(
        "{}  {}  {}  {}",
        out::header_field("check", 14),
        out::header_field("status", 8),
        out::header_field("state", 8),
        out::heading("recorded")
    );
    for (name, node) in latest {
        let pass = check_passed(node);
        let fresh = check_is_fresh_against(&heads, node);
        let pins = node
            .commits
            .iter()
            .map(|pin| format!("{}@{}", pin.repo_id, short_sha(&pin.sha)))
            .collect::<Vec<_>>()
            .join(" ");
        println!(
            "{}  {}  {}  {}",
            out::repo_field(name, 14),
            if pass {
                out::ok(format!("{:<8}", "green"))
            } else {
                out::danger(format!("{:<8}", "red"))
            },
            if fresh {
                out::ok(format!("{:<8}", "fresh"))
            } else {
                out::warn(format!("{:<8}", "stale"))
            },
            out::muted(format!("{} {}", node.created_at, pins))
        );
    }
    Ok(())
}

/// The latest `check.recorded` node per check name, in name order. Later
/// ledger entries win, so a re-run replaces the previous verdict.
pub(crate) fn latest_checks(bundle: &ChangeGroup) -> BTreeMap<&str, &BundleNode> {
    let mut latest = BTreeMap::new();
    for node in &bundle.nodes {
        if node.node_type != "check.recorded" {
            continue;
        }
        if let Some(name) = node.title.as_deref() {
            latest.insert(name, node);
        }
    }
    latest
}

/// Whether a recorded verdict was a pass. The message is written by Knit with
/// a machine-parsable `pass`/`fail` prefix.
pub(crate) fn check_passed(node: &BundleNode) -> bool {
    node.message
        .as_deref()
        .is_some_and(|message| message.starts_with("pass"))
}

/// A verdict is fresh when every repo currently tracked in the bundle still
/// sits on the head SHA the verdict was pinned to. Repos with no pin (added
/// after the verdict was recorded) make it stale.
pub(crate) fn check_is_fresh(active: &ActiveBundle, node: &BundleNode) -> bool {
    check_is_fresh_against(&current_heads(active), node)
}

fn check_is_fresh_against(heads: &BTreeMap<String, String>, node: &BundleNode) -> bool {
    let pins: BTreeMap<&str, &str> = node
        .commits
        .iter()
        .map(|pin| (pin.repo_id.as_str(), pin.sha.as_str()))
        .collect();
    heads
        .iter()
        .all(|(repo_id, sha)| pins.get(repo_id.as_str()) == Some(&sha.as_str()))
}

/// Current head SHA per tracked repo: the live checkout HEAD when one exists,
/// else the recorded head. Repos with neither are omitted (and so cannot be
/// pinned — verdicts over them read as stale, which is the safe direction).
fn current_heads(active: &ActiveBundle) -> BTreeMap<String, String> {
    let mut heads = BTreeMap::new();
    for repo in &active.bundle.repos {
        let sha = checkout_dir(active, repo)
            .and_then(|checkout| rev_parse(&checkout, "HEAD").ok())
            .or_else(|| repo.head_sha.clone());
        if let Some(sha) = sha {
            heads.insert(repo.id.clone(), sha);
        }
    }
    heads
}

fn append_check(name: &str, pass: bool, detail: &str) -> Result<()> {
    let name = slugify(name);
    if name.is_empty() {
        bail!("Check name must not be empty.");
    }
    let mut active = load_active_bundle_for_update()?;
    let pins: Vec<CommitRef> = current_heads(&active)
        .into_iter()
        .map(|(repo_id, sha)| CommitRef { repo_id, sha })
        .collect();
    if pins.is_empty() {
        bail!("No tracked repo heads to pin the check against. Run `knit bundle worktree` first.");
    }

    let verdict = if pass { "pass" } else { "fail" };
    let message = format!("{verdict} — {detail}");
    let node = BundleNode::check_recorded(
        node_id("check"),
        now_iso(),
        name.clone(),
        message,
        pins.clone(),
    );
    let node_ref = node.id.clone();
    active.bundle.nodes.push(node);
    active.bundle.head_node_id = active.bundle.nodes.last().map(|node| node.id.clone());
    active.bundle.updated_at = now_iso();
    save_active_bundle(&active)?;

    println!(
        "{} {} {} {}",
        out::heading("Check:"),
        out::repo(&name),
        if pass {
            out::ok("green")
        } else {
            out::danger("red")
        },
        out::node(&node_ref)
    );
    for pin in &pins {
        println!(
            "  {} {}",
            out::repo(&pin.repo_id),
            out::sha(short_sha(&pin.sha))
        );
    }
    Ok(())
}
