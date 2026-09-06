#![allow(dead_code)]

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn setup_three_repo_project(workspace: &Path, root: &Path) {
    let backend = root.join("backend");
    let frontend = root.join("frontend");
    let docs = root.join("docs");
    fs::create_dir_all(workspace).unwrap();
    init_repo(&backend, "backend");
    init_repo(&frontend, "frontend");
    init_repo(&docs, "docs");
    knit(workspace, ["init", "demo"]);
    knit(
        workspace,
        ["project", "add", "backend", backend.to_str().unwrap()],
    );
    knit(
        workspace,
        ["project", "add", "frontend", frontend.to_str().unwrap()],
    );
    knit(
        workspace,
        [
            "project",
            "add",
            "docs",
            docs.to_str().unwrap(),
            "--observe",
        ],
    );
}

pub fn bundle_repo_ids(workspace: &Path, bundle_id: &str) -> Vec<String> {
    let path = workspace
        .join(".knit/bundles")
        .join(format!("{bundle_id}.bundle.json"));
    let bundle: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    bundle["repos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|repo| repo["id"].as_str().unwrap().to_string())
        .collect()
}

pub fn publish_two_repo_bundle(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let (_backend_remote, backend, _backend_collaborator) = init_remote_repo(root, "backend");
    let (_frontend_remote, frontend, _frontend_collaborator) = init_remote_repo(root, "frontend");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    knit(&workspace, ["bundle", "venue capacity"]);
    knit(
        &workspace,
        [
            "bundle",
            "add",
            backend.to_str().unwrap(),
            frontend.to_str().unwrap(),
        ],
    );
    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/backend/app.txt"),
        "backend land",
    );
    append_line(
        &workspace.join(".knit/worktrees/venue-capacity/frontend/app.txt"),
        "frontend land",
    );
    knit(&workspace, ["commit", "--all", "-m", "Landing change"]);

    let fake_gh_dir = root.join("fake-gh");
    let fake_bin = root.join("fake-bin");
    write_fake_gh(&fake_bin, &fake_gh_dir);
    knit_with_fake_gh(
        &workspace,
        ["publish", "create", "--github", "--no-sync"],
        &fake_bin,
        &fake_gh_dir,
    );
    (workspace, fake_bin, fake_gh_dir)
}

