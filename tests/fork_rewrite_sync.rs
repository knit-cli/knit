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
    local: PathBuf,
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
            local: local.clone(),
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

    fn set_fetch_refspec(&self, narrow: bool) {
        let refspec = if narrow {
            "+refs/heads/main:refs/remotes/origin/main"
        } else {
            "+refs/heads/*:refs/remotes/origin/*"
        };
        git(
            &self.local,
            ["config", "--replace-all", "remote.origin.fetch", refspec],
        );
    }

    fn plain_push(&self) -> String {
        git(&self.checkout, ["push", "origin", BRANCH]);
        let receipts = git(&self.checkout, ["for-each-ref", "refs/knit/contributions/"]);
        assert!(
            receipts.trim().is_empty(),
            "unexpected Knit receipt: {receipts}"
        );
        git(&self.fork, ["rev-parse", BRANCH])
    }

    fn foreign_fork_commit(&self) -> String {
        let old_head = git(&self.fork, ["rev-parse", BRANCH]);
        let tree = git(&self.fork, ["rev-parse", &format!("{BRANCH}^{{tree}}")]);
        configure_git_user(&self.fork);
        let concurrent = git(
            &self.fork,
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
            &self.fork,
            [
                "update-ref",
                &format!("refs/heads/{BRANCH}"),
                concurrent.trim(),
            ],
        );
        concurrent
    }

    fn rewrite(&self) {
        git(&self.checkout, ["reset", "--soft", "main"]);
        git(&self.checkout, ["commit", "-m", "Rewritten change"]);
    }
}

