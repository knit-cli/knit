mod common;

use common::{git, init_repo, unique_temp_dir};
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

struct Workspace {
    root: PathBuf,
    workspace: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let root = unique_temp_dir();
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let this = Self { root, workspace };
        this.ok(&["init", "demo"], None);
        this
    }

    fn run(&self, args: &[&str], input: Option<&str>) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_knit"))
            .current_dir(&self.workspace)
            .env("KNIT_HOME", self.root.join("home"))
            .env("GIT_CONFIG_GLOBAL", self.root.join("empty-gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("KNIT_ADVICE", "false")
            .env_remove("KNIT_BUNDLE")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.unwrap_or_default().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn ok(&self, args: &[&str], input: Option<&str>) -> String {
        let output = self.run(args, input);
        assert!(
            output.status.success(),
            "knit {args:?}:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn add_repo(&self, id: &str, remote: &str) {
        let repo = self.workspace.join(id);
        init_repo(&repo, id);
        git(&repo, ["remote", "add", "origin", remote]);
        self.ok(
            &[
                "project",
                "add",
                id,
                repo.to_str().unwrap(),
                "--base",
                "main",
            ],
            None,
        );
    }

    fn add_credential(&self, name: &str, provider: &str, host: &str, token: &str) {
        self.ok(
            &[
                "auth",
                "add",
                name,
                "--provider",
                provider,
                "--host",
                host,
                "--token-stdin",
            ],
            Some(token),
        );
    }

    fn credentials(&self) -> Vec<(String, Value)> {
        let status: Value =
            serde_json::from_str(&self.ok(&["auth", "status", "--json"], None)).unwrap();
        status["repositories"]
            .as_array()
            .unwrap()
            .iter()
            .map(|repo| {
                (
                    repo["repo"].as_str().unwrap().to_owned(),
                    repo["defaultCredential"].clone(),
                )
            })
            .collect()
    }

    fn helper(&self, request: &str) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_knit"))
            .current_dir(&self.workspace)
            .env("KNIT_HOME", self.root.join("home"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["auth", "git-credential", "--resolve", "get"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(request.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// A dumb-protocol Git HTTP server on a loopback port that records every
/// Authorization header it receives.
fn serve_dumb_http(root: PathBuf) -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            if reader.read_line(&mut request).is_err() {
                continue;
            }
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(value) = line
                    .split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                    .map(|(_, value)| value.trim().to_owned())
                {
                    recorded.lock().unwrap().push(value);
                }
            }
            let path = request.split(' ').nth(1).unwrap_or("/");
            let path = path.split('?').next().unwrap().trim_start_matches('/');
            let response = match fs::read(root.join(path)) {
                Ok(body) if !path.contains("..") => {
                    let mut head = format!(
                        "HTTP/1.0 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    head.extend(body);
                    head
                }
                _ => b"HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec(),
            };
            let _ = stream.write_all(&response);
        }
    });
    (port, seen)
}

fn served_repository(root: &Path) -> PathBuf {
    let source = root.join("source");
    init_repo(&source, "served");
    let served = root.join("srv");
    fs::create_dir_all(served.join("org")).unwrap();
    git(
        root,
        [
            "clone",
            "--bare",
            "--quiet",
            source.to_str().unwrap(),
            served.join("org/app.git").to_str().unwrap(),
        ],
    );
    git(&served.join("org/app.git"), ["update-server-info"]);
    served
}

fn basic(user: &str, token: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = format!("{user}:{token}").into_bytes();
    let mut encoded = String::new();
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        for index in 0..4 {
            if index <= chunk.len() {
                encoded.push(ALPHABET[(n >> (18 - 6 * index) & 63) as usize] as char);
            } else {
                encoded.push('=');
            }
        }
    }
    format!("Basic {encoded}")
}

#[test]
fn loopback_http_remote_on_custom_port_fetches_with_its_saved_token() {
    let workspace = Workspace::new();
    let (port, seen) = serve_dumb_http(served_repository(&workspace.root));
    let host = format!("127.0.0.1:{port}");
    workspace.add_repo("app", &format!("http://{host}/org/app.git"));
    workspace.add_credential("local", "forgejo", &host, "loopback-secret");
    assert_eq!(
        workspace.credentials(),
        vec![("app".to_owned(), Value::from("local"))]
    );

    let output = workspace.run(&["fetch", "--mode", "git"], None);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{text}");
    assert!(!text.contains("loopback-secret"), "{text}");
    let checkout = workspace.workspace.join("app");
    let fetched = Command::new("git")
        .current_dir(&checkout)
        .args(["rev-parse", "--verify", "refs/remotes/origin/main"])
        .output()
        .unwrap();
    assert!(fetched.status.success(), "origin/main was not fetched");
    let expected = basic("oauth2", "loopback-secret");
    let seen = seen.lock().unwrap();
    assert!(
        !seen.is_empty(),
        "the token never reached the loopback forge"
    );
    assert!(seen.iter().all(|header| header == &expected), "{seen:?}");
}

#[test]
fn plain_http_token_needs_loopback_or_an_explicit_allowance() {
    let workspace = Workspace::new();
    let host = "forge.example.invalid:3000";
    workspace.add_repo("app", &format!("http://{host}/org/app.git"));
    workspace.add_credential("internal", "forgejo", host, "internal-secret");

    let refused = workspace.run(&["fetch", "--mode", "git"], None);
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success());
    assert!(
        stderr.contains("plain HTTP to http://forge.example.invalid:3000")
            && stderr.contains("knit auth allow-http forge.example.invalid:3000"),
        "{stderr}"
    );
    assert!(!stderr.contains("internal-secret"));

    let request = format!("protocol=http\nhost={host}\npath=org/app.git\n\n");
    let output = workspace.helper(&request);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("quit=1"), "{stdout}");
    assert!(!stdout.contains("password="), "{stdout}");

    workspace.ok(&["auth", "allow-http", "Forge.Example.Invalid:3000"], None);
    let saved: Value = serde_json::from_str(
        &fs::read_to_string(workspace.root.join("home/forge-auth.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(saved["plainHttpHosts"], serde_json::json!([host]));
    let output = workspace.helper(&request);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "username=oauth2\npassword=internal-secret\n\n"
    );
    let allowed = workspace.run(&["fetch", "--mode", "git"], None);
    assert!(!String::from_utf8_lossy(&allowed.stderr).contains("Refusing to send"));

    // The allowance is per origin: another port on the same host stays refused.
    let other = "protocol=http\nhost=forge.example.invalid:3001\npath=org/app.git\n\n";
    workspace.add_credential("other", "forgejo", "forge.example.invalid:3001", "other");
    assert!(String::from_utf8_lossy(&workspace.helper(other).stdout).contains("quit=1"));

    workspace.ok(&["auth", "allow-http", host, "--revoke"], None);
    let output = workspace.helper(&request);
    assert!(String::from_utf8_lossy(&output.stdout).contains("quit=1"));
}

#[test]
fn plain_http_remote_without_a_credential_is_used_freely() {
    let workspace = Workspace::new();
    let (port, seen) = serve_dumb_http(served_repository(&workspace.root));
    workspace.add_repo("app", &format!("http://localhost:{port}/org/app.git"));
    assert_eq!(
        workspace.credentials(),
        vec![("app".to_owned(), Value::Null)]
    );
    workspace.ok(&["fetch", "--mode", "git"], None);
    assert!(seen.lock().unwrap().is_empty());
}

#[test]
fn credentials_are_keyed_by_host_and_port_with_legacy_default_port_entries() {
    let workspace = Workspace::new();
    workspace.add_repo(
        "ported",
        "https://gitlab.company.test:8443/group/ported.git",
    );
    workspace.add_repo(
        "default",
        "https://gitlab.company.test:443/group/default.git",
    );
    workspace.add_repo("ssh", "ssh://git@gitlab.company.test:2222/group/ssh.git");
    workspace.add_repo("scp", "git@gitlab.company.test:group/scp.git");
    workspace.add_credential(
        "ported",
        "gitlab",
        "gitlab.company.test:8443",
        "ported-secret",
    );
    // A legacy host-only entry, as saved before ports were supported.
    let mut saved: Value = serde_json::from_str(
        &fs::read_to_string(workspace.root.join("home/forge-auth.json")).unwrap(),
    )
    .unwrap();
    saved["credentials"]["legacy"] = serde_json::json!({
        "provider": "gitlab", "host": "gitlab.company.test", "tokenEnv": "KNIT_TEST_LEGACY_TOKEN"
    });
    fs::write(
        workspace.root.join("home/forge-auth.json"),
        serde_json::to_vec_pretty(&saved).unwrap(),
    )
    .unwrap();

    let mut credentials = workspace.credentials();
    credentials.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        credentials,
        vec![
            ("default".to_owned(), Value::from("legacy")),
            ("ported".to_owned(), Value::from("ported")),
            ("scp".to_owned(), Value::from("legacy")),
            ("ssh".to_owned(), Value::from("legacy")),
        ]
    );
    let output = workspace
        .helper("protocol=https\nhost=gitlab.company.test:8443\npath=group/ported.git\n\n");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "username=oauth2\npassword=ported-secret\n\n"
    );
    let list = workspace.ok(&["auth", "list"], None);
    assert!(
        list.contains("default for gitlab.company.test:8443"),
        "{list}"
    );
}
