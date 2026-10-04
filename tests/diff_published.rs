mod common;

use common::*;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

const REVIEW: &str = "https://github.com/upstream/widget/pull/7";
const SOURCE: &str = "https://github.com/contributor/widget.git";
const TARGET: &str = "https://github.com/upstream/widget.git";

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
    checkout: PathBuf,
    fork: PathBuf,
    writer: PathBuf,
    upstream: PathBuf,
    base: String,
    path: String,
}

impl Fixture {
    fn new() -> Self {
        let root = unique_temp_dir();
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let (upstream, local, writer) = init_remote_repo(&root, "widget");
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
        knit(&workspace, ["init", "demo"]);
        knit(
            &workspace,
            ["project", "add", "widget", local.to_str().unwrap()],
        );
        knit(&workspace, ["bundle", "published-diff"]);
        let checkout = workspace.join(".knit/worktrees/published-diff/widget");
        let base = git(&checkout, ["rev-parse", "HEAD"]).trim().to_owned();
        git(&checkout, ["remote", "set-url", "origin", TARGET]);
        git(&checkout, ["remote", "set-url", "--push", "origin", SOURCE]);
        for (url, destination) in [(SOURCE, &fork), (TARGET, &upstream)] {
            git(
                &checkout,
                [
                    "config",
                    &format!("url.{}.insteadOf", destination.display()),
                    url,
                ],
            );
        }
        let bin = root.join("bin");
        write_fake_python_cli(
            &bin,
            "gh",
            r#"import os
import pathlib
import sys

root = pathlib.Path(os.environ["DIFF_FIXTURE"])
args = sys.argv[1:]
with (root / "calls").open("a") as log:
    log.write(" ".join(args) + "\n")
if args[-2:] == ["--hostname", "github.com"]:
    args = args[:-2]
if args != ["api", "repos/upstream/widget/pulls/7"]:
    sys.exit("unexpected forge mutation or query: " + " ".join(args))
if (root / "unavailable").exists():
    sys.exit("review unavailable")
print((root / "review.json").read_text())
"#,
        );
        let path = std::env::join_paths(
            std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap()
        .to_string_lossy()
        .into_owned();
        let f = Self {
            root,
            workspace,
            checkout,
            fork,
            writer,
            upstream,
            base,
            path,
        };
        f.edit_bundle(|b| {
            b["publications"] = json!([{
                "repoId":"widget", "provider":"github", "kind":"pull_request", "number":7,
                "url":REVIEW, "state":"OPEN", "baseBranch":"main", "headBranch":"old-name",
                "updatedAt":"2026-01-01T00:00:00Z"
            }])
        });
        f.review(Some(&f.base));
        f
    }

    fn bundle_path(&self) -> PathBuf {
        self.workspace
            .join(".knit/bundles/published-diff.bundle.json")
    }
    fn edit_bundle(&self, change: impl FnOnce(&mut Value)) {
        let mut value: Value =
            serde_json::from_slice(&fs::read(self.bundle_path()).unwrap()).unwrap();
        change(&mut value);
        fs::write(
            self.bundle_path(),
            serde_json::to_vec_pretty(&value).unwrap(),
        )
        .unwrap();
    }
    fn review(&self, sha: Option<&str>) {
        fs::write(
            self.root.join("review.json"),
            json!({
                "number":7,"html_url":REVIEW,"state":"open",
                "head":{"sha":sha,"ref":"rewritten","repo":{"full_name":"contributor/widget"}},
                "base":{"ref":"main","repo":{"full_name":"upstream/widget"}}
            })
            .to_string(),
        )
        .unwrap();
    }
    fn run(&self, cwd: &Path, args: &[&str], fails: bool) -> String {
        let env = [
            ("PATH", self.path.as_str()),
            ("DIFF_FIXTURE", self.root.to_str().unwrap()),
            ("KNIT_GITHUB_API_TRANSPORT", "cli"),
        ];
        if fails {
            knit_fails_with_env(cwd, args, &env)
        } else {
            knit_with_env(cwd, args, &env)
        }
    }
    fn published_commit(&self, orphan: bool) -> String {
        if orphan {
            git(&self.writer, ["checkout", "--orphan", "rewritten"]);
        }
        fs::write(self.writer.join("app.txt"), "published-only\n").unwrap();
        git(&self.writer, ["add", "app.txt"]);
        git(&self.writer, ["commit", "-m", "Synthetic published tree"]);
        git(
            &self.writer,
            [
                "push",
                self.fork.to_str().unwrap(),
                "HEAD:refs/heads/review",
            ],
        );
        let sha = git(&self.writer, ["rev-parse", "HEAD"]).trim().to_owned();
        self.review(Some(&sha));
        sha
    }
    fn snapshot(&self) -> BTreeMap<String, Vec<u8>> {
        let mut state = BTreeMap::new();
        snapshot_files(&self.workspace.join(".knit"), "metadata", &mut state);
        snapshot_files(&self.checkout, "worktree", &mut state);
        for name in ["index", "HEAD", "FETCH_HEAD", "config"] {
            let path = git(&self.checkout, ["rev-parse", "--git-path", name]);
            let path = self.checkout.join(path.trim());
            state.insert(format!("git/{name}"), fs::read(path).unwrap_or_default());
        }
        for (name, path) in [
            ("local", &self.checkout),
            ("fork", &self.fork),
            ("upstream", &self.upstream),
        ] {
            state.insert(format!("refs/{name}"), git(path, ["show-ref"]).into_bytes());
        }
        state
    }
}

fn snapshot_files(path: &Path, prefix: &str, result: &mut BTreeMap<String, Vec<u8>>) {
    if !path.exists() {
        return;
    }
    if path.is_file() {
        result.insert(prefix.into(), fs::read(path).unwrap());
        return;
    }
    for entry in fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() == ".git" {
            continue;
        }
        snapshot_files(
            &entry.path(),
            &format!("{prefix}/{}", entry.file_name().to_string_lossy()),
            result,
        );
    }
}