pub fn unique_temp_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let base = std::env::temp_dir();
    // Windows temp dirs can come back as 8.3 short names (e.g. RUNNER~1);
    // canonicalize so recorded paths match the long-form paths git prints.
    let base = dunce_canonicalize_or(base);
    let path = base.join(format!(
        "knit-smoke-{}-{nanos}-{counter}",
        std::process::id()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

fn dunce_canonicalize_or(path: PathBuf) -> PathBuf {
    // std::fs::canonicalize would add a \\?\ verbatim prefix on Windows;
    // plain component comparison is what the tests need, so fall back to the
    // original path when canonicalization fails.
    match path.canonicalize() {
        Ok(canonical) => {
            let display = canonical.to_string_lossy();
            match display.strip_prefix("\\\\?\\") {
                Some(stripped) => PathBuf::from(stripped),
                None => canonical,
            }
        }
        Err(_) => path,
    }
}

pub fn init_repo(path: &Path, label: &str) {
    fs::create_dir_all(path).unwrap();
    git(path, ["init"]);
    git(path, ["checkout", "-b", "main"]);
    git(path, ["config", "user.email", "knit@example.test"]);
    git(path, ["config", "user.name", "Knit Smoke"]);
    // Tests write and assert LF content; Git for Windows defaults to
    // autocrlf=true, which would rewrite checkouts to CRLF.
    git(path, ["config", "core.autocrlf", "false"]);
    fs::write(path.join("app.txt"), format!("{label}\n")).unwrap();
    git(path, ["add", "app.txt"]);
    git(path, ["commit", "-m", &format!("Initial {label}")]);
}

pub fn init_remote_repo(root: &Path, label: &str) -> (PathBuf, PathBuf, PathBuf) {
    let seed = root.join(format!("{label}-seed"));
    init_repo(&seed, label);

    let remote = root.join(format!("{label}.git"));
    git(
        root,
        [
            "clone",
            "--bare",
            seed.to_str().unwrap(),
            remote.to_str().unwrap(),
        ],
    );

    let local = root.join(label);
    // autocrlf must be set at clone time: setting it after checkout leaves a
    // CRLF-smudged working tree that git then reports as modified.
    git(
        root,
        [
            "clone",
            "--config",
            "core.autocrlf=false",
            remote.to_str().unwrap(),
            local.to_str().unwrap(),
        ],
    );
    configure_git_user(&local);

    let collaborator = root.join(format!("{label}-collaborator"));
    git(
        root,
        [
            "clone",
            "--config",
            "core.autocrlf=false",
            remote.to_str().unwrap(),
            collaborator.to_str().unwrap(),
        ],
    );
    configure_git_user(&collaborator);

    (remote, local, collaborator)
}

pub fn configure_git_user(path: &Path) {
    git(path, ["config", "user.email", "knit@example.test"]);
    git(path, ["config", "user.name", "Knit Smoke"]);
}

pub fn append_line(path: &Path, line: &str) {
    let mut text = fs::read_to_string(path).unwrap();
    text.push_str(line);
    text.push('\n');
    fs::write(path, text).unwrap();
}

pub fn install_parallel_push_hook(repo: &Path, gate: &Path, id: &str, peer: &str) {
    install_parallel_gate_hook(repo, "pre-push", gate, id, peer);
}

/// Install a git hook in this fixture repository only.
///
/// Resolve the repository's own hooks directory explicitly. Agent and IDE
/// sessions may inject a process-wide core.hooksPath; honoring it here would
/// install a temporary test hook outside the fixture repository and
/// contaminate unrelated tests or real Git operations.
///
/// The *common* git dir, not the per-worktree one: git runs hooks from the
/// shared directory, so a hook written into `.git/worktrees/<name>/hooks`
/// silently never runs — which is exactly the kind of quietly dead test
/// scaffolding these hooks exist to avoid.
pub fn write_hook(repo: &Path, hook: &str, script: &str) {
    let git_dir = PathBuf::from(git(repo, ["rev-parse", "--git-common-dir"]).trim());
    let git_dir = if git_dir.is_absolute() {
        git_dir
    } else {
        repo.join(git_dir)
    };
    let hook_path = git_dir.join("hooks").join(hook);
    fs::create_dir_all(hook_path.parent().unwrap()).unwrap();
    fs::write(&hook_path, script).unwrap();
    make_executable(&hook_path);
}

/// A pre-push hook that fails its first `failures` invocations with `message`
/// and counts every attempt in `state/count`.
pub fn install_flaky_push_hook(repo: &Path, state: &Path, message: &str, failures: u32) {
    fs::create_dir_all(state).unwrap();
    write_hook(
        repo,
        "pre-push",
        &format!(
            r#"#!/bin/sh
set -eu
state={state}
count=$(cat "$state/count" 2>/dev/null || echo 0)
count=$((count + 1))
printf '%s\n' "$count" > "$state/count"
if [ "$count" -le {failures} ]; then
  printf '%s\n' {message} >&2
  exit 1
fi
"#,
            state = shell_quote(&state.to_string_lossy()),
            message = shell_quote(message),
            failures = failures
        ),
    );
}

/// A pre-push hook that hangs, so a small `KNIT_GIT_PUSH_TIMEOUT` can prove
/// the push is bounded rather than left to stall forever.
pub fn install_slow_push_hook(repo: &Path, seconds: u32) {
    write_hook(
        repo,
        "pre-push",
        &format!(
            r#"#!/bin/sh
set -eu
sleep {seconds}
"#
        ),
    );
}

/// A pre-push hook that records how many pushes were in flight at once, so a
/// test can assert the pool never exceeded its limit.
pub fn install_concurrency_probe_hook(repo: &Path, state: &Path) {
    fs::create_dir_all(state).unwrap();
    write_hook(
        repo,
        "pre-push",
        &format!(
            r#"#!/bin/sh
set -eu
state={state}
lock="$state/lock"
enter() {{
  while ! mkdir "$lock" 2>/dev/null; do sleep 0.01; done
  live=$(cat "$state/live" 2>/dev/null || echo 0)
  live=$((live + $1))
  printf '%s\n' "$live" > "$state/live"
  peak=$(cat "$state/peak" 2>/dev/null || echo 0)
  if [ "$live" -gt "$peak" ]; then printf '%s\n' "$live" > "$state/peak"; fi
  rmdir "$lock"
}}
enter 1
sleep 0.3
enter -1
"#,
            state = shell_quote(&state.to_string_lossy())
        ),
    );
}

pub fn read_counter(state: &Path, name: &str) -> u32 {
    fs::read_to_string(state.join(name))
        .map(|value| value.trim().parse().unwrap_or(0))
        .unwrap_or(0)
}

/// Make the fake `gh` fail `pr create` for `repo` with `stderr`. When `once`
/// is set the failure is spent after one attempt, so the retry can succeed.
pub fn fake_gh_fail_create(fake_gh_dir: &Path, repo: &str, stderr: &str, once: bool) {
    fs::write(
        fake_gh_dir.join(format!("create-fail-{repo}")),
        format!("{stderr}\n"),
    )
    .unwrap();
    if once {
        fs::write(fake_gh_dir.join(format!("create-fail-once-{repo}")), "").unwrap();
    }
}

/// Make the fake `gh` report an existing PR once a create attempt has reached
/// it: the shape of a create whose reply was lost after the host stored it.
pub fn fake_gh_existing_after_create(fake_gh_dir: &Path, repo: &str) {
    fs::write(
        fake_gh_dir.join(format!("existing-after-create-{repo}")),
        "",
    )
    .unwrap();
}

pub fn fake_gh_create_attempts(fake_gh_dir: &Path, repo: &str) -> usize {
    fs::read_to_string(fake_gh_dir.join(format!("create-attempts-{repo}")))
        .map(|value| value.lines().count())
        .unwrap_or(0)
}

pub fn install_parallel_gate_hook(repo: &Path, hook: &str, gate: &Path, id: &str, peer: &str) {
    fs::create_dir_all(gate).unwrap();
    write_hook(
        repo,
        hook,
        &format!(
            r#"#!/bin/sh
set -eu
gate={gate}
id={id}
peer={peer}
touch "$gate/$id"
i=0
while [ ! -f "$gate/$peer" ]; do
  i=$((i + 1))
  if [ "$i" -ge 100 ]; then
    echo "timed out waiting for parallel push peer $peer" >&2
    exit 42
  fi
  sleep 0.05
done
"#,
            gate = shell_quote(&gate.to_string_lossy()),
            id = shell_quote(id),
            peer = shell_quote(peer)
        ),
    );
}

pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn read_bundle(workspace: &Path) -> Value {
    let path = workspace.join(".knit/bundles/venue-capacity.bundle.json");
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

pub fn write_bundle_publications(workspace: &Path, bundle_id: &str, state: &str) {
    write_bundle_publications_for_repos(workspace, bundle_id, state, &[]);
}

pub fn write_bundle_publications_for_repos(
    workspace: &Path,
    bundle_id: &str,
    state: &str,
    repo_ids: &[&str],
) {
    let path = workspace
        .join(".knit/bundles")
        .join(format!("{bundle_id}.bundle.json"));
    let mut bundle: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let publications = bundle["repos"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .filter(|(_, repo)| repo_ids.is_empty() || repo_ids.contains(&repo["id"].as_str().unwrap()))
        .map(|(index, repo)| {
            let repo_id = repo["id"].as_str().unwrap();
            let head_branch = repo["featureBranch"].as_str().unwrap();
            let base_branch = repo["baseBranch"].as_str().unwrap();
            json!({
                "repoId": repo_id,
                "provider": "github",
                "kind": "pull_request",
                "number": (index + 1) as u64,
                "url": format!("https://github.com/acme/{repo_id}/pull/{}", index + 1),
                "baseBranch": base_branch,
                "headBranch": head_branch,
                "state": state,
                "updatedAt": "2026-05-22T00:00:00.000Z"
            })
        })
        .collect::<Vec<_>>();
    bundle["publications"] = json!(publications);
    fs::write(
        path,
        format!("{}\n", serde_json::to_string_pretty(&bundle).unwrap()),
    )
    .unwrap();
}

/// A single empty global-config home shared by the whole test process. Every
/// `knit` invocation defaults `KNIT_HOME` here so tests never read the running
/// user's real `~/.config/knit/config.json` (whose global remotes would
/// otherwise merge into test workspaces and break assertions). Tests that need
/// global config set their own `KNIT_HOME`, which still overrides this default.
pub fn isolated_knit_home() -> String {
    static KNIT_HOME: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    KNIT_HOME
        .get_or_init(|| {
            let dir = unique_temp_dir().join("global-knit-home");
            fs::create_dir_all(&dir).unwrap();
            dir
        })
        .to_string_lossy()
        .to_string()
}

/// Isolated global Git config for the binary under test: `knit remote
/// sync-helpers` and clone-time helper installation write `git config
/// --global`, which must never touch the running user's real gitconfig.
pub fn isolated_git_config_global() -> String {
    let dir = std::path::PathBuf::from(isolated_knit_home());
    dir.join("gitconfig").to_string_lossy().to_string()
}

/// Ambient identity overrides (exported by editor/agent harnesses such as a
/// T3 Code session) would silently override the per-repo `git config` identity
/// every fixture sets and inject actor attribution, breaking actor/author
/// assertions. Test-provided env is applied after, so a test can still set any
/// of these deliberately.
fn scrub_ambient_git_identity(command: &mut Command) {
    command
        .env_remove("GIT_AUTHOR_NAME")
        .env_remove("GIT_AUTHOR_EMAIL")
        .env_remove("GIT_COMMITTER_NAME")
        .env_remove("GIT_COMMITTER_EMAIL")
        .env_remove("T3_ACTOR_SESSION")
        .env_remove("T3_ACTOR_LABEL")
        .env_remove("T3_ACTOR_EMAIL");
}

// KNIT_BUNDLE and KNIT_SESSION are removed everywhere: the test process may
// itself run inside a knit bundle / agent session (dogfooding), and inherited
// bundle targeting or ledger attribution would break hermetic assertions.
// Test-provided env is applied after, so a test can still set either.
pub fn knit<I, S>(cwd: &Path, args: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new(env!("CARGO_BIN_EXE_knit"));
    command
        .args(args)
        .current_dir(cwd)
        .env("KNIT_HOME", isolated_knit_home())
        .env("GIT_CONFIG_GLOBAL", isolated_git_config_global())
        .env_remove("KNIT_BUNDLE")
        .env_remove("KNIT_SESSION");
    scrub_ambient_git_identity(&mut command);
    run(command)
}

pub fn knit_with_env<I, S>(cwd: &Path, args: I, env: &[(&str, &str)]) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new(env!("CARGO_BIN_EXE_knit"));
    command
        .args(args)
        .current_dir(cwd)
        .env("KNIT_HOME", isolated_knit_home())
        .env("GIT_CONFIG_GLOBAL", isolated_git_config_global())
        .env_remove("KNIT_BUNDLE")
        .env_remove("KNIT_SESSION");
    scrub_ambient_git_identity(&mut command);
    // Test-provided env wins, so a test can still point KNIT_HOME at its own dir.
    for (key, value) in env {
        command.env(key, value);
    }
    run(command)
}

pub fn knit_fails<I, S>(cwd: &Path, args: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new(env!("CARGO_BIN_EXE_knit"));
    command
        .args(args)
        .current_dir(cwd)
        .env("KNIT_HOME", isolated_knit_home())
        .env("GIT_CONFIG_GLOBAL", isolated_git_config_global())
        .env_remove("KNIT_BUNDLE")
        .env_remove("KNIT_SESSION");
    scrub_ambient_git_identity(&mut command);
    let output = command.output().unwrap();
    assert!(!output.status.success());
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

pub fn knit_fails_with_env<I, S>(cwd: &Path, args: I, env: &[(&str, &str)]) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new(env!("CARGO_BIN_EXE_knit"));
    command
        .args(args)
        .current_dir(cwd)
        .env("KNIT_HOME", isolated_knit_home())
        .env("GIT_CONFIG_GLOBAL", isolated_git_config_global())
        .env_remove("KNIT_BUNDLE")
        .env_remove("KNIT_SESSION");
    scrub_ambient_git_identity(&mut command);
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().unwrap();
    assert!(!output.status.success());
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

pub fn knit_with_fake_gh<I, S>(cwd: &Path, args: I, fake_bin: &Path, fake_gh_dir: &Path) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    knit_with_fake_gh_env(cwd, args, fake_bin, fake_gh_dir, &[])
}

pub fn knit_with_fake_gh_env<I, S>(
    cwd: &Path,
    args: I,
    fake_bin: &Path,
    fake_gh_dir: &Path,
    env: &[(&str, &str)],
) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let old_path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(fake_bin.to_path_buf()).chain(std::env::split_paths(&old_path)),
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_knit"));
    command
        .args(args)
        .current_dir(cwd)
        .env("KNIT_HOME", isolated_knit_home())
        .env("GIT_CONFIG_GLOBAL", isolated_git_config_global())
        .env_remove("KNIT_BUNDLE")
        .env_remove("KNIT_SESSION")
        .env("PATH", path)
        .env("GH_FAKE_DIR", fake_gh_dir);
    scrub_ambient_git_identity(&mut command);
    for (key, value) in env {
        command.env(key, value);
    }
    run(command)
}

pub fn knit_fails_with_fake_gh<I, S>(
    cwd: &Path,
    args: I,
    fake_bin: &Path,
    fake_gh_dir: &Path,
) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    knit_fails_with_fake_gh_env(cwd, args, fake_bin, fake_gh_dir, &[])
}

pub fn knit_fails_with_fake_gh_env<I, S>(
    cwd: &Path,
    args: I,
    fake_bin: &Path,
    fake_gh_dir: &Path,
    env: &[(&str, &str)],
) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let old_path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(fake_bin.to_path_buf()).chain(std::env::split_paths(&old_path)),
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_knit"));
    command
        .args(args)
        .current_dir(cwd)
        .env("KNIT_HOME", isolated_knit_home())
        .env("GIT_CONFIG_GLOBAL", isolated_git_config_global())
        .env_remove("KNIT_BUNDLE")
        .env_remove("KNIT_SESSION")
        .env("PATH", path)
        .env("GH_FAKE_DIR", fake_gh_dir);
    scrub_ambient_git_identity(&mut command);
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().unwrap();
    assert!(!output.status.success());
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

pub fn knit_with_fake_forge<I, S>(
    cwd: &Path,
    args: I,
    fake_bin: &Path,
    fake_dir: &Path,
    env: &[(&str, &str)],
) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let old_path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(fake_bin.to_path_buf()).chain(std::env::split_paths(&old_path)),
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_knit"));
    command
        .args(args)
        .current_dir(cwd)
        .env("KNIT_HOME", isolated_knit_home())
        .env("GIT_CONFIG_GLOBAL", isolated_git_config_global())
        .env_remove("KNIT_BUNDLE")
        .env_remove("KNIT_SESSION")
        .env("PATH", path)
        .env("FORGE_FAKE_DIR", fake_dir);
    scrub_ambient_forge_env(&mut command);
    scrub_ambient_git_identity(&mut command);
    for (key, value) in env {
        command.env(key, value);
    }
    run(command)
}

