use crate::commands::agents::write_project_agents_md;
use crate::git::{current_branch, git_output_optional, git_root, infer_base_branch};
use crate::ids::slugify;
use crate::model::{
    KnitConfig, KnitProject, ProjectRepoEntry, ProjectRunCommand, PROJECT_CONFIG_FILE,
    CheckoutMode,
};
use crate::output as out;
use crate::store::{
    acquire_named_lock, find_knit_root, load_config, project_path, read_json, save_config,
    write_json,
};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::path::Path;

pub fn init_project(name: &str, agents: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).unwrap_or(cwd);
    let project_id = slugify(name);
    let knit_dir = root.join(".knit");
    let project_dir = knit_dir.join("projects");
    fs::create_dir_all(&project_dir).context("failed to create .knit/projects")?;
    fs::create_dir_all(knit_dir.join("bundles")).context("failed to create .knit/bundles")?;
    fs::create_dir_all(knit_dir.join("worktrees")).context("failed to create .knit/worktrees")?;

    let path = project_path(&root, &project_id);
    if path.exists() {
        if agents {
            let project: KnitProject = read_json(&path)?;
            let agents_path = write_project_agents_md(&root, &project)?;
            println!(
                "{} {}",
                out::heading("Project AGENTS.md:"),
                out::path(agents_path.display())
            );
            return Ok(());
        }
        bail!("Project {} already exists.", out::path(path.display()));
    }

    let project = KnitProject::new(project_id.clone(), now_iso());
    write_json(&path, &project)?;

    let mut config = if root.join(".knit/config.json").exists() {
        load_config(&root)?
    } else {
        KnitConfig::new_project(project_id.clone())
    };
    config.active_project = Some(project_id.clone());
    save_config(&root, &config)?;

    println!("{} {}", out::heading("Project:"), out::repo(&project_id));
    println!("{} {}", out::heading("Path:"), out::path(path.display()));
    if agents {
        let agents_path = write_project_agents_md(&root, &project)?;
        println!(
            "{} {}",
            out::heading("Project AGENTS.md:"),
            out::path(agents_path.display())
        );
    }
    Ok(())
}

pub fn add_project_repo(
    repo_id: &str,
    repo_path: &Path,
    base: Option<&str>,
    observe: bool,
    agents: bool,
) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd)
        .context("No Knit project found. Run `knit init <name>` first.")?;
    let config = load_config(&root)?;
    let project_id = config
        .active_project
        .as_deref()
        .context("No active Knit project. Run `knit init <name>` first.")?;
    let _lock = acquire_named_lock(&root, &format!("project-{project_id}"))?;
    let path = project_path(&root, project_id);
    let mut project: KnitProject = read_json(&path)?;
    let repo = resolve_project_repo(repo_id, repo_path, base, observe)?;

    if let Some(existing) = project
        .repos
        .iter_mut()
        .find(|existing| existing.id == repo.id)
    {
        *existing = repo.clone();
        println!("{} {}", out::movement("updated"), out::repo(&repo.id));
    } else {
        println!("{} {}", out::movement("added"), out::repo(&repo.id));
        project.repos.push(repo);
    }

    project.updated_at = now_iso();
    write_json(&path, &project)?;
    if agents {
        let agents_path = write_project_agents_md(&root, &project)?;
        println!(
            "{} {}",
            out::heading("Project AGENTS.md:"),
            out::path(agents_path.display())
        );
    }
    Ok(())
}

pub fn refresh_project_agents(name: Option<&str>) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let config = load_config(&root)?;
    let project_id = name
        .map(slugify)
        .or(config.active_project)
        .context("No project selected. Pass a project name or run `knit init <name>`.")?;
    let project: KnitProject = read_json(&project_path(&root, &project_id))?;
    let agents_path = write_project_agents_md(&root, &project)?;
    println!(
        "{} {}",
        out::heading("Project AGENTS.md:"),
        out::path(agents_path.display())
    );
    Ok(())
}

pub fn list_projects() -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let active = load_config(&root)?.active_project;
    let dir = root.join(".knit/projects");
    if !dir.exists() {
        println!("{}", out::muted("No projects."));
        return Ok(());
    }

    let mut entries = fs::read_dir(&dir)
        .with_context(|| format!("failed to read {}", dir.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension() == Some(OsStr::new("json")))
        .collect::<Vec<_>>();
    entries.sort();

    for path in entries {
        let project: KnitProject = read_json(&path)?;
        let marker = if active.as_deref() == Some(project.id.as_str()) {
            "*"
        } else {
            " "
        };
        println!(
            "{} {} {} repo(s)",
            marker,
            out::repo(&project.id),
            project.repos.len()
        );
    }
    Ok(())
}

pub fn show_project(name: Option<&str>) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let config = load_config(&root)?;
    let project_id = name
        .map(slugify)
        .or(config.active_project)
        .context("No project selected. Pass a project name or run `knit init <name>`.")?;
    let project: KnitProject = read_json(&project_path(&root, &project_id))?;
    let text = serde_json::to_string_pretty(&project).context("failed to serialize project")?;
    println!("{text}");
    Ok(())
}

pub fn remove_project(name: &str, force: bool) -> Result<()> {
    if !force {
        bail!("Removing a project requires --force.");
    }
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let project_id = slugify(name);
    let _lock = acquire_named_lock(&root, &format!("project-{project_id}"))?;
    let path = project_path(&root, &project_id);
    if !path.exists() {
        bail!("No Knit project named `{project_id}` found.");
    }

    fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
    let mut config = load_config(&root)?;
    if config.active_project.as_deref() == Some(project_id.as_str()) {
        config.active_project = None;
        save_config(&root, &config)?;
    }
    println!(
        "{} {}",
        out::heading("Removed project:"),
        out::repo(project_id)
    );
    Ok(())
}

