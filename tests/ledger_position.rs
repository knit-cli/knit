//! How a checkout's position is compared with the bundle ledger: a checkout
//! that is behind the recorded head is not a rewind, and commits a merge
//! brings in from the base branch are not the bundle's own work.
mod common;
use common::*;
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
};

const BRANCH: &str = "knit/shared";
const ENV: [(&str, &str); 1] = [("KNIT_REMOTE_TOKEN", "test-token")];

fn head(checkout: &Path) -> String {
    git(checkout, ["rev-parse", "HEAD"]).trim().to_string()
}

fn bundle(workspace: &Path) -> Value {
    serde_json::from_slice(&fs::read(workspace.join(".knit/bundles/shared.bundle.json")).unwrap())
        .unwrap()
}

fn last_node(bundle: &Value) -> &Value {
    bundle["nodes"].as_array().unwrap().last().unwrap()
}

/// Every movement any `git.observed` node records, in ledger order.
fn observed_movements(bundle: &Value) -> Vec<String> {
    bundle["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|node| node["type"] == "git.observed")
        .flat_map(|node| node["repoChanges"].as_array().unwrap())
        .map(|change| change["movement"].as_str().unwrap().to_string())
        .collect()
}

/// One bundle used from two machines. Each machine has its own clone of the
/// same origin and its own workspace; both talk to one sync remote.
struct Machines {
    origin: PathBuf,
    server: PathBuf,
    a: PathBuf,
    a_checkout: PathBuf,
    b: PathBuf,
    b_checkout: PathBuf,
}

impl Machines {
    /// Machine A creates the bundle, commits once and pushes the branch and
    /// the ledger. Machine B pulls the bundle, so both sit on that commit.
    fn new() -> Self {
        let root = unique_temp_dir();
        let (origin, clone_a, clone_b) = init_remote_repo(&root, "backend");
        let server = root.join("sync-server");
        let url = spawn_fake_remote_push_api(&server);
        fs::write(server.join("stateful-artifacts"), "").unwrap();
        let a = root.join("machine-a");
        let b = root.join("machine-b");
        for (workspace, clone) in [(&a, &clone_a), (&b, &clone_b)] {
            fs::create_dir_all(workspace).unwrap();
            knit_with_env(workspace, ["init", "demo"], &ENV);
            knit_with_env(
                workspace,
                ["project", "add", "backend", clone.to_str().unwrap()],
                &ENV,
            );
            knit_with_env(workspace, ["remote", "add", "hosted", &url], &ENV);
        }
        knit_with_env(&a, ["bundle", "shared"], &ENV);
        let machines = Self {
            origin,
            server,
            a_checkout: a.join(".knit/worktrees/shared/backend"),
            b_checkout: b.join(".knit/worktrees/shared/backend"),
            a,
            b,
        };
        machines.a_commits_and_pushes(&["First change"]);
        knit_with_env(&machines.b, ["bundle", "pull", "shared"], &ENV);
        assert_eq!(head(&machines.b_checkout), head(&machines.a_checkout));
        machines
    }

    /// Machine A records one commit per subject, then pushes the branch to
    /// origin and the ledger to the sync remote.
    fn a_commits_and_pushes(&self, subjects: &[&str]) -> Vec<String> {
        let mut shas = Vec::new();
        for subject in subjects {
            append_line(&self.a_checkout.join("app.txt"), subject);
            knit_with_env(&self.a, ["commit", "--all", "-m", subject], &ENV);
            shas.push(head(&self.a_checkout));
        }
        knit_with_env(&self.a, ["push"], &ENV);
        self.serve_pushed_artifact();
        shas
    }

    /// The sync remote answers reads with the artifact it last accepted.
    fn serve_pushed_artifact(&self) {
        let payload: Value = serde_json::from_slice(
            &fs::read(self.server.join("rb-shared.artifact-payload")).unwrap(),
        )
        .unwrap();
        let hash = fs::read_to_string(self.server.join("rb-shared.artifact-current")).unwrap();
        let export = json!({"data": {
            "project": {"id": "proj-1", "slug": "demo"},
            "knitProject": null,
            "repositories": [],
            "bundles": [{
                "id": "rb-shared", "slug": "shared", "lifecycleState": "open",
                "currentArtifact": {"artifactHash": hash.trim(), "payload": payload},
            }],
            "historyEvents": [],
        }});
        fs::write(self.server.join("export.json"), export.to_string()).unwrap();
    }

    /// Run on machine B from inside its bundle checkout.
    fn b(&self, args: &[&str]) -> String {
        knit_with_env(&self.b_checkout, args, &ENV)
    }

    /// What the sync remote holds for the bundle.
    fn remote_ledger(&self) -> Value {
        serde_json::from_slice(&fs::read(self.server.join("rb-shared.artifact-payload")).unwrap())
            .unwrap()
    }

    fn origin_branch(&self) -> String {
        git(&self.origin, ["rev-parse", BRANCH]).trim().to_string()
    }
}

/// Machine B's checkout is still on the commit both machines started from
/// when the ledger arrives with two commits machine A made. Nothing on B was
/// rewound: B never had those commits.
#[test]
fn a_checkout_behind_the_ledger_reads_as_behind_and_records_no_rewind() {
    let m = Machines::new();
    let start = head(&m.b_checkout);
    let ahead = m.a_commits_and_pushes(&["Second change", "Third change"]);
    let recorded = bundle(&m.a);

    // Artifact only: the ledger moves, the checkout does not.
    m.b(&["sync", "pull", "--bundles"]);
    assert_eq!(head(&m.b_checkout), start);
    assert_eq!(bundle(&m.b)["nodes"], recorded["nodes"]);

    // The ledger got here before the commits did.
    let status = m.b(&["status"]);
    assert!(
        status.contains(&format!(
            "clean (recorded head {} is not in this checkout; run `knit pull`)",
            &ahead[1][..7]
        )),
        "{status}"
    );
    let sync = m.b(&["sync"]);
    assert!(sync.contains("is not in this checkout"), "{sync}");
    assert_eq!(bundle(&m.b)["nodes"], recorded["nodes"]);

    // With the commits fetched, the checkout is an ancestor of the recorded
    // head: the same shape a reset leaves behind.
    git(&m.b_checkout, ["fetch", "origin"]);
    let status = m.b(&["status"]);
    assert!(
        status.contains("clean (behind recorded head by 2; run `knit pull`)"),
        "{status}"
    );
    assert!(!status.contains("rewound"), "{status}");
    let document: Value = serde_json::from_str(&m.b(&["status", "--json"])).unwrap();
    assert_eq!(
        document["repos"][0]["behind"],
        "behind recorded head by 2; run `knit pull`"
    );
    assert!(
        document["repos"][0].get("unrecorded").is_none(),
        "{document}"
    );

    // Nothing records a rewind: not a sync, and not a push, plain or forced.
    let sync = m.b(&["sync"]);
    assert!(
        sync.contains("backend: behind recorded head by 2; run `knit pull`"),
        "{sync}"
    );
    assert!(!sync.contains("rewind"), "{sync}");
    for args in [vec!["push"], vec!["push", "--force-with-lease"]] {
        let push = knit_fails_with_env(&m.b_checkout, &args, &ENV);
        assert!(
            push.contains("backend: behind recorded head by 2; run `knit pull`"),
            "{push}"
        );
    }
    let local = bundle(&m.b);
    assert_eq!(local["nodes"], recorded["nodes"]);
    assert_eq!(local["commitGroups"], recorded["commitGroups"]);
    assert!(observed_movements(&local).is_empty(), "{local}");
    // Machine A's commits are still what origin and the sync remote hold.
    assert_eq!(m.origin_branch(), ahead[1]);
    let remote = m.remote_ledger();
    assert_eq!(remote["nodes"], recorded["nodes"]);
    assert_eq!(remote["commitGroups"], recorded["commitGroups"]);

    // The command the note names brings the checkout forward.
    m.b(&["pull"]);
    assert_eq!(head(&m.b_checkout), ahead[1]);
    let status = m.b(&["status"]);
    assert!(!status.contains("behind"), "{status}");
    assert!(!status.contains("is not in this checkout"), "{status}");
    assert_eq!(bundle(&m.b)["nodes"], recorded["nodes"]);
}

/// Committing on a checkout that is behind is allowed, as it is in git. It
/// records the new commit and nothing else: machine A's commits stay recorded.
#[test]
fn committing_on_a_checkout_that_is_behind_warns_and_records_no_rewind() {
    let m = Machines::new();
    m.a_commits_and_pushes(&["Second change", "Third change"]);
    let recorded = bundle(&m.a);
    m.b(&["sync", "pull", "--bundles"]);
    git(&m.b_checkout, ["fetch", "origin"]);

    fs::write(m.b_checkout.join("local.txt"), "local work\n").unwrap();
    git(&m.b_checkout, ["add", "local.txt"]);
    let commit = m.b(&["commit", "-m", "Local change"]);
    assert!(
        commit.contains("backend: behind recorded head by 2; run `knit pull`"),
        "{commit}"
    );
    let local = bundle(&m.b);
    assert!(observed_movements(&local).is_empty(), "{local}");
    let groups = local["commitGroups"].as_array().unwrap();
    assert_eq!(
        groups[..groups.len() - 1],
        recorded["commitGroups"].as_array().unwrap()[..]
    );
    assert_eq!(groups.last().unwrap()["message"], "Local change");
}

/// Looking at the recorded head is not holding it. A detached checkout of the
/// fetched head and back leaves the branch where it was: still behind.
#[test]
fn visiting_the_recorded_head_detached_does_not_turn_behind_into_a_rewind() {
    let m = Machines::new();
    let ahead = m.a_commits_and_pushes(&["Second change", "Third change"]);
    let recorded = bundle(&m.a);
    m.b(&["sync", "pull", "--bundles"]);
    git(&m.b_checkout, ["fetch", "origin"]);

    git(&m.b_checkout, ["checkout", "--detach", &ahead[1]]);
    git(&m.b_checkout, ["checkout", BRANCH]);

    let status = m.b(&["status"]);
    assert!(
        status.contains("clean (behind recorded head by 2; run `knit pull`)"),
        "{status}"
    );
    let sync = m.b(&["sync"]);
    assert!(!sync.contains("rewind"), "{sync}");
    let local = bundle(&m.b);
    assert_eq!(local["nodes"], recorded["nodes"]);
    assert_eq!(local["commitGroups"], recorded["commitGroups"]);
}

/// A checkout that has done nothing since `knit bundle pull` created it holds
/// the recorded head: creating the branch is enough for a reset to be a rewind.
#[test]
fn a_reset_right_after_the_bundle_was_pulled_is_a_rewind() {
    let m = Machines::new();
    let recorded = head(&m.b_checkout);

    git(&m.b_checkout, ["reset", "--hard", "HEAD~1"]);
    let status = m.b(&["status"]);
    assert!(status.contains("clean (rewound commits: 1)"), "{status}");
    let sync = m.b(&["sync"]);
    assert!(
        sync.contains("backend: observed rewind removing 1 commit(s)"),
        "{sync}"
    );
    let local = bundle(&m.b);
    let change = &last_node(&local)["repoChanges"][0];
    assert_eq!(change["movement"], "rewound");
    assert_eq!(change["droppedCommits"], json!([recorded]));
}

/// The other way to have HEAD on an ancestor of the recorded head: this
/// checkout held the recorded commits and was reset. That is still a rewind.
#[test]
fn a_reset_of_commits_the_checkout_held_still_records_a_rewind() {
    let m = Machines::new();
    let ahead = m.a_commits_and_pushes(&["Second change", "Third change"]);
    m.b(&["sync", "pull", "--bundles"]);
    git(&m.b_checkout, ["fetch", "origin"]);
    assert!(m.b(&["status"]).contains("behind recorded head by 2"));

    // Brought forward by hand, the checkout has been on the recorded head.
    git(
        &m.b_checkout,
        ["merge", "--ff-only", &format!("origin/{BRANCH}")],
    );
    assert_eq!(head(&m.b_checkout), ahead[1]);
    let status = m.b(&["status"]);
    assert!(status.contains("clean"), "{status}");
    assert!(!status.contains("behind"), "{status}");

    git(&m.b_checkout, ["reset", "--hard", "HEAD~1"]);
    let status = m.b(&["status"]);
    assert!(status.contains("clean (rewound commits: 1)"), "{status}");
    let sync = m.b(&["sync"]);
    assert!(
        sync.contains("backend: observed rewind removing 1 commit(s)"),
        "{sync}"
    );
    let local = bundle(&m.b);
    let change = &last_node(&local)["repoChanges"][0];
    assert_eq!(change["movement"], "rewound");
    assert_eq!(change["beforeSha"], ahead[1].as_str());
    assert_eq!(change["afterSha"], ahead[0].as_str());
    assert_eq!(change["droppedCommits"], json!([ahead[1]]));
    // The rewind is the user's to publish, and a forced push still does.
    m.b(&["push", "--force-with-lease"]);
    assert_eq!(m.origin_branch(), ahead[0]);
    assert_eq!(observed_movements(&m.remote_ledger()), ["rewound"]);
}

/// A bundle on one machine, with a second clone standing in for whoever
/// merges the base branch into the feature branch.
struct Feature {
    workspace: PathBuf,
    checkout: PathBuf,
    other: PathBuf,
}

impl Feature {
    fn new() -> Self {
        let root = unique_temp_dir();
        let (_origin, local, other) = init_remote_repo(&root, "backend");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        knit(&workspace, ["bundle", "shared"]);
        knit(&workspace, ["bundle", "add", local.to_str().unwrap()]);
        let feature = Self {
            checkout: workspace.join(".knit/worktrees/shared/backend"),
            workspace,
            other,
        };
        feature.write("feature.txt", "first change");
        knit(
            &feature.workspace,
            ["commit", "--all", "-m", "First change"],
        );
        knit(&feature.workspace, ["push", "--no-remote"]);
        feature
    }

    fn write(&self, file: &str, line: &str) {
        let path = self.checkout.join(file);
        if !path.exists() {
            fs::write(&path, "").unwrap();
            git(&self.checkout, ["add", file]);
        }
        append_line(&path, line);
    }

    /// A commit made with plain git, which the ledger learns of by observing.
    fn git_commit(&self, subject: &str) -> String {
        self.write("feature.txt", subject);
        git(&self.checkout, ["commit", "--all", "-m", subject]);
        head(&self.checkout)
    }

    /// The base branch moves on by `count` commits on origin.
    fn base_advances(&self, count: usize) {
        git(&self.other, ["checkout", "main"]);
        for _ in 0..count {
            let name = format!("base-{}.txt", unique_suffix());
            fs::write(self.other.join(&name), "base work\n").unwrap();
            git(&self.other, ["add", &name]);
            git(&self.other, ["commit", "-m", &format!("Base adds {name}")]);
        }
        git(&self.other, ["push", "origin", "main"]);
    }

    fn range_len(&self, before: &str, after: &str) -> usize {
        git(
            &self.checkout,
            ["rev-list", "--count", &format!("{before}..{after}")],
        )
        .trim()
        .parse()
        .unwrap()
    }
}

fn unique_suffix() -> String {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed).to_string()
}

