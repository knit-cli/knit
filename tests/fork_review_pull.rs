mod common;
use common::*;
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
};

const SOURCE: &str = "https://github.com/contributor/backend.git";
const TARGET: &str = "https://github.com/acme/backend.git";
const BRANCH: &str = "knit/contribution";

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
    checkout: PathBuf,
    fork: PathBuf,
    bin: PathBuf,
    api: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = unique_temp_dir();
        let (upstream, local, _) = init_remote_repo(&root, "backend");
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
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        knit(&workspace, ["bundle", "contribution"]);
        knit(&workspace, ["bundle", "add", local.to_str().unwrap()]);
        let checkout = workspace.join(".knit/worktrees/contribution/backend");
        append_line(&checkout.join("app.txt"), "contribution one");
        knit(&workspace, ["commit", "--all", "-m", "Contribution one"]);
        git(&local, ["remote", "set-url", "origin", TARGET]);
        git(&local, ["remote", "set-url", "--push", "origin", SOURCE]);
        for (url, path) in [(SOURCE, &fork), (TARGET, &upstream)] {
            git(
                &local,
                ["config", &format!("url.{}.insteadOf", path.display()), url],
            );
        }
        let api = root.join("api");
        let bin = root.join("bin");
        fs::create_dir_all(&api).unwrap();
        fs::create_dir_all(&bin).unwrap();
        fs::write(
            api.join("settings.json"),
            serde_json::to_vec(&json!({
                "source": "contributor/backend", "target": "acme/backend",
                "fork": fork, "upstream": upstream
            }))
            .unwrap(),
        )
        .unwrap();
        write_fake_python_cli(&bin, "gh", include_str!("fixtures/contribution_gh.py"));
        let f = Self {
            root,
            workspace,
            checkout,
            fork,
            bin,
            api,
        };
        f.run(&[
            "publish",
            "create",
            "--source-remote",
            "origin",
            "--target-remote",
            "origin",
            "--no-remote",
        ]);
        f
    }

    fn run(&self, args: &[&str]) -> String {
        self.run_in(&self.workspace, args)
    }

    /// A bare `knit pull` means "this bundle" only from inside its worktrees;
    /// at the workspace root it updates every open bundle instead.
    fn run_in(&self, cwd: &Path, args: &[&str]) -> String {
        knit_with_fake_gh_env(
            cwd,
            args,
            &self.bin,
            &self.api,
            &[
                ("KNIT_GITHUB_API_TRANSPORT", "gh"),
                // The fake host never settles mergeability; do not wait for it.
                ("KNIT_MERGEABILITY_SETTLE_MS", "0"),
            ],
        )
    }

    fn head(&self) -> String {
        git(&self.checkout, ["rev-parse", "HEAD"])
            .trim()
            .to_string()
    }

    fn bundle(&self) -> Value {
        serde_json::from_slice(
            &fs::read(
                self.workspace
                    .join(".knit/bundles/contribution.bundle.json"),
            )
            .unwrap(),
        )
        .unwrap()
    }

    /// A maintainer with push access to the fork branch ("allow edits by
    /// maintainers") adds commits from their own clone.
    fn maintainer_pushes(&self, subjects: &[&str]) -> Vec<String> {
        let clone = self.root.join("maintainer");
        if !clone.exists() {
            git(
                &self.root,
                [
                    "clone",
                    self.fork.to_str().unwrap(),
                    clone.to_str().unwrap(),
                ],
            );
            git(&clone, ["config", "user.name", "Maintainer"]);
            git(&clone, ["config", "user.email", "maintainer@example.test"]);
            git(&clone, ["checkout", BRANCH]);
        }
        let mut shas = Vec::new();
        for subject in subjects {
            append_line(&clone.join("app.txt"), subject);
            git(&clone, ["commit", "--all", "-m", subject]);
            shas.push(git(&clone, ["rev-parse", "HEAD"]).trim().to_string());
        }
        git(&clone, ["push", "origin", BRANCH]);
        shas
    }
}

fn last_node(bundle: &Value) -> &Value {
    bundle["nodes"].as_array().unwrap().last().unwrap()
}

