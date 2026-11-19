//! `knit land check` — a live landing-readiness preflight.
//!
//! For each recorded review publication it fetches the host PR once and reports
//! state, mergeability, checks, review decision, and a verdict, so you can see
//! whether `knit land apply` will succeed (and why not) before running it. The
//! per-repo assessment is shared with `knit publish status --live`.

use crate::checkout::checkout_dir;
use crate::model::{Gate, GateActor, GateState};
use crate::output as out;
use crate::providers::{self, gates, publication_for_repo};
use crate::store::{load_active_bundle, ActiveBundle};
use anyhow::{bail, Result};
use std::path::PathBuf;

/// Live landing readiness for one repo's review publication.
pub(crate) struct LandReadiness {
    pub repo_id: String,
    pub number: u64,
    pub state: String,
    /// `clean`, `conflict`, `unknown`, or `-` for terminal states.
    pub mergeable: String,
    /// `passed`, `failed`, `pending`, `unknown`, `none`, or `-`.
    pub checks: String,
    /// `approved`, `required`, `changes`, `unknown`, `none`, or `-`.
    pub review: String,
    pub verdict: String,
    /// True when the PR is not landable yet (so callers can color/aggregate).
    pub blocked: bool,
    /// The target repository's maintainers merge this review, not this account.
    pub upstream: bool,
    pub gates: Vec<Gate>,
}

pub fn check_landing() -> Result<()> {
    let active = load_active_bundle()?;
    if active.bundle.repos.is_empty() {
        bail!("The resolved bundle has no repos. Run `knit bundle add <repo-path>` first.");
    }

    let publications: Vec<(usize, String)> = active
        .bundle
        .repos
        .iter()
        .enumerate()
        .filter_map(|(index, repo)| {
            publication_for_repo(&active.bundle, &repo.id)
                .map(|publication| (index, publication.url.clone()))
        })
        .collect();

    if publications.is_empty() {
        println!(
            "{}",
            out::muted("No review publications recorded. Run `knit publish` first.")
        );
        return Ok(());
    }

    println!("Bundle: {}\n", out::heading(&active.bundle.id));

    let required = super::required_check_names(&active);
    let mut checks_blocked = 0usize;
    if !required.is_empty() {
        for (name, state) in super::validate::assess_required_checks(&active, &required) {
            let label = match state {
                super::validate::RequiredCheckState::Green => out::ok("green"),
                super::validate::RequiredCheckState::Stale => out::warn("stale"),
                _ => out::danger(state.label()),
            };
            if state != super::validate::RequiredCheckState::Green {
                checks_blocked += 1;
            }
            println!(
                "{} {} {}",
                out::heading("Required check:"),
                out::repo(&name),
                label
            );
        }
        println!();
    }
    println!(
        "{}  {}  {}  {}  {}  {}  {}",
        out::header_field("repo", 16),
        out::header_field("pr", 6),
        out::header_field("state", 8),
        out::header_field("mergeable", 10),
        out::header_field("checks", 9),
        out::header_field("review", 9),
        out::heading("verdict")
    );

    let mut ready = 0usize;
    let mut blocked = 0usize;
    let mut landed = 0usize;
    let mut upstream = 0usize;
    for (index, url) in &publications {
        let readiness = assess_landing_readiness(&active, &active.bundle.repos[*index], url);
        print_readiness_row(&readiness);
        print_open_gates(&readiness);
        if readiness.state == "MERGED" {
            landed += 1;
        } else if readiness.upstream {
            upstream += 1;
        } else if readiness.blocked {
            blocked += 1;
        } else {
            ready += 1;
        }
    }

    println!();
    if checks_blocked > 0 {
        println!(
            "{} {ready} ready, {}{blocked} blocked, {landed} already landed; {checks_blocked} required check(s) not green",
            out::heading("Readiness:"),
            awaiting(upstream)
        );
        println!(
            "{} refresh required checks with `knit check run <name>` before `knit land apply`.",
            out::heading("Next:")
        );
        return Ok(());
    }
    println!(
        "{} {ready} ready, {}{blocked} blocked, {landed} already landed",
        out::heading("Readiness:"),
        awaiting(upstream)
    );
    if blocked == 0 && upstream > 0 {
        println!(
            "{} `knit land` then `knit land apply` records merged reviews and waits for the maintainers; `knit land resume` picks up later merges.",
            out::heading("Next:")
        );
    } else if blocked == 0 {
        println!(
            "{} when ready, run `knit land` then `knit land apply`.",
            out::heading("Next:")
        );
    }
    Ok(())
}

