use crate::model::{CommitAuthor, CommitDetail};
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseBranchInference {
    pub branch: String,
    pub source: String,
}

pub fn infer_base_branch(repo: &Path, current_branch: Option<&str>) -> Result<BaseBranchInference> {
    if let Some(remote_head) = git_output_optional(
        repo,
        [
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    )? {
        if let Some(branch) = remote_head.trim().strip_prefix("origin/") {
            if !branch.is_empty() && ref_exists(repo, &remote_head) {
                return Ok(BaseBranchInference {
                    branch: branch.to_string(),
                    source: "cached origin/HEAD".to_string(),
                });
            }
        }
    }

    if git_output_optional(repo, ["remote", "get-url", "origin"])?.is_some() {
        if let Ok(remote_head) = git_output_with_env(
            repo,
            ["ls-remote", "--symref", "origin", "HEAD"],
            &[("GIT_TERMINAL_PROMPT", "0")],
        ) {
            if let Some(branch) = parse_remote_head(&remote_head) {
                return Ok(BaseBranchInference {
                    branch,
                    source: "origin's default branch".to_string(),
                });
            }
        }
    }

    let clean = git_output(repo, ["status", "--short"])?.trim().is_empty();
    if clean {
        if let Some(current) = current_branch {
            let upstream = git_output_optional(
                repo,
                [
                    "rev-parse",
                    "--abbrev-ref",
                    "--symbolic-full-name",
                    "@{upstream}",
                ],
            )?;
            if upstream.as_deref() == Some(format!("origin/{current}").as_str()) {
                return Ok(BaseBranchInference {
                    branch: current.to_string(),
                    source: format!("current branch tracking origin/{current}"),
                });
            }
        }
    }

    let conventional = ["main", "master"]
        .into_iter()
        .filter(|branch| ref_exists(repo, branch) || ref_exists(repo, &format!("origin/{branch}")))
        .collect::<Vec<_>>();

    if clean {
        if let Some(current) = current_branch {
            if !matches!(current, "main" | "master") {
                if conventional.is_empty() {
                    return Ok(BaseBranchInference {
                        branch: current.to_string(),
                        source: "only plausible clean current branch".to_string(),
                    });
                }
                bail!(
                    "Could not infer a base branch safely: clean current branch `{current}` conflicts with existing `{}`. Pass --base <branch>.",
                    conventional.join("` and `")
                );
            }
        }
    }

    if conventional.len() == 1 {
        return Ok(BaseBranchInference {
            branch: conventional[0].to_string(),
            source: "only conventional branch".to_string(),
        });
    }
    if conventional.len() > 1 {
        bail!(
            "Could not infer a base branch safely: both `main` and `master` exist. Pass --base <branch>."
        );
    }

    bail!("Could not infer a base branch. Pass --base <branch>.");
}

fn parse_remote_head(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let line = line.trim();
        let branch = line
            .strip_prefix("ref: refs/heads/")?
            .strip_suffix("\tHEAD")?;
        (!branch.is_empty()).then(|| branch.to_string())
    })
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

/// Subject line and author date of each requested commit that this repo still
/// has. Best effort: unknown or unreadable commits are simply absent from the
/// result, and a repo that is not a usable checkout yields an empty map.
/// Batched so naming a long commit list costs a bounded number of git calls.
pub fn commit_details(cwd: &Path, shas: &[String]) -> BTreeMap<String, CommitDetail> {
    let wanted = shas.iter().cloned().collect::<BTreeSet<_>>();
    let mut details = BTreeMap::new();
    // Chunked so a large batch cannot overflow the platform argument limit.
    for chunk in wanted.iter().cloned().collect::<Vec<_>>().chunks(200) {
        let mut args = vec![
            OsString::from("log"),
            OsString::from("--no-walk"),
            // Missing commits must not fail the whole batch; without this a
            // single pruned sha would cost every other commit its detail.
            OsString::from("--ignore-missing"),
            // Author dates keep the author's UTC offset, but ledger timestamps
            // are compared as strings, so mixed offsets would misorder events.
            // format-local under TZ=UTC pins every date to the ledger's form.
            OsString::from("--date=format-local:%Y-%m-%dT%H:%M:%SZ"),
            OsString::from("--format=%H %ad %s"),
        ];
        args.extend(chunk.iter().map(OsString::from));
        let Ok(output) = git_output_with_env(cwd, args, &[("TZ", "UTC")]) else {
            continue;
        };
        for line in output.lines() {
            let mut parts = line.trim_end().splitn(3, ' ');
            let (Some(sha), Some(authored_at)) = (parts.next(), parts.next()) else {
                continue;
            };
            // With every requested sha missing, git log falls back to HEAD, so
            // only commits that were actually asked for are recorded.
            if !wanted.contains(sha) {
                continue;
            }
            let subject = parts.next().unwrap_or_default();
            details.insert(
                sha.to_string(),
                CommitDetail {
                    subject: subject.lines().next().unwrap_or_default().to_string(),
                    authored_at: authored_at.to_string(),
                },
            );
        }
    }
    details
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

/// `--color` flag for git display output (diff, show) that knit captures and
/// reprints. The capture pipe hides the terminal from git, so git's auto mode
/// would strip color even when knit itself is coloring; forcing here keeps
/// git's color decision aligned with knit's. The flag route is deliberate:
/// since git 2.17, `color.ui=always` in config behaves like `auto`.
pub fn display_color_args() -> Vec<OsString> {
    if crate::output::should_color() {
        vec![OsString::from("--color=always")]
    } else {
        Vec::new()
    }
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

/// Like `git_output`, but with extra environment variables set for the child
/// git process only (e.g. `GIT_TERMINAL_PROMPT=0` for non-interactive remote
/// queries). Never used to pass credentials.
pub fn git_output_with_env<I, S>(cwd: &Path, args: I, envs: &[(&str, &str)]) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args = collect_args(args);
    let mut command = Command::new("git");
    command.args(&args).current_dir(cwd);
    for (key, value) in envs {
        command.env(key, value);
    }
    let output = command
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
