mod common;

use common::*;
use serde_json::Value;
use std::{collections::BTreeSet, fs, path::PathBuf};

const REPOS: [&str; 2] = ["alpha", "beta"];

struct Fixture {
    root: PathBuf,
    home: PathBuf,
    workspace: PathBuf,
    collaborators: Vec<PathBuf>,
}

impl Fixture {
    fn new() -> Self {
        let root = unique_temp_dir();
        let home = root.join("home");
        let workspace = root.join("workspace");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        let mut f = Self {
            root,
            home,
            workspace,
            collaborators: Vec::new(),
        };
        f.run(&["init", "demo"]);
        for id in REPOS {
            let (_, local, collaborator) = init_remote_repo(&f.root, id);
            f.run(&["project", "add", id, local.to_str().unwrap()]);
            f.collaborators.push(collaborator);
        }
        f.run(&["bundle", "rewrite"]);
        for id in REPOS {
            fs::write(f.checkout(id).join("app.txt"), "feature content\n").unwrap();
        }
        f.run(&["commit", "--all", "-m", "First change"]);
        for id in REPOS {
            fs::write(f.checkout(id).join("second.txt"), "second feature\n").unwrap();
        }
        f.run(&["commit", "--all", "-m", "Second change"]);
        assert_eq!(f.bundle()["commitGroups"].as_array().unwrap().len(), 2);
        f
    }

    fn checkout(&self, id: &str) -> PathBuf {
        self.workspace.join(".knit/worktrees/rewrite").join(id)
    }

    fn run(&self, args: &[&str]) -> String {
        knit_with_env(
            &self.workspace,
            args,
            &[
                ("HOME", self.home.to_str().unwrap()),
                ("GIT_EDITOR", "true"),
            ],
        )
    }

    fn fail(&self, args: &[&str]) -> String {
        knit_fails_with_env(
            &self.workspace,
            args,
            &[
                ("HOME", self.home.to_str().unwrap()),
                ("GIT_EDITOR", "true"),
            ],
        )
    }

    fn bundle_path(&self) -> PathBuf {
        self.workspace.join(".knit/bundles/rewrite.bundle.json")
    }

    fn bundle(&self) -> Value {
        serde_json::from_slice(&fs::read(self.bundle_path()).unwrap()).unwrap()
    }

    fn history(&self) -> Vec<u8> {
        fs::read(self.workspace.join(".knit/history/demo.history.jsonl")).unwrap()
    }