#[test]
fn fork_published_tree_to_staged_and_unstaged_working_state_is_read_only() {
    let f = Fixture::new();
    let published = f.published_commit(false);
    fs::write(f.checkout.join("app.txt"), "local committed\n").unwrap();
    git(&f.checkout, ["add", "app.txt"]);
    git(&f.checkout, ["commit", "-m", "Synthetic local divergence"]);
    fs::write(f.checkout.join("staged.txt"), "staged content\n").unwrap();
    git(&f.checkout, ["add", "staged.txt"]);
    append_line(&f.checkout.join("app.txt"), "unstaged content");
    fs::write(f.checkout.join("untracked.txt"), "excluded\n").unwrap();
    // Avoid accidentally passing because the review object was already local.
    assert!(!git_success(&f.checkout, ["cat-file", "-e", &published]));
    let before = f.snapshot();
    let output = f.run(&f.checkout, &["diff", "--published"], false);
    for expected in [
        "== widget",
        REVIEW,
        &published,
        "-published-only",
        "+local committed",
        "+unstaged content",
        "+staged content",
    ] {
        assert!(output.contains(expected), "missing {expected}: {output}");
    }
    assert!(!output.contains("untracked.txt"), "{output}");
    assert!(output.contains(&format!(
        "local HEAD: {}",
        git(&f.checkout, ["rev-parse", "HEAD"]).trim()
    )));
    assert_eq!(before, f.snapshot());
    assert_eq!(
        git(&f.checkout, ["cat-file", "-t", &published]).trim(),
        "commit"
    );
    let stat = f.run(
        &f.workspace,
        &["diff", "widget", "--published", "--stat"],
        false,
    );
    assert!(
        stat.contains("staged.txt") && stat.contains("app.txt"),
        "{stat}"
    );
    assert!(!stat.contains("+unstaged content"), "{stat}");
    assert_eq!(before, f.snapshot());
}

#[test]
fn unrelated_published_history_and_stale_ledger_do_not_change_comparison() {
    let f = Fixture::new();
    let published = f.published_commit(true);
    f.edit_bundle(|b| {
        b["repos"][0]["baseSha"] = json!("f".repeat(40));
        b["repos"][0]["headSha"] = json!("e".repeat(40));
        b["repos"][0]["sourceRemote"] = json!(SOURCE);
        b["repos"][0]["targetRemote"] = json!(TARGET);
        b["nodes"] = json!([]);
        b["commitGroups"] = json!([]);
        b["headNodeId"] = Value::Null;
    });
    let before = f.snapshot();
    let output = f.run(
        &f.workspace,
        &["diff", "--published", f.checkout.to_str().unwrap()],
        false,
    );
    assert!(
        output.contains(&published)
            && output.contains("-published-only")
            && output.contains("+widget"),
        "{output}"
    );
    assert_eq!(before, f.snapshot());
}

#[test]
fn identical_trees_after_rewrite_report_both_full_heads_even_without_selectors() {
    let f = Fixture::new();
    git(
        &f.checkout,
        [
            "commit",
            "--allow-empty",
            "-m",
            "Synthetic rewritten commit",
        ],
    );
    let local = git(&f.checkout, ["rev-parse", "HEAD"]);
    assert_ne!(local.trim(), f.base);
    let output = f.run(&f.workspace, &["diff", "--published"], false);
    for expected in [REVIEW, &f.base, local.trim(), "no diff", "== widget"] {
        assert!(output.contains(expected), "{output}");
    }
}