/// Strip forge token/base env vars so spawned knit processes never pick up a
/// developer's real credentials (or a sibling test's `set_var`) and silently
/// switch adapters into native REST mode.
pub fn scrub_ambient_forge_env(command: &mut Command) {
    for var in [
        "KNIT_GITLAB_API_BASE",
        "KNIT_GITLAB_TOKEN",
        "GITLAB_TOKEN",
        "KNIT_FORGEJO_API_BASE",
        "KNIT_FORGEJO_TOKEN",
        "CODEBERG_TOKEN",
        "GITEA_TOKEN",
        "KNIT_BITBUCKET_API_BASE",
        "KNIT_BITBUCKET_ACCESS_TOKEN",
        "KNIT_BITBUCKET_EMAIL",
        "KNIT_BITBUCKET_API_TOKEN",
    ] {
        command.env_remove(var);
    }
}

pub fn git<I, S>(cwd: &Path, args: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new("git");
    command.args(args).current_dir(cwd);
    scrub_ambient_git_identity(&mut command);
    run(command)
}

pub fn git_success<I, S>(cwd: &Path, args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new("git");
    command.args(args).current_dir(cwd);
    scrub_ambient_git_identity(&mut command);
    command.stdout(Stdio::null()).stderr(Stdio::null());
    command.status().unwrap().success()
}

/// Spawn a short-lived child process, wait for it to exit, and return its pid.
pub fn exited_process_pid() -> u32 {
    let mut child = if cfg!(windows) {
        Command::new("cmd")
            .args(["/C", "exit", "0"])
            .spawn()
            .expect("spawn cmd")
    } else {
        Command::new("true").spawn().expect("spawn true")
    };
    let pid = child.id();
    child.wait().unwrap();
    pid
}

pub fn run(mut command: Command) -> String {
    let output = command.output().unwrap();
    if !output.status.success() {
        panic!(
            "command failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8_lossy(&output.stdout).to_string()
}

pub fn write_fake_gh(fake_bin: &Path, fake_gh_dir: &Path) {
    fs::create_dir_all(fake_bin).unwrap();
    fs::create_dir_all(fake_gh_dir).unwrap();
    let script = fake_bin.join("gh");
    fs::write(
        &script,
        r#"#!/bin/sh
set -eu

api_pr_json() {
  pr_repo="$1"
  number="$2"
  base="main"
  head="knit/artifact-publish"
  if [ -f "$GH_FAKE_DIR/api-$pr_repo.head" ]; then
    head="$(cat "$GH_FAKE_DIR/api-$pr_repo.head")"
  fi
  state="open"
  merged="false"
  if [ -f "$GH_FAKE_DIR/merged-$pr_repo" ]; then
    state="closed"
    merged="true"
  fi
  title="$pr_repo PR"
  if [ -f "$GH_FAKE_DIR/revert-$pr_repo.number" ] && [ "$number" = "$(cat "$GH_FAKE_DIR/revert-$pr_repo.number")" ]; then
    state="open"
    merged="false"
    title="Revert $pr_repo PR"
    head="knit/revert-$pr_repo"
  fi
  mergeable="true"
  mergestate="clean"
  if [ -f "$GH_FAKE_DIR/conflict-$pr_repo" ]; then
    mergeable="false"
    mergestate="dirty"
  fi
  printf '{"number":%s,"html_url":"https://github.com/acme/%s/pull/%s","state":"%s","title":"%s","body":"Existing body","draft":false,"head":{"ref":"%s","sha":"%s-head"},"base":{"ref":"%s"},"merged":%s,"mergeable":%s,"mergeable_state":"%s"}\n' "$number" "$pr_repo" "$number" "$state" "$title" "$head" "$pr_repo" "$base" "$merged" "$mergeable" "$mergestate"
}

if [ "$1" = "api" ]; then
  shift
  method="GET"
  endpoint=""
  input=""
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --method)
        method="$2"
        shift 2
        ;;
      --input)
        input="$2"
        shift 2
        ;;
      --jq)
        shift 2
        ;;
      -*)
        shift
        ;;
      *)
        if [ -z "$endpoint" ]; then
          endpoint="$1"
        fi
        shift
        ;;
      esac
  done
  endpoint_path="${endpoint%%\?*}"
  case "$endpoint_path" in
    repos/acme/backend/pulls|repos/acme/backend/pulls/*) pr_repo=backend ;;
    repos/acme/frontend/pulls|repos/acme/frontend/pulls/*) pr_repo=frontend ;;
    *) pr_repo=other ;;
  esac
  case "$pr_repo" in
    backend) number=101 ;;
    frontend) number=202 ;;
    *) number=303 ;;
  esac
  if [ -f "$GH_FAKE_DIR/next-$pr_repo.number" ]; then
    number="$(cat "$GH_FAKE_DIR/next-$pr_repo.number")"
  fi
  case "$endpoint_path" in
    repos/acme/*/pulls)
      if [ "$method" = "GET" ]; then
        printf '%s\n' "$method" > "$GH_FAKE_DIR/api-$pr_repo-find.method"
        printf '%s\n' "$endpoint" > "$GH_FAKE_DIR/api-$pr_repo-find.endpoint"
        printf '%s\n' "${GH_PROMPT_DISABLED:-}" > "$GH_FAKE_DIR/api-$pr_repo-find.prompt"
        if [ -f "$GH_FAKE_DIR/existing-$pr_repo" ]; then
          printf '['
          api_pr_json "$pr_repo" "$number"
          printf ']\n'
        else
          printf '[]\n'
        fi
      elif [ "$method" = "POST" ]; then
        if [ "$input" = "-" ]; then
          cat > "$GH_FAKE_DIR/api-$pr_repo.json"
          sed -n 's/.*"head":"\([^"]*\)".*/\1/p' "$GH_FAKE_DIR/api-$pr_repo.json" > "$GH_FAKE_DIR/api-$pr_repo.head"
        else
          : > "$GH_FAKE_DIR/api-$pr_repo.json"
        fi
        printf '%s\n' "$method" > "$GH_FAKE_DIR/api-$pr_repo.method"
        printf '%s\n' "$endpoint" > "$GH_FAKE_DIR/api-$pr_repo.endpoint"
        printf '%s\n' "${GH_PROMPT_DISABLED:-}" > "$GH_FAKE_DIR/api-$pr_repo.prompt"
        printf 'https://github.com/acme/%s/pull/%s\n' "$pr_repo" "$number"
      else
        echo "unexpected gh api method for pull collection: $method" >&2
        exit 1
      fi
      ;;
    repos/acme/*/pulls/*)
      number="${endpoint_path##*/}"
      if [ "$method" = "GET" ]; then
        printf '%s\n' "$method" > "$GH_FAKE_DIR/api-$pr_repo-view.method"
        printf '%s\n' "$endpoint" > "$GH_FAKE_DIR/api-$pr_repo-view.endpoint"
        printf '%s\n' "${GH_PROMPT_DISABLED:-}" > "$GH_FAKE_DIR/api-$pr_repo-view.prompt"
        api_pr_json "$pr_repo" "$number"
      elif [ "$method" = "PATCH" ]; then
        if [ "$input" = "-" ]; then
          cat > "$GH_FAKE_DIR/api-$pr_repo-edit.json"
        else
          : > "$GH_FAKE_DIR/api-$pr_repo-edit.json"
        fi
        printf '%s\n' "$method" > "$GH_FAKE_DIR/api-$pr_repo-edit.method"
        printf '%s\n' "$endpoint" > "$GH_FAKE_DIR/api-$pr_repo-edit.endpoint"
        printf '%s\n' "${GH_PROMPT_DISABLED:-}" > "$GH_FAKE_DIR/api-$pr_repo-edit.prompt"
        api_pr_json "$pr_repo" "$number"
      else
        echo "unexpected gh api method for pull item: $method" >&2
        exit 1
      fi
      ;;
    *)
      echo "unexpected gh api endpoint: $endpoint" >&2
      exit 1
      ;;
  esac
  exit 0