    fn assert_pending_rebase_blocks_writes(&self) {
        let ledger = fs::read(self.bundle_path()).unwrap();
        let history = self.history();
        let completed_head = self.head("alpha");
        let conflicted_head = self.head("beta");
        let file = self.checkout("alpha").join("second.txt");
        let content = fs::read(&file).unwrap();
        // Give --all something it could wrongly commit in the completed repo.
        fs::write(&file, "uncommitted during pending rebase\n").unwrap();
        let index = git(&self.checkout("alpha"), ["diff", "--cached"]);
        for args in [
            vec!["sync"],
            vec!["commit", "--all", "-m", "Must not commit"],
        ] {
            let failure = self.fail(&args);
            assert!(
                failure.to_lowercase().contains("pending")
                    || failure.to_lowercase().contains("in progress"),
                "{failure}"
            );
            assert!(failure.contains("rebase --continue"), "{failure}");
            assert!(failure.contains("rebase --abort"), "{failure}");
            assert_eq!(fs::read(self.bundle_path()).unwrap(), ledger);
            assert_eq!(self.history(), history);
            assert_eq!(self.head("alpha"), completed_head);
            assert_eq!(self.head("beta"), conflicted_head);
            assert_eq!(
                git(&self.checkout("alpha"), ["diff", "--cached"]),
                index,
                "blocked --all must not even stage the completed repo"
            );
        }
        let status = self.run(&["status"]);
        assert!(
            status.contains("alpha") && status.contains("beta"),
            "{status}"
        );
        let shown = self.run(&["show", "HEAD"]);
        assert!(shown.contains("Second change"), "{shown}");
        assert_eq!(fs::read(self.bundle_path()).unwrap(), ledger);
        assert_eq!(self.history(), history);
        assert_eq!(self.head("alpha"), completed_head);
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "uncommitted during pending rebase\n"
        );
        fs::write(file, content).unwrap();
    }

    fn head(&self, id: &str) -> String {
        git(&self.checkout(id), ["rev-parse", "HEAD"])
            .trim()
            .to_owned()
    }

    fn advance_upstream(&self, conflict: bool) -> Vec<String> {
        self.collaborators
            .iter()
            .enumerate()
            .map(|(index, repo)| {
                let file = if conflict && index == 1 {
                    "app.txt"
                } else {
                    "upstream.txt"
                };
                fs::write(repo.join(file), "upstream content\n").unwrap();
                git(repo, ["add", file]);
                git(repo, ["commit", "-m", "Upstream change"]);
                git(repo, ["push", "origin", "main"]);
                git(repo, ["rev-parse", "HEAD"]).trim().to_owned()
            })
            .collect()
    }

    fn assert_heads_and_counts(&self, bundle: &Value, count: usize) {
        for id in REPOS {
            let repo = repo(bundle, id);
            assert_eq!(repo["headSha"], self.head(id));
            let range = format!("{}..HEAD", repo["baseSha"].as_str().unwrap());
            assert_eq!(
                git(&self.checkout(id), ["rev-list", "--count", &range]).trim(),
                count.to_string()
            );
            assert!(git_success(
                &self.checkout(id),
                [
                    "merge-base",
                    "--is-ancestor",
                    repo["baseSha"].as_str().unwrap(),
                    "HEAD"
                ]
            ));
            assert_eq!(git(&self.checkout(id), ["status", "--porcelain"]), "");
        }
    }

    fn assert_one_group(&self, bundle: &Value, message: &str) {
        let groups = bundle["commitGroups"].as_array().unwrap();
        assert_eq!(groups.len(), 1, "{groups:#?}");
        assert_eq!(groups[0]["message"], message);
        let commits = groups[0]["commits"].as_array().unwrap();
        assert_eq!(commits.len(), 2);
        for id in REPOS {
            let commit = commits.iter().find(|c| c["repoId"] == id).unwrap();
            assert_eq!(commit["sha"], self.head(id));
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn repo<'a>(bundle: &'a Value, id: &str) -> &'a Value {
    bundle["repos"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == id)
        .unwrap()
}

fn new_changes<'a>(before: &Value, after: &'a Value) -> Vec<&'a Value> {
    after["nodes"].as_array().unwrap()[before["nodes"].as_array().unwrap().len()..]
        .iter()
        .flat_map(|node| node["repoChanges"].as_array().into_iter().flatten())
        .collect()
}

fn assert_superseded(before: &Value, after: &Value) {
    let new_nodes =
        &after["nodes"].as_array().unwrap()[before["nodes"].as_array().unwrap().len()..];
    let superseded: Vec<_> = new_nodes
        .iter()
        .flat_map(|n| {
            n["rewrite"]["supersededGroups"]
                .as_array()
                .into_iter()
                .flatten()
        })
        .collect();
    assert_eq!(superseded.len(), 2, "{new_nodes:#?}");
    for group in before["commitGroups"].as_array().unwrap() {
        assert!(
            superseded.contains(&group),
            "missing full superseded group: {group}"
        );
    }
    let changes = new_changes(before, after);
    for id in REPOS {
        let dropped: BTreeSet<_> = changes
            .iter()
            .filter(|c| c["repoId"] == id)
            .flat_map(|c| c["droppedCommits"].as_array().into_iter().flatten())
            .map(|sha| sha.as_str().unwrap())
            .collect();
        let original: BTreeSet<_> = before["commitGroups"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["commits"].as_array().unwrap())
            .filter(|c| c["repoId"] == id)
            .map(|c| c["sha"].as_str().unwrap())
            .collect();
        assert_eq!(dropped, original, "dropped SHAs for {id}");
    }
}