#[test]
fn missing_invalid_unavailable_and_unfetchable_review_heads_fail_without_state_changes() {
    let f = Fixture::new();
    for sha in [
        None,
        Some(""),
        Some("main"),
        Some("ffffffffffffffffffffffffffffffffffffffff"),
    ] {
        f.review(sha);
        let before = f.snapshot();
        let output = f.run(&f.workspace, &["diff", "--published"], true);
        assert!(
            output.contains("published")
                && (output.contains("head SHA") || output.contains("cannot fetch")),
            "{output}"
        );
        assert_eq!(before, f.snapshot());
    }
    fs::write(f.root.join("unavailable"), "").unwrap();
    let before = f.snapshot();
    let output = f.run(&f.workspace, &["diff", "--published"], true);
    assert!(
        output.contains("cannot resolve published review"),
        "{output}"
    );
    assert_eq!(before, f.snapshot());
}

#[test]
fn selectors_skip_unpublished_repos_and_missing_checkouts_fail_explicitly() {
    let f = Fixture::new();
    let other = f.root.join("other");
    init_repo(&other, "other");
    knit(&f.workspace, ["bundle", "add", other.to_str().unwrap()]);
    f.edit_bundle(|b| b["repos"].as_array_mut().unwrap().reverse());
    let before = f.snapshot();
    let output = f.run(
        &f.workspace,
        &[
            "--bundle",
            "published-diff",
            "diff",
            "--published",
            "widget",
            "--stat",
        ],
        false,
    );
    assert!(
        output.contains("== widget") && !output.contains("== other"),
        "{output}"
    );
    assert_eq!(before, f.snapshot());
    let output = f.run(&f.workspace, &["diff", "--published"], false);
    assert!(
        output.contains("== widget") && output.contains("other: not published (no recorded PR/MR)"),
        "{output}"
    );
    assert!(
        !output.contains("No published reviews recorded"),
        "{output}"
    );
    assert_eq!(before, f.snapshot());
    assert!(output.find("other: not published").unwrap() < output.find("== widget").unwrap());
    let output = f.run(&f.workspace, &["diff", "--published", "other"], true);
    assert!(output.contains("no recorded PR/MR"), "{output}");
    assert_eq!(before, f.snapshot());
    let output = f.run(&f.workspace, &["diff", "--published", "unknown"], true);
    assert!(output.contains("No tracked repo matched"), "{output}");
    assert_eq!(before, f.snapshot());
    f.edit_bundle(|b| b["repos"][1]["worktreePath"] = json!("missing-checkout"));
    let before = f.snapshot();
    let output = f.run(&f.workspace, &["diff", "--published", "widget"], true);
    assert!(output.contains("bundle checkout unavailable"), "{output}");
    assert_eq!(before, f.snapshot());
}

#[test]
fn explicit_unpublished_selector_fails_without_state_changes() {
    let f = Fixture::new();
    f.edit_bundle(|b| b["publications"] = json!([]));
    let before = f.snapshot();
    let output = f.run(&f.workspace, &["diff", "--published", "widget"], true);
    assert!(output.contains("no recorded PR/MR"), "{output}");
    assert!(!output.contains("not published ("), "{output}");
    assert!(!f.root.join("calls").exists());
    assert_eq!(before, f.snapshot());
}

#[test]
fn all_unpublished_repos_report_no_reviews_without_state_changes() {
    let f = Fixture::new();
    let other = f.root.join("other");
    init_repo(&other, "other");
    knit(&f.workspace, ["bundle", "add", other.to_str().unwrap()]);
    f.edit_bundle(|b| b["publications"] = json!([]));
    let before = f.snapshot();
    let output = f.run(&f.workspace, &["diff", "--published"], false);
    for expected in [
        "widget: not published (no recorded PR/MR)",
        "other: not published (no recorded PR/MR)",
        "No published reviews recorded in bundle published-diff",
    ] {
        assert!(output.contains(expected), "{output}");
    }
    assert!(!f.root.join("calls").exists());
    assert_eq!(before, f.snapshot());
}

