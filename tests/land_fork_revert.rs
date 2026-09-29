mod common;
use common::*;
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
};
const SOURCE: &str = "https://github.com/contributor/service.git";
const TARGET: &str = "https://github.com/upstream/service.git";
fn write(path: &Path, value: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}
fn read(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}
struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
    checkout: PathBuf,
    fork: PathBuf,
    upstream: PathBuf,
    api: PathBuf,
    bin: PathBuf,
    before: String,
}
impl Fixture {
    fn new() -> Self {
        let root = unique_temp_dir();
        let (upstream, local, collaborator) = init_remote_repo(&root, "service");
        let before = git(&local, ["rev-parse", "HEAD"]).trim().to_owned();
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
        knit(&workspace, ["bundle", "compensation"]);
        knit(&workspace, ["bundle", "add", local.to_str().unwrap()]);
        let checkout = workspace.join(".knit/worktrees/compensation/service");
        fs::write(checkout.join("feature.txt"), "synthetic feature\n").unwrap();
        knit(&workspace, ["commit", "--all", "-m", "Synthetic feature"]);
        let head = git(&checkout, ["rev-parse", "HEAD"]).trim().to_owned();
        git(&local, ["remote", "set-url", "origin", SOURCE]);
        for (url, path) in [(SOURCE, &fork), (TARGET, &upstream)] {
            git(
                &local,
                ["config", &format!("url.{}.insteadOf", path.display()), url],
            );
        }
        git(
            &checkout,
            ["push", SOURCE, "HEAD:refs/heads/knit/compensation"],
        );
        git(
            &collaborator,
            ["fetch", fork.to_str().unwrap(), "knit/compensation"],
        );
        git(
            &collaborator,
            ["merge", "--no-ff", "--no-edit", "FETCH_HEAD"],
        );
        git(&collaborator, ["push", "origin", "main"]);
        let merged = git(&upstream, ["rev-parse", "main"]).trim().to_owned();
        let artifact = workspace.join(".knit/bundles/compensation.bundle.json");
        let mut bundle = read(&artifact);
        bundle["repos"][0]["remote"] = json!(SOURCE);
        bundle["repos"][0]["sourceRemote"] = json!(SOURCE);
        bundle["repos"][0]["targetRemote"] = json!(TARGET);
        bundle["publications"] = json!([{"repoId":"service","provider":"github","kind":"pull_request","number":7,"url":"https://github.com/upstream/service/pull/7","baseBranch":"main","headBranch":"knit/compensation","state":"MERGED","title":"Synthetic feature","createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:00Z"}]);
        write(&artifact, &bundle);
        write(
            &workspace.join("legacy.json"),
            &json!({"schemaVersion":"0.1","kind":"KnitLandRun","id":"failed-run","planId":"plan","bundleId":"compensation","provider":"github","planPath":"plan.json","status":"failed","createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:00Z","steps":[{"id":"merge-service","type":"merge_pr","status":"succeeded","repoId":"service","publicationUrl":"https://github.com/upstream/service/pull/7"}]}),
        );
        let api = root.join("api");
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        write(
            &api.join("settings.json"),
            &json!({"fork":fork,"upstream":upstream,"head":head,"merged":merged}),
        );
        write_fake_python_cli(&bin, "gh", FAKE);
        Self {
            root,
            workspace,
            checkout,
            fork,
            upstream,
            api,
            bin,
            before,
        }
    }
    fn run(&self, args: &[&str]) -> String {
        knit_with_fake_gh_env(
            &self.workspace,
            args,
            &self.bin,
            &self.api,
            &[("KNIT_GITHUB_API_TRANSPORT", "gh")],
        )
    }
    fn fail(&self, args: &[&str]) -> String {
        knit_fails_with_fake_gh_env(
            &self.workspace,
            args,
            &self.bin,
            &self.api,
            &[("KNIT_GITHUB_API_TRANSPORT", "gh")],
        )
    }
    fn calls(&self) -> String {
        fs::read_to_string(self.api.join("calls")).unwrap_or_default()
    }
    fn artifact(&self) -> PathBuf {
        self.workspace
            .join(".knit/bundles/compensation.bundle.json")
    }
}
const FAKE: &str = r#"#!/usr/bin/env python3
import json,os,pathlib,subprocess,sys,urllib.parse
root=pathlib.Path(os.environ['GH_FAKE_DIR']);settings=json.loads((root/'settings.json').read_text());args=sys.argv[1:]
if args[:2]==['pr','merge']:
 assert args[2]=='https://github.com/upstream/service/pull/7'
 assert args[args.index('--repo')+1]=='upstream/service'
 assert args[args.index('--match-head-commit')+1]==settings['head']
 (root/'open').unlink(missing_ok=True)
 with (root/'calls').open('a', newline='\n') as f:f.write('MERGE repos/upstream/service/pulls/7\n')
 sys.exit(0)
