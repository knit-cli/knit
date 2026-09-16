mod common;

use common::{git, init_repo, unique_temp_dir};
use serde_json::json;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn run_with_input(command: &mut Command, input: &str) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn fixture() -> std::path::PathBuf {
    let root = unique_temp_dir();
    fs::write(root.join("forge-auth.json"), serde_json::to_vec(&json!({
        "credentials": {"work": {"provider":"github", "host":"github.com", "tokenEnv":"KNIT_TEST_PROJECT_TOKEN"}}
    })).unwrap()).unwrap();
    root
}

fn helper(root: &Path, operation: &str, request: &str) -> Output {
    run_with_input(
        Command::new(env!("CARGO_BIN_EXE_knit"))
            .current_dir(root)
            .env("KNIT_HOME", root)
            .env("KNIT_TEST_PROJECT_TOKEN", "scoped-test-secret")
            .args([
                "auth",
                "git-credential",
                "--credential",
                "work",
                "--host",
                "github.com",
                "--path",
                "org/repo.git",
                operation,
            ]),
        request,
    )
}

#[test]
fn helper_returns_secret_only_for_exact_https_repository() {
    let root = fixture();
    let output = helper(
        &root,
        "get",
        "protocol=https\nhost=github.com\npath=org/repo.git\n\n",
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "username=x-access-token\npassword=scoped-test-secret\n\n"
    );
    for request in [
        "protocol=http\nhost=github.com\npath=org/repo.git\n\n",
        "protocol=https\nhost=evil.test\npath=org/repo.git\n\n",
        "protocol=https\nhost=github.com\npath=org/another.git\n\n",
        "protocol=https\nhost=github.com\npath=org/repo.git/child\n\n",
        "protocol=https\nhost=github.com\npath=org/repo\n\n",
        "protocol=https\nhost=github.com\n\n",
        "protocol=https\nhost=github.com\npath=org/repo.git\nurl=https://evil.test/\n\n",
    ] {
        let output = helper(&root, "get", request);
        assert!(
            output.stdout.is_empty(),
            "unexpected credential for {request}"
        );
    }
    for operation in ["store", "erase"] {
        let output = helper(
            &root,
            operation,
            "protocol=https\nhost=github.com\npath=org/repo.git\n\n",
        );
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
    }
    fs::remove_dir_all(root).unwrap();
}

const MODERN_CREDENTIAL_METADATA: &str = "capability[]=authtype\ncapability[]=state\ncapability[]=future\nwwwauth[]=Basic realm=forge\nwwwauth[]=Bearer realm=forge\nstate[]=first\nstate[]=\nstate[]=second\nfuture=value\nfuture=another\nfuture[]=first\nfuture[]=second\n";