fi

if [ "$1" != "pr" ]; then
  echo "unexpected gh command: $*" >&2
  exit 1
fi
shift
sub="$1"
shift
repo="$(basename "$PWD")"

case "$sub" in
  list)
    if [ -f "$GH_FAKE_DIR/existing-after-create-$repo" ] && [ -f "$GH_FAKE_DIR/create-attempted-$repo" ]; then
      case "$repo" in
        backend) number=101 ;;
        frontend) number=202 ;;
        *) number=303 ;;
      esac
      printf '[{"number":%s,"url":"https://github.com/acme/%s/pull/%s","state":"OPEN","title":"%s PR","baseRefName":"main","headRefName":"knit/venue-capacity","body":"Existing body","isDraft":false,"headRefOid":"%s-head","mergeable":"MERGEABLE","mergeStateStatus":"CLEAN","reviewDecision":""}]\n' "$number" "$repo" "$number" "$repo" "$repo"
    else
      printf '[]\n'
    fi
    ;;
  create)
    base="main"
    args="$*"
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --base)
          base="$2"
          shift 2
          ;;
        *)
          shift
          ;;
      esac
    done
    printf '%s\n' "$base" > "$GH_FAKE_DIR/create-$repo.base"
    printf '%s\n' "$args" > "$GH_FAKE_DIR/create-$repo.args"
    cat > "$GH_FAKE_DIR/create-$repo.md"
    printf 'x\n' >> "$GH_FAKE_DIR/create-attempts-$repo"
    touch "$GH_FAKE_DIR/create-attempted-$repo"
    if [ -f "$GH_FAKE_DIR/create-gate-$repo" ]; then
      sh "$GH_FAKE_DIR/create-gate-$repo"
    fi
    if [ -f "$GH_FAKE_DIR/create-fail-$repo" ]; then
      cat "$GH_FAKE_DIR/create-fail-$repo" >&2
      if [ -f "$GH_FAKE_DIR/create-fail-once-$repo" ]; then
        rm -f "$GH_FAKE_DIR/create-fail-once-$repo" "$GH_FAKE_DIR/create-fail-$repo"
      fi
      exit 1
    fi
    case "$repo" in
      backend) number=101 ;;
      frontend) number=202 ;;
      *) number=303 ;;
    esac
    if [ -f "$GH_FAKE_DIR/next-$repo.number" ]; then
      number="$(cat "$GH_FAKE_DIR/next-$repo.number")"
    fi
    printf 'https://github.com/acme/%s/pull/%s\n' "$repo" "$number"
    ;;
  view)
    url="$1"
    tail="${url#https://github.com/acme/}"
    pr_repo="${tail%%/*}"
    number="${url##*/}"
    base="main"
    if [ -f "$GH_FAKE_DIR/create-$pr_repo.base" ]; then
      base="$(cat "$GH_FAKE_DIR/create-$pr_repo.base")"
    fi
    state="OPEN"
    title="$pr_repo PR"
    head="knit/venue-capacity"
    if [ -f "$GH_FAKE_DIR/revert-$pr_repo.number" ] && [ "$number" = "$(cat "$GH_FAKE_DIR/revert-$pr_repo.number")" ]; then
      state="OPEN"
      title="Revert $pr_repo PR"
      head="knit/revert-$pr_repo"
    elif [ -f "$GH_FAKE_DIR/merged-$pr_repo" ]; then
      state="MERGED"
    fi
    draft="false"
    if [ "${GH_FAKE_DRAFT:-0}" = "1" ]; then
      draft="true"
    fi
    mergeable="MERGEABLE"
    mergestate="CLEAN"
    if [ -f "$GH_FAKE_DIR/conflict-$pr_repo" ]; then
      mergeable="CONFLICTING"
      mergestate="DIRTY"
    fi
    review="${GH_FAKE_REVIEW:-}"
    printf '{"number":%s,"url":"%s","state":"%s","title":"%s","baseRefName":"%s","headRefName":"%s","body":"Existing body","isDraft":%s,"headRefOid":"%s-head","mergeable":"%s","mergeStateStatus":"%s","reviewDecision":"%s"}\n' "$number" "$url" "$state" "$title" "$base" "$head" "$draft" "$pr_repo" "$mergeable" "$mergestate" "$review"
    ;;
  edit)
    url="$1"
    shift
    tail="${url#https://github.com/acme/}"
    pr_repo="${tail%%/*}"
    edited_base=""
    body_file=""
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --base)
          edited_base="$2"
          shift 2
          ;;
        --body-file)
          body_file="$2"
          shift 2
          ;;
        *)
          shift
          ;;
      esac
    done
    if [ -n "$edited_base" ]; then
      printf '%s\n' "$edited_base" > "$GH_FAKE_DIR/create-$pr_repo.base"
      printf '%s\n' "$pr_repo $edited_base" >> "$GH_FAKE_DIR/retarget-order.txt"
    elif [ "$body_file" = "-" ]; then
      cat > "$GH_FAKE_DIR/edit-$pr_repo.md"
    fi
    printf '%s\n' "$url"
    ;;
  revert)
    url="$1"
    shift
    tail="${url#https://github.com/acme/}"
    pr_repo="${tail%%/*}"
    title="Revert $pr_repo PR"
    body_written=0
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --title)
          title="$2"
          shift 2
          ;;
        --body-file)
          if [ "$2" = "-" ]; then
            cat > "$GH_FAKE_DIR/revert-$pr_repo.md"
          else
            cp "$2" "$GH_FAKE_DIR/revert-$pr_repo.md"
          fi
          body_written=1
          shift 2
          ;;
        *)
          shift
          ;;
      esac
    done
    if [ "$body_written" -eq 0 ]; then
      : > "$GH_FAKE_DIR/revert-$pr_repo.md"
    fi
    case "$pr_repo" in
      backend) number=901 ;;
      frontend) number=902 ;;
      *) number=903 ;;
    esac
    printf '%s\n' "$number" > "$GH_FAKE_DIR/revert-$pr_repo.number"
    printf '%s\n' "$title" > "$GH_FAKE_DIR/revert-$pr_repo.title"
    printf '%s\n' "$pr_repo" >> "$GH_FAKE_DIR/revert-order.txt"
    printf 'https://github.com/acme/%s/pull/%s\n' "$pr_repo" "$number"
    ;;
  checks)
    if [ "${GH_FAKE_NO_REQUIRED_CHECKS_ERROR:-0}" = "1" ]; then
      echo "no required checks reported" >&2
      exit 1
    fi
    if [ "${GH_FAKE_CHECKS_FAIL:-0}" = "1" ]; then
      printf '[{"name":"test","state":"FAILURE","bucket":"fail"}]\n'
    else
      printf '[]\n'
    fi
    ;;
  merge)
    url="$1"
    tail="${url#https://github.com/acme/}"
    pr_repo="${tail%%/*}"
    printf '%s\n' "$pr_repo" >> "$GH_FAKE_DIR/merge-order.txt"
    method=""
    for arg in "$@"; do
      case "$arg" in
        --merge|--squash|--rebase) method="$arg" ;;
      esac
    done
    printf '%s %s\n' "$pr_repo" "$method" >> "$GH_FAKE_DIR/merge-methods.txt"
    touch "$GH_FAKE_DIR/merged-$pr_repo"
    printf 'Merged pull request %s\n' "$url"
    ;;
  *)
    echo "unexpected gh pr command: $sub" >&2
    exit 1
    ;;
esac
"#,
    )
    .unwrap();
    make_executable(&script);
    write_windows_shim(&script);
}

/// Mark a fake script executable on Unix. On Windows execute bits do not
/// exist; the `.cmd` shim from `write_windows_shim` makes it spawnable.
fn make_executable(script: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(script, permissions).unwrap();
    }
    #[cfg(not(unix))]
    let _ = script;
}