#[test]
fn native_explicit_lease_uses_fork_push_url_and_rejects_concurrent_changes() {
    let f = Fixture::new();
    let first = f.plain_push().trim().to_owned();
    let upstream_before = git(&f.upstream, ["rev-parse", BRANCH]);
    // A fetch observes the upstream branch, which intentionally differs from
    // the fork. An explicit native lease still applies to the push endpoint.
    git(&f.checkout, ["fetch", "origin"]);
    f.rewrite();
    let rewritten = git(&f.checkout, ["rev-parse", "HEAD"]).trim().to_owned();
    let lease = format!("--force-with-lease=refs/heads/{BRANCH}:{first}");
    let refspec = format!("HEAD:refs/heads/{BRANCH}");
    git(
        &f.checkout,
        ["push", lease.as_str(), "origin", refspec.as_str()],
    );
    assert_eq!(git(&f.fork, ["rev-parse", BRANCH]).trim(), rewritten);
    assert_eq!(git(&f.upstream, ["rev-parse", BRANCH]), upstream_before);

    let concurrent = f.foreign_fork_commit();
    let stale_lease = format!("--force-with-lease=refs/heads/{BRANCH}:{rewritten}");
    assert!(!git_success(
        &f.checkout,
        ["push", stale_lease.as_str(), "origin", refspec.as_str()]
    ));
    assert_eq!(git(&f.fork, ["rev-parse", BRANCH]), concurrent);
    assert_eq!(git(&f.upstream, ["rev-parse", BRANCH]), upstream_before);
    fs::remove_dir_all(f.root).unwrap();
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
    let concurrent = f.foreign_fork_commit();
    f.rewrite();
    let failure = f.fail(&["push", "--force-with-lease"]);
    assert!(
        failure.contains("never recorded") && failure.contains(concurrent.trim()),
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

fn check_plain_fork_rewrite(narrow: bool, fetch_upstream: bool) {
    let f = Fixture::new();
    f.set_fetch_refspec(narrow);
    let old_head = f.plain_push();
    let native = git(
        &f.checkout,
        [
            "for-each-ref",
            "--format=%(objectname)",
            &format!("refs/remotes/origin/{BRANCH}"),
        ],
    );
    if narrow {
        assert!(native.trim().is_empty());
    } else {
        assert_eq!(native, old_head);
    }
    if fetch_upstream {
        git(&f.checkout, ["fetch", "origin"]);
        assert_eq!(
            git(
                &f.checkout,
                ["rev-parse", &format!("refs/remotes/origin/{BRANCH}")]
            ),
            git(&f.upstream, ["rev-parse", "main"])
        );
    }
    f.rewrite();
    let rewritten = git(&f.checkout, ["rev-parse", "HEAD"]);
    assert_ne!(old_head, rewritten);
    let output = f.run(&["push", "--force-with-lease"]);
    assert!(
        output.contains("leasing against the remote tip"),
        "{output}"
    );
    assert_eq!(git(&f.fork, ["rev-parse", BRANCH]), rewritten);
    fs::remove_dir_all(f.root).unwrap();
}

#[test]
fn fork_rewrite_after_plain_push_without_feature_tracking_ref() {
    check_plain_fork_rewrite(true, false);
}

#[test]
fn fork_rewrite_after_plain_push_with_feature_tracking_ref() {
    check_plain_fork_rewrite(false, false);
}

#[test]
fn fork_rewrite_after_plain_push_ignores_upstream_tracking_ref() {
    check_plain_fork_rewrite(false, true);
}

#[test]
fn fork_rewrite_without_receipt_refuses_a_concurrent_fork_update() {
    let f = Fixture::new();
    f.plain_push();
    let concurrent = f.foreign_fork_commit();
    f.rewrite();
    let failure = f.fail(&["push", "--force-with-lease"]);
    assert!(failure.contains("never recorded"), "{failure}");
    assert!(failure.contains(concurrent.trim()), "{failure}");
    assert!(failure.contains("git fetch"), "{failure}");
    assert!(
        !failure.contains("re-run the same `knit push`"),
        "{failure}"
    );
    assert_eq!(git(&f.fork, ["rev-parse", BRANCH]), concurrent);
    assert!(!f.server.join("artifact-rewrite.bodies").exists());
    fs::remove_dir_all(f.root).unwrap();
}

fn check_plain_origin_rewrite_without_tracking_ref(local_url: bool) {
    let f = Fixture::new();
    f.set_fetch_refspec(true);
    git(&f.local, ["config", "--unset-all", "remote.origin.pushurl"]);
    if local_url {
        git(
            &f.local,
            ["remote", "set-url", "origin", f.upstream.to_str().unwrap()],
        );
    }
    git(&f.checkout, ["push", "origin", BRANCH]);
    assert!(git(
        &f.checkout,
        ["for-each-ref", &format!("refs/remotes/origin/{BRANCH}")]
    )
    .trim()
    .is_empty());
    assert!(
        git(&f.checkout, ["for-each-ref", "refs/knit/contributions/"])
            .trim()
            .is_empty()
    );
    f.rewrite();
    let rewritten = git(&f.checkout, ["rev-parse", "HEAD"]);
    f.run(&["push", "--force-with-lease"]);
    assert_eq!(git(&f.upstream, ["rev-parse", BRANCH]), rewritten);
    fs::remove_dir_all(f.root).unwrap();
}

#[test]
fn fork_rewrite_accepts_plain_git_head_from_worktree_branch_reflog() {
    let f = Fixture::new();
    // This head never entered the Knit ledger: both the commit and push use Git.
    append_line(&f.checkout.join("app.txt"), "unrecorded local change");
    git(&f.checkout, ["commit", "-am", "Local change"]);
    let old_head = f.plain_push();
    assert!(!serde_json::to_string(&f.bundle())
        .unwrap()
        .contains(old_head.trim()));
    f.rewrite();
    let reflog = git(
        &f.checkout,
        ["log", "-g", "--format=%H", &format!("refs/heads/{BRANCH}")],
    );
    assert!(reflog.lines().any(|sha| sha == old_head.trim()));
    f.run(&["push", "--force-with-lease"]);
    assert_eq!(
        git(&f.fork, ["rev-parse", BRANCH]),
        git(&f.checkout, ["rev-parse", "HEAD"])
    );
    fs::remove_dir_all(f.root).unwrap();
}

#[test]
fn fork_rewrite_accepts_ledger_tip_after_branch_reflog_expired() {
    let f = Fixture::new();
    f.plain_push();
    f.rewrite();
    git(
        &f.checkout,
        [
            "reflog",
            "expire",
            "--expire=all",
            &format!("refs/heads/{BRANCH}"),
        ],
    );
    assert!(git(
        &f.checkout,
        ["log", "-g", "--format=%H", &format!("refs/heads/{BRANCH}")]
    )
    .trim()
    .is_empty());
    f.run(&["push", "--force-with-lease"]);
    assert_eq!(
        git(&f.fork, ["rev-parse", BRANCH]),
        git(&f.checkout, ["rev-parse", "HEAD"])
    );
    fs::remove_dir_all(f.root).unwrap();
}

#[cfg(unix)]
#[test]
fn fork_rewrite_refuses_tip_changed_after_lease_resolution_with_inspection_hint() {
    use std::os::unix::fs::PermissionsExt;

    let f = Fixture::new();
    let old_head = f.plain_push();
    let concurrent = f.foreign_fork_commit();
    git(
        &f.fork,
        [
            "update-ref",
            &format!("refs/heads/{BRANCH}"),
            old_head.trim(),
        ],
    );
    f.rewrite();
    // Git runs this hook after Knit snapshots the lease and before sending updates.
    let hooks = f.root.join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("pre-push");
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\ngit --git-dir='{}' update-ref refs/heads/{BRANCH} {}\n",
            f.fork.display(),
            concurrent.trim()
        ),
    )
    .unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    git(
        &f.checkout,
        ["config", "core.hooksPath", hooks.to_str().unwrap()],
    );
    let failure = f.fail(&["push", "--force-with-lease"]);
    assert!(failure.contains("git fetch"), "{failure}");
    assert!(
        !failure.contains("re-run the same `knit push`"),
        "{failure}"
    );
    assert_eq!(git(&f.fork, ["rev-parse", BRANCH]), concurrent);
    assert!(!f.server.join("artifact-rewrite.bodies").exists());
    fs::remove_dir_all(f.root).unwrap();
}