/// Someone else merges the base branch into the published feature branch (a
/// maintainer updating a pull request, say), and the checkout fast-forwards
/// over that one merge commit. The commits the merge brought in belong to the
/// base branch, not to the bundle.
#[test]
fn merging_the_base_into_the_feature_branch_adds_only_the_merge_commit() {
    let f = Feature::new();
    let recorded = head(&f.checkout);

    f.base_advances(7);
    git(&f.other, ["fetch", "origin"]);
    git(&f.other, ["checkout", BRANCH]);
    git(&f.other, ["merge", "--no-edit", "main"]);
    git(&f.other, ["push", "origin", BRANCH]);
    let merge = head(&f.other);

    git(&f.checkout, ["fetch", "origin"]);
    git(
        &f.checkout,
        ["merge", "--ff-only", &format!("origin/{BRANCH}")],
    );
    assert_eq!(head(&f.checkout), merge);
    assert_eq!(f.range_len(&recorded, &merge), 8);

    let status = knit(&f.workspace, ["status"]);
    assert!(status.contains("clean (unrecorded commits: 1)"), "{status}");
    let sync = knit(&f.workspace, ["sync"]);
    assert!(
        sync.contains("backend: observed 1 unrecorded commit(s)"),
        "{sync}"
    );
    let ledger = bundle(&f.workspace);
    let node = last_node(&ledger);
    assert_eq!(node["type"], "git.observed", "{node}");
    let change = &node["repoChanges"][0];
    assert_eq!(change["movement"], "advanced");
    assert_eq!(change["beforeSha"], recorded.as_str());
    assert_eq!(change["afterSha"], merge.as_str());
    assert_eq!(change["commits"], json!([merge]));
    let details = change["commitDetails"].as_object().unwrap();
    assert_eq!(details.keys().collect::<Vec<_>>(), [&merge]);
    assert!(knit(&f.workspace, ["status"]).contains("clean\n"));
}

