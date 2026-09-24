//! CLI credential paths must preserve authoritative merged-review identity.
#![cfg(unix)]
mod common;

use common::{append_line, git, init_remote_repo, unique_temp_dir};
use knit::providers::{forgejo::Forgejo, gitlab::GitLab, Forge, PrTarget};
use serde_json::{json, Value};
use std::{ffi::OsString, fs, os::unix::fs::PermissionsExt};

struct Environment(Vec<(&'static str, Option<OsString>)>);
impl Environment {
    fn set(&mut self, key: &'static str, value: impl AsRef<std::ffi::OsStr>) {
        self.0.push((key, std::env::var_os(key)));
        std::env::set_var(key, value);
    }
    fn remove(&mut self, key: &'static str) {
        self.0.push((key, std::env::var_os(key)));
        std::env::remove_var(key);
    }
}
impl Drop for Environment {
    fn drop(&mut self) {
        for (key, value) in self.0.iter().rev() {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[test]
fn cli_only_merged_revisions_and_tea_capability_gate() {
    // One test owns this process's environment; credentials are never read from user config.
    let root = unique_temp_dir();
    let mut env = Environment(vec![]);
    env.set("KNIT_HOME", root.join("knit-home"));
    env.set("GIT_CONFIG_GLOBAL", root.join("empty.gitconfig"));
    for key in [
        "KNIT_GITLAB_TOKEN",
        "GITLAB_TOKEN",
        "GLAB_TOKEN",
        "OAUTH_TOKEN",
        "KNIT_GITLAB_API_BASE",
        "KNIT_FORGEJO_TOKEN",
        "CODEBERG_TOKEN",
        "GITEA_TOKEN",
        "KNIT_FORGEJO_API_BASE",
        "KNIT_BUNDLE",
        "KNIT_SESSION",
    ] {
        env.remove(key);
    }
    let (_, repo, _) = init_remote_repo(&root, "service");
    let base = git(&repo, ["rev-parse", "HEAD"]).trim().to_owned();
    append_line(&repo.join("app.txt"), "review content");
    git(&repo, ["add", "app.txt"]);
    git(&repo, ["commit", "-m", "Synthetic review head"]);
    let head = git(&repo, ["rev-parse", "HEAD"]).trim().to_owned();
    let tree = git(&repo, ["rev-parse", "HEAD^{tree}"]);
    let merged = git(
        &repo,
        [
            "commit-tree",
            tree.trim(),
            "-p",
            &base,
            "-p",
            &head,
            "-m",
            "Synthetic merge",
        ],
    )
    .trim()
    .to_owned();
    assert_ne!(merged, head);
    assert_ne!(merged, base);
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    for cli in ["glab", "tea"] {
        let script = bin.join(cli);
        fs::write(
            &script,
            r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$KNIT_CLI_FIXTURE/calls"
case "$(basename "$0"):$*" in
  'glab:mr view 12 --output json') cat "$KNIT_CLI_FIXTURE/response.json" ;;
  'tea:api --method GET repos/{owner}/{repo}/pulls/4')
    if [ -f "$KNIT_CLI_FIXTURE/unsupported" ]; then echo 'unknown command api' >&2; exit 1; fi
    cat "$KNIT_CLI_FIXTURE/response.json" ;;
  'tea:pr merge 4 --style merge') : > "$KNIT_CLI_FIXTURE/merged" ;;
  *) echo "unexpected CLI invocation: $*" >&2; exit 23 ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))
    .unwrap();
    env.set("PATH", path);
    env.set("KNIT_CLI_FIXTURE", &root);
    let response = root.join("response.json");
    let target = PrTarget::checkout(&repo);
    for (forge, url) in [
        (
            &GitLab as &dyn Forge,
            "https://gitlab.com/example/service/-/merge_requests/12",
        ),
        (
            &Forgejo as &dyn Forge,
            "https://codeberg.org/example/service/pulls/4",
        ),
    ] {
        let mut review = json!({"state":"merged","merged":true,"merge_commit_sha":merged,
            "sha":head,"head":{"sha":head},"base":{"sha":base},"squash_commit_sha":head});
        let mut cases = vec![(review.clone(), Some(merged.as_str()))];
        review["merged"] = json!(false);
        review["state"] = json!("opened");
        cases.push((review.clone(), None));
        review["state"] = json!("closed");
        cases.push((review.clone(), None));
        review["merged"] = json!(true);
        review["state"] = json!("merged");
        review["merge_commit_sha"] = Value::Null;
        cases.push((review.clone(), None));
        review.as_object_mut().unwrap().remove("merge_commit_sha");
        cases.push((review.clone(), None));
        review["merge_commit_sha"] = json!(" ");
        cases.push((review, None));
        for (review, expected) in cases {
            fs::write(&response, review.to_string()).unwrap();
            assert_eq!(
                forge.merged_revision(&target, url).unwrap().as_deref(),
                expected,
                "{}",
                forge.id()
            );
        }
    }
    fs::write(root.join("unsupported"), "").unwrap();
    let error = Forgejo
        .merge(&target, "4", "merge", false, None)
        .unwrap_err();
    assert!(format!("{error:#}").contains("before merging"));
    assert!(!root.join("merged").exists());
    fs::remove_file(root.join("unsupported")).unwrap();
    fs::write(&response, "{}").unwrap();
    assert!(Forgejo.merge(&target, "4", "merge", false, None).is_err());
    assert!(!root.join("merged").exists());
    fs::write(&response, r#"{"merged":false,"merge_commit_sha":null}"#).unwrap();
    Forgejo.merge(&target, "4", "merge", false, None).unwrap();
    assert!(root.join("merged").exists());
    fs::write(
        &response,
        json!({"merged":true,"merge_commit_sha":merged}).to_string(),
    )
    .unwrap();
    assert_eq!(
        Forgejo.merged_revision(&target, "4").unwrap().as_deref(),
        Some(merged.as_str())
    );
    fs::remove_dir_all(root).unwrap();
}
