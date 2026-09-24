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
        panic!("landing execution tests require a working Python 3 interpreter");
    })
}

mod common;
use common::*;
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
fn write(path: &Path, value: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}
fn read(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}
struct Fixture {
    root: PathBuf,
    service: PathBuf,
    project: PathBuf,
    bundle: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = unique_temp_dir();
        let service = root.join("service");
        init_repo(&service, "service");
        fs::write(service.join("release.py"),r#"import os,json,pathlib,sys
state=pathlib.Path(os.environ['STATE'])
if sys.argv[1]=='capture': print(json.dumps({'version':state.read_text()}))
elif sys.argv[1]=='deploy':
 state.write_text('partial')
 sys.exit(9)
elif sys.argv[1]=='restore': state.write_text(json.loads(os.environ['KNIT_LAND_CAPTURE'])['version'])
elif sys.argv[1]=='verify': assert state.read_text()==json.loads(os.environ['KNIT_LAND_CAPTURE'])['version']
"#).unwrap();
        git(&service, ["add", "release.py"]);
        git(
            &service,
            ["commit", "-m", "Add synthetic deployment adapter"],
        );
        let head = git(&service, ["rev-parse", "HEAD"]).trim().to_string();
        fs::write(root.join("state"), "previous").unwrap();
        let project_id = root.file_name().unwrap().to_str().unwrap().to_owned();
        let config = json!({"schemaVersion":"0.1","kind":"KnitConfig","activeBundle":"demo","activeProject":project_id});
        write(&root.join(".knit/config.json"), &config);
        let mut b = serde_json::to_value(knit::model::ChangeGroup::new(
            "demo".into(),
            "Synthetic deployment".into(),
            "2026-01-01T00:00:00Z".into(),
        ))
        .unwrap();
        b["projectId"] = json!(project_id);
        b["repos"] = json!([{"id":"service","path":service,"worktreePath":service,"baseBranch":"main","baseSha":head,"headSha":head}]);
        let bundle = root.join(".knit/bundles/demo.bundle.json");
        write(&bundle, &b);
        let mut p = serde_json::to_value(knit::model::KnitProject::new(
            project_id.clone(),
            "2026-01-01T00:00:00Z".into(),
        ))
        .unwrap();
        p["repos"] = json!([{"id":"service","path":service,"baseBranch":"main"}]);
        p["landing"] = json!({"onFailure":"recover","steps":[{"id":"deploy","type":"run","repoId":"service","effect":"deployment","env":{"STATE":root.join("state")},"command":[python_executable(),"release.py","deploy"],"recovery":{"mode":"command","idempotent":true,"capture":{"command":[python_executable(),"release.py","capture"]},"command":[python_executable(),"release.py","restore"],"verify":{"command":[python_executable(),"release.py","verify"]}}}]});
        let project = root.join(format!(".knit/projects/{project_id}.project.json"));
        write(&project, &p);
        write(&root.join("roots.json"), &json!({"service":service}));
        Self {
            root,
            service,
            project,
            bundle,
        }
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
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
#[test]
fn default_generation_matches_artifact_and_json_failure_restores_real_state() {
    let f = Fixture::new();
    let local = f.cmd(&["land", "plan", "--out", "local.json", "--json"]);
    assert!(
        local.status.success(),
        "{}",
        String::from_utf8_lossy(&local.stderr)
    );
    let hosted = f.cmd(&[
        "land",
        "plan",
        "--from-artifact",
        f.bundle.to_str().unwrap(),
        "--project-file",
        f.project.to_str().unwrap(),
        "--out",
        "hosted.json",
        "--json",
    ]);
    assert!(
        hosted.status.success(),
        "{}",
        String::from_utf8_lossy(&hosted.stderr)
    );
    let mut a: Value = serde_json::from_slice(&local.stdout).unwrap();
    let mut b: Value = serde_json::from_slice(&hosted.stdout).unwrap();
    a.as_object_mut().unwrap().remove("createdAt");
    b.as_object_mut().unwrap().remove("createdAt");
    assert_eq!(a, b);
    assert_eq!(a["schemaVersion"], "0.2");
    let plan_schema: Value =
        serde_json::from_str(include_str!("../schemas/land-plan.schema.json")).unwrap();
    let validator = jsonschema::validator_for(&plan_schema).unwrap();
    assert!(
        validator.is_valid(&a),
        "{:?}",
        validator
            .iter_errors(&a)
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
    );
    let validated = f.cmd(&[
        "land",
        "validate",
        "--plan",
        "hosted.json",
        "--from-artifact",
        f.bundle.to_str().unwrap(),
        "--project-file",
        f.project.to_str().unwrap(),
        "--json",
    ]);
    assert!(
        validated.status.success(),
        "{}",
        String::from_utf8_lossy(&validated.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&validated.stdout).unwrap()["valid"],
        true
    );
    let result = f.cmd(&[
        "land",
        "apply",
        "--plan",
        "hosted.json",
        "--from-artifact",
        f.bundle.to_str().unwrap(),
        "--project-file",
        f.project.to_str().unwrap(),
        "--repo-roots",
        "roots.json",
        "--run-out",
        "run.json",
        "--out",
        "out.json",
        "--json",
    ]);
    assert!(!result.status.success());
    let r: Value = serde_json::from_slice(&result.stdout).unwrap_or_else(|e| {
        panic!(
            "{e}: stdout={} stderr={}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        )
    });
    assert_eq!(r["serviceStatus"], "restored");
    let run_schema: Value =
        serde_json::from_str(include_str!("../schemas/land-run.schema.json")).unwrap();
    let validator = jsonschema::validator_for(&run_schema).unwrap();
    assert!(
        validator.is_valid(&r),
        "{:?}",
        validator
            .iter_errors(&r)
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        fs::read_to_string(f.root.join("state")).unwrap(),
        "previous"
    );
    assert!(f.root.join("out.json").exists());
    assert_eq!(git(&f.service, ["branch", "--show-current"]).trim(), "main");
    // Recovery preview is structured and does not mutate the original receipt.
    let before = fs::read(f.root.join("run.json")).unwrap();
    let preview = f.cmd(&["land", "recover", "--run", "run.json", "--json"]);
    assert!(preview.status.success());
    serde_json::from_slice::<Value>(&preview.stdout).unwrap();
    assert_eq!(before, fs::read(f.root.join("run.json")).unwrap());
}
#[test]
fn local_executor_uses_same_saved_plan_and_preserves_forward_failure() {
    let f = Fixture::new();
    let generated = f.cmd(&["land", "plan"]);
    assert!(
        generated.status.success(),
        "{}",
        String::from_utf8_lossy(&generated.stderr)
    );
    let result = f.cmd(&["land", "apply", "--no-remote"]);
    assert!(!result.status.success());
    assert_eq!(
        fs::read_to_string(f.root.join("state")).unwrap(),
        "previous"
    );
    let runs: Vec<_> = fs::read_dir(f.root.join(".knit/land-runs"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|e| e == "json"))
        .collect();
    assert_eq!(runs.len(), 1);
    let run = read(&runs[0].path());
    assert_eq!(run["status"], "failed");
    assert_eq!(run["serviceStatus"], "restored");
    assert!(run["planHash"].is_string());
    let resume = f.cmd(&[
        "land",
        "resume",
        "--run",
        runs[0].path().to_str().unwrap(),
        "--no-remote",
    ]);
    assert!(!resume.status.success());
    assert!(String::from_utf8_lossy(&resume.stderr).contains("recovery has started"));
}
#[cfg(unix)]
#[test]
fn cancellation_quiesces_forward_commands_and_requires_explicit_recovery() {
    let f = Fixture::new();
    let generated = f.cmd(&["land", "plan", "--out", "plan.json"]);
    assert!(generated.status.success());
    let mut plan = read(&f.root.join("plan.json"));
    plan["steps"][0]["command"]=json!([python_executable(),"-c","import pathlib,os,signal,time; pathlib.Path(os.environ['STATE']).write_text('partial'); os.kill(os.getppid(),signal.SIGTERM); time.sleep(20)"]);
    write(&f.root.join("plan.json"), &plan);
    let failed = f.cmd(&[
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
        "run.json",
        "--out",
        "out.json",
        "--json",
    ]);
    assert!(!failed.status.success());
    let run: Value = serde_json::from_slice(&failed.stdout).unwrap();
    assert!(run.get("recoveryStartedAt").is_none());
    assert_eq!(fs::read_to_string(f.root.join("state")).unwrap(), "partial");
    assert_eq!(run["steps"][0]["quiesced"], true);
    let recovered = f.cmd(&[
        "land",
        "recover",
        "--plan",
        "plan.json",
        "--run",
        "run.json",
        "--from-artifact",
        f.bundle.to_str().unwrap(),
        "--repo-roots",
        "roots.json",
        "--out",
        "out.json",
        "--apply",
        "--json",
    ]);
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(
        fs::read_to_string(f.root.join("state")).unwrap(),
        "previous"
    );
}
#[test]
fn intermediate_source_merge_is_pinned_and_partial_receipt_survives_command_failure() {
    let root = unique_temp_dir();
    let (_remote, service, _other) = init_remote_repo(&root, "service");
    let base = git(&service, ["rev-parse", "HEAD"]).trim().to_owned();
    git(&service, ["branch", "staging"]);
    git(&service, ["push", "origin", "staging"]);
    git(&service, ["checkout", "-b", "feature"]);
    append_line(&service.join("app.txt"), "changed");
    git(&service, ["commit", "-am", "Synthetic feature"]);
    let head = git(&service, ["rev-parse", "HEAD"]).trim().to_owned();
    let mut bundle = serde_json::to_value(knit::model::ChangeGroup::new(
        "demo".into(),
        "Synthetic branch deployment".into(),
        "2026-01-01T00:00:00Z".into(),
    ))
    .unwrap();
    bundle["projectId"] = json!(root.file_name().unwrap().to_str().unwrap());
    bundle["repos"] = json!([{"id":"service","path":service,"remote":null,"baseBranch":"main","baseSha":base,"headSha":head,"featureBranch":"feature"}]);
    bundle["commitGroups"] = json!([{"id":"change","message":"Synthetic","createdAt":"2026-01-01T00:00:00Z","commits":[{"repoId":"service","sha":head}]}]);
    write(&root.join("bundle.json"), &bundle);
    let mut project = serde_json::to_value(knit::model::KnitProject::new(
        root.file_name().unwrap().to_str().unwrap().into(),
        "2026-01-01T00:00:00Z".into(),
    ))
    .unwrap();
    project["repos"] = json!([{"id":"service","path":service,"baseBranch":"main"}]);
    project["landing"] = json!({"steps":[{"id":"verify-merged","type":"run","role":"verify","repoId":"service","needs":["merge-service"],"effect":"read_only","command":[python_executable(),"-c","import os,json,subprocess; inputs=json.loads(os.environ['KNIT_LAND_INPUTS']); assert subprocess.check_output(['git','rev-parse','HEAD']).decode().strip()==inputs['merge-service']['revision']; raise SystemExit(3)"]}]});
    write(&root.join("project.json"), &project);
    write(&root.join("roots.json"), &json!({"service":service}));
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_knit"));
    cmd.current_dir(&root)
        .env_remove("KNIT_BUNDLE")
        .env_remove("KNIT_SESSION")
        .args([
            "land",
            "--target",
            "staging",
            "plan",
            "--from-artifact",
            "bundle.json",
            "--project-file",
            "project.json",
            "--out",
            "plan.json",
        ]);
    let generated = cmd.output().unwrap();
    assert!(
        generated.status.success(),
        "{}",
        String::from_utf8_lossy(&generated.stderr)
    );
    let result = Command::new(env!("CARGO_BIN_EXE_knit"))
        .current_dir(&root)
        .env_remove("KNIT_BUNDLE")
        .env_remove("KNIT_SESSION")
        .args([
            "land",
            "apply",
            "--plan",
            "plan.json",
            "--from-artifact",
            "bundle.json",
            "--project-file",
            "project.json",
            "--repo-roots",
            "roots.json",
            "--run-out",
            "run.json",
            "--out",
            "out.json",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(!result.status.success());
    let run: Value = serde_json::from_slice(&result.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&result.stderr)));
    assert_eq!(run["steps"][0]["status"], "succeeded");
    assert_eq!(run["steps"][1]["exitCode"], 3);
    let out = read(&root.join("out.json"));
    assert!(out["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n["type"] == "branch.landed"));
    assert_eq!(
        git(&service, ["branch", "--show-current"]).trim(),
        "feature"
    );
    assert_eq!(git(&service, ["rev-parse", "HEAD"]).trim(), head);
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn build_outputs_are_available_to_deployment_in_the_shared_pinned_checkout() {
    let f = Fixture::new();
    let generated = f.cmd(&["land", "plan", "--out", "plan.json"]);
    assert!(generated.status.success());
    let mut plan = read(&f.root.join("plan.json"));
    let build = json!({"id":"build","type":"run","role":"build","repoId":"service","effect":"read_only","command":[python_executable(),"-c","import pathlib; pathlib.Path('build.bin').write_text('artifact')"]});
    let mut deploy = plan["steps"][0].clone();
    deploy["needs"] = json!(["build"]);
    deploy["command"]=json!([python_executable(),"-c","import pathlib,os; assert pathlib.Path('build.bin').read_text()=='artifact'; assert pathlib.Path(os.environ['KNIT_CHECKOUT_SERVICE']).samefile(pathlib.Path.cwd()); pathlib.Path(os.environ['STATE']).write_text('released')"]);
    plan["steps"] = json!([build, deploy]);
    write(&f.root.join("plan.json"), &plan);
    let result = f.cmd(&[
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
        "run.json",
        "--out",
        "out.json",
        "--json",
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        fs::read_to_string(f.root.join("state")).unwrap(),
        "released"
    );
    assert!(!f.service.join("build.bin").exists());
}
#[test]
fn imported_run_with_unavailable_host_path_recovers_from_embedded_plan() {
    let f = Fixture::new();
    let generated = f.cmd(&["land", "plan", "--out", "plan.json"]);
    assert!(generated.status.success());
    let failed = f.cmd(&[
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
        "run.json",
        "--out",
        "out.json",
        "--json",
    ]);
    assert!(!failed.status.success());
    let mut run = read(&f.root.join("run.json"));
    run["planPath"] = json!("/unavailable-synthetic-host/plan.json");
    write(&f.root.join("run.json"), &run);
    let recovered = f.cmd(&["land", "recover", "--run", "run.json", "--apply", "--json"]);
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&recovered.stdout).unwrap()["serviceStatus"],
        "restored"
    );
    assert!(f.root.join(".knit/land-plans/revisions/demo").exists());
}

#[test]
fn unbundled_consumer_retains_build_revision_when_remote_base_moves() {
    let f = Fixture::new();
    let consumer = f.root.join("consumer");
    init_repo(&consumer, "consumer");
    let origin = f.root.join("consumer.git");
    git(
        &f.root,
        [
            "clone",
            "--bare",
            consumer.to_str().unwrap(),
            origin.to_str().unwrap(),
        ],
    );
    git(
        &consumer,
        ["remote", "add", "origin", origin.to_str().unwrap()],
    );
    let before = git(&consumer, ["rev-parse", "HEAD"]).trim().to_owned();
    let mut project = read(&f.project);
    project["repos"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":"consumer","path":consumer,"baseBranch":"main"}));
    project["landing"] = json!({"steps":[
        {"id":"build","type":"run","repoId":"service","sourceRepos":["consumer"],"effect":"read_only","env":{"CONSUMER_SOURCE":consumer},"command":[python_executable(),"-c","import pathlib,os,subprocess; (pathlib.Path(os.environ['KNIT_CHECKOUT_CONSUMER'])/'build.bin').write_text('artifact'); src=os.environ['CONSUMER_SOURCE']; subprocess.run(['git','-C',src,'commit','--allow-empty','-m','Advance synthetic base'],check=True); subprocess.run(['git','-C',src,'push','origin','main'],check=True)"]},
        {"id":"consume","type":"run","repoId":"service","sourceRepos":["consumer"],"effect":"read_only","needs":["build"],"command":[python_executable(),"-c","import pathlib,os; assert (pathlib.Path(os.environ['KNIT_CHECKOUT_CONSUMER'])/'build.bin').read_text()=='artifact'"]}
    ]});
    write(&f.project, &project);
    write(
        &f.root.join("roots.json"),
        &json!({"service":f.service,"consumer":consumer}),
    );
    let result = f.cmd(&["land", "plan", "--out", "plan.json"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let result = f.cmd(&[
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
        "run.json",
        "--out",
        "out.json",
        "--json",
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let run = read(&f.root.join("run.json"));
    assert_eq!(run["steps"][0]["sourceRevisions"]["consumer"], before);
    assert_eq!(run["steps"][1]["sourceRevisions"]["consumer"], before);
    assert_ne!(git(&consumer, ["rev-parse", "HEAD"]).trim(), before);
    assert!(!consumer.join("build.bin").exists());
}

#[test]
fn unchanged_source_dependency_uses_recorded_head_despite_dirty_advanced_checkout() {
    let f = Fixture::new();
    let dependency = f.root.join("dependency");
    init_repo(&dependency, "dependency");
    fs::write(dependency.join("version"), "recorded").unwrap();
    git(&dependency, ["add", "version"]);
    git(&dependency, ["commit", "-m", "Record synthetic source"]);
    let head = git(&dependency, ["rev-parse", "HEAD"]).trim().to_owned();
    let mut bundle = read(&f.bundle);
    bundle["repos"].as_array_mut().unwrap().push(json!({"id":"dependency","path":dependency,"baseBranch":"main","baseSha":head,"headSha":head}));
    write(&f.bundle, &bundle);
    let mut project = read(&f.project);
    project["repos"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":"dependency","path":dependency,"baseBranch":"main"}));
    project["landing"] = json!({"deployments":[{"id":"release","repoId":"service","sourceRepos":["dependency"],"command":[python_executable(),"-c","import pathlib,os; assert (pathlib.Path(os.environ['KNIT_CHECKOUT_DEPENDENCY'])/'version').read_text()=='recorded'"],"build":{"command":[python_executable(),"-c","import pathlib,os; assert (pathlib.Path(os.environ['KNIT_CHECKOUT_DEPENDENCY'])/'version').read_text()=='recorded'"]},"verify":{"command":[python_executable(),"-c","pass"]}}]});
    write(&f.project, &project);
    let result = f.cmd(&["land", "plan", "--out", "plan.json"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let plan = read(&f.root.join("plan.json"));
    assert_eq!(plan["steps"].as_array().unwrap().len(), 3);
    for step in plan["steps"].as_array().unwrap() {
        assert_eq!(step["sourceRepos"], json!(["dependency"]));
    }
    let typed: knit::model::KnitProject = serde_json::from_value(project.clone()).unwrap();
    assert_eq!(
        serde_json::to_value(typed).unwrap()["landing"]["deployments"][0]["sourceRepos"],
        json!(["dependency"])
    );
    fs::write(dependency.join("version"), "advanced").unwrap();
    git(
        &dependency,
        ["commit", "-am", "Advance synthetic dependency"],
    );
    fs::write(dependency.join("version"), "dirty").unwrap();
    let args = [
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
        "run.json",
        "--out",
        "out.json",
        "--json",
    ];
    let missing = f.cmd(&args);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("repo-root binding"));
    assert!(!f.root.join("run.json").exists());
    write(
        &f.root.join("roots.json"),
        &json!({"service":f.service,"dependency":dependency}),
    );
    let result = f.cmd(&args);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    for step in read(&f.root.join("run.json"))["steps"].as_array().unwrap() {
        assert_eq!(step["sourceRevisions"]["dependency"], head);
    }
    assert_eq!(
        fs::read_to_string(dependency.join("version")).unwrap(),
        "dirty"
    );
}
