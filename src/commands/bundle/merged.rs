//! A bundle whose reviews all merged on the host closes itself once its
//! landing plan has nothing left to run: the saved plan, or without one, the
//! plan the project's landing recipes would generate.

use super::lifecycle::{clear_active_if_matches, mark_archived};
use super::prune::landed_intermediate;
use super::{bundle_state, BundleStatus};
use crate::model::ChangeGroup;
use crate::output as out;
use crate::store::{read_json, save_active_bundle, ActiveBundle};
use anyhow::Result;
use serde_json::Value;
use std::path::Path;

const REASON: &str = "every review merged on the host";

#[derive(Debug, PartialEq)]
pub(crate) enum Merged {
    No,
    Done,
    StepsLeft(Vec<String>),
}

pub(crate) fn assess(bundle: &ChangeGroup, plan: Option<&Value>) -> Merged {
    if bundle_state(bundle) != BundleStatus::Open || landed_intermediate(bundle) {
        return Merged::No;
    }
    let repos = crate::commands::publish::publish_scope_repo_ids(bundle);
    let all_merged = !repos.is_empty()
        && repos.iter().all(|id| {
            crate::providers::publication_for_repo(bundle, id)
                .is_some_and(|p| p.state.eq_ignore_ascii_case("MERGED"))
        });
    if !all_merged {
        return Merged::No;
    }
    let left: Vec<String> = plan
        .and_then(|plan| plan["steps"].as_array())
        .into_iter()
        .flatten()
        .filter(|step| step["type"] != "merge_pr")
        .filter_map(|step| step["id"].as_str().map(str::to_owned))
        .collect();
    if left.is_empty() {
        Merged::Done
    } else {
        Merged::StepsLeft(left)
    }
}

/// `plan` overrides the saved default plan, for callers that just edited it.
pub(crate) fn close_if_merged(active: &mut ActiveBundle, plan: Option<&Value>) -> Result<bool> {
    if assess(&active.bundle, None) == Merged::No {
        return Ok(false);
    }
    let found;
    let plan = match plan {
        Some(plan) => plan,
        None => {
            let path = crate::commands::land::v2::destination_path(active, None, None);
            found = if path.exists() {
                read_json::<Value>(&path)?
            } else {
                match crate::commands::land::v2::default_plan(active, None) {
                    Ok(plan) => plan,
                    Err(error) => {
                        println!(
                            "{} every review is merged, but its landing plan could not be worked out: {error:#}",
                            out::heading("Still open:")
                        );
                        return Ok(false);
                    }
                }
            };
            &found
        }
    };
    match assess(&active.bundle, Some(plan)) {
        Merged::No => Ok(false),
        Merged::StepsLeft(left) => {
            println!(
                "{} every review is merged; the landing plan still has {}. Run `knit land apply` to run it, or `knit land remove {}` to drop it.",
                out::heading("Still open:"),
                left.join(", "),
                left.join(" ")
            );
            Ok(false)
        }
        Merged::Done => {
            if let Some(repo) = dirty_checkout(active)? {
                println!(
                    "{} every review is merged, but {} has uncommitted changes.",
                    out::heading("Still open:"),
                    out::repo(&repo)
                );
                return Ok(false);
            }
            crate::commands::bundle::archive_active_bundle(
                active,
                Some(REASON.to_owned()),
                false,
                false,
            )?;
            save_active_bundle(active)?;
            clear_active_if_matches(&active.root, &active.bundle.id)?;
            println!(
                "{} {}: every review is merged and the landing plan has nothing left to run.",
                out::ok("Closed"),
                out::node(&active.bundle.id)
            );
            if let Err(error) =
                crate::commands::remote::sync_active_bundle_to_remote_if_enabled(active, &[], false)
            {
                println!("{} {error:#}", out::warn("remote sync skipped:"));
            }
            Ok(true)
        }
    }
}