pub fn set_project_run_command(
    name: &str,
    repos: &[String],
    cwd: Option<&Path>,
    env: &[String],
    command: &[OsString],
) -> Result<()> {
    if command.is_empty() {
        bail!("Pass a command after the name, for example `knit project command set dev -- docker compose up`.");
    }
    let cwd_root = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd_root).context("No Knit workspace found.")?;
    let project_id = active_project_id(&root)?;
    let _lock = acquire_named_lock(&root, &format!("project-{project_id}"))?;
    let path = project_path(&root, &project_id);
    let mut project: KnitProject = read_json(&path)?;
    let command_name = slugify(name);
    let env = parse_env(env)?;
    let command = command
        .iter()
        .map(|value| value.to_string_lossy().to_string())
        .collect::<Vec<_>>();
    let cwd = cwd.map(|path| path.to_string_lossy().to_string());

    project.commands.insert(
        command_name.clone(),
        ProjectRunCommand {
            repos: repos.iter().map(|repo| slugify(repo)).collect(),
            cwd,
            command,
            env,
        },
    );
    project.updated_at = now_iso();
    write_json(&path, &project)?;
    println!(
        "{} {}",
        out::heading("Project command:"),
        out::repo(command_name)
    );
    Ok(())
}

pub fn list_project_run_commands() -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let project_id = active_project_id(&root)?;
    let project: KnitProject = read_json(&project_path(&root, &project_id))?;
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

pub fn remove_project_run_command(name: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let project_id = active_project_id(&root)?;
    let _lock = acquire_named_lock(&root, &format!("project-{project_id}"))?;
    let path = project_path(&root, &project_id);
    let mut project: KnitProject = read_json(&path)?;
    let command_name = slugify(name);
    if project.commands.remove(&command_name).is_none() {
        bail!("Project command `{command_name}` does not exist.");
    }
    project.updated_at = now_iso();
    write_json(&path, &project)?;
    println!(
        "{} {}",
        out::heading("Removed project command:"),
        out::repo(command_name)
    );
    Ok(())
}

pub fn pull_project_config(name: Option<&str>, repo_id: &str, agents: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd).context("No Knit workspace found.")?;
    let project_id = match name {
        Some(name) => slugify(name),
        None => active_project_id(&root)?,
    };
    let path = project_path(&root, &project_id);
    if !path.exists() {
        bail!(
            "Project `{}` does not exist locally. Run `knit init {project_id}` first.",
            out::repo(&project_id)
        );
    }

    let mut project: KnitProject = read_json(&path)?;
    let repo_entry = project
        .repos
        .iter()
        .find(|repo| repo.id == repo_id)
        .with_context(|| format!("repo `{repo_id}` is not listed in project `{}`", project.id))?;
    let repo_root = Path::new(&repo_entry.path);
    let config_path = repo_root.join(PROJECT_CONFIG_FILE);
    if !config_path.exists() {
        bail!(
            "No `{}` found in {}. Commit the project runtime config to the stack repo first.",
            PROJECT_CONFIG_FILE,
            out::path(repo_root.display())
        );
    }

    let incoming: KnitProject = read_json(&config_path)?;
    if incoming.id != project.id {
        bail!(
            "Project id mismatch: workspace has `{}` but `{}` declares `{}`.",
            project.id,
            out::path(config_path.display()),
            incoming.id
        );
    }

    if incoming.runtime.is_some() {
        project.runtime = incoming.runtime;
    }
    if incoming.landing.is_some() {
        project.landing = incoming.landing;
    }
    for (command_name, command) in incoming.commands {
        project.commands.entry(command_name).or_insert(command);
    }
    project.updated_at = now_iso();
    write_json(&path, &project)?;

    println!(
        "{} {}",
        out::heading("Pulled project config:"),
        out::path(config_path.display())
    );
    println!("{} {}", out::heading("Updated:"), out::path(path.display()));

    if agents {
        let agents_path = write_project_agents_md(&root, &project)?;
        println!(
            "{} {}",
            out::heading("Project AGENTS.md:"),
            out::path(agents_path.display())
        );
    }
    Ok(())
}

pub fn load_project_by_id(root: &Path, project_id: &str) -> Result<KnitProject> {
    read_json(&project_path(root, project_id))
}

fn active_project_id(root: &Path) -> Result<String> {
    load_config(root)?
        .active_project
        .context("No active Knit project. Run `knit init <name>` first.")
}

fn parse_env(values: &[String]) -> Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();
    for value in values {
        let Some((key, variable_value)) = value.split_once('=') else {
            bail!("Environment entries must use KEY=VALUE syntax: {value}");
        };
        if key.trim().is_empty() {
            bail!("Environment variable names cannot be empty.");
        }
        env.insert(key.to_string(), variable_value.to_string());
    }
    Ok(env)
}

fn resolve_project_repo(
    repo_id: &str,
    repo_path: &Path,
    base_override: Option<&str>,
    observe: bool,
) -> Result<ProjectRepoEntry> {
    let repo_root = git_root(repo_path)?;
    let current_branch = current_branch(&repo_root)?;
    let remote = git_output_optional(&repo_root, ["remote", "get-url", "origin"])?;
    let base_branch = match base_override {
        Some(base) => base.to_string(),
        None => infer_base_branch(&repo_root, current_branch.as_deref())?,
    };

    Ok(ProjectRepoEntry {
        id: slugify(repo_id),
        path: repo_root.to_string_lossy().to_string(),
        remote,
        base_branch,
        checkout_mode: CheckoutMode::Worktree,
        include_by_default: !observe,
    })
}
