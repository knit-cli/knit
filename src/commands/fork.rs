use crate::contribution::same_repository;
use crate::git::{git_output, git_output_optional};
use crate::model::{KnitProject, ProjectRepoEntry};
use crate::providers::{github::GitHub, Forge, ForkRepository, PrTarget};
use crate::store::{find_knit_root, load_config, project_path, read_json};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForkMode {
    Auto,
    Always,
    Never,
}

impl ForkMode {
    pub fn from_flags(fork: bool, no_fork: bool) -> Self {
        if fork {
            Self::Always
        } else if no_fork {
            Self::Never
        } else {
            Self::Auto
        }
    }
}

enum PushAccess {
    Yes,
    No,
    Unknown(anyhow::Error),
}

fn push_access(cwd: &Path, id: &str) -> Result<(String, PushAccess)> {
    let upstream = git_output(cwd, ["config", "--get", "remote.origin.url"])?;
    let upstream = upstream.trim();
    let name = crate::auth::remote_target(upstream)
        .map(|(_, name)| name)
        .unwrap_or_else(|_| id.to_owned());
    let Ok(full_name) = crate::contribution::github_name(upstream) else {
        return Ok((
            name,
            PushAccess::Unknown(anyhow::anyhow!("not a github.com repository")),
        ));
    };
    let mut target = PrTarget::explicit(cwd, &full_name);
    target.repo_remote = Some(upstream.to_owned());
    let access = match GitHub.can_push(&target) {
        Ok(Some(true)) => PushAccess::Yes,
        Ok(Some(false)) => PushAccess::No,
        Ok(None) => PushAccess::Unknown(anyhow::anyhow!("GitHub did not report push permission")),
        Err(error) if missing_credentials(&error) => PushAccess::Unknown(error),
        Err(error) => return Err(error),
    };
    Ok((name, access))
}

pub fn apply_mode(
    repo: &ProjectRepoEntry,
    mode: ForkMode,
    owner: Option<&str>,
    announced: &mut bool,
) -> Result<()> {
    if mode == ForkMode::Never {
        println!("{}: direct (--no-fork)", repo.id);
        return Ok(());
    }
    let cwd = Path::new(&repo.path);
    if mode == ForkMode::Auto {
        let (name, access) = push_access(cwd, &repo.id)?;
        let unknown = || {
            anyhow::anyhow!(
            "Knit can't tell whether you can push to {name}; sign in with `knit auth` or pass --fork or --no-fork"
        )
        };
        match access {
            PushAccess::Yes => {
                println!("{}: direct (you can push)", repo.id);
                return Ok(());
            }
            PushAccess::No => {}
            PushAccess::Unknown(error) if missing_credentials(&error) => {
                return Err(error.context(unknown()))
            }
            PushAccess::Unknown(_) => return Err(unknown()),
        }
    }
    let result = fork_one(&repo.id, cwd, owner, false, false, announced)?;
    println!(
        "{}: via fork {}",
        repo.id,
        result["fork"].as_str().unwrap_or_default()
    );
    Ok(())
}

/// Where a fresh checkout of a project member pushes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckoutPush {
    Direct,
    Fork(String),
    Unknown,
}

impl CheckoutPush {
    pub fn status(&self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Fork(_) => "fork",
            Self::Unknown => "unknown",
        }
    }

    pub fn fork(&self) -> Option<&str> {
        match self {
            Self::Fork(name) => Some(name),
            _ => None,
        }
    }
}

/// The fork-or-direct decision for members cloned from a sync remote. Every
/// person decides for themselves at checkout, so the shared project never
/// records a fork.
pub struct CheckoutForks {
    mode: ForkMode,
    owner: Option<String>,
    announced: Mutex<bool>,
    outcomes: Mutex<BTreeMap<String, CheckoutPush>>,
}

static CHECKOUT_FORKS: OnceLock<CheckoutForks> = OnceLock::new();

/// Set once from the command line, before any repository is cloned.
pub fn set_checkout_forks(mode: ForkMode, owner: Option<String>) {
    let _ = CHECKOUT_FORKS.set(CheckoutForks::new(mode, owner));
}

