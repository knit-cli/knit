mod common;
#[path = "common/provider_fixture.rs"]
mod provider_fixture;

use common::{
    append_line, git, init_remote_repo, knit, knit_with_fake_forge, unique_temp_dir, write_fake_tea,
};
use knit::providers::{forgejo::Forgejo, Forge, PrTarget};
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};

fn spawn_forgejo_api(state: &std::path::Path) -> String {
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

fn pr_json() -> &'static str {
    r#"{"number":4,"html_url":"https://codeberg.org/acme/backend/pulls/4","state":"open","title":"feature","body":"body","draft":false,"merged":false,"mergeable":true,"head":{"ref":"knit/forge","sha":"deadbeef"},"base":{"ref":"main","sha":"base-sha"}}"#
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
    let response = if method == "GET" && path.ends_with("/pulls/4") {
        pr_json().to_string()
    } else if method == "GET" && path.ends_with("/pulls/4/reviews") {
        r#"[{"state":"APPROVED"}]"#.to_string()
    } else if method == "GET" && path.ends_with("/commits/deadbeef/status") {
        r#"{"statuses":[{"context":"ci","state":"success"}]}"#.to_string()
    } else if method == "PATCH" && path.ends_with("/pulls/4") {
        fs::write(state.join("edit.json"), body.as_bytes()).unwrap();
        pr_json().to_string()
    } else if method == "POST" && path.ends_with("/pulls/4/merge") {
        fs::write(state.join("merge.json"), body.as_bytes()).unwrap();
        "{}".to_string()
    } else if method == "POST" && path.ends_with("/pulls") {
        fs::write(state.join("create.json"), body.as_bytes()).unwrap();
        pr_json().to_string()
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
fn forgejo_native_parity_surfaces_are_hermetic() {
    let root = unique_temp_dir();
    let state = root.join("forgejo-state");
    let base = spawn_forgejo_api(&state);
    std::env::set_var("KNIT_FORGEJO_API_BASE", &base);
    std::env::set_var("KNIT_FORGEJO_TOKEN", "test-token");

    let forge = Forgejo;
    let target = PrTarget::explicit(&root, "acme/backend");
    let url = forge
        .create(&target, "main", "knit/forge", "Feature", "body", true)
        .unwrap();
    assert_eq!(url, "https://codeberg.org/acme/backend/pulls/4");
    let create: Value =
        serde_json::from_str(&fs::read_to_string(state.join("create.json")).unwrap()).unwrap();
    assert_eq!(create["title"], "Draft: Feature");

    let pr = forge.view(&target, "4").unwrap();
    assert_eq!(pr.body.as_deref(), Some("body"));
    assert_eq!(pr.head_ref_oid.as_deref(), Some("deadbeef"));
    assert_eq!(pr.mergeable.as_deref(), Some("MERGEABLE"));
    assert_eq!(pr.review_decision.as_deref(), Some("APPROVED"));

    forge.edit_base(&target, "4", "staging").unwrap();
    let edit: Value =
        serde_json::from_str(&fs::read_to_string(state.join("edit.json")).unwrap()).unwrap();
    assert_eq!(edit["base"], "staging");

    let checks = forge.check_runs(&target, "4", true).unwrap();
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].bucket.as_deref(), Some("pass"));

    forge
        .merge(&target, "4", "rebase", true, Some("deadbeef"))
        .unwrap();
    let merge: Value =
        serde_json::from_str(&fs::read_to_string(state.join("merge.json")).unwrap()).unwrap();
    assert_eq!(merge["Do"], "rebase");
    assert_eq!(merge["delete_branch_after_merge"], true);

    let checkout = root.join("checkout");
    fs::create_dir_all(&checkout).unwrap();
    git(&checkout, ["init"]);
    git(
        &checkout,
        [
            "remote",
            "add",
            "origin",
            "https://codeberg.org/acme/backend.git",
        ],
    );
    let checkout_target = PrTarget::checkout(&checkout);
    let url = forge
        .create(
            &checkout_target,
            "main",
            "knit/forge",
            "Checkout feature",
            "body",
            false,
        )
        .unwrap();
    assert_eq!(url, "https://codeberg.org/acme/backend/pulls/4");
    forge
        .merge(&checkout_target, "4", "merge", false, Some("deadbeef"))
        .unwrap();

    std::env::remove_var("KNIT_FORGEJO_API_BASE");
    std::env::remove_var("KNIT_FORGEJO_TOKEN");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn forgejo_cli_workspace_publish_and_land_loop() {
    let root = unique_temp_dir();
    let (remote, backend, _collaborator) = init_remote_repo(&root, "backend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    knit(&workspace, ["bundle", "forge workspace"]);
    knit(&workspace, ["bundle", "add", backend.to_str().unwrap()]);
    let feature = workspace.join(".knit/worktrees/forge-workspace/backend");
    append_line(&feature.join("app.txt"), "forgejo");
    knit(&workspace, ["commit", "--all", "-m", "Forgejo change"]);
    let bundle_path = workspace.join(".knit/bundles/forge-workspace.bundle.json");
    let mut bundle: Value =
        serde_json::from_str(&fs::read_to_string(&bundle_path).unwrap()).unwrap();
    bundle["repos"][0]["remote"] =
        Value::String("https://codeberg.org/acme/backend.git".to_string());
    fs::write(&bundle_path, serde_json::to_string_pretty(&bundle).unwrap()).unwrap();

    let fake_bin = root.join("fake bin");
    let fake_dir = root.join("fake forge");
    write_fake_tea(&fake_bin, &fake_dir);
    let publish = knit_with_fake_forge(
        &workspace,
        [
            "publish",
            "create",
            "--provider",
            "forgejo",
            "--no-sync",
            "--no-remote",
        ],
        &fake_bin,
        &fake_dir,
        &[],
    );
    assert!(publish.contains("created"), "{publish}");
    let recorded: Value = serde_json::from_str(&fs::read_to_string(&bundle_path).unwrap()).unwrap();
    assert_eq!(recorded["publications"][0]["provider"], "forgejo");
    assert_eq!(recorded["publications"][0]["kind"], "pull_request");

    git(
        &feature,
        [
            "remote",
            "set-url",
            "origin",
            "https://codeberg.org/acme/backend.git",
        ],
    );
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
    assert!(fake_dir.join("tea-merged").exists());
    let calls = fs::read_to_string(fake_dir.join("tea-landing.calls")).unwrap();
    assert!(
        calls.contains("api --method GET repos/{owner}/{repo}/pulls/4"),
        "{calls}"
    );
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
            "https://codeberg.org/acme/backend.git",
        ],
    );
    assert_ne!(head.trim(), merged.trim());
    let parents = git(feature, ["rev-list", "--parents", "-n", "1", merged.trim()]);
    assert_eq!(
        parents.split_whitespace().collect::<Vec<_>>(),
        [merged.trim(), base.trim(), head.trim()]
    );
    merged.trim().to_owned()
}

fn configure_cli_landing(bin: &std::path::Path, state: &std::path::Path) {
    let head = fs::read_to_string(state.join("head-sha")).unwrap();
    let merged = fs::read_to_string(state.join("merge-sha")).unwrap();
    for (status, applied) in [("open", false), ("merged", true)] {
        let review = serde_json::json!({"merged":applied,
            "merge_commit_sha":if applied {Some(&merged)} else {None},
            "head":{"sha":head},"base":{"sha":"unrelated-base-tip"}});
        fs::write(
            state.join(format!("review-{status}.json")),
            review.to_string(),
        )
        .unwrap();
    }
    provider_fixture::install(bin, Some("tea"));
}