fn assert_base_movement(before: &Value, after: &Value, upstream: &[String], count: usize) {
    let changes = new_changes(before, after);
    for (id, upstream) in REPOS.into_iter().zip(upstream) {
        assert_eq!(repo(after, id)["baseSha"], *upstream);
        assert_ne!(repo(before, id)["headSha"], repo(after, id)["headSha"]);
        let change = changes
            .iter()
            .find(|c| c["repoId"] == id && c["baseAfterSha"] == *upstream)
            .unwrap_or_else(|| panic!("missing base movement for {id}: {changes:#?}"));
        assert_eq!(change["baseBeforeSha"], repo(before, id)["baseSha"]);
        assert_eq!(change["baseAfterSha"], *upstream);
        let recorded: Vec<_> = changes
            .iter()
            .filter(|c| c["repoId"] == id)
            .flat_map(|c| c["commits"].as_array().into_iter().flatten())
            .map(|sha| sha.as_str().unwrap())
            .collect();
        assert_eq!(
            recorded.len(),
            count,
            "each rewritten feature commit is recorded once"
        );
        assert_eq!(recorded.iter().collect::<BTreeSet<_>>().len(), count);
        for change in changes.iter().filter(|c| c["repoId"] == id) {
            for key in ["commits", "droppedCommits"] {
                assert!(
                    !change[key]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|sha| sha == upstream),
                    "upstream recorded as feature work: {change}"
                );
            }
        }
        for group in after["commitGroups"].as_array().unwrap() {
            assert!(!group["commits"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["repoId"] == id && c["sha"] == *upstream));
        }
    }
}

#[test]
fn squash_two_groups_in_two_repos_preserves_full_audit_and_validates() {
    let f = Fixture::new();
    let before = f.bundle();
    f.run(&["squash", "-m", "Combined change"]);
    let after = f.bundle();
    f.assert_heads_and_counts(&after, 1);
    f.assert_one_group(&after, "Combined change");
    for id in REPOS {
        assert_eq!(repo(&before, id)["baseSha"], repo(&after, id)["baseSha"]);
        assert_eq!(
            git(&f.checkout(id), ["show", "-s", "--format=%s", "HEAD"]).trim(),
            "Combined change"
        );
        assert_eq!(
            fs::read_to_string(f.checkout(id).join("app.txt")).unwrap(),
            "feature content\n"
        );
        assert_eq!(
            fs::read_to_string(f.checkout(id).join("second.txt")).unwrap(),
            "second feature\n"
        );
    }
    assert_superseded(&before, &after);
    f.run(&["bundle", "validate"]);
}

#[test]
fn rebase_advances_bases_and_recreates_groups_without_upstream_commits() {
    let f = Fixture::new();
    let before = f.bundle();
    let upstream = f.advance_upstream(false);
    f.run(&["rebase"]);
    let after = f.bundle();
    f.assert_heads_and_counts(&after, 2);
    assert_base_movement(&before, &after, &upstream, 2);
    assert_superseded(&before, &after);
    let old = before["commitGroups"].as_array().unwrap();
    let new = after["commitGroups"].as_array().unwrap();
    assert_eq!(new.len(), old.len());
    for (old, new) in old.iter().zip(new) {
        assert_eq!(new["message"], old["message"]);
        assert_ne!(new["id"], old["id"]);
        assert_eq!(new["commits"].as_array().unwrap().len(), 2);
        for id in REPOS {
            let commit = new["commits"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["repoId"] == id)
                .unwrap();
            let sha = commit["sha"].as_str().unwrap();
            assert!(git_success(
                &f.checkout(id),
                ["merge-base", "--is-ancestor", sha, "HEAD"]
            ));
            assert_eq!(
                git(&f.checkout(id), ["show", "-s", "--format=%s", sha]).trim(),
                new["message"].as_str().unwrap()
            );
        }
    }
    f.run(&["bundle", "validate"]);
}

#[test]
fn rebase_with_squash_records_one_group_on_new_bases() {
    let f = Fixture::new();
    let before = f.bundle();
    let upstream = f.advance_upstream(false);
    f.run(&["rebase", "--squash", "-m", "Combined change"]);
    let after = f.bundle();
    f.assert_heads_and_counts(&after, 1);
    f.assert_one_group(&after, "Combined change");
    assert_base_movement(&before, &after, &upstream, 1);
    assert_superseded(&before, &after);
    f.run(&["bundle", "validate"]);
}

#[test]
fn multi_repo_conflict_continue_finishes_all_repos_and_records_once() {
    conflict_continue(false);
}

#[test]
fn multi_repo_conflict_squash_continue_finishes_all_repos_and_records_once() {
    conflict_continue(true);
}

