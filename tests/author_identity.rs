mod common;
use common::*;
use std::{fs, path::PathBuf, process::Command};

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
    checkout: PathBuf,
    remote: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = unique_temp_dir();
        let (remote, local, _) = init_remote_repo(&root, "repo");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        knit(&workspace, ["init", "example"]);
        knit(
            &workspace,
            ["project", "add", "repo", local.to_str().unwrap()],
        );
        knit(&workspace, ["bundle", "identity"]);
        let checkout = workspace.join(".knit/worktrees/identity/repo");
        git(&checkout, ["config", "commit.gpgsign", "false"]);
        Self {
            root,
            workspace,
            checkout,
            remote,
        }
    }
    fn injected(&self, args: &[&str]) -> (String, String, bool) {
        knit_split_output(
            &self.workspace,
            args,
            &[
                ("GIT_AUTHOR_NAME", "Injected Actor"),
                ("GIT_AUTHOR_EMAIL", "injected@example.test"),
            ],
        )
    }
    fn commit(&self, name: &str) {
        fs::write(self.checkout.join(name), name).unwrap();
        let (out, err, ok) = self.injected(&["commit", "--all", "-m", name]);
        assert!(ok, "{out}\n{err}");
        assert!(err.contains("ignoring GIT_AUTHOR_NAME"), "{err}");
        assert_eq!(
            git(&self.checkout, ["show", "-s", "--format=%an <%ae>", "HEAD"]).trim(),
            "Knit Smoke <knit@example.test>"
        );
    }
    fn foreign(&self) -> String {
        fs::write(self.checkout.join("foreign.txt"), "foreign change").unwrap();
        git(&self.checkout, ["add", "foreign.txt"]);
        // Model an already-polluted commit from a harness, without changing
        // the repository's configured identity.
        let output = Command::new("git")
            .args(["commit", "--allow-empty", "-m", "Foreign change"])
            .current_dir(&self.checkout)
            .env("GIT_AUTHOR_NAME", "Other Author")
            .env("GIT_AUTHOR_EMAIL", "other@example.test")
            .env("GIT_CONFIG_GLOBAL", isolated_git_config_global())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        git(&self.checkout, ["rev-parse", "HEAD"]).trim().into()
    }
}

#[test]
fn injected_author_is_ignored_for_commit_and_squash() {
    let f = Fixture::new();
    f.commit("one");
    f.commit("two");
    let (out, err, ok) = f.injected(&["squash", "-m", "Combined change"]);
    assert!(ok, "{out}\n{err}");
    assert!(err.contains("ignoring GIT_AUTHOR_NAME"));
    assert_eq!(
        git(&f.checkout, ["show", "-s", "--format=%an <%ae>", "HEAD"]).trim(),
        "Knit Smoke <knit@example.test>"
    );
}

#[test]
fn push_lists_foreign_authors_and_explicit_bypass_preserves_signing_gate() {
    let f = Fixture::new();
    let sha = f.foreign();
    let (_, err, ok) = f.injected(&["push", "--no-remote"]);
    assert!(!ok);
    assert!(err.contains(&sha), "{err}");
    git(&f.checkout, ["config", "commit.gpgsign", "true"]);
    let (_, err, ok) = f.injected(&["push", "--no-remote", "--allow-foreign-author"]);
    assert!(!ok);
    assert!(err.contains("unsigned"), "{err}");
    git(&f.checkout, ["config", "commit.gpgsign", "false"]);
    let (out, err, ok) = f.injected(&["push", "--no-remote", "--allow-foreign-author"]);
    assert!(ok, "{out}\n{err}");
    // Already-published foreign history is not newly outgoing work.
    f.commit("own");
    let (out, err, ok) = f.injected(&["push", "--no-remote"]);
    assert!(ok, "{out}\n{err}");
}