pub fn checkout_forks() -> &'static CheckoutForks {
    CHECKOUT_FORKS.get_or_init(|| CheckoutForks::new(ForkMode::Auto, None))
}

impl CheckoutForks {
    fn new(mode: ForkMode, owner: Option<String>) -> Self {
        Self {
            mode,
            owner,
            announced: Mutex::new(false),
            outcomes: Mutex::new(BTreeMap::new()),
        }
    }

    /// Unlike `knit init`, an unknown answer does not fail: the clone already
    /// succeeded and its owner may only want to read. Only GitHub repositories
    /// can be forked, so other hosts keep pushing upstream.
    pub fn apply(&self, id: &str, cwd: &Path) -> Result<()> {
        let upstream = git_output(cwd, ["config", "--get", "remote.origin.url"])?;
        let upstream = upstream.trim();
        if self.mode != ForkMode::Always && crate::contribution::github_name(upstream).is_err() {
            return Ok(());
        }
        let push = match self.mode {
            ForkMode::Never => CheckoutPush::Direct,
            ForkMode::Auto => match push_access(cwd, id) {
                Ok((_, PushAccess::Yes)) => CheckoutPush::Direct,
                Ok((_, PushAccess::No)) => self.fork(id, cwd)?,
                Ok((_, PushAccess::Unknown(_))) | Err(_) => {
                    let name = crate::auth::remote_target(upstream)
                        .map(|(_, name)| name)
                        .unwrap_or_else(|_| id.to_owned());
                    crate::human!(
                        "{id}: Knit can't tell whether you can push to {name}. If you can't, run `knit fork {id}`."
                    );
                    CheckoutPush::Unknown
                }
            },
            ForkMode::Always => self.fork(id, cwd)?,
        };
        self.outcomes
            .lock()
            .expect("checkout outcomes lock poisoned")
            .insert(id.to_owned(), push);
        Ok(())
    }

    fn fork(&self, id: &str, cwd: &Path) -> Result<CheckoutPush> {
        let mut announced = self.announced.lock().expect("fork notice lock poisoned");
        let result = fork_one(id, cwd, self.owner.as_deref(), false, false, &mut announced)?;
        let name = result["fork"].as_str().unwrap_or_default().to_owned();
        crate::human!("{id}: via fork {name}");
        Ok(CheckoutPush::Fork(name))
    }

    pub fn outcome(&self, id: &str) -> Option<CheckoutPush> {
        self.outcomes
            .lock()
            .expect("checkout outcomes lock poisoned")
            .get(id)
            .cloned()
    }
}

fn missing_credentials(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}").to_ascii_lowercase();
    crate::providers::is_likely_host_auth_failure(error)
        || message.contains("gh auth login")
        || message.contains("not logged")
        || message.contains("requires gh_token or github_token")
        || error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

pub fn fork_repos(
    ids: &[String],
    owner: Option<&str>,
    dry_run: bool,
    json_output: bool,
    force: bool,
) -> Result<()> {
    if json_output {
        crate::output::route_human_lines_to_stderr();
    }
    let root = find_knit_root(&std::env::current_dir()?).context("No Knit workspace found")?;
    let _credentials = crate::auth::activate_clone_root(root.clone());
    let project_id = load_config(&root)?
        .active_project
        .context("No active Knit project")?;
    let project: KnitProject = read_json(&project_path(&root, &project_id))?;
    let ids: Vec<String> = if ids.is_empty() {
        project.repos.iter().map(|repo| repo.id.clone()).collect()
    } else {
        ids.to_vec()
    };
    let mut results = Vec::new();
    let mut failed = false;
    let mut announced = false;
    for id in ids {
        let result = project
            .repos
            .iter()
            .find(|repo| repo.id == id)
            .with_context(|| format!("unknown project repo `{id}`"))
            .and_then(|repo| {
                fork_one(
                    &repo.id,
                    Path::new(&repo.path),
                    owner,
                    dry_run,
                    force,
                    &mut announced,
                )
            });
        match result {
            Ok(value) => {
                if !json_output {
                    println!(
                        "{id}: {} → {} ({}), pushes {} to the fork",
                        value["upstream"].as_str().unwrap_or_default(),
                        value["fork"].as_str().unwrap_or_default(),
                        value["status"].as_str().unwrap_or_default(),
                        if dry_run { "would go" } else { "go" }
                    );
                }
                results.push(value);
            }
            Err(error) => {
                failed = true;
                if !json_output {
                    eprintln!("{id}: {error:#}");
                }
                results
                    .push(json!({"repoId": id, "status": "error", "error": format!("{error:#}")}));
            }
        }
    }
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"dryRun": dry_run, "repos": results}))?
        );
    }
    if failed {
        bail!("one or more repositories could not be forked");
    }
    Ok(())
}

