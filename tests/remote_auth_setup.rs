//! `knit auth remote`: server-verified sync token setup. Real loopback HTTP
//! for verification, piped stdin for noninteractive flows, real PTY (Unix)
//! for the interactive ones. No live service, synthetic tokens only.

mod common;

use common::{knit_fails_with_env, knit_with_env, unique_temp_dir};
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// A minimal loopback sync service: scripted responses per request path, plus
// a log of the Authorization headers each endpoint saw.
// ---------------------------------------------------------------------------

struct FakeService {
    base_url: String,
    bearer_log: Arc<Mutex<Vec<String>>>,
    access_token_requests: Arc<AtomicUsize>,
}

impl FakeService {
    /// `access_token_responses`: one (status, body) per request, in order;
    /// the last entry repeats once the queue is exhausted. `forge` is the
    /// standing response for `/me/forge-credentials`.
    fn start(access_token_responses: &[(u16, &str)], forge: (u16, &str)) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let responses: Vec<(u16, String)> = access_token_responses
            .iter()
            .map(|(status, body)| (*status, (*body).to_string()))
            .collect();
        let forge = (forge.0, forge.1.to_string());
        let bearer_log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let access_token_requests = Arc::new(AtomicUsize::new(0));
        {
            let bearer_log = bearer_log.clone();
            let access_token_requests = access_token_requests.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let responses = responses.clone();
                    let forge = forge.clone();
                    let bearer_log = bearer_log.clone();
                    let access_token_requests = access_token_requests.clone();
                    std::thread::spawn(move || {
                        let _ = respond(
                            &mut stream,
                            &responses,
                            &forge,
                            &bearer_log,
                            &access_token_requests,
                        );
                    });
                }
            });
        }
        Self {
            base_url,
            bearer_log,
            access_token_requests,
        }
    }

    /// The bearer headers the service saw (both endpoints), in order.
    fn bearers(&self) -> Vec<String> {
        self.bearer_log.lock().unwrap().clone()
    }

    fn access_token_requests(&self) -> usize {
        self.access_token_requests.load(Ordering::SeqCst)
    }
}

