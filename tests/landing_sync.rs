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
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

struct Server {
    url: String,
    state: Arc<Mutex<Value>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(json!({"plans":[],"runs":[],"landing":{}})));
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
                let recipes = request.contains("/api/v1/projects/demo/landing-recipes ");
                if recipes {
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
                    for p in payload["plans"].as_array().unwrap() {
                        let current = state["plans"].as_array().unwrap().last();
                        if current.is_some_and(|c| c["hash"] == p["hash"]) {
                            continue;
                        }
                        if current.map(|c| &c["hash"]).unwrap_or(&Value::Null) != &p["parentHash"] {
                            code = 409;
                            break;
                        }
                        state["plans"].as_array_mut().unwrap().push(p.clone());
                    }
                    if code == 200 {
                        for r in payload["runs"].as_array().unwrap() {
                            state["runs"].as_array_mut().unwrap().push(r.clone());
                        }
                    }
                }
                let output = if code == 200 {
                    if recipes {
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