/// On Windows, `Command::new("gh")` cannot spawn a shebang script. Write a
/// sibling `gh.cmd` that runs the sh script through Git for Windows' `sh`,
/// which is present wherever git is.
fn write_windows_shim(script: &Path) {
    #[cfg(windows)]
    {
        let shim = script.with_extension("cmd");
        fs::write(
            shim,
            "@sh \"%~dp0{}\" %*\r\n".replace("{}", &script.file_name().unwrap().to_string_lossy()),
        )
        .unwrap();
    }
    #[cfg(not(windows))]
    let _ = script;
}

pub fn write_fake_glab(fake_bin: &Path, fake_dir: &Path) {
    fs::create_dir_all(fake_bin).unwrap();
    fs::create_dir_all(fake_dir).unwrap();
    let script = fake_bin.join("glab");
    fs::write(
        &script,
        r##"#!/bin/sh
set -eu
repo="$(basename "$PWD")"
if [ "$1" = "api" ]; then
  case "$*" in
    *"/approvals"*) printf '{"approved":true}\n' ;;
    *"/pipelines"*) printf '[]\n' ;;
    *) printf '{}\n' ;;
  esac
  exit 0
fi
[ "$1" = "mr" ] || { echo "unexpected glab command: $*" >&2; exit 1; }
sub="$2"
case "$sub" in
  list) printf '[]\n' ;;
  create)
    printf '%s\n' "$*" >"$FORGE_FAKE_DIR/glab-create.args"
    printf 'https://gitlab.com/acme/%s/-/merge_requests/12\n' "$repo"
    ;;
  view)
    state="opened"
    [ ! -f "$FORGE_FAKE_DIR/glab-merged" ] || state="merged"
    printf '{"iid":12,"web_url":"https://gitlab.com/acme/%s/-/merge_requests/12","state":"%s","title":"feature","target_branch":"main","source_branch":"knit/forge-workspace","description":"body","sha":"deadbeef","detailed_merge_status":"mergeable"}\n' "$repo" "$state"
    ;;
  update) printf '%s\n' "$*" >"$FORGE_FAKE_DIR/glab-update.args" ;;
  merge)
    printf '%s\n' "$*" >"$FORGE_FAKE_DIR/glab-merge.args"
    : >"$FORGE_FAKE_DIR/glab-merged"
    ;;
  *) echo "unexpected glab mr command: $*" >&2; exit 1 ;;
esac
"##,
    )
    .unwrap();
    make_executable(&script);
    write_windows_shim(&script);
}

pub fn write_fake_tea(fake_bin: &Path, fake_dir: &Path) {
    fs::create_dir_all(fake_bin).unwrap();
    fs::create_dir_all(fake_dir).unwrap();
    let script = fake_bin.join("tea");
    fs::write(
        &script,
        r##"#!/bin/sh
set -eu
repo="$(basename "$PWD")"
[ "$1" = "pr" ] || { echo "unexpected tea command: $*" >&2; exit 1; }
sub="$2"
case "$sub" in
  list)
    state="open"
    [ ! -f "$FORGE_FAKE_DIR/tea-merged" ] || state="merged"
    if [ -f "$FORGE_FAKE_DIR/tea-created" ]; then
      printf '[{"Index":4,"State":"%s","Title":"feature","Head":"knit/forge-workspace","Base":"main","URL":"https://codeberg.org/acme/%s/pulls/4"}]\n' "$state" "$repo"
    else
      printf '[]\n'
    fi
    ;;
  create)
    printf '%s\n' "$*" >"$FORGE_FAKE_DIR/tea-create.args"
    : >"$FORGE_FAKE_DIR/tea-created"
    printf 'https://codeberg.org/acme/%s/pulls/4\n' "$repo"
    ;;
  edit) printf '%s\n' "$*" >"$FORGE_FAKE_DIR/tea-edit.args" ;;
  merge)
    printf '%s\n' "$*" >"$FORGE_FAKE_DIR/tea-merge.args"
    : >"$FORGE_FAKE_DIR/tea-merged"
    ;;
  *) echo "unexpected tea pr command: $*" >&2; exit 1 ;;
esac
"##,
    )
    .unwrap();
    make_executable(&script);
    write_windows_shim(&script);
}

/// Serve a fake GitHub REST API on a local port, mirroring the routes the
/// native `KNIT_GITHUB_API_TRANSPORT` transport hits. State is shared with the
/// fake `gh` script through marker files in `fake_gh_dir` (`merged-backend`),
/// and requests are captured as `api-backend-*.json` / `api.authorization`.
pub fn spawn_fake_github_api(fake_gh_dir: &Path) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    fs::create_dir_all(fake_gh_dir).unwrap();
    let dir = fake_gh_dir.to_path_buf();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let dir = dir.clone();
            std::thread::spawn(move || {
                let _ = handle_fake_github_request(&mut stream, &dir);
            });
        }
    });
    base_url
}

/// Serve the Bitbucket Cloud routes exercised by the native adapter.
pub fn spawn_fake_bitbucket_api(dir: &Path) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    fs::create_dir_all(dir).unwrap();
    let dir = dir.to_path_buf();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let dir = dir.clone();
            std::thread::spawn(move || {
                let _ = handle_fake_bitbucket_request(&mut stream, &dir);
            });
        }
    });
    base_url
}

fn fake_bitbucket_pr_json(dir: &Path, number: &str) -> String {
    let merged = dir.join("merged-backend").exists();
    let state = if merged { "MERGED" } else { "OPEN" };
    let base =
        fs::read_to_string(dir.join("bitbucket-backend.base")).unwrap_or_else(|_| "main".into());
    format!(
        "{{\"id\":{number},\"links\":{{\"html\":{{\"href\":\"https://bitbucket.org/acme/backend/pull-requests/{number}\"}}}},\"title\":\"backend PR\",\"state\":\"{state}\",\"description\":\"Existing body\",\"draft\":false,\"source\":{{\"branch\":{{\"name\":\"knit/forge\"}},\"commit\":{{\"hash\":\"deadbeefcafe\"}}}},\"destination\":{{\"branch\":{{\"name\":\"{}\"}}}},\"participants\":[{{\"approved\":true}}]}}",
        base.trim()
    )
}

fn handle_fake_bitbucket_request(
    stream: &mut std::net::TcpStream,
    dir: &Path,
) -> std::io::Result<()> {
    use std::io::{BufRead, BufReader, Read, Write};

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let mut content_length = 0usize;
    let mut authorization = String::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            match name.trim().to_ascii_lowercase().as_str() {
                "content-length" => content_length = value.trim().parse().unwrap_or(0),
                "authorization" => authorization = value.trim().to_string(),
                _ => {}
            }
        }
    }
    let mut body = vec![0; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    let body = String::from_utf8_lossy(&body).to_string();
    if !authorization.is_empty() {
        fs::write(dir.join("bitbucket.authorization"), authorization).unwrap();
    }
    let path = target
        .split('?')
        .next()
        .unwrap_or_default()
        .trim_start_matches('/');
    if target.contains("?q=") {
        fs::write(dir.join("bitbucket.query"), &target).unwrap();
    }
    let segments = path.split('/').collect::<Vec<_>>();
    let (status, response) = match (method.as_str(), segments.as_slice()) {
        ("GET", ["repositories", "acme", "backend", "pullrequests"]) => {
            let response = if dir.join("existing-backend").exists() {
                format!("{{\"values\":[{}]}}", fake_bitbucket_pr_json(dir, "101"))
            } else {
                "{\"values\":[]}".to_string()
            };
            (200, response)
        }
        ("POST", ["repositories", "acme", "backend", "pullrequests"]) => {
            fs::write(dir.join("bitbucket-create.json"), &body).unwrap();
            (201, fake_bitbucket_pr_json(dir, "101"))
        }
        ("GET", ["repositories", "acme", "backend", "pullrequests", number]) => {
            (200, fake_bitbucket_pr_json(dir, number))
        }
        ("PUT", ["repositories", "acme", "backend", "pullrequests", number]) => {
            fs::write(dir.join("bitbucket-edit.json"), &body).unwrap();
            if let Ok(payload) = serde_json::from_str::<Value>(&body) {
                if let Some(base) = payload
                    .pointer("/destination/branch/name")
                    .and_then(Value::as_str)
                {
                    fs::write(dir.join("bitbucket-backend.base"), base).unwrap();
                }
            }
            (200, fake_bitbucket_pr_json(dir, number))
        }
        ("POST", ["repositories", "acme", "backend", "pullrequests", _, "merge"]) => {
            fs::write(dir.join("bitbucket-merge.json"), &body).unwrap();
            fs::write(dir.join("merged-backend"), "").unwrap();
            (200, fake_bitbucket_pr_json(dir, "101"))
        }
        ("GET", ["repositories", "acme", "backend", "pullrequests", _, "statuses"]) => {
            let state = if dir.join("ci-fail-backend").exists() {
                "FAILED"
            } else {
                "SUCCESSFUL"
            };
            (
                200,
                format!("{{\"values\":[{{\"key\":\"ci\",\"state\":\"{state}\"}}]}}"),
            )
        }
        ("GET", ["repositories", "acme", "backend", "commit", _, "statuses", "build"]) => {
            (200, "{\"values\":[]}".to_string())
        }
        _ => (
            404,
            format!("{{\"error\":{{\"message\":\"unexpected {method} /{path}\"}}}}"),
        ),
    };
    write!(
        stream,
        "HTTP/1.1 {status} Fake\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}",
        response.len()
    )?;
    stream.flush()
}

