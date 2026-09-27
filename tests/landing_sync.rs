mod common;
use common::*;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
fn hash(v: &Value) -> String {
    format!("{:x}", Sha256::digest(serde_json::to_vec(v).unwrap()))
}
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

static EXECUTION_LOCK: Mutex<()> = Mutex::new(());

struct Server {
    url: String,
    state: Arc<Mutex<Value>>,
    requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(json!({"plans":[],"runs":[],"landing":{}})));
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let (shared, done) = (state.clone(), stop.clone());
        let handle = thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                let Ok((mut stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(15)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    continue;
                }
                let request = line.clone();
                counter.fetch_add(1, Ordering::Relaxed);
                let mut length = 0;
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':') {
                        if k.eq_ignore_ascii_case("content-length") {
                            length = v.trim().parse::<usize>().unwrap();
                        }
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let mut state = shared.lock().unwrap();
                let mut code = 200;
                let mut response = None;
                let recipes = request.contains("/api/v1/projects/demo/landing-recipes ");
                let executing = state["execution"] == true;
                if executing && request.contains("/landing-ownership ") {
                    let payload: Value = serde_json::from_slice(&body).unwrap();
                    state["ownership"] = json!({"id":"lease", "token":"test-token"});
                    state["boundRun"] = payload["runIdentity"].clone();
                    response = Some(state["ownership"].clone());
                } else if executing && request.starts_with("DELETE ") {
                    assert!(
                        state["boundRun"].is_string(),
                        "release requires a bound receipt"
                    );
                    state["ownership"] = Value::Null;
                    state["releases"] = json!(state["releases"].as_u64().unwrap_or(0) + 1);
                    response = Some(json!({}));
                } else if executing
                    && (request.contains("/api/v1/projects/demo ")
                        || request.contains("/projects/demo/bundles "))
                {
                    response = Some(json!({"id":"demo", "slug":"demo"}));
                } else if executing && request.contains("/bundles/demo/artifacts ") {
                    response = Some(json!({"id":"artifact", "artifactHash":"synthetic"}));
                } else if executing && request.contains("/projects/demo/history-events ") {
                    response = Some(json!({"insertedCount":0, "skippedCount":0}));
                } else if recipes {
                    if request.starts_with("PUT ") {
                        let payload: Value = serde_json::from_slice(&body).unwrap();
                        if payload["expectedHash"] != hash(&state["landing"]) {
                            code = 409;
                        } else {
                            state["landing"] = payload["landing"].clone();
                        }
                    }
                } else if !request.contains("/api/v1/projects/demo/landing-artifacts ") {
                    code = 404;
                } else if request.starts_with("POST ") {
                    let payload: Value = serde_json::from_slice(&body).unwrap();
                    let mut imported = state.clone();
                    for p in payload["plans"].as_array().unwrap() {
                        if let Some(known) = imported["plans"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .find(|c| c["hash"] == p["hash"])
                        {
                            if known["revision"] != p["revision"]
                                || known["parentHash"] != p["parentHash"]
                            {
                                code = 409;
                                break;
                            }
                            continue;
                        }
                        let current =
                            imported["plans"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .rev()
                                .find(|c| {
                                    c["bundleSlug"] == p["bundleSlug"]
                                        && c["plan"]["lane"] == p["plan"]["lane"]
                                        && c["plan"]["targetBranch"] == p["plan"]["targetBranch"]
                                });
                        if current.map(|c| &c["hash"]).unwrap_or(&Value::Null) != &p["parentHash"]
                            || p["revision"]
                                != current.map_or(1, |c| c["revision"].as_u64().unwrap() + 1)
                            || p["hash"] != hash(&p["plan"])
                            || p["bundleSlug"] != p["plan"]["bundleId"]
                        {
                            code = 409;
                            break;
                        }
                        imported["plans"].as_array_mut().unwrap().push(p.clone());
                    }
                    if code == 200 {
                        for r in payload["runs"].as_array().unwrap() {
                            if executing {
                                let owned = !payload["ownership"].is_null()
                                    && payload["ownership"] == imported["ownership"];
                                let old = imported["runs"]
                                    .as_array()
                                    .unwrap()
                                    .iter()
                                    .find(|old| old["run"]["id"] == r["run"]["id"]);
                                // Match the host: a fresh claim cannot adopt an
                                // already-imported unowned execution identity.
                                if (owned && imported["boundRun"].is_null() && old.is_some())
                                    || (!owned && old.is_some_and(|old| old != r))
                                {
                                    code = 409;
                                    break;
                                }
                                if owned && imported["failReceiptOnce"] == true {
                                    state["failReceiptOnce"] = json!(false);
                                    code = 503;
                                    break;
                                }
                                if owned {
                                    imported["boundRun"] = r["run"]["id"].clone();
                                }
                                imported["uploads"].as_array_mut().unwrap().push(json!({
                                    "owned":owned,"run":r["run"]
                                }));
                                imported["runs"]
                                    .as_array_mut()
                                    .unwrap()
                                    .retain(|old| old["run"]["id"] != r["run"]["id"]);
                            }
                            if !imported["plans"].as_array().unwrap().iter().any(|p| {
                                p["hash"] == r["run"]["planHash"]
                                    && p["bundleSlug"] == r["bundleSlug"]
                            }) {
                                code = 409;
                                break;
                            }
                            if !imported["runs"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .any(|old| old["run"]["id"] == r["run"]["id"])
                            {
                                imported["runs"].as_array_mut().unwrap().push(r.clone());
                            }
                        }
                    }
                    if code == 200 {
                        *state = imported;
                    }
                }
                let output = if code == 200 {
                    if let Some(response) = response {
                        json!({"data":response})
                    } else if recipes {
                        json!({"data":{"landing":state["landing"],"hash":hash(&state["landing"])}})
                    } else {
                        json!({"data":*state})
                    }
                } else {
                    json!({"error":{"message":"conflicting revision"}})
                }
                .to_string();
                write!(stream,"HTTP/1.1 {code} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{output}",output.len()).unwrap();
            }
        });
        Self {
            url,
            state,
            requests,
            stop,
            handle: Some(handle),
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.handle.take().unwrap().join();
    }
}
fn write(path: &Path, value: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}
fn setup(root: &Path, url: &str) {
    fs::create_dir_all(root).unwrap();
    knit(root, ["init", "demo"]);
    knit(root, ["remote", "add", "hosted", url]);
    knit(root, ["remote", "token", "hosted", "synthetic-token"]);
    write(
        &root.join(".knit/bundles/demo.bundle.json"),
        &json!({"id":"demo","projectId":"demo"}),
    );
}
#[test]
fn plans_roundtrip_and_cas_preserve_concurrent_local_edits_without_pushing_branches() {
    let server = Server::new();
    let dir = unique_temp_dir();
    let a = dir.join("a");
    let b = dir.join("b");
    setup(&a, &server.url);
    setup(&b, &server.url);
    let rel = ".knit/land-plans/demo.land.json";
    let mut plan = json!({"schemaVersion":"0.2","kind":"KnitLandPlan","id":"plan-demo","bundleId":"demo","steps":[{"id":"deploy","command":["original"]}]});
    write(&a.join(rel), &plan);
    knit(&a, ["sync", "push", "--plans", "--remote", "hosted"]);
    knit(&b, ["sync", "pull", "--plans", "--remote", "hosted"]);
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(b.join(rel)).unwrap()).unwrap(),
        plan
    );
    plan["steps"][0]["command"] = json!(["first-editor"]);
    write(&a.join(rel), &plan);
    knit(&a, ["sync", "push", "--plans", "--remote", "hosted"]);
    let mut other = plan.clone();
    other["steps"][0]["command"] = json!(["second-editor"]);
    write(&b.join(rel), &other);
    assert!(knit_fails(&b, ["sync", "push", "--plans", "--remote", "hosted"]).contains("409"));
    assert!(
        knit_fails(&b, ["sync", "pull", "--plans", "--remote", "hosted"])
            .contains("Local file preserved")
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(b.join(rel)).unwrap()).unwrap(),
        other
    );
    assert_eq!(
        server.state.lock().unwrap()["plans"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        fs::read_dir(b.join(".knit/land-plans/conflicts"))
            .unwrap()
            .count(),
        1
    );
    // No git repos exist in these workspaces: any accidental branch push would fail.
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn synchronized_execution_refuses_an_unavailable_ownership_authority_before_commands() {
    let _execution = EXECUTION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let server = Server::new();
    let dir = unique_temp_dir();
    let workspace = dir.join("workspace");
    setup(&workspace, &server.url);
    let service = workspace.join("service");
    init_repo(&service, "service");
    let head = git(&service, ["rev-parse", "HEAD"]);
    let mut bundle = serde_json::to_value(knit::model::ChangeGroup::new(
        "demo".into(),
        "Synthetic landing".into(),
        "2026-01-01T00:00:00Z".into(),
    ))
    .unwrap();
    bundle["projectId"] = json!("demo");
    bundle["repos"] = json!([{"id":"service","path":service,"baseBranch":"main","baseSha":head.trim(),"headSha":head.trim()}]);
    write(&workspace.join(".knit/bundles/demo.bundle.json"), &bundle);
    let project_path = workspace.join(".knit/projects/demo.project.json");
    let mut project: Value = serde_json::from_slice(&fs::read(&project_path).unwrap()).unwrap();
    project["repos"] = json!([{"id":"service","path":service,"baseBranch":"main"}]);
    project["landing"] = json!({"steps":[{"id":"verify","type":"run","repoId":"service","effect":"read_only","command":["git","rev-parse","HEAD"]}]});
    write(&project_path, &project);
    server.state.lock().unwrap()["landing"] = project["landing"].clone();
    let plan_path = workspace.join(".knit/land-plans/demo.land.json");
    knit(
        &workspace,
        [
            "land",
            "plan",
            "--from-artifact",
            ".knit/bundles/demo.bundle.json",
            "--project-file",
            ".knit/projects/demo.project.json",
            "--out",
            plan_path.to_str().unwrap(),
        ],
    );
    knit(
        &workspace,
        ["sync", "push", "--plans", "--remote", "hosted"],
    );
    write(&workspace.join("roots.json"), &json!({"service":service}));
    let result = knit_fails(
        &workspace,
        [
            "land",
            "apply",
            "--plan",
            plan_path.to_str().unwrap(),
            "--from-artifact",
            ".knit/bundles/demo.bundle.json",
            "--project-file",
            ".knit/projects/demo.project.json",
            "--repo-roots",
            "roots.json",
            "--run-out",
            "run.json",
            "--out",
            "out.json",
        ],
    );
    assert!(result.contains("404"), "{result}");
    assert!(
        !workspace.join("run.json").exists(),
        "Execution must not start without ownership"
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn recipe_sync_preserves_other_project_fields_and_conflicting_edits() {
    let server = Server::new();
    let dir = unique_temp_dir();
    let a = dir.join("a");
    let b = dir.join("b");
    setup(&a, &server.url);
    setup(&b, &server.url);
    for root in [&a, &b] {
        knit(root, ["sync", "pull", "--plans", "--remote", "hosted"]);
    }
    let rel = ".knit/projects/demo.project.json";
    let mut first: Value = serde_json::from_slice(&fs::read(a.join(rel)).unwrap()).unwrap();
    first["landing"] = json!({"maxParallel":2});
    write(&a.join(rel), &first);
    knit(&a, ["sync", "push", "--plans", "--remote", "hosted"]);
    let mut second: Value = serde_json::from_slice(&fs::read(b.join(rel)).unwrap()).unwrap();
    second["commands"] = json!({"check":{"command":["true"]}});
    write(&b.join(rel), &second);
    knit(&b, ["sync", "pull", "--plans", "--remote", "hosted"]);
    second = serde_json::from_slice(&fs::read(b.join(rel)).unwrap()).unwrap();
    assert_eq!(second["landing"], first["landing"]);
    assert_eq!(second["commands"]["check"]["command"], json!(["true"]));
    first["landing"]["maxParallel"] = json!(3);
    write(&a.join(rel), &first);
    knit(&a, ["sync", "push", "--plans", "--remote", "hosted"]);
    second["landing"]["maxParallel"] = json!(4);
    write(&b.join(rel), &second);
    assert!(knit_fails(&b, ["sync", "push", "--plans", "--remote", "hosted"]).contains("409"));
    assert!(
        knit_fails(&b, ["sync", "pull", "--plans", "--remote", "hosted"])
            .contains("Project preserved")
    );
    let retained: Value = serde_json::from_slice(&fs::read(b.join(rel)).unwrap()).unwrap();
    assert_eq!(retained, second);
    assert_eq!(
        fs::read_dir(b.join(".knit/landing-sync/conflicts"))
            .unwrap()
            .count(),
        1
    );
    fs::remove_dir_all(dir).unwrap();
}

fn offline_history(root: &Path, server: &Server) -> (Value, Value, Value, Value) {
    offline_history_recipes(root, server, false)
}
fn offline_history_recipes(
    root: &Path,
    server: &Server,
    change_recipe: bool,
) -> (Value, Value, Value, Value) {
    setup(root, &server.url);
    fs::create_dir_all(root.join("service")).unwrap();
    let mut bundle = serde_json::to_value(knit::model::ChangeGroup::new(
        "demo".into(),
        "Synthetic offline landing".into(),
        "2026-01-01T00:00:00Z".into(),
    ))
    .unwrap();
    bundle["projectId"] = json!("demo");
    bundle["repos"] = json!([{"id":"service","path":root.join("service"),"baseBranch":"main"}]);
    write(&root.join(".knit/bundles/demo.bundle.json"), &bundle);
    let project_file = root.join(".knit/projects/demo.project.json");
    let mut project: Value = serde_json::from_slice(&fs::read(&project_file).unwrap()).unwrap();
    project["repos"] = json!([{"id":"service","path":root.join("service"),"baseBranch":"main"}]);
    project["landing"] = json!({"steps":[{"id":"inspect","type":"run","repoId":"service","effect":"read_only","command":["git","--version"]}]});
    write(&project_file, &project);
    server.state.lock().unwrap()["landing"] = project["landing"].clone();
    let plan_file = ".knit/land-plans/demo.land.json";
    knit(
        root,
        [
            "land",
            "plan",
            "--from-artifact",
            ".knit/bundles/demo.bundle.json",
            "--project-file",
            ".knit/projects/demo.project.json",
            "--out",
            plan_file,
        ],
    );
    let mut a: Value = serde_json::from_slice(&fs::read(root.join(plan_file)).unwrap()).unwrap();
    a["createdAt"] = json!("2026-01-01T00:00:00Z");
    let project_a = project.clone();
    let mut b = a.clone();
    if change_recipe {
        project["landing"]["maxParallel"] = json!(2);
        write(&project_file, &project);
        knit(
            root,
            [
                "land",
                "plan",
                "--force",
                "--from-artifact",
                ".knit/bundles/demo.bundle.json",
                "--project-file",
                ".knit/projects/demo.project.json",
                "--out",
                plan_file,
            ],
        );
        b = serde_json::from_slice(&fs::read(root.join(plan_file)).unwrap()).unwrap();
        server.state.lock().unwrap()["landing"] = project["landing"].clone();
    }
    b["createdAt"] = json!("2026-01-02T00:00:00Z");
    b["maxParallel"] = json!(2);
    write(
        &root.join("roots.json"),
        &json!({"service":root.join("service")}),
    );
    for (plan, name, date) in [
        (&a, "z-first", "2026-01-01T01:00:00Z"),
        (&b, "a-second", "2026-01-02T01:00:00Z"),
    ] {
        write(
            &project_file,
            if name == "z-first" {
                &project_a
            } else {
                &project
            },
        );
        write(&root.join(plan_file), plan);
        let run_file = format!(".knit/land-runs/{name}.run.json");
        knit(
            root,
            [
                "land",
                "apply",
                "--plan",
                plan_file,
                "--from-artifact",
                ".knit/bundles/demo.bundle.json",
                "--project-file",
                ".knit/projects/demo.project.json",
                "--repo-roots",
                "roots.json",
                "--run-out",
                &run_file,
                "--out",
                "output.bundle.json",
            ],
        );
        let mut run: Value =
            serde_json::from_slice(&fs::read(root.join(&run_file)).unwrap()).unwrap();
        run["createdAt"] = json!(date);
        write(&root.join(run_file), &run);
    }
    (a, b, bundle, project)
}

#[test]
fn first_sync_includes_executed_revisions_snapshots_and_idempotent_history() {
    let _execution = EXECUTION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let server = Server::new();
    let root = unique_temp_dir();
    let (a, b, bundle, project) = offline_history(&root, &server);
    knit(&root, ["sync", "push", "--plans", "--remote", "hosted"]);
    let imported = server.state.lock().unwrap().clone();
    assert_eq!(imported["plans"].as_array().unwrap().len(), 2);
    for (index, plan) in [&a, &b].into_iter().enumerate() {
        let record = &imported["plans"][index];
        assert_eq!(record["plan"], *plan);
        assert_eq!(record["hash"], hash(plan));
        assert_eq!(record["revision"], index + 1);
        assert_eq!(record["bundleSlug"], "demo");
        assert_eq!(record["bundleSnapshot"], bundle);
        assert_eq!(record["projectSnapshot"], project);
    }
    assert!(imported["plans"][0]["parentHash"].is_null());
    assert_eq!(imported["plans"][1]["parentHash"], hash(&a));
    assert_eq!(imported["runs"].as_array().unwrap().len(), 2);
    let read_plan = || {
        serde_json::from_slice::<Value>(
            &fs::read(root.join(".knit/land-plans/demo.land.json")).unwrap(),
        )
        .unwrap()
    };
    assert_eq!(read_plan(), b);
    knit(&root, ["sync", "push", "--plans", "--remote", "hosted"]);
    assert_eq!(*server.state.lock().unwrap(), imported);
    // Lost local index after a successful remote commit must replay exact ancestry.
    for entry in fs::read_dir(root.join(".knit/landing-sync")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|x| x == "json") {
            fs::remove_file(path).unwrap();
        }
    }
    knit(&root, ["sync", "push", "--plans", "--remote", "hosted"]);
    assert_eq!(*server.state.lock().unwrap(), imported);
    assert_eq!(read_plan(), b);
    // Re-authoring a known older hash does not create a fictitious revision 3.
    write(&root.join(".knit/land-plans/demo.land.json"), &a);
    knit(&root, ["sync", "push", "--plans", "--remote", "hosted"]);
    assert_eq!(*server.state.lock().unwrap(), imported);
    assert_eq!(read_plan(), a);
    write(&root.join(".knit/land-plans/demo.land.json"), &b);
    // Pull keeps snapshot envelopes, not just plan JSON, and does not rewind B.
    knit(&root, ["sync", "pull", "--plans", "--remote", "hosted"]);
    let sidecar = root.join(format!(
        ".knit/land-plans/revisions/demo/{}.record.json",
        hash(&a)
    ));
    let retained: Value = serde_json::from_slice(&fs::read(sidecar).unwrap()).unwrap();
    assert_eq!(retained["bundleSnapshot"], bundle);
    assert_eq!(retained["projectSnapshot"], project);
    assert_eq!(read_plan(), b);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn tampered_embedded_plan_is_refused_before_import() {
    let _execution = EXECUTION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let server = Server::new();
    let root = unique_temp_dir();
    offline_history(&root, &server);
    let path = root.join(".knit/land-runs/z-first.run.json");
    let mut run: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let original = run.clone();
    run["plan"]["maxParallel"] = json!(999);
    write(&path, &run);
    assert!(
        knit_fails(&root, ["sync", "push", "--plans", "--remote", "hosted"])
            .contains("invalid embedded plan hash")
    );
    run = original.clone();
    run["plan"]["sourceProjectId"] = json!("different-project");
    run["planHash"] = json!(hash(&run["plan"]));
    write(&path, &run);
    assert!(
        knit_fails(&root, ["sync", "push", "--plans", "--remote", "hosted"])
            .contains("source scope")
    );
    run = original;
    run["sourceBundle"]["repos"][0]["headSha"] = json!("not-reviewed");
    write(&path, &run);
    assert!(
        knit_fails(&root, ["sync", "push", "--plans", "--remote", "hosted"])
            .contains("Invalid historical landing source")
    );
    assert!(server.state.lock().unwrap()["plans"]
        .as_array()
        .unwrap()
        .is_empty());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn unknown_history_cannot_be_inserted_before_an_existing_cursor() {
    let _execution = EXECUTION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let server = Server::new();
    let root = unique_temp_dir();
    let (_, b, _, _) = offline_history(&root, &server);
    let old = root.join(".knit/land-runs/z-first.run.json");
    let hidden = root.join("old-receipt.saved");
    fs::rename(&old, &hidden).unwrap();
    knit(&root, ["sync", "push", "--plans", "--remote", "hosted"]);
    let before = server.state.lock().unwrap().clone();
    assert_eq!(before["plans"].as_array().unwrap().len(), 1);
    assert_eq!(before["plans"][0]["hash"], hash(&b));
    fs::rename(hidden, &old).unwrap();
    assert!(
        knit_fails(&root, ["sync", "push", "--plans", "--remote", "hosted"])
            .contains("cannot fabricate immutable ancestry")
    );
    assert_eq!(*server.state.lock().unwrap(), before);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn unexecuted_authored_revision_follows_offline_run_history() {
    let _execution = EXECUTION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let server = Server::new();
    let root = unique_temp_dir();
    let (_, b, _, _) = offline_history(&root, &server);
    let mut current = b.clone();
    current["maxParallel"] = json!(3);
    write(&root.join(".knit/land-plans/demo.land.json"), &current);
    knit(&root, ["sync", "push", "--plans", "--remote", "hosted"]);
    let imported = server.state.lock().unwrap().clone();
    assert_eq!(imported["plans"].as_array().unwrap().len(), 3);
    assert_eq!(imported["plans"][2]["revision"], 3);
    assert_eq!(imported["plans"][2]["parentHash"], hash(&b));
    assert_eq!(imported["plans"][2]["plan"], current);
    assert_eq!(imported["runs"].as_array().unwrap().len(), 2);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn offline_recipe_revisions_keep_exact_projects_and_resume_preserves_provenance() {
    let _execution = EXECUTION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let server = Server::new();
    let root = unique_temp_dir();
    let (a, b, bundle, project_b) = offline_history_recipes(&root, &server, true);
    let read_run = |name: &str| {
        serde_json::from_slice::<Value>(
            &fs::read(root.join(format!(".knit/land-runs/{name}.run.json"))).unwrap(),
        )
        .unwrap()
    };
    let run_a = read_run("z-first");
    let run_b = read_run("a-second");
    assert_ne!(run_a["sourceProject"], run_b["sourceProject"]);
    assert_eq!(run_b["sourceProject"], project_b);
    // Finalization-only resume can receive newer metadata but must not rewrite provenance.
    let mut changed = project_b.clone();
    changed["landing"]["maxParallel"] = json!(7);
    write(&root.join(".knit/projects/demo.project.json"), &changed);
    knit(
        &root,
        [
            "land",
            "apply",
            "--resume",
            "--plan",
            ".knit/land-plans/demo.land.json",
            "--from-artifact",
            ".knit/bundles/demo.bundle.json",
            "--project-file",
            ".knit/projects/demo.project.json",
            "--repo-roots",
            "roots.json",
            "--run-out",
            ".knit/land-runs/a-second.run.json",
            "--out",
            "output.bundle.json",
        ],
    );
    assert_eq!(read_run("a-second")["sourceProject"], project_b);
    write(&root.join(".knit/projects/demo.project.json"), &project_b);
    knit(&root, ["sync", "push", "--plans", "--remote", "hosted"]);
    let imported = server.state.lock().unwrap().clone();
    assert_eq!(imported["plans"][0]["hash"], hash(&a));
    assert_eq!(imported["plans"][1]["hash"], hash(&b));
    assert_eq!(
        imported["plans"][0]["projectSnapshot"],
        run_a["sourceProject"]
    );
    assert_eq!(imported["plans"][1]["projectSnapshot"], project_b);
    assert_eq!(imported["plans"][0]["bundleSnapshot"], bundle);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn unavailable_or_invalid_historical_project_refuses_before_any_network_request() {
    let _execution = EXECUTION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let server = Server::new();
    let root = unique_temp_dir();
    let (a, _, bundle, project_b) = offline_history_recipes(&root, &server, true);
    let path = root.join(".knit/land-runs/z-first.run.json");
    let original: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for snapshot in [
        None,
        Some(Value::Null),
        Some(project_b),
        Some(json!({"id":"different"})),
    ] {
        let mut run = original.clone();
        if let Some(snapshot) = snapshot {
            run["sourceProject"] = snapshot;
        } else {
            run.as_object_mut().unwrap().remove("sourceProject");
        }
        write(&path, &run);
        let before = server.requests.load(Ordering::Relaxed);
        let error = knit_fails(&root, ["sync", "push", "--plans", "--remote", "hosted"]);
        assert!(error.contains("project snapshot"), "{error}");
        assert_eq!(server.requests.load(Ordering::Relaxed), before);
    }
    // A legacy run can use a retained exact project snapshot despite a newer current recipe.
    let mut legacy = original.clone();
    legacy.as_object_mut().unwrap().remove("sourceProject");
    write(&path, &legacy);
    write(
        &root.join(format!(
            ".knit/land-plans/revisions/demo/{}.record.json",
            hash(&a)
        )),
        &json!({"bundleSlug":"demo","revision":1,"hash":hash(&a),"parentHash":null,"plan":a,"bundleSnapshot":bundle,"projectSnapshot":original["sourceProject"]}),
    );
    knit(&root, ["sync", "push", "--plans", "--remote", "hosted"]);
    assert_eq!(
        server.state.lock().unwrap()["plans"][0]["projectSnapshot"],
        original["sourceProject"]
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn legacy_run_without_project_snapshot_uses_compatible_current_project() {
    let _execution = EXECUTION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let server = Server::new();
    let root = unique_temp_dir();
    let (_, _, _, project) = offline_history(&root, &server);
    for name in ["z-first", "a-second"] {
        let path = root.join(format!(".knit/land-runs/{name}.run.json"));
        let mut run: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        run.as_object_mut().unwrap().remove("sourceProject");
        write(&path, &run);
    }
    knit(&root, ["sync", "push", "--plans", "--remote", "hosted"]);
    for record in server.state.lock().unwrap()["plans"].as_array().unwrap() {
        assert_eq!(record["projectSnapshot"], project);
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn local_finalization_publishes_only_complete_receipts_and_retries_without_execution() {
    let _execution = EXECUTION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Cover an already-synchronized plan and the first push of an offline plan.
    for synchronized in [true, false] {
        let server = Server::new();
        let dir = unique_temp_dir();
        let workspace = dir.join("workspace");
        setup(&workspace, &server.url);
        let service = workspace.join("service");
        init_repo(&service, "service");
        let origin = dir.join("origin.git");
        git(&workspace, ["init", "--bare", origin.to_str().unwrap()]);
        git(
            &service,
            ["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git(&service, ["checkout", "-b", "knit/demo"]);
        git(&service, ["push", "-u", "origin", "knit/demo"]);
        let head = git(&service, ["rev-parse", "HEAD"]).trim().to_string();
        let mut bundle = serde_json::to_value(knit::model::ChangeGroup::new(
            "demo".into(),
            "Synthetic landing".into(),
            "2026-01-01T00:00:00Z".into(),
        ))
        .unwrap();
        bundle["projectId"] = json!("demo");
        bundle["repos"] = json!([{"id":"service","path":service,"worktreePath":service,
            "featureBranch":"knit/demo","baseBranch":"main","baseSha":head,"headSha":head}]);
        write(&workspace.join(".knit/bundles/demo.bundle.json"), &bundle);
        let config_path = workspace.join(".knit/config.json");
        let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
        config["activeBundle"] = json!("demo");
        config["pushSync"] = json!(true);
        config["syncRemotes"] = json!(["hosted"]);
        write(&config_path, &config);
        let project_path = workspace.join(".knit/projects/demo.project.json");
        let mut project: Value = serde_json::from_slice(&fs::read(&project_path).unwrap()).unwrap();
        project["repos"] = json!([{"id":"service","path":service,"baseBranch":"main"}]);
        project["landing"] = json!({"merge":{"enabled":false},"onFailure":"stop","steps":[{
            "id":"verify","type":"run","repoId":"service","effect":"read_only",
            "command":["git","rev-parse","HEAD"]
        }]});
        write(&project_path, &project);
        {
            let mut state = server.state.lock().unwrap();
            state["execution"] = json!(true);
            state["uploads"] = json!([]);
            state["landing"] = project["landing"].clone();
        }
        let plan_path = workspace.join(".knit/land-plans/demo.land.json");
        knit(
            &workspace,
            ["land", "plan", "--out", plan_path.to_str().unwrap()],
        );
        let mut plan: Value = serde_json::from_slice(&fs::read(&plan_path).unwrap()).unwrap();
        plan["terminal"] = json!(false);
        write(&plan_path, &plan);
        if synchronized {
            knit(
                &workspace,
                ["sync", "push", "--plans", "--remote", "hosted"],
            );
        }
        let plan_bytes = fs::read(&plan_path).unwrap();
        let apply = [
            "land",
            "apply",
            "--plan",
            plan_path.to_str().unwrap(),
            "--no-tag",
        ];
        knit(&workspace, apply);
        let runs = workspace.join(".knit/land-runs");
        let run_path = fs::read_dir(&runs)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .find(|p| p.to_string_lossy().ends_with(".run.json"))
            .unwrap();
        let first: Value = serde_json::from_slice(&fs::read(&run_path).unwrap()).unwrap();
        assert_eq!(first["status"], "succeeded");
        assert_eq!(first["finalized"], true);
        {
            let state = server.state.lock().unwrap();
            assert_eq!(state["uploads"].as_array().unwrap().len(), 1);
            assert_eq!(state["uploads"][0]["owned"], synchronized);
            assert_eq!(state["uploads"][0]["run"], first);
            assert_eq!(state["runs"][0]["run"], first);
        }
        knit(
            &workspace,
            ["sync", "push", "--plans", "--remote", "hosted"],
        );
        knit(
            &workspace,
            ["sync", "pull", "--plans", "--remote", "hosted"],
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&plan_path).unwrap()).unwrap(),
            serde_json::from_slice::<Value>(&plan_bytes).unwrap()
        );
        // A fresh apply of the same plan gets a new receipt. A failed upload
        // leaves ownership available for resume, without repeating its command.
        server.state.lock().unwrap()["failReceiptOnce"] = json!(true);
        let output = knit_fails(&workspace, apply);
        assert!(output.contains("503"), "{output}");
        let second_path = fs::read_dir(&runs)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .find(|p| p != &run_path && p.to_string_lossy().ends_with(".run.json"))
            .unwrap();
        let failed: Value = serde_json::from_slice(&fs::read(&second_path).unwrap()).unwrap();
        assert_eq!(failed["status"], "succeeded");
        assert_eq!(failed["finalization"]["synchronization"], "failed");
        // Ordinary sync cannot publish a changed receipt outside its lease.
        knit(
            &workspace,
            ["sync", "push", "--plans", "--remote", "hosted"],
        );
        assert_eq!(
            server.state.lock().unwrap()["runs"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        knit(
            &workspace,
            ["land", "resume", "--run", second_path.to_str().unwrap()],
        );
        let resumed: Value = serde_json::from_slice(&fs::read(&second_path).unwrap()).unwrap();
        assert_eq!(resumed["steps"], failed["steps"]);
        assert_eq!(resumed["finalized"], true);
        assert_eq!(resumed["finalization"]["synchronization"], "succeeded");
        knit(
            &workspace,
            ["sync", "push", "--plans", "--remote", "hosted"],
        );
        knit(
            &workspace,
            ["sync", "pull", "--plans", "--remote", "hosted"],
        );
        let state = server.state.lock().unwrap();
        assert!(state["ownership"].is_null());
        assert_eq!(state["runs"].as_array().unwrap().len(), 2);
        assert_eq!(state["runs"][1]["run"], resumed);
        drop(state);
        fs::remove_dir_all(dir).unwrap();
    }
}
