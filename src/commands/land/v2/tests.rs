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

use super::{generate, graph::*, runtime};
use serde_json::{json, Value};
use std::{fs, path::PathBuf};
struct Fixture {
    dir: PathBuf,
    bundle: Value,
    plan: Value,
}
impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(runtime::unique_id("landing-fixture"));
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(dir.join(".knit")).unwrap();
        runtime::durable(
            &dir.join(".knit/config.json"),
            &json!({"schemaVersion":"0.1","kind":"KnitConfig","activeBundle":"synthetic"}),
        )
        .unwrap();
        let mut bundle = serde_json::to_value(crate::model::ChangeGroup::new(
            "synthetic".into(),
            "Synthetic deployment".into(),
            crate::time::now_iso(),
        ))
        .unwrap();
        bundle["repos"] = json!([{"id":"service","path":"portable","baseBranch":"main","remote":null,"featureBranch":null,"worktreePath":null}]);
        let plan = json!({"schemaVersion":"0.2","kind":"KnitLandPlan","id":"synthetic-plan","bundleId":"synthetic","provider":"github","sourceProjectId":dir.file_name().unwrap().to_str().unwrap(),"bundleFingerprint":bundle_fingerprint(&bundle),"projectFingerprint":project_fingerprint(&Value::Null),"terminal":false,"onFailure":"recover","maxParallel":2,"steps":[]});
        Self { dir, bundle, plan }
    }
    fn files(&self) {
        for (name, v) in [
            ("bundle.json", &self.bundle),
            ("plan.json", &self.plan),
            ("roots.json", &json!({"service":self.dir})),
        ] {
            runtime::durable(&self.dir.join(name), v).unwrap();
        }
    }
    fn apply(&self, resume: bool) -> anyhow::Result<()> {
        self.files();
        runtime::apply(
            &self.dir.join("plan.json"),
            &self.dir.join("bundle.json"),
            None,
            Some(&self.dir.join("roots.json")),
            &self.dir.join("run.json"),
            &self.dir.join("out.json"),
            resume,
            false,
        )
    }
    fn run(&self) -> Value {
        crate::store::read_json(&self.dir.join("run.json")).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}