fn fake_github_pr_json(dir: &Path, number: &str) -> String {
    let merged = dir.join("merged-backend").exists();
    let (state, merged_flag) = if merged {
        ("closed", "true")
    } else {
        ("open", "false")
    };
    let base =
        fs::read_to_string(dir.join("api-backend.base")).unwrap_or_else(|_| "main".to_string());
    let base = base.trim();
    format!(
        "{{\"number\":{number},\"html_url\":\"https://github.com/acme/backend/pull/{number}\",\"state\":\"{state}\",\"title\":\"backend PR\",\"body\":\"Existing body\",\"draft\":false,\"head\":{{\"ref\":\"knit/artifact-publish\",\"sha\":\"backend-head\"}},\"base\":{{\"ref\":\"{base}\"}},\"merged\":{merged_flag},\"mergeable\":true,\"mergeable_state\":\"clean\"}}"
    )
}

fn handle_fake_github_request(stream: &mut std::net::TcpStream, dir: &Path) -> std::io::Result<()> {
    use std::io::{BufRead, BufReader, Read, Write};

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();

    let mut content_length = 0usize;
    let mut authorization = String::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            match name.trim().to_ascii_lowercase().as_str() {
                "content-length" => content_length = value.trim().parse().unwrap_or(0),
                "authorization" => authorization = value.trim().to_string(),
                _ => {}
            }
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    let body = String::from_utf8_lossy(&body).to_string();
    if !authorization.is_empty() {
        let _ = fs::write(dir.join("api.authorization"), &authorization);
    }

    let path = target
        .split('?')
        .next()
        .unwrap_or_default()
        .trim_start_matches('/')
        .to_string();
    let segments: Vec<&str> = path.split('/').collect();
    let (status, response) = match (method.as_str(), segments.as_slice()) {
        ("GET", ["repos", "acme", "backend", "pulls"]) => (200, "[]".to_string()),
        ("POST", ["repos", "acme", "backend", "pulls"]) => {
            fs::write(dir.join("api-backend-create.json"), &body).unwrap();
            (201, fake_github_pr_json(dir, "101"))
        }
        ("PUT", ["repos", "acme", "backend", "pulls", _, "merge"]) => {
            fs::write(dir.join("api-backend-merge.json"), &body).unwrap();
            fs::write(dir.join("merged-backend"), "").unwrap();
            (
                200,
                "{\"merged\":true,\"message\":\"Pull Request successfully merged\",\"sha\":\"merge-sha\"}".to_string(),
            )
        }
        ("GET", ["repos", "acme", "backend", "pulls", number]) => {
            (200, fake_github_pr_json(dir, number))
        }
        ("PATCH", ["repos", "acme", "backend", "pulls", number]) => {
            fs::write(dir.join("api-backend-edit.json"), &body).unwrap();
            if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&body) {
                if let Some(base) = payload.get("base").and_then(|value| value.as_str()) {
                    fs::write(dir.join("api-backend.base"), base).unwrap();
                }
            }
            (200, fake_github_pr_json(dir, number))
        }
        ("POST", ["repos", "acme", repo, "merges"]) => {
            // Branch merge, the endpoint an intermediate lane landing uses.
            // `conflict-<repo>` opts into 409; a repeated merge of the same
            // head answers 204, the host's way of saying nothing to do.
            fs::write(dir.join(format!("api-{repo}-merges.json")), &body).unwrap();
            let head = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|payload| {
                    payload
                        .get("head")
                        .and_then(|value| value.as_str())
                        .map(ToOwned::to_owned)
                })
                .unwrap_or_default();
            let marker = dir.join(format!("merged-branch-{repo}-{}", head.replace('/', "-")));
            if dir.join(format!("conflict-{repo}")).exists() {
                (409, "{\"message\":\"Merge conflict\"}".to_string())
            } else if marker.exists() {
                (204, String::new())
            } else {
                fs::write(&marker, "").unwrap();
                (
                    201,
                    "{\"sha\":\"branch-merge-sha\",\"merged\":true}".to_string(),
                )
            }
        }
        ("GET", ["repos", "acme", repo, "commits", _, "check-runs"]) => {
            // Marker files opt a repo into non-empty commit CI: `ci-pass-<repo>`
            // serves one passing run, `ci-fail-<repo>` one failing run. Without
            // a marker the response stays empty, which land tests rely on.
            let body = if dir.join(format!("ci-fail-{repo}")).exists() {
                "{\"total_count\":1,\"check_runs\":[{\"name\":\"ci\",\"status\":\"completed\",\"conclusion\":\"failure\"}]}".to_string()
            } else if dir.join(format!("ci-pass-{repo}")).exists() {
                "{\"total_count\":1,\"check_runs\":[{\"name\":\"ci\",\"status\":\"completed\",\"conclusion\":\"success\"}]}".to_string()
            } else {
                "{\"total_count\":0,\"check_runs\":[]}".to_string()
            };
            (200, body)
        }
        ("GET", ["repos", "acme", _, "commits", _, "status"]) => {
            (200, "{\"state\":\"success\",\"statuses\":[]}".to_string())
        }
        _ => (
            404,
            format!("{{\"message\":\"unexpected endpoint {method} /{path}\"}}"),
        ),
    };
    write!(
        stream,
        "HTTP/1.1 {status} Fake\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}",
        response.len()
    )?;
    stream.flush()
}
/// Reserve a local port and immediately release it, yielding a base URL that
/// refuses connections — an unreachable sync remote for resilience tests.
pub fn unreachable_remote_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    base_url
}

/// Spawn a minimal fake remote API that answers every request with a project
/// export containing a single bundle record. Enough for creation-time slug
/// collision checks against the sync remote.
pub fn spawn_fake_remote_export(bundle_slug: &str, lifecycle_state: &str) -> String {
    spawn_fake_remote_with_body(format!(
        "{{\"data\":{{\"project\":{{\"slug\":\"demo\"}},\"knitProject\":null,\"repositories\":[],\"bundles\":[{{\"id\":\"rb-1\",\"slug\":\"{bundle_slug}\",\"lifecycleState\":\"{lifecycle_state}\",\"currentArtifact\":null}}],\"historyEvents\":[]}}}}"
    ))
}

/// Spawn a fake remote API that answers every request with the given JSON
/// body, e.g. a full project export including bundle artifact payloads.
pub fn spawn_fake_remote_with_body(body: String) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let body = body.clone();
            std::thread::spawn(move || {
                let _ = respond_with_json(&mut stream, &body);
            });
        }
    });
    base_url
}

/// Spawn a fake sync remote that serves the project export and per-bundle
/// artifacts from separate files, the way a server with the slim export does:
///
/// - `GET /api/v1/projects/:slug/export` -> `<dir>/export.json`
/// - `GET /api/v1/bundles/:id` -> `<dir>/bundle-<id>.json` (404 when absent)
///
/// Every per-bundle artifact request is appended to `<dir>/artifact-fetches.txt`,
/// one bundle id per line, so tests can assert exactly which payloads the
/// client downloaded — and which it never asked for.
pub fn spawn_fake_remote_bundle_api(dir: &Path) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    fs::create_dir_all(dir).unwrap();
    let dir = dir.to_path_buf();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let dir = dir.clone();
            std::thread::spawn(move || {
                let _ = handle_fake_remote_bundle_request(&mut stream, &dir);
            });
        }
    });
    base_url
}

/// The bundle ids whose artifact `spawn_fake_remote_bundle_api` served, in
/// request order. Empty when the client fetched none.
pub fn recorded_artifact_fetches(dir: &Path) -> Vec<String> {
    fs::read_to_string(dir.join("artifact-fetches.txt"))
        .unwrap_or_default()
        .lines()
        .map(|line| line.to_string())
        .collect()
}