fn respond(
    stream: &mut TcpStream,
    responses: &[(u16, String)],
    forge: &(u16, String),
    bearer_log: &Mutex<Vec<String>>,
    access_token_requests: &AtomicUsize,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let target = request_line.split_whitespace().nth(1).unwrap_or_default();
    let mut authorization = String::new();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            break;
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("authorization") {
                authorization = value.trim().to_string();
            }
        }
    }
    bearer_log.lock().unwrap().push(authorization);
    let (status, body) = if target.contains("/me/forge-credentials") {
        forge.clone()
    } else {
        // The access-token queue advances per access-token request only.
        let index = access_token_requests.fetch_add(1, Ordering::SeqCst);
        responses
            .get(index.min(responses.len().saturating_sub(1)))
            .cloned()
            .unwrap_or((200, r#"{"data":{"tokenKind":"legacy"}}"#.to_string()))
    };
    let reason = match status {
        200 => "OK",
        401 => "Unauthorized",
        403 => "Forbidden",
        _ => "Server Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

const ACCEPTED: &str =
    r#"{"data":{"tokenKind":"legacy","subjectUserId":"user-1","scopes":["bundle:read"]}}"#;

// ---------------------------------------------------------------------------
// Harness helpers
// ---------------------------------------------------------------------------

struct Fixture {
    root: PathBuf,
    outside: PathBuf,
    knit_home: PathBuf,
    env: Vec<(String, String)>,
}

impl Fixture {
    fn new() -> Self {
        let root = unique_temp_dir();
        let outside = root.join("outside");
        let knit_home = root.join("knit-home");
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(&knit_home).unwrap();
        let env = vec![(
            "KNIT_HOME".to_string(),
            knit_home.to_string_lossy().into_owned(),
        )];
        Self {
            root,
            outside,
            knit_home,
            env,
        }
    }

    fn env(&self) -> Vec<(&str, &str)> {
        self.env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }

    fn ok(&self, args: &[&str]) -> String {
        knit_with_env(&self.outside, args, &self.env())
    }

    fn fail(&self, args: &[&str]) -> String {
        knit_fails_with_env(&self.outside, args, &self.env())
    }

    fn run_with_stdin(&self, args: &[&str], stdin: Option<&str>) -> Output {
        run_with_stdin_and_env(&self.outside, args, &self.env(), stdin, &[])
    }

    fn config(&self) -> Value {
        serde_json::from_str(&fs::read_to_string(self.knit_home.join("config.json")).unwrap())
            .unwrap()
    }

    fn stored_token(&self, remote: &str) -> Option<String> {
        self.config()["remotes"][remote]["token"]
            .as_str()
            .map(ToString::to_string)
    }

    fn stored_url(&self, remote: &str) -> String {
        self.config()["remotes"][remote]["url"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }
}

fn run_with_stdin_and_env(
    cwd: &Path,
    args: &[&str],
    env: &[(&str, &str)],
    stdin: Option<&str>,
    env_remove: &[&str],
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_knit"));
    command
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", common::isolated_git_config_global())
        .env_remove("KNIT_BUNDLE")
        .env_remove("KNIT_SESSION");
    for (key, value) in env {
        command.env(key, value);
    }
    for key in env_remove {
        command.env_remove(key);
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    match stdin {
        Some(text) => {
            let mut child = command.stdin(Stdio::piped()).spawn().unwrap();
            // A guard error may close stdin before the pipe is written; the
            // exit status and output are what the assertions care about.
            let _ = child.stdin.take().unwrap().write_all(text.as_bytes());
            child.wait_with_output().unwrap()
        }
        None => command.stdin(Stdio::null()).output().unwrap(),
    }
}

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// A dead loopback endpoint (bound, then dropped) for unreachable-service
/// scenarios.
fn dead_endpoint() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    base
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn verified_token_is_saved_with_paths_and_commands() {
    let service = FakeService::start(&[(200, ACCEPTED)], (403, r#"{"errors":{}}"#));
    let f = Fixture::new();
    f.ok(&["remote", "add", "hosted", &service.base_url, "--global"]);

    let result = f.run_with_stdin(
        &["auth", "remote", "hosted", "--token-stdin"],
        Some("good-synthetic-token\n"),
    );
    let output = combined(&result);
    assert!(result.status.success(), "{output}");
    assert!(output.contains("updated hosted"), "{output}");
    assert!(output.contains(&service.base_url), "{output}");
    assert!(output.contains("Verifying token with"), "{output}");
    assert!(
        output.contains(&format!("token verified with {}", service.base_url)),
        "{output}"
    );
    assert!(
        output.contains("Token saved to:"),
        "{output} (must name the storage path)"
    );
    assert!(
        output.contains(f.knit_home.join("config.json").to_str().unwrap()),
        "{output}"
    );
    assert!(
        output.contains("knit remote auth-status hosted"),
        "{output}"
    );
    assert!(output.contains("knit auth remote hosted"), "{output}");
    assert!(!output.contains("good-synthetic-token"), "{output}");
    assert_eq!(
        f.stored_token("hosted").as_deref(),
        Some("good-synthetic-token")
    );
    assert_eq!(f.stored_url("hosted"), service.base_url);
    // The candidate (not an env override) was verified exactly once.
    assert_eq!(service.access_token_requests(), 1);
    assert_eq!(
        service.bearers()[0],
        "Bearer good-synthetic-token",
        "verification must present the candidate bearer"
    );
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
fn rejected_token_fails_noninteractive_and_preserves_old_config() {
    let service = FakeService::start(&[(401, r#"{"errors":{"detail":"nope"}}"#)], (403, "{}"));
    let f = Fixture::new();
    f.ok(&[
        "remote",
        "add",
        "hosted",
        &service.base_url,
        "--global",
        "--token",
        "kept-old-secret",
    ]);

    let result = f.run_with_stdin(
        &["auth", "remote", "hosted", "--token-stdin"],
        Some("wrong-synthetic-token\n"),
    );
    let output = combined(&result);
    assert!(!result.status.success(), "{output}");
    assert!(output.contains("rejected the token"), "{output}");
    assert!(output.contains("HTTP 401"), "{output}");
    assert!(
        output.contains("knit auth remote hosted --token-stdin"),
        "{output}"
    );
    assert!(output.contains("nothing was saved"), "{output}");
    assert!(!output.contains("wrong-synthetic-token"), "{output}");
    assert!(!output.contains("kept-old-secret"), "{output}");
    assert_eq!(f.stored_token("hosted").as_deref(), Some("kept-old-secret"));
    assert_eq!(f.stored_url("hosted"), service.base_url);
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
fn server_failure_and_malformed_responses_preserve_config_with_offline_guidance() {
    for (label, status, body) in [
        ("5xx", 500u16, r#"{"errors":{"detail":"boom"}}"#),
        ("malformed", 200, "not json"),
        ("wrong envelope shape", 200, r#"{"data":[1,2]}"#),
    ] {
        let service = FakeService::start(&[(status, body)], (403, "{}"));
        let f = Fixture::new();
        f.ok(&[
            "remote",
            "add",
            "hosted",
            &service.base_url,
            "--global",
            "--token",
            "kept-old-secret",
        ]);
        let result = f.run_with_stdin(
            &["auth", "remote", "hosted", "--token-stdin"],
            Some("any-synthetic-token\n"),
        );
        let output = combined(&result);
        assert!(!result.status.success(), "{label}: {output}");
        assert!(
            output.contains("--offline"),
            "{label}: must point at --offline: {output}"
        );
        assert!(
            output.contains("does not prove the token invalid"),
            "{label}: must not claim an invalid token: {output}"
        );
        assert!(output.contains("Nothing was saved"), "{label}: {output}");
        assert!(
            !output.contains(body),
            "{label}: raw body must not echo: {output}"
        );
        assert_eq!(
            f.stored_token("hosted").as_deref(),
            Some("kept-old-secret"),
            "{label}: config must stay untouched"
        );
        fs::remove_dir_all(&f.root).unwrap();
    }

    // A dead endpoint is the same class of failure: no false invalid-token
    // claim, no mutation.
    let dead = dead_endpoint();
    let f = Fixture::new();
    f.ok(&[
        "remote",
        "add",
        "hosted",
        &dead,
        "--global",
        "--token",
        "kept-old-secret",
    ]);
    let result = f.run_with_stdin(
        &["auth", "remote", "hosted", "--token-stdin"],
        Some("any-synthetic-token\n"),
    );
    let output = combined(&result);
    assert!(!result.status.success(), "{output}");
    assert!(output.contains("--offline"), "{output}");
    assert!(
        output.contains("does not prove the token invalid"),
        "{output}"
    );
    assert_eq!(f.stored_token("hosted").as_deref(), Some("kept-old-secret"));
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
fn offline_saves_unverified_without_any_network() {
    let dead = dead_endpoint();
    let f = Fixture::new();
    let result = f.run_with_stdin(
        &[
            "auth",
            "remote",
            "hosted",
            "--url",
            &dead,
            "--offline",
            "--token-stdin",
        ],
        Some("offline-synthetic-token\n"),
    );
    let output = combined(&result);
    assert!(result.status.success(), "{output}");
    assert!(output.contains("configured hosted"), "{output}");
    assert!(
        output.contains("token saved WITHOUT verification (--offline)"),
        "{output}"
    );
    assert!(!output.contains("token verified with"), "{output}");
    assert!(output.contains("Token saved to:"), "{output}");
    assert_eq!(
        f.stored_token("hosted").as_deref(),
        Some("offline-synthetic-token")
    );
    assert_eq!(f.stored_url("hosted"), dead);
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
fn noninteractive_failures_are_actionable_before_reading_stdin() {
    let f = Fixture::new();
    f.ok(&[
        "remote",
        "add",
        "hosted",
        "https://host.example",
        "--global",
    ]);

    // No name: list the configured remotes instead of hanging.
    let missing_name = f.run_with_stdin(&["auth", "remote"], None);
    let output = combined(&missing_name);
    assert!(!missing_name.status.success(), "{output}");
    assert!(output.contains("A remote name is required"), "{output}");
    assert!(output.contains("hosted"), "{output}");
    assert!(output.contains("--token-stdin"), "{output}");

    // No token source.
    let no_stdin = f.run_with_stdin(&["auth", "remote", "hosted"], None);
    let output = combined(&no_stdin);
    assert!(!no_stdin.status.success(), "{output}");
    assert!(
        output.contains("No token provided without a terminal"),
        "{output}"
    );
    assert!(output.contains("--token-stdin"), "{output}");

    // New remote without a URL.
    let no_url = f.run_with_stdin(&["auth", "remote", "fresh", "--token-stdin"], None);
    let output = combined(&no_url);
    assert!(!no_url.status.success(), "{output}");
    assert!(output.contains("`fresh` is not configured"), "{output}");
    assert!(output.contains("--url"), "{output}");

    // Names that carry no letter or digit never slugify into a remote name
    // (slugify's generic fallback must not be reachable here). Leading-dash
    // inputs are rejected by the CLI itself and need no in-flow check.
    for bad_name in ["///", "!!!", " "] {
        let empty_name = f.run_with_stdin(&["auth", "remote", bad_name, "--token-stdin"], None);
        let output = combined(&empty_name);
        assert!(!empty_name.status.success(), "{bad_name:?}: {output}");
        assert!(
            output.contains("must contain at least one letter or digit"),
            "{bad_name:?}: {output}"
        );
        assert!(
            f.config()["remotes"].as_object().unwrap().len() == 1,
            "{bad_name:?}: no `bundle` remote may appear: {output}"
        );
    }

    // The stored state is untouched by every failure above.
    assert!(f.stored_token("hosted").is_none());
    assert_eq!(f.stored_url("hosted"), "https://host.example");
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
fn malformed_oversized_and_multiline_input_rejected_before_any_write() {
    let service = FakeService::start(&[(200, ACCEPTED)], (403, "{}"));
    let f = Fixture::new();
    f.ok(&[
        "remote",
        "add",
        "hosted",
        &service.base_url,
        "--global",
        "--token",
        "kept-old-secret",
    ]);
    let oversized = format!("{}\n", "x".repeat(64 * 1024 + 16));
    for (label, stdin) in [
        ("empty", "\n"),
        ("whitespace only", "   \n"),
        ("multiline", "two\nlines\n"),
        ("oversized", oversized.as_str()),
    ] {
        let result = f.run_with_stdin(&["auth", "remote", "hosted", "--token-stdin"], Some(stdin));
        let output = combined(&result);
        assert!(!result.status.success(), "{label}: {output}");
        let expected = match label {
            "empty" | "whitespace only" => "remote token is empty; nothing was saved",
            "multiline" => "remote token must be a single line without control characters",
            _ => "remote token exceeds 65536 bytes",
        };
        assert!(output.contains(expected), "{label}: {output}");
        assert_eq!(
            f.stored_token("hosted").as_deref(),
            Some("kept-old-secret"),
            "{label}: rejected input must never overwrite the stored token"
        );
    }
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
fn environment_override_is_named_but_never_substituted_for_the_candidate() {
    // The server rejects every bearer: the env override must not rescue the
    // run, and its value must never appear in the output.
    let service = FakeService::start(&[(401, r#"{"errors":{}}"#)], (403, "{}"));
    let f = Fixture::new();
    f.ok(&[
        "remote",
        "add",
        "hosted",
        &service.base_url,
        "--global",
        "--token",
        "kept-old-secret",
    ]);
    let mut command = Command::new(env!("CARGO_BIN_EXE_knit"));
    command
        .args(["auth", "remote", "hosted", "--token-stdin"])
        .current_dir(&f.outside)
        .env("KNIT_HOME", &f.knit_home)
        .env("KNIT_REMOTE_HOSTED_TOKEN", "env-override-secret")
        .env("GIT_CONFIG_GLOBAL", common::isolated_git_config_global())
        .env_remove("KNIT_BUNDLE")
        .env_remove("KNIT_SESSION")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let _ = child.stdin.take().unwrap().write_all(b"piped-candidate\n");
    let output = child.wait_with_output().unwrap();
    let text = combined(&output);
    assert!(!output.status.success(), "{text}");
    assert!(!text.contains("env-override-secret"), "{text}");
    assert!(!text.contains("piped-candidate"), "{text}");
    assert_eq!(f.stored_token("hosted").as_deref(), Some("kept-old-secret"));
    // The candidate — not the override — was what got verified.
    assert_eq!(service.bearers()[0], "Bearer piped-candidate", "{text}");

    // Success path: the override is still named (variable name only) next to
    // the verified, stored candidate.
    let service_ok = FakeService::start(&[(200, ACCEPTED)], (403, "{}"));
    f.ok(&["remote", "add", "hosted2", &service_ok.base_url, "--global"]);
    let mut command = Command::new(env!("CARGO_BIN_EXE_knit"));
    command
        .args(["auth", "remote", "hosted2", "--token-stdin"])
        .current_dir(&f.outside)
        .env("KNIT_HOME", &f.knit_home)
        .env("KNIT_REMOTE_HOSTED2_TOKEN", "env-override-secret")
        .env("GIT_CONFIG_GLOBAL", common::isolated_git_config_global())
        .env_remove("KNIT_BUNDLE")
        .env_remove("KNIT_SESSION")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let _ = child
        .stdin
        .take()
        .unwrap()
        .write_all(b"stored-synthetic-token\n");
    let output = child.wait_with_output().unwrap();
    let text = combined(&output);
    assert!(output.status.success(), "{text}");
    assert!(
        text.contains("KNIT_REMOTE_HOSTED2_TOKEN overrides the stored token"),
        "{text}"
    );
    assert!(!text.contains("env-override-secret"), "{text}");
    assert_eq!(
        f.stored_token("hosted2").as_deref(),
        Some("stored-synthetic-token")
    );
    assert_eq!(service_ok.bearers()[0], "Bearer stored-synthetic-token");
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
fn unrelated_remotes_survive_and_explicit_url_change_is_deliberate() {
    let old_service = FakeService::start(&[(401, "{}")], (403, "{}"));
    let new_service = FakeService::start(&[(200, ACCEPTED)], (403, "{}"));
    let f = Fixture::new();
    f.ok(&[
        "remote",
        "add",
        "hosted",
        &old_service.base_url,
        "--global",
        "--token",
        "kept-old-secret",
    ]);
    f.ok(&[
        "remote",
        "add",
        "other",
        "https://other.example",
        "--global",
        "--token",
        "other-kept-secret",
    ]);

    let result = f.run_with_stdin(
        &[
            "auth",
            "remote",
            "hosted",
            "--url",
            &new_service.base_url,
            "--token-stdin",
        ],
        Some("fresh-synthetic-token\n"),
    );
    let output = combined(&result);
    assert!(result.status.success(), "{output}");
    assert!(output.contains("endpoint changes from"), "{output}");
    assert!(output.contains(&new_service.base_url), "{output}");
    // The candidate went to the NEW endpoint only; the old service (holding
    // the previous URL) never saw a request.
    assert_eq!(old_service.access_token_requests(), 0);
    assert_eq!(new_service.bearers()[0], "Bearer fresh-synthetic-token");
    assert_eq!(f.stored_url("hosted"), new_service.base_url);
    assert_eq!(
        f.stored_token("hosted").as_deref(),
        Some("fresh-synthetic-token")
    );
    // Sibling remote untouched.
    assert_eq!(
        f.stored_token("other").as_deref(),
        Some("other-kept-secret")
    );
    assert_eq!(f.stored_url("other"), "https://other.example");
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
fn invalid_stored_url_is_refused_before_any_secret_is_read() {
    let f = Fixture::new();
    // A legacy stored URL carrying embedded credentials must be refused
    // (never trusted as a verification target) without exposing itself.
    let secret_url = "https://user:legacy-secret@host.example";
    fs::write(
        f.knit_home.join("config.json"),
        format!(
            r#"{{"schemaVersion":"0.1","remotes":{{"hosted":{{"url":"{secret_url}","token":"kept-old-secret"}}}}}}"#
        ),
    )
    .unwrap();
    let result = f.run_with_stdin(&["auth", "remote", "hosted", "--token-stdin"], Some("x\n"));
    let output = combined(&result);
    assert!(!result.status.success(), "{output}");
    assert!(
        output.contains("stored URL that cannot be verified against"),
        "{output}"
    );
    assert!(output.contains("--url"), "{output}");
    assert!(!output.contains("legacy-secret"), "{output}");
    assert!(!output.contains("user:legacy"), "{output}");
    assert_eq!(
        f.config()["remotes"]["hosted"]["url"],
        secret_url,
        "config must stay untouched"
    );
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
fn endpoint_validation_happens_before_any_secret_is_read() {
    let f = Fixture::new();
    f.ok(&[
        "remote",
        "add",
        "hosted",
        "https://host.example",
        "--global",
    ]);
    for (label, url) in [
        ("embedded credentials", "https://user:secret@host.example"),
        ("query", "https://host.example?token=SECRET"),
        ("fragment", "https://host.example#frag"),
        ("non-http scheme", "ftp://host.example"),
        ("no host", "https://"),
    ] {
        let result = f.run_with_stdin(
            &["auth", "remote", "hosted", "--url", url, "--token-stdin"],
            Some("never-saved\n"),
        );
        let output = combined(&result);
        assert!(!result.status.success(), "{label}: {output}");
        assert!(!output.contains("SECRET"), "{label}: {output}");
        assert!(!output.contains("secret"), "{label}: {output}");
        assert!(!output.contains("never-saved"), "{label}: {output}");
        assert!(f.stored_token("hosted").is_none(), "{label}");
        assert_eq!(f.stored_url("hosted"), "https://host.example", "{label}");
    }
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
fn storage_follows_knit_home_xdg_and_home() {
    let dead = dead_endpoint();
    let root = unique_temp_dir();
    let cwd = root.join("outside");
    fs::create_dir_all(&cwd).unwrap();

    // KNIT_HOME wins.
    let knit_home = root.join("by-knit-home");
    let output = run_with_stdin_and_env(
        &cwd,
        &[
            "auth",
            "remote",
            "a",
            "--url",
            &dead,
            "--offline",
            "--token-stdin",
        ],
        &[("KNIT_HOME", knit_home.to_str().unwrap())],
        Some("synthetic-a\n"),
        &[],
    );
    assert!(output.status.success(), "{}", combined(&output));
    assert!(knit_home.join("config.json").exists());

    // XDG_CONFIG_HOME without KNIT_HOME.
    let xdg = root.join("by-xdg");
    let output = run_with_stdin_and_env(
        &cwd,
        &[
            "auth",
            "remote",
            "b",
            "--url",
            &dead,
            "--offline",
            "--token-stdin",
        ],
        &[("XDG_CONFIG_HOME", xdg.to_str().unwrap())],
        Some("synthetic-b\n"),
        &["KNIT_HOME"],
    );
    assert!(output.status.success(), "{}", combined(&output));
    assert!(xdg.join("knit/config.json").exists());

    // HOME fallback without KNIT_HOME or XDG_CONFIG_HOME.
    let home = root.join("by-home");
    fs::create_dir_all(&home).unwrap();
    let output = run_with_stdin_and_env(
        &cwd,
        &[
            "auth",
            "remote",
            "c",
            "--url",
            &dead,
            "--offline",
            "--token-stdin",
        ],
        &[("HOME", home.to_str().unwrap())],
        Some("synthetic-c\n"),
        &["KNIT_HOME", "XDG_CONFIG_HOME"],
    );
    assert!(output.status.success(), "{}", combined(&output));
    assert!(home.join(".config/knit/config.json").exists());

    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn auth_status_decouples_forge_probe_and_guides_rejected_tokens() {
    // Validated login + failed optional forge probe: success with an inline
    // note, in both output modes.
    let service = FakeService::start(&[(200, ACCEPTED)], (403, r#"{"errors":{}}"#));
    let f = Fixture::new();
    f.ok(&[
        "remote",
        "add",
        "hosted",
        &service.base_url,
        "--global",
        "--token",
        "stored-synthetic-token",
    ]);
    let json = f.ok(&["remote", "auth-status", "hosted", "--json"]);
    let parsed: Value = serde_json::from_str(json.trim())
        .unwrap_or_else(|error| panic!("auth-status emitted non-JSON ({error}): {json}"));
    assert_eq!(parsed["tokenKind"], "legacy");
    assert_eq!(parsed["subjectUserId"], "user-1");
    assert_eq!(parsed["forgeCredentials"], Value::Array(Vec::new()));
    assert!(
        parsed["forgeProbeError"]
            .as_str()
            .unwrap_or_default()
            .contains("403"),
        "{json}"
    );
    let text = f.ok(&["remote", "auth-status", "hosted"]);
    assert!(text.contains("Token kind: legacy"), "{text}");
    assert!(
        text.contains("Forge credentials: unavailable (HTTP 403)"),
        "{text}"
    );
    assert!(text.contains("sync login above is still valid"), "{text}");
    assert!(!text.contains("stored-synthetic-token"), "{text}");

    // Working forge probe: connected count.
    let service_ok = FakeService::start(
        &[(200, ACCEPTED)],
        (
            200,
            r#"{"data":[{"connected":true,"hosts":["github.com"]}]}"#,
        ),
    );
    f.ok(&[
        "remote",
        "add",
        "hosted2",
        &service_ok.base_url,
        "--global",
        "--token",
        "stored-synthetic-token",
    ]);
    let text = f.ok(&["remote", "auth-status", "hosted2"]);
    assert!(text.contains("Forge credentials: 1 connected"), "{text}");
    let json = f.ok(&["remote", "auth-status", "hosted2", "--json"]);
    let parsed: Value = serde_json::from_str(json.trim()).unwrap();
    assert!(parsed.get("forgeProbeError").is_none(), "{json}");
    assert_eq!(
        parsed["forgeCredentials"][0]["hosts"][0], "github.com",
        "{json}"
    );

    // A rejected sync token is a hard failure with the rotation command.
    let service_rejected = FakeService::start(&[(401, r#"{"errors":{}}"#)], (403, "{}"));
    f.ok(&[
        "remote",
        "add",
        "hosted3",
        &service_rejected.base_url,
        "--global",
        "--token",
        "stored-synthetic-token",
    ]);
    let failure = f.fail(&["remote", "auth-status", "hosted3"]);
    assert!(failure.contains("rejected by"), "{failure}");
    assert!(failure.contains("HTTP 401"), "{failure}");
    assert!(failure.contains("knit auth remote hosted3"), "{failure}");
    assert!(failure.contains("KNIT_REMOTE_HOSTED3_TOKEN"), "{failure}");
    assert!(!failure.contains("stored-synthetic-token"), "{failure}");

    // No token configured at all: actionable setup guidance.
    let f2 = Fixture::new();
    f2.ok(&["remote", "add", "fresh", "https://host.example", "--global"]);
    let no_token = f2.fail(&["remote", "auth-status", "fresh"]);
    assert!(
        no_token.contains("No remote token configured for `fresh`"),
        "{no_token}"
    );
    assert!(no_token.contains("knit auth remote fresh"), "{no_token}");
    fs::remove_dir_all(&f.root).unwrap();
    fs::remove_dir_all(&f2.root).unwrap();
}

#[test]
fn auth_add_token_stdin_pipe_contract_stays_bounded() {
    let f = Fixture::new();
    let oversized = format!("{}\n", "x".repeat(64 * 1024 + 16));
    let result = f.run_with_stdin(
        &[
            "auth",
            "add",
            "classic",
            "--provider",
            "github",
            "--token-stdin",
        ],
        Some(&oversized),
    );
    let output = combined(&result);
    assert!(!result.status.success(), "{output}");
    assert!(
        output.contains("forge token exceeds 65536 bytes"),
        "{output}"
    );
    assert!(!f.knit_home.join("forge-auth.json").exists(), "{output}");
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
fn auth_status_refuses_invalid_stored_url_without_leaking_it() {
    let f = Fixture::new();
    // Legacy stored URLs carrying userinfo or a query string must never be
    // requested with a token attached, and never echoed back.
    for (label, bad_url) in [
        (
            "embedded credentials",
            "https://user:legacy-secret@host.example",
        ),
        ("query string", "https://host.example?token=QUERY-SECRET"),
    ] {
        fs::write(
            f.knit_home.join("config.json"),
            format!(
                r#"{{"schemaVersion":"0.1","remotes":{{"hosted":{{"url":"{bad_url}","token":"stored-synthetic-token"}}}}}}"#
            ),
        )
        .unwrap();
        let failure = f.fail(&["remote", "auth-status", "hosted"]);
        assert!(
            failure.contains("stored URL that cannot be safely requested"),
            "{label}: {failure}"
        );
        assert!(
            failure.contains("knit auth remote hosted --url https://host.example"),
            "{label}: {failure}"
        );
        assert!(!failure.contains("legacy-secret"), "{label}: {failure}");
        assert!(!failure.contains("QUERY-SECRET"), "{label}: {failure}");
        assert!(
            !failure.contains(bad_url),
            "{label}: raw stored URL leaked: {failure}"
        );
        assert!(!failure.contains("@host.example"), "{label}: {failure}");
        assert!(!failure.contains('?'), "{label}: {failure}");
        assert!(
            !failure.contains("stored-synthetic-token"),
            "{label}: {failure}"
        );
    }
    fs::remove_dir_all(&f.root).unwrap();
}

#[test]
fn auth_status_rejection_names_the_active_environment_override() {
    // A 401 under an active environment override must point at the variable,
    // not at replacing the stored token the override shadows.
    let service = FakeService::start(&[(401, r#"{"errors":{}}"#)], (403, "{}"));
    let f = Fixture::new();
    f.ok(&[
        "remote",
        "add",
        "hosted",
        &service.base_url,
        "--global",
        "--token",
        "stored-synthetic-token",
    ]);
    for (label, var) in [
        ("specific override", "KNIT_REMOTE_HOSTED_TOKEN"),
        ("generic override", "KNIT_REMOTE_TOKEN"),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_knit"));
        command
            .args(["remote", "auth-status", "hosted"])
            .current_dir(&f.outside)
            .env("KNIT_HOME", &f.knit_home)
            .env(var, "env-active-secret")
            .env("GIT_CONFIG_GLOBAL", common::isolated_git_config_global())
            .env_remove("KNIT_BUNDLE")
            .env_remove("KNIT_SESSION")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Only the named variable is active in each run.
        if var == "KNIT_REMOTE_TOKEN" {
            command.env_remove("KNIT_REMOTE_HOSTED_TOKEN");
        }
        let output = command.output().unwrap();
        let failure = combined(&output);
        assert!(!output.status.success(), "{label}: {failure}");
        assert!(failure.contains(var), "{label}: {failure}");
        assert!(
            failure.contains("overrides the stored one"),
            "{label}: must explain the override wins: {failure}"
        );
        assert!(failure.contains("update or unset"), "{label}: {failure}");
        // The stored-token rotation is offered as the fallback only.
        assert!(
            failure.contains("knit auth remote hosted"),
            "{label}: {failure}"
        );
        assert!(!failure.contains("env-active-secret"), "{label}: {failure}");
        assert!(
            !failure.contains("stored-synthetic-token"),
            "{label}: {failure}"
        );
        if var == "KNIT_REMOTE_HOSTED_TOKEN" {
            assert!(
                failure.contains("unset both KNIT_REMOTE_HOSTED_TOKEN and KNIT_REMOTE_TOKEN"),
                "{label}: {failure}"
            );
        } else {
            assert!(
                failure.contains("unset any remote-token environment overrides"),
                "{label}: {failure}"
            );
        }
    }

    // Both overrides set: the specific one is named, and the fallback lists
    // both variables — unsetting only the specific one would reveal the
    // generic override, not the stored token.
    let mut command = Command::new(env!("CARGO_BIN_EXE_knit"));
    command
        .args(["remote", "auth-status", "hosted"])
        .current_dir(&f.outside)
        .env("KNIT_HOME", &f.knit_home)
        .env("KNIT_REMOTE_HOSTED_TOKEN", "env-active-secret")
        .env("KNIT_REMOTE_TOKEN", "env-generic-secret")
        .env("GIT_CONFIG_GLOBAL", common::isolated_git_config_global())
        .env_remove("KNIT_BUNDLE")
        .env_remove("KNIT_SESSION")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = command.output().unwrap();
    let failure = combined(&output);
    assert!(!output.status.success(), "{failure}");
    assert!(failure.contains("KNIT_REMOTE_HOSTED_TOKEN"), "{failure}");
    assert!(
        failure.contains("unset both KNIT_REMOTE_HOSTED_TOKEN and KNIT_REMOTE_TOKEN"),
        "{failure}"
    );
    assert!(!failure.contains("env-active-secret"), "{failure}");
    assert!(!failure.contains("env-generic-secret"), "{failure}");
    fs::remove_dir_all(&f.root).unwrap();
}

// The PTY fixture drives a real terminal against a real loopback service:
// bare-auth `r` discovery, hidden Enter prompts, wrong-token retry, and
// cancellation that preserves the stored configuration.
#[cfg(unix)]
#[test]
fn remote_auth_pty_discovery_retry_and_cancellation() {
    let root = std::env::temp_dir().join(format!("knit-remote-auth-pty-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let result = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/remote_auth_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_knit"))
        .arg(&root)
        .current_dir(&root)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);
    let _ = fs::remove_dir_all(&root);
    assert!(result.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("PASS"), "{stdout}");
}
