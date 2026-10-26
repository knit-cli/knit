//! `knit sign`: re-sign a bundle's commits in place with the repo's own git
//! signing configuration, keeping every named commit group 1:1 onto the new
//! SHAs, then force-pushing with a lease and syncing the hosted copy.

mod common;

use common::*;
use serde_json::Value;
use std::fs;
use std::path::PathBuf;

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
    backend: PathBuf,
    worktree: PathBuf,
    signing_key: PathBuf,
    slug: String,
}

/// A one-repo workspace pushing to a local origin, with an SSH signing key
/// the tests can wire into git config.
fn fixture(label: &str) -> Fixture {
    let root = unique_temp_dir();
    let (_origin, backend, _collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    knit(&workspace, ["init", "demo"]);
    knit(
        &workspace,
        [
            "project",
            "add",
            "backend",
            backend.to_str().unwrap(),
            "--base",
            "main",
        ],
    );
    let signing_key = root.join("signing");
    assert!(
        std::process::Command::new("ssh-keygen")
            .args(["-t", "ed25519", "-N", "", "-C", "knit-test", "-q", "-f"])
            .arg(&signing_key)
            .status()
            .expect("ssh-keygen")
            .success(),
        "ssh-keygen must produce a signing key"
    );

    knit(&workspace, ["bundle", label, "--repo", "backend"]);
    let worktree = workspace
        .join(".knit/worktrees")
        .join(slug(label))
        .join("backend");
    Fixture {
        root,
        workspace,
        backend,
        worktree,
        signing_key,
        slug: slug(label),
    }
}

fn slug(title: &str) -> String {
    title.replace(' ', "-")
}

impl Fixture {
    /// Point git's signing config at the fixture's SSH key. Shared across the
    /// source checkout and the bundle worktree, as one git config is.
    fn configure_signing(&self) {
        git(&self.backend, ["config", "gpg.format", "ssh"]);
        git(
            &self.backend,
            [
                "config",
                "user.signingkey",
                self.signing_key.with_extension("pub").to_str().unwrap(),
            ],
        );
    }

    fn commit_is_signed(&self, reference: &str) -> bool {
        git(&self.worktree, ["cat-file", "commit", reference]).contains("gpgsig ")
    }

    fn origin_tip(&self) -> String {
        git(
            &self.worktree,
            [
                "ls-remote",
                "origin",
                &format!("refs/heads/knit/{}", self.slug),
            ],
        )
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string()
    }

    fn bundle(&self) -> Value {
        let path = self
            .workspace
            .join(".knit/bundles")
            .join(format!("{}.bundle.json", self.slug));
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap()
    }

    fn unsigned_commit(&self, line: &str, message: &str) {
        append_line(&self.worktree.join("app.txt"), line);
        knit(&self.worktree, ["commit", "--all", "-m", message]);
    }
}

/// Make a git signing failure deterministic: openpgp with a key id that has
/// no secret key behind it.
fn configure_missing_key(fixture: &Fixture) {
    git(&fixture.backend, ["config", "gpg.format", "openpgp"]);
    git(
        &fixture.backend,
        ["config", "user.signingkey", "0xDEADBEEF"],
    );
}

#[test]
fn commit_signs_exactly_when_git_config_signs() {
    let fixture = fixture("git signing");
    fixture.configure_signing();
    git(&fixture.backend, ["config", "commit.gpgsign", "true"]);
    append_line(&fixture.worktree.join("app.txt"), "signed by git config");

    let output = knit(
        &fixture.worktree,
        ["commit", "--all", "-m", "Signed change"],
    );

    assert!(output.contains("committed"), "{output}");
    assert!(
        fixture.commit_is_signed("HEAD"),
        "the commit should carry a signature:\n{}",
        git(&fixture.worktree, ["cat-file", "commit", "HEAD"])
    );

    fs::remove_dir_all(fixture.root).unwrap();
}

#[test]
fn sign_resigns_in_place_keeps_groups_and_records_resign() {
    let fixture = fixture("resign work");
    fixture.unsigned_commit("first change", "First change");
    fixture.unsigned_commit("second change", "Second change");

    let before = fixture.bundle();
    let groups: Vec<(String, String)> = before["commitGroups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|group| {
            (
                group["message"].as_str().unwrap().to_string(),
                group["commits"][0]["sha"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(groups.len(), 2, "{before}");
    let subjects_before = git(&fixture.worktree, ["log", "--format=%s", "-2"]);

    fixture.configure_signing();
    let output = knit(&fixture.worktree, ["sign", "--no-push"]);

    assert!(output.contains("re-signed"), "{output}");
    assert!(
        output.contains("2 of 2 commit(s) were unsigned"),
        "{output}"
    );
    assert!(output.contains("knit push --force-with-lease"), "{output}");

    // Both commits are signed, and the work itself is untouched.
    assert!(fixture.commit_is_signed("HEAD"));
    assert!(fixture.commit_is_signed("HEAD~1"));
    assert_eq!(
        git(&fixture.worktree, ["log", "--format=%s", "-2"]),
        subjects_before
    );

    // Every named group survived 1:1, on the new SHAs.
    let after = fixture.bundle();
    let resigned: Vec<(String, String)> = after["commitGroups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|group| {
            (
                group["message"].as_str().unwrap().to_string(),
                group["commits"][0]["sha"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(resigned.len(), 2, "{after}");
    for (index, (message, sha)) in groups.iter().enumerate() {
        assert_eq!(&resigned[index].0, message, "{after}");
        assert_ne!(&resigned[index].1, sha, "{after}");
    }

    // The ledger records its own rewrite kind, and the old SHAs stay
    // traceable through the observation that superseded them.
    let rewrite = after["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|node| node.get("rewrite"))
        .next_back()
        .expect("a rewrite node");
    assert_eq!(rewrite["kind"], "resign", "{after}");
    assert_eq!(rewrite["supersededGroups"].as_array().unwrap().len(), 2);
    let node = after["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|node| node.get("rewrite").is_some())
        .unwrap();
    let old_shas: Vec<String> = node["repoChanges"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|change| {
            change["droppedCommits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|sha| sha.as_str().unwrap().to_string())
        })
        .collect();
    for (_, sha) in &groups {
        assert!(old_shas.contains(sha), "old sha {sha} lost: {after}");
    }
    // `knit log` renders the re-sign rewrite as its own kind. The rewrite
    // observation is followed by the replacement group nodes.
    let log = knit(&fixture.worktree, ["log", "-3"]);
    assert!(log.contains("resign"), "{log}");

    // Nothing was published: `--no-push` leaves origin untouched — the
    // feature branch was never pushed at all.
    assert_eq!(
        fixture.origin_tip(),
        "",
        "--no-push must leave origin untouched"
    );

    // A second pass has nothing to do.
    let again = knit(&fixture.worktree, ["sign", "--no-push"]);
    assert!(again.contains("already carries a signature"), "{again}");

    fs::remove_dir_all(fixture.root).unwrap();
}

#[test]
fn sign_pushes_with_lease_and_syncs_the_hosted_copy() {
    let fixture = fixture("resign and publish");
    let fake_dir = fixture.root.join("fake-remote");
    let base_url = spawn_fake_remote_push_api(&fake_dir);
    knit(&fixture.workspace, ["remote", "add", "hosted", &base_url]);
    let env = [("KNIT_REMOTE_TOKEN", "test-token")];

    fixture.unsigned_commit("work to re-sign", "Signed later");
    fixture.configure_signing();

    let output = knit_with_env(&fixture.worktree, ["sign"], &env);

    assert!(output.contains("re-signed"), "{output}");
    assert!(output.contains("pushed (forced)"), "{output}");
    assert!(output.contains("syncing"), "{output}");
    assert!(!output.contains("Next:"), "{output}");

    // Origin holds the re-signed head, and the hosted copy followed it.
    let head = git(&fixture.worktree, ["rev-parse", "HEAD"])
        .trim()
        .to_string();
    assert_eq!(fixture.origin_tip(), head);
    assert!(fixture.commit_is_signed("HEAD"));
    let states = fs::read_to_string(fake_dir.join("artifact-resign-and-publish.states")).unwrap();
    assert_eq!(states.lines().last(), Some("open"), "{states}");

    fs::remove_dir_all(fixture.root).unwrap();
}

#[test]
fn sign_without_a_signing_config_fails_with_gits_error() {
    let fixture = fixture("keyless work");
    fixture.unsigned_commit("keyless change", "Keyless change");
    configure_missing_key(&fixture);

    let error = knit_fails(&fixture.worktree, ["sign", "--no-push"]);
    assert!(error.contains("signing failed"), "{error}");
    assert!(error.contains("gpg failed to sign"), "{error}");

    // Nothing changed: the commits are still there, unsigned.
    assert!(!fixture.commit_is_signed("HEAD"));
    let after = fixture.bundle();
    let group = &after["commitGroups"][0];
    assert_eq!(group["message"], "Keyless change", "{after}");

    fs::remove_dir_all(fixture.root).unwrap();
}