fn conflict_continue(squash: bool) {
    let f = Fixture::new();
    let before = f.bundle();
    let upstream = f.advance_upstream(true);
    let args = if squash {
        vec!["rebase", "--squash", "-m", "Combined change"]
    } else {
        vec!["rebase"]
    };
    let failure = f.fail(&args);
    assert!(failure.to_lowercase().contains("conflict"), "{failure}");
    assert_ne!(
        f.head("alpha"),
        repo(&before, "alpha")["headSha"].as_str().unwrap(),
        "first repo must already be rebased before the second conflicts"
    );
    assert_eq!(
        f.bundle(),
        before,
        "a partial rewrite must not change the ledger"
    );
    f.assert_pending_rebase_blocks_writes();
    assert!(!git(
        &f.checkout("beta"),
        ["diff", "--name-only", "--diff-filter=U"]
    )
    .trim()
    .is_empty());
    fs::write(
        f.checkout("beta").join("app.txt"),
        "resolved feature and upstream\n",
    )
    .unwrap();
    git(&f.checkout("beta"), ["add", "app.txt"]);
    f.run(&["rebase", "--continue"]);
    let after = f.bundle();
    let count = if squash { 1 } else { 2 };
    f.assert_heads_and_counts(&after, count);
    if squash {
        f.assert_one_group(&after, "Combined change");
    } else {
        assert_eq!(after["commitGroups"].as_array().unwrap().len(), 2);
    }
    assert_base_movement(&before, &after, &upstream, count);
    assert_superseded(&before, &after);
    assert_eq!(
        fs::read_to_string(f.checkout("beta").join("app.txt")).unwrap(),
        "resolved feature and upstream\n"
    );
    let settled = fs::read(f.bundle_path()).unwrap();
    f.run(&["sync"]);
    assert_eq!(
        fs::read(f.bundle_path()).unwrap(),
        settled,
        "continuation must leave no unrecorded rewrite"
    );
    f.run(&["bundle", "validate"]);
}

#[test]
fn multi_repo_conflict_abort_restores_original_heads_and_exact_ledger() {
    conflict_abort(false);
}

#[test]
fn multi_repo_conflict_squash_abort_restores_original_heads_and_exact_ledger() {
    conflict_abort(true);
}

fn conflict_abort(squash: bool) {
    let f = Fixture::new();
    let before = f.bundle();
    let ledger = fs::read(f.bundle_path()).unwrap();
    let history = f.history();
    f.advance_upstream(true);
    let args = if squash {
        vec!["rebase", "--squash", "-m", "Combined change"]
    } else {
        vec!["rebase"]
    };
    let failure = f.fail(&args);
    assert!(failure.to_lowercase().contains("conflict"), "{failure}");
    assert_ne!(
        f.head("alpha"),
        repo(&before, "alpha")["headSha"].as_str().unwrap(),
        "first repo must already be rebased before the second conflicts"
    );
    assert_eq!(
        f.bundle(),
        before,
        "a partial rewrite must not change the ledger"
    );
    f.assert_pending_rebase_blocks_writes();
    f.run(&["rebase", "--abort"]);
    assert_eq!(fs::read(f.bundle_path()).unwrap(), ledger);
    assert_eq!(f.history(), history);
    f.assert_heads_and_counts(&before, 2);
    for id in REPOS {
        assert_eq!(
            git(&f.checkout(id), ["symbolic-ref", "--short", "HEAD"]).trim(),
            "knit/rewrite"
        );
        assert_eq!(
            fs::read_to_string(f.checkout(id).join("app.txt")).unwrap(),
            "feature content\n"
        );
        assert!(!f.checkout(id).join("upstream.txt").exists());
    }
    f.run(&["bundle", "validate"]);
}

#[test]
fn manual_git_rebase_then_sync_advances_bases_without_importing_upstream() {
    let f = Fixture::new();
    let before = f.bundle();
    let upstream = f.advance_upstream(false);
    for id in REPOS {
        git(&f.checkout(id), ["fetch", "origin"]);
        git(&f.checkout(id), ["rebase", "origin/main"]);
    }
    f.run(&["sync"]);
    let after = f.bundle();
    f.assert_heads_and_counts(&after, 2);
    assert_base_movement(&before, &after, &upstream, 2);
    assert_superseded(&before, &after);
    let settled = fs::read(f.bundle_path()).unwrap();
    f.run(&["sync"]);
    assert_eq!(fs::read(f.bundle_path()).unwrap(), settled);
    f.run(&["bundle", "validate"]);
}

#[test]
fn soft_reset_then_knit_commit_leaves_exactly_one_live_group() {
    let f = Fixture::new();
    let before = f.bundle();
    for id in REPOS {
        git(
            &f.checkout(id),
            [
                "reset",
                "--soft",
                repo(&before, id)["baseSha"].as_str().unwrap(),
            ],
        );
    }
    f.run(&["commit", "-m", "Replacement change"]);
    let after = f.bundle();
    f.assert_heads_and_counts(&after, 1);
    f.assert_one_group(&after, "Replacement change");
    assert_superseded(&before, &after);
    f.run(&["bundle", "validate"]);
}