assert args[0]=='api',args
endpoint=next(a for a in args if a.startswith('repos/'));method=args[args.index('--method')+1] if '--method' in args else 'GET'
with (root/'calls').open('a', newline='\n') as f:f.write(method+' '+endpoint+'\n')
if (root/'require-credentials').exists():
 expected='synthetic-source' if endpoint.startswith('repos/contributor/') else 'synthetic-target'
 assert os.environ.get('GH_TOKEN')==expected,'wrong repository credential'
payload=json.loads(sys.stdin.read()) if '--input' in args else {}
def sha(repo,branch):return subprocess.check_output(['git','--git-dir',settings[repo],'rev-parse',branch],text=True).strip()
if endpoint=='repos/contributor/service':v={'id':2,'full_name':'contributor/service','fork':True,'parent':{'id':1}}
elif endpoint=='repos/upstream/service':v={'id':1,'full_name':'upstream/service'}
elif '/git/ref/heads/' in endpoint:
 repo,branch=endpoint.split('/git/ref/heads/');v={'object':{'sha':sha('fork' if 'contributor' in repo else 'upstream',urllib.parse.unquote(branch))}}
elif endpoint=='repos/upstream/service/pulls/7':
 v={'number':7,'html_url':'https://github.com/upstream/service/pull/7','state':'closed','merged':True,'merge_commit_sha':settings['merged'],'draft':False,'mergeable':True,'mergeable_state':'clean','head':{'ref':'knit/compensation','sha':settings['head'],'repo':{'full_name':'contributor/service'}},'base':{'ref':'main','repo':{'full_name':'upstream/service'}}}
 if (root/'open').exists():v['state']='open';v['merged']=False
 if (root/'wrong-head').exists():v['head']['repo']['full_name']='collision/service'
elif endpoint=='repos/contributor/service/pulls/7':
 v={'number':7,'html_url':'https://github.com/contributor/service/pull/7','state':'closed','merged':True,'head':{'ref':'collision','sha':'b'*40},'base':{'ref':'main'}}
elif endpoint=='repos/upstream/service/pulls/7/merge' and method=='PUT':
 (root/'open').unlink(missing_ok=True);v={'merged':True}
elif endpoint.endswith('/check-runs'):v={'check_runs':[]}
elif endpoint.endswith('/status'):v={'state':'success','statuses':[]}
elif endpoint=='repos/upstream/service/pulls' and method=='POST':
 owner,branch=payload['head'].split(':',1);assert owner=='contributor';assert payload['head_repo']=='service';assert not payload['draft']
 v={'number':8,'html_url':'https://github.com/upstream/service/pull/8','state':'open','head':{'ref':branch,'sha':sha('fork',branch),'repo':{'full_name':'contributor/service'}},'base':{'ref':payload['base'],'repo':{'full_name':'upstream/service'}}}
 (root/'created.json').write_text(json.dumps(v))
elif endpoint=='repos/upstream/service/pulls/8':v=json.loads((root/'created.json').read_text())
else:raise AssertionError((method,endpoint))
print(v['html_url'] if '--jq' in args else json.dumps(v))
"#;

#[test]
fn legacy_rollback_verifies_target_and_pushes_compensation_only_to_source() {
    let f = Fixture::new();
    let target_before = git(&f.upstream, ["rev-parse", "main"]);
    let feature_before = git(&f.checkout, ["rev-parse", "HEAD"]);
    f.run(&[
        "land",
        "--schema-version",
        "0.1",
        "rollback",
        "--run",
        "legacy.json",
    ]);
    assert!(!f.calls().contains("repos/contributor/service/pulls"));
    assert!(!f.api.join("created.json").exists());
    f.run(&[
        "land",
        "--schema-version",
        "0.1",
        "rollback",
        "--run",
        "legacy.json",
        "--apply",
    ]);
    let created = read(&f.api.join("created.json"));
    let branch = created["head"]["ref"].as_str().unwrap();
    assert_eq!(
        git(&f.fork, ["rev-parse", &format!("{branch}^{{tree}}")]),
        git(
            &f.upstream,
            ["rev-parse", &format!("{}^{{tree}}", f.before)]
        )
    );
    assert!(!git_success(
        &f.upstream,
        ["show-ref", "--verify", &format!("refs/heads/{branch}")]
    ));
    assert_eq!(git(&f.upstream, ["rev-parse", "main"]), target_before);
    assert_eq!(git(&f.checkout, ["rev-parse", "HEAD"]), feature_before);
    assert_eq!(read(&f.artifact())["publications"][0]["number"], 7);
    assert!(read(&f.workspace.join("legacy.json"))["rolledBackAt"].is_string());
    assert!(!f.calls().contains("repos/contributor/service/pulls"));
}