#[test]
fn plain_origin_rewrite_without_feature_tracking_ref() {
    check_plain_origin_rewrite_without_tracking_ref(false);
}

#[test]
fn local_origin_rewrite_without_feature_tracking_ref() {
    check_plain_origin_rewrite_without_tracking_ref(true);
}

fn push_rewrite_fixture() -> Fixture {
    let f = Fixture::new();
    git(&f.local, ["config", "--unset-all", "remote.origin.pushurl"]);
    git(
        &f.local,
        ["remote", "set-url", "origin", f.upstream.to_str().unwrap()],
    );
    git(
        &f.local,
        [
            "config",
            &format!("url.{}.pushInsteadOf", f.fork.display()),
            f.upstream.to_str().unwrap(),
        ],
    );
    f
}

#[test]
fn fork_rewrite_with_push_instead_of_leases_actual_push_destination() {
    let f = push_rewrite_fixture();
    let first = f.plain_push();
    f.rewrite();
    f.run(&["push", "--force-with-lease"]);
    let rewritten = git(&f.checkout, ["rev-parse", "HEAD"]);
    assert_ne!(first, rewritten);
    assert_eq!(git(&f.fork, ["rev-parse", BRANCH]), rewritten);
    assert_eq!(
        git(&f.upstream, ["rev-parse", BRANCH]),
        git(&f.upstream, ["rev-parse", "main"])
    );
    fs::remove_dir_all(f.root).unwrap();
}

#[test]
fn fork_rewrite_with_push_instead_of_ignores_foreign_upstream_tracking_tip() {
    let f = push_rewrite_fixture();
    f.plain_push();
    let concurrent = f.foreign_fork_commit();
    git(
        &f.upstream,
        [
            "fetch",
            f.fork.to_str().unwrap(),
            &format!("+refs/heads/{BRANCH}:refs/heads/{BRANCH}"),
        ],
    );
    git(&f.checkout, ["fetch", "origin"]);
    assert_eq!(
        git(
            &f.checkout,
            ["rev-parse", &format!("refs/remotes/origin/{BRANCH}")]
        ),
        concurrent
    );
    f.rewrite();
    let failure = f.fail(&["push", "--force-with-lease"]);
    assert!(failure.contains("never recorded"), "{failure}");
    assert!(failure.contains(concurrent.trim()), "{failure}");
    assert_eq!(git(&f.fork, ["rev-parse", BRANCH]), concurrent);
    assert!(!f.server.join("artifact-rewrite.bodies").exists());
    fs::remove_dir_all(f.root).unwrap();
}