fn python(code: &str) -> Value {
    json!({"command":[python_executable(),"-c",code],"timeoutSeconds":10})
}
fn deploy(id: &str, code: &str) -> Value {
    let mut s = python(code);
    s["id"] = json!(id);
    s["type"] = json!("deploy");
    s["repoId"] = json!("service");
    s["effect"] = json!("deployment");
    let mut recovery=python("import os,json,pathlib; c=json.loads(os.environ['KNIT_LAND_CAPTURE']); pathlib.Path('state').write_text(c['version'])");
    recovery["mode"] = json!("command");
    recovery["idempotent"] = json!(true);
    recovery["capture"] = python(
        "import json,pathlib; print(json.dumps({'version':pathlib.Path('state').read_text()}))",
    );
    recovery["verify"]=python("import os,json,pathlib; assert pathlib.Path('state').read_text()==json.loads(os.environ['KNIT_LAND_CAPTURE'])['version']");
    s["recovery"] = recovery;
    s
}
#[test]
fn partially_applied_failed_command_restores_real_state_and_blocks_resume() {
    let mut f = Fixture::new();
    fs::write(f.dir.join("state"), "previous").unwrap();
    f.plan["steps"] = json!([deploy(
        "deploy",
        "import pathlib; pathlib.Path('state').write_text('broken'); raise SystemExit(7)"
    )]);
    assert!(f.apply(false).is_err());
    assert_eq!(fs::read_to_string(f.dir.join("state")).unwrap(), "previous");
    let run = f.run();
    assert_eq!(run["status"], "failed");
    assert_eq!(run["serviceStatus"], "restored");
    assert_eq!(run["steps"][0]["attribution"], "uncertain");
    assert_eq!(run["steps"][0]["capture"]["version"], "previous");
    assert!(f.dir.join("out.json").exists());
    assert!(f
        .apply(true)
        .unwrap_err()
        .to_string()
        .contains("recovery has started"));
}
#[test]
fn recovery_retry_preserves_capture_and_skips_finished_restorations() {
    let mut f = Fixture::new();
    fs::write(f.dir.join("state"), "old").unwrap();
    fs::write(f.dir.join("block"), "yes").unwrap();
    let mut a = deploy(
        "release",
        "import pathlib; pathlib.Path('state').write_text('bad'); raise SystemExit(2)",
    );
    a["recovery"]["command"]=json!([python_executable(),"-c","import pathlib,os,json; assert not pathlib.Path('block').exists(); pathlib.Path('state').write_text(json.loads(os.environ['KNIT_LAND_CAPTURE'])['version'])"]);
    f.plan["steps"] = json!([a]);
    assert!(f.apply(false).is_err());
    assert_eq!(f.run()["recoveryStatus"], "failed");
    fs::remove_file(f.dir.join("block")).unwrap();
    runtime::recover(
        Some(&f.dir.join("plan.json")),
        &f.dir.join("run.json"),
        Some(&f.dir.join("bundle.json")),
        Some(&f.dir.join("roots.json")),
        None,
        Some(&f.dir.join("out.json")),
        true,
        false,
    )
    .unwrap();
    let count = f.run()["steps"][0]["attempts"].as_array().unwrap().len();
    runtime::recover(
        Some(&f.dir.join("plan.json")),
        &f.dir.join("run.json"),
        Some(&f.dir.join("bundle.json")),
        Some(&f.dir.join("roots.json")),
        None,
        Some(&f.dir.join("out.json")),
        true,
        false,
    )
    .unwrap();
    assert_eq!(
        f.run()["steps"][0]["attempts"].as_array().unwrap().len(),
        count
    );
    assert_eq!(fs::read_to_string(f.dir.join("state")).unwrap(), "old");
}
#[test]
fn workflow_is_authoritative_and_retains_required_outputs() {
    let mut f = Fixture::new();
    let a =
        json!({"id":"a","type":"run","repoId":"service","effect":"read_only","command":["true"]});
    let mut b = a.clone();
    b["id"] = json!("b");
    b["requires"] = json!(["a"]);
    f.plan["steps"] = json!([a, b]);
    f.plan["workflow"] = json!({"sequence":[{"step":"a"},{"step":"b"}]});
    assert_eq!(compile(&f.plan).unwrap().1, vec![vec!["a"], vec!["b"]]);
    f.plan["workflow"] = json!({"sequence":[{"step":"b"},{"step":"a"}]});
    assert!(compile(&f.plan).is_err());
    f.plan["workflow"] = json!({"parallel":[{"step":"a"},{"step":"a"}]});
    assert!(compile(&f.plan).is_err());
}
#[test]
fn portable_fingerprints_ignore_localization_but_detect_recipe_and_head_changes() {
    let f = Fixture::new();
    let mut localized = f.bundle.clone();
    localized["repos"][0]["path"] = json!("/different/runner");
    localized["repos"][0]["worktreePath"] = json!("elsewhere");
    localized["updatedAt"] = json!("later");
    assert_eq!(
        bundle_fingerprint(&f.bundle),
        bundle_fingerprint(&localized)
    );
    localized["repos"][0]["headSha"] = json!("changed");
    assert_ne!(
        bundle_fingerprint(&f.bundle),
        bundle_fingerprint(&localized)
    );
    let p = json!({"id":"synthetic","landing":{"deployments":[]},"repos":[]});
    let mut changed = p.clone();
    changed["landing"]["steps"] = json!([]);
    assert_ne!(project_fingerprint(&p), project_fingerprint(&changed));
}
#[test]
fn immutable_resume_checks_commands_even_with_same_step_ids() {
    let mut f = Fixture::new();
    let mut s = python("pass");
    s["id"] = json!("build");
    s["repoId"] = json!("service");
    s["type"] = json!("run");
    s["effect"] = json!("read_only");
    f.plan["steps"] = json!([s]);
    f.apply(false).unwrap();
    f.plan["steps"][0]["command"] = json!(["false"]);
    assert!(f
        .apply(true)
        .unwrap_err()
        .to_string()
        .contains("immutable plan"));
}
#[test]
fn missing_restore_tool_refuses_before_deployment() {
    let mut f = Fixture::new();
    fs::write(f.dir.join("state"), "old").unwrap();
    let mut s = deploy(
        "release",
        "import pathlib; pathlib.Path('state').write_text('new')",
    );
    s["recovery"]["command"] = json!(["knit-missing-synthetic-tool"]);
    f.plan["steps"] = json!([s]);
    assert!(f.apply(false).is_err());
    assert_eq!(fs::read_to_string(f.dir.join("state")).unwrap(), "old");
}
#[test]
fn project_recipe_roundtrip_preserves_extensions() {
    let raw = json!({"deployments":[{"id":"release","repoId":"service","command":["true"],"build":{"command":["true"]},"verify":{"command":["true"]},"recovery":{"mode":"manual","reason":"External approval"},"customMetadata":{"key":"value"}}],"steps":[{"id":"extra","command":["true"]}],"maxParallel":2,"onFailure":"recover"});
    let typed: crate::model::ProjectLandingPlan = serde_json::from_value(raw.clone()).unwrap();
    let after = serde_json::to_value(typed).unwrap();
    for key in ["steps", "maxParallel", "onFailure"] {
        assert_eq!(raw[key], after[key]);
    }
    for key in ["build", "verify", "recovery", "customMetadata"] {
        assert_eq!(raw["deployments"][0][key], after["deployments"][0][key]);
    }
}
#[test]
fn independent_steps_overlap_but_downstream_waits_for_the_wave() {
    let mut f = Fixture::new();
    fs::create_dir_all(f.dir.join("other")).unwrap();
    let mut other = f.bundle["repos"][0].clone();
    other["id"] = json!("other");
    f.bundle["repos"].as_array_mut().unwrap().push(other);
    f.plan["bundleFingerprint"] = json!(bundle_fingerprint(&f.bundle));
    let mut a=python("import pathlib,time; pathlib.Path('started').write_text('yes'); deadline=time.monotonic()+8;
while not pathlib.Path('other/started').exists() and time.monotonic()<deadline: time.sleep(.01)
assert pathlib.Path('other/started').exists(); pathlib.Path('done').write_text('yes')");
    a["id"] = json!("a");
    a["type"] = json!("run");
    a["repoId"] = json!("service");
    a["effect"] = json!("read_only");
    let mut b=python("import pathlib,time; pathlib.Path('started').write_text('yes'); deadline=time.monotonic()+8;
while not pathlib.Path('../started').exists() and time.monotonic()<deadline: time.sleep(.01)
assert pathlib.Path('../started').exists(); pathlib.Path('done').write_text('yes')");
    b["id"] = json!("b");
    b["type"] = json!("run");
    b["repoId"] = json!("other");
    b["effect"] = json!("read_only");
    let mut c=python("import pathlib; assert pathlib.Path('done').exists() and pathlib.Path('other/done').exists()");
    c["id"] = json!("c");
    c["type"] = json!("run");
    c["repoId"] = json!("service");
    c["effect"] = json!("read_only");
    c["needs"] = json!(["a", "b"]);
    f.plan["steps"] = json!([a, b, c]);
    f.files();
    runtime::durable(
        &f.dir.join("roots.json"),
        &json!({"service":f.dir,"other":f.dir.join("other")}),
    )
    .unwrap();
    runtime::apply(
        &f.dir.join("plan.json"),
        &f.dir.join("bundle.json"),
        None,
        Some(&f.dir.join("roots.json")),
        &f.dir.join("run.json"),
        &f.dir.join("out.json"),
        false,
        false,
    )
    .unwrap();
}
#[test]
fn generator_includes_build_verify_and_custom_steps() {
    let f = Fixture::new();
    let mut project = serde_json::to_value(crate::model::KnitProject::new(
        "synthetic-project".into(),
        crate::time::now_iso(),
    ))
    .unwrap();
    project["repos"] =
        json!([{"id":"service","path":"portable","remote":null,"baseBranch":"main"}]);
    project["landing"] = json!({"deployments":[{"id":"release","repoId":"service","whenChanged":["*"],"command":["true"],"build":{"command":["true"]},"verify":{"command":["true"]}}],"steps":[{"id":"extra","repoId":"service","command":["true"],"effect":"read_only"}]});
    project["landing"]["deployments"][0]["recovery"] =
        json!({"mode":"manual","reason":"main recovery"});
    project["landing"]["targets"] = json!({"staging":{"deployments":[{"id":"release","repoId":"service","command":["false"],"build":{"command":["false"]},"verify":{"command":["false"]},"recovery":{"mode":"manual","reason":"staging recovery"}}]}});
    let active = crate::store::ActiveBundle::unlocked(
        f.dir.clone(),
        f.dir.join("bundle.json"),
        serde_json::from_value(f.bundle.clone()).unwrap(),
    );
    let plan = generate::build(&active, &f.bundle, &project, None, None, None).unwrap();
    assert_eq!(plan["steps"][0]["command"], json!(["true"]));
    assert_eq!(plan["steps"][1]["recovery"]["reason"], "main recovery");
    assert_eq!(plan["steps"][2]["command"], json!(["true"]));
    let ids: Vec<_> = plan["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec!["release-build", "release", "release-verify", "extra"]
    );
}
#[test]
fn reverse_recovery_restores_original_not_intermediate_state() {
    let mut f = Fixture::new();
    fs::write(f.dir.join("state"), "original").unwrap();
    let a = deploy(
        "first",
        "import pathlib; pathlib.Path('state').write_text('intermediate')",
    );
    let mut b = deploy(
        "second",
        "import pathlib; pathlib.Path('state').write_text('partial'); raise SystemExit(4)",
    );
    b["needs"] = json!(["first"]);
    f.plan["steps"] = json!([a, b]);
    assert!(f.apply(false).is_err());
    let run = f.run();
    assert_eq!(run["steps"][0]["capture"]["version"], "original");
    assert_eq!(run["steps"][1]["capture"]["version"], "intermediate");
    assert_eq!(fs::read_to_string(f.dir.join("state")).unwrap(), "original");
    assert_eq!(run["serviceStatus"], "restored");
}
#[test]
fn crashed_effect_requires_probe_and_reconciles_without_redeploy() {
    let mut f = Fixture::new();
    fs::write(f.dir.join("state"), "old").unwrap();
    let mut s=deploy("release","import pathlib; p=pathlib.Path('count'); p.write_text(str(int(p.read_text())+1) if p.exists() else '1'); pathlib.Path('state').write_text('new')");
    s["recovery"]["probe"] =
        python("import json; print(json.dumps({'status':'applied','quiesced':True}))");
    f.plan["onFailure"] = json!("stop");
    f.plan["steps"] = json!([s]);
    f.apply(false).unwrap();
    let mut run = f.run();
    run["status"] = json!("running");
    run["steps"][0]["status"] = json!("running");
    run["steps"][0]["attribution"] = json!("uncertain");
    run["steps"][0]["quiesced"] = json!(false);
    runtime::durable(&f.dir.join("run.json"), &run).unwrap();
    f.apply(true).unwrap();
    assert_eq!(fs::read_to_string(f.dir.join("count")).unwrap(), "1");
    assert_eq!(f.run()["steps"][0]["attribution"], "performed");
}
#[test]
fn completed_steps_resume_finalization_without_deploying_twice() {
    let mut f = Fixture::new();
    let mut s=python("import pathlib; p=pathlib.Path('count'); p.write_text(str(int(p.read_text())+1) if p.exists() else '1')");
    s["id"] = json!("build");
    s["type"] = json!("run");
    s["repoId"] = json!("service");
    s["effect"] = json!("read_only");
    f.plan["steps"] = json!([s]);
    f.apply(false).unwrap();
    let mut run = f.run();
    run["status"] = json!("running");
    run["finalization"] = json!({"ledger":"pending"});
    runtime::durable(&f.dir.join("run.json"), &run).unwrap();
    f.apply(true).unwrap();
    assert_eq!(fs::read_to_string(f.dir.join("count")).unwrap(), "1");
    assert_eq!(f.run()["finalization"]["ledger"], "succeeded");
}
#[test]
fn required_named_checks_refuse_before_any_effect() {
    let mut f = Fixture::new();
    fs::write(f.dir.join("state"), "old").unwrap();
    f.plan["requireChecks"] = json!(["release-gate"]);
    f.plan["steps"] = json!([deploy(
        "release",
        "import pathlib; pathlib.Path('state').write_text('new')"
    )]);
    let error = f.apply(false).unwrap_err();
    assert!(error.to_string().contains("Required checks"));
    assert_eq!(fs::read_to_string(f.dir.join("state")).unwrap(), "old");
    assert!(!f.dir.join("run.json").exists());
}
#[test]
fn failed_dependent_restore_blocks_prerequisite_inverse() {
    let mut f = Fixture::new();
    fs::write(f.dir.join("state"), "old").unwrap();
    let a = deploy(
        "database",
        "import pathlib; pathlib.Path('state').write_text('new-schema')",
    );
    let mut b = deploy("app", "raise SystemExit(9)");
    b["needs"] = json!(["database"]);
    b["recovery"]["command"] = json!([python_executable(), "-c", "raise SystemExit(8)"]);
    f.plan["steps"] = json!([a, b]);
    assert!(f.apply(false).is_err());
    assert_eq!(f.run()["steps"][0]["recovery"]["status"], "blocked");
    assert_eq!(
        fs::read_to_string(f.dir.join("state")).unwrap(),
        "new-schema"
    );
}
#[test]
fn superseded_recovery_refuses_before_restoring_old_state() {
    let mut f = Fixture::new();
    fs::write(f.dir.join("state"), "old").unwrap();
    f.plan["onFailure"] = json!("stop");
    f.plan["steps"] = json!([deploy(
        "release",
        "import pathlib; pathlib.Path('state').write_text('first')"
    )]);
    f.apply(false).unwrap();
    let mut next = f.plan.clone();
    next["steps"][0]["command"] = json!([
        python_executable(),
        "-c",
        "import pathlib; pathlib.Path('state').write_text('second')"
    ]);
    runtime::durable(&f.dir.join("next-plan.json"), &next).unwrap();
    runtime::apply(
        &f.dir.join("next-plan.json"),
        &f.dir.join("bundle.json"),
        None,
        Some(&f.dir.join("roots.json")),
        &f.dir.join("second-run.json"),
        &f.dir.join("second-out.json"),
        false,
        false,
    )
    .unwrap();
    let error = runtime::recover(
        Some(&f.dir.join("plan.json")),
        &f.dir.join("run.json"),
        Some(&f.dir.join("bundle.json")),
        Some(&f.dir.join("roots.json")),
        None,
        Some(&f.dir.join("out.json")),
        true,
        false,
    )
    .unwrap_err();
    assert!(error.to_string().contains("superseded"));
    assert_eq!(fs::read_to_string(f.dir.join("state")).unwrap(), "second");
}
#[test]
fn saved_plan_and_run_match_published_json_schemas() {
    let mut f = Fixture::new();
    fs::write(f.dir.join("state"), "old").unwrap();
    f.plan["steps"] = json!([deploy(
        "release",
        "import pathlib; pathlib.Path('state').write_text('new')"
    )]);
    f.apply(false).unwrap();
    for (text, value) in [
        (
            include_str!("../../../../schemas/land-plan.schema.json"),
            f.plan.clone(),
        ),
        (
            include_str!("../../../../schemas/land-run.schema.json"),
            f.run(),
        ),
    ] {
        let schema: Value = serde_json::from_str(text).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let errors: Vec<_> = validator
            .iter_errors(&value)
            .map(|e| e.to_string())
            .collect();
        assert!(errors.is_empty(), "{}", errors.join("\n"));
    }
}
#[test]
fn destination_aliases_share_project_ownership_even_with_distinct_checkouts() {
    let mut first = Fixture::new();
    let mut second = Fixture::new();
    let mut step =
        python("import pathlib,time; pathlib.Path('started').write_text('yes'); time.sleep(1)");
    step["id"] = json!("verify");
    step["repoId"] = json!("service");
    step["type"] = json!("run");
    step["effect"] = json!("read_only");
    first.plan["steps"] = json!([step.clone()]);
    second.plan["steps"] = json!([step]);
    second.plan["sourceProjectId"] = first.plan["sourceProjectId"].clone();
    second.plan["targetBranch"] = json!("main");
    std::thread::scope(|scope| {
        let handle = scope.spawn(|| first.apply(false));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !first.dir.join("started").exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let error = second.apply(false).unwrap_err();
        assert!(error.to_string().contains("resource is owned"));
        assert!(!second.dir.join("started").exists());
        handle.join().unwrap().unwrap();
    });
}
#[test]
fn uncertain_source_revert_never_becomes_a_retryable_failed_proposal() {
    let mut f = Fixture::new();
    f.plan["onFailure"] = json!("stop");
    f.plan["steps"] = json!([{"id":"merge-service","type":"merge_pr","repoId":"service","effect":"source","recovery":{"mode":"revert_pr"}}]);
    f.files();
    let run = json!({"schemaVersion":"0.2","kind":"KnitLandRun","id":runtime::unique_id("uncertain-revert"),"planId":f.plan["id"],"bundleId":f.bundle["id"],"provider":"github","planHash":canonical_hash(&f.plan),"plan":f.plan,"sourceBundle":f.bundle,"status":"failed","finalization":{},"steps":[{"id":"merge-service","type":"merge_pr","repoId":"service","status":"succeeded","attribution":"performed","quiesced":true,"recovery":{"status":"uncertain"}}]});
    runtime::durable(&f.dir.join("run.json"), &run).unwrap();
    for _ in 0..2 {
        assert!(runtime::recover(
            Some(&f.dir.join("plan.json")),
            &f.dir.join("run.json"),
            Some(&f.dir.join("bundle.json")),
            Some(&f.dir.join("roots.json")),
            None,
            Some(&f.dir.join("out.json")),
            true,
            false
        )
        .is_err());
        assert_eq!(f.run()["steps"][0]["recovery"]["status"], "uncertain");
        assert_eq!(f.run()["serviceStatus"], "unchanged");
    }
}

#[test]
fn resume_requires_quiescence_for_applied_and_absent_probe_results() {
    for status in ["applied", "absent"] {
        let mut f = Fixture::new();
        fs::write(f.dir.join("state"), "old").unwrap();
        let mut s = deploy(
            "release",
            "import pathlib; pathlib.Path('state').write_text('new')",
        );
        s["recovery"]["probe"] = python(&format!(
            "import json; print(json.dumps({{'status':'{status}','quiesced':False}}))"
        ));
        f.plan["onFailure"] = json!("stop");
        f.plan["steps"] = json!([s]);
        f.apply(false).unwrap();
        let mut run = f.run();
        run["steps"][0]["status"] = json!("running");
        run["steps"][0]["attribution"] = json!("uncertain");
        run["steps"][0]["quiesced"] = json!(false);
        runtime::durable(&f.dir.join("run.json"), &run).unwrap();
        assert!(f
            .apply(true)
            .unwrap_err()
            .to_string()
            .contains("quiescence"));
        assert_eq!(f.run()["steps"][0]["attribution"], "uncertain");
    }
}

#[test]
fn edited_original_plan_path_recovers_from_immutable_snapshot() {
    let mut f = Fixture::new();
    fs::write(f.dir.join("state"), "old").unwrap();
    f.plan["steps"] = json!([deploy(
        "release",
        "import pathlib; pathlib.Path('state').write_text('new')"
    )]);
    f.apply(false).unwrap();
    let run = f.run();
    let mut edited = f.plan.clone();
    edited["maxParallel"] = json!(7);
    runtime::durable(&f.dir.join("plan.json"), &edited).unwrap();
    let path = runtime::immutable_plan_path(&f.dir, &run).unwrap();
    assert_ne!(path, f.dir.join("plan.json"));
    assert_eq!(crate::store::read_json::<Value>(&path).unwrap(), f.plan);
}

#[test]
fn edited_bundle_heads_cannot_bypass_reviewed_source_fingerprint() {
    let mut f = Fixture::new();
    f.bundle["repos"][0]["headSha"] = json!("reviewed");
    f.plan["bundleFingerprint"] = json!(bundle_fingerprint(&f.bundle));
    f.plan["bundleHeads"] = json!({"service":"other"});
    let mut s = python("print('test')");
    s["id"] = json!("read");
    s["type"] = json!("run");
    s["repoId"] = json!("service");
    s["effect"] = json!("read_only");
    f.plan["steps"] = json!([s]);
    let v = validation(&f.plan, Some(&f.bundle), None);
    assert_eq!(v["valid"], false);
    assert!(v["errors"].to_string().contains("bundleHeads"));
}

#[test]
fn interrupted_restore_requires_reverse_attempt_quiescence() {
    let mut f = Fixture::new();
    fs::write(f.dir.join("state"), "old").unwrap();
    let mut s = deploy(
        "release",
        "import pathlib; pathlib.Path('state').write_text('new')",
    );
    s["recovery"]["probe"]=python("import json,os,pathlib; print(json.dumps({'phase':'restore','attemptId':os.environ.get('KNIT_LAND_RECONCILE_ATTEMPT_ID'),'quiesced':pathlib.Path('quiescent').exists()}))");
    f.plan["steps"] = json!([s]);
    f.apply(false).unwrap();
    let mut run = f.run();
    run["recoveryStartedAt"] = json!(crate::time::now_iso());
    run["steps"][0]["recovery"] = json!({"status":"running"});
    run["steps"][0]["attempts"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":"restore-in-flight","phase":"restore","status":"running"}));
    runtime::durable(&f.dir.join("run.json"), &run).unwrap();
    let recover = || {
        runtime::recover(
            Some(&f.dir.join("plan.json")),
            &f.dir.join("run.json"),
            Some(&f.dir.join("bundle.json")),
            Some(&f.dir.join("roots.json")),
            None,
            Some(&f.dir.join("out.json")),
            true,
            false,
        )
    };
    assert!(recover().is_err());
    assert_eq!(fs::read_to_string(f.dir.join("state")).unwrap(), "new");
    assert_eq!(f.run()["steps"][0]["recovery"]["status"], "uncertain");
    fs::write(f.dir.join("quiescent"), "yes").unwrap();
    recover().unwrap();
    assert_eq!(fs::read_to_string(f.dir.join("state")).unwrap(), "old");
}

#[test]
fn recovery_probe_records_forward_quiescence_before_restoration() {
    let mut f = Fixture::new();
    fs::write(f.dir.join("state"), "old").unwrap();
    let mut s = deploy(
        "release",
        "import pathlib; pathlib.Path('state').write_text('new')",
    );
    s["recovery"]["probe"] =
        python("import json; print(json.dumps({'status':'applied','quiesced':True}))");
    f.plan["steps"] = json!([s]);
    f.apply(false).unwrap();
    let mut run = f.run();
    run["steps"][0]["status"] = json!("running");
    run["steps"][0]["attribution"] = json!("uncertain");
    run["steps"][0]["quiesced"] = json!(false);
    runtime::durable(&f.dir.join("run.json"), &run).unwrap();
    runtime::recover(
        Some(&f.dir.join("plan.json")),
        &f.dir.join("run.json"),
        Some(&f.dir.join("bundle.json")),
        Some(&f.dir.join("roots.json")),
        None,
        Some(&f.dir.join("out.json")),
        true,
        false,
    )
    .unwrap();
    assert_eq!(f.run()["steps"][0]["quiesced"], true);
    assert_eq!(fs::read_to_string(f.dir.join("state")).unwrap(), "old");
}

#[test]
fn declared_sources_require_merge_ancestors_and_known_pins() {
    let mut f = Fixture::new();
    f.plan["steps"] = json!([
        {"id":"source","type":"merge_pr","repoId":"service"},
        {"id":"build","type":"run","repoId":"consumer","effect":"read_only","command":["true"],"sourceRepos":["service"]}
    ]);
    assert!(compile(&f.plan)
        .unwrap_err()
        .to_string()
        .contains("prerequisite merge"));
    f.plan["steps"][1]["needs"] = json!(["source"]);
    assert!(compile(&f.plan).is_ok());
    f.plan["steps"][1]["sourceRepos"] = json!(["missing"]);
    assert_eq!(validation(&f.plan, None, None)["valid"], false);
    f.plan["steps"][1]["sourceRepos"] = json!([7]);
    assert_eq!(validation(&f.plan, None, None)["valid"], false);
}

#[test]
fn executable_preflight_matches_native_path_search_and_explicit_paths() {
    let f = Fixture::new();
    let bin = f.dir.join("tools with spaces");
    fs::create_dir(&bin).unwrap();
    let name = format!("synthetic-tool{}", std::env::consts::EXE_SUFFIX);
    let program = bin.join(&name);
    fs::copy(std::env::current_exe().unwrap(), &program).unwrap();
    let step = json!({"command":["synthetic-tool"],"env":{"PATH":bin}});
    let resolved = runtime::command_path(&step, &step, &f.dir).unwrap();
    assert_eq!(resolved, program);
    // Exercise the real native Command resolver with the same child PATH.
    let native = std::process::Command::new("synthetic-tool")
        .arg("--list")
        .env("PATH", &bin)
        .current_dir(&f.dir)
        .output()
        .unwrap();
    assert!(native.status.success());
    for path in [
        format!("tools with spaces/{name}"),
        "tools with spaces/synthetic-tool".into(),
    ] {
        let spec = json!({"command":[path]});
        let resolved = runtime::command_path(&spec, &spec, &f.dir).unwrap();
        assert!(std::process::Command::new(resolved)
            .arg("--list")
            .output()
            .unwrap()
            .status
            .success());
    }
    let missing = json!({"command":["synthetic-no-such-tool"],"env":{"PATH":bin}});
    assert!(runtime::command_path(&missing, &missing, &f.dir).is_err());
    let overridden = json!({"command":["synthetic-tool"],"env":{"PATH":f.dir.join("absent")}});
    assert!(runtime::command_path(&step, &overridden, &f.dir).is_err());
}

#[cfg(windows)]
#[test]
fn windows_preflight_accepts_exe_and_explicit_cmd_without_pathext_inference() {
    let f = Fixture::new();
    let script = f.dir.join("synthetic-script.cmd");
    fs::write(&script, "@echo off\r\necho synthetic-ok\r\n").unwrap();
    let step =
        json!({"command":["synthetic-script.cmd"],"env":{"PATH":f.dir,"PATHEXT":".CMD;.EXE"}});
    let path = runtime::command_path(&step, &step, &f.dir).unwrap();
    let output = std::process::Command::new(path).output().unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("synthetic-ok"));
    let bare = json!({"command":["synthetic-script"],"env":step["env"]});
    assert!(runtime::command_path(&bare, &bare, &f.dir).is_err());
    assert!(std::process::Command::new("synthetic-script")
        .env("PATH", &f.dir)
        .env("PATHEXT", ".CMD;.EXE")
        .output()
        .is_err());
    let relative = json!({"command":[".\\synthetic-script.cmd"]});
    assert!(runtime::command_path(&relative, &relative, &f.dir).is_ok());
}

#[test]
fn durable_receipts_replace_existing_files_and_preserve_destination_on_failure() {
    let f = Fixture::new();
    let path = f.dir.join("receipt.json");
    for revision in 0..16 {
        let value = json!({"revision":revision,"payload":"synthetic"});
        runtime::durable(&path, &value).unwrap();
        assert_eq!(crate::store::read_json::<Value>(&path).unwrap(), value);
    }
    let destination = f.dir.join("directory.json");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("keep"), "retained").unwrap();
    assert!(runtime::durable(&destination, &json!({"replacement":true})).is_err());
    assert_eq!(
        fs::read_to_string(destination.join("keep")).unwrap(),
        "retained"
    );
    assert!(!fs::read_dir(&f.dir).unwrap().any(|entry| entry
        .unwrap()
        .path()
        .extension()
        .is_some_and(|s| s == "tmp")));
}

#[test]
fn manual_and_interactive_validate_but_artifact_execution_refuses_before_journal() {
    for mut step in [
        json!({"id":"ack","type":"manual","repoId":"service","instructions":"Inspect synthetic service","effect":"external","recovery":{"mode":"manual","reason":"Inspect and restore manually"}}),
        json!({"id":"ask","type":"run","repoId":"service","interactive":true,"command":["true"],"effect":"read_only","recovery":{"mode":"none"}}),
    ] {
        let mut f = Fixture::new();
        f.plan["onFailure"] = json!("stop");
        f.plan["steps"] = json!([step]);
        f.plan["requiredExecutorVersion"] = json!("0.3");
        assert_eq!(validation(&f.plan, Some(&f.bundle), None)["valid"], true);
        let error = f.apply(false).unwrap_err().to_string();
        assert!(error.contains("local execution"), "{error}");
        assert!(!f.dir.join("run.json").exists());
        assert!(!f.dir.join("out.json").exists());
        step["runner"] = json!("hosted");
        f.plan["steps"] = json!([step]);
        f.plan["requiredExecutorVersion"] = json!("0.3");
        assert_eq!(validation(&f.plan, None, None)["valid"], false);
    }
}

#[test]
fn scoped_recipes_replace_defaults_and_external_integration_needs_no_publications() {
    let f = Fixture::new();
    let active = crate::store::ActiveBundle::unlocked(
        f.dir.clone(),
        f.dir.join("bundle.json"),
        serde_json::from_value(f.bundle.clone()).unwrap(),
    );
    let mut project = serde_json::to_value(crate::model::KnitProject::new(
        "synthetic".into(),
        crate::time::now_iso(),
    ))
    .unwrap();
    project["repos"] = json!([{"id":"service","path":"portable","baseBranch":"main"}]);
    let default = json!({"id":"default-release","repoId":"service","command":["default-release"],"effect":"read_only"});
    let stage =
        json!({"id":"stage","repoId":"service","command":["stage-only"],"effect":"read_only"});
    project["landing"] = json!({"steps":[default],"deployments":[{"id":"default-deploy","repoId":"service","command":["default-deploy"],"whenChanged":["*"]}],"lanes":{"preview":{"terminal":false,"branches":{"service":"preview"},"merge":{"enabled":false},"steps":[stage]}},"targets":{"release":{"terminal":true,"merge":{"enabled":false},"steps":[stage]}}});
    for (target, lane) in [(None, Some("preview")), (Some("release"), None)] {
        let plan = generate::build(&active, &f.bundle, &project, None, target, lane).unwrap();
        assert_eq!(plan["merge"]["enabled"], false);
        assert_eq!(plan["steps"].as_array().unwrap().len(), 1);
        assert_eq!(plan["steps"][0]["command"], json!(["stage-only"]));
        assert_eq!(plan["terminal"], json!(target.is_some()));
    }
    project["landing"]["lanes"]["preview"]["steps"] = json!([]);
    assert!(generate::build(&active, &f.bundle, &project, None, None, Some("preview")).is_err());
}

#[test]
fn recorded_non_base_review_uses_target_commands_without_default_inheritance() {
    let mut f = Fixture::new();
    f.bundle["publications"] = json!([{"repoId":"service","provider":"github","kind":"pull_request","number":1,"url":"https://example.test/service/pull/1","baseBranch":"preview","headBranch":"feature","state":"OPEN","updatedAt":crate::time::now_iso()}]);
    f.bundle["commitGroups"] = json!([{"id":"change","message":"Synthetic change","createdAt":crate::time::now_iso(),"commits":[{"repoId":"service","sha":"synthetic"}]}]);
    let active = crate::store::ActiveBundle::unlocked(
        f.dir.clone(),
        f.dir.join("bundle.json"),
        serde_json::from_value(f.bundle.clone()).unwrap(),
    );
    let mut project = serde_json::to_value(crate::model::KnitProject::new(
        "synthetic".into(),
        crate::time::now_iso(),
    ))
    .unwrap();
    project["repos"] = json!([{"id":"service","path":"portable","baseBranch":"main"}]);
    project["landing"] = json!({"steps":[{"id":"default-release","repoId":"service","command":["default-release"],"effect":"read_only"}],"targets":{"preview":{"steps":[{"id":"preview-check","repoId":"service","command":["preview-check"],"effect":"read_only"}]}}});
    let plan = generate::build(&active, &f.bundle, &project, None, None, None).unwrap();
    assert_eq!(plan["steps"].as_array().unwrap().len(), 2);
    assert_eq!(plan["steps"][1]["id"], "preview-check");
    project["landing"]["targets"]["preview"]["merge"] = json!({"enabled":false});
    assert!(
        generate::build(&active, &f.bundle, &project, None, None, None)
            .unwrap_err()
            .to_string()
            .contains("select it explicitly")
    );
    project["landing"]["targets"] = json!({});
    let plan = generate::build(&active, &f.bundle, &project, None, None, None).unwrap();
    assert_eq!(plan["steps"].as_array().unwrap().len(), 1);
    assert_eq!(plan["steps"][0]["type"], "merge_pr");
}