#[test]
fn pull_brings_in_commits_a_maintainer_pushed_to_the_fork_branch() {
    let f = Fixture::new();
    let recorded = f.head();
    let pushed = f.maintainer_pushes(&["Maintainer fix one", "Maintainer fix two"]);

    // Nothing local can know yet, but the host does: the live reading names
    // the moved head and the command that brings it in.
    assert!(f.run(&["status"]).contains("clean"));
    let live = f.run(&["publish", "status", "--live"]);
    assert!(!live.contains("PR unavailable"), "{live}");
    assert!(
        live.contains(&format!(
            "review head {} has commits this bundle has not recorded (recorded head {}): run `knit pull`",
            &pushed[1][..7],
            &recorded[..7]
        )),
        "{live}"
    );
    assert!(live.contains("action needed: review head"), "{live}");

    // The branch lives on the fork only; origin fetches from upstream, which
    // has no such branch, and the checkout tracks nothing.
    let pull = f.run_in(&f.checkout, &["pull", "--no-remote"]);
    assert!(pull.contains("feature advanced"), "{pull}");
    assert!(pull.contains("observed 2 unrecorded commit(s)"), "{pull}");
    assert_eq!(f.head(), pushed[1]);
    assert_eq!(
        git(&f.checkout, ["log", "-2", "--format=%an <%ae>"]),
        "Maintainer <maintainer@example.test>\nMaintainer <maintainer@example.test>\n"
    );

    let bundle = f.bundle();
    let node = last_node(&bundle);
    assert_eq!(node["type"], "git.observed", "{node}");
    let change = &node["repoChanges"][0];
    assert_eq!(change["repoId"], "backend");
    assert_eq!(change["movement"], "advanced");
    assert_eq!(change["beforeSha"], recorded.as_str());
    assert_eq!(change["afterSha"], pushed[1].as_str());
    assert_eq!(change["commits"], json!(pushed));
    for (sha, subject) in pushed
        .iter()
        .zip(["Maintainer fix one", "Maintainer fix two"])
    {
        let detail = &change["commitDetails"][sha];
        assert_eq!(detail["subject"], subject);
        assert_eq!(
            detail["author"],
            json!({ "name": "Maintainer", "email": "maintainer@example.test" })
        );
    }
    assert_eq!(bundle["repos"][0]["headSha"], pushed[1].as_str());

    let live = f.run(&["publish", "status", "--live"]);
    assert!(!live.contains("review head"), "{live}");
    // A second pull has nothing to bring in and records nothing.
    let again = f.run_in(&f.checkout, &["pull", "--no-remote"]);
    assert!(!again.contains("feature"), "{again}");
    assert_eq!(f.bundle()["nodes"], bundle["nodes"]);
}

#[test]
fn pull_leaves_a_dirty_or_diverged_feature_checkout_alone() {
    let f = Fixture::new();
    let recorded = f.head();
    f.maintainer_pushes(&["Maintainer fix"]);

    // A file of its own, so the later rebase has nothing to resolve.
    fs::write(f.checkout.join("notes.txt"), "local edit\n").unwrap();
    git(&f.checkout, ["add", "notes.txt"]);
    let dirty = f.run_in(&f.checkout, &["pull", "--no-remote"]);
    assert!(dirty.contains("feature not updated"), "{dirty}");
    assert!(dirty.contains("1 published commit waiting"), "{dirty}");
    assert_eq!(f.head(), recorded);

    // Local work on top of the old head: no fast-forward exists, and Knit
    // does not rewrite recorded commits on its own.
    f.run(&["commit", "-m", "Local follow-up"]);
    let local = f.head();
    let nodes = f.bundle()["nodes"].clone();
    let diverged = f.run_in(&f.checkout, &["pull", "--no-remote"]);
    assert!(diverged.contains("feature not updated"), "{diverged}");
    assert!(diverged.contains("have diverged"), "{diverged}");
    assert_eq!(f.head(), local);
    assert_eq!(f.bundle()["nodes"], nodes);

    // The command that message names does take them in, when asked.
    f.run(&["pull", "--feature", "--rebase", "--no-remote"]);
    assert_eq!(
        git(&f.checkout, ["log", "-2", "--format=%s"]),
        "Local follow-up\nMaintainer fix\n"
    );
}

#[test]
fn live_status_tells_an_unpushed_contribution_from_an_unreadable_review() {
    let f = Fixture::new();
    let published = f.head();
    append_line(&f.checkout.join("app.txt"), "contribution two");
    f.run(&["commit", "--all", "-m", "Contribution two"]);

    let live = f.run(&["publish", "status", "--live"]);
    assert!(!live.contains("PR unavailable"), "{live}");
    assert!(
        live.contains(&format!(
            "review still shows {}, not the recorded head {}: run `knit push`",
            &published[..7],
            &f.head()[..7]
        )),
        "{live}"
    );

    // A local rewrite leaves the review on a commit that is no ancestor of
    // the recorded head. The ledger still knows it, so this is a push too,
    // never commits to pull.
    git(&f.checkout, ["reset", "--hard", &published]);
    git(
        &f.checkout,
        ["commit", "--amend", "-m", "Contribution one, reworded"],
    );
    f.run(&["sync"]);
    let live = f.run(&["publish", "status", "--live"]);
    assert!(!live.contains("knit pull"), "{live}");
    assert!(
        live.contains(&format!(
            "review still shows {}, not the recorded head {}: run `knit push`",
            &published[..7],
            &f.head()[..7]
        )),
        "{live}"
    );
}

