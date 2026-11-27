mod common;

use common::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

/// Two local repos in one bundle with a recorded commit and a `knit/v1` tag.
/// Returns (root, workspace, backend checkout, frontend checkout).
fn setup_bundle() -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let root = unique_temp_dir();
    let backend = root.join("backend");
    let frontend = root.join("frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    init_repo(&backend, "backend");
    init_repo(&frontend, "frontend");

    knit(&workspace, ["bundle", "pipe reader"]);
    knit(
        &workspace,
        [
            "bundle",
            "add",
            backend.to_str().unwrap(),
            frontend.to_str().unwrap(),
        ],
    );
    let worktrees = workspace.join(".knit/worktrees/pipe-reader");
    let (backend_checkout, frontend_checkout) =
        (worktrees.join("backend"), worktrees.join("frontend"));
    append_line(&backend_checkout.join("app.txt"), "backend change");
    append_line(&frontend_checkout.join("app.txt"), "frontend change");
    knit(&workspace, ["commit", "--all", "-m", "First change"]);
    knit(&workspace, ["tag", "v1", "--no-push"]);
    (root, workspace, backend_checkout, frontend_checkout)
}

/// Run knit with stdout connected to a pipe whose reader closes before knit
/// writes anything, the way `knit log | head -1` looks once `head` has its
/// line. Returns knit's exit status and stderr.
fn knit_into_closed_pipe(cwd: &Path, args: &[&str]) -> (ExitStatus, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_knit"));
    command
        .args(args)
        .current_dir(cwd)
        .env("KNIT_HOME", isolated_knit_home())
        .env("GIT_CONFIG_GLOBAL", isolated_git_config_global())
        .env_remove("KNIT_BUNDLE")
        .env_remove("KNIT_SESSION")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    // Close the only read end: every write knit makes to stdout now fails
    // with a broken pipe.
    drop(child.stdout.take());
    let output = child.wait_with_output().unwrap();
    (
        output.status,
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

#[test]
fn commands_exit_quietly_when_the_reader_closes_stdout() {
    let (root, workspace, _, _) = setup_bundle();

    for args in [
        &["log"][..],
        &["log", "--json"],
        &["status"],
        &["status", "--json"],
        &["tag", "show", "v1"],
        &["bundle", "list"],
        &["--help"],
    ] {
        let (status, stderr) = knit_into_closed_pipe(&workspace, args);
        assert!(
            !stderr.contains("panicked") && !stderr.contains("Broken pipe"),
            "knit {args:?} into a closed pipe printed:\n{stderr}"
        );
        assert!(stderr.is_empty(), "knit {args:?} stderr:\n{stderr}");
        assert!(status.success(), "knit {args:?} exited {status}");
    }

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn commit_across_repos_finishes_recording_when_the_reader_closes_stdout() {
    // The reader going away must not stop a multi-repo mutation between the
    // git commits and the bundle ledger write: both repos end up committed
    // and the commit group recorded, exactly as with a live reader.
    let (root, workspace, backend, frontend) = setup_bundle();
    append_line(&backend.join("app.txt"), "second backend change");
    append_line(&frontend.join("app.txt"), "second frontend change");

    let (status, stderr) =
        knit_into_closed_pipe(&workspace, &["commit", "--all", "-m", "Reader gone"]);
    assert!(stderr.is_empty(), "{stderr}");
    assert!(status.success(), "knit commit exited {status}");

    for checkout in [&backend, &frontend] {
        assert_eq!(
            git(checkout, ["log", "-1", "--format=%s"]).trim(),
            "Reader gone"
        );
    }
    let log = knit(&workspace, ["log"]);
    assert!(log.contains("Reader gone"), "{log}");
    let status = knit(&workspace, ["status"]);
    assert!(!status.contains("unrecorded"), "{status}");

    fs::remove_dir_all(root).unwrap();
}