fn handle_fake_remote_bundle_request(
    stream: &mut std::net::TcpStream,
    dir: &Path,
) -> std::io::Result<()> {
    use std::io::{BufRead, BufReader, Read, Write};

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();

    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    if content_length > 0 {
        let mut sink = vec![0u8; content_length];
        reader.read_exact(&mut sink)?;
    }

    let path = target
        .split('?')
        .next()
        .unwrap_or_default()
        .trim_start_matches('/')
        .to_string();
    let segments: Vec<&str> = path.split('/').collect();
    let (status, response) = match (method.as_str(), segments.as_slice()) {
        ("GET", ["api", "v1", "projects", _, "export"]) => {
            match fs::read_to_string(dir.join("export.json")) {
                Ok(body) => (200, body),
                Err(_) => (
                    404,
                    "{\"errors\":{\"detail\":\"no export staged\"}}".to_string(),
                ),
            }
        }
        ("GET", ["api", "v1", "bundles", bundle_id]) => {
            let record = dir.join("artifact-fetches.txt");
            let mut fetched = fs::read_to_string(&record).unwrap_or_default();
            fetched.push_str(bundle_id);
            fetched.push('\n');
            fs::write(&record, fetched).unwrap();
            match fs::read_to_string(dir.join(format!("bundle-{bundle_id}.json"))) {
                Ok(body) => (200, body),
                Err(_) => (
                    404,
                    format!("{{\"errors\":{{\"detail\":\"no bundle {bundle_id} staged\"}}}}"),
                ),
            }
        }
        _ => (
            404,
            format!("{{\"errors\":{{\"detail\":\"unexpected endpoint {method} /{path}\"}}}}"),
        ),
    };
    write!(
        stream,
        "HTTP/1.1 {status} Fake\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}",
        response.len()
    )?;
    stream.flush()
}

fn respond_with_json(stream: &mut std::net::TcpStream, body: &str) -> std::io::Result<()> {
    use std::io::{BufRead, BufReader, Read, Write};

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    if content_length > 0 {
        let mut sink = vec![0u8; content_length];
        reader.read_exact(&mut sink)?;
    }
    write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    )?;
    stream.flush()
}

/// Spawn a fake remote push API: enough routes for `knit sync push --bundles`
/// and the archive/restore lifecycle sync. Every pushed bundle artifact's
/// payload state is appended to `<dir>/artifact-<slug>.states`, one state per
/// line, so tests can assert which lifecycle states reached the remote. The
/// full POSTed body of each artifact push is appended as one JSON line to
/// `<dir>/artifact-<slug>.bodies`, so tests can assert the force/lease fields.
///
/// Behavior markers in `dir`:
/// - `current-artifact-hash`: `GET /bundles/:id/artifacts` reports one artifact
///   with this hash (absent: empty list, i.e. no current artifact)
/// - `post-current-artifact-hash`: the hash the POST compare-and-swap checks a
///   supplied `expectedArtifactHash` against (defaults to `current-artifact-hash`;
///   set both differently to simulate a concurrent push between GET and POST)
/// - `enforce-fast-forward`: POSTs without `force: true` are refused with a
///   plain 409, like a remote whose ledger is ahead
/// - `reject-node-type`: artifact POSTs containing this node type fail with 503
pub fn spawn_fake_remote_push_api(dir: &Path) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    fs::create_dir_all(dir).unwrap();
    let dir = dir.to_path_buf();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let dir = dir.clone();
            std::thread::spawn(move || {
                let _ = handle_fake_remote_push_request(&mut stream, &dir);
            });
        }
    });
    base_url
}

fn handle_fake_remote_push_request(
    stream: &mut std::net::TcpStream,
    dir: &Path,
) -> std::io::Result<()> {
    use std::io::{BufRead, BufReader, Read, Write};

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();

    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);

    let path = target
        .split('?')
        .next()
        .unwrap_or_default()
        .trim_start_matches('/')
        .to_string();
    let segments: Vec<&str> = path.split('/').collect();
    if method == "PATCH" && matches!(segments.as_slice(), ["api", "v1", "projects", _]) {
        let record = dir.join("project-shape-writes.jsonl");
        let mut existing = fs::read_to_string(&record).unwrap_or_default();
        existing.push_str(&body.to_string());
        existing.push('\n');
        fs::write(record, existing)?;
    }
    let (status, response) = match (method.as_str(), segments.as_slice()) {
        ("GET", ["api", "v1", "me", "access-token"]) => (
            200,
            r#"{"data":{"scopes":["bundle:push","bundle:read"]}}"#.to_string(),
        ),
        ("GET", ["api", "v1", "me", "forge-credentials"]) => (200, r#"{"data":[]}"#.to_string()),
        // Project export: served from `<dir>/export.json` when a test staged
        // one, so prune's remote-orphan scan sees a configurable bundle list.
        ("GET", ["api", "v1", "projects", _, "export"]) => {
            match fs::read_to_string(dir.join("export.json")) {
                Ok(body) => (200, body),
                Err(_) => (
                    404,
                    "{\"errors\":{\"detail\":\"no export staged\"}}".to_string(),
                ),
            }
        }
        // One bundle with its artifact payload, the incremental door the
        // client uses when the export is slim: served from
        // `<dir>/bundle-<id>.json` when a test staged one.
        ("GET", ["api", "v1", "bundles", bundle_id]) => {
            match fs::read_to_string(dir.join(format!("bundle-{bundle_id}.json"))) {
                Ok(body) => (200, body),
                Err(_) => (
                    404,
                    format!("{{\"errors\":{{\"detail\":\"no bundle {bundle_id} staged\"}}}}"),
                ),
            }
        }
        ("PATCH", ["api", "v1", "bundles", bundle_id, "archive"]) => {
            fs::write(dir.join(format!("archived-{bundle_id}")), "").unwrap();
            let slug = bundle_id.trim_start_matches("rb-");
            (
                200,
                format!("{{\"data\":{{\"id\":\"{bundle_id}\",\"slug\":\"{slug}\"}}}}"),
            )
        }
        ("DELETE", ["api", "v1", "bundles", bundle_id]) => {
            fs::write(dir.join(format!("deleted-{bundle_id}")), "").unwrap();
            let slug = bundle_id.trim_start_matches("rb-");
            (
                200,
                format!("{{\"data\":{{\"id\":\"{bundle_id}\",\"slug\":\"{slug}\"}}}}"),
            )
        }
        // Stage `project-shape-forbidden` to play a project the caller can
        // read and push bundles into but not reshape (a collaborator, not
        // the owner) — PATCH refuses, GET still resolves the record.
        ("PATCH", ["api", "v1", "projects", _]) if dir.join("project-shape-forbidden").exists() => {
            (403, "{\"errors\":{\"detail\":\"Forbidden\"}}".to_string())
        }
        (_, ["api", "v1", "projects", slug]) => (
            200,
            format!("{{\"data\":{{\"id\":\"proj-1\",\"slug\":\"{slug}\"}}}}"),
        ),
        ("POST", ["api", "v1", "projects"]) => {
            fs::write(dir.join("project-created.txt"), "").unwrap();
            (
                201,
                "{\"data\":{\"id\":\"proj-1\",\"slug\":\"demo\"}}".to_string(),
            )
        }
        ("POST", ["api", "v1", "projects", _, "repositories"]) => {
            let record = dir.join("repositories-pushed.txt");
            let mut existing = fs::read_to_string(&record).unwrap_or_default();
            existing.push_str(&serde_json::to_string(&body).unwrap_or_default());
            existing.push('\n');
            fs::write(&record, existing).unwrap();
            (201, "{\"data\":{}}".to_string())
        }
        // Repository listing for `project push --prune`: served from a staged
        // `repositories.json` (the `data` array) so a test can present remote
        // orphans; empty when none staged.
        ("GET", ["api", "v1", "projects", _, "repositories"]) => {
            match fs::read_to_string(dir.join("repositories.json")) {
                Ok(body) => (200, body),
                Err(_) => (200, "{\"data\":[]}".to_string()),
            }
        }
        // Repository delete: record each pruned repository id, one per line.
        ("DELETE", ["api", "v1", "projects", _, "repositories", repo_id]) => {
            let record = dir.join("deleted-repositories.txt");
            let mut existing = fs::read_to_string(&record).unwrap_or_default();
            existing.push_str(repo_id);
            existing.push('\n');
            fs::write(&record, existing).unwrap();
            (200, format!("{{\"data\":{{\"id\":\"{repo_id}\"}}}}"))
        }
        ("POST", ["api", "v1", "projects", _, "history-events"]) => {
            // Every history push is recorded, one line per request, so a
            // test can see how many requests a sync made and which events
            // rode in each.
            let record = dir.join("history-pushes.jsonl");
            let mut existing = fs::read_to_string(&record).unwrap_or_default();
            existing.push_str(&body.to_string());
            existing.push('\n');
            fs::write(&record, existing).unwrap();
            let count = body["events"].as_array().map(Vec::len).unwrap_or(0);
            (
                201,
                format!("{{\"data\":{{\"insertedCount\":{count},\"skippedCount\":0}}}}"),
            )
        }
        ("POST", ["api", "v1", "projects", _, "bundles"]) => {
            let slug = body["slug"].as_str().unwrap_or("unknown").to_string();
            (
                201,
                format!("{{\"data\":{{\"id\":\"rb-{slug}\",\"slug\":\"{slug}\"}}}}"),
            )
        }
        ("GET", ["api", "v1", "bundles", _, "artifacts"]) => {
            match fs::read_to_string(dir.join("current-artifact-hash")) {
                Ok(hash) => (
                    200,
                    format!(
                        "{{\"data\":[{{\"id\":\"art-0\",\"artifactHash\":\"{}\"}}]}}",
                        hash.trim()
                    ),
                ),
                Err(_) => (200, "{\"data\":[]}".to_string()),
            }
        }
        ("POST", ["api", "v1", "bundles", bundle_id, "artifacts"]) => {
            let slug = bundle_id.trim_start_matches("rb-");
            let state = body["payload"]["state"].as_str().unwrap_or("unset");
            let record = dir.join(format!("artifact-{slug}.states"));
            let mut existing = fs::read_to_string(&record).unwrap_or_default();
            existing.push_str(state);
            existing.push('\n');
            fs::write(&record, existing).unwrap();
            let bodies = dir.join(format!("artifact-{slug}.bodies"));
            let mut recorded = fs::read_to_string(&bodies).unwrap_or_default();
            recorded.push_str(&serde_json::to_string(&body).unwrap());
            recorded.push('\n');
            fs::write(&bodies, recorded).unwrap();

            let force = body["force"].as_bool().unwrap_or(false);
            let expected = body["expectedArtifactHash"].as_str();
            let server_hash = fs::read_to_string(dir.join("post-current-artifact-hash"))
                .or_else(|_| fs::read_to_string(dir.join("current-artifact-hash")))
                .ok()
                .map(|hash| hash.trim().to_string());
            let accepted =
                "{\"data\":{\"id\":\"art-1\",\"artifactHash\":\"fakehash\"}}".to_string();
            let rejected_node = fs::read_to_string(dir.join("reject-node-type")).ok();
            let reject = rejected_node.as_deref().is_some_and(|kind| {
                body["payload"]["nodes"].as_array().is_some_and(|nodes| {
                    nodes
                        .iter()
                        .any(|node| node["type"].as_str() == Some(kind.trim()))
                })
            });
            if reject {
                (503, r#"{"error":{"kind":"unavailable","message":"injected artifact publication failure"}}"#.to_string())
            } else if let Some(expected) = expected {
                // Compare-and-swap: accept only when the lease matches the
                // hash this server currently holds.
                if force && server_hash.as_deref() == Some(expected) {
                    (201, accepted)
                } else {
                    let current = server_hash
                        .map(|hash| format!("\"{hash}\""))
                        .unwrap_or_else(|| "null".to_string());
                    (
                        409,
                        format!(
                            "{{\"error\":{{\"kind\":\"leaseMismatch\",\"message\":\"the bundle's current artifact is not the leased one\",\"currentArtifactHash\":{current}}}}}"
                        ),
                    )
                }
            } else if !force && dir.join("enforce-fast-forward").exists() {
                (
                    409,
                    "{\"error\":{\"kind\":\"conflict\",\"message\":\"artifact is not a fast-forward of the current ledger\"}}"
                        .to_string(),
                )
            } else {
                (201, accepted)
            }
        }
        _ => (
            404,
            format!("{{\"errors\":{{\"detail\":\"unexpected endpoint {method} /{path}\"}}}}"),
        ),
    };
    write!(
        stream,
        "HTTP/1.1 {status} Fake\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}",
        response.len()
    )?;
    stream.flush()
}

/// Run knit capturing stdout and stderr separately: `--json` commands put the
/// machine-readable document alone on stdout with human lines on stderr, which
/// the combined-output helpers cannot assert. Ambient remote credentials and
/// git config are scrubbed so a developer's real setup never leaks in;
/// test-provided env is applied last and wins.
pub fn knit_split_output(
    cwd: &Path,
    args: &[&str],
    env: &[(&str, &str)],
) -> (String, String, bool) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_knit"));
    command
        .args(args)
        .current_dir(cwd)
        .env("KNIT_HOME", isolated_knit_home())
        .env("GIT_CONFIG_GLOBAL", isolated_git_config_global())
        .env_remove("KNIT_BUNDLE")
        .env_remove("KNIT_SESSION")
        .env_remove("KNIT_REMOTE_URL")
        .env_remove("KNIT_REMOTE_TOKEN")
        .env_remove("KNIT_REMOTE_HOSTED_TOKEN");
    scrub_ambient_git_identity(&mut command);
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().unwrap();
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.success(),
    )
}

