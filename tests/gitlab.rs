mod common;

use common::{
    append_line, git, init_remote_repo, knit, knit_with_fake_forge, unique_temp_dir,
    write_fake_glab,
};
use knit::providers::{gitlab::GitLab, Forge, PrTarget};
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};

fn spawn_gitlab_api(state: &std::path::Path) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let state = state.to_path_buf();
    fs::create_dir_all(&state).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let state = state.clone();
            std::thread::spawn(move || {
                let _ = handle(&mut stream, &state);
            });
        }
    });
    base
}

fn handle(stream: &mut TcpStream, state: &std::path::Path) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let mut length = 0usize;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let body = String::from_utf8_lossy(&body);
    let path = target.trim_start_matches('/');
    let mr = r#"{"iid":12,"web_url":"https://gitlab.com/acme/backend/-/merge_requests/12","state":"opened","title":"feature","target_branch":"main","source_branch":"knit/forge","description":"body","sha":"deadbeef","detailed_merge_status":"mergeable","merge_commit_sha":"merge-sha"}"#;
    let response = if method == "GET" && path.ends_with("/merge_requests/12") {
        mr.to_string()
    } else if method == "GET" && path.ends_with("/merge_requests/12/approvals") {
        r#"{"approved":true}"#.to_string()
    } else if method == "GET" && path.contains("/merge_requests/12/pipelines") {
        r#"[{"id":55,"status":"success"}]"#.to_string()
    } else if method == "GET" && path.contains("/pipelines/55/jobs") {
        r#"[{"name":"test","status":"success"},{"name":"lint","status":"failed"}]"#.to_string()
    } else if method == "PUT" && path.ends_with("/merge_requests/12/merge") {
        fs::write(state.join("merge.json"), body.as_bytes()).unwrap();
        "{}".to_string()
    } else if method == "POST" && path.ends_with("/repository/branches") {
        fs::write(state.join("branch.json"), body.as_bytes()).unwrap();
        "{}".to_string()
    } else if method == "POST" && path.ends_with("/repository/commits/merge-sha/revert") {
        fs::write(state.join("revert.json"), body.as_bytes()).unwrap();
        "{}".to_string()
    } else if method == "POST" && path.ends_with("/merge_requests") {
        r#"{"iid":99,"web_url":"https://gitlab.com/acme/backend/-/merge_requests/99","state":"opened","source_branch":"knit/revert","target_branch":"main"}"#.to_string()
    } else {
        format!(r#"{{"unexpected":"{method} {path}"}}"#)
    };
    write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}",
        response.len()
    )?;
    stream.flush()
}