#[test]
fn rewrite_preflight_rejects_dirty_second_repo_without_touching_first() {
    for command in ["squash", "rebase"] {
        let f = Fixture::new();
        let before = f.bundle();
        let ledger = fs::read(f.bundle_path()).unwrap();
        f.advance_upstream(false);
        fs::write(f.checkout("beta").join("second.txt"), "uncommitted work\n").unwrap();
        let args = if command == "squash" {
            vec![command, "-m", "Combined change"]
        } else {
            vec![command]
        };
        let failure = f.fail(&args);
        assert!(
            failure.contains("dirty")
                || failure.contains("clean")
                || failure.contains("uncommitted")
                || failure.contains("stashed"),
            "{failure}"
        );
        assert_eq!(fs::read(f.bundle_path()).unwrap(), ledger);
        for id in REPOS {
            assert_eq!(repo(&before, id)["headSha"], f.head(id));
        }
        assert_eq!(
            fs::read_to_string(f.checkout("beta").join("second.txt")).unwrap(),
            "uncommitted work\n"
        );
    }
}

#[test]
fn rewrite_preflight_rejects_detached_second_repo_without_touching_first() {
    for command in ["squash", "rebase"] {
        let f = Fixture::new();
        let before = f.bundle();
        let ledger = fs::read(f.bundle_path()).unwrap();
        f.advance_upstream(false);
        git(&f.checkout("beta"), ["checkout", "--detach"]);
        let args = if command == "squash" {
            vec![command, "-m", "Combined change"]
        } else {
            vec![command]
        };
        let failure = f.fail(&args);
        assert!(
            failure.contains("branch") || failure.contains("detached"),
            "{failure}"
        );
        assert_eq!(fs::read(f.bundle_path()).unwrap(), ledger);
        for id in REPOS {
            assert_eq!(repo(&before, id)["headSha"], f.head(id));
        }
        assert!(!git_success(
            &f.checkout("beta"),
            ["symbolic-ref", "-q", "HEAD"]
        ));
    }
}

#[test]
fn offline_rebase_uses_cached_origin_base_when_remote_is_unavailable() {
    offline_rebase(false);
}

#[test]
fn offline_rebase_falls_back_to_local_base_without_cached_origin_ref() {
    offline_rebase(true);
}

fn offline_rebase(local_fallback: bool) {
    let f = Fixture::new();
    let before = f.bundle();
    let upstream = f.advance_upstream(false);
    for (id, base) in REPOS.into_iter().zip(&upstream) {
        let checkout = f.checkout(id);
        git(&checkout, ["fetch", "origin"]);
        if local_fallback {
            // The source fixture has main checked out; update its tree along
            // with the ref, then remove the cached remote branch entirely.
            git(&f.root.join(id), ["reset", "--hard", base]);
            git(&checkout, ["update-ref", "-d", "refs/remotes/origin/main"]);
            assert!(!git_success(
                &checkout,
                ["rev-parse", "--verify", "refs/remotes/origin/main"]
            ));
            assert_eq!(
                git(&checkout, ["rev-parse", "refs/heads/main"]).trim(),
                base
            );
        } else {
            assert_eq!(
                git(&checkout, ["rev-parse", "refs/remotes/origin/main"]).trim(),
                base
            );
            assert_eq!(
                git(&checkout, ["rev-parse", "refs/heads/main"]).trim(),
                repo(&before, id)["baseSha"].as_str().unwrap()
            );
        }
        // Keep configured URLs intact while making the actual remote absent.
        fs::rename(
            f.root.join(format!("{id}.git")),
            f.root.join(format!("{id}-unavailable.git")),
        )
        .unwrap();
        assert!(!git_success(&checkout, ["fetch", "origin"]));
    }
    f.run(&["rebase", "--offline"]);
    let after = f.bundle();
    f.assert_heads_and_counts(&after, 2);
    assert_base_movement(&before, &after, &upstream, 2);
    assert_superseded(&before, &after);
    assert_eq!(after["commitGroups"].as_array().unwrap().len(), 2);
    for id in REPOS {
        assert_eq!(
            fs::read_to_string(f.checkout(id).join("upstream.txt")).unwrap(),
            "upstream content\n"
        );
    }
    f.run(&["bundle", "validate"]);
}