/// Basic-auth header accepted by the legacy dumb-HTTP fixture.
pub const FAKE_REMOTE_GIT_AUTH: &str = "Basic eC1hY2Nlc3MtdG9rZW46dmVuZGVkLXBhc3M=";

/// Spawn a fake sync-remote API covering project export/views, the generic
/// forge credential endpoint, and legacy dumb-HTTP fixture files.
pub fn spawn_fake_remote_api(dir: &Path, export_body: String) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join("export.json"), export_body).unwrap();
    let dir = dir.to_path_buf();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let dir = dir.clone();
            std::thread::spawn(move || {
                let _ = handle_fake_remote_request(&mut stream, &dir);
            });
        }
    });
    base_url
}

fn handle_fake_remote_request(stream: &mut std::net::TcpStream, dir: &Path) -> std::io::Result<()> {
    use std::io::{BufRead, BufReader, Read, Write};

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();

    let mut content_length = 0usize;
    let mut authorization = String::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            match name.trim().to_ascii_lowercase().as_str() {
                "content-length" => content_length = value.trim().parse().unwrap_or(0),
                "authorization" => authorization = value.trim().to_string(),
                _ => {}
            }
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    let body = String::from_utf8_lossy(&body).to_string();

    let path = target.split('?').next().unwrap_or_default().to_string();

    // Dumb-HTTP git fixture. Secure forge helpers deliberately never service
    // this transport.
    if path.starts_with("/git/") {
        if authorization != FAKE_REMOTE_GIT_AUTH {
            let body = b"auth required";
            write!(
                stream,
                "HTTP/1.1 401 Unauthorized\r\nwww-authenticate: Basic realm=\"fake-remote\"\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            )?;
            stream.write_all(body)?;
            return stream.flush();
        }
        let file_path = dir.join(path.trim_start_matches('/'));
        return match fs::read(&file_path) {
            Ok(bytes) => {
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    bytes.len()
                )?;
                stream.write_all(&bytes)?;
                stream.flush()
            }
            Err(_) => {
                write!(
                    stream,
                    "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                )?;
                stream.flush()
            }
        };
    }

    let (status, response) = match (method.as_str(), path.as_str()) {
        ("GET", "/api/v1/me/forge-credentials") => (
            200,
            "{\"data\":[{\"forge\":\"test-forge\",\"hosts\":[\"code.example.test\"],\"connected\":true},{\"forge\":\"other\",\"hosts\":[\"off.example.test\"],\"connected\":false}]}"
                .to_string(),
        ),
        ("POST", "/api/v1/me/forge-credentials/git") => {
            let mut log = fs::read_to_string(dir.join("vend-requests.txt")).unwrap_or_default();
            log.push_str(&format!("{body}\n"));
            fs::write(dir.join("vend-requests.txt"), log).unwrap();
            if body.contains("\"protocol\":\"https\"")
                && body.contains("\"host\":\"code.example.test\"")
            {
                (
                    200,
                    "{\"data\":{\"username\":\"forge-user\",\"password\":\"forge-secret\",\"forge\":\"test-forge\",\"actsAs\":\"alice\"}}"
                        .to_string(),
                )
            } else {
                (
                    404,
                    "{\"error\":{\"kind\":\"unsupportedForgeHost\",\"message\":\"unsupported host\"}}"
                        .to_string(),
                )
            }
        }
        ("GET", path) if path.starts_with("/api/v1/projects/") && path.ends_with("/export") => {
            let export = fs::read_to_string(dir.join("export.json")).unwrap_or_default();
            (200, export)
        }
        ("GET", path) if path.starts_with("/api/v1/projects/") && path.ends_with("/view") => {
            (200, "{\"data\":{\"views\":{}}}".to_string())
        }
        _ => (
            404,
            format!("{{\"error\":{{\"kind\":\"notFound\",\"message\":\"unexpected {method} {path}\"}}}}"),
        ),
    };
    write!(
        stream,
        "HTTP/1.1 {status} Fake\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}",
        response.len()
    )?;
    stream.flush()
}

/// The history pushes the fake push remote received: one entry per request,
/// each the list of event ids that request carried.
pub fn recorded_history_pushes(dir: &Path) -> Vec<Vec<String>> {
    fs::read_to_string(dir.join("history-pushes.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let body: serde_json::Value = serde_json::from_str(line).unwrap();
            body["events"]
                .as_array()
                .map(|events| {
                    events
                        .iter()
                        .filter_map(|event| event["eventId"].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        })
        .collect()
}