/// Without a saved `plan`, `project` (when given) supplies the landing
/// recipes that decide whether steps besides merges are still due.
pub(crate) fn close_artifact_if_merged(
    root: &Path,
    artifact_path: &Path,
    bundle: &mut ChangeGroup,
    plan: Option<&Value>,
    project: Option<&Value>,
) -> bool {
    if assess(bundle, None) == Merged::No {
        return false;
    }
    let built;
    let plan = match (plan, project) {
        (Some(plan), _) => Some(plan),
        (None, Some(project)) => {
            let active = ActiveBundle::unlocked(
                root.to_path_buf(),
                artifact_path.to_path_buf(),
                bundle.clone(),
            );
            match crate::commands::land::v2::default_plan(&active, Some(project)) {
                Ok(plan) => {
                    built = plan;
                    Some(&built)
                }
                Err(error) => {
                    println!(
                        "{} every review is merged, but its landing plan could not be worked out: {error:#}",
                        out::heading("Still open:")
                    );
                    return false;
                }
            }
        }
        (None, None) => None,
    };
    match assess(bundle, plan) {
        Merged::Done => {
            mark_archived(bundle, Some(REASON.to_owned()));
            println!(
                "{} {}: every review is merged and the landing plan has nothing left to run.",
                out::ok("Closed"),
                out::node(&bundle.id)
            );
            true
        }
        Merged::StepsLeft(left) => {
            println!(
                "{} every review is merged; the landing plan still has {}.",
                out::heading("Still open:"),
                left.join(", ")
            );
            false
        }
        Merged::No => false,
    }
}

fn dirty_checkout(active: &ActiveBundle) -> Result<Option<String>> {
    for repo in &active.bundle.repos {
        if crate::checkout::is_in_place(repo) {
            continue;
        }
        let Some(path) = crate::checkout::checkout_dir(active, repo) else {
            continue;
        };
        let pending = crate::pending::path_pending_changes(&path)?;
        if pending.tracked || pending.untracked {
            return Ok(Some(repo.id.clone()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bundle(states: &[(&str, &str)]) -> ChangeGroup {
        let mut value = serde_json::to_value(ChangeGroup::new(
            "demo".into(),
            "Demo".into(),
            "2026-10-09T00:00:00Z".into(),
        ))
        .unwrap();
        value["repos"] = states
            .iter()
            .map(|(id, _)| json!({"id": id, "path": format!("/tmp/{id}"), "baseBranch": "main"}))
            .collect();
        value["publications"] = states
            .iter()
            .enumerate()
            .map(|(n, (id, state))| {
                json!({
                    "repoId": id, "provider": "github", "kind": "pull_request", "number": n + 1,
                    "url": format!("https://github.com/o/{id}/pull/{}", n + 1),
                    "baseBranch": "main", "headBranch": "knit/demo", "state": state,
                    "updatedAt": "2026-10-09T00:00:00Z"
                })
            })
            .collect();
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn a_bundle_closes_only_when_every_review_merged_and_no_step_is_left() {
        let merged = bundle(&[("api", "MERGED"), ("web", "MERGED")]);
        assert_eq!(assess(&merged, None), Merged::Done);
        let plan = json!({"steps": [
            {"id": "merge-api", "type": "merge_pr"},
            {"id": "deploy-api", "type": "deploy"}
        ]});
        assert_eq!(
            assess(&merged, Some(&plan)),
            Merged::StepsLeft(vec!["deploy-api".into()])
        );
        let open = bundle(&[("api", "MERGED"), ("web", "OPEN")]);
        assert_eq!(assess(&open, None), Merged::No);
    }

    #[test]
    fn an_artifact_records_the_close_in_its_ledger() {
        let mut merged = bundle(&[("api", "merged")]);
        let (root, path) = (Path::new("/tmp"), Path::new("/tmp/demo.bundle.json"));
        assert!(close_artifact_if_merged(
            root,
            path,
            &mut merged,
            None,
            None
        ));
        assert_eq!(bundle_state(&merged), BundleStatus::Archived);
        assert_eq!(
            merged.nodes.last().map(|n| n.node_type.as_str()),
            Some("feature.archived")
        );
        assert!(!close_artifact_if_merged(
            root,
            path,
            &mut merged,
            None,
            None
        ));
    }
}