#[test]
fn rollback_rejects_same_number_source_url_and_wrong_head_before_mutation() {
    let f = Fixture::new();
    let mut run = read(&f.workspace.join("legacy.json"));
    run["steps"][0]["publicationUrl"] = json!("https://github.com/contributor/service/pull/7");
    write(&f.workspace.join("wrong.json"), &run);
    assert!(f
        .fail(&[
            "land",
            "--schema-version",
            "0.1",
            "rollback",
            "--run",
            "wrong.json",
            "--apply"
        ])
        .contains("contradicts targetRemote"));
    assert!(f.calls().is_empty());
    fs::write(f.api.join("wrong-head"), "").unwrap();
    assert!(f
        .fail(&[
            "land",
            "--schema-version",
            "0.1",
            "rollback",
            "--run",
            "legacy.json",
            "--apply"
        ])
        .contains("contradicts"));
    assert!(!f.calls().contains("POST "));
    assert!(!f.api.join("created.json").exists());
    assert_eq!(
        git(
            &f.fork,
            [
                "for-each-ref",
                "--format=%(refname)",
                "refs/heads/knit/revert-"
            ]
        ),
        ""
    );
}

#[test]
fn rollback_uses_independent_target_and_source_credentials() {
    let f = Fixture::new();
    knit(&f.workspace, ["init", "credentials"]);
    let project_path = f.workspace.join(".knit/projects/credentials.project.json");
    let mut project = read(&project_path);
    project["repos"] = json!([{"id":"source","path":f.checkout,"remote":SOURCE,"baseBranch":"main"},{"id":"target","path":f.checkout,"remote":TARGET,"baseBranch":"main"}]);
    write(&project_path, &project);
    let mut bundle = read(&f.artifact());
    bundle["projectId"] = json!("credentials");
    write(&f.artifact(), &bundle);
    let home = f.root.join("credentials");
    let key = fs::canonicalize(&project_path)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    write(
        &home.join("forge-auth.json"),
        &json!({"credentials":{"source":{"provider":"github","host":"github.com","tokenEnv":"TEST_SOURCE_TOKEN"},"target":{"provider":"github","host":"github.com","tokenEnv":"TEST_TARGET_TOKEN"}},"scopedCredentials":["source","target"],"projects":{key:{"source":"source","target":"target"}}}),
    );
    fs::write(f.api.join("require-credentials"), "").unwrap();
    knit_with_fake_gh_env(
        &f.workspace,
        [
            "land",
            "--schema-version",
            "0.1",
            "rollback",
            "--run",
            "legacy.json",
            "--apply",
        ],
        &f.bin,
        &f.api,
        &[
            ("KNIT_HOME", home.to_str().unwrap()),
            ("KNIT_GITHUB_API_TRANSPORT", "gh"),
            ("TEST_SOURCE_TOKEN", "synthetic-source"),
            ("TEST_TARGET_TOKEN", "synthetic-target"),
        ],
    );
    assert!(f.api.join("created.json").exists());
    assert!(!f.calls().contains("repos/contributor/service/pulls"));
}