#[cfg(unix)]
fn check_split_transport_aliases_refuse_unknown_tip(with_role_receipt: bool) {
    use std::os::unix::fs::PermissionsExt;

    let f = Fixture::new();
    // These spellings share a portable identity, but Git sends them to different repos.
    let ssh_alias = "git@github.com:upstream/widget.git";
    git(
        &f.local,
        ["remote", "set-url", "--push", "origin", ssh_alias],
    );
    git(
        &f.local,
        [
            "config",
            "--add",
            &format!("url.{}.insteadOf", f.fork.display()),
            ssh_alias,
        ],
    );
    f.plain_push();
    let unknown = f.foreign_fork_commit();
    git(
        &f.upstream,
        [
            "fetch",
            f.fork.to_str().unwrap(),
            &format!("+refs/heads/{BRANCH}:refs/heads/{BRANCH}"),
        ],
    );
    git(&f.checkout, ["fetch", "origin"]);
    assert_eq!(
        git(
            &f.checkout,
            ["rev-parse", &format!("refs/remotes/origin/{BRANCH}")]
        ),
        unknown
    );
    assert!(!serde_json::to_string(&f.bundle())
        .unwrap()
        .contains(unknown.trim()));
    let reflog = git(
        &f.checkout,
        ["log", "-g", "--format=%H", &format!("refs/heads/{BRANCH}")],
    );
    assert!(!reflog.lines().any(|sha| sha == unknown.trim()));
    if with_role_receipt {
        let mut repo: knit::model::RepoEntry =
            serde_json::from_value(f.bundle()["repos"][0].clone()).unwrap();
        repo.source_remote = Some(TARGET.into());
        repo.target_remote = Some(TARGET.into());
        let upstream_role =
            knit::contribution::fetch_ref(&f.checkout, &repo, BRANCH, true).unwrap();
        repo.source_remote = Some(ssh_alias.into());
        assert_eq!(
            upstream_role,
            knit::contribution::role_ref(&repo, BRANCH, true).unwrap()
        );
        assert_eq!(git(&f.checkout, ["rev-parse", &upstream_role]), unknown);
        // Isolate the role receipt: no native feature tracking ref remains.
        git(
            &f.checkout,
            ["update-ref", "-d", &format!("refs/remotes/origin/{BRANCH}")],
        );
    }
    f.rewrite();
    let hooks = f.root.join("rejection-hooks");
    fs::create_dir_all(&hooks).unwrap();
    let marker = f.root.join("pre-push-reached");
    let hook = hooks.join("pre-push");
    fs::write(
        &hook,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    git(
        &f.checkout,
        ["config", "core.hooksPath", hooks.to_str().unwrap()],
    );
    let before = f.bundle();
    let failure = f.fail(&["push", "--force-with-lease"]);
    assert!(failure.contains("never recorded"), "{failure}");
    assert!(failure.contains(unknown.trim()), "{failure}");
    assert!(!marker.exists(), "push hook was reached: {failure}");
    assert_eq!(git(&f.fork, ["rev-parse", BRANCH]), unknown);
    assert_eq!(git(&f.upstream, ["rev-parse", BRANCH]), unknown);
    assert_eq!(f.bundle(), before);
    assert!(!f.server.join("artifact-rewrite.bodies").exists());
    fs::remove_dir_all(f.root).unwrap();
}

#[cfg(unix)]
#[test]
fn split_transport_aliases_do_not_borrow_native_upstream_receipt() {
    check_split_transport_aliases_refuse_unknown_tip(false);
}

#[cfg(unix)]
#[test]
fn split_transport_aliases_do_not_borrow_upstream_role_receipt() {
    check_split_transport_aliases_refuse_unknown_tip(true);
}

fn check_same_destination_alias_observation(native: bool) {
    let f = Fixture::new();
    let ssh_alias = "git@github.com:contributor/widget.git";
    git(&f.local, ["remote", "set-url", "origin", SOURCE]);
    git(
        &f.local,
        ["remote", "set-url", "--push", "origin", ssh_alias],
    );
    git(
        &f.local,
        [
            "config",
            "--add",
            &format!("url.{}.insteadOf", f.fork.display()),
            ssh_alias,
        ],
    );
    f.plain_push();
    let unknown = f.foreign_fork_commit();
    if native {
        git(&f.checkout, ["fetch", "origin"]);
    } else {
        f.set_fetch_refspec(true);
        git(
            &f.checkout,
            ["update-ref", "-d", &format!("refs/remotes/origin/{BRANCH}")],
        );
        let mut repo: knit::model::RepoEntry =
            serde_json::from_value(f.bundle()["repos"][0].clone()).unwrap();
        repo.source_remote = Some(SOURCE.into());
        repo.target_remote = Some(SOURCE.into());
        knit::contribution::fetch_ref(&f.checkout, &repo, BRANCH, true).unwrap();
    }
    assert!(!serde_json::to_string(&f.bundle())
        .unwrap()
        .contains(unknown.trim()));
    let reflog = git(
        &f.checkout,
        ["log", "-g", "--format=%H", &format!("refs/heads/{BRANCH}")],
    );
    assert!(!reflog.lines().any(|sha| sha == unknown.trim()));
    f.rewrite();
    let output = f.run(&["push", "--force-with-lease"]);
    assert!(!output.contains("no Knit push receipt"), "{output}");
    assert_eq!(
        git(&f.fork, ["rev-parse", BRANCH]),
        git(&f.checkout, ["rev-parse", "HEAD"])
    );
    fs::remove_dir_all(f.root).unwrap();
}

#[test]
fn same_destination_transport_aliases_share_native_observations() {
    check_same_destination_alias_observation(true);
}

#[test]
fn same_destination_transport_aliases_share_knit_observations() {
    check_same_destination_alias_observation(false);
}

#[test]
fn split_alias_source_observation_does_not_overwrite_upstream_tracking_ref() {
    let f = Fixture::new();
    let ssh_alias = "git@github.com:upstream/widget.git";
    git(
        &f.local,
        ["remote", "set-url", "--push", "origin", ssh_alias],
    );
    git(
        &f.local,
        [
            "config",
            "--add",
            &format!("url.{}.insteadOf", f.fork.display()),
            ssh_alias,
        ],
    );
    f.plain_push();
    let unknown = f.foreign_fork_commit();
    git(&f.checkout, ["fetch", "origin"]);
    let native = format!("refs/remotes/origin/{BRANCH}");
    let upstream_tip = git(&f.checkout, ["rev-parse", &native]);
    assert_ne!(upstream_tip, unknown);
    let mut repo: knit::model::RepoEntry =
        serde_json::from_value(f.bundle()["repos"][0].clone()).unwrap();
    repo.source_remote = Some(ssh_alias.into());
    repo.target_remote = Some(TARGET.into());
    knit::contribution::fetch_ref(&f.checkout, &repo, BRANCH, true).unwrap();
    assert_eq!(git(&f.checkout, ["rev-parse", &native]), upstream_tip);
    assert!(!serde_json::to_string(&f.bundle())
        .unwrap()
        .contains(unknown.trim()));
    f.rewrite();
    let output = f.run(&["push", "--force-with-lease"]);
    assert!(!output.contains("no Knit push receipt"), "{output}");
    assert_eq!(
        git(&f.fork, ["rev-parse", BRANCH]),
        git(&f.checkout, ["rev-parse", "HEAD"])
    );
    fs::remove_dir_all(f.root).unwrap();
}

#[test]
fn fork_rebase_uses_upstream_target_not_fork_source_base() {
    let f = Fixture::new();
    // Capture contribution roles using only the fixture's local remotes/server.
    f.run(&["push"]);
    let before = f.bundle();
    // Advance the two remotes independently: choosing the fork's main would
    // produce a valid rebase with the wrong base, which must be detected.
    let mut bases = Vec::new();
    for (name, remote) in [("target", &f.upstream), ("source", &f.fork)] {
        let writer = f.root.join(format!("{name}-writer"));
        git(
            &f.root,
            ["clone", remote.to_str().unwrap(), writer.to_str().unwrap()],
        );
        configure_git_user(&writer);
        fs::write(
            writer.join(format!("{name}.txt")),
            format!("{name} content\n"),
        )
        .unwrap();
        git(&writer, ["add", "."]);
        git(&writer, ["commit", "-m", &format!("Advance {name} base")]);
        git(&writer, ["push", "origin", "main"]);
        bases.push(git(&writer, ["rev-parse", "HEAD"]).trim().to_owned());
    }
    f.run(&["rebase"]);
    let after = f.bundle();
    let head = git(&f.checkout, ["rev-parse", "HEAD"]);
    assert_ne!(after["repos"][0]["headSha"], before["repos"][0]["headSha"]);
    assert_eq!(after["repos"][0]["headSha"], head.trim());
    assert_eq!(after["repos"][0]["baseSha"], bases[0]);
    assert_ne!(after["repos"][0]["baseSha"], bases[1]);
    assert!(f.checkout.join("target.txt").exists());
    assert!(!f.checkout.join("source.txt").exists());
    assert_eq!(
        git(
            &f.checkout,
            ["rev-list", "--count", &format!("{}..HEAD", bases[0])]
        )
        .trim(),
        "1"
    );
    assert_eq!(after["repos"][0]["sourceRemote"], SOURCE);
    assert_eq!(after["repos"][0]["targetRemote"], TARGET);
    let changes: Vec<_> = after["nodes"].as_array().unwrap()
        [before["nodes"].as_array().unwrap().len()..]
        .iter()
        .flat_map(|n| n["repoChanges"].as_array().into_iter().flatten())
        .collect();
    let movement = changes
        .iter()
        .find(|c| c["repoId"] == "widget" && c["baseAfterSha"] == bases[0])
        .unwrap();
    assert_eq!(movement["baseBeforeSha"], before["repos"][0]["baseSha"]);
    let recorded: Vec<_> = changes
        .iter()
        .filter(|c| c["repoId"] == "widget")
        .flat_map(|c| c["commits"].as_array().into_iter().flatten())
        .map(|sha| sha.as_str().unwrap())
        .collect();
    assert_eq!(recorded, vec![head.trim()]);
    f.run(&["bundle", "validate"]);
    fs::remove_dir_all(f.root).unwrap();
}

#[test]
fn manual_fork_rebase_ignores_feature_work_in_fallback_base_refs() {
    let f = Fixture::new();
    f.run(&["push"]);
    let before = f.bundle();
    let repo: knit::model::RepoEntry = serde_json::from_value(before["repos"][0].clone()).unwrap();
    let destination = knit::contribution::role_ref(&repo, "main", false).unwrap();
    let writer = f.root.join("target-writer");
    git(
        &f.root,
        [
            "clone",
            f.upstream.to_str().unwrap(),
            writer.to_str().unwrap(),
        ],
    );
    configure_git_user(&writer);
    fs::write(writer.join("upstream.txt"), "Upstream work\n").unwrap();
    git(&writer, ["add", "."]);
    git(&writer, ["commit", "-m", "Upstream work"]);
    git(&writer, ["push", "origin", "main"]);
    let base = git(&writer, ["rev-parse", "HEAD"]).trim().to_owned();
    git(
        &f.checkout,
        [
            "fetch",
            f.upstream.to_str().unwrap(),
            &format!("+refs/heads/main:{destination}"),
        ],
    );
    git(&f.checkout, ["rebase", &destination]);
    let head = git(&f.checkout, ["rev-parse", "HEAD"]).trim().to_owned();
    for reference in ["refs/heads/main", "refs/remotes/origin/main"] {
        git(&f.checkout, ["update-ref", reference, &head]);
    }
    f.run(&["sync"]);
    let after = f.bundle();
    assert_eq!(after["repos"][0]["baseSha"], base);
    assert_eq!(after["commitGroups"].as_array().unwrap().len(), 1);
    assert_eq!(
        after["commitGroups"][0]["message"],
        before["commitGroups"][0]["message"]
    );
    assert_eq!(after["commitGroups"][0]["commits"][0]["sha"], head);
    let observation = after["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|n| n["type"] == "git.observed")
        .unwrap();
    assert_eq!(observation["repoChanges"][0]["baseAfterSha"], base);
    assert_eq!(
        observation["repoChanges"][0]["commits"],
        serde_json::json!([])
    );
    assert_eq!(
        observation["rewrite"]["supersededGroups"][0],
        before["commitGroups"][0]
    );
    f.run(&["bundle", "validate"]);
    fs::remove_dir_all(f.root).unwrap();
}