fn fork_one(
    id: &str,
    cwd: &Path,
    owner: Option<&str>,
    dry_run: bool,
    force: bool,
    announced: &mut bool,
) -> Result<Value> {
    let upstream = git_output(cwd, ["config", "--get", "remote.origin.url"])?
        .trim()
        .to_owned();
    let full_name = crate::contribution::github_name(&upstream)
        .map_err(|_| anyhow::anyhow!("{upstream}: Knit can only create forks on github.com"))?;
    let pushurls = git_output_optional(cwd, ["config", "--get-all", "remote.origin.pushurl"])?
        .unwrap_or_default();
    let current = match pushurls.lines().collect::<Vec<_>>()[..] {
        [] => None,
        [url] => Some(url.to_owned()),
        _ => bail!("origin has several push URLs; keep one and run again"),
    };
    let mut target = PrTarget::explicit(cwd, &full_name);
    target.repo_remote = Some(upstream.clone());
    let ssh = !upstream.starts_with("https://") && !upstream.starts_with("http://");
    let push_url = |fork: &ForkRepository| match ssh {
        true => fork.ssh_url.clone(),
        false => fork.clone_url.clone(),
    };

    let planned = GitHub.ensure_fork(&target, owner, true)?;
    check_current(cwd, current.as_deref(), &push_url(&planned), force)?;
    let fork = if dry_run {
        planned
    } else {
        if !*announced {
            let login = planned.full_name.split('/').next().unwrap_or_default();
            eprintln!("Forks are public repositories under {login}.");
            *announced = true;
        }
        GitHub.ensure_fork(&target, owner, false)?
    };
    let url = push_url(&fork);
    let unchanged = current
        .as_deref()
        .is_some_and(|current| same_repository(current, &url).unwrap_or(false));
    if !dry_run && !unchanged {
        check_current(cwd, current.as_deref(), &url, force)?;
        git_output(
            cwd,
            [
                "config",
                "--local",
                "--replace-all",
                "remote.origin.pushurl",
                &url,
            ],
        )?;
    }
    let status = match (dry_run, fork.existed) {
        (true, false) => "would create",
        (false, false) => "forked",
        (_, true) => "exists",
    };
    Ok(json!({
        "repoId": id,
        "upstream": full_name,
        "fork": fork.full_name,
        "status": status,
        "pushUrl": current.filter(|_| unchanged).unwrap_or(url),
    }))
}

fn check_current(cwd: &Path, current: Option<&str>, intended: &str, force: bool) -> Result<()> {
    let Some(current) = current else {
        return Ok(());
    };
    if same_repository(current, intended).unwrap_or(false) {
        return Ok(());
    }
    let shown = crate::commands::push::sanitized_push_url(current);
    if !force {
        bail!("origin already pushes to `{shown}`; use --force to point it at `{intended}`");
    }
    // Replacing the local value would leave a global or included one in place: two push URLs.
    let local = git_output_optional(
        cwd,
        [
            "config",
            "--local",
            "--no-includes",
            "--get",
            "remote.origin.pushurl",
        ],
    )?;
    if local.as_deref().map(str::trim) != Some(current) {
        bail!("origin's push URL `{shown}` is set outside this repository's Git config; remove it there and run again");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn conflict_message_hides_embedded_credentials() {
        let current = "https://synthetic-secret@example.com/other/library.git";
        let error = super::check_current(
            std::path::Path::new("."),
            Some(current),
            "https://github.com/contributor/library.git",
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("https://example.com/other/library.git"));
        assert!(!error.contains("synthetic-secret"));
    }
}
