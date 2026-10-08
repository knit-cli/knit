//! GitHub landing gates, read from the review, the base branch's rulesets and
//! protection, the head commit's checks, workflow runs awaiting approval, and
//! commit signatures. All of it is readable without admin rights on the
//! target, which is what contributing to someone else's repository needs.

use super::api::{can_merge, commit_check_runs, encode_path_allow_slash, encode_query_component};
use super::transport::github_api_output;
use crate::model::{Gate, GateActor, GateState};
use crate::providers::gates::{self as common, check_outcome, gate, names, plural, CheckOutcome};
use crate::providers::{CheckRun, PrTarget, PullRequest};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeSet;

pub(super) fn repo_full_name(target: &PrTarget, url: &str) -> Option<String> {
    if let Some(name) = &target.repo_full_name {
        return Some(name.clone());
    }
    let path = url.split("://").nth(1)?;
    let mut parts = path.split('/').skip(1);
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let name = parts.next().filter(|s| !s.is_empty())?;
    Some(format!("{owner}/{name}"))
}

fn get(target: &PrTarget, endpoint: &str) -> Result<Value> {
    let output = github_api_output(target, "GET", endpoint, None)?;
    serde_json::from_str(&output).with_context(|| format!("failed to decode GitHub {endpoint}"))
}

/// The base branch rules that apply to everyone, from rulesets and from the
/// public part of classic branch protection.
#[derive(Default)]
struct BaseRules {
    approvals: u64,
    code_owners: bool,
    contexts: BTreeSet<String>,
    signatures: bool,
}

