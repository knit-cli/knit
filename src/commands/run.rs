use crate::checkout::checkout_dir;
use crate::model::{KnitProject, ProjectRunCommand, RepoEntry};
use crate::output as out;
use crate::repo_selectors::resolve_repo_indexes;
use crate::store::{load_active_bundle, load_config, project_path, read_json, ActiveBundle};
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const RUNTIME_COMMANDS: [&str; 4] = ["up", "down", "status", "eject"];

pub fn run_project_command(
    name: Option<&str>,
    explicit_repos: &[String],
    all: bool,
    list: bool,
    force: bool,
    purge: bool,
    raw_args: &[OsString],
) -> Result<()> {
    if purge && name != Some("down") {
        bail!("`--purge` is only valid with `knit run down`.");
    }
    if list {
        if name.is_some()
            || !raw_args.is_empty()
            || all
            || !explicit_repos.is_empty()
            || force
            || purge
        {
            bail!("Use `knit run --list` without a command or repo selector.");
        }
        return list_commands();
    }

    if name.is_some() && !raw_args.is_empty() {
        bail!("Use either a named command or a raw command after --, not both.");
    }

    if raw_args.is_empty() {
        if let Some(runtime_command) = name.filter(|name| RUNTIME_COMMANDS.contains(name)) {
            // An explicitly configured project command of the same name wins
            // over the built-in runtime verbs.
            let shadowed = load_active_bundle()
                .ok()
                .and_then(|active| load_project_for_bundle(&active).ok())
                .is_some_and(|project| {
                    project
                        .commands
                        .contains_key(&crate::ids::slugify(runtime_command))
                });
            if !shadowed {
                if crate::commands::runtime::try_handle(runtime_command, force, purge)? {
                    return Ok(());
                }

                let active = load_active_bundle()?;
                if project_has_runtime(&active)? {
                    bail!(
                        "Bundle runtime is configured but could not run `{runtime_command}`. Use an updated knit CLI from `.knit/worktrees/<bundle>/knit` and run the command from a stack worktree checkout."
                    );
                }

                bail!(
                    "`knit run {runtime_command}` needs a bundle repo with a docker-compose file, or a `runtime` block in the Knit project (pull it with `knit project pull --repo <stack-repo>`)."
                );
            }
            if purge {
                bail!(
                    "Project command `down` shadows the built-in bundle runtime, so `--purge` cannot be applied."
                );
            }
        }
    }

    if name.is_none() && raw_args.is_empty() {
        bail!(
            "Pass a project command name, `knit run up|down|status|eject`, or a raw command after --."
        );
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

fn project_has_runtime(active: &ActiveBundle) -> Result<bool> {
    Ok(load_project_for_bundle(active)
        .ok()
        .and_then(|project| project.runtime)
        .is_some())
}

fn list_commands() -> Result<()> {
    let active = load_active_bundle()?;
    let project = load_project_for_bundle(&active)?;
    if project.commands.is_empty() {
        if project.runtime.is_some() {
            println!(
                "{}",
                out::muted("Runtime commands: up, down, status, eject")
            );
            return Ok(());
        }
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
    if project.runtime.is_some() {
        println!(
            "{} {}",
            out::repo("up|down|status|eject"),
            out::muted("bundle runtime (docker-compose)")
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
    let failures = run_invocation_collect(active, invocation)?;
    if !failures.is_empty() {
        bail!("run command failed: {}", failures.join(", "));
    }
    Ok(())
}

/// What `run_named_command_collect` learned from one execution: the command
/// line it ran and the per-repo failures (empty means every repo exited 0).
pub(crate) struct NamedRunOutcome {
    pub(crate) command_display: String,
    pub(crate) failures: Vec<String>,
}

/// Run the configured project command `name` across its repos like `knit run
/// <name>`, but report per-repo failures instead of failing the command.
/// Used by `knit check run`, where a failing command is a recordable verdict
/// rather than an error.
pub(crate) fn run_named_command_collect(
    active: &ActiveBundle,
    name: &str,
    explicit_repos: &[String],
    all: bool,
) -> Result<NamedRunOutcome> {
    if active.bundle.repos.is_empty() {
        bail!("The resolved bundle has no repos. Run `knit bundle add <repo-path>` first.");
    }
    let project = load_project_for_bundle(active)?;
    let command_name = crate::ids::slugify(name);
    let Some(command) = project.commands.get(&command_name) else {
        bail!(
            "Project command `{command_name}` is not configured. Define it with `knit project command set {command_name} -- <command>`."
        );
    };
    let invocation = RunInvocation {
        label: command_name,
        repos: resolve_command_repos(active, command, explicit_repos, all)?,
        args: command.command.iter().map(OsString::from).collect(),
        cwd: command.cwd.clone(),
        env: command
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    };
    if invocation.args.is_empty() {
        bail!("Command `{}` has no executable.", invocation.label);
    }
    let command_display = command.command.join(" ");
    let failures = run_invocation_collect(active, invocation)?;
    Ok(NamedRunOutcome {
        command_display,
        failures,
    })
}

fn run_invocation_collect(active: &ActiveBundle, invocation: RunInvocation) -> Result<Vec<String>> {
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

    Ok(failures)
}

fn command_cwd(active: &ActiveBundle, repo: &RepoEntry, subdir: Option<&str>) -> Result<PathBuf> {
    let checkout = checkout_dir(active, repo).with_context(|| {
        format!(
            "{} has no active checkout. Run `knit bundle worktree` to materialize it.",
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
