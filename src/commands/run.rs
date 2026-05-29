use crate::checkout::checkout_dir;
use crate::model::{KnitProject, ProjectRunCommand, RepoEntry};
use crate::output as out;
use crate::repo_selectors::resolve_repo_indexes;
use crate::store::{load_active_bundle, load_config, project_path, read_json, ActiveBundle};
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub fn run_project_command(
    name: Option<&str>,
    explicit_repos: &[String],
    all: bool,
    list: bool,
    raw_args: &[OsString],
) -> Result<()> {
    if list {
        if name.is_some() || !raw_args.is_empty() || all || !explicit_repos.is_empty() {
            bail!("Use `knit run --list` without a command or repo selector.");
        }
        return list_commands();
    }

    if name.is_some() && !raw_args.is_empty() {
        bail!("Use either a named command or a raw command after --, not both.");
    }
    if name.is_none() && raw_args.is_empty() {
        if crate::commands::runtime::try_handle(None, raw_args)? {
            return Ok(());
        }
    }

    if let Some(name) = name {
        if raw_args.is_empty() && matches!(name, "up" | "down" | "status") {
            if crate::commands::runtime::try_handle(Some(name), raw_args)? {
                return Ok(());
            }
        }
    }

    if name.is_none() && raw_args.is_empty() {
        bail!("Pass a project command name, `knit run up|down|status`, or a raw command after --.");
    }

    let active = load_active_bundle()?;
    if active.bundle.repos.is_empty() {
        bail!("The resolved bundle has no repos. Run `knit bundle add <repo-path>` first.");
    }

    let invocation = match name {
        Some(name) => {
            let project = load_project_for_bundle(&active)?;
            let command_name = crate::ids::slugify(name);
            let Some(command) = project.commands.get(&command_name) else {
                bail!("Project command `{command_name}` is not configured.");
            };
            RunInvocation {
                label: command_name,
                repos: resolve_command_repos(&active, command, explicit_repos, all)?,
                args: command.command.iter().map(OsString::from).collect(),
                cwd: command.cwd.clone(),
                env: command
                    .env
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            }
        }
        None => RunInvocation {
            label: "raw".to_string(),
            repos: resolve_raw_repos(&active, explicit_repos, all)?,
            args: raw_args.to_vec(),
            cwd: None,
            env: Vec::new(),
        },
    };

    if invocation.args.is_empty() {
        bail!("Command `{}` has no executable.", invocation.label);
    }

    run_invocation(&active, invocation)
}

fn list_commands() -> Result<()> {
    let active = load_active_bundle()?;
    let project = load_project_for_bundle(&active)?;
    if project.commands.is_empty() {
        println!("{}", out::muted("No project commands."));
        return Ok(());
    }

    for (name, command) in project.commands {
        let repo_label = if command.repos.is_empty() {
            "(select at run time)".to_string()
        } else {
            command.repos.join(",")
        };
        println!(
            "{} {} {}",
            out::repo(name),
            out::muted(repo_label),
            command.command.join(" ")
        );
    }
    Ok(())
}

fn load_project_for_bundle(active: &ActiveBundle) -> Result<KnitProject> {
    let config = load_config(&active.root)?;
    let project_id = active
        .bundle
        .project_id
        .as_deref()
        .or(config.active_project.as_deref())
        .context("The resolved bundle is not associated with a Knit project.")?;
    read_json(&project_path(&active.root, project_id))
}

fn resolve_command_repos(
    active: &ActiveBundle,
    command: &ProjectRunCommand,
    explicit_repos: &[String],
    all: bool,
) -> Result<Vec<usize>> {
    if all || !explicit_repos.is_empty() {
        return resolve_repo_indexes(active, explicit_repos, all);
    }
    if !command.repos.is_empty() {
        return resolve_repo_indexes(active, &command.repos, false);
    }
    resolve_default_repo(active)
}

fn resolve_raw_repos(
    active: &ActiveBundle,
    explicit_repos: &[String],
    all: bool,
) -> Result<Vec<usize>> {
    if all || !explicit_repos.is_empty() {
        return resolve_repo_indexes(active, explicit_repos, all);
    }
    resolve_default_repo(active)
}

fn resolve_default_repo(active: &ActiveBundle) -> Result<Vec<usize>> {
    if active.bundle.repos.len() == 1 {
        Ok(vec![0])
    } else {
        bail!("Select a repo with --repo, use --all, or configure repos on the project command.")
    }
}

fn run_invocation(active: &ActiveBundle, invocation: RunInvocation) -> Result<()> {
    let multiple = invocation.repos.len() > 1;
    let mut failures = Vec::new();
    for index in invocation.repos {
        let repo = &active.bundle.repos[index];
        let cwd = match command_cwd(active, repo, invocation.cwd.as_deref()) {
            Ok(cwd) => cwd,
            Err(error) => {
                failures.push(format!("{}: {error:#}", repo.id));
                continue;
            }
        };
        if multiple {
            println!(
                "== {} ({}) ==",
                out::repo(&repo.id),
                out::path(cwd.display())
            );
        }

        let mut child = Command::new(&invocation.args[0]);
        child
            .args(&invocation.args[1..])
            .current_dir(&cwd)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .env("KNIT_ROOT", &active.root)
            .env("KNIT_BUNDLE", &active.bundle.id)
            .env("KNIT_REPO", &repo.id)
            .env("KNIT_CHECKOUT", &cwd);
        for (key, value) in &invocation.env {
            child.env(key, value);
        }

        let status = child
            .status()
            .with_context(|| format!("failed to run command in {}", cwd.display()))?;
        if !status.success() {
            failures.push(match status.code() {
                Some(code) => format!("{} exited {code}", repo.id),
                None => format!("{} terminated by signal", repo.id),
            });
        }
    }

    if !failures.is_empty() {
        bail!("run command failed: {}", failures.join(", "));
    }
    Ok(())
}

fn command_cwd(active: &ActiveBundle, repo: &RepoEntry, subdir: Option<&str>) -> Result<PathBuf> {
    let checkout = checkout_dir(active, repo).with_context(|| {
        format!(
            "{} has no active checkout. Run `knit worktree` to materialize it.",
            repo.id
        )
    })?;
    match subdir {
        Some(subdir) => Ok(resolve_subdir(&checkout, subdir)),
        None => Ok(checkout),
    }
}

fn resolve_subdir(checkout: &Path, subdir: &str) -> PathBuf {
    let path = PathBuf::from(subdir);
    if path.is_absolute() {
        path
    } else {
        checkout.join(path)
    }
}

struct RunInvocation {
    label: String,
    repos: Vec<usize>,
    args: Vec<OsString>,
    cwd: Option<String>,
    env: Vec<(String, String)>,
}
