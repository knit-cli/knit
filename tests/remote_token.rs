//! Remote token entry: terminal hidden prompts, piped stdin replacement, and
//! the guardrails around them (scope, conflicts, no clobber on empty input).

mod common;

use common::{knit_fails_with_env, knit_with_env, unique_temp_dir};
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn read_global_config(knit_home: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(knit_home.join("config.json")).unwrap()).unwrap()
}

fn stored_token(knit_home: &Path, remote: &str) -> Option<String> {
    read_global_config(knit_home)["remotes"][remote]["token"]
        .as_str()
        .map(ToString::to_string)
}

/// Run the binary with an explicit stdin: `Some` pipes the text and closes
/// (EOF-terminated, like a secret manager); `None` leaves stdin at the null
/// device, which is never a terminal, for fail-fast assertions.
fn run_with_stdin(cwd: &Path, args: &[&str], knit_home: &Path, stdin: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_knit"));
    command
        .args(args)
        .current_dir(cwd)
        .env("KNIT_HOME", knit_home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match stdin {
        Some(text) => {
            let mut child = command.stdin(Stdio::piped()).spawn().unwrap();
            // The child may reject the token before reading (guard errors);
            // a broken pipe here is fine, the exit status is what matters.
            let _ = child.stdin.take().unwrap().write_all(text.as_bytes());
            child.wait_with_output().unwrap()
        }
        None => command.stdin(Stdio::null()).output().unwrap(),
    }
}

#[test]
fn remote_token_stdin_replaces_stores_privately_and_prints_safe_guidance() {
    let root = unique_temp_dir();
    let outside = root.join("outside");
    let knit_home = root.join("knit-home");
    fs::create_dir_all(&outside).unwrap();
    fs::create_dir_all(&knit_home).unwrap();
    let env = [("KNIT_HOME", knit_home.to_str().unwrap())];
    let config_path = knit_home.join("config.json");

    let add = knit_with_env(
        &outside,
        [
            "remote",
            "add",
            "hosted",
            "https://host.example",
            "--global",
            "--token",
            "first-secret",
        ],
        &env,
    );
    assert!(add.contains("configured global hosted"), "{add}");
    assert!(add.contains("Token saved to:"), "{add}");
    assert!(
        add.contains(config_path.to_str().unwrap()),
        "guidance must name the actual config path: {add}"
    );
    assert!(
        add.contains("Authentication has not been checked."),
        "{add}"
    );
    assert!(add.contains("knit remote auth-status hosted"), "{add}");
    assert!(add.contains("knit remote token hosted --global"), "{add}");
    assert!(
        !add.contains("KNIT_REMOTE_HOSTED_TOKEN"),
        "no env override is active, guidance must not mention one: {add}"
    );
    assert!(!add.contains("first-secret"), "{add}");
    assert_eq!(
        stored_token(&knit_home, "hosted").as_deref(),
        Some("first-secret")
    );

    // Piped `remote add --token-stdin` keeps the bounded EOF read.
    let piped = run_with_stdin(
        &outside,
        &[
            "remote",
            "add",
            "second",
            "https://host.example",
            "--global",
            "--token-stdin",
        ],
        &knit_home,
        Some("piped-add-secret\n"),
    );
    let piped_out = format!(
        "{}{}",
        String::from_utf8_lossy(&piped.stdout),
        String::from_utf8_lossy(&piped.stderr)
    );
    assert!(piped.status.success(), "{piped_out}");
    assert_eq!(
        stored_token(&knit_home, "second").as_deref(),
        Some("piped-add-secret")
    );
    assert!(!piped_out.contains("piped-add-secret"), "{piped_out}");

    // The clean replacement path: pipe a new token into `remote token`.
    let replace = run_with_stdin(
        &outside,
        &["remote", "token", "hosted", "--global", "--token-stdin"],
        &knit_home,
        Some("second-secret\n"),
    );
    let replace_out = format!(
        "{}{}",
        String::from_utf8_lossy(&replace.stdout),
        String::from_utf8_lossy(&replace.stderr)
    );
    assert!(replace.status.success(), "{replace_out}");
    assert!(replace_out.contains("stored hosted"), "{replace_out}");
    assert!(
        replace_out.contains(config_path.to_str().unwrap()),
        "{replace_out}"
    );
    assert!(
        replace_out.contains("Authentication has not been checked."),
        "{replace_out}"
    );
    assert!(
        replace_out.contains("knit remote auth-status hosted"),
        "{replace_out}"
    );
    assert!(!replace_out.contains("second-secret"), "{replace_out}");
    assert_eq!(
        stored_token(&knit_home, "hosted").as_deref(),
        Some("second-secret")
    );

    // An active environment override is named (never its value); without one
    // the guidance stays silent about env vars, checked above.
    let mut override_command = Command::new(env!("CARGO_BIN_EXE_knit"));
    override_command
        .args(["remote", "token", "hosted", "--global", "--token-stdin"])
        .current_dir(&outside)
        .env("KNIT_HOME", &knit_home)
        .env("KNIT_REMOTE_HOSTED_TOKEN", "env-override-secret")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = override_command.spawn().unwrap();
    let _ = child.stdin.take().unwrap().write_all(b"third-secret\n");
    let with_override = child.wait_with_output().unwrap();
    let with_override_out = format!(
        "{}{}",
        String::from_utf8_lossy(&with_override.stdout),
        String::from_utf8_lossy(&with_override.stderr)
    );
    assert!(with_override.status.success(), "{with_override_out}");
    assert!(
        with_override_out.contains("KNIT_REMOTE_HOSTED_TOKEN overrides the stored token"),
        "{with_override_out}"
    );
    assert!(
        !with_override_out.contains("env-override-secret"),
        "{with_override_out}"
    );
    assert!(
        !with_override_out.contains("third-secret"),
        "{with_override_out}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&config_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    // Explicit --clear keeps its historical meaning.
    let cleared = knit_with_env(
        &outside,
        ["remote", "token", "second", "--clear", "--global"],
        &env,
    );
    assert!(cleared.contains("cleared second"), "{cleared}");
    assert_eq!(stored_token(&knit_home, "second"), None);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn remote_token_flag_conflicts_and_scope_guards_fail_before_reading_secrets() {
    let root = unique_temp_dir();
    let outside = root.join("outside");
    let knit_home = root.join("knit-home");
    fs::create_dir_all(&outside).unwrap();
    fs::create_dir_all(&knit_home).unwrap();
    let env = [("KNIT_HOME", knit_home.to_str().unwrap())];
    knit_with_env(
        &outside,
        [
            "remote",
            "add",
            "hosted",
            "https://host.example",
            "--global",
            "--token",
            "kept-secret",
        ],
        &env,
    );

    // --token-stdin refuses to share an invocation with a token or --clear.
    let with_token = knit_fails_with_env(
        &outside,
        [
            "remote",
            "token",
            "hosted",
            "inline-secret",
            "--global",
            "--token-stdin",
        ],
        &env,
    );
    assert!(with_token.contains("cannot be used with"), "{with_token}");
    let with_clear = knit_fails_with_env(
        &outside,
        [
            "remote",
            "token",
            "hosted",
            "--clear",
            "--global",
            "--token-stdin",
        ],
        &env,
    );
    assert!(with_clear.contains("cannot be used with"), "{with_clear}");

    // Hidden entry is per-user: never silently stored in workspace config.
    let scope = knit_fails_with_env(
        &outside,
        ["remote", "token", "hosted", "--token-stdin"],
        &env,
    );
    assert!(scope.contains("--token-stdin requires --global"), "{scope}");

    // The remote is resolved before any secret is collected.
    let missing = run_with_stdin(
        &outside,
        &["remote", "token", "nosuch", "--global", "--token-stdin"],
        &knit_home,
        Some("never-consumed\n"),
    );
    let missing_out = format!(
        "{}{}",
        String::from_utf8_lossy(&missing.stdout),
        String::from_utf8_lossy(&missing.stderr)
    );
    assert!(!missing.status.success(), "{missing_out}");
    assert!(
        missing_out.contains("No remote named `nosuch`"),
        "{missing_out}"
    );

    assert_eq!(
        stored_token(&knit_home, "hosted").as_deref(),
        Some("kept-secret")
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn remote_token_without_value_or_terminal_fails_fast_with_guidance() {
    let root = unique_temp_dir();
    let outside = root.join("outside");
    let knit_home = root.join("knit-home");
    fs::create_dir_all(&outside).unwrap();
    fs::create_dir_all(&knit_home).unwrap();
    let env = [("KNIT_HOME", knit_home.to_str().unwrap())];
    knit_with_env(
        &outside,
        [
            "remote",
            "add",
            "hosted",
            "https://host.example",
            "--global",
            "--token",
            "kept-secret",
        ],
        &env,
    );

    // Null stdin (never a terminal): immediate, actionable error.
    let null_stdin = run_with_stdin(
        &outside,
        &["remote", "token", "hosted", "--global"],
        &knit_home,
        None,
    );
    assert!(!null_stdin.status.success());
    let null_out = format!(
        "{}{}",
        String::from_utf8_lossy(&null_stdin.stdout),
        String::from_utf8_lossy(&null_stdin.stderr)
    );
    assert!(null_out.contains("no token provided"), "{null_out}");
    assert!(null_out.contains("--token-stdin"), "{null_out}");
    assert!(null_out.contains("hidden"), "{null_out}");

    // Piped stdin without --token-stdin must not silently consume the pipe.
    let piped_stdin = run_with_stdin(
        &outside,
        &["remote", "token", "hosted", "--global"],
        &knit_home,
        Some("piped-but-unrequested\n"),
    );
    assert!(!piped_stdin.status.success());

    // An empty piped token is rejected and never overwrites the stored one.
    let empty = run_with_stdin(
        &outside,
        &["remote", "token", "hosted", "--global", "--token-stdin"],
        &knit_home,
        Some("\n"),
    );
    assert!(!empty.status.success());
    let empty_out = format!(
        "{}{}",
        String::from_utf8_lossy(&empty.stdout),
        String::from_utf8_lossy(&empty.stderr)
    );
    assert!(empty_out.contains("empty"), "{empty_out}");

    assert_eq!(
        stored_token(&knit_home, "hosted").as_deref(),
        Some("kept-secret")
    );
    fs::remove_dir_all(root).unwrap();
}

// The fixture drives a real PTY (Unix pty/termios); the rest stay cross-platform.
#[cfg(unix)]
#[test]
fn remote_token_pty_hidden_prompts_enter_completion_and_no_clobber() {
    let root = std::env::temp_dir().join(format!("knit-remote-token-pty-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let result = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/remote_token_pty.py"
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
