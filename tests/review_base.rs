mod common;

use common::*;
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
};

const FORK: &str = "https://github.com/contributor/widget.git";
const UPSTREAM: &str = "https://github.com/upstream/widget.git";
const BUNDLE: &str = "feature";
const BRANCH: &str = "knit/feature";

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
    checkout: PathBuf,
    collaborator: PathBuf,
}

impl Fixture {
    /// A project repo whose checkout fetches `origin`. With `fork`, `origin` is
    /// a fork whose `main` stays at the seed commit while upstream moves on.
    fn new(fork: bool) -> Self {
        let root = unique_temp_dir();
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let (upstream, local, collaborator) = init_remote_repo(&root, "widget");
        if fork {
            let fork = root.join("fork.git");
            git(
                &root,
                [
                    "clone",
                    "--bare",
                    upstream.to_str().unwrap(),
                    fork.to_str().unwrap(),
                ],
            );
            git(&local, ["remote", "set-url", "origin", FORK]);
            for (url, path) in [(FORK, &fork), (UPSTREAM, &upstream)] {
                git(
                    &local,
                    ["config", &format!("url.{}.insteadOf", path.display()), url],
                );
            }
            git(&local, ["fetch", "origin"]);
        }
        knit(&workspace, ["init", "demo"]);
        knit(
            &workspace,
            ["project", "add", "widget", local.to_str().unwrap()],
        );
        knit(&workspace, ["bundle", BUNDLE]);
        let checkout = workspace.join(".knit/worktrees/feature/widget");
        fs::write(checkout.join("feature.txt"), "feature\n").unwrap();
        knit(&workspace, ["commit", "--all", "-m", "Feature change"]);
        Self {
            root,
            workspace,
            checkout,
            collaborator,
        }
    }

    fn bundle_path(&self) -> PathBuf {
        self.workspace.join(".knit/bundles/feature.bundle.json")
    }

    fn bundle(&self) -> Value {
        serde_json::from_slice(&fs::read(self.bundle_path()).unwrap()).unwrap()
    }

    /// Record a review against `base_branch`; with `fork`, of the fork's branch in upstream.
    fn record_review(&self, fork: bool, base_branch: &str) {
        let mut bundle = self.bundle();
        let repository = if fork {
            bundle["repos"][0]["sourceRemote"] = json!(FORK);
            bundle["repos"][0]["targetRemote"] = json!(UPSTREAM);
            "upstream/widget"
        } else {
            "contributor/widget"
        };
        bundle["publications"] = json!([{
            "repoId": "widget",
            "provider": "github",
            "kind": "pull_request",
            "number": 7,
            "url": format!("https://github.com/{repository}/pull/7"),
            "baseBranch": base_branch,
            "headBranch": BRANCH,
            "state": "OPEN",
            "updatedAt": "2026-01-01T00:00:00Z"
        }]);
        fs::write(
            self.bundle_path(),
            serde_json::to_vec_pretty(&bundle).unwrap(),
        )
        .unwrap();
    }

    /// Commit an unrelated file to `branch` of the repository the collaborator pushes to.
    fn advance(&self, branch: &str, file: &str) -> String {
        git(&self.collaborator, ["fetch", "origin"]);
        git(
            &self.collaborator,
            ["checkout", "-B", branch, &format!("origin/{branch}")],
        );
        fs::write(self.collaborator.join(file), format!("{file}\n")).unwrap();
        git(&self.collaborator, ["add", file]);
        git(&self.collaborator, ["commit", "-m", &format!("Add {file}")]);
        git(&self.collaborator, ["push", "origin", branch]);
        git(&self.collaborator, ["rev-parse", "HEAD"])
            .trim()
            .to_owned()
    }

