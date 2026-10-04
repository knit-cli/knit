mod common;

use common::*;
use knit::providers::{forgejo::Forgejo, github::GitHub, gitlab::GitLab, Forge, PrTarget};
use serde_json::json;
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    process::Command,
};

const HOST: &str = "review.example.test";
const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

#[test]
fn provider_child() {
    let Ok(provider) = std::env::var("PROVIDER_AUTH_CHILD") else {
        return;
    };
    let mut target = PrTarget::explicit(std::env::current_dir().unwrap(), "upstream/widget");
    target.repo_remote = Some(format!("https://{HOST}/upstream/widget.git"));
    if std::env::var_os("MISMATCH_TARGET").is_some() {
        target.repo_full_name = Some("other/widget".into());
    }
    let forge: &dyn Forge = match provider.as_str() {
        "github" => &GitHub,
        "gitlab" => &GitLab,
        "forgejo" => &Forgejo,
        _ => panic!("unknown fixture provider"),
    };
    let result = forge.view(&target, "7");
    if let Ok(expected) = std::env::var("EXPECTED_ERROR") {
        assert!(format!("{:#}", result.unwrap_err()).contains(&expected));
    } else {
        let pr = result.unwrap();
        assert_eq!(pr.number, 7);
        assert_eq!(pr.head_ref_oid.as_deref(), Some(SHA));
        assert_eq!(pr.base_ref_name.as_deref(), Some("main"));
    }
}

fn exercise(provider: &str, mode: &str) {
    let root = unique_temp_dir();
    let repo = root.join("repo");
    init_repo(&repo, "widget");
    git(
        &repo,
        [
            "remote",
            "add",
            "origin",
            "https://origin.example.test/fork/widget.git",
        ],
    );
    let bin = root.join("bin");
    let script = r#"
import json, os, sys
args = sys.argv[1:]
provider = os.environ['PROVIDER_AUTH_CHILD']
with open(os.environ['CALL_LOG'], 'a') as f: f.write(json.dumps(args) + '\n')
if args == ['logins', 'list', '--output', 'json']:
    mode = os.environ['AUTH_MODE']
    rows = [{'name':'wrong', 'url':'https://origin.example.test', 'default':'true'}]
    if mode != 'missing': rows.append({'name':'recorded', 'url':'https://review.example.test/', 'default':'false'})
    if mode == 'ambiguous': rows.append({'name':'duplicate', 'url':'https://review.example.test'})
    print(json.dumps(rows))
else:
    endpoint = 'repos/upstream/widget/pulls/7'
    if provider == 'github':
        assert args == ['api', endpoint, '--hostname', 'review.example.test'], args
    elif provider == 'gitlab':
        endpoint = 'projects/upstream%2Fwidget/merge_requests/7'
        assert args in [['api', '--method', 'GET', endpoint + suffix, '--hostname', 'review.example.test'] for suffix in ['', '/approvals']], args
    else:
        assert args == ['api', '--method', 'GET', '--login', 'recorded', endpoint], args
    print(os.environ['REVIEW_JSON'])
"#;
    for cli in ["gh", "glab", "tea"] {
        write_fake_python_cli(&bin, cli, script);
    }
    let payload = json!({
        "number":7, "iid":7,
        "html_url":format!("https://{HOST}/upstream/widget/pulls/7"),
        "web_url":format!("https://{HOST}/upstream/widget/-/merge_requests/7"),
        "state":"open", "head":{"ref":"feature", "sha":SHA}, "base":{"ref":"main"},
        "sha":SHA, "target_branch":"main", "source_branch":"feature"
    })
    .to_string();
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "provider_child", "--nocapture"])
        .current_dir(&repo)
        .env("KNIT_HOME", root.join("home"))
        .env("PATH", path)
        .env("PROVIDER_AUTH_CHILD", provider)
        .env("AUTH_MODE", mode)
        .env("REVIEW_JSON", &payload)
        .env("CALL_LOG", root.join("calls"))
        .env_remove("KNIT_BUNDLE")
        .env_remove("KNIT_SESSION");
    scrub_ambient_forge_env(&mut command);
    for name in [
        "KNIT_GITHUB_API_BASE",
        "KNIT_GITHUB_API_TRANSPORT",
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GH_ENTERPRISE_TOKEN",
        "GITHUB_ENTERPRISE_TOKEN",
        "EXPECTED_ERROR",
        "MISMATCH_TARGET",
    ] {
        command.env_remove(name);
    }
    match mode {
        "missing" => {
            command.env("EXPECTED_ERROR", "No saved tea login");
        }
        "ambiguous" => {
            command.env("EXPECTED_ERROR", "Ambiguous saved tea logins");
        }
        "mismatch" => {
            command
                .env("EXPECTED_ERROR", "disagrees with its credential URL")
                .env("MISMATCH_TARGET", "1");
        }
        _ => {}
    }
    let server = if mode == "native" {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}/override/", listener.local_addr().unwrap());
        let prefix = provider.to_uppercase();
        command.env(format!("KNIT_{prefix}_API_BASE"), base);
        match provider {
            "github" => {
                command
                    .env("KNIT_GITHUB_API_TRANSPORT", "native")
                    .env("GH_TOKEN", "synthetic");
            }
            "gitlab" => {
                command.env("KNIT_GITLAB_TOKEN", "synthetic");
            }
            _ => {
                command.env("KNIT_FORGEJO_TOKEN", "synthetic");
            }
        }
        let provider = provider.to_owned();
        Some(std::thread::spawn(move || {
            let count = if provider == "github" { 1 } else { 2 };
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            for _ in 0..count {
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "missing native request"
                            );
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        Err(e) => panic!("{e}"),
                    }
                };
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap();
                let endpoint = if provider == "gitlab" {
                    "projects/upstream%2Fwidget/merge_requests/7"
                } else {
                    "repos/upstream/widget/pulls/7"
                };
                assert!(
                    request.starts_with(&format!("GET /override/{endpoint}")),
                    "{request}"
                );
                assert!(request.contains("synthetic"));
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", payload.len(), payload).unwrap();
            }
        }))
    } else {
        None
    };
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if let Some(server) = server {
        server.join().unwrap();
    }
    let calls = fs::read_to_string(root.join("calls")).unwrap_or_default();
    if matches!(mode, "native" | "mismatch") {
        assert!(calls.is_empty());
    }
    if matches!(mode, "missing" | "ambiguous") {
        assert_eq!(calls.lines().count(), 1);
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn explicit_hosts_use_cli_auth() {
    for provider in ["github", "gitlab", "forgejo"] {
        exercise(provider, "cli");
    }
}

#[test]
fn native_environment_auth_preserves_api_overrides() {
    for provider in ["github", "gitlab", "forgejo"] {
        exercise(provider, "native");
    }
}

#[test]
fn tea_requires_one_matching_saved_server() {
    for mode in ["missing", "ambiguous"] {
        exercise("forgejo", mode);
    }
}

#[test]
fn explicit_remote_must_match_slug() {
    for provider in ["github", "gitlab", "forgejo"] {
        exercise(provider, "mismatch");
    }
}