#[test]
fn gitlab_native_parity_surfaces_are_hermetic() {
    let root = unique_temp_dir();
    let state = root.join("gitlab-state");
    let base = spawn_gitlab_api(&state);
    std::env::set_var("KNIT_GITLAB_API_BASE", &base);
    std::env::set_var("KNIT_GITLAB_TOKEN", "test-token");

    let forge = GitLab;
    let target = PrTarget::explicit(&root, "acme/backend");
    let pr = forge.view(&target, "12").unwrap();
    assert_eq!(pr.mergeable.as_deref(), Some("MERGEABLE"));
    assert_eq!(pr.review_decision.as_deref(), Some("APPROVED"));
    assert_eq!(pr.head_ref_oid.as_deref(), Some("deadbeef"));

    let checks = forge.check_runs(&target, "12", true).unwrap();
    assert_eq!(checks.len(), 2);
    assert_eq!(checks[0].bucket.as_deref(), Some("pass"));
    assert_eq!(checks[1].bucket.as_deref(), Some("fail"));

    forge
        .merge(&target, "12", "squash", true, Some("deadbeef"))
        .unwrap();
    let merge = fs::read_to_string(state.join("merge.json")).unwrap();
    assert!(merge.contains("\"sha\":\"deadbeef\""));
    assert!(merge.contains("\"squash\":true"));

    let url = forge
        .revert_pull_request(&target, "12", "Revert feature", "why")
        .unwrap();
    assert_eq!(url, "https://gitlab.com/acme/backend/-/merge_requests/99");
    assert!(state.join("branch.json").exists());
    assert!(state.join("revert.json").exists());

    std::env::remove_var("KNIT_GITLAB_API_BASE");
    std::env::remove_var("KNIT_GITLAB_TOKEN");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn gitlab_cli_workspace_publish_and_land_loop() {
    let root = unique_temp_dir();
    let (remote, backend, _collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    knit(&workspace, ["bundle", "forge workspace"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let feature = workspace.join(".knit/worktrees/forge-workspace/backend");
    append_line(&feature.join("app.txt"), "gitlab");
    knit(&workspace, ["commit", "--all", "-m", "GitLab change"]);
    git(
        &feature,
        [
            "remote",
            "set-url",
            "origin",
            "https://gitlab.com/acme/backend.git",
        ],
    );
    git(
        &feature,
        [
            "remote",
            "set-url",
            "--push",
            "origin",
            remote.to_str().unwrap(),
        ],
    );
    let bundle_path = workspace.join(".knit/bundles/forge-workspace.bundle.json");
    let mut bundle: Value =
        serde_json::from_str(&fs::read_to_string(&bundle_path).unwrap()).unwrap();
    bundle["repos"][0]["remote"] = Value::String("https://gitlab.com/acme/backend.git".to_string());
    fs::write(&bundle_path, serde_json::to_string_pretty(&bundle).unwrap()).unwrap();

    let fake_bin = root.join("fake-bin");
    let fake_dir = root.join("fake-forge");
    write_fake_glab(&fake_bin, &fake_dir);
    let publish = knit_with_fake_forge(
        &workspace,
        [
            "publish",
            "create",
            "--provider",
            "gitlab",
            "--no-sync",
            "--no-remote",
        ],
        &fake_bin,
        &fake_dir,
        &[],
    );
    assert!(publish.contains("created"), "{publish}");
    let recorded: Value = serde_json::from_str(&fs::read_to_string(&bundle_path).unwrap()).unwrap();
    assert_eq!(recorded["publications"][0]["provider"], "gitlab");
    assert_eq!(recorded["publications"][0]["kind"], "merge_request");

    let merged = configure_landing_fixture(&fake_dir, &feature, &remote);
    configure_cli_landing(&fake_bin, &fake_dir);
    let env = [];
    knit_with_fake_forge(&workspace, ["land"], &fake_bin, &fake_dir, &env);
    let _landed = knit_with_fake_forge(
        &workspace,
        ["land", "apply", "--no-remote"],
        &fake_bin,
        &fake_dir,
        &env,
    );
    assert!(fake_dir.join("glab-merged").exists());
    let archived: Value = serde_json::from_str(&fs::read_to_string(&bundle_path).unwrap()).unwrap();
    assert_eq!(archived["state"], "archived");
    assert_eq!(archived["publications"][0]["state"], "MERGED");
    assert!(!feature.exists());
    assert_eq!(git(&remote, ["rev-parse", "main"]).trim(), merged);
    let run_path = fs::read_dir(workspace.join(".knit/land-runs"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.to_string_lossy().ends_with(".run.json"))
        .unwrap();
    let run: Value = serde_json::from_str(&fs::read_to_string(run_path).unwrap()).unwrap();
    assert_eq!(run["status"], "succeeded");
    let merge_step = run["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["type"] == "merge_pr")
        .unwrap();
    assert_eq!(merge_step["output"]["revision"], merged);
    assert_eq!(merge_step["attribution"], "performed");
    fs::remove_dir_all(root).unwrap();
}

// Model a real merge commit, distinct from the review head, using only disposable repositories.
fn configure_landing_fixture(
    state: &std::path::Path,
    feature: &std::path::Path,
    remote: &std::path::Path,
) -> String {
    let head = git(feature, ["rev-parse", "HEAD"]);
    let base = git(feature, ["rev-parse", "HEAD^"]);
    let tree = git(feature, ["rev-parse", "HEAD^{tree}"]);
    let merged = git(
        feature,
        [
            "commit-tree",
            tree.trim(),
            "-p",
            base.trim(),
            "-p",
            head.trim(),
            "-m",
            "Synthetic review merge",
        ],
    );
    git(
        feature,
        [
            "push",
            remote.to_str().unwrap(),
            &format!("{}:refs/fixture/merge", merged.trim()),
        ],
    );
    fs::write(state.join("head-sha"), head.trim()).unwrap();
    fs::write(state.join("merge-sha"), merged.trim()).unwrap();
    fs::write(state.join("remote-path"), remote.to_str().unwrap()).unwrap();
    // Keep provider detection realistic while ensuring Git never contacts the forge.
    git(
        feature,
        [
            "config",
            &format!("url.{}.insteadOf", remote.display()),
            "https://gitlab.com/acme/backend.git",
        ],
    );
    assert_ne!(head.trim(), merged.trim());
    merged.trim().to_owned()
}

fn configure_cli_landing(bin: &std::path::Path, state: &std::path::Path) {
    let head = fs::read_to_string(state.join("head-sha")).unwrap();
    let merged = fs::read_to_string(state.join("merge-sha")).unwrap();
    for status in ["opened", "merged"] {
        let review = serde_json::json!({"iid":12,"web_url":"https://gitlab.com/acme/backend/-/merge_requests/12",
            "state":status,"title":"feature","target_branch":"main","source_branch":"knit/forge-workspace",
            "sha":head,"merge_commit_sha":if status=="merged" {Some(&merged)} else {None},"detailed_merge_status":"mergeable"});
        fs::write(
            state.join(format!("review-{status}.json")),
            review.to_string(),
        )
        .unwrap();
    }
    let script = bin.join("glab");
    let original = fs::read_to_string(&script).unwrap();
    let script_text = original.lines().map(|line| {
        if line.contains("printf '{\"iid\":12") {
            "    cat \"$FORGE_FAKE_DIR/review-$state.json\"".to_owned()
        } else if line.contains(": >\"$FORGE_FAKE_DIR/glab-merged\"") {
            "    git --git-dir=\"$(cat \"$FORGE_FAKE_DIR/remote-path\")\" update-ref refs/heads/main \"$(cat \"$FORGE_FAKE_DIR/merge-sha\")\"\n    : >\"$FORGE_FAKE_DIR/glab-merged\"".to_owned()
        } else { line.to_owned() }
    }).collect::<Vec<_>>().join("\n");
    fs::write(script, script_text).unwrap();
    write_checkout_git(bin);
}

fn write_checkout_git(bin: &std::path::Path) {
    let real = std::process::Command::new("git")
        .args(["--exec-path"])
        .output()
        .unwrap();
    assert!(real.status.success());
    let real = std::path::PathBuf::from(String::from_utf8(real.stdout).unwrap().trim()).join("git");
    let quoted = format!("'{}'", real.to_string_lossy().replace('\'', "'\\''"));
    let script = bin.join("git");
    fs::write(&script, format!("#!/bin/sh\nif [ \"$*\" = 'remote get-url origin' ]; then\n  exec {quoted} config --get remote.origin.url\nfi\nexec {quoted} \"$@\"\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }
    #[cfg(windows)]
    fs::write(script.with_extension("cmd"), "@sh \"%~dp0git\" %*\r\n").unwrap();
}