#[test]
fn helper_ignores_repeated_modern_credential_metadata() {
    let root = fixture();
    let output = helper(
        &root,
        "get",
        &format!(
            "{MODERN_CREDENTIAL_METADATA}protocol=https\nhost=github.com\npath=org/repo.git\n\n"
        ),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "username=x-access-token\npassword=scoped-test-secret\n\n"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn helper_metadata_does_not_bypass_scope_or_duplicate_scalar_checks() {
    let root = fixture();
    for (key, valid, mismatch) in [
        ("protocol", "https", "http"),
        ("host", "github.com", "evil.test"),
        ("path", "org/repo.git", "org/another.git"),
        (
            "url",
            "https://github.com/org/repo.git",
            "https://evil.test/",
        ),
    ] {
        let other_fields: String = [
            ("protocol", "https"),
            ("host", "github.com"),
            ("path", "org/repo.git"),
        ]
        .into_iter()
        .filter(|(field, _)| *field != key)
        .map(|(field, value)| format!("{field}={value}\n"))
        .collect();
        for values in [
            vec![mismatch],
            vec![valid, valid],
            vec![valid, mismatch],
            vec![mismatch, valid],
        ] {
            let repeated: String = values
                .iter()
                .map(|value| format!("{key}={value}\n"))
                .collect();
            let request = format!("{other_fields}{MODERN_CREDENTIAL_METADATA}{repeated}\n");
            let output = helper(&root, "get", &request);
            assert!(
                output.stdout.is_empty(),
                "unexpected credential for {request}"
            );
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(!error.contains("scoped-test-secret"));
            if values.len() > 1 {
                assert!(!output.status.success());
                assert!(
                    error.contains("duplicate field in Git credential request"),
                    "{error}"
                );
            } else {
                assert!(output.status.success(), "{error}");
            }
        }
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn helper_ignored_metadata_still_counts_toward_request_limit() {
    let root = fixture();
    let request = format!(
        "protocol=https\nhost=github.com\npath=org/repo.git\nfuture={}\n\n",
        "x".repeat(64 * 1024)
    );
    let output = helper(&root, "get", &request);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("Git credential request exceeds limit"),
        "{error}"
    );
    assert!(!error.contains("scoped-test-secret"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn bitbucket_helper_uses_git_api_token_username_or_repository_token_username() {
    let root = fixture();
    for (username, expected) in [
        (Some("person@example.test"), "x-bitbucket-api-token-auth"),
        (None, "x-token-auth"),
    ] {
        fs::write(root.join("forge-auth.json"), serde_json::to_vec(&json!({
            "credentials": {"work": {"provider":"bitbucket", "host":"bitbucket.org", "username": username, "tokenEnv":"KNIT_TEST_PROJECT_TOKEN"}}
        })).unwrap()).unwrap();
        let output = run_with_input(
            Command::new(env!("CARGO_BIN_EXE_knit"))
                .current_dir(&root)
                .env("KNIT_HOME", &root)
                .env("KNIT_TEST_PROJECT_TOKEN", "test-token")
                .args([
                    "auth",
                    "git-credential",
                    "--credential",
                    "work",
                    "--host",
                    "bitbucket.org",
                    "--path",
                    "workspace/repo.git",
                    "get",
                ]),
            "protocol=https\nhost=bitbucket.org\npath=workspace/repo.git\n\n",
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains(&format!("username={expected}\n")));
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn scoped_git_helper_resets_ambient_credentials_and_does_not_fall_back() {
    let root = fixture();
    let repo = root.join("repo");
    init_repo(&repo, "app");
    git(
        &repo,
        [
            "config",
            "credential.helper",
            "!f() { echo username=ambient; echo password=ambient-secret; }; f",
        ],
    );
    let executable = env!("CARGO_BIN_EXE_knit").replace('\'', "'\\''");
    let helper = format!("credential.https://github.com/org/repo.git.helper=!'{}' auth git-credential --credential work --host github.com --path org/repo.git", executable);
    let run = |request: &str, token: &str| {
        run_with_input(
            Command::new("git")
                .current_dir(&repo)
                .env("GIT_CONFIG_GLOBAL", root.join("empty-gitconfig"))
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("KNIT_HOME", &root)
                .env("KNIT_TEST_PROJECT_TOKEN", token)
                .env("GIT_TERMINAL_PROMPT", "0")
                .args([
                    "-c",
                    "credential.useHttpPath=true",
                    "-c",
                    "core.askPass=",
                    "-c",
                    "credential.https://github.com/org/repo.git.helper=",
                    "-c",
                    &helper,
                    "credential",
                    "fill",
                ]),
            request,
        )
    };
    let request = "protocol=https\nhost=github.com\npath=org/repo.git\n\n";
    let result = run(request, "scoped-test-secret");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stdout).contains("password=scoped-test-secret"));
    let result = run(request, "");
    assert!(!result.status.success());
    assert!(!String::from_utf8_lossy(&result.stdout).contains("ambient-secret"));
    let result = run(
        "protocol=https\nhost=github.com\npath=org/other.git\n\n",
        "scoped-test-secret",
    );
    assert!(result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains("password=ambient-secret"));
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn project_access_check_rewrites_ssh_without_exposing_or_persisting_token() {
    use std::os::unix::fs::PermissionsExt;
    let root = fixture();
    let workspace = root.join("workspace");
    let repo = workspace.join("app");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&repo, "app");
    git(
        &repo,
        ["remote", "add", "origin", "git@github.com:org/repo.git"],
    );
    let invoke = |args: &[&str], path: Option<&str>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_knit"));
        command
            .current_dir(&workspace)
            .env("KNIT_HOME", &root)
            .env("KNIT_TEST_PROJECT_TOKEN", "scoped-test-secret")
            .env("GIT_CONFIG_GLOBAL", root.join("empty-gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("KNIT_BUNDLE")
            .env_remove("KNIT_SESSION")
            .args(args);
        if let Some(path) = path {
            command
                .env("PATH", path)
                .env("GIT_TRACE_CURL", root.join("unsafe-curl-trace"))
                .env("GIT_CURL_VERBOSE", "1")
                .env("GIT_TRACE_REDACT", "0");
        }
        command.output().unwrap()
    };
    for args in [
        vec!["init", "demo"],
        vec![
            "project",
            "add",
            "app",
            repo.to_str().unwrap(),
            "--base",
            "main",
        ],
        vec!["auth", "use", "work", "--repo", "app"],
    ] {
        let result = invoke(&args, None);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let original_config = fs::read(repo.join(".git/config")).unwrap();
    let git_path = Command::new("/bin/sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let git_path = String::from_utf8(git_path.stdout).unwrap();
    let fake_bin = root.join("bin");
    fs::create_dir_all(&fake_bin).unwrap();
    let script = format!(
        r#"#!/bin/sh
network=
helper=
for arg in "$@"; do
  case "$arg" in
    ls-remote) network=1 ;;
    credential.*.helper=!*) helper=${{arg#*=!}} ;;
  esac
done
if [ -n "$network" ]; then
  printf '%s\n' "$@" > '{root}/git-args'
  printf '%s' "${{KNIT_GIT_AUTH_HEADER_0-unset}}" > '{root}/git-auth-header'
  printf '%s|%s|%s' "${{GIT_TRACE_CURL-unset}}" "${{GIT_CURL_VERBOSE-unset}}" "${{GIT_TRACE_REDACT-unset}}" > '{root}/git-env'
  printf 'protocol=https\nhost=github.com\npath=org/repo.git\n\n' | /bin/sh -c "$helper get" > '{root}/helper-result'
  printf 'test server rejected scoped-test-secret Authorization: Basic eC1hY2Nlc3MtdG9rZW46c2NvcGVkLXRlc3Qtc2VjcmV0\n' >&2
  exit 1
fi
exec '{git}' "$@"
"#,
        root = root.display(),
        git = git_path.trim()
    );
    let fake_git = fake_bin.join("git");
    fs::write(&fake_git, script).unwrap();
    fs::set_permissions(&fake_git, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", fake_bin.display(), std::env::var("PATH").unwrap());
    let result = invoke(&["auth", "status", "--check"], Some(&path));
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!result.status.success());
    assert!(!output.contains("scoped-test-secret"));
    let args = fs::read_to_string(root.join("git-args")).unwrap();
    assert!(!args.contains("scoped-test-secret"));
    assert!(args.contains(
        "--config-env=http.https://github.com/org/repo.git.extraHeader=KNIT_GIT_AUTH_HEADER_0"
    ));
    assert_eq!(
        fs::read_to_string(root.join("git-auth-header")).unwrap(),
        "Authorization: Basic eC1hY2Nlc3MtdG9rZW46c2NvcGVkLXRlc3Qtc2VjcmV0"
    );
    assert!(
        args.contains("url.https://github.com/org/repo.git.insteadOf=git@github.com:org/repo.git")
    );
    assert!(args.contains("credential.https://github.com/org/repo.git.helper="));
    assert!(args.contains("http.https://github.com/org/repo.git.extraHeader="));
    assert_eq!(
        fs::read_to_string(root.join("git-env")).unwrap(),
        "unset|unset|1"
    );
    assert!(fs::read_to_string(root.join("helper-result"))
        .unwrap()
        .contains("password=scoped-test-secret"));
    assert_eq!(fs::read(repo.join(".git/config")).unwrap(), original_config);
    let result = invoke(&["bundle", "auth-check", "--offline"], None);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let result = invoke(
        &[
            "--bundle",
            "auth-check",
            "git",
            "--all",
            "ls-remote",
            "origin",
            "HEAD",
        ],
        Some(&path),
    );
    assert!(!result.status.success());
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!output.contains("scoped-test-secret"));
    assert!(output.contains("[REDACTED]"), "{output}");
    assert!(!output.contains("eC1hY2Nlc3MtdG9rZW46c2NvcGVkLXRlc3Qtc2VjcmV0"));
    // A passthrough location override must not pick the original checkout's
    // token for a different Git repository.
    for location in [repo.to_str().unwrap(), root.to_str().unwrap()] {
        let result = invoke(
            &[
                "--bundle",
                "auth-check",
                "git",
                "--all",
                "--",
                "-C",
                location,
                "fetch",
                "origin",
            ],
            Some(&path),
        );
        let error = String::from_utf8_lossy(&result.stderr);
        assert!(!result.status.success());
        assert!(error.contains("repository location overrides"), "{error}");
    }
    // Removing the assignments restores the legacy passthrough behavior.
    let result = invoke(&["auth", "clear"], None);
    assert!(result.status.success());
    let result = invoke(
        &[
            "--bundle",
            "auth-check",
            "git",
            "--all",
            "--",
            "-C",
            repo.to_str().unwrap(),
            "fetch",
            "--dry-run",
            ".",
            "HEAD",
        ],
        None,
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn project_auth_real_https_reads_pushes_and_isolation() {
    let output = Command::new("python3")
        .arg("-I")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/auth_git_https.py"
        ))
        .arg(env!("CARGO_BIN_EXE_knit"))
        .output()
        .expect("HTTPS auth coverage requires Python 3.9+ (python3), openssl, and Git with http-backend on Unix CI");
    assert!(
        output.status.success(),
        "HTTPS auth fixture failed (requires Python 3.9+, openssl req -addext, and Git http-backend):\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

// --- Plain-Git (no Knit wrapper) credential integration --------------------
// The dynamic helper (`auth git-credential --resolve`), the installer reached
// through `auth status` and the auth/worktree hooks, and real `git credential
// fill` from source checkouts and linked bundle worktrees. All credentials
// are synthetic; KNIT_HOME and GIT_CONFIG_GLOBAL are isolated per test.

struct PlainGitHarness {
    root: std::path::PathBuf,
    workspace: std::path::PathBuf,
}

/// An ambient global helper that would answer any host with a wrong
/// credential: if Knit's host-scoped reset ever fails, this leaks into output.
const AMBIENT_HELPER: &str = "!f() { echo username=ambient; echo password=ambient-wrong; }; f";

fn plain_git_harness() -> PlainGitHarness {
    let root = unique_temp_dir();
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(
        root.join("ambient-gitconfig"),
        format!(
            "[user]\n\tname = Fixture\n\temail = fixture@example.invalid\n\
             [credential]\n\thelper = \"{AMBIENT_HELPER}\"\n"
        ),
    )
    .unwrap();
    let harness = PlainGitHarness { root, workspace };
    harness.knit(&harness.workspace, &["init", "demo"]);
    harness
}

impl PlainGitHarness {
    fn knit(&self, cwd: &Path, args: &[&str]) -> Output {
        let output = Command::new(env!("CARGO_BIN_EXE_knit"))
            .current_dir(cwd)
            .env("KNIT_HOME", self.root.join("home"))
            .env("GIT_CONFIG_GLOBAL", self.root.join("ambient-gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("KNIT_ADVICE", "false")
            .env_remove("KNIT_BUNDLE")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "knit {:?} failed in {}:\n{}\n{}",
            args,
            cwd.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn knit_stdin(&self, cwd: &Path, args: &[&str], input: &str) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_knit"));
        command
            .current_dir(cwd)
            .env("KNIT_HOME", self.root.join("home"))
            .env("GIT_CONFIG_GLOBAL", self.root.join("ambient-gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("KNIT_ADVICE", "false")
            .env_remove("KNIT_BUNDLE")
            .args(args);
        run_with_input(&mut command, input)
    }

    /// Plain `git credential fill` inside a checkout, with an ambient helper
    /// appended *after* the repository's config (command scope) so a quit
    /// from the Knit helper must stop even later helpers.
    fn fill(&self, checkout: &Path, request: &str, trailing_ambient: bool) -> Output {
        let mut command = Command::new("git");
        command
            .current_dir(checkout)
            .env("GIT_CONFIG_GLOBAL", self.root.join("ambient-gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("KNIT_HOME", self.root.join("home"))
            .env("KNIT_TEST_PROJECT_TOKEN", "scoped-test-secret")
            .env("KNIT_TEST_ROTATED_TOKEN", "rotated-test-secret");
        if trailing_ambient {
            command
                .arg("-c")
                .arg(format!("credential.helper={AMBIENT_HELPER}"))
                .arg("-c")
                .arg(format!(
                    "credential.https://github.com/.helper={AMBIENT_HELPER}"
                ));
        }
        run_with_input(command.arg("credential").arg("fill"), request)
    }

    fn forge_auth(&self) -> serde_json::Value {
        serde_json::from_str(
            &fs::read_to_string(self.root.join("home/forge-auth.json"))
                .unwrap_or_else(|_| "{}".to_owned()),
        )
        .unwrap_or(json!({}))
    }

    fn write_forge_auth(&self, value: &serde_json::Value) {
        fs::create_dir_all(self.root.join("home")).unwrap();
        fs::write(
            self.root.join("home/forge-auth.json"),
            serde_json::to_vec_pretty(value).unwrap(),
        )
        .unwrap();
    }
}

fn add_project_checkout(harness: &PlainGitHarness, id: &str, remote: &str) -> std::path::PathBuf {
    let repo = harness.workspace.join(id);
    init_repo(&repo, id);
    git(&repo, ["remote", "add", "origin", remote]);
    harness.knit(
        &harness.workspace,
        &[
            "project",
            "add",
            id,
            repo.to_str().unwrap(),
            "--base",
            "main",
        ],
    );
    repo
}

#[test]
fn dynamic_helper_serves_resolved_selection_and_fails_closed() {
    let harness = plain_git_harness();
    let repo = add_project_checkout(&harness, "app", "https://github.com/org/app.git");
    // A sole GitHub credential becomes the host default; the repository has
    // no assignment, so the helper resolves the default dynamically.
    harness.knit_stdin(
        &harness.workspace,
        &[
            "auth",
            "add",
            "gh",
            "--provider",
            "github",
            "--host",
            "github.com",
            "--token-stdin",
        ],
        "default-token",
    );
    let helper = |request: &str| {
        run_with_input(
            Command::new(env!("CARGO_BIN_EXE_knit"))
                .current_dir(&repo)
                .env("KNIT_HOME", harness.root.join("home"))
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args(["auth", "git-credential", "--resolve", "get"]),
            request,
        )
    };
    let request = "protocol=https\nhost=github.com\npath=org/app.git\n\n";
    let output = helper(request);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "username=x-access-token\npassword=default-token\n\n"
    );

    // A project override wins over the host default, and rotating the saved
    // token serves the new secret with no regeneration step.
    let _ = harness.knit_stdin(
        &harness.workspace,
        &[
            "auth",
            "add",
            "override",
            "--provider",
            "github",
            "--host",
            "github.com",
            "--token-stdin",
        ],
        "override-token",
    );
    harness.knit(
        &harness.workspace,
        &["auth", "use", "override", "--repo", "app"],
    );
    let output = helper(request);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "username=x-access-token\npassword=override-token\n\n"
    );

    // Malformed or confused requests fail closed: `quit=1` and a reason on
    // stderr, never a credential, and a zero exit so Git honors the quit.
    for bad in [
        // duplicate scope scalar
        "protocol=https\nhost=github.com\nhost=github.com\npath=org/app.git\n\n",
        // non-HTTPS scheme
        "protocol=http\nhost=github.com\npath=org/app.git\n\n",
        // raw host with a port
        "protocol=https\nhost=github.com:443\npath=org/app.git\n\n",
        // embedded credentials smuggled through the host field
        "protocol=https\nhost=user:secret@github.com\npath=org/app.git\n\n",
        // missing path: exact matching is impossible
        "protocol=https\nhost=github.com\n\n",
        // a rewritten url field must not be split into parts
        "protocol=https\nhost=github.com\npath=org/app.git\nurl=https://evil.test/org/app.git\n\n",
        // traversal in the path
        "protocol=https\nhost=github.com\npath=org/../app.git\n\n",
    ] {
        let output = helper(bad);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stdout.contains("quit=1"), "{bad:?}: {stdout}");
        assert!(!stdout.contains("password="), "{bad:?}: {stdout}");
        assert!(!stderr.is_empty(), "{bad:?}: expected a reason on stderr");
        assert!(
            output.status.success(),
            "{bad:?}: helper must exit cleanly after quit so Git honors it"
        );
    }

    // store/erase never touch personal token storage.
    for operation in ["store", "erase"] {
        let output = run_with_input(
            Command::new(env!("CARGO_BIN_EXE_knit"))
                .current_dir(&repo)
                .env("KNIT_HOME", harness.root.join("home"))
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args(["auth", "git-credential", "--resolve", operation]),
            request,
        );
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
    }
    fs::remove_dir_all(harness.root).unwrap();
}

#[test]
fn plain_git_fill_resolves_default_override_and_rotation() {
    let harness = plain_git_harness();
    let app = add_project_checkout(&harness, "app", "https://github.com/org/app.git");
    let lib = add_project_checkout(&harness, "lib", "https://gitlab.com/org/lib.git");
    harness.knit_stdin(
        &harness.workspace,
        &[
            "auth",
            "add",
            "gh",
            "--provider",
            "github",
            "--host",
            "github.com",
            "--token-stdin",
        ],
        "first-token",
    );
    // Activation for an existing install: noninteractive, saved credentials.
    // (Status wording is production's; the include assertions below carry
    // the behavioral weight.)
    harness.knit(&harness.workspace, &["auth", "status"]);

    // Generated entries live in an owned include file, never in the local
    // config itself; unrelated local configuration is preserved. Scope key
    // shapes are production's choice — assert the helper and the absence of
    // secrets, then behavior through real Git below.
    let include = app.join(".git").join("knit-credentials.inc");
    let content = fs::read_to_string(&include).unwrap();
    assert!(
        content.contains("auth git-credential --resolve"),
        "{content}"
    );
    assert!(!content.contains("first-token"), "{content}");
    let local_config = fs::read_to_string(app.join(".git/config")).unwrap();
    // Mechanism (plain include vs conditional includeIf) is production's
    // choice; the owned file must be referenced and hold the policy.
    assert!(
        local_config.contains("knit-credentials.inc"),
        "{local_config}"
    );
    // `git config --get-regexp` exits 1 on no match, so inspect directly.
    let local_credential_keys = Command::new("git")
        .current_dir(&app)
        .env("GIT_CONFIG_GLOBAL", harness.root.join("ambient-gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["config", "--local", "--get-regexp", r"^credential\."])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&local_credential_keys.stdout).trim(),
        "",
        "generated credential policy must stay in the owned include file"
    );
    // A repository whose host has no Knit credential keeps inherited config.
    assert!(!lib.join(".git").join("knit-credentials.inc").exists());
    assert!(!fs::read_to_string(lib.join(".git/config"))
        .unwrap()
        .contains("knit-credentials"));

    // Plain Git fill: the GitHub host uses Knit's default (ambient never
    // answers), while the GitLab host — untouched by Knit — keeps ambient.
    let github = "protocol=https\nhost=github.com\npath=org/app.git\n\n";
    let output = harness.fill(&app, github, true);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "protocol=https\nhost=github.com\npath=org/app.git\nusername=x-access-token\npassword=first-token\n"
    );
    let gitlab = "protocol=https\nhost=gitlab.com\npath=org/lib.git\n\n";
    let output = harness.fill(&lib, gitlab, false);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("username=ambient"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("password=ambient-wrong"));

    // Idempotence: a second activation rewrites nothing.
    let before = fs::read_to_string(&include).unwrap();
    harness.knit(&harness.workspace, &["auth", "status"]);
    assert_eq!(fs::read_to_string(&include).unwrap(), before);

    // Token rotation reaches plain Git immediately (dynamic resolution).
    harness.knit_stdin(
        &harness.workspace,
        &[
            "auth",
            "add",
            "gh",
            "--provider",
            "github",
            "--host",
            "github.com",
            "--replace",
            "--token-stdin",
        ],
        "second-token",
    );
    let output = harness.fill(&app, github, true);
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("password=second-token"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );

    // Removing the last credential (host default and no assignment) uninstalls
    // the generated entries: ambient behavior is fully restored.
    harness.knit(&harness.workspace, &["auth", "clear"]);
    let mut registry = harness.forge_auth();
    registry["credentials"]
        .as_object_mut()
        .unwrap()
        .remove("gh");
    if let Some(defaults) = registry.get_mut("defaults").and_then(|d| d.as_object_mut()) {
        defaults.remove("github.com");
    }
    harness.write_forge_auth(&registry);
    harness.knit(&harness.workspace, &["auth", "status"]);
    assert!(!include.exists());
    assert!(!fs::read_to_string(app.join(".git/config"))
        .unwrap()
        .contains("knit-credentials"));
    let output = harness.fill(&app, github, false);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("password=ambient-wrong"));
    fs::remove_dir_all(harness.root).unwrap();
}

#[test]
fn plain_git_fail_closed_missing_token_and_missing_assignment() {
    let harness = plain_git_harness();
    let app = add_project_checkout(&harness, "app", "https://github.com/org/app.git");
    // Two GitHub credentials keep the host default-less; an explicit
    // assignment is the only selection that can answer.
    for (name, variable) in [
        ("work", "KNIT_TEST_PROJECT_TOKEN"),
        ("spare", "KNIT_TEST_ROTATED_TOKEN"),
    ] {
        let mut registry = harness.forge_auth();
        registry["credentials"][name] = json!({
            "provider": "github",
            "host": "github.com",
            "tokenEnv": variable
        });
        harness.write_forge_auth(&registry);
    }
    fs::write(
        harness.root.join("home/forge-secrets.json"),
        serde_json::to_vec(&json!({"work": "work-secret"})).unwrap(),
    )
    .unwrap();
    harness.knit(
        &harness.workspace,
        &["auth", "use", "work", "--repo", "app"],
    );
    let github = "protocol=https\nhost=github.com\npath=org/app.git\n\n";

    // A second, unassigned repository on the same host fails closed: the
    // strict project gate refuses, quit stops Git, ambient never answers.
    let other = add_project_checkout(&harness, "other", "https://github.com/org/other.git");
    let other_request = "protocol=https\nhost=github.com\npath=org/other.git\n\n";
    let output = harness.fill(&other, other_request, true);
    assert!(!output.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!text.contains("ambient-wrong"), "{text}");
    assert!(!text.contains("work-secret"), "{text}");

    // A missing explicit token (binding present, environment unset) is the
    // same refusal at request time, with no ambient fallthrough.
    let mut broken_env = Command::new("git");
    broken_env
        .current_dir(&app)
        .env("GIT_CONFIG_GLOBAL", harness.root.join("ambient-gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("KNIT_HOME", harness.root.join("home"))
        .env_remove("KNIT_TEST_PROJECT_TOKEN")
        .args([
            "-c",
            &format!("credential.helper={AMBIENT_HELPER}"),
            "credential",
            "fill",
        ]);
    let output = run_with_input(&mut broken_env, github);
    assert!(!output.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!text.contains("ambient-wrong"), "{text}");
    fs::remove_dir_all(harness.root).unwrap();
}

#[cfg(unix)]
#[test]
fn linked_worktree_plain_git_fill_resolves_per_worktree_context() {
    let harness = plain_git_harness();
    let app = add_project_checkout(&harness, "app", "https://github.com/org/app.git");
    // Two credentials with saved secrets (no environment reference), explicit
    // assignments: worktrees resolve per context.
    let mut registry = json!({"credentials": {}});
    let mut secrets = json!({});
    for (name, token) in [("work", "work-secret"), ("other", "other-secret")] {
        registry["credentials"][name] = json!({
            "provider": "github",
            "host": "github.com"
        });
        secrets[name] = json!(token);
    }
    harness.write_forge_auth(&registry);
    fs::write(
        harness.root.join("home/forge-secrets.json"),
        serde_json::to_vec(&secrets).unwrap(),
    )
    .unwrap();
    harness.knit(
        &harness.workspace,
        &["auth", "use", "work", "--repo", "app"],
    );
    // Bundle creation materializes a linked worktree; the automatic hook
    // installs the plain-Git helper for it (per worktree git dir or shared —
    // placement is production's choice; assert the effective config).
    harness.knit(&harness.workspace, &["bundle", "wt-fill", "--offline"]);
    let worktree = harness.workspace.join(".knit/worktrees/wt-fill/app");
    assert!(worktree.is_dir());
    let effective = git(
        &worktree,
        ["config", "--show-origin", "--get-regexp", r"^credential\."],
    );
    assert!(
        effective.contains("knit-credentials.inc"),
        "worktree must carry the generated include: {effective}"
    );
    assert!(
        effective.contains("auth git-credential --resolve"),
        "{effective}"
    );
    assert!(app.join(".git").join("knit-credentials.inc").exists());

    // Plain `git credential fill` inside the linked worktree resolves the
    // project assignment from the worktree's own context.
    let request = "protocol=https\nhost=github.com\npath=org/app.git\n\n";
    let output = harness.fill(&worktree, request, true);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "protocol=https\nhost=github.com\npath=org/app.git\nusername=x-access-token\npassword=work-secret\n"
    );

    // Switching the assignment and refreshing changes what plain Git gets,
    // without rewriting the worktree or its remotes.
    harness.knit(
        &harness.workspace,
        &["auth", "use", "other", "--repo", "app"],
    );
    let output = harness.fill(&worktree, request, true);
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("password=other-secret"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    // The saved remote URL and unrelated local config are untouched.
    assert_eq!(
        git(&worktree, ["remote", "get-url", "origin"]).trim(),
        "https://github.com/org/app.git"
    );
    fs::remove_dir_all(harness.root).unwrap();
}

#[test]
fn same_host_ambient_remote_survives_selection_takeover() {
    // Regression: one checkout whose origin is covered by a project-scoped
    // token, plus a second remote on the SAME host with a recorded ambient
    // allowance and no host default. Taking over the selected repository
    // must not kill the sibling remote's inherited ambient credentials.
    let harness = plain_git_harness();
    let checkout = add_project_checkout(&harness, "app", "https://github.com/org/a.git");
    git(
        &checkout,
        ["remote", "add", "fork", "https://github.com/org/b.git"],
    );
    // The project knows both repositories; only `app` is bound.
    let project_path = harness.workspace.join(".knit/projects/demo.project.json");
    let mut project: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&project_path).unwrap()).unwrap();
    project["repos"].as_array_mut().unwrap().push(json!({
        "id": "fork", "path": "fork",
        "remote": "https://github.com/org/b.git", "baseBranch": "main"
    }));
    fs::write(&project_path, serde_json::to_vec(&project).unwrap()).unwrap();
    // Registry: binding app -> selected (project-scoped, so it can never be
    // the host default), and an ambient allowance for the fork remote's
    // exact target.
    let key = fs::canonicalize(&project_path)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let registry = json!({
        "credentials": {"selected": {"provider": "github", "host": "github.com"}},
        "scopedCredentials": ["selected"],
        "projects": {key.clone(): {"app": "selected"}},
        "ambient": {key: {"fork": "github.com/org/b"}}
    });
    harness.write_forge_auth(&registry);
    fs::write(
        harness.root.join("home/forge-secrets.json"),
        serde_json::to_vec(&json!({"selected": "selected-secret"})).unwrap(),
    )
    .unwrap();
    harness.knit(&harness.workspace, &["auth", "status"]);

    // The selected repository resolves through Knit even with a trailing
    // ambient helper; the ambient-allowed sibling on the same host keeps
    // inherited credentials (an ambient helper still answers it).
    let selected = harness.fill(
        &checkout,
        "protocol=https\nhost=github.com\npath=org/a.git\n\n",
        true,
    );
    assert!(
        selected.status.success(),
        "{}",
        String::from_utf8_lossy(&selected.stderr)
    );
    let text = String::from_utf8_lossy(&selected.stdout);
    assert!(text.contains("password=selected-secret"), "{text}");
    assert!(!text.contains("ambient-wrong"), "{text}");
    let ambient = harness.fill(
        &checkout,
        "protocol=https\nhost=github.com\npath=org/b.git\n\n",
        false,
    );
    assert!(
        ambient.status.success(),
        "{}",
        String::from_utf8_lossy(&ambient.stderr)
    );
    let text = String::from_utf8_lossy(&ambient.stdout);
    assert!(text.contains("password=ambient-wrong"), "{text}");
    fs::remove_dir_all(harness.root).unwrap();
}

/// An external source checkout outside any Knit workspace still resolves its
/// registered project's override — not the host default — for plain Git in
/// the checkout itself and in a linked bundle worktree.
#[cfg(unix)]
#[test]
fn external_checkout_resolves_project_override_over_default() {
    let harness = plain_git_harness();
    let external = harness.root.join("external").join("app");
    init_repo(&external, "app");
    git(
        &external,
        ["remote", "add", "origin", "https://github.com/org/app.git"],
    );
    // A cached origin/main base so `bundle --offline` needs no network.
    git(
        &external,
        ["update-ref", "refs/remotes/origin/main", "main"],
    );
    harness.knit(
        &harness.workspace,
        &[
            "project",
            "add",
            "app",
            external.to_str().unwrap(),
            "--base",
            "main",
        ],
    );
    // Host default (`global`, the sole credential when added) plus a project
    // override (`restricted`) for the external repository.
    harness.knit_stdin(
        &harness.workspace,
        &[
            "auth",
            "add",
            "global",
            "--provider",
            "github",
            "--token-stdin",
        ],
        "synthetic-global",
    );
    harness.knit_stdin(
        &harness.workspace,
        &[
            "auth",
            "add",
            "restricted",
            "--provider",
            "github",
            "--token-stdin",
        ],
        "synthetic-restricted",
    );
    harness.knit(
        &harness.workspace,
        &["auth", "use", "restricted", "--repo", "app"],
    );
    let request = "protocol=https\nhost=github.com\npath=org/app.git\n\n";
    let output = harness.fill(&external, request, true);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("password=synthetic-restricted"), "{text}");
    assert!(!text.contains("synthetic-global"), "{text}");
    assert!(!text.contains("ambient-wrong"), "{text}");

    // The linked worktree materialized from the external repository keeps
    // the same override.
    harness.knit(&harness.workspace, &["bundle", "ext-override", "--offline"]);
    let worktree = harness.workspace.join(".knit/worktrees/ext-override/app");
    assert!(worktree.is_dir());
    let output = harness.fill(&worktree, request, true);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("password=synthetic-restricted"));
    fs::remove_dir_all(harness.root).unwrap();
}

/// Registering the same external repository in a second project — with no
/// host default and the second context ambient — must never disturb the
/// first project's linked-worktree helper. Rotating the first project's
/// token updates only the first project.
#[cfg(unix)]
#[test]
fn second_project_cannot_disturb_first_worktree_credentials() {
    let harness = plain_git_harness();
    let workspace_b = harness.root.join("workspace-b");
    fs::create_dir_all(&workspace_b).unwrap();
    harness.knit(&workspace_b, &["init", "projb"]);
    let external = harness.root.join("external").join("app");
    init_repo(&external, "app");
    git(
        &external,
        ["remote", "add", "origin", "https://github.com/org/app.git"],
    );
    git(
        &external,
        ["update-ref", "refs/remotes/origin/main", "main"],
    );
    let project_a = harness.workspace.join(".knit/projects/demo.project.json");
    let project_b = workspace_b.join(".knit/projects/projb.project.json");
    harness.knit(
        &harness.workspace,
        &[
            "project",
            "add",
            "app",
            external.to_str().unwrap(),
            "--base",
            "main",
        ],
    );
    // All credentials project-scoped, so no host default can ever answer.
    // Project A binds the repository; project B holds a recorded ambient
    // allowance for the same remote.
    let key_a = fs::canonicalize(&project_a)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let key_b = fs::canonicalize(&project_b)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    harness.write_forge_auth(&json!({
        "credentials": {"restricted": {"provider": "github", "host": "github.com"}},
        "scopedCredentials": ["restricted"],
        "projects": {key_a.clone(): {"app": "restricted"}},
        "ambient": {key_b.clone(): {"app": "github.com/org/app"}}
    }));
    fs::write(
        harness.root.join("home/forge-secrets.json"),
        serde_json::to_vec(&json!({"restricted": "synthetic-restricted"})).unwrap(),
    )
    .unwrap();
    harness.knit(&harness.workspace, &["auth", "status"]);
    harness.knit(&harness.workspace, &["bundle", "alpha", "--offline"]);
    let alpha = harness.workspace.join(".knit/worktrees/alpha/app");
    assert!(alpha.is_dir());
    let request = "protocol=https\nhost=github.com\npath=org/app.git\n\n";

    // The second project registers the same external repository (its source
    // association becomes the last one) and refreshes its own context; the
    // first project's linked worktree helper must survive untouched.
    harness.knit(
        &workspace_b,
        &[
            "project",
            "add",
            "app",
            external.to_str().unwrap(),
            "--base",
            "main",
        ],
    );
    harness.knit(&workspace_b, &["auth", "status"]);
    harness.knit(&workspace_b, &["bundle", "beta", "--offline"]);
    let beta = workspace_b.join(".knit/worktrees/beta/app");
    assert!(beta.is_dir());
    let output = harness.fill(&alpha, request, true);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("password=synthetic-restricted"),
        "first project's worktree credential was disturbed: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let output = harness.fill(&beta, request, false);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("password=ambient-wrong"),
        "ambient-allowed second project must keep inherited credentials"
    );

    // A refresh from the first project, then a token rotation for its
    // credential: the first project updates, the second stays ambient.
    harness.knit(&harness.workspace, &["auth", "status"]);
    fs::write(
        harness.root.join("home/forge-secrets.json"),
        serde_json::to_vec(&json!({"restricted": "synthetic-rotated"})).unwrap(),
    )
    .unwrap();
    let output = harness.fill(&alpha, request, true);
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("password=synthetic-rotated"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let output = harness.fill(&beta, request, false);
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("password=ambient-wrong"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    fs::remove_dir_all(harness.root).unwrap();
}