/// The bundle's own commits are kept on both sides of a merge, and a reset
/// back over the merge drops what was counted, no more.
#[test]
fn own_commits_around_a_base_merge_are_all_counted() {
    let f = Feature::new();
    let recorded = head(&f.checkout);

    let before_merge = f.git_commit("Second change");
    f.base_advances(3);
    git(&f.checkout, ["fetch", "origin"]);
    git(&f.checkout, ["merge", "--no-edit", "origin/main"]);
    let merge = head(&f.checkout);
    let after_merge = f.git_commit("Third change");
    assert_eq!(f.range_len(&recorded, &after_merge), 6);

    let status = knit(&f.workspace, ["status"]);
    assert!(status.contains("clean (unrecorded commits: 3)"), "{status}");
    knit(&f.workspace, ["sync"]);
    let ledger = bundle(&f.workspace);
    let own = json!([before_merge, merge, after_merge]);
    assert_eq!(last_node(&ledger)["repoChanges"][0]["commits"], own);

    git(&f.checkout, ["reset", "--hard", &recorded]);
    let status = knit(&f.workspace, ["status"]);
    assert!(status.contains("clean (rewound commits: 3)"), "{status}");
    let sync = knit(&f.workspace, ["sync"]);
    assert!(
        sync.contains("backend: observed rewind removing 3 commit(s)"),
        "{sync}"
    );
    let ledger = bundle(&f.workspace);
    let change = &last_node(&ledger)["repoChanges"][0];
    assert_eq!(change["movement"], "rewound");
    assert_eq!(change["droppedCommits"], own);
}
