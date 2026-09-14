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