#[test]
fn native_saved_plan_recovery_uses_same_verified_fork_compensation() {
    let f = Fixture::new();
    fs::write(f.api.join("open"), "").unwrap();
    let mut bundle = read(&f.artifact());
    bundle["publications"][0]["state"] = json!("OPEN");
    write(&f.artifact(), &bundle);
    let mut project = serde_json::to_value(knit::model::KnitProject::new(
        f.root.file_name().unwrap().to_str().unwrap().into(),
        "2026-01-01T00:00:00Z".into(),
    ))
    .unwrap();
    project["repos"] =
        json!([{"id":"service","path":f.checkout,"remote":TARGET,"baseBranch":"main"}]);
    project["landing"] = json!({"onFailure":"stop","steps":[{"id":"fail","type":"run","repoId":"service","effect":"read_only","command":[python_executable(),"-c","raise SystemExit(19)"]}]});
    write(&f.workspace.join("project.json"), &project);
    write(
        &f.workspace.join("roots.json"),
        &json!({"service":f.checkout}),
    );
    f.run(&[
        "land",
        "plan",
        "--from-artifact",
        f.artifact().to_str().unwrap(),
        "--project-file",
        "project.json",
        "--out",
        "native-plan.json",
    ]);
    let mut plan = read(&f.workspace.join("native-plan.json"));
    let merge_id = plan["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["type"] == "merge_pr")
        .unwrap()["id"]
        .clone();
    for step in plan["steps"].as_array_mut().unwrap() {
        if step["id"] == "fail" {
            step["needs"] = json!([merge_id]);
        }
    }
    write(&f.workspace.join("native-plan.json"), &plan);
    let failure = f.fail(&[
        "land",
        "apply",
        "--plan",
        "native-plan.json",
        "--from-artifact",
        f.artifact().to_str().unwrap(),
        "--project-file",
        "project.json",
        "--repo-roots",
        "roots.json",
        "--run-out",
        "native-run.json",
        "--out",
        "native-bundle.json",
    ]);
    assert!(f.workspace.join("native-run.json").exists(), "{failure}");
    let run = read(&f.workspace.join("native-run.json"));
    assert!(run["steps"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["type"] == "merge_pr" && s["status"] == "succeeded"));
    f.run(&[
        "land",
        "recover",
        "--run",
        "native-run.json",
        "--repo-roots",
        "roots.json",
        "--apply",
    ]);
    assert_eq!(
        read(&f.workspace.join("native-run.json"))["sourceStatus"],
        "revert_proposed"
    );
    let created = read(&f.api.join("created.json"));
    let branch = created["head"]["ref"].as_str().unwrap();
    assert_eq!(
        git(&f.fork, ["rev-parse", &format!("{branch}^{{tree}}")]),
        git(
            &f.upstream,
            ["rev-parse", &format!("{}^{{tree}}", f.before)]
        )
    );
    assert!(!f.calls().contains("repos/contributor/service/pulls"));
}

#[test]
fn squash_compensation_reverts_the_complete_reviewed_change() {
    let f = Fixture::new();
    let collaborator = f.root.join("service-collaborator");
    git(&collaborator, ["reset", "--hard", &f.before]);
    git(&collaborator, ["merge", "--squash", "FETCH_HEAD"]);
    git(&collaborator, ["commit", "-m", "Squashed feature"]);
    git(&collaborator, ["push", "--force", "origin", "main"]);
    let mut settings = read(&f.api.join("settings.json"));
    settings["merged"] = json!(git(&f.upstream, ["rev-parse", "main"]).trim());
    write(&f.api.join("settings.json"), &settings);
    f.run(&[
        "land",
        "--schema-version",
        "0.1",
        "rollback",
        "--run",
        "legacy.json",
        "--apply",
    ]);
    let created = read(&f.api.join("created.json"));
    let branch = created["head"]["ref"].as_str().unwrap();
    assert_eq!(
        git(&f.fork, ["rev-parse", &format!("{branch}^{{tree}}")]),
        git(
            &f.upstream,
            ["rev-parse", &format!("{}^{{tree}}", f.before)]
        )
    );
}

#[test]
fn rebase_merge_cannot_silently_revert_only_the_last_reviewed_commit() {
    let f = Fixture::new();
    let collaborator = f.root.join("service-collaborator");
    let first = git(&f.checkout, ["rev-parse", "HEAD"]);
    fs::write(f.checkout.join("second.txt"), "second change\n").unwrap();
    git(&f.checkout, ["add", "second.txt"]);
    git(&f.checkout, ["commit", "-m", "Second reviewed change"]);
    let head = git(&f.checkout, ["rev-parse", "HEAD"]).trim().to_owned();
    git(
        &f.checkout,
        ["push", SOURCE, "HEAD:refs/heads/knit/compensation"],
    );
    git(
        &collaborator,
        ["fetch", f.fork.to_str().unwrap(), "knit/compensation"],
    );
    git(&collaborator, ["reset", "--hard", &f.before]);
    git(&collaborator, ["cherry-pick", first.trim(), &head]);
    git(&collaborator, ["push", "--force", "origin", "main"]);
    let mut bundle = read(&f.artifact());
    bundle["repos"][0]["headSha"] = json!(head);
    write(&f.artifact(), &bundle);
    let mut settings = read(&f.api.join("settings.json"));
    settings["head"] = json!(head);
    settings["merged"] = json!(git(&f.upstream, ["rev-parse", "main"]).trim());
    write(&f.api.join("settings.json"), &settings);
    let error = f.fail(&[
        "land",
        "--schema-version",
        "0.1",
        "rollback",
        "--run",
        "legacy.json",
        "--apply",
    ]);
    assert!(error.contains("complete reviewed change"), "{error}");
    assert!(!f.api.join("created.json").exists());
    assert!(!f.calls().contains("POST "));
    assert!(git(
        &f.fork,
        [
            "for-each-ref",
            "--format=%(refname)",
            "refs/heads/knit/revert-"
        ]
    )
    .trim()
    .is_empty());
}