    fn feature_parent(&self) -> String {
        git(&self.checkout, ["rev-parse", "HEAD^"])
            .trim()
            .to_owned()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn short(sha: &str) -> &str {
    &sha[..7]
}

fn origin_main(checkout: &Path) -> String {
    git(checkout, ["rev-parse", "refs/remotes/origin/main"])
        .trim()
        .to_owned()
}

#[test]
fn fork_rebase_targets_the_review_base_not_the_lagging_fork_main() {
    let f = Fixture::new(true);
    f.record_review(true, "main");
    let upstream_main = f.advance("main", "upstream.txt");

    let output = knit(&f.workspace, ["--bundle", BUNDLE, "rebase"]);

    assert_eq!(f.feature_parent(), upstream_main);
    assert_ne!(origin_main(&f.checkout), upstream_main);
    assert_eq!(f.bundle()["repos"][0]["baseSha"], json!(upstream_main));
    assert!(
        output.contains(&format!(
            "(base {}, review base github.com/upstream/widget main)",
            short(&upstream_main)
        )),
        "{output}"
    );
}

#[test]
fn fork_workspace_status_names_the_review_base_it_did_not_compare_against() {
    let f = Fixture::new(true);
    f.record_review(true, "main");
    let upstream_main = f.advance("main", "upstream.txt");

    let before = knit(&f.workspace, ["workspace", "status"]);
    assert!(before.contains("ahead=0 behind=0"), "{before}");
    assert!(
        before.contains(
            "note: ahead/behind compare main with origin/main; open reviews target another base:"
        ),
        "{before}"
    );
    assert!(
        before.contains(
            "feature: https://github.com/upstream/widget/pull/7 targets github.com/upstream/widget main (not fetched; run `knit --bundle feature fetch`)"
        ),
        "{before}"
    );

    knit(&f.workspace, ["--bundle", BUNDLE, "fetch"]);
    let after = knit(&f.workspace, ["workspace", "status"]);
    assert!(
        after.contains(&format!(
            "feature: https://github.com/upstream/widget/pull/7 targets github.com/upstream/widget main {}; origin/main behind=1, {BRANCH} behind=1",
            short(&upstream_main)
        )),
        "{after}"
    );
}

#[test]
fn rebase_follows_a_review_retargeted_to_another_branch() {
    let f = Fixture::new(false);
    git(&f.collaborator, ["push", "origin", "main:release"]);
    let release = f.advance("release", "release.txt");
    let main = f.advance("main", "main.txt");
    f.record_review(false, "release");

    let output = knit(&f.workspace, ["--bundle", BUNDLE, "rebase"]);

    assert_eq!(f.feature_parent(), release);
    assert_ne!(release, main);
    assert!(
        output.contains(&format!(
            "(base {}, review base origin/release)",
            short(&release)
        )),
        "{output}"
    );
    let status = knit(&f.workspace, ["workspace", "status"]);
    assert!(
        status.contains(&format!(
            "feature: https://github.com/contributor/widget/pull/7 targets origin/release {}; origin/main behind=1, {BRANCH} behind=0",
            short(&release)
        )),
        "{status}"
    );
}

#[test]
fn rebase_without_a_fork_still_uses_origin_main() {
    for review in [false, true] {
        let f = Fixture::new(false);
        if review {
            f.record_review(false, "main");
        }
        let main = f.advance("main", "main.txt");

        let output = knit(&f.workspace, ["--bundle", BUNDLE, "rebase"]);

        assert_eq!(f.feature_parent(), main);
        assert!(
            output.contains(&format!("(base {})", short(&main))),
            "{output}"
        );
        let status = knit(&f.workspace, ["workspace", "status"]);
        assert!(!status.contains("note:"), "{status}");
    }
}

#[test]
fn rebase_accepts_repo_selectors() {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let (_, api, api_collaborator) = init_remote_repo(&root, "api");
    let (_, web, web_collaborator) = init_remote_repo(&root, "web");
    knit(&workspace, ["init", "demo"]);
    knit(&workspace, ["project", "add", "api", api.to_str().unwrap()]);
    knit(&workspace, ["project", "add", "web", web.to_str().unwrap()]);
    knit(&workspace, ["bundle", BUNDLE]);
    let checkouts = workspace.join(".knit/worktrees/feature");
    for repo in ["api", "web"] {
        fs::write(checkouts.join(repo).join("feature.txt"), "feature\n").unwrap();
    }
    knit(&workspace, ["commit", "--all", "-m", "Feature change"]);
    let mut heads = Vec::new();
    for collaborator in [&api_collaborator, &web_collaborator] {
        fs::write(collaborator.join("base.txt"), "base\n").unwrap();
        git(collaborator, ["add", "base.txt"]);
        git(collaborator, ["commit", "-m", "Advance base"]);
        git(collaborator, ["push", "origin", "main"]);
        heads.push(git(collaborator, ["rev-parse", "HEAD"]).trim().to_owned());
    }
    let web_before = git(&checkouts.join("web"), ["rev-parse", "HEAD"]);

    knit(&workspace, ["--bundle", BUNDLE, "rebase", "api"]);

    assert_eq!(
        git(&checkouts.join("api"), ["rev-parse", "HEAD^"]).trim(),
        heads[0]
    );
    assert_eq!(
        git(&checkouts.join("web"), ["rev-parse", "HEAD"]),
        web_before
    );

    let error = knit_fails(
        &workspace,
        ["--bundle", BUNDLE, "rebase", "api", "--squash"],
    );
    assert!(error.contains("cannot be used with"), "{error}");
    let error = knit_fails(&workspace, ["--bundle", BUNDLE, "rebase", "nope"]);
    assert!(error.contains("No tracked repo matched `nope`"), "{error}");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn land_update_merges_the_same_review_base_rebase_uses() {
    let f = Fixture::new(false);
    git(&f.collaborator, ["push", "origin", "main:release"]);
    let release = f.advance("release", "release.txt");
    f.advance("main", "main.txt");
    f.record_review(false, "release");

    knit(&f.workspace, ["--bundle", BUNDLE, "land", "update"]);

    let parents = git(&f.checkout, ["show", "-s", "--format=%P", "HEAD"]);
    assert_eq!(parents.split_whitespace().nth(1), Some(release.as_str()));
    assert!(f.checkout.join("release.txt").exists());
    assert!(!f.checkout.join("main.txt").exists());
}

#[test]
fn status_and_rebase_flag_a_review_outside_origin_without_a_recorded_target() {
    let f = Fixture::new(true);
    f.record_review(true, "main");
    let mut bundle = f.bundle();
    let repo = bundle["repos"][0].as_object_mut().unwrap();
    repo.remove("sourceRemote");
    repo.remove("targetRemote");
    fs::write(f.bundle_path(), serde_json::to_vec_pretty(&bundle).unwrap()).unwrap();
    f.advance("main", "upstream.txt");
    let head = git(&f.checkout, ["rev-parse", "HEAD"]);

    let status = knit(&f.workspace, ["workspace", "status"]);
    assert!(
        status.contains("feature: https://github.com/upstream/widget/pull/7 is in github.com/upstream/widget, but the bundle records no contribution target, so its base is unknown here; `knit --bundle feature rebase` explains how to record it"),
        "{status}"
    );
    let error = knit_fails(&f.workspace, ["--bundle", BUNDLE, "rebase"]);
    assert!(
        error.contains("knit --bundle feature publish create widget --source-remote origin --target-remote upstream"),
        "{error}"
    );
    assert_eq!(git(&f.checkout, ["rev-parse", "HEAD"]), head);
}

#[test]
fn a_review_in_origin_itself_is_not_flagged() {
    let f = Fixture::new(true);
    f.record_review(false, "main");
    let fork_main = origin_main(&f.checkout);

    let output = knit(&f.workspace, ["--bundle", BUNDLE, "rebase"]);

    assert!(
        output.contains(&format!("(base {})", short(&fork_main))),
        "{output}"
    );
    let status = knit(&f.workspace, ["workspace", "status"]);
    assert!(!status.contains("note:"), "{status}");
}
