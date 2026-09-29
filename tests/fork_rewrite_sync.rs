mod common;

use common::*;
use serde_json::Value;
use std::{fs, path::PathBuf};

const SOURCE: &str = "https://github.com/contributor/widget.git";
const TARGET: &str = "https://github.com/upstream/widget.git";
const BRANCH: &str = "knit/rewrite";

struct Fixture {
    root: PathBuf,
    home: PathBuf,
    workspace: PathBuf,
    checkout: PathBuf,
    upstream: PathBuf,
    fork: PathBuf,
    server: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = unique_temp_dir();
        let home = root.join("home");
        let workspace = root.join("workspace");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        let (upstream, local, _) = init_remote_repo(&root, "widget");
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
        let server = root.join("sync-server");
        let url = spawn_fake_remote_push_api(&server);
        fs::write(server.join("stateful-artifacts"), "").unwrap();
        let f = Self {
            checkout: workspace.join(".knit/worktrees/rewrite/widget"),
            root,
            home,
            workspace,
            upstream,
            fork,
            server,
        };
        f.run(&["init", "demo"]);
        f.run(&["project", "add", "widget", local.to_str().unwrap()]);
        f.run(&["bundle", "rewrite"]);
        append_line(&f.checkout.join("app.txt"), "first change");
        f.run(&["commit", "--all", "-m", "First change"]);
        // Portable forge identities use Git URL rewrites to reach local bare repos only.
        git(&local, ["remote", "set-url", "origin", TARGET]);
        git(&local, ["remote", "set-url", "--push", "origin", SOURCE]);
        for (url, path) in [(SOURCE, &f.fork), (TARGET, &f.upstream)] {
            git(
                &local,
                ["config", &format!("url.{}.insteadOf", path.display()), url],
            );
        }
        // An upstream branch with the same name must never supply the fork lease.
        git(&f.upstream, ["branch", BRANCH, "main"]);
        f.run(&["remote", "add", "hosted", &url]);
        f
    }

    fn run(&self, args: &[&str]) -> String {
        knit_with_env(
            &self.workspace,
            args,
            &[
                ("HOME", self.home.to_str().unwrap()),
                ("KNIT_REMOTE_TOKEN", "test-token"),
            ],
        )
    }

    fn fail(&self, args: &[&str]) -> String {
        knit_fails_with_env(
            &self.workspace,
            args,
            &[
                ("HOME", self.home.to_str().unwrap()),
                ("KNIT_REMOTE_TOKEN", "test-token"),
            ],
        )
    }

    fn bundle(&self) -> Value {
        serde_json::from_slice(
            &fs::read(self.workspace.join(".knit/bundles/rewrite.bundle.json")).unwrap(),
        )
        .unwrap()
    }

    fn remote_hash(&self) -> String {
        fs::read_to_string(self.server.join("rb-rewrite.artifact-current")).unwrap()
    }

    fn last_body(&self) -> Value {
        let bodies = fs::read_to_string(self.server.join("artifact-rewrite.bodies")).unwrap();
        serde_json::from_str(bodies.lines().last().unwrap()).unwrap()
    }

    fn rewrite(&self) {
        git(&self.checkout, ["reset", "--soft", "main"]);
        git(&self.checkout, ["commit", "-m", "Rewritten change"]);
    }
}

#[test]
fn fork_rewrite_push_and_subsequent_artifact_lease_use_last_successful_receipts() {
    let f = Fixture::new();
    f.run(&["push"]);
    let old_head = git(&f.fork, ["rev-parse", BRANCH]);
    let first_hash = f.remote_hash();
    assert_eq!(f.bundle()["syncTargets"][0]["artifactHash"], first_hash);
    assert_eq!(f.bundle()["repos"][0]["sourceRemote"], SOURCE);
    assert_eq!(f.bundle()["repos"][0]["targetRemote"], TARGET);

    f.rewrite();
    let rewritten = git(&f.checkout, ["rev-parse", "HEAD"]);
    assert_ne!(rewritten, old_head);
    // Require a force lease at the artifact endpoint from this point onward.
    fs::write(f.server.join("enforce-fast-forward"), "").unwrap();
    f.run(&["push", "--force-with-lease"]);
    assert_eq!(git(&f.fork, ["rev-parse", BRANCH]), rewritten);
    assert_eq!(f.bundle()["repos"][0]["headSha"], rewritten.trim());
    assert_eq!(f.bundle()["repos"][0]["sourceRemote"], SOURCE);
    assert_eq!(f.bundle()["repos"][0]["targetRemote"], TARGET);
    assert_eq!(
        git(&f.upstream, ["rev-parse", BRANCH]),
        git(&f.upstream, ["rev-parse", "main"])
    );
    assert_eq!(f.last_body()["expectedArtifactHash"], first_hash);
    assert_eq!(
        f.last_body()["payload"]["repos"][0]["headSha"],
        rewritten.trim()
    );
    assert!(f.bundle()["nodes"].as_array().unwrap().iter().any(|node| {
        node["type"] == "git.observed"
            && node["repoChanges"].as_array().is_some_and(|changes| {
                changes
                    .iter()
                    .any(|change| change["movement"] == "diverged")
            })
    }));
    let second_hash = f.remote_hash();
    assert_ne!(second_hash, first_hash);
    assert_eq!(f.bundle()["syncTargets"][0]["artifactHash"], second_hash);

    f.run(&["sync", "push", "--force-with-lease"]);
    assert_eq!(f.last_body()["expectedArtifactHash"], second_hash);
    assert_eq!(
        f.bundle()["syncTargets"][0]["artifactHash"],
        f.remote_hash()
    );
    fs::remove_dir_all(f.root).unwrap();
}

#[test]
fn fork_rewrite_refuses_a_concurrent_fork_update_before_artifact_publication() {
    let f = Fixture::new();
    f.run(&["push"]);
    let artifact_hash = f.remote_hash();
    let old_head = git(&f.fork, ["rev-parse", BRANCH]);
    let tree = git(&f.fork, ["rev-parse", &format!("{BRANCH}^{{tree}}")]);
    configure_git_user(&f.fork);
    let concurrent = git(
        &f.fork,
        [
            "commit-tree",
            tree.trim(),
            "-p",
            old_head.trim(),
            "-m",
            "Concurrent change",
        ],
    );
    git(
        &f.fork,
        [
            "update-ref",
            &format!("refs/heads/{BRANCH}"),
            concurrent.trim(),
        ],
    );
    f.rewrite();
    let failure = f.fail(&["push", "--force-with-lease"]);
    assert!(
        failure.contains("stale info") || failure.contains("lease"),
        "{failure}"
    );
    assert_eq!(git(&f.fork, ["rev-parse", BRANCH]), concurrent);
    assert_eq!(f.remote_hash(), artifact_hash);
    fs::remove_dir_all(f.root).unwrap();
}

#[test]
fn fork_rewrite_refuses_a_concurrent_hosted_artifact_update() {
    let f = Fixture::new();
    f.run(&["push"]);
    let known_hash = f.remote_hash();
    fs::write(
        f.server.join("rb-rewrite.artifact-current"),
        "concurrent-artifact",
    )
    .unwrap();
    let failure = f.fail(&["sync", "push", "--force-with-lease"]);
    assert!(failure.contains("remote artifact changed"), "{failure}");
    assert_eq!(f.last_body()["expectedArtifactHash"], known_hash);
    assert_eq!(f.remote_hash(), "concurrent-artifact");
    assert_eq!(f.bundle()["syncTargets"][0]["artifactHash"], known_hash);
    fs::remove_dir_all(f.root).unwrap();
}
