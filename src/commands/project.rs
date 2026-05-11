use crate::git::{current_branch, git_output_optional, git_root, infer_base_branch};
use crate::ids::slugify;
use crate::model::{KnitConfig, KnitProject, ProjectRepoEntry, CHECKOUT_MODE_WORKTREE};
use crate::output as out;
use crate::store::{
    acquire_named_lock, find_knit_root, load_config, project_path, read_json, save_config,
    write_json,
};
use crate::time::now_iso;
use anyhow::{bail, Context, Result};
use std::ffi::OsStr;
use std::fs;
use std::path::Path;

pub fn init_project(name: &str) -> Result<()> {
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
    Ok(())
}

pub fn add_project_repo(
    repo_id: &str,
    repo_path: &Path,
    base: Option<&str>,
    observe: bool,
) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let root = find_knit_root(&cwd)
        .context("No Knit project found. Run `knit project init <name>` first.")?;
    let config = load_config(&root)?;
    let project_id = config
        .active_project
        .as_deref()
        .context("No active Knit project. Run `knit project init <name>` first.")?;
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
        .context("No project selected. Pass a project name or run `knit project init <name>`.")?;
    let project: KnitProject = read_json(&project_path(&root, &project_id))?;
    let text = serde_json::to_string_pretty(&project).context("failed to serialize project")?;
    println!("{text}");
    Ok(())
}

pub fn load_project_by_id(root: &Path, project_id: &str) -> Result<KnitProject> {
    read_json(&project_path(root, project_id))
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
        checkout_mode: CHECKOUT_MODE_WORKTREE.to_string(),
        include_by_default: !observe,
    })
}