#[test]
fn split_push_destination_and_rewritten_history_are_checked() {
    let f = Fixture::new();
    let fork = f.root.join("fork.git");
    git(
        &f.root,
        [
            "clone",
            "--bare",
            f.remote.to_str().unwrap(),
            fork.to_str().unwrap(),
        ],
    );
    git(
        &f.checkout,
        [
            "remote",
            "set-url",
            "--push",
            "origin",
            fork.to_str().unwrap(),
        ],
    );
    let sha = f.foreign();
    // Upstream has this commit, but the push fork does not: it must be checked.
    git(
        &f.checkout,
        [
            "push",
            f.remote.to_str().unwrap(),
            "HEAD:refs/heads/knit/identity",
        ],
    );
    let (_, err, ok) = f.injected(&["push", "--no-remote"]);
    assert!(!ok);
    assert!(err.contains(&sha), "{err}");
    let (out, err, ok) = f.injected(&["push", "--no-remote", "--allow-foreign-author"]);
    assert!(ok, "{out}\n{err}");
    git(
        &f.checkout,
        ["commit", "--amend", "-m", "Rewritten foreign change"],
    );
    let rewritten = git(&f.checkout, ["rev-parse", "HEAD"]);
    let (_, err, ok) = f.injected(&["push", "--no-remote", "--force-with-lease"]);
    assert!(!ok);
    assert!(err.contains(rewritten.trim()), "{err}");
}

#[test]
fn ssh_signed_configured_commit_passes_preflight_with_injected_author() {
    let f = Fixture::new();
    let key = f.root.join("signing-key");
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&key)
        .status()
        .unwrap();
    assert!(status.success());
    git(&f.checkout, ["config", "gpg.format", "ssh"]);
    git(
        &f.checkout,
        ["config", "user.signingkey", key.to_str().unwrap()],
    );
    git(&f.checkout, ["config", "commit.gpgsign", "true"]);
    f.commit("signed");
    f.foreign();
    let (out, err, ok) = f.injected(&["rebase", "--offline"]);
    assert!(ok, "{out}\n{err}");
    assert_eq!(
        git(&f.checkout, ["show", "-s", "--format=%an <%ae>", "HEAD"]).trim(),
        "Knit Smoke <knit@example.test>"
    );
    assert!(git(&f.checkout, ["cat-file", "commit", "HEAD"])
        .contains("gpgsig -----BEGIN SSH SIGNATURE-----"));
    let (out, err, ok) = f.injected(&["push", "--no-remote"]);
    assert!(ok, "{out}\n{err}");
}

#[test]
fn replay_replaces_polluted_authors_with_configured_identity() {
    let f = Fixture::new();
    let sha = f.foreign();
    knit(&f.workspace, ["sync"]);
    let collaborator = f.root.join("repo-collaborator");
    fs::write(collaborator.join("upstream.txt"), "upstream change").unwrap();
    git(&collaborator, ["add", "upstream.txt"]);
    git(&collaborator, ["commit", "-m", "Upstream change"]);
    git(&collaborator, ["push", "origin", "main"]);
    let (out, err, ok) = f.injected(&["rebase"]);
    assert!(ok, "{out}\n{err}");
    assert!(err.contains("ignoring GIT_AUTHOR_NAME"), "{err}");
    assert_ne!(git(&f.checkout, ["rev-parse", "HEAD"]).trim(), sha);
    assert_eq!(
        git(&f.checkout, ["show", "-s", "--format=%an <%ae>", "HEAD"]).trim(),
        "Knit Smoke <knit@example.test>"
    );
    let replayed = git(&f.checkout, ["rev-parse", "HEAD"]);
    knit(&f.workspace, ["bundle", "destination"]);
    let destination = f.workspace.join(".knit/worktrees/destination/repo");
    let (out, err, ok) = knit_split_output(
        &destination,
        &["cherrypick", "--from", "identity", replayed.trim()],
        &[
            ("GIT_AUTHOR_NAME", "Injected Actor"),
            ("GIT_AUTHOR_EMAIL", "injected@example.test"),
        ],
    );
    assert!(ok, "{out}\n{err}");
    assert!(err.contains("ignoring GIT_AUTHOR_NAME"), "{err}");
    assert_eq!(
        git(&destination, ["show", "-s", "--format=%an <%ae>", "HEAD"]).trim(),
        "Knit Smoke <knit@example.test>"
    );
}

