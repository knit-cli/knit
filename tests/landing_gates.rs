//! Paused local runs against real Git history and a synthetic forge.
#![cfg(unix)]
mod common;
use common::*;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
fn read(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}
fn write(path: &Path, value: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}
fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
    bin: PathBuf,
    state: PathBuf,
    collaborator: PathBuf,
    artifact: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = unique_temp_dir();
        let (_, backend, _) = init_remote_repo(&root, "backend");
        let (_, frontend, collaborator) = init_remote_repo(&root, "frontend");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        knit(&workspace, ["bundle", "library gates"]);
        knit(
            &workspace,
            [
                "bundle",
                "add",
                backend.to_str().unwrap(),
                frontend.to_str().unwrap(),
            ],
        );
        for repo in ["backend", "frontend"] {
            append_line(
                &workspace.join(format!(".knit/worktrees/library-gates/{repo}/app.txt")),
                "reviewed change",
            );
        }
        knit(
            &workspace,
            ["commit", "--all", "-m", "Reviewed source changes"],
        );
        let artifact = workspace.join(".knit/bundles/library-gates.bundle.json");
        let mut bundle = read(&artifact);
        let project_id = root.file_name().unwrap().to_str().unwrap();
        bundle["projectId"] = json!(project_id);
        bundle["publications"] = json!(["backend", "frontend"].map(|repo| json!({"repoId":repo,"provider":"github","kind":"pull_request","number":1,"url":format!("https://github.com/example/{repo}/pull/1"),"baseBranch":"main","headBranch":"knit/library-gates","state":"OPEN","title":"Synthetic review","updatedAt":"2026-01-01T00:00:00Z"})));
        write(&artifact, &bundle);
        let mut project = serde_json::to_value(knit::model::KnitProject::new(
            project_id.to_owned(),
            "2026-01-01T00:00:00Z".into(),
        ))
        .unwrap();
        // The wildcard must include frontend even though it is bundle-only.
        project["repos"] = json!([{"id":"backend","path":backend,"baseBranch":"main"}]);
        project["landing"] = json!({"dependencies":[{"library":"backend","consumers":"*","release":{"instructions":"Publish the library release."},"bump":{"instructions":"Update the consumer dependency.","paths":["Cargo.toml","Cargo.lock"]}}]});
        write(
            &workspace.join(format!(".knit/projects/{project_id}.project.json")),
            &project,
        );
        let bin = root.join("bin");
        let state = root.join("forge");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&state).unwrap();
        fs::write(bin.join("gh"), r#"#!/usr/bin/env python3
import os,sys,json,pathlib,subprocess
s=pathlib.Path(os.environ['GATE_FORGE']); a=sys.argv[1:]; repo=pathlib.Path.cwd().name
if len(a)>2 and a[2].startswith('https://'): repo=a[2].split('/')[4]
p=s/(repo+'.json'); v=json.loads(p.read_text()); head=v['head']; merged=v.get('merged',False)
with (s/'calls').open('a') as f: f.write(json.dumps({'repo':repo,'args':a,'head':head})+'\n')
if a[:2]==['pr','view']:
 print(json.dumps({'number':1,'url':'https://github.com/example/'+repo+'/pull/1','state':'MERGED' if merged else 'OPEN','title':'Synthetic review','baseRefName':v.get('base','main'),'headRefName':v.get('branch','knit/library-gates'),'headRepository':{'nameWithOwner':v.get('source','example/'+repo)},'isDraft':False,'headRefOid':head,'mergeable':'MERGEABLE','mergeStateStatus':'CLEAN','reviewDecision':'','mergeCommit':{'oid':head} if merged else None}))
elif a[:2]==['pr','checks']:
 if (s/'checks-fail').exists(): sys.exit(1)
 if (s/'identity-drift').exists():
  v.update(json.loads((s/'identity-drift').read_text()));p.write_text(json.dumps(v))
 if (s/'drift').exists():
  v['head']=(s/'drift').read_text().strip();p.write_text(json.dumps(v))
 print('[]')
elif a[:2]==['pr','edit']:
 v['base']=a[a.index('--base')+1];p.write_text(json.dumps(v))
elif a[:2]==['pr','merge']:
 assert a[a.index('--match-head-commit')+1]==head, 'head moved after checks'
 subprocess.run(['git','--git-dir',v['remote'],'update-ref','refs/heads/'+v.get('base','main'),head],check=True)
 v['merged']=True;p.write_text(json.dumps(v))
else: raise Exception('unexpected forge call '+str(a))
"#).unwrap();
        fs::set_permissions(bin.join("gh"), fs::Permissions::from_mode(0o755)).unwrap();
        for repo in bundle["repos"].as_array().unwrap() {
            let id = repo["id"].as_str().unwrap();
            let checkout = workspace.join(format!(".knit/worktrees/library-gates/{id}"));
            git(&checkout, ["push", "origin", "HEAD"]);
            let remote = git(&checkout, ["remote", "get-url", "origin"]);
            write(
                &state.join(format!("{id}.json")),
                &json!({"head":repo["headSha"],"remote":remote.trim()}),
            );
        }
        let f = Self {
            root,
            workspace,
            bin,
            state,
            collaborator,
            artifact,
        };
        success(f.cmd(&["land", "plan", "--out", "plan.json"]));
        f
    }
    fn cmd(&self, args: &[&str]) -> Output {
        let path = std::env::join_paths(
            std::iter::once(self.bin.clone())
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        Command::new(env!("CARGO_BIN_EXE_knit"))
            .current_dir(&self.workspace)
            .args(args)
            .env("PATH", path)
            .env("GATE_FORGE", &self.state)
            .env("KNIT_HOME", self.root.join("home"))
            .env_remove("KNIT_BUNDLE")
            .env_remove("KNIT_SESSION")
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .output()
            .unwrap()
    }
    fn run_path(&self) -> PathBuf {
        fs::read_dir(self.workspace.join(".knit/land-runs"))
            .unwrap()
            .map(|p| p.unwrap().path())
            .find(|p| p.extension().is_some_and(|s| s == "json"))
            .unwrap()
    }
    fn pause(&self) -> Value {
        success(self.cmd(&[
            "land",
            "apply",
            "--plan",
            "plan.json",
            "--no-remote",
            "--keep-worktrees",
        ]));
        let run = read(&self.run_path());
        assert_eq!(run["status"], "paused");
        assert_eq!(run["pause"]["step"], "release-backend");
        run
    }
    fn bump(&self, file: &str) -> String {
        git(&self.collaborator, ["fetch", "origin"]);
        git(
            &self.collaborator,
            ["checkout", "-B", "bump", "origin/knit/library-gates"],
        );
        fs::write(self.collaborator.join(file), "synthetic dependency bump\n").unwrap();
        git(&self.collaborator, ["add", "--", file]);
        git(&self.collaborator, ["commit", "-m", "Update dependency"]);
        git(
            &self.collaborator,
            ["push", "origin", "HEAD:knit/library-gates"],
        );
        let head = git(&self.collaborator, ["rev-parse", "HEAD"])
            .trim()
            .to_owned();
        let path = self.state.join("frontend.json");
        let mut state = read(&path);
        state["head"] = json!(head);
        write(&path, &state);
        head
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
#[test]
fn library_release_and_remote_bump_use_one_immutable_run_and_exact_merge_pin() {
    let f = Fixture::new();
    let plan = read(&f.workspace.join("plan.json"));
    let validator = jsonschema::validator_for(
        &serde_json::from_str::<Value>(include_str!("../schemas/land-plan.schema.json")).unwrap(),
    )
    .unwrap();
    assert!(
        validator.is_valid(&plan),
        "{:?}",
        validator.iter_errors(&plan).collect::<Vec<_>>()
    );
    assert!(plan["steps"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["id"] == "bump-frontend"));
    let first = f.pause();
    assert_eq!(read(&f.artifact)["state"], "open");
    success(f.cmd(&[
        "land",
        "resume",
        "--acknowledge",
        "release-backend",
        "--note",
        "Library published",
        "--no-remote",
        "--json",
    ]));
    let waiting = read(&f.run_path());
    assert_eq!(waiting["status"], "paused");
    assert_eq!(waiting["pause"]["step"], "bump-frontend");
    assert_eq!(
        waiting["acknowledgements"]["release-backend"]["notes"],
        "Library published"
    );
    let bumped = f.bump("Cargo.toml");
    success(f.cmd(&[
        "land",
        "resume",
        "--no-remote",
        "--keep-worktrees",
        "--json",
    ]));
    let complete = read(&f.run_path());
    assert_eq!(complete["status"], "succeeded");
    assert_eq!(complete["id"], first["id"]);
    assert_eq!(complete["planHash"], first["planHash"]);
    assert_eq!(complete["plan"], plan);
    let gate = complete["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "bump-frontend")
        .unwrap();
    assert_eq!(gate["output"]["revision"], bumped);
    let calls = fs::read_to_string(f.state.join("calls")).unwrap();
    let calls: Vec<Value> = calls
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let merges: Vec<_> = calls.iter().filter(|c| c["args"][1] == "merge").collect();
    assert_eq!(merges.len(), 2);
    assert_eq!(merges[1]["head"], bumped);
    assert!(merges[1]["args"]
        .as_array()
        .unwrap()
        .contains(&json!(bumped)));
    assert!(calls
        .iter()
        .any(|c| c["repo"] == "frontend" && c["args"][1] == "checks" && c["head"] == bumped));
    assert!(read(&f.artifact)["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n["type"] == "git.observed" && n.to_string().contains(&bumped)));
    let schema: Value =
        serde_json::from_str(include_str!("../schemas/land-run.schema.json")).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    for run in [&first, &waiting, &complete] {
        assert!(
            validator.is_valid(run),
            "{:?}",
            validator.iter_errors(run).collect::<Vec<_>>()
        );
    }
}
#[test]
fn forbidden_update_cannot_be_acknowledged_away() {
    let f = Fixture::new();
    f.pause();
    f.bump("app.txt");
    let output = f.cmd(&[
        "land",
        "resume",
        "--acknowledge",
        "release-backend",
        "--acknowledge",
        "bump-frontend",
        "--no-remote",
    ]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("outside the allowed paths"));
    assert_ne!(read(&f.state.join("frontend.json"))["merged"], true);
}
#[test]
fn acknowledgement_accepts_an_already_bumped_review_but_unknown_gate_fails() {
    let f = Fixture::new();
    let first = f.pause();
    let bad = f.cmd(&["land", "resume", "--acknowledge", "typo", "--no-remote"]);
    assert!(!bad.status.success());
    assert_eq!(read(&f.run_path()), first);
    success(f.cmd(&[
        "land",
        "resume",
        "--acknowledge",
        "release-backend",
        "--acknowledge",
        "bump-frontend",
        "--no-remote",
        "--keep-worktrees",
    ]));
    assert_eq!(read(&f.run_path())["status"], "succeeded");
}
#[test]
fn resume_refuses_review_identity_and_base_changes() {
    let f = Fixture::new();
    f.pause();
    let original = read(&f.artifact);
    for field in ["baseBranch", "featureBranch", "baseSha"] {
        let mut changed = original.clone();
        changed["repos"][1][field] = json!("unexpected");
        write(&f.artifact, &changed);
        assert!(
            !f.cmd(&["land", "resume", "--no-remote"]).status.success(),
            "{field}"
        );
    }
    let mut changed = original.clone();
    changed["publications"][1]["url"] = json!("https://github.com/example/frontend/pull/2");
    write(&f.artifact, &changed);
    assert!(!f.cmd(&["land", "resume", "--no-remote"]).status.success());
}

#[test]
fn forge_cas_refuses_a_head_that_moves_during_fresh_checks() {
    let f = Fixture::new();
    f.pause();
    let accepted = f.bump("Cargo.toml");
    fs::write(f.collaborator.join("Cargo.lock"), "next update\n").unwrap();
    git(&f.collaborator, ["add", "Cargo.lock"]);
    git(&f.collaborator, ["commit", "-m", "Subsequent update"]);
    let moved = git(&f.collaborator, ["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    fs::write(f.state.join("drift"), &moved).unwrap();
    let output = f.cmd(&[
        "land",
        "resume",
        "--acknowledge",
        "release-backend",
        "--no-remote",
    ]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("live review head changed after checks")
    );
    assert_ne!(read(&f.state.join("frontend.json"))["merged"], true);
    let run = read(&f.run_path());
    let gate = run["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "bump-frontend")
        .unwrap();
    assert_eq!(gate["output"]["revision"], accepted);
}

#[test]
fn synced_bump_resumes_but_new_repository_scope_does_not() {
    let f = Fixture::new();
    f.pause();
    let accepted = f.bump("Cargo.toml");
    let checkout = f.workspace.join(".knit/worktrees/library-gates/frontend");
    git(&checkout, ["fetch", "origin"]);
    git(
        &checkout,
        ["merge", "--ff-only", "origin/knit/library-gates"],
    );
    success(f.cmd(&["sync"]));
    let recorded = read(&f.artifact);
    let mut extra = recorded.clone();
    extra["repos"].as_array_mut().unwrap().push(
        json!({"id":"unexpected","path":f.collaborator,"baseBranch":"main","headSha":accepted}),
    );
    write(&f.artifact, &extra);
    assert!(!f.cmd(&["land", "resume", "--no-remote"]).status.success());
    write(&f.artifact, &recorded);
    success(f.cmd(&[
        "land",
        "resume",
        "--acknowledge",
        "release-backend",
        "--no-remote",
        "--keep-worktrees",
    ]));
    assert_eq!(read(&f.run_path())["status"], "succeeded");
}

#[test]
fn named_checks_must_be_refreshed_for_the_accepted_bump_and_can_be_retried() {
    for record_before_resume in [false, true] {
        let f = Fixture::new();
        let project_path = f.workspace.join(format!(
            ".knit/projects/{}.project.json",
            f.root.file_name().unwrap().to_str().unwrap()
        ));
        let mut project = read(&project_path);
        project["landing"]["requireChecks"] = json!(["quality"]);
        write(&project_path, &project);
        success(f.cmd(&["land", "plan", "--out", "plan.json", "--force"]));
        success(f.cmd(&["check", "record", "quality", "--pass"]));
        f.pause();
        f.bump("Cargo.toml");
        if !record_before_resume {
            let stale = f.cmd(&[
                "land",
                "resume",
                "--acknowledge",
                "release-backend",
                "--no-remote",
            ]);
            assert!(!stale.status.success());
            assert!(String::from_utf8_lossy(&stale.stderr).contains("stale"));
            let run = read(&f.run_path());
            let merge = run["steps"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["id"] == "merge-frontend")
                .unwrap();
            assert_ne!(merge["attribution"], "uncertain");
        }
        let checkout = f.workspace.join(".knit/worktrees/library-gates/frontend");
        git(&checkout, ["fetch", "origin"]);
        git(
            &checkout,
            ["merge", "--ff-only", "origin/knit/library-gates"],
        );
        success(f.cmd(&["sync"]));
        success(f.cmd(&["check", "record", "quality", "--pass"]));
        success(f.cmd(&[
            "land",
            "resume",
            "--acknowledge",
            "release-backend",
            "--no-remote",
            "--keep-worktrees",
        ]));
        assert_eq!(read(&f.run_path())["status"], "succeeded");
    }
}

#[test]
fn adapter_without_atomic_merge_refuses_before_library_effects() {
    let f = Fixture::new();
    let mut bundle = read(&f.artifact);
    bundle["repos"][1]["remote"] = json!("https://bitbucket.org/example/frontend.git");
    write(&f.artifact, &bundle);
    success(f.cmd(&["land", "plan", "--out", "plan.json", "--force"]));
    let rejected = f.cmd(&["land", "apply", "--plan", "plan.json", "--no-remote"]);
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("atomic head-conditional merge"));
    assert_ne!(read(&f.state.join("backend.json"))["merged"], true);
    assert!(!f.workspace.join(".knit/land-runs").exists());
}

#[test]
fn accepted_gate_refuses_later_bundle_commits() {
    let f = Fixture::new();
    let project_path = f.workspace.join(format!(
        ".knit/projects/{}.project.json",
        f.root.file_name().unwrap().to_str().unwrap()
    ));
    let mut project = read(&project_path);
    project["landing"]["requireChecks"] = json!(["quality"]);
    write(&project_path, &project);
    success(f.cmd(&["land", "plan", "--out", "plan.json", "--force"]));
    success(f.cmd(&["check", "record", "quality", "--pass"]));
    f.pause();
    let accepted = f.bump("Cargo.toml");
    let stale = f.cmd(&[
        "land",
        "resume",
        "--acknowledge",
        "release-backend",
        "--no-remote",
    ]);
    assert!(!stale.status.success());
    assert!(String::from_utf8_lossy(&stale.stderr).contains("stale"));
    let run = read(&f.run_path());
    assert!(run["steps"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["type"] == "await_update"
            && s["status"] == "succeeded"
            && s["output"]["revision"] == accepted));
    let checkout = f.workspace.join(".knit/worktrees/library-gates/frontend");
    git(&checkout, ["fetch", "origin"]);
    git(
        &checkout,
        ["merge", "--ff-only", "origin/knit/library-gates"],
    );
    append_line(&checkout.join("Cargo.toml"), "later unreviewed change");
    git(&checkout, ["add", "Cargo.toml"]);
    git(&checkout, ["commit", "-m", "Later dependency change"]);
    success(f.cmd(&["sync"]));
    let refused = f.cmd(&["land", "resume", "--no-remote"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("immutable head"));
    assert_ne!(read(&f.state.join("frontend.json"))["merged"], true);
}

#[test]
fn update_gate_never_discards_unpushed_checkout_commits() {
    let f = Fixture::new();
    f.pause();
    f.bump("Cargo.toml");
    let checkout = f.workspace.join(".knit/worktrees/library-gates/frontend");
    append_line(&checkout.join("app.txt"), "unpushed local work");
    git(&checkout, ["add", "app.txt"]);
    git(&checkout, ["commit", "-m", "Local work"]);
    let refused = f.cmd(&[
        "land",
        "resume",
        "--acknowledge",
        "release-backend",
        "--no-remote",
    ]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("local checkout contains work"));
    assert_ne!(read(&f.state.join("frontend.json"))["merged"], true);
}

#[test]
fn commands_started_after_update_use_accepted_sources_not_prior_step_pins() {
    let f = Fixture::new();
    let mut plan = read(&f.workspace.join("plan.json"));
    let reviewed = plan["bundleHeads"]["frontend"].as_str().unwrap().to_owned();
    let observations = f.root.join("observations");
    let script = format!("import os,subprocess,pathlib; root=os.getcwd(); head=subprocess.check_output(['git','-C',root,'rev-parse','HEAD']).decode().strip(); p=pathlib.Path({:?}); p.open('a').write(head+'\\n')", observations.to_str().unwrap());
    let steps = plan["steps"].as_array_mut().unwrap();
    steps
        .iter_mut()
        .find(|s| s["id"] == "release-backend")
        .unwrap()["needs"]
        .as_array_mut()
        .unwrap()
        .push(json!("verify-before"));
    steps
        .iter_mut()
        .find(|s| s["id"] == "merge-frontend")
        .unwrap()["needs"]
        .as_array_mut()
        .unwrap()
        .push(json!("verify-after"));
    for (id, needs) in [
        ("verify-before", vec![]),
        ("verify-after", vec!["bump-frontend"]),
    ] {
        let step = json!({"id":id,"type":"run","role":"verify","repoId":"frontend","needs":needs,"effect":"read_only","command":[python_executable(),"-c",script]});
        steps.push(step);
    }
    write(&f.workspace.join("plan.json"), &plan);
    f.pause();
    assert_eq!(
        fs::read_to_string(&observations).unwrap(),
        format!("{reviewed}\n")
    );
    let accepted = f.bump("Cargo.toml");
    success(f.cmd(&[
        "land",
        "resume",
        "--acknowledge",
        "release-backend",
        "--no-remote",
        "--keep-worktrees",
    ]));
    assert_eq!(
        fs::read_to_string(&observations).unwrap(),
        format!("{reviewed}\n{accepted}\n")
    );
    let run = read(&f.run_path());
    for (id, head) in [("verify-before", reviewed), ("verify-after", accepted)] {
        assert_eq!(
            run["steps"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["id"] == id)
                .unwrap()["sourceRevisions"]["frontend"],
            head
        );
    }
}

#[test]
fn accepted_update_refuses_live_identity_drift_on_resume_then_retries() {
    let f = Fixture::new();
    let project_path = f.workspace.join(format!(
        ".knit/projects/{}.project.json",
        f.root.file_name().unwrap().to_str().unwrap()
    ));
    let mut project = read(&project_path);
    project["landing"]["requireChecks"] = json!(["quality"]);
    write(&project_path, &project);
    success(f.cmd(&["land", "plan", "--out", "plan.json", "--force"]));
    success(f.cmd(&["check", "record", "quality", "--pass"]));
    f.pause();
    f.bump("Cargo.toml");
    let stale = f.cmd(&[
        "land",
        "resume",
        "--acknowledge",
        "release-backend",
        "--no-remote",
    ]);
    assert!(!stale.status.success());
    assert!(String::from_utf8_lossy(&stale.stderr).contains("stale"));
    let checkout = f.workspace.join(".knit/worktrees/library-gates/frontend");
    git(&checkout, ["fetch", "origin"]);
    git(
        &checkout,
        ["merge", "--ff-only", "origin/knit/library-gates"],
    );
    success(f.cmd(&["sync"]));
    success(f.cmd(&["check", "record", "quality", "--pass"]));
    let original = read(&f.state.join("frontend.json"));
    for (field, value, message) in [
        ("base", "unexpected", "base changed"),
        ("branch", "unexpected", "source branch changed"),
        ("source", "elsewhere/frontend", "source repository"),
    ] {
        let mut drift = original.clone();
        drift[field] = json!(value);
        write(&f.state.join("frontend.json"), &drift);
        let calls_before = fs::read_to_string(f.state.join("calls")).unwrap();
        let refused = f.cmd(&["land", "resume", "--no-remote"]);
        assert!(!refused.status.success());
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains(message),
            "{}",
            String::from_utf8_lossy(&refused.stderr)
        );
        let calls_after = fs::read_to_string(f.state.join("calls")).unwrap();
        assert!(!calls_after[calls_before.len()..].contains("\"edit\""));
        assert!(!calls_after[calls_before.len()..].contains("\"merge\""));
    }
    write(&f.state.join("frontend.json"), &original);
    let checkout = f.workspace.join(".knit/worktrees/library-gates/frontend");
    git(&checkout, ["fetch", "origin"]);
    git(
        &checkout,
        ["merge", "--ff-only", "origin/knit/library-gates"],
    );
    success(f.cmd(&["sync"]));
    success(f.cmd(&["check", "record", "quality", "--pass"]));
    success(f.cmd(&["land", "resume", "--no-remote", "--keep-worktrees"]));
    assert_eq!(read(&f.run_path())["status"], "succeeded");
}

#[test]
fn gated_merge_rechecks_identity_after_ci_before_any_retarget_or_merge() {
    for field in ["base", "branch", "source"] {
        let f = Fixture::new();
        f.pause();
        f.bump("Cargo.toml");
        let original = read(&f.state.join("frontend.json"));
        let mut drift = json!({});
        drift[field] = json!("unexpected");
        write(&f.state.join("identity-drift"), &drift);
        let refused = f.cmd(&[
            "land",
            "resume",
            "--acknowledge",
            "release-backend",
            "--no-remote",
        ]);
        assert!(!refused.status.success());
        assert!(String::from_utf8_lossy(&refused.stderr).contains("gated review"));
        assert_ne!(read(&f.state.join("frontend.json"))["merged"], true);
        fs::remove_file(f.state.join("identity-drift")).unwrap();
        write(&f.state.join("frontend.json"), &original);
        success(f.cmd(&["land", "resume", "--no-remote", "--keep-worktrees"]));
        assert_eq!(read(&f.run_path())["status"], "succeeded");
    }
}

#[test]
fn explicit_retarget_is_allowed_and_its_confirmed_base_remains_pinned_on_retry() {
    let f = Fixture::new();
    for repo in ["backend", "frontend"] {
        git(
            &f.workspace
                .join(format!(".knit/worktrees/library-gates/{repo}")),
            ["push", "origin", "HEAD:refs/heads/release"],
        );
    }
    let project_path = f.workspace.join(format!(
        ".knit/projects/{}.project.json",
        f.root.file_name().unwrap().to_str().unwrap()
    ));
    let mut project = read(&project_path);
    project["landing"]["requireChecks"] = json!(["quality"]);
    project["landing"]["targets"] = json!({"release":{"terminal":true}});
    write(&project_path, &project);
    success(f.cmd(&[
        "land",
        "--target",
        "release",
        "plan",
        "--out",
        "plan.json",
        "--force",
    ]));
    success(f.cmd(&["check", "record", "quality", "--pass"]));
    f.pause();
    f.bump("Cargo.toml");
    let stale = f.cmd(&[
        "land",
        "resume",
        "--acknowledge",
        "release-backend",
        "--no-remote",
    ]);
    assert!(!stale.status.success());
    assert!(
        String::from_utf8_lossy(&stale.stderr).contains("stale"),
        "{}",
        String::from_utf8_lossy(&stale.stderr)
    );
    let checkout = f.workspace.join(".knit/worktrees/library-gates/frontend");
    git(&checkout, ["fetch", "origin"]);
    git(
        &checkout,
        ["merge", "--ff-only", "origin/knit/library-gates"],
    );
    success(f.cmd(&["sync"]));
    success(f.cmd(&["check", "record", "quality", "--pass"]));
    let original = read(&f.state.join("frontend.json"));
    assert_eq!(original["base"], "release");
    let mut drift = original.clone();
    drift["base"] = json!("main");
    write(&f.state.join("frontend.json"), &drift);
    let refused = f.cmd(&["land", "resume", "--no-remote"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("base changed"));
    write(&f.state.join("frontend.json"), &original);
    let checkout = f.workspace.join(".knit/worktrees/library-gates/frontend");
    git(&checkout, ["fetch", "origin"]);
    git(
        &checkout,
        ["merge", "--ff-only", "origin/knit/library-gates"],
    );
    success(f.cmd(&["sync"]));
    success(f.cmd(&["check", "record", "quality", "--pass"]));
    success(f.cmd(&["land", "resume", "--no-remote", "--keep-worktrees"]));
    assert_eq!(read(&f.run_path())["status"], "succeeded");
}