pub(crate) fn print_open_gates(r: &LandReadiness) {
    for gate in gates::open(&r.gates) {
        let state = format!("{:<8}", gate_state(gate.state));
        let state = match gate.state {
            GateState::Blocked => out::danger(&state),
            GateState::Pending => out::warn(&state),
            _ => out::muted(&state),
        };
        let actor = match gate.actor {
            GateActor::You => "you",
            GateActor::Maintainers => "maintainers",
            GateActor::Host => "host",
        };
        println!("{:16}  {state}  {actor:<11}  {}", "", gate.summary);
    }
}

fn gate_state(state: GateState) -> &'static str {
    match state {
        GateState::Met => "met",
        GateState::Pending => "pending",
        GateState::Blocked => "blocked",
        GateState::Unknown => "unknown",
    }
}

/// Render one readiness row, coloring the verdict by landability.
pub(crate) fn print_readiness_row(r: &LandReadiness) {
    let verdict = if r.state == "MERGED" {
        out::ok(&r.verdict)
    } else if r.blocked {
        out::warn(&r.verdict)
    } else {
        out::ok(&r.verdict)
    };
    let number = format!("#{}", r.number);
    let state = r.state.to_lowercase();
    println!(
        "{}  {}  {}  {:<10}  {:<9}  {:<9}  {}",
        out::repo_field(&r.repo_id, 16),
        out::sha(format!("{number:<6}")),
        out::status(&format!("{state:<8}")),
        r.mergeable,
        r.checks,
        r.review,
        verdict
    );
}

fn awaiting(upstream: usize) -> String {
    match upstream {
        0 => String::new(),
        n => format!("{n} awaiting maintainers, "),
    }
}

/// A gate for a review whose head commit is not the one this bundle recorded.
///
/// Commits someone else pushed to the review branch (a maintainer editing a
/// pull request opened from a fork, a teammate on a shared branch) are on the
/// review but in no ledger until a checkout pulls them, and nothing local
/// shows it. A review that still shows an earlier recorded head is waiting for
/// a push instead; that is reported only for a contribution, whose landing
/// pins the exact head. Elsewhere unpushed work is an ordinary state.
fn head_gate(
    active: &ActiveBundle,
    repo: &crate::model::RepoEntry,
    cwd: &std::path::Path,
    pr: &providers::PullRequest,
) -> Option<Gate> {
    let live = pr.head_ref_oid.as_deref()?;
    // Some hosts and older adapters report an abbreviated or placeholder head.
    if !matches!(live.len(), 40 | 64) || !live.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let live = live.to_ascii_lowercase();
    let recorded = crate::tracking::latest_recorded_head_sha(&active.bundle, repo)?;
    if recorded == live {
        return None;
    }
    let short = crate::ids::short_sha;
    // An earlier head is an ancestor of the recorded one, or after a rewrite
    // (squash, rebase, amend) a commit the ledger replaced. A head this
    // checkout has never fetched is neither.
    let earlier = crate::git::is_ancestor(cwd, &live, &recorded)
        || crate::tracking::ledger_mentions_commit(&active.bundle, &repo.id, &live);
    let summary = if !earlier {
        format!(
            "review head {} has commits this bundle has not recorded (recorded head {}): run `knit pull`",
            short(&live),
            short(&recorded)
        )
    } else if crate::contribution::cross_repository(repo).unwrap_or(false) {
        format!(
            "review still shows {}, not the recorded head {}: run `knit push`",
            short(&live),
            short(&recorded)
        )
    } else {
        return None;
    };
    Some(gates::gate(
        "head",
        GateState::Blocked,
        GateActor::You,
        summary,
    ))
}

