use crate::model::CommitAuthor;
use anyhow::{bail, Context, Result};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub fn git_root(path: &Path) -> Result<PathBuf> {
    let path = crate::paths::canonicalize(path)
        .with_context(|| format!("failed to resolve {}", path.display()))?;
    let output = git_output(&path, ["rev-parse", "--show-toplevel"])?;
    Ok(crate::paths::canonicalize(PathBuf::from(output.trim()))
        .with_context(|| format!("failed to resolve git root {}", output.trim()))?)
}

pub fn current_branch(repo: &Path) -> Result<Option<String>> {
    let branch = git_output(repo, ["branch", "--show-current"])?;
    let branch = branch.trim();
    if branch.is_empty() {
        Ok(None)
    } else {
        Ok(Some(branch.to_string()))
    }
}

pub fn infer_base_branch(repo: &Path, current_branch: Option<&str>) -> Result<String> {
    let clean = git_output(repo, ["status", "--short"])?.trim().is_empty();
    if clean {
        if matches!(current_branch, Some("main" | "master")) {
            return Ok(current_branch.unwrap().to_string());
        }
    }

    if ref_exists(repo, "main") || ref_exists(repo, "origin/main") {
        return Ok("main".to_string());
    }
    if ref_exists(repo, "master") || ref_exists(repo, "origin/master") {
        return Ok("master".to_string());
    }

    bail!("Could not infer a base branch. Pass --base <branch>.");
}

pub fn resolve_base_ref(repo: &Path, base_branch: &str) -> String {
    if ref_exists(repo, base_branch) {
        base_branch.to_string()
    } else if ref_exists(repo, &format!("origin/{base_branch}")) {
        format!("origin/{base_branch}")
    } else {
        base_branch.to_string()
    }
}

pub fn branch_exists(repo: &Path, branch: &str) -> bool {
    git_success(
        repo,
        [
            OsString::from("show-ref"),
            OsString::from("--verify"),
            OsString::from("--quiet"),
            OsString::from(format!("refs/heads/{branch}")),
        ],
    )
}

pub fn ref_exists(repo: &Path, reference: &str) -> bool {
    git_success(
        repo,
        [
            OsString::from("rev-parse"),
            OsString::from("--verify"),
            OsString::from("--quiet"),
            OsString::from(format!("{reference}^{{commit}}")),
        ],
    )
}

pub fn is_git_worktree(path: &Path) -> bool {
    git_success(path, ["rev-parse", "--is-inside-work-tree"])
}

pub fn rev_parse(cwd: &Path, reference: &str) -> Result<String> {
    Ok(git_output(cwd, ["rev-parse", reference])?
        .trim()
        .to_string())
}

/// Reads the recorded Git author (name + email) of a commit. Reads the actual
/// commit, so it reflects per-repo `user.name`/`user.email` rather than guessing.
pub fn commit_author(cwd: &Path, sha: &str) -> Result<CommitAuthor> {
    let output = git_output(
        cwd,
        [
            OsString::from("show"),
            OsString::from("-s"),
            OsString::from("--format=%an%n%ae"),
            OsString::from(sha),
        ],
    )?;

    let mut lines = output.lines();
    let name = lines.next().unwrap_or_default().trim().to_string();
    let email = lines.next().unwrap_or_default().trim().to_string();

    Ok(CommitAuthor { name, email })
}

pub fn rev_list(cwd: &Path, before_sha: &str, after_sha: &str) -> Result<Vec<String>> {
    if before_sha == after_sha {
        return Ok(Vec::new());
    }

    let range = format!("{before_sha}..{after_sha}");
    let output = git_output(
        cwd,
        [
            OsString::from("rev-list"),
            OsString::from("--reverse"),
            OsString::from(range),
        ],
    )?;

    Ok(output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect())
}

pub fn is_ancestor(cwd: &Path, ancestor: &str, descendant: &str) -> bool {
    git_success(cwd, ["merge-base", "--is-ancestor", ancestor, descendant])
}

/// Commit SHA a reference resolves to (peeling annotated tags); `None` when the
/// reference does not exist.
pub fn ref_commit_sha(cwd: &Path, reference: &str) -> Result<Option<String>> {
    git_output_optional(
        cwd,
        ["rev-parse", "--verify", &format!("{reference}^{{commit}}")],
    )
}

/// Commit SHA a ref points to on `remote` via ls-remote; prefers the peeled
/// `^{}` line so annotated tags resolve to their commit. `None` when the ref is
/// absent on the remote; `Err` when the remote is unreachable.
pub fn remote_ref_sha(cwd: &Path, remote: &str, reference: &str) -> Result<Option<String>> {
    // The peeled line is only emitted for patterns that explicitly request it,
    // so ask for both the ref and its `^{}` form.
    let peeled = format!("{reference}^{{}}");
    let output = git_output(cwd, ["ls-remote", remote, reference, &peeled])?;
    let mut plain = None;
    for line in output.lines() {
        let mut parts = line.split_whitespace();
        let (Some(sha), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        if name.ends_with("^{}") {
            return Ok(Some(sha.to_string()));
        }
        plain = Some(sha.to_string());
    }
    Ok(plain)
}

pub fn merge_base(cwd: &Path, left: &str, right: &str) -> Result<Option<String>> {
    git_output_optional(cwd, ["merge-base", left, right])
}

pub fn git_output<I, S>(cwd: &Path, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args = collect_args(args);
    let output = Command::new("git")
        .args(&args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("failed to run git in {}", cwd.display()))?;

    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    bail!(
        "git {} failed in {}: {}",
        display_args(&args),
        cwd.display(),
        detail
    );
}

pub fn git_output_optional<I, S>(cwd: &Path, args: I) -> Result<Option<String>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args = collect_args(args);
    let output = Command::new("git")
        .args(&args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("failed to run git in {}", cwd.display()))?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Ok((!stdout.is_empty()).then_some(stdout));
    }

    Ok(None)
}

fn git_success<I, S>(cwd: &Path, args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args = collect_args(args);
    Command::new("git")
        .args(&args)
        .current_dir(cwd)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn collect_args<I, S>(args: I) -> Vec<OsString>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    args.into_iter()
        .map(|arg| arg.as_ref().to_os_string())
        .collect()
}

fn display_args(args: &[OsString]) -> String {
    args.iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}
