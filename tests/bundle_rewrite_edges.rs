mod common;

use common::*;
use serde_json::Value;
use std::{collections::BTreeSet, fs, path::PathBuf};

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = unique_temp_dir();
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        for id in ["alpha", "backend"] {
            let (_, local, _) = init_remote_repo(&root, id);
            if id == "alpha" {
                knit(&workspace, ["init", "widget"]);
            }
            knit(&workspace, ["project", "add", id, local.to_str().unwrap()]);
        }
        knit(&workspace, ["bundle", "rewrite"]);
        Self { root, workspace }
    }

    fn checkout(&self, id: &str) -> PathBuf {
        self.workspace.join(".knit/worktrees/rewrite").join(id)
    }

    fn bundle(&self) -> Value {
        serde_json::from_slice(&self.bytes()).unwrap()
    }

    fn bytes(&self) -> Vec<u8> {
        fs::read(self.workspace.join(".knit/bundles/rewrite.bundle.json")).unwrap()
    }

    fn commit_alpha(&self, text: &str) {
        append_line(&self.checkout("alpha").join("app.txt"), text);
        knit(&self.workspace, ["commit", "--all", "-m", text]);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn squash_never_records_an_untouched_repositories_base_as_bundle_work() {
    let f = Fixture::new();
    f.commit_alpha("First widget change");
    f.commit_alpha("Second widget change");
    let unchanged = git(&f.checkout("backend"), ["rev-parse", "HEAD"]);
    knit(&f.workspace, ["squash", "-m", "Widget"]);
    let bundle = f.bundle();
    let groups = bundle["commitGroups"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    let commits = groups[0]["commits"].as_array().unwrap();
    assert_eq!(commits.len(), 1, "an untouched base is not a squash commit");
    assert_eq!(commits[0]["repoId"], "alpha");
    assert_eq!(
        git(&f.checkout("backend"), ["rev-parse", "HEAD"]),
        unchanged
    );
}

#[test]
fn rebase_with_unchanged_bases_leaves_the_ledger_unchanged() {
    let f = Fixture::new();
    f.commit_alpha("Widget");
    let before = f.bytes();
    knit(&f.workspace, ["rebase", "--offline"]);
    assert_eq!(
        f.bytes(),
        before,
        "skipped repos must not generate a rewrite"
    );
}

#[test]
fn squash_already_matching_single_commit_is_a_noop() {
    let f = Fixture::new();
    f.commit_alpha("Widget");
    knit(&f.workspace, ["squash", "-m", "Widget"]);
    let before = f.bytes();
    let head = git(&f.checkout("alpha"), ["rev-parse", "HEAD"]);
    knit(&f.workspace, ["squash", "-m", "Widget"]);
    assert_eq!(git(&f.checkout("alpha"), ["rev-parse", "HEAD"]), head);
    assert!(
        f.bytes() == before,
        "an already matching squash must be a no-op"
    );
}

/// The hosted service adds then drops within each observed node.
fn current_commits(bundle: &Value) -> BTreeSet<(String, String)> {
    let mut current = BTreeSet::new();
    for node in bundle["nodes"].as_array().unwrap() {
        if ["tag.created", "check.recorded", "branch.landed"]
            .contains(&node["type"].as_str().unwrap_or_default())
        {
            continue;
        }
        for group in bundle["commitGroups"].as_array().unwrap() {
            if group["id"] == node["commitGroupId"] {
                for c in group["commits"].as_array().unwrap() {
                    current.insert((
                        c["repoId"].as_str().unwrap().into(),
                        c["sha"].as_str().unwrap().into(),
                    ));
                }
            }
        }
        if let Some(commits) = node["commits"].as_array() {
            for c in commits {
                current.insert((
                    c["repoId"].as_str().unwrap().into(),
                    c["sha"].as_str().unwrap().into(),
                ));
            }
        }
        if let Some(changes) = node["repoChanges"].as_array() {
            for change in changes {
                let id = change["repoId"].as_str().unwrap();
                for sha in change["commits"].as_array().unwrap() {
                    current.insert((id.into(), sha.as_str().unwrap().into()));
                }
                if node["type"] == "git.observed"
                    && ["rewound", "diverged"]
                        .contains(&change["movement"].as_str().unwrap_or_default())
                {
                    if let Some(dropped) = change["droppedCommits"].as_array() {
                        for sha in dropped {
                            current.remove(&(id.into(), sha.as_str().unwrap().into()));
                        }
                    }
                }
            }
        }
    }
    current
}

#[test]
fn partial_rewind_keeps_earlier_ungrouped_observed_work_current() {
    let f = Fixture::new();
    let path = f.checkout("alpha");
    append_line(&path.join("app.txt"), "First raw change");
    git(&path, ["commit", "-am", "First raw change"]);
    let first = git(&path, ["rev-parse", "HEAD"]).trim().to_owned();
    knit(&f.workspace, ["sync"]);
    append_line(&path.join("app.txt"), "Second raw change");
    git(&path, ["commit", "-am", "Second raw change"]);
    knit(&f.workspace, ["sync"]);
    git(&path, ["reset", "--hard", &first]);
    knit(&f.workspace, ["sync"]);
    assert_eq!(
        current_commits(&f.bundle()),
        BTreeSet::from([("alpha".into(), first)])
    );
}

#[test]
fn rebase_squash_updates_empty_repos_base_without_adding_it_to_the_group() {
    let f = Fixture::new();
    f.commit_alpha("First widget change");
    f.commit_alpha("Second widget change");
    let upstream = f.root.join("backend-collaborator");
    append_line(&upstream.join("app.txt"), "Upstream work");
    git(&upstream, ["commit", "-am", "Upstream work"]);
    git(&upstream, ["push", "origin", "main"]);
    let base = git(&upstream, ["rev-parse", "HEAD"]).trim().to_owned();
    knit(&f.workspace, ["rebase", "--squash", "-m", "Widget"]);
    let bundle = f.bundle();
    let backend = bundle["repos"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "backend")
        .unwrap();
    assert_eq!(backend["baseSha"], base);
    assert_eq!(backend["headSha"], base);
    assert_eq!(
        bundle["commitGroups"][0]["commits"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(!current_commits(&bundle).contains(&("backend".into(), base)));
}

#[test]
fn rebase_squash_that_is_already_upstream_leaves_no_bundle_commits() {
    let f = Fixture::new();
    f.commit_alpha("Widget change");
    let upstream = f.root.join("alpha-collaborator");
    append_line(&upstream.join("app.txt"), "Widget change");
    git(&upstream, ["commit", "-am", "Upstream widget change"]);
    git(&upstream, ["push", "origin", "main"]);
    knit(&f.workspace, ["rebase", "--squash", "-m", "Widget"]);
    let bundle = f.bundle();
    assert!(bundle["commitGroups"].as_array().unwrap().is_empty());
    assert!(current_commits(&bundle).is_empty());
    knit(&f.workspace, ["bundle", "validate"]);
}

#[test]
fn pending_rewrite_blocks_push_before_any_feature_branch_is_published() {
    let f = Fixture::new();
    f.commit_alpha("Widget");
    let state_dir = f.workspace.join(".knit/rebase");
    fs::create_dir_all(&state_dir).unwrap();
    fs::write(state_dir.join("rewrite.json"), "{}").unwrap();
    let before = f.bytes();
    let failure = knit_fails(&f.workspace, ["push", "--force-with-lease"]);
    assert!(
        failure.contains("pending") && failure.contains("rebase --continue"),
        "{failure}"
    );
    assert!(f.bytes() == before);
    for id in ["alpha", "backend"] {
        assert!(!git_success(
            &f.root.join(format!("{id}.git")),
            ["show-ref", "--verify", "refs/heads/knit/rewrite"]
        ));
    }
}

#[test]
fn amend_sync_then_squash_retires_stale_groups_and_preserves_hosted_projection() {
    let f = Fixture::new();
    f.commit_alpha("First change");
    f.commit_alpha("Second change");
    let before = f.bundle();
    let path = f.checkout("alpha");
    append_line(&path.join("app.txt"), "Amended content");
    git(&path, ["commit", "-am", "Amended change", "--amend"]);
    let amended = git(&path, ["rev-parse", "HEAD"]).trim().to_owned();
    knit(&f.workspace, ["sync"]);
    let synced = f.bundle();
    assert_eq!(synced["commitGroups"].as_array().unwrap().len(), 1);
    assert_eq!(synced["commitGroups"][0], before["commitGroups"][0]);
    let observation = synced["nodes"].as_array().unwrap().last().unwrap();
    assert_eq!(observation["rewrite"]["kind"], "observed");
    assert_eq!(
        observation["rewrite"]["supersededGroups"][0],
        before["commitGroups"][1]
    );
    assert_eq!(
        observation["repoChanges"][0]["commits"],
        serde_json::json!([amended])
    );
    assert_eq!(
        current_commits(&synced),
        BTreeSet::from([
            (
                "alpha".into(),
                before["commitGroups"][0]["commits"][0]["sha"]
                    .as_str()
                    .unwrap()
                    .into()
            ),
            ("alpha".into(), amended),
        ])
    );
    knit(&f.workspace, ["squash", "-m", "Combined"]);
    let after = f.bundle();
    let head = git(&path, ["rev-parse", "HEAD"]).trim().to_owned();
    assert_eq!(after["commitGroups"].as_array().unwrap().len(), 1);
    assert_eq!(
        after["commitGroups"][0]["commits"],
        serde_json::json!([{"repoId":"alpha","sha":head}])
    );
    assert_eq!(
        current_commits(&after),
        BTreeSet::from([("alpha".into(), head)])
    );
    knit(&f.workspace, ["bundle", "validate"]);
}

#[test]
fn squash_preflight_rejects_nonancestor_second_repo_before_any_mutation() {
    for args in [
        vec!["squash", "-m", "Combined"],
        vec!["rebase", "--squash", "-m", "Combined"],
    ] {
        let f = Fixture::new();
        f.commit_alpha("First change");
        f.commit_alpha("Second change");
        let path = f.checkout("backend");
        // A replacement upstream root followed by a manually rebased feature.
        git(&path, ["checkout", "--orphan", "replacement"]);
        git(&path, ["commit", "-m", "Replacement upstream"]);
        let replacement = git(&path, ["rev-parse", "HEAD"]).trim().to_owned();
        git(&path, ["checkout", "knit/rewrite"]);
        git(&path, ["reset", "--hard", &replacement]);
        append_line(&path.join("app.txt"), "Rebased feature");
        git(&path, ["commit", "-am", "Rebased feature"]);
        let ledger = f.bytes();
        let heads: Vec<_> = ["alpha", "backend"]
            .iter()
            .map(|id| git(&f.checkout(id), ["rev-parse", "HEAD"]))
            .collect();
        let failure = knit_fails(&f.workspace, args);
        assert!(
            failure.contains("not an ancestor")
                && failure.contains("knit sync")
                && failure.contains("backend"),
            "{failure}"
        );
        assert_eq!(f.bytes(), ledger);
        for (id, head) in ["alpha", "backend"].iter().zip(heads) {
            assert_eq!(git(&f.checkout(id), ["rev-parse", "HEAD"]), head);
            assert!(git(&f.checkout(id), ["status", "--porcelain"])
                .trim()
                .is_empty());
        }
        assert!(!f.workspace.join(".knit/rebase/rewrite.json").exists());
    }
}
