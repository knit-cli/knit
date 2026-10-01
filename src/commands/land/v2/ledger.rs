//! Derive landing summaries from durable, successful merge receipts. Repair is
//! explicit (migrate/resume); reading a bundle never invents ledger metadata.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeSet;

// Internal only: repository visibility projection filters the existing public
// summary fields. Never serialize this per-repository evidence into a bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CompletedMerge {
    repo_id: String,
    merge_type: String,
    target_branch: String,
    publication_url: Option<String>,
}

pub(super) fn branch_only(merges: &[CompletedMerge]) -> bool {
    !merges.is_empty() && merges.iter().all(|m| m.merge_type == "merge_branch")
}

pub(super) fn no_source_merges(run: &Value) -> bool {
    let no_merges = |steps: &Value| {
        steps.as_array().is_some_and(|steps| {
            steps
                .iter()
                .all(|s| !matches!(s["type"].as_str(), Some("merge_pr" | "merge_branch")))
        })
    };
    no_merges(&run["plan"]["steps"])
        && no_merges(&run["steps"])
        && run["steps"]
            .as_array()
            .is_some_and(|steps| steps.iter().all(|s| s["status"] == "succeeded"))
}

fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().filter(|s| !s.trim().is_empty())
}

pub(super) fn completed_merges(run: &Value, bundle: &Value) -> Result<Vec<CompletedMerge>> {
    let mut merges = Vec::new();
    for step in run["steps"].as_array().context("run steps required")? {
        if step["status"] != "succeeded"
            || !matches!(step["type"].as_str(), Some("merge_pr" | "merge_branch"))
        {
            continue;
        }
        let repo = nonempty(&step["repoId"]).context("merge repo required")?;
        ensure!(
            bundle["repos"]
                .as_array()
                .is_some_and(|repos| repos.iter().any(|r| r["id"] == repo)),
            "merge repository is not in bundle"
        );
        let publication_url = if step["type"] == "merge_pr" {
            // The receipt is authoritative. For older receipts lacking the URL,
            // use the exact publication pinned in the run's source snapshot only
            // when the bundle confirms that same review has been merged.
            let url = nonempty(&step["output"]["publicationUrl"])
                .map(str::to_owned)
                .or_else(|| {
                    let candidates: BTreeSet<_> = run["sourceBundle"]["publications"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|p| p["repoId"] == repo)
                        .filter_map(|p| nonempty(&p["url"]))
                        .collect();
                    if candidates.len() != 1 {
                        return None;
                    }
                    let url = *candidates.first()?;
                    bundle["publications"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|p| p["repoId"] == repo && p["url"] == url && p["state"] == "MERGED")
                        .then(|| url.to_owned())
                })
                .context("successful review merge lacks an unambiguous publication URL")?;
            Some(url)
        } else {
            None
        };
        let merge = CompletedMerge {
            repo_id: repo.into(),
            merge_type: step["type"].as_str().unwrap().into(),
            target_branch: nonempty(&step["output"]["targetBranch"])
                .context("successful merge lacks target branch")?
                .into(),
            publication_url,
        };
        if !merges.contains(&merge) {
            merges.push(merge);
        }
    }
    Ok(merges)
}