/// Fetch a publication's live PR state and classify its landing readiness. Forge
/// errors are captured into the verdict rather than aborting the whole table.
pub(crate) fn assess_landing_readiness(
    active: &ActiveBundle,
    repo: &crate::model::RepoEntry,
    publication_url: &str,
) -> LandReadiness {
    let base = LandReadiness {
        repo_id: repo.id.clone(),
        number: providers::pr_number_from_url(publication_url).unwrap_or(0),
        state: "?".to_string(),
        mergeable: "-".to_string(),
        checks: "-".to_string(),
        review: "-".to_string(),
        verdict: String::new(),
        blocked: true,
        upstream: false,
        gates: Vec::new(),
    };

    let forge = match providers::for_repo(repo) {
        Ok(forge) => forge,
        Err(error) => {
            return LandReadiness {
                verdict: format!("provider unavailable: {error}"),
                ..base
            }
        }
    };
    let cwd = checkout_dir(active, repo).unwrap_or_else(|| PathBuf::from(&repo.path));
    let branch = publication_for_repo(&active.bundle, &repo.id)
        .map(|p| p.base_branch.as_str())
        .unwrap_or(&repo.base_branch);
    let mut target = match crate::contribution::target(&cwd, repo, forge.as_ref(), branch, false) {
        Ok(target) => target,
        Err(error) => {
            return LandReadiness {
                verdict: format!("invalid contribution: {error}"),
                ..base
            }
        }
    };
    // A review whose head moved is still a readable review. `head_gate` says
    // what moved and what to run; the adapter would only refuse it outright.
    target.verify_head = false;

    let pr = match forge.view(&target, publication_url).and_then(|pr| {
        providers::settle_mergeability(forge.as_ref(), &target, publication_url, &repo.id, pr)
    }) {
        Ok(pr) => pr,
        Err(error) => {
            return LandReadiness {
                verdict: format!("PR unavailable: {error}"),
                ..base
            }
        }
    };
    let state = pr.state.clone().unwrap_or_else(|| "UNKNOWN".to_string());

    // Terminal states: nothing to assess.
    match state.as_str() {
        "MERGED" => {
            return LandReadiness {
                state,
                verdict: "already landed".to_string(),
                blocked: false,
                ..base
            }
        }
        "CLOSED" => {
            return LandReadiness {
                state,
                verdict: "closed".to_string(),
                ..base
            }
        }
        _ => {}
    }

    let mut gates = match forge.gates(&target, &pr) {
        Ok(gates) => gates,
        Err(error) => {
            return LandReadiness {
                state,
                verdict: format!("gates unavailable: {error}"),
                ..base
            }
        }
    };
    gates.extend(head_gate(active, repo, &cwd, &pr));
    let mergeable = if pr.mergeability_computing() {
        "computing"
    } else if pr.is_conflicting() {
        "conflict"
    } else if pr.mergeable.as_deref() == Some("MERGEABLE") {
        "clean"
    } else {
        "unknown"
    };
    let column = |kind: &str, labels: [&'static str; 4]| {
        gates
            .iter()
            .find(|gate| gate.kind == kind)
            .map(|gate| match gate.state {
                GateState::Met => labels[0],
                GateState::Pending => labels[1],
                GateState::Blocked => labels[2],
                GateState::Unknown => labels[3],
            })
            .unwrap_or("none")
    };
    let checks = column("checks", ["passed", "pending", "failed", "unknown"]);
    let review = column("review", ["approved", "required", "changes", "unknown"]);

    let upstream = crate::contribution::cross_repository(repo).unwrap_or(false)
        && gates.iter().any(|gate| gate.kind == "merge_permission");
    let yours = gates::open(&gates).any(|gate| gate.actor == GateActor::You);
    let (verdict, blocked) = match gates::headline(&gates) {
        None => ("ready".to_string(), false),
        Some(headline) if upstream && yours => (format!("awaiting maintainers; {headline}"), false),
        Some(_) if upstream => ("awaiting maintainers".to_string(), false),
        Some(headline) => (headline, true),
    };

    LandReadiness {
        repo_id: repo.id.clone(),
        number: pr.number,
        state,
        mergeable: mergeable.to_string(),
        checks: checks.to_string(),
        review: review.to_string(),
        verdict,
        blocked,
        upstream,
        gates,
    }
}
