//! Configurable repository release workflows: repository-sequence execution,
//! mergeability preflight, integration sources, target-drift protection, and
//! the executor-0.4 run environment contract.
//!
//! Everything runs against local bare repositories with synthetic recorder
//! commands; there is no network and no real deployment.

mod common;
use common::*;
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

fn python_executable() -> &'static str {
    static PYTHON: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PYTHON.get_or_init(|| {
        for name in ["python3", "python", "py"] {
            if let Ok(output) = std::process::Command::new(name)
                .args([
                    "-c",
                    "import sys; assert sys.version_info.major == 3; print(sys.executable)",
                ])
                .output()
            {
                if output.status.success() {
                    return String::from_utf8(output.stdout).unwrap().trim().to_owned();
                }
            }
        }
        panic!("landing sequence tests require a working Python 3 interpreter");
    })
}

fn write(path: &Path, value: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

fn read(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

/// Recorder: writes the authoritative env pins it received, appends its repo
/// id to the trace, optionally moves the other repository's target (drift
/// injection), and fails while a gate file is absent.
const RECORDER: &str = r#"
import json,os,pathlib,subprocess
r=os.environ['KNIT_REPO']
pathlib.Path(os.environ['RECORD']).write_text(json.dumps({k:os.environ.get(k) for k in ['KNIT_REV','KNIT_SOURCE_SHA','KNIT_TARGET_BRANCH','KNIT_LAND_ENVIRONMENT']}))
pathlib.Path(os.environ['TRACE']).open('a').write(r+chr(10))
if os.environ.get('INJECT')=='delete' and r=='alpha':
    subprocess.check_call(['git','--git-dir',os.environ['BETA_REMOTE'],'update-ref','-d','refs/heads/staging'])
elif os.environ.get('INJECT')=='1' and r=='alpha':
    subprocess.check_call(['git','--git-dir',os.environ['BETA_REMOTE'],'update-ref','refs/heads/staging',os.environ['BETA_SOURCE']])
if os.environ.get('VERIFY_GATE') and not pathlib.Path(os.environ['VERIFY_GATE']).exists():
    raise SystemExit(9)
"#;

struct Fixture {
    root: PathBuf,
    repos: Vec<(String, PathBuf, PathBuf, String, String)>, // id, clone, bare, base, head
    project: PathBuf,
    bundle: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = unique_temp_dir();
        let mut repos = vec![];
        for name in ["alpha", "beta"] {
            let remote = root.join(format!("{name}.git"));
            let repo = root.join(name);
            let remote_arg = remote.to_string_lossy().into_owned();
            run_git(&root, &["init", "--bare", remote_arg.as_str()]);
            let repo_arg = repo.to_string_lossy().into_owned();
            run_git(&root, &["clone", remote_arg.as_str(), repo_arg.as_str()]);
            run_git(&repo, &["config", "user.name", "Synthetic"]);
            run_git(
                &repo,
                &["config", "user.email", "synthetic@example.invalid"],
            );
            run_git(&repo, &["checkout", "-b", "main"]);
            fs::write(repo.join("app.txt"), "base\n").unwrap();
            run_git(&repo, &["add", "."]);
            run_git(&repo, &["commit", "-m", "Base"]);
            run_git(&repo, &["push", "origin", "main"]);
            let base = head(&repo, "HEAD");
            run_git(&repo, &["checkout", "-b", "staging"]);
            run_git(&repo, &["push", "origin", "staging"]);
            run_git(&repo, &["checkout", "-b", "feature", "main"]);
            fs::write(repo.join("app.txt"), "feature\n").unwrap();
            run_git(&repo, &["commit", "-am", "Feature work"]);
            run_git(&repo, &["push", "origin", "feature"]);
            let head_sha = head(&repo, "HEAD");
            repos.push((name.to_owned(), repo, remote, base, head_sha));
        }
        let now = "2026-01-01T00:00:00Z";
        let project_id = root.file_name().unwrap().to_str().unwrap().to_owned();
        write(
            &root.join(".knit/config.json"),
            &json!({"schemaVersion":"0.1","kind":"KnitConfig","activeBundle":"demo","activeProject":project_id}),
        );
        let mut bundle = serde_json::to_value(knit::model::ChangeGroup::new(
            "demo".into(),
            "Synthetic release".into(),
            now.to_owned(),
        ))
        .unwrap();
        bundle["projectId"] = json!(project_id);
        bundle["repos"] = json!(repos.iter().map(|(id, repo, _, base, head)| {
            json!({"id":id,"path":repo,"remote":null,"baseBranch":"main","baseSha":base,"featureBranch":"feature","worktreePath":null,"headSha":head})
        }).collect::<Vec<_>>());
        bundle["commitGroups"] = json!([{"id":"change","message":"Synthetic","createdAt":now,
            "commits":repos.iter().map(|(id,_,_,_,head)| json!({"repoId":id,"sha":head})).collect::<Vec<_>>()}]);
        let bundle_path = root.join(".knit/bundles/demo.bundle.json");
        write(&bundle_path, &bundle);
        let mut project = serde_json::to_value(knit::model::KnitProject::new(
            project_id.clone(),
            now.to_owned(),
        ))
        .unwrap();
        project["repos"] = json!(repos
            .iter()
            .map(|(id, repo, _, _, _)| { json!({"id":id,"path":repo,"baseBranch":"main"}) })
            .collect::<Vec<_>>());
        let project_path = root.join(format!(".knit/projects/{project_id}.project.json"));
        write(&project_path, &project);
        write(
            &root.join("roots.json"),
            &json!(repos
                .iter()
                .map(|(id, repo, _, _, _)| (id.clone(), repo.to_string_lossy().into_owned()))
                .collect::<std::collections::BTreeMap<_, _>>()),
        );
        let f = Self {
            root,
            repos,
            project: project_path,
            bundle: bundle_path,
        };
        f.set_landing(json!({"lanes":{"preview":{"terminal":false,"branches":{"alpha":"staging","beta":"staging"}}}}));
        f
    }

    /// Replace the project's landing configuration.
    fn set_landing(&self, landing: Value) {
        let mut project = read(&self.project);
        project["landing"] = landing;
        write(&self.project, &project);
    }

    fn lane(&self, mut lane: Value) {
        lane["terminal"] = json!(false);
        if lane["branches"].is_null() {
            lane["branches"] = json!({"alpha":"staging","beta":"staging"});
        }
        if lane["deployments"].is_null() {
            lane["deployments"] = json!(self.recorder_deployments());
        }
        let mut project = read(&self.project);
        project["landing"]["lanes"]["preview"] = lane;
        write(&self.project, &project);
    }

    fn recorder_deployments(&self) -> Value {
        let pinned_heads: Vec<String> = self
            .repos
            .iter()
            .map(|(_, repo, ..)| head(repo, "HEAD"))
            .collect();
        self.repos
            .iter()
            .map(|(id, _, _, _, _)| {
                json!({
                    "id": format!("deploy-{id}"),
                    "repoId": id,
                    "whenChanged": [id],
                    "mode": "command",
                    "command": [python_executable(), "-c", RECORDER],
                    "env": {
                        "TRACE": self.trace_path().to_string_lossy(),
                        "RECORD": self.root.join(format!("record-{id}.json")).to_string_lossy(),
                        "KNIT_REV": "spoofed-by-recipe",
                        "KNIT_SOURCE_SHA": "recipe-value",
                        "KNIT_TARGET_BRANCH": "recipe-value",
                        "KNIT_LAND_ENVIRONMENT": "recipe-value",
                        "BETA_REMOTE": self.repos[1].2.to_string_lossy(),
                        "BETA_SOURCE": pinned_heads[1],
                    },
                    "effect": "deployment",
                    "recovery": {"mode": "manual", "reason": "Synthetic recorder"}
                })
            })
            .collect::<Vec<_>>()
            .into()
    }

    fn trace_path(&self) -> PathBuf {
        self.root.join("trace")
    }

    fn cmd(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_knit"))
            .current_dir(&self.root)
            .args(args)
            .env_remove("KNIT_BUNDLE")
            .env_remove("KNIT_SESSION")
            .env("KNIT_HOME", self.root.join("home"))
            .output()
            .unwrap()
    }

    fn plan(&self) -> Value {
        let out = self.cmd(&[
            "land",
            "--lane",
            "preview",
            "plan",
            "--from-artifact",
            self.bundle.to_str().unwrap(),
            "--project-file",
            self.project.to_str().unwrap(),
            "--out",
            "plan.json",
            "--json",
        ]);
        assert!(
            out.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        read(&self.root.join("plan.json"))
    }

    fn apply(&self, plan: &str, resume: bool) -> std::process::Output {
        let mut args = vec![
            "land",
            "apply",
            "--plan",
            plan,
            "--from-artifact",
            if resume {
                "out.json"
            } else {
                self.bundle.to_str().unwrap()
            },
            "--project-file",
            self.project.to_str().unwrap(),
            "--repo-roots",
            "roots.json",
            "--run-out",
            "run.json",
            "--out",
            "out.json",
            "--json",
        ];
        if resume {
            args.push("--resume");
        }
        self.cmd(&args)
    }

    fn remote_tip(&self, repo: &str, branch: &str) -> String {
        let bare = &self.repos.iter().find(|(id, ..)| id == repo).unwrap().2;
        head(bare, &format!("refs/heads/{branch}"))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// A clone with only its default local branch; feature work belongs to a
/// separate checkout and the environment branch initially exists only remotely.
fn branch_fixture() -> Fixture {
    let f = Fixture::new();
    let mut bundle = read(&f.bundle);
    for (id, repo, _, _, _) in &f.repos {
        run_git(repo, &["checkout", "main"]);
        run_git(repo, &["branch", "-D", "staging"]);
        let feature = f.root.join(format!("feature-{id}"));
        run_git(
            repo,
            &["worktree", "add", feature.to_str().unwrap(), "feature"],
        );
        bundle["repos"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|r| r["id"] == *id)
            .unwrap()["worktreePath"] = json!(feature);
    }
    write(&f.bundle, &bundle);
    let deployments: Vec<Value> = f
        .repos
        .iter()
        .map(|(id, repo, _, _, _)| {
            json!({
                "id":format!("deploy-{id}"), "repoId":id, "whenChanged":[id], "mode":"command",
                "checkout":{"mode":"branch","branch":"staging","remote":"origin","update":"pull"},
                "command":[python_executable(),"-c",r#"
import json,os,pathlib,subprocess
def git(*args): return subprocess.check_output(['git',*args],text=True).strip()
assert pathlib.Path.cwd().resolve() == pathlib.Path(os.environ['EXPECTED_ROOT']).resolve()
assert git('symbolic-ref','--short','HEAD') == 'staging'
assert git('show','staging:app.txt') == 'feature'
revision=git('rev-parse','HEAD')
assert git('ls-remote','origin','refs/heads/staging').split()[0] == revision
assert os.environ['KNIT_REV'] == revision
pathlib.Path(os.environ['TRACE']).open('a').write(os.environ['KNIT_REPO']+'\n')
"#],
                "env":{"EXPECTED_ROOT":repo,"TRACE":f.trace_path()},
                "effect":"read_only","recovery":{"mode":"none"}
            })
        })
        .collect();
    f.lane(json!({"deployments":deployments,"execution":{"mode":"repository_sequence","repoOrder":["alpha","beta"]},"preflight":{"mergeability":"all"}}));
    f
}

fn assert_success(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn advance_features(f: &Fixture) -> Vec<String> {
    f.repos
        .iter()
        .map(|(id, ..)| {
            let feature = f.root.join(format!("feature-{id}"));
            run_git(
                &feature,
                &[
                    "commit",
                    "--allow-empty",
                    "-m",
                    "Exercise another branch landing",
                ],
            );
            run_git(&feature, &["push", "origin", "feature"]);
            head(&feature, "HEAD")
        })
        .collect()
}

#[test]
fn branch_checkout_command_accepts_cache_and_records_its_revision() {
    let f = branch_fixture();
    let mut project = read(&f.project);
    for deployment in project["landing"]["lanes"]["preview"]["deployments"]
        .as_array_mut()
        .unwrap()
    {
        deployment["cache"] = json!("unchanged");
    }
    write(&f.project, &project);
    let plan = f.plan();
    assert!(plan["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["type"] == "deploy")
        .all(|s| s["cache"] == "unchanged"));
    fs::create_dir_all(f.root.join(".knit/land-runs")).unwrap();
    let apply = |run: &str, out: &str| {
        f.cmd(&[
            "land",
            "apply",
            "--plan",
            "plan.json",
            "--from-artifact",
            f.bundle.to_str().unwrap(),
            "--project-file",
            f.project.to_str().unwrap(),
            "--repo-roots",
            "roots.json",
            "--run-out",
            run,
            "--out",
            out,
            "--json",
        ])
    };
    assert_success(&apply(".knit/land-runs/first.run.json", "first-out.json"));
    let first = read(&f.root.join(".knit/land-runs/first.run.json"));
    for step in first["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["type"] == "deploy")
    {
        assert!(step["fingerprint"].is_string());
        assert!(step["sourceRevisions"][step["repoId"].as_str().unwrap()].is_string());
    }
    let trace = fs::read_to_string(f.trace_path()).unwrap();
    assert_success(&apply(".knit/land-runs/second.run.json", "second-out.json"));
    let second = read(&f.root.join(".knit/land-runs/second.run.json"));
    assert_eq!(fs::read_to_string(f.trace_path()).unwrap(), trace);
    for step in second["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["type"] == "deploy")
    {
        assert!(step["output"]["reused"].is_object());
        assert_eq!(step["attribution"], "already_satisfied");
    }
}

#[test]
fn branch_checkout_reuses_exact_plan_after_both_feature_branches_advance() {
    for sync_bundle in [false, true] {
        let f = branch_fixture();
        let plan = f.plan();
        let authored = fs::read(f.root.join("plan.json")).unwrap();
        assert_success(&f.apply("plan.json", false));
        let new_heads = advance_features(&f);
        if sync_bundle {
            assert_success(&f.cmd(&["--bundle", "demo", "sync"]));
        }
        assert_success(&f.cmd(&[
            "land",
            "validate",
            "--plan",
            "plan.json",
            "--from-artifact",
            f.bundle.to_str().unwrap(),
            "--project-file",
            f.project.to_str().unwrap(),
            "--json",
        ]));
        let preflight = f.cmd(&[
            "land",
            "preflight",
            "--plan",
            "plan.json",
            "--from-artifact",
            f.bundle.to_str().unwrap(),
            "--project-file",
            f.project.to_str().unwrap(),
            "--repo-roots",
            "roots.json",
            "--json",
        ]);
        assert_success(&preflight);
        let report: Value = serde_json::from_slice(&preflight.stdout).unwrap();
        for (i, (id, ..)) in f.repos.iter().enumerate() {
            let check = report["checks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["repoId"] == *id)
                .unwrap();
            assert_eq!(check["sourceSha"], new_heads[i]);
        }
        assert_success(&f.cmd(&[
            "land",
            "apply",
            "--plan",
            "plan.json",
            "--from-artifact",
            f.bundle.to_str().unwrap(),
            "--project-file",
            f.project.to_str().unwrap(),
            "--repo-roots",
            "roots.json",
            "--run-out",
            "second.json",
            "--out",
            "second-out.json",
            "--json",
        ]));
        let first = read(&f.root.join("run.json"));
        let second = read(&f.root.join("second.json"));
        assert_eq!(first["planHash"], second["planHash"]);
        assert_eq!(second["plan"], plan);
        assert_eq!(fs::read(f.root.join("plan.json")).unwrap(), authored);
        for (i, (id, repo, ..)) in f.repos.iter().enumerate() {
            let merge = second["steps"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["id"] == format!("merge-{id}"))
                .unwrap();
            assert_eq!(merge["output"]["source"], new_heads[i]);
            assert_eq!(head(repo, "HEAD^2"), new_heads[i]);
            assert_eq!(f.remote_tip(id, "staging"), head(repo, "HEAD"));
            assert_eq!(
                run_git(repo, &["show", "-s", "--format=%s", "HEAD"]),
                "Merge branch 'feature' into staging"
            );
            assert_eq!(
                head(&f.root.join(format!("feature-{id}")), "HEAD"),
                new_heads[i]
            );
            let source = second["sourceBundle"]["repos"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["id"] == *id)
                .unwrap();
            assert_eq!(source["headSha"], new_heads[i]);
        }
        assert_eq!(
            fs::read_to_string(f.trace_path())
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            ["alpha", "beta", "alpha", "beta"]
        );
    }
}

#[test]
fn branch_checkout_local_reapply_accepts_unrecorded_feature_heads() {
    let f = branch_fixture();
    f.plan();
    let args = [
        "--bundle",
        "demo",
        "land",
        "--lane",
        "preview",
        "apply",
        "--plan",
        "plan.json",
        "--no-tag",
        "--no-remote",
    ];
    assert_success(&f.cmd(&args));
    let new_heads = advance_features(&f);
    assert_success(&f.cmd(&args));
    for (i, (_, repo, ..)) in f.repos.iter().enumerate() {
        assert_eq!(head(repo, "HEAD^2"), new_heads[i]);
    }
    assert_eq!(
        fs::read_to_string(f.trace_path())
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        ["alpha", "beta", "alpha", "beta"]
    );
}

#[test]
fn branch_checkout_reuse_keeps_branch_identity_and_other_plan_pins() {
    let f = branch_fixture();
    f.plan();
    let original = read(&f.bundle);
    for key in ["featureBranch", "remote", "baseBranch", "baseSha"] {
        let mut changed = original.clone();
        changed["repos"][0][key] = json!("different");
        write(&f.root.join("changed.json"), &changed);
        let result = f.cmd(&[
            "land",
            "validate",
            "--plan",
            "plan.json",
            "--from-artifact",
            "changed.json",
            "--json",
        ]);
        assert!(!result.status.success(), "accepted changed {key}");
    }
    run_git(&f.repos[1].2, &["update-ref", "-d", "refs/heads/feature"]);
    assert!(!f.apply("plan.json", false).status.success());
    for (id, _, _, base, _) in &f.repos {
        assert_eq!(&f.remote_tip(id, "staging"), base);
    }
    assert!(!f.trace_path().exists());
    let pinned = Fixture::new();
    pinned.plan();
    let mut changed = read(&pinned.bundle);
    changed["repos"][0]["headSha"] = json!("a".repeat(40));
    write(&pinned.root.join("changed.json"), &changed);
    let result = pinned.cmd(&[
        "land",
        "validate",
        "--plan",
        "plan.json",
        "--from-artifact",
        "changed.json",
        "--json",
    ]);
    assert!(
        !result.status.success(),
        "ordinary pinned plan accepted changed source"
    );
}

#[test]
fn branch_checkout_merges_pushes_and_runs_in_source_without_touching_feature() {
    let f = branch_fixture();
    let plan = f.plan();
    assert_eq!(plan["requiredExecutorVersion"], "0.5");
    for step in plan["steps"].as_array().unwrap() {
        assert_eq!(step["checkout"]["mode"], "branch");
    }
    assert_success(&f.apply("plan.json", false));
    let run = read(&f.root.join("run.json"));
    for (id, repo, _, _, source) in &f.repos {
        assert_eq!(
            run_git(repo, &["symbolic-ref", "--short", "HEAD"]),
            "staging"
        );
        assert_eq!(
            run_git(repo, &["config", "branch.staging.remote"]),
            "origin"
        );
        assert_eq!(
            run_git(repo, &["config", "branch.staging.merge"]),
            "refs/heads/staging"
        );
        let revision = head(repo, "HEAD");
        assert_eq!(f.remote_tip(id, "staging"), revision);
        assert_ne!(&revision, source);
        assert_eq!(
            run_git(repo, &["show", "-s", "--format=%s", "HEAD"]),
            "Merge branch 'feature' into staging"
        );
        assert_eq!(head(&f.root.join(format!("feature-{id}")), "HEAD"), *source);
        assert_eq!(
            run_git(
                &f.root.join(format!("feature-{id}")),
                &["symbolic-ref", "--short", "HEAD"]
            ),
            "feature"
        );
        let command = run["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == format!("deploy-{id}"))
            .unwrap();
        assert_eq!(command["sourceRevisions"][id], revision);
    }
    assert_eq!(
        fs::read_to_string(f.trace_path())
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        ["alpha", "beta"]
    );
    assert!(!f.root.join("run.checkouts").exists());
    // A fresh application of the same sources does not create another merge.
    let tips: Vec<_> = f
        .repos
        .iter()
        .map(|(id, ..)| f.remote_tip(id, "staging"))
        .collect();
    assert_success(&f.cmd(&[
        "land",
        "apply",
        "--plan",
        "plan.json",
        "--from-artifact",
        f.bundle.to_str().unwrap(),
        "--project-file",
        f.project.to_str().unwrap(),
        "--repo-roots",
        "roots.json",
        "--run-out",
        "again.json",
        "--out",
        "again-out.json",
        "--json",
    ]));
    for (index, (id, ..)) in f.repos.iter().enumerate() {
        assert_eq!(f.remote_tip(id, "staging"), tips[index]);
    }
}

#[test]
fn branch_checkout_local_apply_uses_registered_source_not_bundle_worktree() {
    let f = branch_fixture();
    f.plan();
    assert_success(&f.cmd(&[
        "--bundle",
        "demo",
        "land",
        "--lane",
        "preview",
        "apply",
        "--plan",
        "plan.json",
        "--no-tag",
        "--no-remote",
    ]));
    assert_eq!(
        fs::read_to_string(f.trace_path())
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        ["alpha", "beta"]
    );
}

#[test]
fn branch_checkout_refuses_unsafe_roots_before_any_push() {
    for problem in ["dirty", "diverged", "held", "feature"] {
        let f = branch_fixture();
        let repo = &f.repos[1].1;
        match problem {
            "dirty" => {
                fs::write(repo.join("untracked.txt"), "keep me").unwrap();
            }
            "diverged" => {
                run_git(repo, &["checkout", "-b", "staging", "origin/staging"]);
                fs::write(repo.join("local.txt"), "keep me").unwrap();
                run_git(repo, &["add", "."]);
                run_git(repo, &["commit", "-m", "Unpublished local work"]);
            }
            "held" => {
                run_git(
                    repo,
                    &[
                        "worktree",
                        "add",
                        "-b",
                        "staging",
                        f.root.join("held").to_str().unwrap(),
                        "origin/staging",
                    ],
                );
            }
            "feature" => {
                let mut roots = read(&f.root.join("roots.json"));
                roots["beta"] = json!(f.root.join("feature-beta"));
                write(&f.root.join("roots.json"), &roots);
            }
            _ => unreachable!(),
        }
        let before = head(repo, "HEAD");
        f.plan();
        let output = f.apply("plan.json", false);
        assert!(!output.status.success(), "accepted unsafe {problem}");
        assert_eq!(head(repo, "HEAD"), before, "changed unsafe {problem}");
        for (id, _, _, base, _) in &f.repos {
            assert_eq!(
                &f.remote_tip(id, "staging"),
                base,
                "pushed before refusing {problem}"
            );
        }
        assert!(!f.trace_path().exists());
    }
}

#[test]
fn branch_checkout_requires_explicit_executor_and_matching_merge_target() {
    let f = branch_fixture();
    let plan = f.plan();
    for problem in ["version", "capability", "target", "parallel"] {
        let mut invalid = plan.clone();
        match problem {
            "version" => invalid["requiredExecutorVersion"] = json!("0.4"),
            "capability" => invalid["requiredCapabilities"] = json!(["commands"]),
            "target" => invalid["steps"][0]["checkout"]["branch"] = json!("other"),
            "parallel" => {
                invalid.as_object_mut().unwrap().remove("workflow");
                invalid.as_object_mut().unwrap().remove("execution");
                let deploy = invalid["steps"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|s| s["id"] == "deploy-alpha")
                    .unwrap();
                deploy["needs"] = json!([]);
                deploy["requires"] = json!([]);
            }
            _ => unreachable!(),
        }
        write(&f.root.join("invalid.json"), &invalid);
        let out = f.cmd(&["land", "validate", "--plan", "invalid.json", "--json"]);
        assert!(!out.status.success(), "accepted {problem}");
    }
    let schema: Value =
        serde_json::from_str(include_str!("../schemas/land-plan.schema.json")).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    assert!(
        validator.is_valid(&plan),
        "schema rejected branch plan: {:?}",
        validator
            .iter_errors(&plan)
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
    );
    let mut downgraded = plan;
    downgraded["requiredExecutorVersion"] = json!("0.4");
    assert!(!validator.is_valid(&downgraded));
}

#[test]
fn branch_checkout_resume_uses_current_remote_head_without_remerging() {
    let f = branch_fixture();
    let gate = f.root.join("ready");
    let mut project = read(&f.project);
    let command = &mut project["landing"]["lanes"]["preview"]["deployments"][0];
    command["env"]["GATE"] = json!(gate);
    let script = command["command"][2].as_str().unwrap().to_owned();
    command["command"][2] = json!(format!("import os,pathlib\nif not pathlib.Path(os.environ['GATE']).exists(): raise SystemExit(7)\n{script}"));
    write(&f.project, &project);
    f.plan();
    assert!(!f.apply("plan.json", false).status.success());
    let first = read(&f.root.join("run.json"));
    let merge = first["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "merge-alpha")
        .unwrap()
        .clone();
    assert_eq!(merge["status"], "succeeded");
    let repo = &f.repos[0].1;
    let merged = f.remote_tip("alpha", "staging");
    let newer = run_git(
        repo,
        &[
            "commit-tree",
            "HEAD^{tree}",
            "-p",
            &merged,
            "-m",
            "Later remote work",
        ],
    );
    run_git(
        repo,
        &["push", "origin", &format!("{newer}:refs/heads/staging")],
    );
    let advanced = advance_features(&f);
    fs::write(gate, "ready").unwrap();
    assert_success(&f.apply("plan.json", true));
    let final_run = read(&f.root.join("run.json"));
    let steps = final_run["steps"].as_array().unwrap();
    assert_eq!(
        *steps.iter().find(|s| s["id"] == "merge-alpha").unwrap(),
        merge
    );
    assert_eq!(
        steps.iter().find(|s| s["id"] == "deploy-alpha").unwrap()["sourceRevisions"]["alpha"],
        newer
    );
    assert_eq!(f.remote_tip("alpha", "staging"), newer);
    assert_eq!(head(repo, "HEAD"), newer);
    let beta = steps.iter().find(|s| s["id"] == "merge-beta").unwrap();
    assert_eq!(beta["output"]["source"], f.repos[1].4);
    assert_ne!(beta["output"]["source"], advanced[1]);
}

fn run_git(dir: &Path, args: &[&str]) -> String {
    let out = git_raw(dir, args);
    assert!(
        out.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn git_raw(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Synthetic")
        .env("GIT_AUTHOR_EMAIL", "synthetic@example.invalid")
        .env("GIT_COMMITTER_NAME", "Synthetic")
        .env("GIT_COMMITTER_EMAIL", "synthetic@example.invalid")
        .output()
        .unwrap()
}

/// A git invocation whose failure is a boolean, for ancestry probes.
fn git_succeeds(dir: &Path, args: &[&str]) -> bool {
    git_raw(dir, args).status.success()
}

fn head(dir: &Path, rev: &str) -> String {
    let out = git_raw(dir, &["rev-parse", rev]);
    assert!(out.status.success(), "rev-parse {rev}");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn sequence_lane(f: &Fixture) {
    f.lane(json!({
        "execution": {"mode": "repository_sequence", "repoOrder": ["alpha", "beta"]},
        "preflight": {"mergeability": "all"},
        "maxParallel": 1,
        "onFailure": "stop"
    }));
}

#[test]
fn default_plan_generation_is_unchanged() {
    let f = Fixture::new();
    f.lane(json!({}));
    let plan = f.plan();
    assert_eq!(plan["schemaVersion"], "0.2");
    assert_eq!(plan["requiredExecutorVersion"], "0.3");
    for key in ["workflow", "execution", "preflight", "integrationSources"] {
        assert!(plan.get(key).is_none(), "default plan must not carry {key}");
    }
    let applied = f.apply("plan.json", false);
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    // No sequence policy: both repositories deploy, in unspecified order.
    let trace = fs::read_to_string(f.trace_path()).unwrap();
    let mut traced: Vec<&str> = trace.lines().collect();
    traced.sort_unstable();
    assert_eq!(traced, vec!["alpha", "beta"]);
    // Executor 0.3 plans keep the historical environment: recipe-provided
    // KNIT_* values pass through untouched.
    let record: Value = read(&f.root.join("record-alpha.json"));
    assert_eq!(record["KNIT_REV"], "spoofed-by-recipe");
    assert_eq!(record["KNIT_SOURCE_SHA"], "recipe-value");
    assert_eq!(record["KNIT_TARGET_BRANCH"], "recipe-value");
    assert_eq!(record["KNIT_LAND_ENVIRONMENT"], "recipe-value");
}

#[test]
fn repository_sequence_compiles_explicit_order_and_pins_the_run_environment() {
    let f = Fixture::new();
    sequence_lane(&f);
    let plan = f.plan();
    assert_eq!(plan["requiredExecutorVersion"], "0.4");
    assert_eq!(
        plan["requiredCapabilities"],
        json!(["mergeability-preflight", "repository-sequence"])
    );
    assert_eq!(
        plan["execution"],
        json!({"mode": "repository_sequence", "repoOrder": ["alpha", "beta"]})
    );
    assert_eq!(plan["preflight"], json!({"mergeability": "all"}));
    let workflow = serde_json::to_string(&plan["workflow"]).unwrap();
    let alpha_at = workflow.find("merge-alpha").unwrap();
    let beta_at = workflow.find("merge-beta").unwrap();
    assert!(alpha_at < beta_at, "alpha's group must precede beta's");

    let applied = f.apply("plan.json", false);
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    // Progress must not corrupt the JSON result, and every remote mutation
    // must remain visible and have its own recorded command output.
    serde_json::from_slice::<Value>(&applied.stdout).unwrap();
    let progress = String::from_utf8_lossy(&applied.stderr);
    for repo in ["alpha", "beta"] {
        for operation in ["fetch", "merge", "push"] {
            assert!(
                progress.contains(&format!("[merge-{repo}/forward] $ git {operation}")),
                "{progress}"
            );
        }
    }
    assert_eq!(
        fs::read_to_string(f.trace_path())
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        ["alpha", "beta"]
    );
    let run = read(&f.root.join("run.json"));
    for repo in ["alpha", "beta"] {
        let merge = run["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == json!(format!("merge-{repo}")))
            .unwrap();
        assert_eq!(merge["status"], "succeeded");
        let commands = merge["attempts"].as_array().unwrap();
        let pushed = commands
            .iter()
            .find(|c| c["command"] == "git push")
            .unwrap();
        assert_eq!(pushed["status"], "succeeded");
        assert!(!pushed["stderr"].as_str().unwrap().is_empty());
        let tip = f.remote_tip(repo, "staging");
        assert_eq!(merge["output"]["revision"], tip);
        let record: Value = read(&f.root.join(format!("record-{repo}.json")));
        assert_eq!(record["KNIT_REV"], tip, "KNIT_REV is the merged revision");
        assert_eq!(record["KNIT_SOURCE_SHA"], merge["output"]["source"]);
        assert_eq!(record["KNIT_TARGET_BRANCH"], "staging");
        assert_eq!(record["KNIT_LAND_ENVIRONMENT"], "preview");
    }
    // Feature checkouts were never touched.
    for (id, repo, _, _, pinned) in &f.repos {
        assert_eq!(head(repo, "HEAD"), *pinned, "{id} checkout moved");
    }
}

#[test]
fn later_repository_conflict_fails_the_whole_preflight() {
    let f = Fixture::new();
    sequence_lane(&f);
    f.plan();
    // Move beta's target so the pinned source conflicts with it.
    run_git(&f.repos[1].1, &["checkout", "staging"]);
    fs::write(f.repos[1].1.join("app.txt"), "target moved\n").unwrap();
    run_git(&f.repos[1].1, &["commit", "-am", "Target moved"]);
    run_git(&f.repos[1].1, &["push", "origin", "staging"]);
    run_git(&f.repos[1].1, &["checkout", "feature"]);

    let before: Vec<_> = f
        .repos
        .iter()
        .map(|(id, ..)| f.remote_tip(id, "staging"))
        .collect();
    let applied = f.apply("plan.json", false);
    assert!(!applied.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&applied.stdout),
        String::from_utf8_lossy(&applied.stderr)
    );
    assert!(text.contains("conflict"), "{text}");
    assert!(
        text.contains("app.txt"),
        "conflict report names files: {text}"
    );
    assert!(!f.trace_path().exists(), "no deployment may run");
    for ((id, ..), before) in f.repos.iter().zip(&before) {
        assert_eq!(&f.remote_tip(id, "staging"), before);
    }
}

#[test]
fn preflight_fetches_fresh_target_tips() {
    let f = Fixture::new();
    sequence_lane(&f);
    f.plan();
    // Non-conflicting target movement after generation.
    fs::write(f.repos[0].1.join("notes.txt"), "fresh\n").unwrap();
    run_git(&f.repos[0].1, &["add", "notes.txt"]);
    run_git(&f.repos[0].1, &["commit", "-m", "Fresh target work"]);
    run_git(&f.repos[0].1, &["push", "origin", "staging"]);
    let fresh_tip = f.remote_tip("alpha", "staging");

    let out = f.cmd(&[
        "land",
        "preflight",
        "--plan",
        "plan.json",
        "--from-artifact",
        f.bundle.to_str().unwrap(),
        "--project-file",
        f.project.to_str().unwrap(),
        "--repo-roots",
        "roots.json",
        "--json",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["valid"], true);
    assert!(report["planHash"].is_string());
    let alpha = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["repoId"] == "alpha")
        .unwrap();
    assert_eq!(alpha["targetSha"], fresh_tip);
    assert_eq!(alpha["status"], "mergeable");

    let applied = f.apply("plan.json", false);
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    let run = read(&f.root.join("run.json"));
    let merge = run["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "merge-alpha")
        .unwrap();
    assert_eq!(
        merge["output"]["revision"],
        f.remote_tip("alpha", "staging")
    );
}

#[test]
fn target_drift_is_rejected_and_resume_rechecks_pending_work() {
    let f = Fixture::new();
    sequence_lane(&f);
    let mut lane_deployments = f.recorder_deployments();
    lane_deployments[0]["env"]["INJECT"] = json!("1");
    let mut project = read(&f.project);
    project["landing"]["lanes"]["preview"]["deployments"] = lane_deployments;
    write(&f.project, &project);
    f.plan();

    let applied = f.apply("plan.json", false);
    assert!(!applied.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&applied.stdout),
        String::from_utf8_lossy(&applied.stderr)
    );
    assert!(text.to_lowercase().contains("drift"), "{text}");
    assert_eq!(
        fs::read_to_string(f.trace_path())
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        ["alpha"],
        "alpha completed; beta never ran"
    );
    let api_after = f.remote_tip("alpha", "staging");

    let resumed = f.apply("plan.json", true);
    assert!(
        resumed.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&resumed.stdout),
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(
        fs::read_to_string(f.trace_path())
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        ["alpha", "beta"]
    );
    assert_eq!(f.remote_tip("alpha", "staging"), api_after);
    let run = read(&f.root.join("run.json"));
    assert_eq!(run["status"], "succeeded");
}

#[test]
fn integration_sources_pin_provenance_and_reject_drift() {
    let f = Fixture::new();
    sequence_lane(&f);
    f.plan();
    // A compatibility branch that already contains the reviewed work.
    run_git(
        &f.repos[0].1,
        &["checkout", "-b", "compatibility", "feature"],
    );
    fs::write(f.repos[0].1.join("compat.txt"), "resolved companion work\n").unwrap();
    run_git(&f.repos[0].1, &["add", "."]);
    run_git(&f.repos[0].1, &["commit", "-m", "Compatibility work"]);
    run_git(&f.repos[0].1, &["push", "origin", "compatibility"]);
    let compat_sha = head(&f.repos[0].1, "HEAD");
    run_git(&f.repos[0].1, &["checkout", "feature"]);
    let original_plan = read(&f.root.join("plan.json"));

    let sourced = f.cmd(&[
        "land",
        "source",
        "--plan",
        "plan.json",
        "--repo",
        "alpha",
        "--branch",
        "compatibility",
        "--repo-root",
        f.repos[0].1.to_string_lossy().as_ref(),
        "--out",
        "selected.json",
    ]);
    assert!(
        sourced.status.success(),
        "{}",
        String::from_utf8_lossy(&sourced.stderr)
    );
    let selected = read(&f.root.join("selected.json"));
    assert_eq!(
        selected["integrationSources"]["alpha"],
        json!({"branch": "compatibility", "sha": compat_sha})
    );
    assert_eq!(selected["bundleHeads"], original_plan["bundleHeads"]);
    assert_eq!(
        read(&f.root.join("plan.json")),
        original_plan,
        "authoring must never modify the source plan"
    );
    assert_eq!(selected["requiredExecutorVersion"], "0.4");

    let applied = f.apply("selected.json", false);
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    let run = read(&f.root.join("run.json"));
    let merge = run["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "merge-alpha")
        .unwrap();
    assert_eq!(merge["output"]["source"], compat_sha);
    let staging_tip = f.remote_tip("alpha", "staging");
    assert!(git_succeeds(
        &f.repos[0].2,
        &["merge-base", "--is-ancestor", &compat_sha, &staging_tip]
    ));
}

#[test]
fn integration_source_drift_prevents_every_remote_merge() {
    let f = Fixture::new();
    sequence_lane(&f);
    f.plan();
    run_git(
        &f.repos[0].1,
        &["checkout", "-b", "compatibility", "feature"],
    );
    fs::write(f.repos[0].1.join("compat.txt"), "resolved companion work\n").unwrap();
    run_git(&f.repos[0].1, &["add", "."]);
    run_git(&f.repos[0].1, &["commit", "-m", "Compatibility work"]);
    run_git(&f.repos[0].1, &["push", "origin", "compatibility"]);
    run_git(&f.repos[0].1, &["checkout", "feature"]);
    let sourced = f.cmd(&[
        "land",
        "source",
        "--plan",
        "plan.json",
        "--repo",
        "alpha",
        "--branch",
        "compatibility",
        "--repo-root",
        f.repos[0].1.to_string_lossy().as_ref(),
        "--out",
        "selected.json",
    ]);
    assert!(sourced.status.success());
    // The branch moves after authoring: the pin no longer matches reality.
    run_git(&f.repos[0].1, &["checkout", "compatibility"]);
    fs::write(f.repos[0].1.join("compat.txt"), "source moved\n").unwrap();
    run_git(&f.repos[0].1, &["commit", "-am", "Source moved"]);
    run_git(&f.repos[0].1, &["push", "origin", "compatibility"]);
    run_git(&f.repos[0].1, &["checkout", "feature"]);

    let before: Vec<_> = f
        .repos
        .iter()
        .map(|(id, ..)| f.remote_tip(id, "staging"))
        .collect();
    let applied = f.apply("selected.json", false);
    assert!(!applied.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&applied.stdout),
        String::from_utf8_lossy(&applied.stderr)
    );
    assert!(text.to_lowercase().contains("drift"), "{text}");
    assert!(!f.trace_path().exists());
    for ((id, ..), before) in f.repos.iter().zip(&before) {
        assert_eq!(&f.remote_tip(id, "staging"), before);
    }
}

#[test]
fn landing_never_merges_automatically_into_a_feature_branch() {
    let f = Fixture::new();
    sequence_lane(&f);
    let mut plan = f.plan();
    plan["steps"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|s| s["id"] == "merge-alpha")
        .unwrap()["targetBranch"] = json!("feature");
    write(&f.root.join("plan.json"), &plan);
    let validated = f.cmd(&[
        "land",
        "validate",
        "--plan",
        "plan.json",
        "--from-artifact",
        f.bundle.to_str().unwrap(),
        "--project-file",
        f.project.to_str().unwrap(),
        "--json",
    ]);
    assert!(!validated.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&validated.stdout),
        String::from_utf8_lossy(&validated.stderr)
    );
    assert!(text.contains("feature branch"), "{text}");
}

#[test]
fn resume_skips_completed_steps() {
    let f = Fixture::new();
    sequence_lane(&f);
    // Beta's verify fails while the gate is absent; both merges and deploys
    // complete first.
    let mut lane_deployments = f.recorder_deployments();
    lane_deployments[1]["verify"] = json!({
        "command": [python_executable(), "-c",
            "import os,pathlib; raise SystemExit(0 if pathlib.Path(os.environ['VERIFY_GATE']).exists() else 9)"],
        "env": {"VERIFY_GATE": f.root.join("gate").to_string_lossy()}
    });
    let mut project = read(&f.project);
    project["landing"]["lanes"]["preview"]["deployments"] = lane_deployments;
    write(&f.project, &project);
    f.plan();

    let first = f.apply("plan.json", false);
    assert!(!first.status.success());
    assert_eq!(
        fs::read_to_string(f.trace_path())
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        ["alpha", "beta"]
    );

    fs::write(f.root.join("gate"), "ready\n").unwrap();
    let resumed = f.apply("plan.json", true);
    assert!(
        resumed.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&resumed.stdout),
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(
        fs::read_to_string(f.trace_path())
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        ["alpha", "beta"],
        "completed steps must not re-run"
    );
    let run = read(&f.root.join("run.json"));
    assert_eq!(run["status"], "succeeded");
}

#[test]
fn scoped_null_disables_and_absent_scope_inherits_the_root_policy() {
    let f = Fixture::new();
    let mut project = read(&f.project);
    project["landing"]["execution"] =
        json!({"mode": "repository_sequence", "repoOrder": ["alpha", "beta"]});
    project["landing"]["preflight"] = json!({"mergeability": "all"});
    project["landing"]["lanes"]["canary"] = json!({
        "terminal": false,
        "branches": {"alpha": "staging", "beta": "staging"},
        "execution": null,
        "preflight": null
    });
    write(&f.project, &project);

    let inherited = f.cmd(&[
        "land",
        "--lane",
        "preview",
        "plan",
        "--from-artifact",
        f.bundle.to_str().unwrap(),
        "--project-file",
        f.project.to_str().unwrap(),
        "--out",
        "inherited.json",
        "--json",
    ]);
    assert!(inherited.status.success());
    let plan = read(&f.root.join("inherited.json"));
    assert!(
        plan["execution"].is_object(),
        "absent scope key inherits root"
    );
    assert!(plan["preflight"].is_object());
    assert_eq!(plan["requiredExecutorVersion"], "0.4");

    let disabled = f.cmd(&[
        "land",
        "--lane",
        "canary",
        "plan",
        "--from-artifact",
        f.bundle.to_str().unwrap(),
        "--project-file",
        f.project.to_str().unwrap(),
        "--out",
        "disabled.json",
        "--json",
    ]);
    assert!(disabled.status.success());
    let plan = read(&f.root.join("disabled.json"));
    assert!(plan.get("execution").is_none(), "explicit null disables");
    assert!(plan.get("preflight").is_none());
    assert_eq!(plan["requiredExecutorVersion"], "0.3");
}

#[test]
fn local_apply_reports_missing_plans_and_still_rejects_legacy_semantics() {
    let local = LocalFixture::new(false);
    let f = &local.fixture;
    for args in [
        vec!["land", "apply"],
        vec!["land", "apply", "--plan", "missing.json"],
    ] {
        let applied = f.cmd(&args);
        assert!(!applied.status.success());
        let error = String::from_utf8_lossy(&applied.stderr);
        assert!(error.contains("No land plan found"), "{error}");
        assert!(error.contains("knit land plan"), "{error}");
    }
    for (key, value) in [
        (
            "execution",
            json!({"mode": "repository_sequence", "repoOrder": ["alpha"]}),
        ),
        ("preflight", json!({"mergeability": "all"})),
        (
            "integrationSources",
            json!({"alpha": {"branch": "compatibility", "sha": f.repos[0].4}}),
        ),
        ("requiredExecutorVersion", json!("0.4")),
    ] {
        let mut plan = json!({"schemaVersion": "0.1"});
        plan[key] = value;
        write(&f.root.join("legacy.json"), &plan);
        let applied = f.cmd(&["land", "apply", "--plan", "legacy.json"]);
        assert!(!applied.status.success());
        let error = String::from_utf8_lossy(&applied.stderr);
        assert!(
            error.contains(key) && error.contains("schema 0.2"),
            "{error}"
        );
    }
    assert!(!f.trace_path().exists());
    assert!(!f.root.join(".knit/land-runs").exists());
}

#[test]
fn legacy_schema_rejects_executor04_semantics() {
    let f = Fixture::new();
    sequence_lane(&f);
    let mut plan = f.plan();
    plan["schemaVersion"] = json!("0.1");
    write(&f.root.join("legacy.json"), &plan);
    let validated = f.cmd(&[
        "land",
        "validate",
        "--plan",
        "legacy.json",
        "--from-artifact",
        f.bundle.to_str().unwrap(),
        "--project-file",
        f.project.to_str().unwrap(),
        "--json",
    ]);
    assert!(!validated.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&validated.stdout),
        String::from_utf8_lossy(&validated.stderr)
    );
    assert!(text.contains("schema 0.2"), "{text}");
}

#[test]
fn sequence_generation_rejects_impossible_orders() {
    let f = Fixture::new();
    // beta's deployment watches alpha's changes: with order [beta, alpha] the
    // declared order contradicts the declared dependency.
    let mut deployments = f.recorder_deployments();
    deployments[1]["whenChanged"] = json!(["alpha"]);
    f.lane(json!({
        "execution": {"mode": "repository_sequence", "repoOrder": ["beta", "alpha"]},
        "deployments": deployments
    }));
    let out = f.cmd(&[
        "land",
        "--lane",
        "preview",
        "plan",
        "--from-artifact",
        f.bundle.to_str().unwrap(),
        "--project-file",
        f.project.to_str().unwrap(),
        "--out",
        "plan.json",
        "--json",
    ]);
    assert!(!out.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("contradicts a declared dependency"), "{text}");

    // Undeclared repository: beta has steps but is missing from repoOrder.
    f.lane(json!({
        "execution": {"mode": "repository_sequence", "repoOrder": ["alpha"]}
    }));
    let out = f.cmd(&[
        "land",
        "--lane",
        "preview",
        "plan",
        "--from-artifact",
        f.bundle.to_str().unwrap(),
        "--project-file",
        f.project.to_str().unwrap(),
        "--out",
        "plan.json",
        "--json",
    ]);
    assert!(!out.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("does not declare repository beta"), "{text}");
}

#[test]
fn parallel_cross_repository_workflow_cannot_pose_as_a_sequence() {
    let f = Fixture::new();
    sequence_lane(&f);
    let mut plan = f.plan();
    plan["workflow"] = json!({"parallel": [
        {"sequence": [{"step": "merge-alpha"}, {"step": "deploy-alpha"}]},
        {"sequence": [{"step": "merge-beta"}, {"step": "deploy-beta"}]}
    ]});
    write(&f.root.join("plan.json"), &plan);
    let validated = f.cmd(&[
        "land",
        "validate",
        "--plan",
        "plan.json",
        "--from-artifact",
        f.bundle.to_str().unwrap(),
        "--project-file",
        f.project.to_str().unwrap(),
        "--json",
    ]);
    assert!(!validated.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&validated.stdout),
        String::from_utf8_lossy(&validated.stderr)
    );
    assert!(text.contains("does not depend on"), "{text}");
}

#[test]
fn preflight_without_policy_requires_nothing() {
    let f = Fixture::new();
    f.plan();
    let out = f.cmd(&[
        "land",
        "preflight",
        "--plan",
        "plan.json",
        "--from-artifact",
        f.bundle.to_str().unwrap(),
        "--project-file",
        f.project.to_str().unwrap(),
        "--repo-roots",
        "roots.json",
        "--json",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["valid"], true);
    for check in report["checks"].as_array().unwrap() {
        assert_eq!(check["status"], "not_required");
        assert!(check["conflicts"].as_array().unwrap().is_empty());
    }
    assert!(report["handoff"]["repoRoots"].is_object());
}

#[test]
fn deleted_target_refuses_the_merge_and_resume_completes_after_restore() {
    let f = Fixture::new();
    sequence_lane(&f);
    let mut deployments = f.recorder_deployments();
    deployments[0]["env"]["INJECT"] = json!("delete");
    let mut project = read(&f.project);
    project["landing"]["lanes"]["preview"]["deployments"] = deployments;
    write(&f.project, &project);
    f.plan();
    let expected_beta = f.remote_tip("beta", "staging");

    let applied = f.apply("plan.json", false);
    assert!(!applied.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&applied.stdout),
        String::from_utf8_lossy(&applied.stderr)
    );
    assert!(text.contains("missing from origin"), "{text}");
    assert_eq!(
        fs::read_to_string(f.trace_path())
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        ["alpha"]
    );

    // The operator restores the deleted target to the planned tip.
    run_git(
        &f.repos[1].2,
        &["update-ref", "refs/heads/staging", &expected_beta],
    );
    let resumed = f.apply("plan.json", true);
    assert!(
        resumed.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&resumed.stdout),
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(
        fs::read_to_string(f.trace_path())
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        ["alpha", "beta"]
    );
}

// Inject a transport outage after alpha deployed: beta's first lookup is
// provider_step's before-state read, the second is the guarded lookup, and
// the third is prepare_branch_checkout's fetch. No process-global Git shim.
#[cfg(unix)]
fn guarded_target_transport_failure_resumes(fail_on: u32, diagnostic: &str) {
    let f = Fixture::new();
    sequence_lane(&f);
    let fault = f.root.join("upload-pack.py");
    fs::write(
        &fault,
        format!(
            r#"
import pathlib,subprocess,sys
counter=pathlib.Path(__file__).with_suffix('.count')
n=int(counter.read_text())+1 if counter.exists() else 1
counter.write_text(str(n))
if n == {fail_on}:
    print('synthetic target transport unavailable',file=sys.stderr)
    raise SystemExit(71)
raise SystemExit(subprocess.call(['git','upload-pack']+sys.argv[1:]))
"#
        ),
    )
    .unwrap();
    let quote = |s: &str| format!("'{}'", s.replace('\'', "'\"'\"'"));
    let transport = format!(
        "{} {}",
        quote(python_executable()),
        quote(fault.to_str().unwrap())
    );
    let mut deployments = f.recorder_deployments();
    deployments[0]["command"] = json!([python_executable(), "-c", format!(
        "{RECORDER}\nsubprocess.check_call(['git','-C',os.environ['BETA_CHECKOUT'],'config','remote.origin.uploadpack',os.environ['FAULT_TRANSPORT']])"
    )]);
    deployments[0]["env"]["BETA_CHECKOUT"] = json!(f.repos[1].1);
    deployments[0]["env"]["FAULT_TRANSPORT"] = json!(transport);
    let mut project = read(&f.project);
    project["landing"]["lanes"]["preview"]["deployments"] = deployments;
    write(&f.project, &project);
    f.plan();
    let beta_before = f.remote_tip("beta", "staging");
    let applied = f.apply("plan.json", false);
    assert!(!applied.status.success());
    let run = read(&f.root.join("run.json"));
    let receipt = run["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "merge-beta")
        .unwrap();
    assert_eq!(receipt["status"], "failed");
    assert_eq!(receipt["quiesced"], true);
    assert!(receipt.get("attribution").is_none(), "{receipt}");
    let error = receipt["error"].as_str().unwrap();
    assert!(error.contains(diagnostic), "{error}");
    assert!(
        error.contains("synthetic target transport unavailable"),
        "{error}"
    );
    assert!(error.contains("resume the landing run"), "{error}");
    assert_eq!(
        fs::read_to_string(fault.with_extension("count")).unwrap(),
        fail_on.to_string()
    );
    assert_eq!(f.remote_tip("beta", "staging"), beta_before);
    assert_eq!(
        fs::read_to_string(f.trace_path())
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        ["alpha"]
    );
    let alpha_after = f.remote_tip("alpha", "staging");

    run_git(
        &f.repos[1].1,
        &["config", "--unset", "remote.origin.uploadpack"],
    );
    let resumed = f.apply("plan.json", true);
    assert!(
        resumed.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&resumed.stdout),
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(
        fs::read_to_string(f.trace_path())
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        ["alpha", "beta"]
    );
    assert_eq!(f.remote_tip("alpha", "staging"), alpha_after);
    assert_eq!(read(&f.root.join("run.json"))["status"], "succeeded");
}

#[cfg(unix)]
#[test]
fn guarded_target_lookup_transport_failure_is_retryable() {
    guarded_target_transport_failure_resumes(2, "failed to read target origin/staging");
}

#[cfg(unix)]
#[test]
fn guarded_target_fetch_transport_failure_is_retryable() {
    guarded_target_transport_failure_resumes(3, "failed to prepare target origin/staging");
}

/// A local workspace fixture: the active bundle has real checkout bindings,
/// so local planning, `land source`, and local apply all resolve through
/// them. `in_place` selects inPlace checkout mode instead of a worktree path.
struct LocalFixture {
    fixture: Fixture,
}

impl LocalFixture {
    fn new(in_place: bool) -> Self {
        let f = Fixture::new();
        let mut bundle = read(&f.bundle);
        for repo in bundle["repos"].as_array_mut().unwrap() {
            if in_place {
                repo["checkoutMode"] = json!("inPlace");
                repo["worktreePath"] = Value::Null;
            } else {
                repo["checkoutMode"] = json!("worktree");
                repo["worktreePath"] = json!(
                    f.repos
                        .iter()
                        .find(|(id, ..)| id == repo["id"].as_str().unwrap())
                        .unwrap()
                        .1
                );
            }
        }
        write(&f.bundle, &bundle);
        sequence_lane(&f);
        let mut project = read(&f.project);
        project["landing"]["requireChecks"] = json!(["suite"]);
        write(&f.project, &project);
        Self { fixture: f }
    }

    /// Record the `suite` verdict while alpha's checkout sits on `branch`.
    /// `return_branch` moves the checkout back afterwards; recording at a
    /// non-feature revision only stays clean while the checkout stays there,
    /// because check pins are recorded heads.
    fn record_check_on(&self, branch: &str, return_branch: Option<&str>) {
        run_git(&self.fixture.repos[0].1, &["checkout", branch]);
        let out = self.fixture.cmd(&["check", "record", "suite", "--pass"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        if let Some(back) = return_branch {
            run_git(&self.fixture.repos[0].1, &["checkout", back]);
        }
    }

    fn author_compatibility_source(&self) -> String {
        run_git(
            &self.fixture.repos[0].1,
            &["checkout", "-b", "compatibility", "feature"],
        );
        fs::write(
            self.fixture.repos[0].1.join("compat.txt"),
            "resolved companion work\n",
        )
        .unwrap();
        run_git(&self.fixture.repos[0].1, &["add", "."]);
        run_git(
            &self.fixture.repos[0].1,
            &["commit", "-m", "Compatibility work"],
        );
        run_git(
            &self.fixture.repos[0].1,
            &["push", "origin", "compatibility"],
        );
        let sha = head(&self.fixture.repos[0].1, "HEAD");
        run_git(&self.fixture.repos[0].1, &["checkout", "feature"]);
        let local_plan =
            self.fixture
                .cmd(&["land", "--lane", "preview", "plan", "--out", "plan.json"]);
        assert!(
            local_plan.status.success(),
            "{}",
            String::from_utf8_lossy(&local_plan.stderr)
        );
        let sourced = self.fixture.cmd(&[
            "land",
            "source",
            "--plan",
            "plan.json",
            "--repo",
            "alpha",
            "--branch",
            "compatibility",
            "--out",
            "selected.json",
        ]);
        assert!(
            sourced.status.success(),
            "{}",
            String::from_utf8_lossy(&sourced.stderr)
        );
        sha
    }

    fn apply_selected(&self) -> std::process::Output {
        self.fixture.cmd(&[
            "land",
            "apply",
            "--plan",
            "selected.json",
            "--lane",
            "preview",
        ])
    }
}

fn required_checks_evaluate_integration_heads(in_place: bool) {
    let f = LocalFixture::new(in_place);

    // A verdict recorded at the reviewed feature head is stale for the
    // integration source that will actually run: old head checks are not
    // certified green for compatibility code.
    f.record_check_on("feature", Some("feature"));
    f.author_compatibility_source();
    let applied = f.apply_selected();
    assert!(!applied.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&applied.stdout),
        String::from_utf8_lossy(&applied.stderr)
    );
    assert!(text.contains("Required checks"), "{text}");
    assert!(text.contains("stale"), "{text}");
    assert!(
        !f.fixture.trace_path().exists(),
        "nothing may run with stale checks"
    );

    // A verdict recorded at the pinned source head stays green: the check
    // speaks for the code that will actually be integrated.
    let f = LocalFixture::new(in_place);
    f.author_compatibility_source();
    f.record_check_on("compatibility", None);
    let applied = f.apply_selected();
    assert!(
        applied.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&applied.stdout),
        String::from_utf8_lossy(&applied.stderr)
    );
    assert_eq!(
        fs::read_to_string(f.fixture.trace_path())
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        ["alpha", "beta"]
    );
    let runs = fs::read_dir(f.fixture.root.join(".knit/land-runs")).unwrap();
    for entry in runs {
        let entry = entry.unwrap();
        if entry.path().extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let run = read(&entry.path());
        let merge = run["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == "merge-alpha")
            .unwrap();
        assert_eq!(merge["output"]["sourceBranch"], "compatibility");
    }
}

#[test]
fn required_checks_evaluate_integration_heads_for_worktree_checkouts() {
    required_checks_evaluate_integration_heads(false);
}

#[test]
fn required_checks_evaluate_integration_heads_for_inplace_checkouts() {
    required_checks_evaluate_integration_heads(true);
}
