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
        ["publish", "github", "create", "--no-sync"],
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
    let path = std::env::temp_dir().join(format!(
        "knit-smoke-{}-{nanos}-{counter}",
        std::process::id()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

pub fn init_repo(path: &Path, label: &str) {
    fs::create_dir_all(path).unwrap();
    git(path, ["init"]);
    git(path, ["checkout", "-b", "main"]);
    git(path, ["config", "user.email", "knit@example.test"]);
    git(path, ["config", "user.name", "Knit Smoke"]);
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
    git(
        root,
        ["clone", remote.to_str().unwrap(), local.to_str().unwrap()],
    );
    configure_git_user(&local);

    let collaborator = root.join(format!("{label}-collaborator"));
    git(
        root,
        [
            "clone",
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

#[cfg(unix)]
pub fn install_parallel_push_hook(repo: &Path, gate: &Path, id: &str, peer: &str) {
    install_parallel_gate_hook(repo, "pre-push", gate, id, peer);
}

#[cfg(unix)]
pub fn install_parallel_gate_hook(repo: &Path, hook: &str, gate: &Path, id: &str, peer: &str) {
    use std::os::unix::fs::PermissionsExt;

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
    let mut permissions = fs::metadata(&hook_path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&hook_path, permissions).unwrap();
}

#[cfg(unix)]
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
    let path = format!("{}:{}", fake_bin.display(), old_path.to_string_lossy());
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
    let path = format!("{}:{}", fake_bin.display(), old_path.to_string_lossy());
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

#[cfg(unix)]
pub fn write_fake_gh(fake_bin: &Path, fake_gh_dir: &Path) {
    use std::os::unix::fs::PermissionsExt;

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
    let mut permissions = fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script, permissions).unwrap();
}

#[cfg(not(unix))]
pub fn write_fake_gh(_fake_bin: &Path, _fake_gh_dir: &Path) {
    panic!("fake gh smoke test requires a unix-like shell");
}

#[cfg(unix)]
pub fn write_fake_curl(fake_bin: &Path, fake_gh_dir: &Path) {
    use std::os::unix::fs::PermissionsExt;

    fs::create_dir_all(fake_bin).unwrap();
    fs::create_dir_all(fake_gh_dir).unwrap();
    let script = fake_bin.join("curl");
    fs::write(
        &script,
        r#"#!/bin/sh
set -eu

api_pr_json() {
  number="$1"
  state="open"
  merged="false"
  if [ -f "$GH_FAKE_DIR/merged-backend" ]; then
    state="closed"
    merged="true"
  fi
  printf '{"number":%s,"html_url":"https://github.com/acme/backend/pull/%s","state":"%s","title":"backend PR","body":"Existing body","draft":false,"head":{"ref":"knit/artifact-publish","sha":"backend-head"},"base":{"ref":"main"},"merged":%s,"mergeable":true,"mergeable_state":"clean"}\n' "$number" "$number" "$state" "$merged"
}

method="GET"
url=""
netrc=""
ipv4=0
data=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --request)
      method="$2"
      shift 2
      ;;
    --netrc-file)
      netrc="$2"
      shift 2
      ;;
    --ipv4)
      ipv4=1
      shift
      ;;
    --data-binary)
      data="$2"
      shift 2
      ;;
    --header|--connect-timeout|--max-time)
      shift 2
      ;;
    --silent|--show-error|--fail-with-body|--location)
      shift
      ;;
    http*)
      url="$1"
      shift
      ;;
    *)
      shift
      ;;
  esac
done

printf '%s\n' "$ipv4" > "$GH_FAKE_DIR/curl.ipv4"
if [ -n "$netrc" ]; then
  cat "$netrc" > "$GH_FAKE_DIR/curl.netrc"
fi

endpoint="${url#https://api.github.com/}"
endpoint_path="${endpoint%%\?*}"

case "$endpoint_path" in
  repos/acme/backend/pulls)
    if [ "$method" = "GET" ]; then
      printf '[]\n'
    elif [ "$method" = "POST" ]; then
      if [ "$data" = "@-" ]; then
        cat > "$GH_FAKE_DIR/curl-backend-create.json"
      fi
      api_pr_json 101
    else
      echo "unexpected curl method for pull collection: $method" >&2
      exit 1
    fi
    ;;
  repos/acme/backend/pulls/*/merge)
    if [ "$method" = "PUT" ]; then
      if [ "$data" = "@-" ]; then
        cat > "$GH_FAKE_DIR/curl-backend-merge.json"
      fi
      touch "$GH_FAKE_DIR/merged-backend"
      printf '{"merged":true,"message":"Pull Request successfully merged","sha":"merge-sha"}\n'
    else
      echo "unexpected curl method for merge: $method" >&2
      exit 1
    fi
    ;;
  repos/acme/backend/pulls/*)
    if [ "$method" = "GET" ]; then
      api_pr_json "${endpoint_path##*/}"
    elif [ "$method" = "PATCH" ]; then
      if [ "$data" = "@-" ]; then
        cat > "$GH_FAKE_DIR/curl-backend-edit.json"
      fi
      api_pr_json "${endpoint_path##*/}"
    else
      echo "unexpected curl method for pull item: $method" >&2
      exit 1
    fi
    ;;
  repos/acme/backend/commits/*/check-runs)
    if [ "$method" = "GET" ]; then
      printf '{"total_count":0,"check_runs":[]}\n'
    else
      echo "unexpected curl method for check runs: $method" >&2
      exit 1
    fi
    ;;
  repos/acme/backend/commits/*/status)
    if [ "$method" = "GET" ]; then
      printf '{"state":"success","statuses":[]}\n'
    else
      echo "unexpected curl method for statuses: $method" >&2
      exit 1
    fi
    ;;
  *)
    echo "unexpected curl endpoint: $endpoint" >&2
    exit 1
    ;;
esac
"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script, permissions).unwrap();
}

#[cfg(not(unix))]
pub fn write_fake_curl(_fake_bin: &Path, _fake_gh_dir: &Path) {
    panic!("fake curl smoke test requires a unix-like shell");
}