#[test]
fn conflict_continuation_replaces_polluted_author_despite_injected_environment() {
    let f = Fixture::new();
    fs::write(f.checkout.join("app.txt"), "feature\n").unwrap();
    git(&f.checkout, ["add", "app.txt"]);
    git(
        &f.checkout,
        [
            "-c",
            "user.name=Other Author",
            "-c",
            "user.email=other@example.test",
            "commit",
            "-m",
            "Feature edit",
        ],
    );
    knit(&f.workspace, ["sync"]);
    let collaborator = f.root.join("repo-collaborator");
    fs::write(collaborator.join("app.txt"), "upstream\n").unwrap();
    git(&collaborator, ["add", "app.txt"]);
    git(&collaborator, ["commit", "-m", "Upstream edit"]);
    git(&collaborator, ["push", "origin", "main"]);
    let (_, _, ok) = f.injected(&["rebase"]);
    assert!(!ok);
    fs::write(f.checkout.join("app.txt"), "resolved\n").unwrap();
    git(&f.checkout, ["add", "app.txt"]);
    let (out, err, ok) = f.injected(&["rebase", "--continue"]);
    assert!(ok, "{out}\n{err}");
    assert_eq!(
        git(&f.checkout, ["show", "-s", "--format=%an <%ae>", "HEAD"]).trim(),
        "Knit Smoke <knit@example.test>"
    );
}

#[test]
fn merged_upstream_history_is_excluded_from_outgoing_author_checks() {
    let f = Fixture::new();
    f.commit("feature");
    let collaborator = f.root.join("repo-collaborator");
    fs::write(collaborator.join("upstream.txt"), "upstream\n").unwrap();
    git(&collaborator, ["add", "upstream.txt"]);
    git(
        &collaborator,
        [
            "-c",
            "user.name=Upstream Author",
            "-c",
            "user.email=upstream@example.test",
            "commit",
            "-m",
            "Upstream edit",
        ],
    );
    git(&collaborator, ["push", "origin", "main"]);
    git(&f.checkout, ["fetch", "origin"]);
    git(&f.checkout, ["merge", "--no-edit", "origin/main"]);
    let (out, err, ok) = f.injected(&["push", "--no-remote"]);
    assert!(ok, "{out}\n{err}");
}

#[test]
fn same_base_rebase_corrects_pollution_without_touching_base() {
    let f = Fixture::new();
    let base = git(&f.checkout, ["rev-parse", "HEAD"]);
    f.foreign();
    let (out, err, ok) = f.injected(&["rebase", "--offline"]);
    assert!(ok, "{out}\n{err}");
    assert_eq!(git(&f.checkout, ["rev-parse", "HEAD^"]), base);
    assert_eq!(
        git(&f.checkout, ["show", "-s", "--format=%an <%ae>", "HEAD"]).trim(),
        "Knit Smoke <knit@example.test>"
    );
}

#[test]
fn cherrypick_corrects_polluted_source_and_conflict_completion() {
    for conflict in [false, true] {
        let f = Fixture::new();
        let sha = f.foreign();
        knit(&f.workspace, ["sync"]);
        knit(&f.workspace, ["bundle", "destination"]);
        let destination = f.workspace.join(".knit/worktrees/destination/repo");
        if conflict {
            fs::write(destination.join("foreign.txt"), "different content").unwrap();
            knit(
                &destination,
                ["commit", "--all", "-m", "Destination change"],
            );
        }
        let env = [
            ("GIT_AUTHOR_NAME", "Injected Actor"),
            ("GIT_AUTHOR_EMAIL", "injected@example.test"),
        ];
        let (out, err, ok) = knit_split_output(
            &destination,
            &["cherrypick", "--from", "identity", &sha],
            &env,
        );
        if conflict {
            assert!(!ok, "{out}\n{err}");
            assert!(err.contains("knit commit"), "{err}");
            fs::write(destination.join("foreign.txt"), "resolved content").unwrap();
            git(&destination, ["add", "foreign.txt"]);
            let (out, err, ok) =
                knit_split_output(&destination, &["commit", "-m", "Resolved pick"], &env);
            assert!(ok, "{out}\n{err}");
        } else {
            assert!(ok, "{out}\n{err}");
        }
        assert_eq!(
            git(&destination, ["show", "-s", "--format=%an <%ae>", "HEAD"]).trim(),
            "Knit Smoke <knit@example.test>"
        );
        assert_eq!(
            git(&f.checkout, ["show", "-s", "--format=%an <%ae>", &sha]).trim(),
            "Other Author <other@example.test>"
        );
    }
}