fn base_rules(target: &PrTarget, repo: &str, base: &str) -> BaseRules {
    let mut rules = BaseRules::default();
    let branch = encode_path_allow_slash(base);
    if let Ok(Value::Array(items)) = get(target, &format!("repos/{repo}/rules/branches/{branch}")) {
        for rule in items {
            let parameters = &rule["parameters"];
            match rule["type"].as_str() {
                Some("pull_request") => {
                    let count = parameters["required_approving_review_count"]
                        .as_u64()
                        .unwrap_or(0);
                    rules.approvals = rules.approvals.max(count);
                    rules.code_owners |= parameters["require_code_owner_review"]
                        .as_bool()
                        .unwrap_or(false);
                }
                Some("required_status_checks") => {
                    rules.contexts.extend(
                        parameters["required_status_checks"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|check| check["context"].as_str().map(str::to_owned)),
                    );
                }
                Some("required_signatures") => rules.signatures = true,
                _ => {}
            }
        }
    }
    if let Ok(branch) = get(target, &format!("repos/{repo}/branches/{branch}")) {
        let checks = &branch["protection"]["required_status_checks"];
        rules.contexts.extend(
            checks["contexts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|context| context.as_str().map(str::to_owned)),
        );
    }
    rules
}

fn review_decision(target: &PrTarget, repo: &str, number: u64) -> Option<String> {
    let (owner, name) = repo.split_once('/')?;
    let body = json!({
        "query": "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){reviewDecision}}}",
        "variables": {"owner": owner, "name": name, "number": number},
    })
    .to_string();
    let output = github_api_output(target, "POST", "graphql", Some(&body)).ok()?;
    let value: Value = serde_json::from_str(&output).ok()?;
    value["data"]["repository"]["pullRequest"]["reviewDecision"]
        .as_str()
        .map(str::to_owned)
}

fn workflows_awaiting_approval(target: &PrTarget, repo: &str, sha: &str) -> Result<Vec<String>> {
    let runs = get(
        target,
        &format!(
            "repos/{repo}/actions/runs?head_sha={}&status=action_required&per_page=100",
            encode_query_component(sha)
        ),
    )?;
    let mut names: Vec<String> = runs["workflow_runs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|run| run["name"].as_str().map(str::to_owned))
        .collect();
    names.sort();
    names.dedup();
    Ok(names)
}

fn unsigned_commits(target: &PrTarget, repo: &str, number: u64) -> Result<usize> {
    let commits = get(
        target,
        &format!("repos/{repo}/pulls/{number}/commits?per_page=100"),
    )?;
    Ok(commits
        .as_array()
        .into_iter()
        .flatten()
        .filter(|commit| commit["commit"]["verification"]["verified"].as_bool() != Some(true))
        .count())
}

fn review_gate(decision: Option<&str>, rules: &BaseRules) -> Option<Gate> {
    let needed = if rules.approvals > 1 {
        format!("needs {} approving reviews", rules.approvals)
    } else {
        "needs an approving review".to_string()
    };
    let needed = if rules.code_owners {
        format!("{needed}, including a code owner")
    } else {
        needed
    };
    match decision {
        Some("APPROVED") => Some(gate(
            "review",
            GateState::Met,
            GateActor::Maintainers,
            "approved".to_string(),
        )),
        Some("CHANGES_REQUESTED") => Some(common::changes_requested()),
        Some("REVIEW_REQUIRED") => Some(gate(
            "review",
            GateState::Pending,
            GateActor::Maintainers,
            needed,
        )),
        _ if rules.approvals > 0 => Some(gate(
            "review",
            GateState::Pending,
            GateActor::Maintainers,
            needed,
        )),
        _ => None,
    }
}

/// Required checks that never reported are pending, not passed: on a fork
/// review they usually wait for a maintainer to approve the workflow run.
fn required_checks_gate(
    runs: &[CheckRun],
    contexts: &BTreeSet<String>,
    awaiting_approval: bool,
) -> Gate {
    let mut failed = Vec::new();
    let mut running = Vec::new();
    let mut missing = Vec::new();
    for context in contexts {
        let matching: Vec<&CheckRun> = runs.iter().filter(|run| &run.name == context).collect();
        if matching.is_empty() {
            missing.push(context.as_str());
        } else if matching
            .iter()
            .any(|run| matches!(check_outcome(run), CheckOutcome::Failed))
        {
            failed.push(context.as_str());
        } else if matching
            .iter()
            .any(|run| matches!(check_outcome(run), CheckOutcome::Pending))
        {
            running.push(context.as_str());
        }
    }
    if !failed.is_empty() {
        return gate(
            "checks",
            GateState::Blocked,
            GateActor::You,
            format!("required checks failing: {}", names(&failed)),
        );
    }
    if !missing.is_empty() {
        let actor = if awaiting_approval {
            GateActor::Maintainers
        } else {
            GateActor::Host
        };
        return gate(
            "checks",
            GateState::Pending,
            actor,
            format!("required checks not reported yet: {}", names(&missing)),
        );
    }
    if !running.is_empty() {
        return gate(
            "checks",
            GateState::Pending,
            GateActor::Host,
            format!("required checks running: {}", names(&running)),
        );
    }
    gate(
        "checks",
        GateState::Met,
        GateActor::Host,
        "required checks passed".to_string(),
    )
}

pub(super) fn gates(target: &PrTarget, pr: &PullRequest) -> Result<Vec<Gate>> {
    let repo = repo_full_name(target, &pr.url)
        .with_context(|| format!("could not tell the repository of `{}`", pr.url))?;
    let detail = get(target, &format!("repos/{repo}/pulls/{}", pr.number))?;
    let sha = detail["head"]["sha"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let base = detail["base"]["ref"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let state = detail["mergeable_state"].as_str().unwrap_or("unknown");
    let html_url = detail["html_url"].as_str().unwrap_or(&pr.url).to_owned();

    let mut gates = Vec::new();
    if detail["draft"].as_bool() == Some(true) {
        gates.push(common::draft());
    }
    match state {
        "dirty" => gates.push(common::conflict()),
        "behind" => gates.push(common::behind()),
        _ => {}
    }

    let rules = base_rules(target, &repo, &base);
    let decision = pr
        .review_decision
        .clone()
        .filter(|d| !d.is_empty())
        .or_else(|| review_decision(target, &repo, pr.number));
    gates.extend(review_gate(decision.as_deref(), &rules));

    let awaiting = if sha.is_empty() {
        Vec::new()
    } else {
        workflows_awaiting_approval(target, &repo, &sha).unwrap_or_default()
    };
    if !awaiting.is_empty() {
        let mut ci = gate(
            "ci_approval",
            GateState::Pending,
            GateActor::Maintainers,
            format!(
                "{} for a maintainer to approve: {}",
                plural(awaiting.len(), "workflow waits", "workflows wait"),
                names(&awaiting.iter().map(String::as_str).collect::<Vec<_>>())
            ),
        );
        ci.url = Some(format!("{html_url}/checks"));
        gates.push(ci);
    }

    match commit_check_runs(target, &repo, &sha) {
        Ok(runs) if !rules.contexts.is_empty() => gates.push(required_checks_gate(
            &runs,
            &rules.contexts,
            !awaiting.is_empty(),
        )),
        Ok(runs) => gates.extend(common::checks(&runs)),
        Err(_) => gates.push(gate(
            "checks",
            GateState::Unknown,
            GateActor::Host,
            "checks could not be read".to_string(),
        )),
    }

    if rules.signatures {
        match unsigned_commits(target, &repo, pr.number) {
            Ok(0) => gates.push(gate(
                "signatures",
                GateState::Met,
                GateActor::You,
                "commits signed".to_string(),
            )),
            Ok(count) => gates.push(gate(
                "signatures",
                GateState::Blocked,
                GateActor::You,
                format!(
                    "{} unsigned; the base requires signed commits",
                    plural(count, "commit", "commits")
                ),
            )),
            Err(_) => gates.push(gate(
                "signatures",
                GateState::Unknown,
                GateActor::You,
                "the base requires signed commits; signatures could not be read".to_string(),
            )),
        }
    }

    if matches!(can_merge(target, &repo), Ok(Some(false))) {
        gates.push(common::merge_permission());
    }

    let explained = common::open(&gates).next().is_some();
    match state {
        "blocked" if !explained => gates.push(gate(
            "host",
            GateState::Blocked,
            GateActor::Host,
            "GitHub reports it blocked by a rule Knit cannot read".to_string(),
        )),
        "unknown" if !explained => gates.push(gate(
            "host",
            GateState::Unknown,
            GateActor::Host,
            "GitHub is still working out whether it can merge".to_string(),
        )),
        _ => {}
    }
    for gate in &mut gates {
        if gate.url.is_none() {
            gate.url = Some(html_url.clone());
        }
    }
    Ok(gates)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(name: &str, bucket: &str) -> CheckRun {
        CheckRun {
            name: name.to_string(),
            state: None,
            bucket: Some(bucket.to_string()),
        }
    }

    #[test]
    fn a_required_check_that_never_ran_waits_for_maintainers_on_a_fork() {
        let contexts = BTreeSet::from(["Build Job".to_string()]);
        let gate = required_checks_gate(&[run("Lint", "pass")], &contexts, true);
        assert_eq!(gate.state, GateState::Pending);
        assert_eq!(gate.actor, GateActor::Maintainers);
        assert_eq!(gate.summary, "required checks not reported yet: Build Job");
    }

    #[test]
    fn optional_failures_do_not_block_when_required_checks_are_known() {
        let contexts = BTreeSet::from(["build".to_string()]);
        let runs = [run("build", "pass"), run("changelog", "fail")];
        let gate = required_checks_gate(&runs, &contexts, false);
        assert_eq!(gate.state, GateState::Met);
    }

    #[test]
    fn rulesets_give_the_review_count_when_the_decision_is_unknown() {
        let rules = BaseRules {
            approvals: 2,
            code_owners: true,
            ..BaseRules::default()
        };
        let gate = review_gate(None, &rules).unwrap();
        assert_eq!(
            gate.summary,
            "needs 2 approving reviews, including a code owner"
        );
        assert!(review_gate(None, &BaseRules::default()).is_none());
    }

    #[test]
    fn the_repository_comes_from_the_review_url_without_a_recorded_name() {
        let target = PrTarget::checkout("/tmp");
        assert_eq!(
            repo_full_name(&target, "https://github.com/acme/api/pull/12").as_deref(),
            Some("acme/api")
        );
    }
}
