mod common;
use common::*;
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

#[test]
fn native_template_preview_uses_upstream_target_and_preserves_template_prose() {
    let root = unique_temp_dir();
    let (_, source, _) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    knit(&workspace, ["bundle", "template preview"]);
    knit(&workspace, ["bundle", "add", source.to_str().unwrap()]);
    let checkout = workspace.join(".knit/worktrees/template-preview/backend");
    append_line(&checkout.join("app.txt"), "feature");
    knit(&workspace, ["commit", "--all", "-m", "Review feature"]);
    let artifact = workspace.join(".knit/bundles/template-preview.bundle.json");
    let mut bundle: Value = serde_json::from_slice(&fs::read(&artifact).unwrap()).unwrap();
    bundle["repos"][0]["remote"] = json!("https://github.com/upstream/backend.git");
    bundle["repos"][0]["sourceRemote"] = json!("https://github.com/contributor/backend.git");
    bundle["repos"][0]["targetRemote"] = json!("https://github.com/upstream/backend.git");
    bundle["publish"] = json!({"body":{"file":"missing-{repo}.md","fallback":"upstream-template"}});
    fs::write(&artifact, serde_json::to_vec_pretty(&bundle).unwrap()).unwrap();
    let before = fs::read(&artifact).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    let server = std::thread::spawn(move || {
        for mut stream in listener.incoming().take(4).map(Result::unwrap) {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line.trim().is_empty() {
                    break;
                }
                headers.push_str(&line);
            }
            assert!(headers
                .to_ascii_lowercase()
                .contains("accept: application/vnd.github.raw+json"));
            seen.lock().unwrap().push(request.clone());
            let (status, body) = if request.contains("/docs/PULL_REQUEST_TEMPLATE.md?") {
                (
                    "200 OK",
                    "## Review checklist\nTitle: Ordinary template prose\n- [ ] Verify behavior\n",
                )
            } else {
                ("404 Not Found", "missing")
            };
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    let preview = knit_with_env(
        &root,
        [
            "publish",
            "create",
            "--from-artifact",
            artifact.to_str().unwrap(),
            "--dry-run",
            "--target",
            "release/next",
        ],
        &[
            ("KNIT_GITHUB_API_TRANSPORT", "1"),
            ("KNIT_GITHUB_API_BASE", &base),
            ("GH_TOKEN", "synthetic-token"),
        ],
    );
    server.join().unwrap();
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    for request in requests.iter() {
        assert!(
            request.starts_with("GET /repos/upstream/backend/contents/"),
            "{request}"
        );
        assert!(request.contains("ref=release%2Fnext"), "{request}");
        assert!(!request.contains("contributor"), "{request}");
    }
    assert!(
        preview.contains(
            "body source: upstream:upstream/backend/docs/PULL_REQUEST_TEMPLATE.md@release/next"
        ),
        "{preview}"
    );
    assert!(
        preview.contains("Title: Ordinary template prose"),
        "{preview}"
    );
    assert!(preview.contains("<!-- BEGIN KNIT BUNDLE -->"), "{preview}");
    assert_eq!(fs::read(&artifact).unwrap(), before);
    fs::remove_dir_all(root).unwrap();
}