/// The same gap on an ordinary bundle: a teammate pushes to the feature branch
/// on `origin`, and the worktree has no upstream for a plain `git pull`.
#[test]
fn pull_brings_in_commits_a_teammate_pushed_to_origin() {
    let root = unique_temp_dir();
    let (_, local, teammate) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    knit(&workspace, ["bundle", "shared"]);
    knit(&workspace, ["bundle", "add", local.to_str().unwrap()]);
    let checkout = workspace.join(".knit/worktrees/shared/backend");
    append_line(&checkout.join("app.txt"), "first change");
    knit(&workspace, ["commit", "--all", "-m", "First change"]);

    // Not published yet: there is no branch to read, and nothing to say.
    let unpublished = knit(&checkout, ["pull", "--no-remote"]);
    assert!(!unpublished.contains("feature"), "{unpublished}");

    knit(&workspace, ["push", "--no-remote"]);
    configure_git_user(&teammate);
    git(&teammate, ["fetch", "origin"]);
    git(&teammate, ["checkout", "knit/shared"]);
    append_line(&teammate.join("app.txt"), "teammate change");
    git(&teammate, ["commit", "--all", "-m", "Teammate change"]);
    git(&teammate, ["push", "origin", "knit/shared"]);
    let pushed = git(&teammate, ["rev-parse", "HEAD"]);

    let pull = knit(&checkout, ["pull", "--no-remote"]);
    assert!(pull.contains("feature advanced"), "{pull}");
    assert_eq!(git(&checkout, ["rev-parse", "HEAD"]), pushed);
    let bundle: Value = serde_json::from_slice(
        &fs::read(workspace.join(".knit/bundles/shared.bundle.json")).unwrap(),
    )
    .unwrap();
    let node = last_node(&bundle);
    assert_eq!(node["type"], "git.observed", "{node}");
    assert_eq!(node["repoChanges"][0]["commits"], json!([pushed.trim()]));
}

/// With a sync remote configured, refreshing the bundle artifact already
/// fast-forwards the checkout from the fork. Those commits are no less new to
/// the ledger than ones the feature step brings in itself.
#[test]
fn pull_with_a_sync_remote_records_what_the_artifact_refresh_brought_in() {
    let root = unique_temp_dir();
    let home = root.join("home");
    let workspace = root.join("workspace");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&workspace).unwrap();
    let (upstream, local, _) = init_remote_repo(&root, "backend");
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
    let env = [
        ("HOME", home.to_str().unwrap()),
        ("KNIT_REMOTE_TOKEN", "test-token"),
    ];
    let run = |cwd: &Path, args: &[&str]| knit_with_env(cwd, args, &env);

    run(&workspace, &["init", "demo"]);
    run(
        &workspace,
        &["project", "add", "backend", local.to_str().unwrap()],
    );
    run(&workspace, &["bundle", "contribution"]);
    let checkout = workspace.join(".knit/worktrees/contribution/backend");
    append_line(&checkout.join("app.txt"), "contribution one");
    run(&workspace, &["commit", "--all", "-m", "Contribution one"]);
    git(&local, ["remote", "set-url", "origin", TARGET]);
    git(&local, ["remote", "set-url", "--push", "origin", SOURCE]);
    for (remote, path) in [(SOURCE, &fork), (TARGET, &upstream)] {
        git(
            &local,
            [
                "config",
                &format!("url.{}.insteadOf", path.display()),
                remote,
            ],
        );
    }
    run(&workspace, &["remote", "add", "hosted", &url]);
    run(&workspace, &["push"]);
    let bundle = || -> Value {
        serde_json::from_slice(
            &fs::read(workspace.join(".knit/bundles/contribution.bundle.json")).unwrap(),
        )
        .unwrap()
    };
    assert_eq!(bundle()["repos"][0]["sourceRemote"], SOURCE);

    let maintainer = root.join("maintainer");
    git(
        &root,
        [
            "clone",
            fork.to_str().unwrap(),
            maintainer.to_str().unwrap(),
        ],
    );
    git(&maintainer, ["config", "user.name", "Maintainer"]);
    git(
        &maintainer,
        ["config", "user.email", "maintainer@example.test"],
    );
    git(&maintainer, ["checkout", BRANCH]);
    append_line(&maintainer.join("app.txt"), "maintainer fix");
    git(&maintainer, ["commit", "--all", "-m", "Maintainer fix"]);
    git(&maintainer, ["push", "origin", BRANCH]);
    let pushed = git(&maintainer, ["rev-parse", "HEAD"]);

    let pull = run(&checkout, &["pull"]);
    assert_eq!(git(&checkout, ["rev-parse", "HEAD"]), pushed, "{pull}");
    assert!(pull.contains("observed 1 unrecorded commit(s)"), "{pull}");
    let bundle = bundle();
    let node = last_node(&bundle);
    assert_eq!(node["type"], "git.observed", "{node}");
    assert_eq!(node["repoChanges"][0]["commits"], json!([pushed.trim()]));
    assert_eq!(
        node["repoChanges"][0]["commitDetails"][pushed.trim()]["author"]["name"],
        "Maintainer"
    );
}