#[test]
fn commit_reuse_child() {
    let Some(path) = std::env::var_os("KNIT_AUTHOR_TEST_REPO") else {
        return;
    };
    let mode = std::env::var("KNIT_AUTHOR_TEST_MODE").unwrap();
    let args = if mode == "message" {
        vec!["commit", "--allow-empty", "-m", "--author"]
    } else if mode == "explicit" {
        vec![
            "commit",
            "--amend",
            "--no-edit",
            "--author=Injected Actor <injected@example.test>",
        ]
    } else if mode == "--amend" {
        vec!["commit", "--amend", "--no-edit"]
    } else {
        vec!["commit", mode.as_str(), "HEAD", "--allow-empty"]
    };
    knit::git::git_output(std::path::Path::new(&path), args).unwrap();
}

#[test]
fn reused_commit_authors_cannot_override_configured_identity() {
    for mode in ["--amend", "-C", "-c", "message", "explicit"] {
        let f = Fixture::new();
        f.foreign();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "commit_reuse_child", "--nocapture"])
            .env("KNIT_AUTHOR_TEST_REPO", &f.checkout)
            .env("KNIT_AUTHOR_TEST_MODE", mode)
            .env("GIT_AUTHOR_NAME", "Injected Actor")
            .env("GIT_AUTHOR_EMAIL", "injected@example.test")
            .env("GIT_EDITOR", "true")
            .env("GIT_CONFIG_GLOBAL", isolated_git_config_global())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            git(&f.checkout, ["show", "-s", "--format=%an <%ae>", "HEAD"]).trim(),
            "Knit Smoke <knit@example.test>"
        );
    }
}

#[test]
fn single_commit_squash_corrects_pollution_even_when_message_is_unchanged() {
    let f = Fixture::new();
    let base = git(&f.checkout, ["rev-parse", "HEAD"]);
    f.foreign();
    let (out, err, ok) = f.injected(&["squash", "-m", "Foreign change"]);
    assert!(ok, "{out}\n{err}");
    assert_eq!(git(&f.checkout, ["rev-parse", "HEAD^"]), base);
    assert_eq!(
        git(&f.checkout, ["show", "-s", "--format=%an <%ae>", "HEAD"]).trim(),
        "Knit Smoke <knit@example.test>"
    );
}

fn recorded_repo(f: &Fixture) -> knit::model::RepoEntry {
    let bundle: knit::model::ChangeGroup = serde_json::from_slice(
        &fs::read(f.workspace.join(".knit/bundles/identity.bundle.json")).unwrap(),
    )
    .unwrap();
    bundle.repos.into_iter().next().unwrap()
}

#[test]
fn publication_checks_pre_pushed_pollution_and_author_override_cannot_bypass_signing() {
    let f = Fixture::new();
    let sha = f.foreign();
    let repo = recorded_repo(&f);
    git(
        &f.checkout,
        ["push", "origin", "HEAD:refs/heads/knit/identity"],
    );
    // A push has no new outgoing work; a new review still introduces this commit.
    knit::author::preflight_push(&f.checkout, &repo, "origin", false).unwrap();
    let error = knit::author::preflight_publish(&f.checkout, &repo, "origin", None, false)
        .unwrap_err()
        .to_string();
    assert!(error.contains(&sha) && error.contains("author"), "{error}");
    knit::author::preflight_publish(&f.checkout, &repo, "origin", None, true).unwrap();
    git(&f.checkout, ["config", "commit.gpgsign", "true"]);
    knit::author::preflight_push(&f.checkout, &repo, "origin", false).unwrap();
    let error = knit::author::preflight_publish(&f.checkout, &repo, "origin", None, true)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains(&sha) && error.contains("unsigned"),
        "{error}"
    );
}

#[test]
fn publication_uses_effective_named_target_and_rejects_missing_target() {
    let f = Fixture::new();
    let sha = f.foreign();
    let repo = recorded_repo(&f);
    git(&f.checkout, ["push", "origin", "HEAD:refs/heads/release"]);
    let error = knit::author::preflight_publish(&f.checkout, &repo, "origin", None, false)
        .unwrap_err()
        .to_string();
    assert!(error.contains(&sha), "{error}");
    // The foreign commit is already in this review's selected upstream base.
    knit::author::preflight_publish(&f.checkout, &repo, "origin", Some("release"), false).unwrap();
    let error = knit::author::preflight_publish(
        &f.checkout,
        &repo,
        "origin",
        Some("missing-target"),
        false,
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("missing-target") && error.contains("unavailable"),
        "{error}"
    );
}