#[test]
fn unpublished_repos_require_available_checkouts_and_resolvable_heads() {
    let f = Fixture::new();
    f.edit_bundle(|b| b["publications"] = json!([]));
    git(&f.checkout, ["symbolic-ref", "HEAD", "refs/heads/unborn"]);
    let before = f.snapshot();
    for args in [
        vec!["diff", "--published"],
        vec!["diff", "--published", "widget"],
    ] {
        let output = f.run(&f.workspace, &args, true);
        assert!(output.contains("no resolvable local HEAD"), "{output}");
        assert!(!output.contains("no recorded PR/MR"), "{output}");
        assert_eq!(before, f.snapshot());
    }
    f.edit_bundle(|b| b["repos"][0]["worktreePath"] = json!("missing-checkout"));
    let before = f.snapshot();
    for args in [
        vec!["diff", "--published"],
        vec!["diff", "--published", "widget"],
    ] {
        let output = f.run(&f.workspace, &args, true);
        assert!(output.contains("bundle checkout unavailable"), "{output}");
        assert!(!output.contains("no recorded PR/MR"), "{output}");
        assert_eq!(before, f.snapshot());
    }
    assert!(!f.root.join("calls").exists());
}

#[test]
fn push_instead_of_and_ordinary_diff_keep_their_respective_bases() {
    let f = Fixture::new();
    let published = f.published_commit(false);
    git(
        &f.checkout,
        ["config", "--unset-all", "remote.origin.pushurl"],
    );
    git(
        &f.checkout,
        [
            "config",
            &format!("url.{}.pushInsteadOf", f.fork.display()),
            TARGET,
        ],
    );
    let before = f.snapshot();
    let output = f.run(&f.workspace, &["diff", "--published", "widget"], false);
    assert!(
        output.contains(&published) && output.contains("-published-only"),
        "{output}"
    );
    assert_eq!(before, f.snapshot());
    let ordinary = f.run(&f.workspace, &["diff", "widget"], false);
    assert!(
        ordinary.contains("no diff") && !ordinary.contains(REVIEW),
        "{ordinary}"
    );
}

#[test]
fn native_review_adapters_use_the_recorded_destination_and_live_full_sha() {
    use std::{
        io::{BufRead, BufReader, Write},
        net::TcpListener,
        time::{Duration, Instant},
    };
    for (provider, review_url, base_env, token_env, endpoint, extra) in [
        (
            "gitlab",
            "https://gitlab.com/upstream/team/widget/-/merge_requests/7",
            "KNIT_GITLAB_API_BASE",
            "KNIT_GITLAB_TOKEN",
            "/projects/upstream%2Fteam%2Fwidget/merge_requests/7",
            Some("approvals"),
        ),
        (
            "forgejo",
            "https://codeberg.org/upstream/widget/pulls/7",
            "KNIT_FORGEJO_API_BASE",
            "KNIT_FORGEJO_TOKEN",
            "/repos/upstream/widget/pulls/7",
            Some("reviews"),
        ),
        (
            "bitbucket",
            "https://bitbucket.org/upstream/widget/pull-requests/7",
            "KNIT_BITBUCKET_API_BASE",
            "KNIT_BITBUCKET_ACCESS_TOKEN",
            "/repositories/upstream/widget/pullrequests/7",
            None,
        ),
    ] {
        let f = Fixture::new();
        let published = f.published_commit(true);
        f.edit_bundle(|b| {
            b["publications"][0]["provider"] = json!(provider);
            b["publications"][0]["url"] = json!(review_url);
            if provider == "gitlab" {
                b["publications"][0]["kind"] = json!("merge_request");
            }
        });
        let body = json!({
            "id":7,"iid":7,"number":7,"web_url":review_url,"html_url":review_url,
            "links":{"html":{"href":review_url}},"state":"open","sha":published,
            "head":{"sha":published,"ref":"review"},"base":{"ref":"main"},
            "source":{"branch":{"name":"review"},"commit":{"hash":published}},
            "destination":{"branch":{"name":"main"}}
        })
        .to_string();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut requests = vec![(endpoint.to_owned(), body)];
            if let Some(extra) = extra {
                requests.push((
                    format!("{endpoint}/{extra}"),
                    if extra == "reviews" { "[]" } else { "{}" }.into(),
                ));
            }
            for (endpoint, body) in requests {
                let deadline = Instant::now() + Duration::from_secs(15);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            std::thread::sleep(Duration::from_millis(5))
                        }
                        Err(e) => panic!("expected review request: {e}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                assert_eq!(line.trim_end(), format!("GET {endpoint} HTTP/1.1"));
                loop {
                    line.clear();
                    assert!(reader.read_line(&mut line).unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                }
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        let before = f.snapshot();
        let output = knit_with_env(
            &f.checkout,
            ["diff", "--published"],
            &[(base_env, &base), (token_env, "synthetic-token")],
        );
        server.join().unwrap();
        assert!(
            output.contains(review_url)
                && output.contains(&published)
                && output.contains("-published-only"),
            "{output}"
        );
        assert_eq!(before, f.snapshot());
    }
}