pub(super) fn merge_repos(merges: &[CompletedMerge]) -> Vec<String> {
    merges
        .iter()
        .map(|m| m.repo_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}
pub(super) fn merge_urls(merges: &[CompletedMerge]) -> Vec<String> {
    merges
        .iter()
        .filter_map(|m| m.publication_url.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Returns diagnostics for unresolved nodes. Only bundle nodes change; saved
/// plans, hashes, runs, timestamps, attribution and unrelated fields stay intact.
pub(crate) fn repair_landed_nodes(bundle: &mut Value, runs: &[Value]) -> Vec<String> {
    let mut warnings = Vec::new();
    let snapshot = bundle.clone();
    for node in bundle["nodes"].as_array_mut().into_iter().flatten() {
        if node["type"] != "feature.landed"
            || node["publicationUrls"]
                .as_array()
                .is_some_and(|a| !a.is_empty())
            || node["landing"]["branchOnly"] == true
            || node["landing"]["mergeMode"] == "none"
        {
            continue;
        }
        let candidates: Vec<_> = runs
            .iter()
            .filter(|run| {
                nonempty(&node["runId"]).is_some()
                    && nonempty(&node["planId"]).is_some()
                    && run["id"] == node["runId"]
                    && run["bundleId"] == snapshot["id"]
                    && run["planId"] == node["planId"]
            })
            .collect();
        let repair = (|| -> Result<()> {
            ensure!(
                candidates.len() == 1,
                "matching run is missing or ambiguous"
            );
            let run = candidates[0];
            super::runtime::verify_run(run, &run["plan"])?;
            ensure!(
                run["plan"]["kind"] == "KnitLandPlan" && run["plan"]["schemaVersion"] == "0.2",
                "run lacks a v0.2 plan"
            );
            ensure!(
                run["sourceBundle"]["id"] == snapshot["id"],
                "run source bundle differs from landing bundle"
            );
            ensure!(
                run["steps"]
                    .as_array()
                    .is_some_and(|steps| steps.iter().all(|s| s["status"] == "succeeded")),
                "run has incomplete steps"
            );
            ensure!(run["recoveryStartedAt"].is_null(), "run entered recovery");
            let merges = completed_merges(run, &snapshot)?;
            let repos = merge_repos(&merges);
            let recorded: BTreeSet<_> = node["repoIds"]
                .as_array()
                .context("node repoIds required")?
                .iter()
                .filter_map(Value::as_str)
                .collect();
            ensure!(
                (!repos.is_empty() || no_source_merges(run))
                    && recorded == repos.iter().map(String::as_str).collect(),
                "node repositories differ from successful merges"
            );
            if no_source_merges(run) {
                ensure!(
                    node["landing"].is_object(),
                    "no-merge node lacks landing destination"
                );
                node["landing"]["mergeMode"] = json!("none");
            }
            let urls = merge_urls(&merges);
            if !urls.is_empty() {
                node["publicationUrls"] = json!(urls);
            }
            if branch_only(&merges) {
                ensure!(
                    node["landing"].is_object(),
                    "branch-only node lacks landing destination"
                );
                node["landing"]["branchOnly"] = json!(true);
            }
            Ok(())
        })();
        if let Err(error) = repair {
            warnings.push(format!(
                "landing node {}: {error:#}",
                node["id"].as_str().unwrap_or("(unknown)")
            ));
        }
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::land::v2::canonical_hash;

    fn fixture(branch: bool) -> (Value, Value) {
        let kind = if branch { "merge_branch" } else { "merge_pr" };
        let bundle = json!({"id":"sample","repos":[{"id":"api"},{"id":"web"}],"extension":{"keep":true},
            "nodes":[{"id":"land-1","type":"feature.landed","repoIds":["api"],"runId":"run-1","planId":"plan-1","createdAt":"original","actor":{"session":"original"},"custom":{"preserve":42},"landing":{"terminal":!branch}}]});
        let plan = json!({"schemaVersion":"0.2","kind":"KnitLandPlan","id":"plan-1","bundleId":"sample","steps":[{"id":"merge-api","type":kind,"repoId":"api"}]});
        let mut output = json!({"targetBranch":"main"});
        if !branch {
            output["publicationUrl"] = json!("https://example.invalid/api/pull/1");
        }
        let run = json!({"schemaVersion":"0.2","kind":"KnitLandRun","id":"run-1","bundleId":"sample","planId":"plan-1","planHash":canonical_hash(&plan),"plan":plan,
            "sourceBundle":{"id":"sample"},"steps":[{"id":"merge-api","type":kind,"repoId":"api","status":"succeeded","output":output}]});
        (bundle, run)
    }

    #[test]
    fn landing_record_no_merge_repair_requires_complete_matching_immutable_plan() {
        let (mut bundle, mut run) = fixture(false);
        bundle["nodes"][0]["repoIds"] = json!([]);
        run["plan"]["steps"] = json!([{"id":"deploy","type":"run","repoId":"api"}]);
        run["planHash"] = json!(canonical_hash(&run["plan"]));
        run["steps"] = json!([{"id":"deploy","type":"run","repoId":"api","status":"succeeded"}]);
        let before = bundle.clone();
        for bad in [false, true] {
            let mut candidate = bundle.clone();
            let mut receipt = run.clone();
            if bad {
                receipt["steps"][0]["status"] = json!("failed");
            }
            assert_eq!(
                repair_landed_nodes(&mut candidate, &[receipt]).is_empty(),
                !bad
            );
            if bad {
                assert_eq!(candidate, before);
            } else {
                assert_eq!(candidate["nodes"][0]["landing"]["mergeMode"], "none");
                let repaired = candidate.clone();
                assert!(repair_landed_nodes(&mut candidate, std::slice::from_ref(&run)).is_empty());
                assert_eq!(candidate, repaired);
                candidate["nodes"][0]["landing"]
                    .as_object_mut()
                    .unwrap()
                    .remove("mergeMode");
                assert_eq!(candidate, before);
            }
        }
    }

    #[test]
    fn landing_record_repair_is_additive_idempotent_and_does_not_modify_run() {
        for branch in [false, true] {
            let (mut bundle, run) = fixture(branch);
            let before = bundle.clone();
            let run_before = run.clone();
            assert!(repair_landed_nodes(&mut bundle, std::slice::from_ref(&run)).is_empty());
            let repaired = bundle.clone();
            assert!(repair_landed_nodes(&mut bundle, std::slice::from_ref(&run)).is_empty());
            assert_eq!(bundle, repaired);
            assert_eq!(run, run_before);
            if branch {
                assert_eq!(bundle["nodes"][0]["landing"]["branchOnly"], true);
                bundle["nodes"][0]["landing"]
                    .as_object_mut()
                    .unwrap()
                    .remove("branchOnly");
            } else {
                assert_eq!(
                    bundle["nodes"][0]["publicationUrls"],
                    json!(["https://example.invalid/api/pull/1"])
                );
                bundle["nodes"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("publicationUrls");
            }
            assert_eq!(bundle, before);
        }
    }

    #[test]
    fn landing_record_repair_refuses_missing_ambiguous_or_untrusted_evidence() {
        for case in 0..13 {
            let (mut bundle, mut run) = fixture(false);
            let mut runs = vec![];
            match case {
                0 => run["bundleId"] = json!("other"),
                1 => run["planHash"] = json!("changed"),
                2 => run["steps"][0]["repoId"] = json!("web"),
                3 => run["steps"][0]["status"] = json!("failed"),
                4 => {
                    run["steps"][0]["output"]
                        .as_object_mut()
                        .unwrap()
                        .remove("publicationUrl");
                }
                5 => run["recoveryStartedAt"] = json!("earlier"),
                6 => bundle["nodes"][0]["repoIds"] = json!(["web"]),
                7 => run["id"] = json!("other-run"),
                8 => runs.push(run.clone()),
                9 => run["planId"] = json!("other-plan"),
                10 => run["sourceBundle"]["id"] = json!("other-bundle"),
                11 => run["steps"][0]["id"] = json!("other-step"),
                12 => run["steps"][0]["type"] = json!("merge_branch"),
                _ => unreachable!(),
            }
            runs.push(run);
            let before = bundle.clone();
            assert_eq!(
                repair_landed_nodes(&mut bundle, &runs).len(),
                1,
                "case {case}"
            );
            assert_eq!(bundle, before, "case {case}");
        }
    }

    #[test]
    fn landing_record_receipt_selection_keeps_repo_association_and_ignores_unfinished_steps() {
        let (bundle, mut run) = fixture(false);
        run["steps"].as_array_mut().unwrap().extend([
            json!({"id":"failed","repoId":"web","type":"merge_pr","status":"failed","output":{"publicationUrl":"https://example.invalid/web/pull/2","targetBranch":"main"}}),
            json!({"id":"pending","repoId":"web","type":"merge_branch","status":"pending","output":{"targetBranch":"main"}}),
        ]);
        let merges = completed_merges(&run, &bundle).unwrap();
        assert_eq!(merge_repos(&merges), vec!["api"]);
        assert_eq!(
            merge_urls(&merges),
            vec!["https://example.invalid/api/pull/1"]
        );
        assert!(!branch_only(&merges));
        assert!(!branch_only(&[]));
    }

    #[test]
    fn landing_record_repair_publication_fallback_requires_exact_pinned_review_and_repo() {
        let (bundle, mut run) = fixture(false);
        run["steps"][0]["output"]
            .as_object_mut()
            .unwrap()
            .remove("publicationUrl");
        run["sourceBundle"]["publications"] =
            json!([{"repoId":"api","url":"https://example.invalid/api/pull/1"}]);
        for (repo, url, state, succeeds) in [
            ("api", "https://example.invalid/api/pull/1", "MERGED", true),
            ("web", "https://example.invalid/api/pull/1", "MERGED", false),
            ("api", "https://example.invalid/api/pull/2", "MERGED", false),
            ("api", "https://example.invalid/api/pull/1", "OPEN", false),
        ] {
            let mut bundle = bundle.clone();
            bundle["publications"] = json!([{"repoId":repo,"url":url,"state":state}]);
            let before = bundle.clone();
            assert_eq!(
                repair_landed_nodes(&mut bundle, std::slice::from_ref(&run)).is_empty(),
                succeeds
            );
            if !succeeds {
                assert_eq!(bundle, before);
            }
        }
    }
}
