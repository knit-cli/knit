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
    knit(workspace, ["init", "arbient"]);
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

pub fn install_parallel_gate_hook(repo: &Path, hook: &str, gate: &Path, id: &str, peer: &str) {
    fs::create_dir_all(gate).unwrap();
    let hook_path = git(repo, ["rev-parse", "--git-path", &format!("hooks/{hook}")]);
    let hook_path = PathBuf::from(hook_path.trim());
    let hook_path = if hook_path.is_absolute() {
        hook_path
    } else {
        repo.join(hook_path)
    };
    fs::create_dir_all(hook_path.parent().unwrap()).unwrap();
    fs::write(
        &hook_path,
        format!(
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
    )
    .unwrap();
    make_executable(&hook_path);
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

pub fn knit<I, S>(cwd: &Path, args: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new(env!("CARGO_BIN_EXE_knit"));
    command
        .args(args)
        .current_dir(cwd)
        .env("KNIT_HOME", isolated_knit_home());
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
        .env("KNIT_HOME", isolated_knit_home());
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
        .env("KNIT_HOME", isolated_knit_home());
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
        .env("KNIT_HOME", isolated_knit_home());
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
        .env("PATH", path)
        .env("GH_FAKE_DIR", fake_gh_dir);
    for (key, value) in env {
        command.env(key, value);
    }
    run(command)
}

pub fn knit_fails_with_fake_gh<I, S>(cwd: &Path, args: I, fake_bin: &Path, fake_gh_dir: &Path) -> String
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
        .env("PATH", path)
        .env("GH_FAKE_DIR", fake_gh_dir);
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

pub fn git<I, S>(cwd: &Path, args: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new("git");
    command.args(args).current_dir(cwd);
    run(command)
}

pub fn git_success<I, S>(cwd: &Path, args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new("git");
    command.args(args).current_dir(cwd);
    command.stdout(Stdio::null()).stderr(Stdio::null());
    command.status().unwrap().success()
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
  mergeable="true"
  mergestate="clean"
  if [ -f "$GH_FAKE_DIR/conflict-$pr_repo" ]; then
    mergeable="false"
    mergestate="dirty"
  fi
  printf '{"number":%s,"html_url":"https://github.com/acme/%s/pull/%s","state":"%s","title":"%s PR","body":"Existing body","draft":false,"head":{"ref":"%s","sha":"%s-head"},"base":{"ref":"%s"},"merged":%s,"mergeable":%s,"mergeable_state":"%s"}\n' "$number" "$pr_repo" "$number" "$state" "$pr_repo" "$head" "$pr_repo" "$base" "$merged" "$mergeable" "$mergestate"
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
    printf '[]\n'
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
    case "$repo" in
      backend) number=101 ;;
      frontend) number=202 ;;
      *) number=303 ;;
    esac
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
    tail="${url#https://github.com/acme/}"
    pr_repo="${tail%%/*}"
    cat > "$GH_FAKE_DIR/edit-$pr_repo.md"
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
        fs::write(shim, "@sh \"%~dp0{}\" %*\r\n".replace("{}", &script.file_name().unwrap().to_string_lossy())).unwrap();
    }
    #[cfg(not(windows))]
    let _ = script;
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

fn fake_github_pr_json(dir: &Path, number: &str) -> String {
    let merged = dir.join("merged-backend").exists();
    let (state, merged_flag) = if merged {
        ("closed", "true")
    } else {
        ("open", "false")
    };
    format!(
        "{{\"number\":{number},\"html_url\":\"https://github.com/acme/backend/pull/{number}\",\"state\":\"{state}\",\"title\":\"backend PR\",\"body\":\"Existing body\",\"draft\":false,\"head\":{{\"ref\":\"knit/artifact-publish\",\"sha\":\"backend-head\"}},\"base\":{{\"ref\":\"main\"}},\"merged\":{merged_flag},\"mergeable\":true,\"mergeable_state\":\"clean\"}}"
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
            (200, fake_github_pr_json(dir, number))
        }
        ("GET", ["repos", "acme", "backend", "commits", _, "check-runs"]) => (
            200,
            "{\"total_count\":0,\"check_runs\":[]}".to_string(),
        ),
        ("GET", ["repos", "acme", "backend", "commits", _, "status"]) => (
            200,
            "{\"state\":\"success\",\"statuses\":[]}".to_string(),
        ),
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